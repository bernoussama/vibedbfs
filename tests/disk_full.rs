//! Disk-full behaviour. `Dbfs::set_max_page_count` caps the database with
//! `PRAGMA max_page_count`, so a write that needs a page past the cap fails with
//! `SQLITE_FULL`, which dbfs reports as `ENOSPC`. Nothing is mounted.

use std::fs;
use std::path::{Path, PathBuf};

use dbfs::{Db, Dbfs};
use rusqlite::Connection;

const CHUNK: usize = 65_536;
/// A cap that lets the filesystem run again.
const NO_CAP: u64 = 1 << 31;

/// A fresh database file under the target dir (disk-backed, not tmpfs).
fn db_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("disk_full");
    fs::create_dir_all(&dir).expect("create work dir");
    let path = dir.join(format!("{name}-{}.db", std::process::id()));
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", path.display()));
    }
    path
}

fn pattern(len: usize, salt: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(salt))
        .collect()
}

/// The current size of the database in pages, as a cap for `set_max_page_count`.
fn page_count(path: &Path) -> u64 {
    let conn = Connection::open(path).expect("open database for page count");
    let pages: i64 = conn
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .expect("page_count");
    pages as u64
}

fn assert_integrity_ok(path: &Path) {
    let conn = Connection::open(path).expect("open database for integrity_check");
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity_check runs");
    assert_eq!(verdict, "ok", "integrity_check after reopen");
}

/// A file with fsynced contents, written before the cap is set.
fn fsynced_file(fs: &Dbfs, name: &[u8], data: &[u8]) -> u64 {
    let ino = fs.create(1, name, 0o644, 0, 0, 0).expect("create").ino;
    assert_eq!(fs.write(ino, 0, data), Ok(data.len() as u32));
    fs.flush_all().expect("fsync before the cap");
    ino
}

#[test]
fn enospc_is_reported_by_write_flush_and_fsync_and_fsynced_data_survives() {
    let path = db_path("enospc");
    // One 64 KiB buffer: the second chunk of a write evicts the first.
    let fs = Dbfs::with_write_buffer(Db::open(&path).expect("open"), CHUNK);
    let keep_data = pattern(3 * CHUNK + 1000, 3);
    let keep = fsynced_file(&fs, b"keep", &keep_data);

    fs.set_max_page_count(page_count(&path) + 2)
        .expect("cap the database");
    let big_data = pattern(2 * CHUNK, 9);
    let big = fs.create(1, b"big", 0o644, 0, 0, 0).expect("create").ino;

    assert_eq!(fs.write(big, 0, &big_data[..CHUNK]), Ok(CHUNK as u32));
    // The eviction flush needs pages past the cap.
    assert_eq!(
        fs.write(big, CHUNK as u64, &big_data[CHUNK..]).err(),
        Some(libc::ENOSPC),
        "write must report ENOSPC"
    );
    assert_eq!(
        fs.getattr(big).expect("getattr").size,
        CHUNK as u64,
        "a failed write must not grow the file"
    );
    assert_eq!(
        fs.flush_file(big).err(),
        Some(libc::ENOSPC),
        "flush must report ENOSPC"
    );
    assert_eq!(
        fs.flush_all().err(),
        Some(libc::ENOSPC),
        "fsync must report ENOSPC"
    );

    // Failed data stays buffered and readable, and fsynced data is untouched.
    assert_eq!(
        fs.read(big, 0, CHUNK as u32).expect("read"),
        big_data[..CHUNK]
    );
    assert_eq!(
        fs.read(keep, 0, keep_data.len() as u32).expect("read"),
        keep_data
    );

    // With space back, the next fsync writes the buffered data.
    fs.set_max_page_count(NO_CAP).expect("lift the cap");
    fs.flush_all().expect("fsync once space is back");
    drop(fs);

    assert_integrity_ok(&path);
    let fs = Dbfs::new(Db::open(&path).expect("reopen"));
    let keep = fs.lookup(1, b"keep").expect("lookup keep").ino;
    let big = fs.lookup(1, b"big").expect("lookup big").ino;
    assert_eq!(
        fs.read(keep, 0, keep_data.len() as u32).expect("read"),
        keep_data
    );
    assert_eq!(
        fs.read(big, 0, 2 * CHUNK as u32).expect("read"),
        big_data[..CHUNK]
    );
}

#[test]
fn release_reports_enospc_and_keeps_the_data_for_a_later_fsync() {
    let path = db_path("release");
    let fs = Dbfs::new(Db::open(&path).expect("open"));
    let file = fs.create(1, b"file", 0o644, 0, 0, 0).expect("create").ino;
    let fh = fs.open(file).expect("open");
    let data = pattern(CHUNK + 10, 5);
    assert_eq!(fs.write_fh(fh, 0, &data), Ok(data.len() as u32));

    fs.set_max_page_count(page_count(&path) + 2)
        .expect("cap the database");
    assert_eq!(
        fs.release_file(fh).err(),
        Some(libc::ENOSPC),
        "release must report the failed flush"
    );
    assert_eq!(
        fs.read_fh(fh, 0, 1).err(),
        Some(libc::EBADF),
        "the handle is gone even though the flush failed"
    );
    assert_eq!(fs.read(file, 0, data.len() as u32).expect("read"), data);

    fs.set_max_page_count(NO_CAP).expect("lift the cap");
    fs.flush_all().expect("fsync once space is back");
    drop(fs);

    assert_integrity_ok(&path);
    let fs = Dbfs::new(Db::open(&path).expect("reopen"));
    let file = fs.lookup(1, b"file").expect("lookup").ino;
    assert_eq!(fs.read(file, 0, data.len() as u32).expect("read"), data);
}

#[test]
fn crash_after_enospc_keeps_fsynced_data_and_drops_the_failed_write() {
    let path = db_path("crash");
    let fs = Dbfs::with_write_buffer(Db::open(&path).expect("open"), CHUNK);
    let keep_data = pattern(200_000, 11);
    fsynced_file(&fs, b"keep", &keep_data);

    fs.set_max_page_count(page_count(&path) + 2)
        .expect("cap the database");
    let big = fs.create(1, b"big", 0o644, 0, 0, 0).expect("create").ino;
    assert_eq!(fs.write(big, 0, &pattern(CHUNK, 4)), Ok(CHUNK as u32));
    assert_eq!(
        fs.write(big, CHUNK as u64, &pattern(CHUNK, 4)).err(),
        Some(libc::ENOSPC)
    );
    // Simulate a crash: drop the filesystem without flushing its buffers.
    drop(fs);

    assert_integrity_ok(&path);
    let fs = Dbfs::new(Db::open(&path).expect("reopen"));
    let keep = fs.lookup(1, b"keep").expect("lookup keep").ino;
    assert_eq!(
        fs.read(keep, 0, keep_data.len() as u32).expect("read"),
        keep_data
    );
    let big = fs.lookup(1, b"big").expect("lookup big").ino;
    assert_eq!(
        fs.getattr(big).expect("getattr").size,
        0,
        "no part of the failed write may reach the database"
    );
}
