//! Clean shutdown: SIGTERM, SIGINT, SIGHUP and an external `fusermount3 -u` must
//! detach the mount, keep every buffered write, and exit 0.
//!
//! Run with `DBFS_RUN_FUSE_TESTS=1 cargo test --test shutdown -- --ignored`.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

mod common;

const EXIT_DEADLINE: Duration = Duration::from_secs(5);

/// 200 KiB + 17 bytes: spans several 64 KiB chunks and ends part-way through one.
const PAYLOAD_LEN: usize = 200 * 1024 + 17;

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn sigterm_flushes_an_open_file_and_exits_zero() {
    assert_open_file_survives_signal(libc::SIGTERM);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn sigint_flushes_an_open_file_and_exits_zero() {
    assert_open_file_survives_signal(libc::SIGINT);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn sighup_flushes_an_open_file_and_exits_zero() {
    assert_open_file_survives_signal(libc::SIGHUP);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn sigterm_after_close_unmounts_and_exits_zero() {
    let _serial = require_fuse();
    let work = work_dir("shutdown-closed");
    let (db, mnt) = (work.join("db.sqlite"), work.join("mnt"));
    let payload = payload();

    let mut dbfs = Dbfs::start(&db, &mnt);
    write_and_close(&mnt.join("notes.bin"), &payload);

    send_signal(dbfs.pid(), libc::SIGTERM);
    let (status, stderr) = dbfs.wait_for_exit();
    assert_eq!(status.code(), Some(0), "dbfs exit status, stderr: {stderr}");
    assert!(!is_mounted(&mnt), "mount still present after dbfs exited");

    drop(dbfs);
    let remounted = Dbfs::start(&db, &mnt);
    assert_eq!(read(&mnt.join("notes.bin")), payload);
    drop(remounted);
    let _ = fs::remove_dir_all(&work);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn external_unmount_exits_zero_and_keeps_data() {
    let _serial = require_fuse();
    let work = work_dir("shutdown-external");
    let (db, mnt) = (work.join("db.sqlite"), work.join("mnt"));
    let payload = payload();

    let mut dbfs = Dbfs::start(&db, &mnt);
    write_and_close(&mnt.join("notes.bin"), &payload);

    // Non-lazy: this only succeeds when nothing holds the mount open.
    let unmount = Command::new("fusermount3")
        .arg("-u")
        .arg(&mnt)
        .status()
        .expect("run fusermount3 -u");
    assert!(unmount.success(), "fusermount3 -u failed");

    let (status, stderr) = dbfs.wait_for_exit();
    assert_eq!(status.code(), Some(0), "dbfs exit status, stderr: {stderr}");

    drop(dbfs);
    let remounted = Dbfs::start(&db, &mnt);
    assert_eq!(read(&mnt.join("notes.bin")), payload);
    drop(remounted);
    let _ = fs::remove_dir_all(&work);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn failed_flush_on_shutdown_exits_one_with_a_message() {
    let _serial = require_fuse();
    let work = work_dir("shutdown-flush-fails");
    let (db, mnt) = (work.join("db.sqlite"), work.join("mnt"));
    let payload = payload();

    let mut dbfs = Dbfs::start(&db, &mnt);
    let mut file = File::create(mnt.join("held.bin")).expect("create file on mount");
    file.write_all(&payload).expect("write through mount");
    push_dirty_pages(&file);

    // Hold SQLite's write lock, so dbfs's flush cannot commit. It waits out its 5 s
    // busy timeout and then fails. The test must report that, not exit 0.
    let locker = Connection::open(&db).expect("open database");
    locker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("take write lock");

    send_signal(dbfs.pid(), libc::SIGTERM);
    let (status, stderr) = dbfs.wait_for_exit_within(Duration::from_secs(15));
    assert_eq!(status.code(), Some(1), "dbfs exit status, stderr: {stderr}");
    assert!(
        stderr.contains("failed to flush unsaved data on shutdown"),
        "stderr should name the failed flush: {stderr}"
    );
    assert!(!is_mounted(&mnt), "mount still present after dbfs exited");

    locker
        .execute_batch("ROLLBACK")
        .expect("release write lock");
    drop(locker);
    drop(file);
    drop(dbfs);
    let _ = fs::remove_dir_all(&work);
}

/// Writes a file through the mount and leaves the fd open, then sends `signal` to dbfs.
///
/// The fd stays open on purpose. The bytes reach dbfs before the signal, so they sit
/// in dbfs's dirty buffers when it arrives. In the default mode each `write` has
/// already reached dbfs. With `--kernel-cache` the writeback cache may still hold
/// them, so `push_dirty_pages` sends them first. dbfs
/// unmounts lazily, which is what `fusermount -u -z` does. A process that still has
/// the file open keeps the FUSE session alive, so a shutdown that waited for
/// `Filesystem::destroy` would never flush these bytes. The test therefore checks
/// that dbfs flushes while the mount is still up.
fn assert_open_file_survives_signal(signal: libc::c_int) {
    let _serial = require_fuse();
    let work = work_dir("shutdown-open");
    let (db, mnt) = (work.join("db.sqlite"), work.join("mnt"));
    let payload = payload();

    let mut dbfs = Dbfs::start(&db, &mnt);
    let mut file = File::create(mnt.join("held.bin")).expect("create file on mount");
    file.write_all(&payload).expect("write through mount");
    push_dirty_pages(&file);

    send_signal(dbfs.pid(), signal);
    let (status, stderr) = dbfs.wait_for_exit();
    assert_eq!(status.code(), Some(0), "dbfs exit status, stderr: {stderr}");
    assert!(!is_mounted(&mnt), "mount still present after dbfs exited");

    // Closing now fails with ENOTCONN, because the connection is gone. That is expected.
    drop(file);
    drop(dbfs);

    let remounted = Dbfs::start(&db, &mnt);
    assert_eq!(read(&mnt.join("held.bin")), payload, "bytes after remount");
    drop(remounted);
    let _ = fs::remove_dir_all(&work);
}

/// A running dbfs mount. Dropping it unmounts and kills the process.
struct Dbfs {
    child: Child,
    mountpoint: PathBuf,
}

impl Dbfs {
    fn start(db: &Path, mountpoint: &Path) -> Self {
        fs::create_dir_all(mountpoint).expect("create mountpoint");
        let child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
            .arg("mount")
            .args(common::mount_args(&[]))
            .arg(db)
            .arg(mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start dbfs");
        let mut dbfs = Self {
            child,
            mountpoint: mountpoint.to_path_buf(),
        };
        if !wait_for_mount(mountpoint) {
            let (_, stderr) = dbfs.wait_for_exit_or_kill();
            panic!("dbfs did not mount {}: {stderr}", mountpoint.display());
        }
        dbfs
    }

    fn pid(&self) -> libc::pid_t {
        self.child.id() as libc::pid_t
    }

    /// Waits for dbfs to exit and returns its status and stderr. Kills it and panics
    /// if it is still running after `EXIT_DEADLINE`.
    fn wait_for_exit(&mut self) -> (ExitStatus, String) {
        self.wait_for_exit_within(EXIT_DEADLINE)
    }

    fn wait_for_exit_within(&mut self, timeout: Duration) -> (ExitStatus, String) {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll dbfs") {
                return (status, self.take_stderr());
            }
            if Instant::now() >= deadline {
                let (_, stderr) = self.wait_for_exit_or_kill();
                panic!("dbfs did not exit within {timeout:?}, stderr: {stderr}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_exit_or_kill(&mut self) -> (ExitStatus, String) {
        let _ = self.child.kill();
        let status = self.child.wait().expect("wait for dbfs");
        (status, self.take_stderr())
    }

    fn take_stderr(&mut self) -> String {
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        stderr
    }
}

impl Drop for Dbfs {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Makes the kernel send this file's dirty pages to dbfs and waits until dbfs has
/// replied. dbfs only buffers them; nothing is committed. It does nothing when the
/// writes have already reached dbfs, as in the default mode.
fn push_dirty_pages(file: &File) {
    use std::os::unix::io::AsRawFd;

    let flags = libc::SYNC_FILE_RANGE_WAIT_BEFORE
        | libc::SYNC_FILE_RANGE_WRITE
        | libc::SYNC_FILE_RANGE_WAIT_AFTER;
    // SAFETY: the fd is valid for the life of `file`, and sync_file_range takes no pointers.
    let rc = unsafe { libc::sync_file_range(file.as_raw_fd(), 0, 0, flags) };
    assert_eq!(
        rc,
        0,
        "sync_file_range: {}",
        std::io::Error::last_os_error()
    );
}

fn send_signal(pid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: kill only sends a signal to the given pid.
    let result = unsafe { libc::kill(pid, signal) };
    assert_eq!(result, 0, "kill({pid}, {signal}) failed");
}

/// Writes and closes. Close, not fsync, is what makes dbfs flush this file.
fn write_and_close(path: &Path, bytes: &[u8]) {
    let mut file = File::create(path).expect("create file on mount");
    file.write_all(bytes).expect("write through mount");
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).expect("read file through mount")
}

fn payload() -> Vec<u8> {
    (0..PAYLOAD_LEN).map(|i| (i % 251) as u8).collect()
}

/// Serializes the tests in this file. Each test holds a file open with unflushed
/// data. If another test forks a process meanwhile (to start dbfs or run
/// fusermount3), the child inherits that fd and closes it at exec. The kernel then
/// sends FUSE_FLUSH, which commits the data early and breaks the test's
/// assumption that the data is still dirty when the signal arrives.
static SERIAL: Mutex<()> = Mutex::new(());

fn require_fuse() -> MutexGuard<'static, ()> {
    if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none() || !command_exists("fusermount3") {
        panic!("shutdown tests require DBFS_RUN_FUSE_TESTS=1 and fusermount3");
    }
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        path.to_string_lossy()
            .split(':')
            .any(|dir| Path::new(dir).join(name).is_file())
    })
}

fn is_mounted(path: &Path) -> bool {
    Command::new("mountpoint")
        .arg("-q")
        .arg(path)
        .status()
        .is_ok_and(|status| status.success())
}

fn wait_for_mount(mountpoint: &Path) -> bool {
    for _ in 0..50 {
        if is_mounted(mountpoint) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// A fresh directory under the repository's `.bench/`. Never `/tmp`, which is tmpfs here.
fn work_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".bench")
        .join(format!("{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create disk-backed work dir");
    dir
}
