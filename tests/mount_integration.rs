use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dbfs::Db;

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

#[test]
fn large_write_keeps_daemon_memory_bounded_when_enabled() {
    if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none() || command_path("fusermount3").is_none() {
        return;
    }

    const MIB: u64 = 1 << 20;
    const WRITTEN_MIB: u64 = 512;

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
        rss < WRITTEN_MIB * MIB / 4,
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
