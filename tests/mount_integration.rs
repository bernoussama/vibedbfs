use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn mounted_filesystem_supports_basic_file_lifecycle_when_enabled() {
    if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none() || command_path("fusermount3").is_none() {
        panic!("FUSE mount tests require DBFS_RUN_FUSE_TESTS=1 and fusermount3");
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
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn mounted_sparse_file_past_4gib_reads_marker_at_that_offset() {
    if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none() || command_path("fusermount3").is_none() {
        panic!("FUSE mount tests require DBFS_RUN_FUSE_TESTS=1 and fusermount3");
    }

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
