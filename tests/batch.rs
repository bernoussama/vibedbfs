//! Group commit: metadata operations share one open SQLite transaction until a
//! commit point, instead of committing once each. These tests cover the commit
//! points, fail-stop after a failed batch, and the modes that commit every
//! operation. The design is in the module docs of `src/batch.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dbfs::{Db, Dbfs, OpenOptions};
use rusqlite::{Connection, OpenFlags};

const CHUNK: usize = 65_536;
/// A cap that lets the filesystem run again.
const NO_CAP: u64 = 1 << 31;
/// Long enough that the commit timer never fires during a test.
const HOLD_BATCH: Duration = Duration::from_secs(3600);

/// A fresh database file under the target dir (disk-backed, not tmpfs).
fn db_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("batch");
    fs::create_dir_all(&dir).expect("create work dir");
    dir.join(format!("{name}-{}-{nanos}.sqlite", std::process::id()))
}

/// Removes the database and its sidecar files.
fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm", ".lock"] {
        let _ = fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

fn pattern(len: usize, salt: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(salt))
        .collect()
}

/// A read-only connection, as a second process would use. It sees committed data only.
fn reader(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open read-only connection")
}

fn inode_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM inodes", [], |row| row.get(0))
        .expect("count inodes")
}

fn stored_size(conn: &Connection, ino: u64) -> i64 {
    conn.query_row(
        "SELECT size FROM inodes WHERE ino = ?1",
        [ino as i64],
        |row| row.get(0),
    )
    .expect("read stored size")
}

fn page_count(path: &Path) -> u64 {
    let conn = Connection::open(path).expect("open database for page count");
    let pages: i64 = conn
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .expect("page_count");
    pages as u64
}

fn assert_integrity_ok(path: &Path) {
    let conn = Connection::open(path).expect("open database");
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity_check runs");
    assert_eq!(verdict, "ok", "integrity_check of {}", path.display());
}

#[test]
fn creates_share_one_batch_and_all_survive_reopen() {
    let path = db_path("many-creates");
    {
        let db = Db::open(&path).expect("open database");
        for i in 0..1_000 {
            db.create_file(1, format!("file{i}").as_bytes(), 0o644, 0, 0)
                .expect("create file");
        }
    }
    let db = Db::open(&path).expect("reopen database");
    let created = db
        .list_dir(1)
        .expect("list root")
        .iter()
        .filter(|entry| entry.name.starts_with(b"file"))
        .count();
    assert_eq!(created, 1_000);
    drop(db);
    assert_integrity_ok(&path);
    remove_db(&path);
}

#[test]
fn fsync_and_fsyncdir_commit_the_batch_for_a_second_connection() {
    let path = db_path("fsync-commits");
    let fs = Dbfs::new(Db::open(&path).expect("open database"));
    let second = reader(&path);

    let file = fs
        .create(1, b"notes.txt", 0o644, 0, 0, 0)
        .expect("create file");
    fs.write(file.ino, 0, b"hello").expect("write file");
    fs.flush_all().expect("fsync");
    // The batch has committed by the time fsync returns.
    assert_eq!(stored_size(&second, file.ino), 5);

    let before = inode_count(&second);
    fs.mkdir(1, b"dir", 0o755, 0, 0, 0).expect("mkdir");
    fs.sync_dir().expect("fsyncdir");
    assert_eq!(inode_count(&second), before + 1);

    drop(second);
    drop(fs);
    assert_integrity_ok(&path);
    remove_db(&path);
}

/// With no commit point, an open batch commits about `COMMIT_DELAY` (5 ms) after its
/// first operation. The commit is seen from a second connection.
#[test]
fn an_idle_batch_commits_within_the_commit_delay() {
    let path = db_path("idle-timer");
    let db = Db::open(&path).expect("open database");
    let second = reader(&path);

    let before = inode_count(&second);
    db.create_file(1, b"idle.txt", 0o644, 0, 0)
        .expect("create file");
    let started = Instant::now();
    while inode_count(&second) == before {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the batch did not commit within 2 seconds"
        );
        std::thread::sleep(Duration::from_micros(200));
    }
    eprintln!(
        "idle batch visible to a second connection after {:?}",
        started.elapsed()
    );

    drop(second);
    drop(db);
    assert_integrity_ok(&path);
    remove_db(&path);
}

/// Every create is committed when it returns, so a second connection sees it at
/// once.
fn assert_every_create_commits(name: &str, options: OpenOptions) {
    let path = db_path(name);
    let db = Db::open_with(&path, options).expect("open database");
    let second = reader(&path);
    let mut expected = inode_count(&second);
    for i in 0..20 {
        db.create_file(1, format!("file{i}").as_bytes(), 0o644, 0, 0)
            .expect("create file");
        expected += 1;
        assert_eq!(
            inode_count(&second),
            expected,
            "create {i} is committed when it returns"
        );
    }
    drop(second);
    drop(db);
    assert_integrity_ok(&path);
    remove_db(&path);
}

#[test]
fn sync_metadata_commits_each_operation_before_it_returns() {
    assert_every_create_commits(
        "sync-metadata",
        OpenOptions {
            sync_metadata: true,
            ..OpenOptions::default()
        },
    );
}

/// Read-only mounts open the database without background checkpoints, and commit
/// every operation the same way.
#[test]
fn mounts_without_background_checkpoints_commit_each_operation() {
    assert_every_create_commits(
        "no-background-checkpoints",
        OpenOptions {
            background_checkpoints: false,
            ..OpenOptions::default()
        },
    );
}

