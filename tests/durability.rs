use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn supports_fuse_trace() -> bool {
    Path::new("/dev/fuse").exists()
        && command_exists("fusermount3")
        && command_exists("strace")
        && command_exists("mountpoint")
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
    child: Child,
    mountpoint: PathBuf,
}

impl Mounted {
    fn start(db: &Path, mountpoint: &Path) -> Self {
        fs::create_dir_all(mountpoint).expect("create mountpoint");
        let mut child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
            .arg("mount")
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
            child,
            mountpoint: mountpoint.to_path_buf(),
        }
    }

    fn trace_syncs(&self, log: &Path, body: impl FnOnce()) {
        let (attached_tx, attached_rx) = mpsc::channel();
        let mut strace = Command::new("strace")
            .arg("-f")
            .arg("-y")
            .arg("-e")
            .arg("trace=fsync,fdatasync")
            .arg("-o")
            .arg(log)
            .arg("-p")
            .arg(self.child.id().to_string())
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
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
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
fn fsync_of_dirty_data_fdatasyncs_the_wal() {
    if !supports_fuse_trace() {
        return;
    }

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
        file.write_all(b"durable").expect("overwrite with durable bytes");
        file.sync_all().expect("fsync dirty file");
    });

    let log = fs::read_to_string(&log_path).expect("read strace log");
    assert_wal_synced(&log, &db);
    assert_eq!(
        fs::read(&file).expect("read file after fsync"),
        b"durable"
    );
}
