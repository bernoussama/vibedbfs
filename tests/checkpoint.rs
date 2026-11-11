//! WAL checkpoints off the write path. A sustained write keeps the `-wal` file
//! within `--wal-cap`, an idle database resets its WAL, closing empties the WAL,
//! and the data survives reopen. The mounted test checks that unmount does too.
//!
//! Most tests drive `Db` directly. The mounted test needs `DBFS_RUN_FUSE_TESTS=1`
//! and `fusermount3`; run it with `-- --include-ignored`.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dbfs::{Db, DbError, OpenOptions};
use rusqlite::Connection;

mod common;

const MIB: u64 = 1 << 20;
const CHUNK: usize = 128 * 1024;
const ROOT: u64 = 1;
/// How far a WAL may run past its cap: one write's frames, plus margin.
const SLACK: u64 = MIB;
/// Longer than the checkpoint thread's 1 s idle wait, so an idle WAL gets reset.
const IDLE_WAIT: Duration = Duration::from_millis(2500);

/// A fresh directory under the target dir (disk-backed, not tmpfs).
fn work_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("checkpoint-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create work dir");
    dir
}

fn wal_of(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

/// Size of the `-wal` file, or 0 when there is none.
fn wal_len(db: &Path) -> u64 {
    fs::metadata(wal_of(db)).map_or(0, |metadata| metadata.len())
}

fn open(db: &Path, wal_cap: u64, background_checkpoints: bool) -> Db {
    Db::open_with(
        db,
        OpenOptions {
            wal_cap,
            background_checkpoints,
            sync_metadata: false,
        },
    )
    .expect("open database")
}

/// The bytes of chunk `index`. Each chunk differs, so a misplaced chunk is caught.
fn chunk_bytes(index: u64) -> Vec<u8> {
    (0..CHUNK)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(index as u8))
        .collect()
}

/// Writes `chunks` chunks into a new file, one `CHUNK`-sized write each. Returns the
/// largest `-wal` size seen after a write.
fn write_stream(db: &Db, db_path: &Path, name: &[u8], chunks: u64) -> u64 {
    let ino = db
        .create_file(ROOT, name, 0o644, 0, 0)
        .expect("create file")
        .ino;
    let mut largest = 0;
    for index in 0..chunks {
        let written = db
            .write_file(ino, index * CHUNK as u64, &chunk_bytes(index))
            .expect("write chunk");
        assert_eq!(written as usize, CHUNK);
        largest = largest.max(wal_len(db_path));
    }
    largest
}

/// Checks that `name` holds the chunks `write_stream` wrote, read back in full.
fn assert_stream(db: &Db, name: &[u8], chunks: u64) {
    let inode = db.lookup(ROOT, name).expect("lookup stream");
    assert_eq!(inode.size, chunks * CHUNK as u64, "file size");
    for index in 0..chunks {
        let data = db
            .read_file(inode.ino, index * CHUNK as u64, CHUNK as u32)
            .expect("read chunk");
        assert!(data == chunk_bytes(index), "chunk {index} differs");
    }
}

/// A second connection that stays open, so SQLite does not checkpoint and delete the
/// WAL when the first connection closes. A query attaches it to the WAL: a connection
/// that has only opened the file does not count.
fn keep_wal_open(db: &Path) -> Connection {
    let conn = Connection::open(db).expect("open second connection");
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
        .expect("attach second connection to the WAL");
    conn
}

fn assert_integrity_ok(db: &Path) {
    let conn = Connection::open(db).expect("open database");
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity_check runs");
    assert_eq!(verdict, "ok", "integrity_check of {}", db.display());
}

#[test]
fn sustained_writes_keep_the_wal_under_the_cap() {
    let dir = work_dir("bound");
    let db_path = dir.join("db.sqlite");
    let cap = 8 * MIB;
    let chunks = 3 * cap / CHUNK as u64;

    let largest = {
        let db = open(&db_path, cap, true);
        write_stream(&db, &db_path, b"stream.bin", chunks)
    };
    assert!(
        largest <= cap + SLACK,
        "the WAL reached {largest} bytes with a {cap}-byte cap"
    );

    let db = open(&db_path, cap, true);
    assert_stream(&db, b"stream.bin", chunks);
    drop(db);
    assert_integrity_ok(&db_path);
}

