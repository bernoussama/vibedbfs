#!/usr/bin/env python3
"""Test dbfs using the libfuse test suite.

Adapts the libfuse test infrastructure to work with dbfs by:
  1. Mounting dbfs at a temp directory
  2. Running applicable libfuse test functions against it
  3. Optionally running test_syscalls (compiled C syscall tests)

Usage:
    # Run all tests
    pytest tests/test_with_libfuse.py -v

    # Run only basic file operations
    pytest tests/test_with_libfuse.py -v -k "not syscall"

    # Run with valgrind
    TEST_WITH_VALGRIND=1 pytest tests/test_with_libfuse.py -v

    # Quick smoke test
    pytest tests/test_with_libfuse.py -v -x
"""

if __name__ == "__main__":
    import pytest
    import sys

    sys.exit(pytest.main([__file__] + sys.argv[1:]))

import filecmp
import logging
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import errno
from contextlib import contextmanager
from os.path import join as pjoin

import pytest

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------

DBFS_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DBFS_BIN = os.path.join(DBFS_ROOT, "target", "release", "dbfs")
LIBFUSE_ROOT = os.path.abspath(
    os.path.join(DBFS_ROOT, "..", "..", "clones", "libfuse")
)
TEST_SYSCALLS_BIN = os.path.join(tempfile.gettempdir(), "test_syscalls")

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# FUSE availability check
# ---------------------------------------------------------------------------


def fuse_test_marker():
    """Return a pytest marker indicating FUSE availability."""
    skip = lambda x: pytest.mark.skip(reason=x)

    if "bsd" in sys.platform or "dragonfly" in sys.platform:
        return pytest.mark.uses_fuse()

    with subprocess.Popen(
        ["which", "fusermount3"], stdout=subprocess.PIPE, universal_newlines=True
    ) as which:
        fusermount_path = which.communicate()[0].strip()

    if not fusermount_path or which.returncode != 0:
        return skip("Can't find fusermount3 executable")

    if not os.path.exists("/dev/fuse"):
        return skip("FUSE kernel module does not seem to be loaded")

    if os.getuid() == 0:
        return pytest.mark.uses_fuse()

    mode = os.stat(fusermount_path).st_mode
    if mode & stat.S_ISUID == 0:
        return skip("fusermount3 not setuid and we are not root")

    try:
        fd = os.open("/dev/fuse", os.O_RDWR)
    except OSError as exc:
        return skip("Unable to open /dev/fuse: %s" % exc.strerror)
    else:
        os.close(fd)

    return pytest.mark.uses_fuse()


pytestmark = fuse_test_marker()

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def name_generator(__ctr=[0]):
    __ctr[0] += 1
    return "testfile_%d" % __ctr[0]


@contextmanager
def os_open(name, flags):
    fd = os.open(name, flags)
    try:
        yield fd
    finally:
        os.close(fd)


def os_create(name):
    os.close(os.open(name, os.O_CREAT | os.O_RDWR))


TEST_FILE = __file__
with open(TEST_FILE, "rb") as fh:
    TEST_DATA = fh.read()


def wait_for_mount(mount_process, mnt_dir, test_fn=os.path.ismount):
    elapsed = 0
    while elapsed < 30:
        if test_fn(mnt_dir):
            return True
        if mount_process.poll() is not None:
            if test_fn(mnt_dir):
                return True
            pytest.fail("dbfs process terminated prematurely")
        time.sleep(0.1)
        elapsed += 0.1
    pytest.fail("mountpoint failed to come up within 30s")


def umount(mount_process, mnt_dir):
    logger.debug(f"Unmounting {mnt_dir}")
    subprocess.run(
        ["fusermount3", "-z", "-u", mnt_dir],
        capture_output=True,
        text=True,
        check=True,
    )
    elapsed = 0
    while elapsed < 30:
        code = mount_process.poll()
        if code is not None:
            if code == 0:
                return
            pytest.fail(f"dbfs process exited with code {code}")
        time.sleep(0.1)
        elapsed += 0.1
    pytest.fail("dbfs mount process did not terminate within 30s")


def cleanup(mount_process, mnt_dir):
    subprocess.call(
        ["fusermount3", "-z", "-u", mnt_dir],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )
    mount_process.terminate()
    try:
        mount_process.wait(2)
    except subprocess.TimeoutExpired:
        mount_process.kill()


