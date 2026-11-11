//! Durability tests that mount dbfs. Each one is `#[ignore]`d and panics unless
//! `DBFS_RUN_FUSE_TESTS=1` is set. Run them with:
//!
//! ```text
//! DBFS_RUN_FUSE_TESTS=1 cargo test --test durability -- --ignored
//! ```
//!
//! The two fsync tests also attach strace, which needs `ptrace` permission.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod common;

fn supports_fuse() -> bool {
    Path::new("/dev/fuse").exists() && command_exists("fusermount3") && command_exists("mountpoint")
}

/// Panics unless the FUSE gate is open. The tests are `#[ignore]`d, so this runs
/// only under `-- --ignored`, and a missing prerequisite fails instead of passing.
fn require_fuse_gate() {
    if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none() || !supports_fuse() {
        panic!(
            "FUSE mount tests require DBFS_RUN_FUSE_TESTS=1, /dev/fuse, fusermount3 and mountpoint"
        );
    }
}

/// `require_fuse_gate`, plus strace, which the fsync tests attach to dbfs.
fn require_fuse_trace_gate() {
    require_fuse_gate();
    if !command_exists("strace") {
        panic!("the fsync trace tests require strace");
    }
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        path.to_string_lossy().split(':').any(|dir| {
            let candidate = Path::new(dir).join(name);
            candidate.is_file()
        })
    })
}

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

struct Mounted {
    child: Option<Child>,
    mountpoint: PathBuf,
}

impl Mounted {
    fn start(db: &Path, mountpoint: &Path) -> Self {
        fs::create_dir_all(mountpoint).expect("create mountpoint");
        let mut child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
            .arg("mount")
            .args(common::mount_args(&[]))
            .arg(db)
            .arg(mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start dbfs");
        if !wait_for_mount(mountpoint) {
            let mut err = String::new();
            if let Some(stderr) = child.stderr.take() {
                let _ = BufReader::new(stderr).read_to_string(&mut err);
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!("dbfs did not mount {}: {err}", mountpoint.display());
        }
        Self {
            child: Some(child),
            mountpoint: mountpoint.to_path_buf(),
        }
    }

    fn kill_and_unmount(&mut self) {
        if let Some(mut child) = self.child.take() {
            let status = Command::new("kill")
                .args(["-9", &child.id().to_string()])
                .status()
                .expect("run kill");
            assert!(status.success(), "kill -9 of dbfs failed");
            let exited = child.wait().expect("wait for killed dbfs");
            assert!(
                exited.code().is_none(),
                "dbfs was not killed by a signal: {exited}"
            );
        }
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn trace_syncs(&self, log: &Path, body: impl FnOnce()) {
        let (attached_tx, attached_rx) = mpsc::channel();
        let pid = self
            .child
            .as_ref()
            .expect("dbfs is running")
            .id()
            .to_string();
        let mut strace = Command::new("strace")
            .arg("-f")
            .arg("-y")
            .arg("-e")
            .arg("trace=fsync,fdatasync")
            .arg("-o")
            .arg(log)
            .arg("-p")
            .arg(pid)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start strace");
        let stderr = strace.stderr.take().expect("strace stderr");
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line.contains("attached") {
                    let _ = attached_tx.send(());
                }
                line.clear();
            }
        });
        attached_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("strace attached to dbfs");
        body();
        let _ = strace.kill();
        let _ = strace.wait();
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
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

fn assert_wal_synced(log: &str, db: &Path) {
    let mut wal = db.as_os_str().to_os_string();
    wal.push("-wal");
    let wal = wal.to_string_lossy();
    let synced = log.lines().any(|line| {
        (line.contains("fdatasync(") || line.contains("fsync("))
            && line.contains(wal.as_ref())
            && line.contains("= 0")
    });
    assert!(
        synced,
        "expected a successful fsync or fdatasync of {wal}, log:\n{log}"
    );
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1, fusermount3 and strace"]
fn fsync_of_dirty_data_fdatasyncs_the_wal() {
    require_fuse_trace_gate();

    let dir = work_dir("dirty-fsync");
    let db = dir.join("db.sqlite");
    let mountpoint = dir.join("mnt");
    let mounted = Mounted::start(&db, &mountpoint);
    let file = mountpoint.join("notes.txt");
    {
        let mut initial = File::create(&file).expect("create file");
        initial
            .write_all(b"old")
            .expect("write bytes that commit before the traced fsync");
    }

    let log_path = dir.join("strace.log");
    mounted.trace_syncs(&log_path, || {
        let mut file = OpenOptions::new()
            .write(true)
            .open(&file)
            .expect("reopen file");
        file.write_all(b"durable")
            .expect("overwrite with durable bytes");
        file.sync_all().expect("fsync dirty file");
    });

    let log = fs::read_to_string(&log_path).expect("read strace log");
    assert_wal_synced(&log, &db);
    assert_eq!(fs::read(&file).expect("read file after fsync"), b"durable");
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1, fusermount3 and strace"]
fn fsync_of_a_clean_file_fdatasyncs_the_wal() {
    require_fuse_trace_gate();

    let dir = work_dir("clean-fsync");
    let db = dir.join("db.sqlite");
    let mountpoint = dir.join("mnt");
    let mounted = Mounted::start(&db, &mountpoint);
    let file = mountpoint.join("notes.txt");
    {
        let mut initial = File::create(&file).expect("create file");
        initial
            .write_all(b"kept")
            .expect("commit bytes before the traced fsync");
    }

    let log_path = dir.join("strace.log");
    mounted.trace_syncs(&log_path, || {
        let file = OpenOptions::new()
            .read(true)
            .open(&file)
            .expect("reopen clean file");
        file.sync_all().expect("fsync clean file");
    });

    let log = fs::read_to_string(&log_path).expect("read strace log");
    assert_wal_synced(&log, &db);
    assert_eq!(fs::read(&file).expect("read file after fsync"), b"kept");
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn fsynced_bytes_survive_kill_and_remount() {
    require_fuse_gate();

    let dir = work_dir("kill-remount");
    let db = dir.join("db.sqlite");
    let mountpoint = dir.join("mnt");
    let mut mounted = Mounted::start(&db, &mountpoint);
    let file = mountpoint.join("notes.txt");
    let mut handle = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&file)
        .expect("create file");
    handle.write_all(b"kept-across-kill").expect("write bytes");
    handle.sync_all().expect("fsync before kill");

    mounted.kill_and_unmount();
    drop(handle);

    let remount_point = dir.join("mnt2");
    let _remounted = Mounted::start(&db, &remount_point);
    assert_eq!(
        fs::read(remount_point.join("notes.txt")).expect("read file after remount"),
        b"kept-across-kill"
    );
}
