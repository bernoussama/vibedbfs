use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dbfs::Db;

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
fn close_after_many_small_random_writes_does_not_stall_other_requests_when_enabled() {
    if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none() || command_path("fusermount3").is_none() {
        return;
    }

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

struct Mount {
    db_path: PathBuf,
    mountpoint: PathBuf,
    child: Child,
}

impl Mount {
    fn start(db_path: PathBuf) -> Self {
        let mountpoint = db_path.with_extension("mnt");
        fs::create_dir(&mountpoint).expect("create mountpoint");
        let child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
            .arg("mount")
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

fn unmount(mountpoint: &Path, child: &mut Child) {
    let _ = Command::new("fusermount3")
        .arg("-u")
        .arg(mountpoint)
        .status();
    let _ = child.wait();
}
