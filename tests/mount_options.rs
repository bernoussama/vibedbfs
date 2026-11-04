//! Mount-level tests for the opt-in mount options (`--allow-other`,
//! `--allow-root`, `--auto-unmount`, `-o`). They need FUSE, so they are ignored
//! by default. Run them with:
//!
//! ```text
//! DBFS_RUN_FUSE_TESTS=1 cargo test --test mount_options -- --ignored
//! ```
//!
//! Work directories live under the repository's `.bench/` directory. Never use
//! `/tmp`, which is tmpfs on this machine.
//!
//! Tests that need `allow_other` as a non-root user are skipped (and say so on
//! stderr) unless `/etc/fuse.conf` contains `user_allow_other`, or the test runs
//! as root.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const POLL: Duration = Duration::from_millis(50);
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn noatime_option_is_applied_to_the_mount() {
    require_fuse_gate();
    let mounted = Mounted::start(bench_dir("mount-opt-noatime"), &["-o", "noatime"]);

    let line = mounted.mountinfo_line().expect("mount listed in mountinfo");
    let mount_options = line
        .split_whitespace()
        .nth(5)
        .expect("per-mount options field");
    assert!(
        mount_options.split(',').any(|option| option == "noatime"),
        "expected noatime in mountinfo, got: {line}"
    );
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn allow_other_refusal_reports_the_fuse_conf_hint() {
    require_fuse_gate();
    if euid() == 0 {
        skip("allow_other refusal needs a non-root user; running as root");
        return;
    }
    if fuse_conf_allows_user_allow_other() {
        skip("/etc/fuse.conf has user_allow_other, so allow_other is accepted");
        return;
    }

    let dir = bench_dir("mount-opt-refused");
    let db_path = dir.join("fs.sqlite");
    let mountpoint = dir.join("mnt");
    fs::create_dir(&mountpoint).expect("create mountpoint");

    let (success, stderr) = run_mount_to_exit(&db_path, &mountpoint, &["--allow-other"]);

    assert!(!success, "mount should be refused: {stderr}");
    assert!(
        stderr.contains("user_allow_other"),
        "fusermount text changed: {stderr}"
    );
    assert!(
        stderr.contains("hint: allow_other requires 'user_allow_other' in /etc/fuse.conf"),
        "missing hint: {stderr}"
    );
    assert!(!is_mounted(&mountpoint), "refused mount must not be listed");

    cleanup(&dir, &db_path, &mountpoint);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn unknown_option_is_passed_to_fusermount_and_fails_the_mount() {
    require_fuse_gate();
    // Unknown options are passed through to fusermount3, which rejects the ones
    // it doesn't know. The text is fusermount3 3.x output.
    let dir = bench_dir("mount-opt-unknown");
    let db_path = dir.join("fs.sqlite");
    let mountpoint = dir.join("mnt");
    fs::create_dir(&mountpoint).expect("create mountpoint");

    let (success, stderr) = run_mount_to_exit(&db_path, &mountpoint, &["-o", "bogus_option"]);

    assert!(!success, "unknown option should fail the mount");
    assert!(stderr.contains("unknown option 'bogus_option'"), "{stderr}");
    assert!(!is_mounted(&mountpoint), "failed mount must not be listed");

    cleanup(&dir, &db_path, &mountpoint);
}

/// Runs `dbfs mount` with `args` and waits for it to exit, which a refused or
/// invalid mount does. Returns whether it succeeded, and its stderr.
fn run_mount_to_exit(db_path: &Path, mountpoint: &Path, args: &[&str]) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
        .arg("mount")
        .args(args)
        .arg(db_path)
        .arg(mountpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start dbfs mount");
    let status = wait_for_exit(&mut child, READY_TIMEOUT).unwrap_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        let _ = Command::new("fusermount3")
            .arg("-u")
            .arg(mountpoint)
            .status();
        panic!("dbfs mount {args:?} did not exit within {READY_TIMEOUT:?}");
    });
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr pipe")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    (status.success(), stderr)
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn allow_other_mount_exposes_the_mount_with_default_permissions() {
    require_fuse_gate();
    if !can_allow_other() {
        skip("needs user_allow_other in /etc/fuse.conf, or root");
        return;
    }
    let mounted = Mounted::start(bench_dir("mount-opt-allow-other"), &["--allow-other"]);

    let line = mounted.mountinfo_line().expect("mount listed in mountinfo");
    let superblock_options = line
        .split(" - ")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().nth(2))
        .expect("superblock options");
    let options: Vec<&str> = superblock_options.split(',').collect();
    assert!(options.contains(&"allow_other"), "{line}");
    assert!(
        options.contains(&"default_permissions"),
        "allow_other must keep default_permissions on: {line}"
    );
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn auto_unmount_removes_the_mount_after_the_daemon_is_killed() {
    require_fuse_gate();
    if !can_allow_other() {
        skip("needs user_allow_other in /etc/fuse.conf, or root");
        return;
    }
    let mut mounted = Mounted::start(
        bench_dir("mount-opt-auto-unmount"),
        &["--allow-other", "--auto-unmount"],
    );

    mounted.kill_daemon();

    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    while mounted.mountinfo_line().is_some() {
        assert!(
            Instant::now() < deadline,
            "auto-unmount left a stale mount after SIGKILL"
        );
        sleep(POLL);
    }
}

/// A dbfs mount in a `.bench/` directory. `Drop` unmounts it and removes only
/// empty directories and the database files, so a failed unmount never causes
/// recursive deletes through a live mount.
struct Mounted {
    dir: PathBuf,
    mountpoint: PathBuf,
    db_path: PathBuf,
    child: Option<Child>,
}

impl Mounted {
    fn start(dir: PathBuf, extra_args: &[&str]) -> Self {
        let db_path = dir.join("fs.sqlite");
        let mountpoint = dir.join("mnt");
        fs::create_dir(&mountpoint).expect("create mountpoint");
        let child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
            .arg("mount")
            .args(extra_args)
            .arg(&db_path)
            .arg(&mountpoint)
            .spawn()
            .expect("start dbfs mount");
        let mut mounted = Self {
            dir,
            mountpoint,
            db_path,
            child: Some(child),
        };
        mounted.wait_until_mounted();
        mounted
    }

    fn wait_until_mounted(&mut self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        while Instant::now() < deadline {
            if self.mountinfo_line().is_some() {
                return;
            }
            let exited = self
                .child
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten());
            if let Some(status) = exited {
                panic!("dbfs exited before mounting: {status}");
            }
            sleep(POLL);
        }
        panic!("mount did not become ready");
    }

    fn mountinfo_line(&self) -> Option<String> {
        mountinfo_line(&self.mountpoint)
    }

    fn kill_daemon(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn unmount(&mut self) {
        if self.mountinfo_line().is_some() {
            let _ = Command::new("fusermount3")
                .arg("-u")
                .arg(&self.mountpoint)
                .status();
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        self.unmount();
        cleanup(&self.dir, &self.db_path, &self.mountpoint);
    }
}

fn cleanup(dir: &Path, db_path: &Path, mountpoint: &Path) {
    let _ = fs::remove_dir(mountpoint);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(db_path.with_extension("sqlite-wal"));
    let _ = fs::remove_file(db_path.with_extension("sqlite-shm"));
    let _ = fs::remove_dir(dir);
}

fn require_fuse_gate() {
    if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none() || command_path("fusermount3").is_none() {
        panic!("FUSE mount tests require DBFS_RUN_FUSE_TESTS=1 and fusermount3");
    }
}

/// Whether `allow_other` can succeed for this process: root always can, and a
/// non-root user needs `user_allow_other` in /etc/fuse.conf.
fn can_allow_other() -> bool {
    euid() == 0 || fuse_conf_allows_user_allow_other()
}

fn fuse_conf_allows_user_allow_other() -> bool {
    fs::read_to_string("/etc/fuse.conf")
        .map(|conf| conf.lines().any(|line| line.trim() == "user_allow_other"))
        .unwrap_or(false)
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Reports a skipped test on stderr. Written directly to the stderr handle so
/// the message appears in normal `cargo test` output, not only with
/// `--nocapture`.
fn skip(reason: &str) {
    let _ = writeln!(std::io::stderr(), "SKIPPED mount_options test: {reason}");
}

/// The `/proc/self/mountinfo` line for `mountpoint`, if it is mounted. The
/// caller passes an absolute path, which matches what the kernel records.
fn mountinfo_line(mountpoint: &Path) -> Option<String> {
    let target = mountpoint.to_str()?;
    fs::read_to_string("/proc/self/mountinfo")
        .ok()?
        .lines()
        .find(|line| line.split_whitespace().nth(4) == Some(target))
        .map(str::to_string)
}

fn is_mounted(mountpoint: &Path) -> bool {
    // Use the path as given. canonicalize fails on a dead FUSE mount.
    let absolute = if mountpoint.is_absolute() {
        mountpoint.to_path_buf()
    } else {
        std::env::current_dir()
            .expect("current dir")
            .join(mountpoint)
    };
    mountinfo_line(&absolute).is_some()
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll dbfs") {
            return Some(status);
        }
        sleep(POLL);
    }
    None
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

/// A fresh, disk-backed directory under `.bench/`, created with an absolute
/// path so it matches `/proc/self/mountinfo`.
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
