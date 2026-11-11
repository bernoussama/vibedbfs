//! `dbfs migrate` through the public API. The copy keeps the tree, the contents, the
//! links, the timestamps and `user_version`, and a database that is open is refused.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dbfs::{Db, DbError, FileKind, MetadataUpdate, MigrateError, Timestamp, migrate_page_size};
use rusqlite::Connection;

const DATA_LEN: usize = 200_000;
const MTIME: Timestamp = Timestamp {
    secs: 1_700_000_000,
    nsec: 123_456_789,
};

fn temp_db_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    path.push(format!(
        "dbfs-migrate-{name}-{}-{nanos}.sqlite",
        std::process::id()
    ));
    path
}

/// The database file with `suffix` added, such as `-wal` or `.lock`.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", path.display()))
}

fn remove_files(path: &Path) {
    for suffix in ["", "-wal", "-shm", ".lock", ".migrate"] {
        let _ = std::fs::remove_file(sidecar(path, suffix));
    }
}

fn raw_i64(path: &Path, pragma: &str) -> i64 {
    let conn = Connection::open(path).expect("open raw sqlite connection");
    conn.query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))
        .expect("read pragma")
}

fn raw_string(path: &Path, pragma: &str) -> String {
    let conn = Connection::open(path).expect("open raw sqlite connection");
    conn.query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))
        .expect("read pragma")
}

fn data() -> Vec<u8> {
    (0..DATA_LEN).map(|i| (i % 251) as u8).collect()
}

fn build_tree(db: &Db) {
    let docs = db
        .create_dir(1, b"docs", 0o755, 1000, 1000)
        .expect("create docs");
    let big = db
        .create_file(docs.ino, b"big.bin", 0o644, 1000, 1000)
        .expect("create big.bin");
    db.write_file(big.ino, 0, &data()).expect("write big.bin");
    db.update_metadata(
        big.ino,
        MetadataUpdate {
            mtime: Some(MTIME),
            ..Default::default()
        },
    )
    .expect("set mtime");
    db.link(big.ino, docs.ino, b"alias.bin")
        .expect("hard link big.bin");
    db.create_symlink(1, b"latest", b"docs/big.bin", 1000, 1000)
        .expect("create symlink");
    db.create_file(1, b"empty", 0o600, 1000, 1000)
        .expect("create empty file");
}

fn assert_tree(db: &Db) {
    let docs = db.lookup(1, b"docs").expect("lookup docs");
    assert_eq!(docs.kind, FileKind::Directory);
    let big = db.lookup(docs.ino, b"big.bin").expect("lookup big.bin");
    let alias = db.lookup(docs.ino, b"alias.bin").expect("lookup alias.bin");
    assert_eq!(alias.ino, big.ino, "the hard link shares the inode");
    assert_eq!(big.nlink, 2);
    assert_eq!(big.size, DATA_LEN as u64);
    assert_eq!(big.mtime, MTIME);
    assert_eq!(
        db.read_file(big.ino, 0, DATA_LEN as u32)
            .expect("read big.bin"),
        data()
    );
    let latest = db.lookup(1, b"latest").expect("lookup symlink");
    assert_eq!(latest.kind, FileKind::Symlink);
    assert_eq!(
        db.read_symlink(latest.ino).expect("read symlink"),
        b"docs/big.bin"
    );
    let empty = db.lookup(1, b"empty").expect("lookup empty file");
    assert_eq!(empty.size, 0);
}

#[test]
fn round_trip_keeps_the_tree_contents_links_and_timestamps() {
    let path = temp_db_path("round-trip");
    {
        let db = Db::open(&path).expect("open new database");
        build_tree(&db);
    }
    let user_version = raw_i64(&path, "user_version");
    assert_eq!(raw_i64(&path, "page_size"), 16384);

    assert_eq!(
        migrate_page_size(&path, 4096).expect("migrate to 4 KiB pages"),
        16384
    );
    assert_eq!(raw_i64(&path, "page_size"), 4096);
    assert_eq!(raw_i64(&path, "user_version"), user_version);
    {
        let db = Db::open(&path).expect("open migrated database");
        assert_tree(&db);
    }
    assert_eq!(
        raw_i64(&path, "page_size"),
        4096,
        "opening a database keeps its page size"
    );
    assert_eq!(raw_string(&path, "journal_mode"), "wal");
    assert_eq!(raw_string(&path, "integrity_check"), "ok");

    assert_eq!(
        migrate_page_size(&path, 16384).expect("migrate back to 16 KiB pages"),
        4096
    );
    assert_eq!(raw_i64(&path, "page_size"), 16384);
    assert_eq!(raw_i64(&path, "user_version"), user_version);
    {
        let db = Db::open(&path).expect("open migrated database");
        assert_tree(&db);
    }
    assert_eq!(raw_string(&path, "journal_mode"), "wal");
    assert_eq!(raw_string(&path, "integrity_check"), "ok");
    assert!(!sidecar(&path, ".migrate").exists(), "no copy is left");

    remove_files(&path);
}

#[test]
fn a_database_that_is_open_is_refused_and_keeps_its_page_size() {
    let path = temp_db_path("locked");
    let db = Db::open(&path).expect("open database");
    build_tree(&db);

    let error = migrate_page_size(&path, 4096).expect_err("an open database is refused");
    assert!(
        matches!(error, MigrateError::Db(DbError::DatabaseInUse)),
        "{error:?}"
    );
    assert_eq!(raw_i64(&path, "page_size"), 16384);
    assert!(!sidecar(&path, ".migrate").exists());
    assert_tree(&db);

    drop(db);
    assert_eq!(
        migrate_page_size(&path, 4096).expect("migrate after the database closes"),
        16384
    );
    assert_eq!(raw_i64(&path, "page_size"), 4096);

    remove_files(&path);
}

#[test]
fn a_missing_database_is_refused_without_creating_files() {
    let path = temp_db_path("missing");

    assert!(matches!(
        migrate_page_size(&path, 4096),
        Err(MigrateError::NoDatabase)
    ));
    assert!(!path.exists());
    assert!(!sidecar(&path, ".lock").exists());
}

#[test]
fn migrating_to_the_current_page_size_changes_nothing() {
    let path = temp_db_path("same-size");
    {
        let db = Db::open(&path).expect("open new database");
        build_tree(&db);
    }
    let before = std::fs::read(&path).expect("read database");

    assert_eq!(
        migrate_page_size(&path, 16384).expect("migrate to the current size"),
        16384
    );
    assert_eq!(std::fs::read(&path).expect("read database"), before);
    assert!(!sidecar(&path, ".migrate").exists());

    remove_files(&path);
}
