use std::fs::{self, FileTimes};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[test]
fn mounted_filesystem_supports_basic_file_lifecycle_when_enabled() {
    if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none() || command_path("fusermount3").is_none() {
        return;
    }

    let base = temp_path("mount-integration");
    let db_path = base.with_extension("sqlite");
    let mountpoint = base.with_extension("mnt");
    fs::create_dir(&mountpoint).expect("create mountpoint");

    let mut child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
        .arg("mount")
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
fn utimens_round_trips_nanoseconds_when_enabled() {
    let Some(mounted) = Mounted::start("utimens-nsec") else {
        return;
    };

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
    std::env::temp_dir().join(format!("dbfs-{name}-{}-{nanos}", std::process::id()))
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

fn unmount(mountpoint: &Path, child: &mut Child) {
    let _ = Command::new("fusermount3")
        .arg("-u")
        .arg(mountpoint)
        .status();
    let _ = child.wait();
}

struct Mounted {
    mountpoint: PathBuf,
    db_path: PathBuf,
    child: Child,
}

impl Mounted {
    fn start(name: &str) -> Option<Self> {
        if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none()
            || command_path("fusermount3").is_none()
        {
            return None;
        }

        let base = temp_path(name);
        let db_path = base.with_extension("sqlite");
        let mountpoint = base.with_extension("mnt");
        fs::create_dir(&mountpoint).expect("create mountpoint");
        let child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
            .arg("mount")
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
        Some(mounted)
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