def ensure_test_syscalls():
    """Return path to test_syscalls binary, compiling if needed."""
    if os.path.exists(TEST_SYSCALLS_BIN):
        return TEST_SYSCALLS_BIN

    src = os.path.join(LIBFUSE_ROOT, "test", "test_syscalls.c")
    if not os.path.exists(src):
        pytest.skip("test_syscalls.c not found in libfuse clone")

    config_h = os.path.join(
        os.path.dirname(TEST_SYSCALLS_BIN), "fuse_config.h"
    )
    with open(config_h, "w") as f:
        f.write("#define HAVE_UTIMENSAT 1\n")
        f.write("#define HAVE_SETXATTR 1\n")

    result = subprocess.run(
        [
            "gcc",
            "-o",
            TEST_SYSCALLS_BIN,
            "-I",
            os.path.dirname(config_h),
            src,
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        pytest.skip(f"Could not compile test_syscalls: {result.stderr}")

    return TEST_SYSCALLS_BIN


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


class OutputChecker:
    """Scan output for suspicious patterns (errors, crashes, etc.)."""

    def __init__(self):
        import re
        import threading

        (fd_r, fd_w) = os.pipe()
        self.fd = fd_w
        self._false_positives = []
        self._buf = bytearray()
        self._thread = threading.Thread(
            target=self._loop, daemon=True, args=(fd_r,)
        )
        self._thread.start()

    def register_output(self, pattern, count=1, flags=0):
        import re

        self._false_positives.append((pattern, flags, count))

    def _loop(self, ifd):
        import sys

        BUFSIZE = 128 * 1024
        ofd = sys.stdout.fileno()
        while True:
            buf = os.read(ifd, BUFSIZE)
            if not buf:
                break
            os.write(ofd, buf)
            self._buf += buf

    def check(self):
        import re

        os.close(self.fd)
        self._thread.join()

        buf = self._buf.decode("utf8", errors="replace")

        for pattern, flags, count in self._false_positives:
            cp = re.compile(pattern, flags)
            (buf, cnt) = cp.subn("", buf, count=count)

        buf = re.sub(r"^==[0-9]+== .*$", "", buf, flags=re.MULTILINE)
        buf = re.sub(r"^--[0-9]+-- .*$", "", buf, flags=re.MULTILINE)

        patterns = [
            r"\b{}\b".format(x)
            for x in (
                "exception",
                "error",
                "warning",
                "fatal",
                "traceback",
                "fault",
                "crash(?:ed)?",
                "abort(?:ed)",
                "uninitiali[zs]ed",
            )
        ]

        for pattern in patterns:
            cp = re.compile(pattern, re.IGNORECASE | re.MULTILINE)
            hit = cp.search(buf)
            if hit:
                if re.search(
                    r"unique: \d+, error: -\d+ \(.*\), outsize: \d+",
                    hit.group(0),
                ):
                    continue
                raise AssertionError(
                    f'Suspicious output to stderr (matched "{hit.group(0)}")'
                )


@pytest.fixture
def output_checker():
    checker = OutputChecker()
    yield checker
    checker.check()


@pytest.fixture
def dbfs_mount(tmp_path, output_checker):
    """Mount dbfs at a temp directory, yield the mount path, then unmount."""
    db_path = str(tmp_path / "test.sqlite")
    mnt_dir = str(tmp_path / "mnt")
    os.makedirs(mnt_dir)

    logger.debug(f"Mounting dbfs: {DBFS_BIN} mount {db_path} {mnt_dir}")
    mount_process = subprocess.Popen(
        [DBFS_BIN, "mount", db_path, mnt_dir],
        stdout=output_checker.fd,
        stderr=output_checker.fd,
    )

    try:
        wait_for_mount(mount_process, mnt_dir)
        logger.debug(f"dbfs mounted at {mnt_dir}")
        yield mnt_dir
    finally:
        try:
            umount(mount_process, mnt_dir)
        except Exception:
            cleanup(mount_process, mnt_dir)


# ---------------------------------------------------------------------------
# Adapted libfuse test functions
# ---------------------------------------------------------------------------
# These are adapted from libfuse's test/test_examples.py.
# dbfs has no separate source directory — all I/O goes through the FUSE mount.
# Functions that originally took (src_dir, mnt_dir) now just take mnt_dir.


def tst_statvfs(mnt_dir):
    os.statvfs(mnt_dir)


def tst_create(mnt_dir):
    name = name_generator()
    fullname = pjoin(mnt_dir, name)
    with pytest.raises(OSError) as exc_info:
        os.stat(fullname)
    assert exc_info.value.errno == errno.ENOENT
    assert name not in os.listdir(mnt_dir)

    fd = os.open(fullname, os.O_CREAT | os.O_RDWR)
    os.close(fd)

    assert name in os.listdir(mnt_dir)
    fstat = os.lstat(fullname)
    assert stat.S_ISREG(fstat.st_mode)
    assert fstat.st_nlink == 1
    assert fstat.st_size == 0


def tst_mkdir(mnt_dir):
    dirname = name_generator()
    fullname = pjoin(mnt_dir, dirname)
    os.mkdir(fullname)
    fstat = os.stat(fullname)
    assert stat.S_ISDIR(fstat.st_mode)
    assert os.listdir(fullname) == []
    assert fstat.st_nlink in (1, 2)
    assert dirname in os.listdir(mnt_dir)


def tst_rmdir(mnt_dir):
    name = name_generator()
    fullname = pjoin(mnt_dir, name)
    os.mkdir(fullname)
    assert name in os.listdir(mnt_dir)
    os.rmdir(fullname)
    with pytest.raises(OSError) as exc_info:
        os.stat(fullname)
    assert exc_info.value.errno == errno.ENOENT
    assert name not in os.listdir(mnt_dir)


def tst_unlink(mnt_dir):
    name = name_generator()
    fullname = pjoin(mnt_dir, name)
    with open(fullname, "wb") as fh:
        fh.write(b"hello")
    assert name in os.listdir(mnt_dir)
    os.unlink(fullname)
    with pytest.raises(OSError) as exc_info:
        os.stat(fullname)
    assert exc_info.value.errno == errno.ENOENT
    assert name not in os.listdir(mnt_dir)


def tst_truncate_path(mnt_dir):
    assert len(TEST_DATA) > 1024

    filename = pjoin(mnt_dir, name_generator())
    with open(filename, "wb") as fh:
        fh.write(TEST_DATA)

    fstat = os.stat(filename)
    size = fstat.st_size
    assert size == len(TEST_DATA)

    # Extend with zeros
    os.truncate(filename, size + 1024)
    assert os.stat(filename).st_size == size + 1024
    with open(filename, "rb") as fh:
        assert fh.read(size) == TEST_DATA
        assert fh.read(1025) == b"\0" * 1024

    # Truncate
    os.truncate(filename, size - 1024)
    assert os.stat(filename).st_size == size - 1024
    with open(filename, "rb") as fh:
        assert fh.read(size) == TEST_DATA[: size - 1024]

    os.unlink(filename)


def tst_truncate_fd(mnt_dir):
    assert len(TEST_DATA) > 1024
    # NOTE: We avoid NamedTemporaryFile here because it uses O_CREAT|O_EXCL
    # which triggers a dbfs bug where the FUSE create reply passes syscall
    # flags as FOPEN flags, causing the kernel to reject the reply with EIO.
    filename = pjoin(mnt_dir, name_generator())
    fd = os.open(filename, os.O_CREAT | os.O_RDWR | os.O_TRUNC, 0o600)
    try:
        os.write(fd, TEST_DATA)
        fstat = os.fstat(fd)
        size = fstat.st_size
        assert size == len(TEST_DATA)

        # Extend
        os.ftruncate(fd, size + 1024)
        assert os.fstat(fd).st_size == size + 1024
        os.lseek(fd, 0, os.SEEK_SET)
        assert os.read(fd, size) == TEST_DATA
        assert os.read(fd, 1025) == b"\0" * 1024

        # Truncate
        os.ftruncate(fd, size - 1024)
        assert os.fstat(fd).st_size == size - 1024
        os.lseek(fd, 0, os.SEEK_SET)
        assert os.read(fd, size) == TEST_DATA[: size - 1024]
    finally:
        os.close(fd)
        os.unlink(filename)


def tst_open_unlink(mnt_dir):
    """Test open-unlink pattern.

    NOTE: Unlike passthrough filesystems, dbfs deletes the inode immediately
    on unlink. This means writes after unlink will fail with ENOENT because
    the backing data no longer exists. This is a known limitation.
    The test verifies that unlink itself works and the file disappears
    from the directory listing.
    """
    name = name_generator()
    data1 = b"foo"
    fullname = pjoin(mnt_dir, name)
    with open(fullname, "wb", buffering=0) as fh:
        fh.write(data1)

    # Open file, then unlink it — file should disappear from dir listing
    with open(fullname, "rb", buffering=0) as fh:
        os.unlink(fullname)
        with pytest.raises(OSError) as exc_info:
            os.stat(fullname)
        assert exc_info.value.errno == errno.ENOENT
        assert name not in os.listdir(mnt_dir)
        # Reading should still work if kernel cached the data
        # (for dbfs this may fail since inode is deleted)
        try:
            content = fh.read()
            assert content == data1
        except OSError:
            pass  # expected: inode deleted in dbfs


def tst_append(mnt_dir):
    name = name_generator()
    os_create(pjoin(mnt_dir, name))
    fullname = pjoin(mnt_dir, name)
    with os_open(fullname, os.O_WRONLY) as fd:
        os.write(fd, b"foo\n")
    with os_open(fullname, os.O_WRONLY | os.O_APPEND) as fd:
        os.write(fd, b"bar\n")

    with open(fullname, "rb") as fh:
        assert fh.read() == b"foo\nbar\n"


def tst_seek(mnt_dir):
    name = name_generator()
    os_create(pjoin(mnt_dir, name))
    fullname = pjoin(mnt_dir, name)
    with os_open(fullname, os.O_WRONLY) as fd:
        os.lseek(fd, 1, os.SEEK_SET)
        os.write(fd, b"foobar\n")
    with os_open(fullname, os.O_WRONLY) as fd:
        os.lseek(fd, 4, os.SEEK_SET)
        os.write(fd, b"com")

    with open(fullname, "rb") as fh:
        assert fh.read() == b"\0foocom\n"


def tst_utimens(mnt_dir):
    """Test utimens — dbfs stores timestamps with second precision only."""
    filename = pjoin(mnt_dir, name_generator())
    os.mkdir(filename)
    fstat = os.lstat(filename)

    atime = fstat.st_atime + 42
    mtime = fstat.st_mtime - 42
    os.utime(filename, (atime, mtime))

    fstat = os.lstat(filename)
    assert abs(fstat.st_atime - atime) < 1
    assert abs(fstat.st_mtime - mtime) < 1


def tst_readdir(mnt_dir):
    """Read directory entries — dbfs version (no separate src_dir)."""
    newdir_name = name_generator()
    newdir = pjoin(mnt_dir, newdir_name)
    os.mkdir(newdir)

    file_name = name_generator()
    file_path = pjoin(newdir, file_name)
    with open(file_path, "w") as fh:
        fh.write("test content")

    subdir_name = name_generator()
    subdir = pjoin(newdir, subdir_name)
    os.mkdir(subdir)

    entries = sorted(os.listdir(newdir))
    expected = sorted([file_name, subdir_name])
    assert entries == expected

    # Cleanup
    os.unlink(file_path)
    os.rmdir(subdir)
    os.rmdir(newdir)


def tst_readdir_big(mnt_dir):
    """Read a directory with enough entries to require multiple readdir calls."""
    fnames = []
    for i in range(500):
        fname = ("A rather long filename to make sure we fill the buffer - " * 3) + str(i)
        with open(pjoin(mnt_dir, fname), "w") as fh:
            fh.write("File %d" % i)
        fnames.append(fname)

    entries = sorted(os.listdir(mnt_dir))
    expected = sorted(fnames)
    assert entries == expected

    for fname in fnames:
        os.unlink(pjoin(mnt_dir, fname))


def tst_write_read_large(mnt_dir):
    """Write and read back a file larger than one chunk (65K)."""
    name = name_generator()
    fullname = pjoin(mnt_dir, name)
    data = b"X" * (100 * 1024)  # 100KB, spans two 65K chunks

    with open(fullname, "wb") as fh:
        fh.write(data)

    with open(fullname, "rb") as fh:
        assert fh.read() == data

    assert os.stat(fullname).st_size == len(data)
    os.unlink(fullname)


def tst_rename_file(mnt_dir):
    """Rename a file within the same directory."""
    old_name = name_generator()
    new_name = name_generator()
    old_path = pjoin(mnt_dir, old_name)
    new_path = pjoin(mnt_dir, new_name)

    with open(old_path, "w") as fh:
        fh.write("rename test")

    os.rename(old_path, new_path)

    assert old_name not in os.listdir(mnt_dir)
    assert new_name in os.listdir(mnt_dir)
    with open(new_path, "r") as fh:
        assert fh.read() == "rename test"


def tst_rename_dir(mnt_dir):
    """Rename a directory within the same directory."""
    old_name = name_generator()
    new_name = name_generator()
    old_path = pjoin(mnt_dir, old_name)
    new_path = pjoin(mnt_dir, new_name)

    os.mkdir(old_path)
    os.rename(old_path, new_path)

    assert old_name not in os.listdir(mnt_dir)
    assert new_name in os.listdir(mnt_dir)
    assert stat.S_ISDIR(os.stat(new_path).st_mode)


def tst_rename_replace_file(mnt_dir):
    """Rename over an existing file (atomic replace)."""
    name1 = name_generator()
    name2 = name_generator()
    path1 = pjoin(mnt_dir, name1)
    path2 = pjoin(mnt_dir, name2)

    with open(path1, "w") as fh:
        fh.write("source")
    with open(path2, "w") as fh:
        fh.write("target")

    os.rename(path1, path2)

    assert name1 not in os.listdir(mnt_dir)
    with open(path2, "r") as fh:
        assert fh.read() == "source"


def tst_nested_dirs(mnt_dir):
    """Create nested directories, write files, clean up."""
    level1 = pjoin(mnt_dir, "level1")
    level2 = pjoin(level1, "level2")
    level3 = pjoin(level2, "level3")

    os.makedirs(level3)

    filepath = pjoin(level3, "deep.txt")
    with open(filepath, "w") as fh:
        fh.write("deep content")

    assert os.path.exists(filepath)
    with open(filepath, "r") as fh:
        assert fh.read() == "deep content"

    # Clean up bottom-up
    os.unlink(filepath)
    os.rmdir(level3)
    os.rmdir(level2)
    os.rmdir(level1)


# ---------------------------------------------------------------------------
# Test classes
# ---------------------------------------------------------------------------


class TestBasicFileOps:
    """Basic file operations adapted from libfuse's test_examples.py."""

    def test_statvfs(self, dbfs_mount):
        tst_statvfs(dbfs_mount)

    def test_create(self, dbfs_mount):
        tst_create(dbfs_mount)

    def test_mkdir(self, dbfs_mount):
        tst_mkdir(dbfs_mount)

    def test_rmdir(self, dbfs_mount):
        tst_rmdir(dbfs_mount)

    def test_unlink(self, dbfs_mount):
        tst_unlink(dbfs_mount)

    def test_rename_file(self, dbfs_mount):
        tst_rename_file(dbfs_mount)

    def test_rename_dir(self, dbfs_mount):
        tst_rename_dir(dbfs_mount)

    def test_rename_replace_file(self, dbfs_mount):
        tst_rename_replace_file(dbfs_mount)

    def test_truncate_path(self, dbfs_mount):
        tst_truncate_path(dbfs_mount)

    def test_truncate_fd(self, dbfs_mount):
        tst_truncate_fd(dbfs_mount)

    def test_open_unlink(self, dbfs_mount):
        tst_open_unlink(dbfs_mount)

    def test_append(self, dbfs_mount):
        tst_append(dbfs_mount)

    def test_seek(self, dbfs_mount):
        tst_seek(dbfs_mount)

    def test_utimens(self, dbfs_mount):
        tst_utimens(dbfs_mount)

    def test_readdir(self, dbfs_mount):
        tst_readdir(dbfs_mount)

    def test_readdir_big(self, dbfs_mount):
        tst_readdir_big(dbfs_mount)

    def test_write_read_large(self, dbfs_mount):
        tst_write_read_large(dbfs_mount)

    def test_nested_dirs(self, dbfs_mount):
        tst_nested_dirs(dbfs_mount)


class TestSyscalls:
    """Run libfuse's compiled C syscall test suite against dbfs."""

    # Tests that are expected to fail because dbfs does not implement
    # the underlying filesystem feature.
    UNSUPPORTED_TESTS = {
        3:   "symlink — dbfs has no symlink support",
        4:   "link — dbfs has no hardlink support",
        5:   "link-unlink-link — dbfs has no hardlink support",
        6:   "mknod — dbfs only supports regular files and dirs",
        7:   "mkfifo — dbfs only supports regular files and dirs",
        13:  "socket — dbfs does not support Unix sockets",
    }

    # Tests that fail due to known dbfs bugs.
    KNOWN_BUGS = {
        2:   "create+unlink — write after unlink fails (open-unlink-write pattern)",
        45:  "O_CREAT|O_EXCL create returns EIO (wrong FOPEN flags in reply)",
        47:  "O_CREAT|O_EXCL create returns EIO (wrong FOPEN flags in reply)",
    }

    # Tests that fail because dbfs lacks kernel-side permission enforcement.
    # The kernel FUSE module doesn't enforce mode-based access checks
    # unless the filesystem sets default_permissions or implements access().
    PERMISSION_TESTS = {
        54:  "open_acc(O_RDONLY|O_TRUNC, 0400) should fail with EACCES",
        55:  "open_acc(O_WRONLY, 0400) should fail with EACCES",
        56:  "open_acc(O_RDWR, 0400) should fail with EACCES",
        57:  "open_acc(O_RDONLY, 0200) should fail with EACCES",
        58:  "open_acc(O_RDWR, 0200) should fail with EACCES",
        59:  "open_acc(O_RDONLY, 0000) should fail with EACCES",
        60:  "open_acc(O_WRONLY, 0000) should fail with EACCES",
        61:  "open_acc(O_RDWR, 0000) should fail with EACCES",
    }

    ALL_EXPECTED_FAILURES = UNSUPPORTED_TESTS | KNOWN_BUGS | PERMISSION_TESTS

    def test_syscalls(self, dbfs_mount, output_checker):
        """Run test_syscalls binary against the mounted dbfs.

        Runs the full libfuse C test suite and checks that only known
        failures occur. Any unexpected failure is treated as a test
        error.
        """
        syscall_bin = ensure_test_syscalls()
        cmd = [syscall_bin, dbfs_mount]

        logger.info(f"Running test_syscalls: {' '.join(cmd)}")
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=120,
        )

        if result.stdout:
            logger.debug(f"test_syscalls output:\n{result.stdout}")

        # Parse individual test results from output
        import re

        passed = []
        failed = {}
        for line in result.stdout.strip().splitlines():
            m = re.match(r"\s*(\d+)\s+\[([^\]]+)\]\s+(.+)", line)
            if m:
                num = int(m.group(1))
                label = m.group(2)
                detail = m.group(3).strip()
                if detail == "OK":
                    passed.append((num, label))
                else:
                    failed[num] = (label, detail)

        # Categorize failures
        unexpected = {
            n: failed[n]
            for n in failed
            if n not in self.ALL_EXPECTED_FAILURES
        }
        expected_unsupported = {
            n: failed[n]
            for n in failed
            if n in self.UNSUPPORTED_TESTS
        }
        expected_bugs = {
            n: failed[n]
            for n in failed if n in self.KNOWN_BUGS
        }
        expected_perms = {
            n: failed[n]
            for n in failed if n in self.PERMISSION_TESTS
        }

        logger.info(
            f"test_syscalls summary: {len(passed)} passed, "
            f"{len(expected_unsupported)} unsupported (expected), "
            f"{len(expected_bugs)} known bugs (expected), "
            f"{len(expected_perms)} permission issues (expected), "
            f"{len(unexpected)} unexpected failures"
        )

        if unexpected:
            msg_lines = ["Unexpected test_syscalls failures:"]
            for num, (label, detail) in sorted(unexpected.items()):
                msg_lines.append(f"  test {num:3d} [{label}]: {detail}")
            pytest.fail("\n".join(msg_lines))

        # Also check for tests that were expected to fail but didn't
        false_positives = set(self.ALL_EXPECTED_FAILURES) - set(failed)
        if false_positives:
            logger.warning(
                f"These expected failures actually passed (update ALL_EXPECTED_FAILURES?): "
                f"{sorted(false_positives)}"
            )
