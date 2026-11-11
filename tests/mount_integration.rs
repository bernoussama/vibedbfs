//! Tests that mount dbfs through FUSE. Each one is `#[ignore]`d and panics unless
//! `DBFS_RUN_FUSE_TESTS=1` is set and `fusermount3` exists. Run them with:
//!
//! ```text
//! DBFS_RUN_FUSE_TESTS=1 cargo test --test mount_integration -- --ignored
//! ```

use std::ffi::CString;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dbfs::Db;
use rusqlite::Connection;

mod common;

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn mounted_filesystem_supports_basic_file_lifecycle_when_enabled() {
    require_fuse_gate();

    let base = temp_path("mount-integration");
    let db_path = base.with_extension("sqlite");
    let mountpoint = base.with_extension("mnt");
    fs::create_dir(&mountpoint).expect("create mountpoint");

    let mut child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
        .arg("mount")
        .args(common::mount_args(&[]))
        .arg(&db_path)
        .arg(&mountpoint)
        .spawn()
        .expect("start dbfs mount");

    wait_for_mount(&mountpoint);

    fs::create_dir(mountpoint.join("docs")).expect("create directory through mount");
    fs::write(mountpoint.join("docs/readme.txt"), b"hello dbfs").expect("write file through mount");
    assert_eq!(
        fs::read(mountpoint.join("docs/readme.txt")).expect("read file through mount"),
        b"hello dbfs"
    );
    fs::rename(
        mountpoint.join("docs/readme.txt"),
        mountpoint.join("docs/notes.txt"),
    )
    .expect("rename through mount");
    fs::remove_file(mountpoint.join("docs/notes.txt")).expect("remove file through mount");
    fs::remove_dir(mountpoint.join("docs")).expect("remove dir through mount");

    unmount(&mountpoint, &mut child);
    let _ = fs::remove_dir(&mountpoint);
    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(db_path.with_extension("sqlite-wal"));
    let _ = fs::remove_file(db_path.with_extension("sqlite-shm"));
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn utimens_round_trips_nanoseconds_when_enabled() {
    let mounted = Mounted::start("utimens-nsec");

    let path = mounted.mountpoint.join("notes.txt");
    fs::write(&path, b"hello").expect("create file through mount");
    let atime = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
    let mtime = UNIX_EPOCH + Duration::new(1_700_000_111, 987_654_321);
    fs::set_times(
        &path,
        FileTimes::new().set_accessed(atime).set_modified(mtime),
    )
    .expect("utimens through mount");

    let meta = fs::metadata(&path).expect("stat file through mount");
    assert_eq!(meta.atime(), 1_700_000_000, "atime seconds");
    assert_eq!(meta.atime_nsec(), 123_456_789, "atime nanoseconds");
    assert_eq!(meta.mtime(), 1_700_000_111, "mtime seconds");
    assert_eq!(meta.mtime_nsec(), 987_654_321, "mtime nanoseconds");
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn statfs_reports_sqlite_blocks_and_inodes_when_enabled() {
    let mounted = Mounted::start("statfs");

    fs::write(mounted.mountpoint.join("notes.txt"), b"hello").expect("create file through mount");

    let mut stat = unsafe { std::mem::zeroed::<libc::statvfs>() };
    let path = CString::new(mounted.mountpoint.to_str().expect("utf-8 mount path")).unwrap();
    let rc = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
    assert_eq!(rc, 0, "statvfs");

    let conn = Connection::open(&mounted.db_path).expect("open database");
    let page_size = pragma_u64(&conn, "page_size");
    let page_count = pragma_u64(&conn, "page_count");
    let freelist = pragma_u64(&conn, "freelist_count");
    let files: i64 = conn
        .query_row("SELECT COUNT(*) FROM inodes", [], |row| row.get(0))
        .expect("count inodes");
    let files = files as u64;

    assert_eq!(stat.f_bsize as u64, page_size, "block size");
    assert_eq!(stat.f_frsize as u64, page_size, "fragment size");
    assert_eq!(stat.f_blocks, page_count, "block count");
    assert_eq!(stat.f_bfree, freelist, "free blocks");
    assert_eq!(stat.f_bavail, freelist, "available blocks");
    assert_eq!(stat.f_files, files, "inode count");
    assert!(stat.f_blocks > 0, "block count is derived");
    assert!(stat.f_files >= 2, "root and the new file are counted");
    assert_eq!(stat.f_namemax, 255, "name max");
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn rename_noreplace_does_not_replace_an_existing_target_when_enabled() {
    let mounted = Mounted::start("rename-noreplace");

    let source = mounted.mountpoint.join("source.txt");
    let target = mounted.mountpoint.join("target.txt");
    let fresh = mounted.mountpoint.join("fresh.txt");
    fs::write(&source, b"source").expect("write source");
    fs::write(&target, b"target").expect("write target");

    let rc = rename_at(&source, &target, libc::RENAME_NOREPLACE);
    let error = std::io::Error::last_os_error();
    assert_eq!(rc, -1, "noreplace of an existing file");
    assert_eq!(error.raw_os_error(), Some(libc::EEXIST));
    assert_eq!(fs::read(&source).expect("source remains"), b"source");
    assert_eq!(fs::read(&target).expect("target unchanged"), b"target");

    let rc = rename_at(&source, &fresh, libc::RENAME_NOREPLACE);
    assert_eq!(
        rc,
        0,
        "noreplace of a missing file: {}",
        std::io::Error::last_os_error()
    );
    assert!(!source.exists());
    assert_eq!(fs::read(&fresh).expect("fresh contents"), b"source");

    fs::write(&source, b"again").expect("rewrite source");
    let rc = rename_at(&source, &fresh, libc::RENAME_EXCHANGE);
    let error = std::io::Error::last_os_error();
    assert_eq!(rc, -1, "unknown rename flag");
    assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    assert_eq!(fs::read(&source).expect("source kept"), b"again");
    assert_eq!(fs::read(&fresh).expect("fresh kept"), b"source");
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn rm_rf_removes_every_entry_of_a_large_directory_when_enabled() {
    // The kernel lists a directory in buffer-sized batches. `rm -rf` deletes
    // each batch before asking for the next, so positional readdir offsets
    // used to skip entries and leave the directory non-empty.
    let mount = BenchMount::start("rm-rf");
    let tree = mount.mountpoint().join("tree");
    std::fs::create_dir(&tree).expect("mkdir tree");
    for i in 0..5_000 {
        File::create(tree.join(format!("entry-{i:05}"))).expect("create entry");
    }
    let status = std::process::Command::new("rm")
        .arg("-rf")
        .arg(&tree)
        .status()
        .expect("run rm -rf");
    assert!(status.success(), "rm -rf failed: {status}");
    assert!(!tree.exists(), "rm -rf left the directory behind");
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn mounted_fallocate_extends_size_and_reads_zeros_when_enabled() {
    const MIB: u64 = 1 << 20;
    let mount = BenchMount::start("fallocate");
    let path = mount.mountpoint().join("sparse.bin");
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)
        .expect("create file through mount");
    file.write_at(b"head", 0).expect("write through mount");

    fallocate_fd(&file, 0, 0, MIB as i64).expect("fallocate mode 0 through mount");
    assert_eq!(
        file.metadata().expect("stat").len(),
        MIB,
        "size after fallocate"
    );
    assert_head_then_zeros(&file, b"head", MIB);

    posix_fallocate_fd(&file, MIB as i64, 4096).expect("posix_fallocate through mount");
    assert_eq!(file.metadata().expect("stat").len(), MIB + 4096);
    assert_head_then_zeros(&file, b"head", MIB + 4096);

    fallocate_fd(&file, libc::FALLOC_FL_KEEP_SIZE, 8 * MIB as i64, 4096)
        .expect("keep-size fallocate beyond EOF");
    assert_eq!(
        file.metadata().expect("stat").len(),
        MIB + 4096,
        "KEEP_SIZE leaves the size alone"
    );

    assert_eq!(
        fallocate_fd(
            &file,
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            0,
            4096
        )
        .expect_err("punch hole through mount"),
        libc::EOPNOTSUPP
    );
    assert_eq!(
        fallocate_fd(&file, libc::FALLOC_FL_ZERO_RANGE, 0, 4096).expect_err("zero range"),
        libc::EOPNOTSUPP
    );
    assert_eq!(file.metadata().expect("stat").len(), MIB + 4096);
    assert_head_then_zeros(&file, b"head", MIB + 4096);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn mounted_xattr_calls_return_the_documented_errnos_and_an_empty_list_when_enabled() {
    let mount = BenchMount::start("xattr");
    let path = mount.mountpoint().join("notes.txt");
    fs::write(&path, b"hello").expect("create file through mount");

    let file = c_string(&path);
    let root = c_string(mount.mountpoint());
    let name = CString::new("user.x").expect("attribute name");
    let value = b"1";

    // getxattr: the first call reaches dbfs, which replies ENOSYS. The kernel
    // reports EOPNOTSUPP and stops sending getxattr for the mount, so the second
    // call (a real buffer) fails the same way without a request.
    for size in [0usize, 64] {
        let mut buf = vec![0u8; size];
        let rc =
            unsafe { libc::getxattr(file.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), size) };
        assert_eq!(rc, -1, "getxattr with size {size} must fail");
        assert_eq!(libc_errno(), libc::EOPNOTSUPP, "getxattr with size {size}");
    }

    // listxattr: the size probe returns 0 (an empty list) on files and the root.
    for target in [&file, &root] {
        let rc = unsafe { libc::listxattr(target.as_ptr(), std::ptr::null_mut(), 0) };
        assert_eq!(rc, 0, "listxattr size probe on {target:?}");
    }
    let mut names = [0u8; 64];
    let rc = unsafe { libc::listxattr(file.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    assert_eq!(
        rc, 0,
        "listxattr into a non-empty buffer returns zero bytes"
    );

    // setxattr: refused with EOPNOTSUPP.
    let rc = unsafe {
        libc::setxattr(
            file.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    assert_eq!(rc, -1, "setxattr must fail");
    assert_eq!(libc_errno(), libc::EOPNOTSUPP, "setxattr errno");

    // removexattr: the attribute does not exist, so ENODATA.
    let rc = unsafe { libc::removexattr(file.as_ptr(), name.as_ptr()) };
    assert_eq!(rc, -1, "removexattr must fail");
    assert_eq!(libc_errno(), libc::ENODATA, "removexattr errno");
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn mounted_cp_a_copies_a_directory_tree_when_enabled() {
    use std::os::unix::fs::PermissionsExt;

    let mount = BenchMount::start("cp-a");
    let src = mount.base().join("src");
    fs::create_dir_all(src.join("a/b")).expect("create source tree");
    fs::create_dir(src.join("empty")).expect("create empty directory");
    fs::write(src.join("a/b/file.txt"), b"hello").expect("write small file");
    fs::set_permissions(src.join("a/b/file.txt"), fs::Permissions::from_mode(0o640))
        .expect("chmod source file");
    let big: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
    fs::write(src.join("a/big.bin"), &big).expect("write big file");
    std::os::unix::fs::symlink("b/file.txt", src.join("a/link")).expect("create symlink");

    let dst = mount.mountpoint().join("dst");
    let status = Command::new("cp")
        .arg("-a")
        .arg(&src)
        .arg(&dst)
        .status()
        .expect("run cp -a");
    assert!(status.success(), "cp -a onto dbfs failed: {status}");

    assert_eq!(
        fs::read(dst.join("a/b/file.txt")).expect("read copy"),
        b"hello"
    );
    assert_eq!(fs::read(dst.join("a/big.bin")).expect("read big copy"), big);
    assert_eq!(
        fs::read_link(dst.join("a/link")).expect("readlink copy"),
        Path::new("b/file.txt")
    );
    assert!(dst.join("empty").is_dir());
    assert_eq!(
        fs::metadata(dst.join("a/b/file.txt"))
            .expect("stat copy")
            .permissions()
            .mode()
            & 0o777,
        0o640,
        "cp -a preserves the mode"
    );
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn mount_teardown_detaches_a_busy_mount_instead_of_hanging_when_enabled() {
    let mount = BenchMount::start("busy-teardown");
    let mountpoint = mount.mountpoint().to_path_buf();
    // A process whose working directory is inside the mount makes the first
    // umount fail with EBUSY.
    let mut holder = Command::new("sleep")
        .arg("600")
        .current_dir(&mountpoint)
        .spawn()
        .expect("start a process inside the mount");

    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(mount);
        let _ = done.send(());
    });
    let teardown = finished.recv_timeout(Duration::from_secs(60));
    let _ = holder.kill();
    let _ = holder.wait();

    assert!(teardown.is_ok(), "teardown of a busy mount did not finish");
    let mounts = fs::read_to_string("/proc/mounts").expect("read /proc/mounts");
    assert!(
        !mounts
            .lines()
            .any(|line| line.split(' ').nth(1) == mountpoint.to_str()),
        "mount is still attached after teardown"
    );
}

/// Calls `fallocate(2)` on an open file and maps failure to its errno.
fn fallocate_fd(file: &File, mode: i32, offset: i64, len: i64) -> Result<(), i32> {
    use std::os::unix::io::AsRawFd;

    let rc = unsafe { libc::fallocate(file.as_raw_fd(), mode, offset, len) };
    if rc == 0 { Ok(()) } else { Err(libc_errno()) }
}

/// Calls `posix_fallocate(3)`, which returns the errno directly.
fn posix_fallocate_fd(file: &File, offset: i64, len: i64) -> Result<(), i32> {
    use std::os::unix::io::AsRawFd;

    let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), offset, len) };
    if rc == 0 { Ok(()) } else { Err(rc) }
}

/// Reads `len` bytes from the start and checks that they are `head`, then zeros.
fn assert_head_then_zeros(file: &File, head: &[u8], len: u64) {
    let mut data = vec![0u8; len as usize];
    file.read_exact_at(&mut data, 0)
        .expect("read whole file through mount");
    assert_eq!(&data[..head.len()], head);
    assert!(
        data[head.len()..].iter().all(|&byte| byte == 0),
        "bytes past the written head must read as zeros"
    );
}

fn c_string(path: &Path) -> CString {
    use std::os::unix::ffi::OsStrExt;

    CString::new(path.as_os_str().as_bytes()).expect("path without NUL bytes")
}

fn libc_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn rename_at(from: &Path, to: &Path, flags: libc::c_uint) -> i32 {
    let from = CString::new(from.to_str().expect("utf-8 source")).unwrap();
    let to = CString::new(to.to_str().expect("utf-8 target")).unwrap();
    unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            flags,
        )
    }
}

fn pragma_u64(conn: &Connection, name: &str) -> u64 {
    let sql = format!("PRAGMA {name}");
    let value: i64 = conn
        .query_row(&sql, [], |row| row.get(0))
        .unwrap_or_else(|error| panic!("read {name}: {error}"));
    value as u64
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn close_after_many_small_random_writes_does_not_stall_other_requests_when_enabled() {
    require_fuse_gate();

    const BLOCK: u64 = 4096;
    const BLOCKS: u64 = 65_536;
    const DEADLINE: Duration = Duration::from_secs(4);

    let db_path = temp_path("random-writes").with_extension("sqlite");
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    Db::open(&db_path)
        .expect("open database")
        .create_file(1, b"other.txt", 0o644, uid, gid)
        .expect("create file the kernel has not looked up yet");
    let mount = Mount::start(db_path);
    let mountpoint = &mount.mountpoint;

    let file = File::create(mountpoint.join("random.bin")).expect("create random.bin");
    let mut block = vec![0u8; BLOCK as usize];
    for i in 0..BLOCKS {
        let index = (i * 40_503) % BLOCKS;
        block[..8].copy_from_slice(&index.to_le_bytes());
        file.write_all_at(&block, index * BLOCK)
            .expect("pwrite 4 KiB block");
    }

    let closer = std::thread::spawn(move || {
        let start = Instant::now();
        drop(file);
        start.elapsed()
    });
    std::thread::sleep(Duration::from_millis(100));
    let stat_start = Instant::now();
    fs::metadata(mountpoint.join("other.txt")).expect("stat other.txt");
    let stat_elapsed = stat_start.elapsed();
    let close_elapsed = closer.join().expect("join closer");

    assert!(
        close_elapsed < DEADLINE,
        "close after {BLOCKS} random 4 KiB writes took {close_elapsed:?}"
    );
    assert!(
        stat_elapsed < DEADLINE,
        "stat of another file during close took {stat_elapsed:?}"
    );

    let file = File::open(mountpoint.join("random.bin")).expect("reopen random.bin");
    assert_eq!(
        file.metadata().expect("stat random.bin").len(),
        BLOCKS * BLOCK
    );
    let mut stamp = [0u8; 8];
    for index in (0..BLOCKS).step_by(997) {
        file.read_exact_at(&mut stamp, index * BLOCK)
            .expect("read block stamp");
        assert_eq!(
            u64::from_le_bytes(stamp),
            index,
            "block {index} holds wrong data"
        );
    }
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn large_write_keeps_daemon_memory_bounded_when_enabled() {
    require_fuse_gate();

    const MIB: u64 = 1 << 20;
    const WRITTEN_MIB: u64 = 512;
    // The default write buffer (256 MiB) may be full, plus room for SQLite and the
    // process. Without a bound on the buffer, the daemon would hold all of it.
    const RSS_LIMIT_MIB: u64 = 256 + 64;

    let mount = Mount::start(temp_path("large-write").with_extension("sqlite"));
    let mut file = File::create(mount.mountpoint.join("large.bin")).expect("create large.bin");
    let mut block = vec![0u8; MIB as usize];
    for i in 0..WRITTEN_MIB {
        block.fill(i as u8);
        file.write_all(&block).expect("write 1 MiB block");
    }

    let rss = daemon_rss_bytes(mount.child.id());
    drop(file);
    assert!(
        rss < RSS_LIMIT_MIB * MIB,
        "daemon RSS was {} MiB after writing {WRITTEN_MIB} MiB",
        rss / MIB
    );

    let file = File::open(mount.mountpoint.join("large.bin")).expect("reopen large.bin");
    assert_eq!(
        file.metadata().expect("stat large.bin").len(),
        WRITTEN_MIB * MIB
    );
    let mut last = vec![0u8; MIB as usize];
    file.read_exact_at(&mut last, (WRITTEN_MIB - 1) * MIB)
        .expect("read last block");
    assert!(last.iter().all(|&byte| byte == (WRITTEN_MIB - 1) as u8));
}

fn daemon_rss_bytes(pid: u32) -> u64 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).expect("read daemon status");
    let kib: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("parse VmRSS");
    kib * 1024
}

struct Mount {
    db_path: PathBuf,
    mountpoint: PathBuf,
    child: Child,
}

impl Mount {
    fn start(db_path: PathBuf) -> Self {
        require_fuse_gate();
        let mountpoint = db_path.with_extension("mnt");
        fs::create_dir(&mountpoint).expect("create mountpoint");
        let child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
            .arg("mount")
            .args(common::mount_args(&[]))
            .arg(&db_path)
            .arg(&mountpoint)
            .spawn()
            .expect("start dbfs mount");
        wait_for_mount(&mountpoint);
        Self {
            db_path,
            mountpoint,
            child,
        }
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        unmount(&self.mountpoint, &mut self.child);
        let _ = fs::remove_dir(&self.mountpoint);
        let _ = fs::remove_file(&self.db_path);
        let _ = fs::remove_file(self.db_path.with_extension("sqlite-wal"));
        let _ = fs::remove_file(self.db_path.with_extension("sqlite-shm"));
    }
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn mounted_sparse_file_past_4gib_reads_marker_at_that_offset() {
    require_fuse_gate();

    const FOUR_GIB: u64 = 1 << 32;
    let marker_at = FOUR_GIB + 64;
    let base = bench_dir("mount-sparse-4gib");
    let db_path = base.join("fs.sqlite");
    let mountpoint = base.join("mnt");
    fs::create_dir(&mountpoint).expect("create mountpoint");

    let mut mounted = FuseMount::start(&db_path, &mountpoint);
    let file_path = mountpoint.join("sparse.bin");
    {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&file_path)
            .expect("create sparse file");
        file.write_at(b"MARK", marker_at)
            .expect("write marker through mount");
        file.set_len(marker_at + FOUR_GIB)
            .expect("extend file so more than 4 GiB remains after the marker");
        file.sync_all().expect("flush marker");
    }
    mounted.unmount();

    let mut mounted = FuseMount::start(&db_path, &mountpoint);
    let file = File::open(&file_path).expect("reopen sparse file");
    let mut buf = [0u8; 4];
    let n = file
        .read_at(&mut buf, marker_at)
        .expect("read marker through mount");
    assert_eq!(n, 4);
    assert_eq!(&buf, b"MARK");
    drop(file);
    mounted.unmount();

    let _ = fs::remove_dir_all(&base);
}

struct FuseMount {
    mountpoint: PathBuf,
    child: Option<Child>,
}

impl FuseMount {
    fn start(db_path: &Path, mountpoint: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
            .arg("mount")
            .args(common::mount_args(&[]))
            .arg(db_path)
            .arg(mountpoint)
            .spawn()
            .expect("start dbfs mount");
        let mounted = Self {
            mountpoint: mountpoint.to_path_buf(),
            child: Some(child),
        };
        wait_for_mount(mountpoint);
        mounted
    }

    fn unmount(&mut self) {
        if let Some(child) = self.child.as_mut() {
            unmount(&self.mountpoint, child);
        }
        self.child = None;
    }
}

impl Drop for FuseMount {
    fn drop(&mut self) {
        self.unmount();
    }
}

/// A dbfs mount whose database and mountpoint live under `.bench/`. Drop
/// unmounts it, then deletes its files unless the unmount failed.
struct BenchMount {
    base: PathBuf,
    mount: FuseMount,
}

impl BenchMount {
    fn start(name: &str) -> Self {
        require_fuse_gate();
        let base = bench_dir(name);
        let mountpoint = base.join("mnt");
        fs::create_dir(&mountpoint).expect("create mountpoint");
        let mount = FuseMount::start(&base.join("fs.sqlite"), &mountpoint);
        Self { base, mount }
    }

    /// Host-side scratch directory next to the database, outside the mount.
    fn base(&self) -> &Path {
        &self.base
    }

    fn mountpoint(&self) -> &Path {
        &self.mount.mountpoint
    }
}

impl Drop for BenchMount {
    fn drop(&mut self) {
        self.mount.unmount();
        // While the mount is still up, its mountpoint shows dbfs contents, not host files.
        let still_mounted = Command::new("mountpoint")
            .arg("-q")
            .arg(&self.mount.mountpoint)
            .status()
            .is_ok_and(|status| status.success());
        if !still_mounted {
            let _ = fs::remove_dir_all(&self.base);
        }
    }
}

fn bench_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".bench")
        .join(format!("{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create disk-backed test directory");
    dir
}

/// Panics unless the FUSE gate is open. The mount tests are `#[ignore]`d, so this
/// runs only under `-- --ignored`, and a missing prerequisite fails instead of passing.
fn require_fuse_gate() {
    if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none() || command_path("fusermount3").is_none() {
        panic!("FUSE mount tests require DBFS_RUN_FUSE_TESTS=1 and fusermount3");
    }
}

fn command_path(command: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .find_map(|dir| {
            let path = Path::new(dir).join(command);
            path.exists().then_some(path)
        })
}

fn temp_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dbfs-{name}-{}-{nanos}", std::process::id()))
}

fn wait_for_mount(mountpoint: &Path) {
    for _ in 0..50 {
        if Command::new("mountpoint")
            .arg("-q")
            .arg(mountpoint)
            .status()
            .expect("run mountpoint")
            .success()
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("mount did not become ready");
}

/// Unmounts `mountpoint` and waits for the daemon to exit. It never blocks
/// indefinitely: a plain umount can fail with EBUSY while a transient reference
/// to the mount exists, so it is retried, then detached lazily, and a daemon
/// that still does not exit is killed.
fn unmount(mountpoint: &Path, child: &mut Child) {
    if !fusermount_unmount(mountpoint, false) {
        let retried = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            fusermount_unmount(mountpoint, false)
        });
        if !retried {
            eprintln!(
                "{} could not be unmounted; detaching it lazily",
                mountpoint.display()
            );
            fusermount_unmount(mountpoint, true);
        }
    }
    wait_for_exit(child);
}