/// With a 128 MiB cap the thread checkpoints at 64 MiB. While writes keep coming, the
/// WAL stays well below the cap, so no writer has to restart it at the cap itself.
#[test]
fn background_checkpoints_keep_the_wal_below_the_cap() {
    let dir = work_dir("background");
    let db_path = dir.join("db.sqlite");
    let cap = 128 * MIB;
    let chunks = 160 * MIB / CHUNK as u64;

    let largest = {
        let db = open(&db_path, cap, true);
        write_stream(&db, &db_path, b"stream.bin", chunks)
    };
    assert!(
        largest < cap,
        "the WAL reached {largest} bytes, the {cap}-byte cap: the writers had to restart it"
    );

    let db = open(&db_path, cap, true);
    assert_stream(&db, b"stream.bin", chunks);
    drop(db);
    assert_integrity_ok(&db_path);
}

/// Under sustained writes the reset is done by the handshake: the thread resets the
/// WAL, no writer restarts it at the cap, and the WAL stays near the forced threshold.
#[test]
fn the_handshake_resets_the_wal_under_sustained_writes() {
    let dir = work_dir("handshake");
    let db_path = dir.join("db.sqlite");
    // Passive at 16 MiB. The handshake is forced at three quarters of the cap, 24 MiB.
    let cap = 32 * MIB;
    let forced = 24 * MIB;
    let chunks = 128 * MIB / CHUNK as u64;

    let db = open(&db_path, cap, true);
    let largest = write_stream(&db, &db_path, b"stream.bin", chunks);
    let counts = db.checkpoint_counts();
    assert!(counts.resets > 0, "the handshake never reset the WAL");
    assert_eq!(
        counts.writer_restarts, 0,
        "a writer restarted the WAL at the cap"
    );
    // Writers keep appending while a pass runs, so the WAL can pass the forced
    // threshold by what one pass lets in.
    assert!(
        largest <= forced + 8 * MIB,
        "the WAL reached {largest} bytes, past the {forced}-byte forced threshold"
    );
    drop(db);
    assert_integrity_ok(&db_path);

    let db = open(&db_path, cap, true);
    assert_stream(&db, b"stream.bin", chunks);
    drop(db);
    assert_integrity_ok(&db_path);
}

/// After an idle period the WAL is reset, so a write does not grow the `-wal` file.
#[test]
fn an_idle_database_resets_its_wal() {
    let dir = work_dir("reset");
    let db_path = dir.join("db.sqlite");
    let cap = 8 * MIB;
    let db = open(&db_path, cap, true);

    // Past the 4 MiB passive threshold that a cap of 8 MiB gives, so the thread
    // checkpoints while the writes are still going.
    write_stream(&db, &db_path, b"burst.bin", 6 * MIB / CHUNK as u64);
    let before = wal_len(&db_path);
    assert!(
        before > 4 * MIB,
        "the burst left only {before} bytes of WAL"
    );

    thread::sleep(IDLE_WAIT);
    // A reset makes the next write start at the top of the WAL, so the file does not
    // grow. Without one, the write would append to the WAL and the file would grow.
    let note = db
        .create_file(ROOT, b"note.txt", 0o644, 0, 0)
        .expect("create note");
    db.write_file(note.ino, 0, b"written after idle")
        .expect("write after idle");
    let after = wal_len(&db_path);
    assert!(
        after <= before,
        "a write after an idle reset grew the WAL from {before} to {after} bytes"
    );
    drop(db);
    assert_integrity_ok(&db_path);
}

#[test]
fn hard_cap_bounds_the_wal_without_background_checkpoints() {
    let dir = work_dir("hard-cap");
    let db_path = dir.join("db.sqlite");
    let cap = 4 * MIB;
    let chunks = 12 * MIB / CHUNK as u64;

    // Opened after the database is in WAL mode. Keeps SQLite from checkpointing and
    // deleting the WAL when the database closes, so the reopen below replays it.
    let db = open(&db_path, cap, false);
    let keeper = keep_wal_open(&db_path);
    let largest = write_stream(&db, &db_path, b"stream.bin", chunks);
    drop(db);
    // Without the thread, only the writers' own RESTART keeps the WAL in check. The
    // WAL must have grown past the cap for that to be tested.
    assert!(
        largest > cap,
        "the WAL never reached the cap ({largest} bytes), so the cap was not tested"
    );
    assert!(
        largest <= cap + SLACK,
        "the WAL reached {largest} bytes with a {cap}-byte cap"
    );
    assert!(
        wal_len(&db_path) > 0,
        "closing without background checkpoints must leave the WAL in place"
    );
    drop(keeper);

    let db = open(&db_path, cap, true);
    assert_stream(&db, b"stream.bin", chunks);
    drop(db);
    assert_integrity_ok(&db_path);
}

