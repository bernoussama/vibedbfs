//! Crash safety through a real mount. The dbfs process is killed with SIGKILL,
//! then the tests check that fsynced files are intact and that the database
//! passes `PRAGMA integrity_check`.
//!
//! Run with: `DBFS_RUN_FUSE_TESTS=1 cargo test --locked --test crash -- --include-ignored --test-threads=1`
//!
//! No fsync is issued on directories: dbfs does not implement `fsyncdir`.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

mod common;

const MIB: usize = 1 << 20;

fn require_fuse() {
    if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none() || command_path("fusermount3").is_none() {
        panic!("FUSE mount tests require DBFS_RUN_FUSE_TESTS=1 and fusermount3");
    }
}

fn command_path(command: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|dir| Path::new(dir).join(command))
        .find(|candidate| candidate.is_file())
}

/// A fresh directory under the target dir (disk-backed, not tmpfs).
fn work_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("crash-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create work dir");
    dir
}

fn wait_for_mount(mountpoint: &Path) -> bool {
    for _ in 0..50 {
        if Command::new("mountpoint")
            .arg("-q")
            .arg(mountpoint)
            .status()
            .is_ok_and(|status| status.success())
        {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// A dbfs process mounted at `mountpoint`. Dropping it unmounts and kills it.
struct Mounted {
    child: Option<Child>,
    mountpoint: PathBuf,
}

impl Mounted {
    fn start(db: &Path, mountpoint: &Path, log: &Path, extra: &[&str]) -> Self {
        fs::create_dir_all(mountpoint).expect("create mountpoint");
        let log_file = File::create(log).expect("create dbfs log");
        let mut child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
            .arg("mount")
            .arg(db)
            .arg(mountpoint)
            .args(common::mount_args(extra))
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_file))
            .spawn()
            .expect("start dbfs");
        if !wait_for_mount(mountpoint) {
            let _ = child.kill();
            let _ = child.wait();
            let log = fs::read_to_string(log).unwrap_or_default();
            panic!("dbfs did not mount {}: {log}", mountpoint.display());
        }
        Self {
            child: Some(child),
            mountpoint: mountpoint.to_path_buf(),
        }
    }

    fn path(&self) -> &Path {
        &self.mountpoint
    }

    /// Sends SIGKILL, as a crash would, and checks that the process died of it.
    fn kill_9(&mut self) {
        let mut child = self.child.take().expect("dbfs is running");
        child.kill().expect("send SIGKILL to dbfs");
        let status = child.wait().expect("wait for dbfs");
        assert_eq!(
            status.signal(),
            Some(9),
            "dbfs must die of SIGKILL, got {status}"
        );
    }

    /// Lazy unmount: detaches the mount even while a process still holds a file open.
    fn unmount(&self) {
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        self.unmount();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Deterministic bytes that differ from one salt to the next.
fn pattern(len: usize, salt: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(13).wrapping_add(salt))
        .collect()
}

/// Size of the `-wal` file, or 0 when there is none.
fn wal_len(db: &Path) -> u64 {
    let mut name = db.as_os_str().to_owned();
    name.push("-wal");
    fs::metadata(name).map_or(0, |metadata| metadata.len())
}

fn write_synced(path: &Path, data: &[u8]) {
    let mut file = File::create(path).expect("create file");
    file.write_all(data).expect("write file");
    file.sync_all().expect("fsync file");
}

fn assert_integrity_ok(db: &Path) {
    let conn = Connection::open(db).expect("open database");
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity_check runs");
    assert_eq!(verdict, "ok", "integrity_check of {}", db.display());
}

fn fsynced_files_survive_kill_and_remount(name: &str, extra: &[&str]) {
    require_fuse();
    let dir = work_dir(name);
    let db = dir.join("db.sqlite");
    let first = pattern(300_000, 1);
    let second = pattern(70_000, 2);

    let mut mounted = Mounted::start(&db, &dir.join("mnt-1"), &dir.join("dbfs-1.log"), extra);
    write_synced(&mounted.path().join("first.bin"), &first);
    write_synced(&mounted.path().join("second.txt"), &second);
    fs::write(mounted.path().join("unsynced.txt"), b"may be lost").expect("write unsynced");
    mounted.kill_9();
    mounted.unmount();

    let remounted = Mounted::start(&db, &dir.join("mnt-2"), &dir.join("dbfs-2.log"), extra);
    assert_eq!(
        fs::read(remounted.path().join("first.bin")).expect("read first.bin"),
        first
    );
    assert_eq!(
        fs::read(remounted.path().join("second.txt")).expect("read second.txt"),
        second
    );
    drop(remounted);
    assert_integrity_ok(&db);
}

/// Writes 1 MiB blocks until `stop` is set or a write fails. After the kill every
/// write fails, which ends the loop.
fn heavy_writer(path: &Path, written: &AtomicU64, stop: &AtomicBool) {
    let Ok(mut file) = File::create(path) else {
        return;
    };
    let block = pattern(MIB, 3);
    while !stop.load(Ordering::Relaxed) {
        if file.write_all(&block).is_err() {
            return;
        }
        written.fetch_add(block.len() as u64, Ordering::Relaxed);
    }
}

fn join_within(handle: JoinHandle<()>, limit: Duration) {
    let until = Instant::now() + limit;
    while !handle.is_finished() {
        assert!(
            Instant::now() < until,
            "writer thread still blocked after dbfs was killed"
        );
        thread::sleep(Duration::from_millis(10));
    }
    handle.join().expect("writer thread");
}

/// Kills dbfs during heavy unsynced writes. With `wal_bound`, the `-wal` file must stay
/// within it while the writes run.
fn kill_during_heavy_unsynced_writes(name: &str, extra: &[&str], wal_bound: Option<u64>) {
    require_fuse();
    let dir = work_dir(name);
    let db = dir.join("db.sqlite");
    let stable = pattern(200_000, 7);

    let mut mounted = Mounted::start(&db, &dir.join("mnt-1"), &dir.join("dbfs-1.log"), extra);
    write_synced(&mounted.path().join("stable.bin"), &stable);

    let written = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let path = mounted.path().join("heavy.bin");
        let written = Arc::clone(&written);
        let stop = Arc::clone(&stop);
        thread::spawn(move || heavy_writer(&path, &written, &stop))
    };
    // Kill once 16 MiB of unsynced writes are in, or after 30 seconds.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut largest_wal = 0;
    while written.load(Ordering::Relaxed) < 16 * MIB as u64
        && Instant::now() < deadline
        && !writer.is_finished()
    {
        largest_wal = largest_wal.max(wal_len(&db));
        thread::sleep(Duration::from_millis(5));
    }
    let heavy_bytes = written.load(Ordering::Relaxed);
    assert!(
        heavy_bytes >= 4 * MIB as u64,
        "heavy phase wrote only {heavy_bytes} bytes before the kill"
    );
    if let Some(bound) = wal_bound {
        assert!(
            largest_wal <= bound,
            "the WAL reached {largest_wal} bytes, above its {bound}-byte bound"
        );
    }
    mounted.kill_9();
    mounted.unmount();
    stop.store(true, Ordering::Relaxed);
    join_within(writer, Duration::from_secs(30));
    assert_integrity_ok(&db);

    let remounted = Mounted::start(&db, &dir.join("mnt-2"), &dir.join("dbfs-2.log"), extra);
    assert_eq!(
        fs::read(remounted.path().join("stable.bin")).expect("read stable.bin"),
        stable,
        "a file fsynced before the heavy phase must survive the kill"
    );
    // The filesystem still works after the crash.
    let after = pattern(10_000, 8);
    write_synced(&remounted.path().join("after.bin"), &after);
    assert_eq!(
        fs::read(remounted.path().join("after.bin")).expect("read after.bin"),
        after
    );
    drop(remounted);
    assert_integrity_ok(&db);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn kill_after_fsync_keeps_synced_files_default_mount() {
    fsynced_files_survive_kill_and_remount("fsync-default", &[]);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn kill_after_fsync_keeps_synced_files_with_small_write_buffer() {
    fsynced_files_survive_kill_and_remount("fsync-small-buffer", &["--write-buffer", "256K"]);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn kill_after_fsync_keeps_synced_files_with_kernel_cache() {
    fsynced_files_survive_kill_and_remount("fsync-kernel-cache", &["--kernel-cache"]);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn kill_during_heavy_unsynced_writes_default_mount() {
    kill_during_heavy_unsynced_writes("heavy-default", &[], None);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn kill_during_heavy_unsynced_writes_with_1m_write_buffer() {
    kill_during_heavy_unsynced_writes("heavy-1m-buffer", &["--write-buffer", "1M"], None);
}

/// A 1 MiB WAL cap with a 1 MiB write buffer: every few MiB of writes commit and
/// checkpoint, by the writers or by the checkpoint thread. The kill can land in either.
#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn kill_during_checkpointing_keeps_synced_data_and_integrity() {
    kill_during_heavy_unsynced_writes(
        "checkpoint-small-cap",
        &["--write-buffer", "1M", "--wal-cap", "1M"],
        Some(3 * MIB as u64),
    );
}