fn fusermount_unmount(mountpoint: &Path, lazy: bool) -> bool {
    let mut command = Command::new("fusermount3");
    command.arg("-u");
    if lazy {
        command.arg("-z");
    }
    command
        .arg(mountpoint)
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn wait_for_exit(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => break,
            _ => return,
        }
    }
    eprintln!(
        "dbfs pid {} did not exit after unmount; killing it",
        child.id()
    );
    let _ = child.kill();
    let _ = child.wait();
}

struct Mounted {
    mountpoint: PathBuf,
    db_path: PathBuf,
    child: Child,
}

impl Mounted {
    fn start(name: &str) -> Self {
        require_fuse_gate();
        let base = temp_path(name);
        let db_path = base.with_extension("sqlite");
        let mountpoint = base.with_extension("mnt");
        fs::create_dir(&mountpoint).expect("create mountpoint");
        let child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
            .arg("mount")
            .args(common::mount_args(&[]))
            .arg(&db_path)
            .arg(&mountpoint)
            .spawn()
            .expect("start dbfs mount");
        let mounted = Self {
            mountpoint,
            db_path,
            child,
        };
        wait_for_mount(&mounted.mountpoint);
        mounted
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        unmount(&self.mountpoint, &mut self.child);
        let _ = fs::remove_dir(&self.mountpoint);
        let _ = fs::remove_file(&self.db_path);
        let _ = fs::remove_file(self.db_path.with_extension("sqlite-wal"));
        let _ = fs::remove_file(self.db_path.with_extension("sqlite-shm"));
    }
}