#[test]
fn closing_the_database_truncates_the_wal_to_zero() {
    let dir = work_dir("shutdown");
    let db_path = dir.join("db.sqlite");
    let chunks = 2 * MIB / CHUNK as u64;

    let db = open(&db_path, 1 << 30, true);
    write_stream(&db, &db_path, b"stream.bin", chunks);
    // Keeps SQLite from deleting the WAL file itself on close, so the empty file
    // below shows that the truncate ran.
    let keeper = keep_wal_open(&db_path);
    assert!(wal_len(&db_path) > 0, "the writes left nothing in the WAL");

    drop(db);
    assert!(
        wal_of(&db_path).exists(),
        "the WAL file was deleted, not truncated"
    );
    assert_eq!(wal_len(&db_path), 0, "closing must empty the WAL");
    drop(keeper);

    let db = open(&db_path, 1 << 30, true);
    assert_stream(&db, b"stream.bin", chunks);
    drop(db);
    assert_integrity_ok(&db_path);
}

#[test]
fn a_wal_cap_below_one_chunk_is_refused() {
    let dir = work_dir("min-cap");
    let result = Db::open_with(
        dir.join("db.sqlite"),
        OpenOptions {
            wal_cap: 1024,
            background_checkpoints: true,
            sync_metadata: false,
        },
    );
    assert!(matches!(result, Err(DbError::InvalidInput)));
}

fn require_fuse() {
    if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none() || !command_exists("fusermount3") {
        panic!("FUSE mount tests require DBFS_RUN_FUSE_TESTS=1 and fusermount3");
    }
}

fn command_exists(command: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        path.to_string_lossy()
            .split(':')
            .any(|dir| Path::new(dir).join(command).is_file())
    })
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

fn wait_for_exit(child: &mut Child, limit: Duration) -> ExitStatus {
    let until = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().expect("poll dbfs") {
            return status;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            panic!("dbfs did not exit within {limit:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// A dbfs process mounted at `mountpoint`. Dropping it unmounts lazily and kills
/// the process if it is still running.
struct Mount {
    child: Option<Child>,
    mountpoint: PathBuf,
}

impl Drop for Mount {
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

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn unmount_leaves_no_wal_behind() {
    require_fuse();
    let dir = work_dir("unmount");
    let db_path = dir.join("db.sqlite");
    let mountpoint = dir.join("mnt");
    fs::create_dir_all(&mountpoint).expect("create mountpoint");
    let log = dir.join("dbfs.log");
    let child = Command::new(env!("CARGO_BIN_EXE_dbfs"))
        .arg("mount")
        .arg(&db_path)
        .arg(&mountpoint)
        .args(common::mount_args(&[]))
        .stdout(Stdio::null())
        .stderr(Stdio::from(File::create(&log).expect("create dbfs log")))
        .spawn()
        .expect("start dbfs");
    let mut mount = Mount {
        child: Some(child),
        mountpoint: mountpoint.clone(),
    };
    assert!(wait_for_mount(&mountpoint), "dbfs did not mount");

    // Several MiB, so the WAL holds real frames when the unmount comes.
    let payload: Vec<u8> = (0..3 * MIB).map(|i| (i as u8).wrapping_mul(7)).collect();
    fs::write(mountpoint.join("big.bin"), &payload).expect("write through mount");

    let unmounted = Command::new("fusermount3")
        .arg("-u")
        .arg(&mountpoint)
        .status()
        .expect("run fusermount3");
    assert!(unmounted.success(), "fusermount3 -u failed");
    let mut child = mount.child.take().expect("dbfs is running");
    let exit = wait_for_exit(&mut child, Duration::from_secs(15));
    assert_eq!(
        exit.code(),
        Some(0),
        "dbfs exit status; log: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    assert_eq!(wal_len(&db_path), 0, "unmount must leave an empty WAL");

    let db = Db::open(&db_path).expect("reopen database");
    let inode = db.lookup(ROOT, b"big.bin").expect("lookup big.bin");
    let data = db
        .read_file(inode.ino, 0, payload.len() as u32)
        .expect("read big.bin");
    assert!(data == payload, "big.bin differs after unmount");
    drop(db);
    assert_integrity_ok(&db_path);
}