/// Free space below the floor makes every operation commit by itself.
#[test]
fn low_free_space_commits_each_operation_before_it_returns() {
    let path = db_path("low-space");
    let db = Db::open(&path).expect("open database");
    // Every measurement now counts as below the floor, as on a nearly full disk.
    db.set_min_free_space_for_tests(u64::MAX);
    let second = reader(&path);
    let mut expected = inode_count(&second);
    for i in 0..10 {
        db.create_file(1, format!("file{i}").as_bytes(), 0o644, 0, 0)
            .expect("create file");
        expected += 1;
        assert_eq!(inode_count(&second), expected, "create {i} is committed");
    }
    drop(second);
    drop(db);
    assert_integrity_ok(&path);
    remove_db(&path);
}

/// Disk full while a batch holds acknowledged operations. The write that hits the
/// cap reports ENOSPC. SQLite then rolls back the whole batch, so dbfs fails stop:
/// changes and fsync return EIO from then on, even after the cap is lifted. Reads
/// still work, and data fsynced before the batch survives a reopen.
#[test]
fn disk_full_inside_a_batch_fails_stop_and_keeps_fsynced_data() {
    let path = db_path("disk-full-batch");
    let db = Db::open(&path).expect("open database");
    // This test needs the batch to stay open, so neither the timer nor the
    // free-space guard may commit it.
    db.set_min_free_space_for_tests(0);
    db.set_commit_delay_for_tests(HOLD_BATCH);
    let fs = Dbfs::new(db);

    let keep_data = pattern(3 * CHUNK + 1000, 3);
    let keep = fs
        .create(1, b"keep", 0o644, 0, 0, 0)
        .expect("create keep")
        .ino;
    fs.write(keep, 0, &keep_data).expect("write keep");
    fs.flush_all().expect("fsync keep");

    // Acknowledged, uncommitted creates share the batch with the failing write.
    for i in 0..10 {
        fs.create(1, format!("pending{i}").as_bytes(), 0o644, 0, 0, 0)
            .expect("create pending file");
    }
    let big = fs
        .create(1, b"big", 0o644, 0, 0, 0)
        .expect("create big")
        .ino;
    fs.write(big, 0, &pattern(2 * CHUNK, 9))
        .expect("buffered write");
    fs.set_max_page_count(page_count(&path) + 2)
        .expect("cap the database");
    assert_eq!(
        fs.flush_all(),
        Err(libc::ENOSPC),
        "the write that hits the cap reports ENOSPC"
    );

    assert_eq!(
        fs.create(1, b"after", 0o644, 0, 0, 0).err(),
        Some(libc::EIO)
    );
    assert_eq!(fs.flush_all(), Err(libc::EIO), "fsync is refused");
    assert_eq!(fs.sync_dir(), Err(libc::EIO), "fsyncdir is refused");
    fs.set_max_page_count(NO_CAP).expect("lift the cap");
    assert_eq!(
        fs.create(1, b"after", 0o644, 0, 0, 0).err(),
        Some(libc::EIO),
        "still refused after the cap is lifted"
    );
    assert_eq!(
        fs.read(keep, 0, keep_data.len() as u32).expect("read keep"),
        keep_data
    );

    drop(fs);
    assert_integrity_ok(&path);
    let fs = Dbfs::new(Db::open(&path).expect("reopen database"));
    assert_eq!(
        fs.read(keep, 0, keep_data.len() as u32)
            .expect("read keep after reopen"),
        keep_data
    );
    assert_eq!(fs.lookup(1, b"after").err(), Some(libc::ENOENT));
    drop(fs);
    assert_integrity_ok(&path);
    remove_db(&path);
}

/// A create that finds its name taken, and an rmdir of a directory with entries,
/// both fail before any write. Between them, the batch keeps working.
#[test]
fn eexist_and_enotempty_mid_batch_leave_the_batch_alone() {
    let path = db_path("validation-mid-batch");
    let db = Db::open(&path).expect("open database");
    db.set_commit_delay_for_tests(HOLD_BATCH);
    let fs = Dbfs::new(db);
    let dir = fs.mkdir(1, b"dir", 0o755, 0, 0, 0).expect("mkdir").ino;
    for i in 0..50 {
        let name = format!("kept{i}");
        fs.create(1, name.as_bytes(), 0o644, 0, 0, 0)
            .expect("create kept file");
        fs.create(dir, format!("child{i}").as_bytes(), 0o644, 0, 0, 0)
            .expect("create child");
        assert_eq!(
            fs.create(1, name.as_bytes(), 0o644, 0, 0, 0).err(),
            Some(libc::EEXIST)
        );
        assert_eq!(fs.rmdir(1, b"dir"), Err(libc::ENOTEMPTY));
    }
    fs.create(1, b"later", 0o644, 0, 0, 0)
        .expect("create after the failures");
    fs.flush_all().expect("fsync");
    drop(fs);

    assert_integrity_ok(&path);
    let db = Db::open(&path).expect("reopen database");
    let root = db.list_dir(1).expect("list root");
    let kept = root
        .iter()
        .filter(|entry| entry.name.starts_with(b"kept"))
        .count();
    assert_eq!(kept, 50, "every kept file survives");
    assert!(root.iter().any(|entry| entry.name == b"later"));
    let children = db.list_dir(dir).expect("list dir").len();
    assert_eq!(children, 50, "every child survives");
    drop(db);
    remove_db(&path);
}