/// The system's page size, in bytes.
fn page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

/// Counts the pages of `file` that the kernel holds in its page cache. It uses
/// `mincore(2)` over a read-only mapping of the first `len` bytes. Mapping does not
/// fault pages in, so the count is what the kernel held before any read.
fn cached_pages(file: &File, len: usize) -> usize {
    use std::os::unix::io::AsRawFd;

    let mut residency = vec![0u8; len.div_ceil(page_size())];
    // SAFETY: the mapping, the residency vector and the length given to mincore
    // all describe the same `len` bytes, and the mapping is removed before returning.
    unsafe {
        let map = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        );
        assert_ne!(
            map,
            libc::MAP_FAILED,
            "mmap: {}",
            std::io::Error::last_os_error()
        );
        let rc = libc::mincore(map, len, residency.as_mut_ptr());
        assert_eq!(rc, 0, "mincore: {}", std::io::Error::last_os_error());
        libc::munmap(map, len);
    }
    residency.iter().filter(|byte| **byte & 1 != 0).count()
}

/// The modification time as (seconds, nanoseconds), which orders like time.
fn mtime_of(meta: &fs::Metadata) -> (i64, i64) {
    (meta.mtime(), meta.mtime_nsec())
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn warm_reread_after_reopen_keeps_the_page_cache_unless_no_keep_cache() {
    const LEN: usize = 1 << 20;
    let mounted = Mounted::start("warm-reread");
    let path = mounted.mountpoint.join("warm.bin");
    let data: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();
    fs::write(&path, &data).expect("write file through mount");
    assert_eq!(fs::read(&path).expect("first read"), data);

    // Nothing is read between the open and the check, so the pages counted here
    // are the ones the open left in the page cache.
    let file = File::open(&path).expect("reopen file");
    let cached = cached_pages(&file, LEN);
    if common::keeps_page_cache() {
        assert_eq!(
            cached,
            LEN / page_size(),
            "the default mode keeps cached pages on open"
        );
    } else {
        assert_eq!(cached, 0, "--no-keep-cache drops cached pages on open");
    }
    assert_eq!(fs::read(&path).expect("warm re-read"), data);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn size_and_mtime_seen_by_one_fd_follow_writes_through_another() {
    let mounted = Mounted::start("size-mtime");
    let path = mounted.mountpoint.join("shared.txt");
    let writer = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)
        .expect("create file");
    let other = OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open second fd");

    writer.write_all_at(b"hello", 0).expect("first write");
    assert_eq!(writer.metadata().expect("fstat").len(), 5);

    std::thread::sleep(Duration::from_millis(20));
    let before = mtime_of(&fs::metadata(&path).expect("stat"));
    other
        .write_all_at(b"world!", 100)
        .expect("write through the second fd");
    let by_path = fs::metadata(&path).expect("stat by path");
    let by_fd = writer.metadata().expect("fstat through the first fd");
    assert_eq!(by_path.len(), 106, "size seen by path");
    assert_eq!(by_fd.len(), 106, "size seen through the first fd");
    assert!(
        mtime_of(&by_path) > before,
        "mtime moves forward after a write through another fd"
    );
    assert_eq!(
        mtime_of(&by_fd),
        mtime_of(&by_path),
        "both fds see the same mtime"
    );

    std::thread::sleep(Duration::from_millis(20));
    let before = mtime_of(&by_path);
    other.set_len(3).expect("truncate through the second fd");
    let by_path = fs::metadata(&path).expect("stat after truncate");
    assert_eq!(by_path.len(), 3, "size after truncate, seen by path");
    assert_eq!(
        writer.metadata().expect("fstat after truncate").len(),
        3,
        "size after truncate, seen through the first fd"
    );
    assert!(
        mtime_of(&by_path) > before,
        "mtime moves forward after truncate through another fd"
    );
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn rename_over_a_hard_link_updates_nlink_of_the_surviving_name() {
    let mounted = Mounted::start("rename-hard-link-nlink");
    let root = &mounted.mountpoint;
    fs::write(root.join("target"), b"keep").expect("create target");
    fs::hard_link(root.join("target"), root.join("alias")).expect("hard link");
    assert_eq!(
        fs::metadata(root.join("alias"))
            .expect("stat alias")
            .nlink(),
        2
    );

    fs::write(root.join("replacement"), b"new").expect("create replacement");
    fs::rename(root.join("replacement"), root.join("target")).expect("rename over target");

    assert_eq!(
        fs::metadata(root.join("alias"))
            .expect("stat alias")
            .nlink(),
        1,
        "the replaced inode lost one name"
    );
    assert_eq!(fs::read(root.join("alias")).expect("read alias"), b"keep");
    assert_eq!(fs::read(root.join("target")).expect("read target"), b"new");
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn rename_over_an_open_file_reports_nlink_zero_through_the_open_fd() {
    let mounted = Mounted::start("rename-open-nlink");
    let root = &mounted.mountpoint;
    fs::write(root.join("victim"), b"old").expect("create victim");
    let open = File::open(root.join("victim")).expect("open victim");
    assert_eq!(open.metadata().expect("fstat victim").nlink(), 1);

    fs::write(root.join("new"), b"new").expect("create replacement");
    fs::rename(root.join("new"), root.join("victim")).expect("rename over open file");

    assert_eq!(
        open.metadata().expect("fstat after rename").nlink(),
        0,
        "the replaced inode has no names left"
    );
    let mut buf = [0u8; 3];
    open.read_exact_at(&mut buf, 0)
        .expect("read through the open fd");
    assert_eq!(&buf, b"old", "the open file keeps its data");
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn directory_nlink_follows_mkdir_rmdir_and_cross_directory_rename() {
    let mounted = Mounted::start("dir-nlink");
    let root = &mounted.mountpoint;
    let nlink = |path: &Path| fs::metadata(path).expect("stat directory").nlink();
    let root_before = nlink(root);

    fs::create_dir(root.join("a")).expect("mkdir a");
    fs::create_dir(root.join("b")).expect("mkdir b");
    assert_eq!(
        nlink(root),
        root_before + 2,
        "mkdir adds a link to the parent"
    );

    fs::create_dir(root.join("a/x")).expect("mkdir a/x");
    assert_eq!(nlink(&root.join("a")), 3, "a subdirectory adds a link to a");

    fs::rename(root.join("a/x"), root.join("b/x")).expect("cross-directory rename");
    assert_eq!(nlink(&root.join("a")), 2, "the old parent loses the link");
    assert_eq!(nlink(&root.join("b")), 3, "the new parent gains the link");

    fs::remove_dir(root.join("b/x")).expect("rmdir b/x");
    assert_eq!(nlink(&root.join("b")), 2, "rmdir removes the link");
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn fallocate_extension_updates_size_and_mtime_seen_by_path_stat() {
    let mounted = Mounted::start("fallocate-attrs");
    let path = mounted.mountpoint.join("grow.bin");
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)
        .expect("create file");
    file.write_all_at(b"abcd", 0).expect("write");
    let before = fs::metadata(&path).expect("stat before fallocate");
    assert_eq!(before.len(), 4);

    std::thread::sleep(Duration::from_millis(20));
    fallocate_fd(&file, 0, 0, 1 << 20).expect("fallocate extends the file");

    let after = fs::metadata(&path).expect("stat after fallocate");
    assert_eq!(after.len(), 1 << 20, "size seen by path");
    assert_eq!(
        file.metadata().expect("fstat after fallocate").len(),
        1 << 20,
        "size seen through the open fd"
    );
    assert!(
        mtime_of(&after) > mtime_of(&before),
        "mtime moves forward after fallocate"
    );
}
