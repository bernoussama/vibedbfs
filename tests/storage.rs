use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbfs::{
    Db, DbError, FileKind, MetadataUpdate, SCHEMA_VERSION, Timestamp, file_attr_from_inode,
};
use rusqlite::{Connection, params};

fn temp_db_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    path.push(format!("dbfs-{name}-{}-{nanos}.sqlite", std::process::id()));
    path
}

#[test]
fn initializes_root_directory_inode() {
    let db = Db::open_in_memory().expect("open in-memory database");

    let root = db.get_inode(1).expect("load root inode");

    assert_eq!(root.ino, 1);
    assert_eq!(root.kind, FileKind::Directory);
    assert_eq!(root.size, 0);
    assert_eq!(root.nlink, 2);
}

#[test]
fn file_backed_database_persists_entries_after_reopen() {
    let path = temp_db_path("persistence");
    {
        let db = Db::open(&path).expect("open file-backed database");
        let docs = db
            .create_dir(1, b"docs", 0o755, 1000, 1000)
            .expect("create directory");
        db.create_file(docs.ino, b"readme.txt", 0o644, 1000, 1000)
            .expect("create file");
    }

    let db = Db::open(&path).expect("reopen file-backed database");
    let docs = db.lookup(1, b"docs").expect("lookup persisted directory");
    assert_eq!(
        db.lookup(docs.ino, b"readme.txt")
            .expect("lookup persisted file")
            .kind,
        FileKind::RegularFile
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

#[test]
fn file_backed_database_enables_wal_and_foreign_keys() {
    let path = temp_db_path("pragmas");
    let db = Db::open(&path).expect("open file-backed database");

    assert_eq!(
        db.pragma_string("journal_mode").expect("journal mode"),
        "wal"
    );
    assert_eq!(db.pragma_i64("foreign_keys").expect("foreign keys"), 1);
    assert_eq!(db.pragma_i64("busy_timeout").expect("busy timeout"), 5000);

    drop(db);
    let conn = Connection::open(&path).expect("open raw sqlite connection");
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read journal mode");
    assert_eq!(journal_mode, "wal");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

#[test]
fn opening_legacy_database_migrates_root_owner_to_mount_user() {
    let path = temp_db_path("legacy-root-owner");
    {
        let conn = Connection::open(&path).expect("open raw sqlite connection");
        conn.execute_batch(
            "
            CREATE TABLE inodes (
              ino INTEGER PRIMARY KEY,
              kind INTEGER NOT NULL,
              mode INTEGER NOT NULL,
              uid INTEGER NOT NULL,
              gid INTEGER NOT NULL,
              size INTEGER NOT NULL,
              atime INTEGER NOT NULL,
              mtime INTEGER NOT NULL,
              ctime INTEGER NOT NULL,
              nlink INTEGER NOT NULL
            );
            INSERT INTO inodes
              (ino, kind, mode, uid, gid, size, atime, mtime, ctime, nlink)
              VALUES (1, 2, 493, 0, 0, 0, 1, 1, 1, 1);
            ",
        )
        .expect("create legacy root inode");
    }

    let db = Db::open(&path).expect("open legacy database");
    let root = db.get_inode(1).expect("load root inode");

    assert_eq!(root.uid, unsafe { libc::getuid() });
    assert_eq!(root.gid, unsafe { libc::getgid() });

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

#[test]
fn legacy_second_timestamps_read_back_with_zero_nsec() {
    let path = temp_db_path("legacy-seconds");
    {
        let conn = Connection::open(&path).expect("open raw sqlite connection");
        conn.execute_batch(
            "
            CREATE TABLE inodes (
              ino INTEGER PRIMARY KEY,
              kind INTEGER NOT NULL,
              mode INTEGER NOT NULL,
              uid INTEGER NOT NULL,
              gid INTEGER NOT NULL,
              size INTEGER NOT NULL,
              atime INTEGER NOT NULL,
              mtime INTEGER NOT NULL,
              ctime INTEGER NOT NULL,
              nlink INTEGER NOT NULL
            );
            INSERT INTO inodes
              (ino, kind, mode, uid, gid, size, atime, mtime, ctime, nlink)
              VALUES (1, 2, 493, 1, 1, 0, 10, 20, 30, 1);
            INSERT INTO inodes
              (ino, kind, mode, uid, gid, size, atime, mtime, ctime, nlink)
              VALUES (2, 1, 420, 1, 1, 4, 111, 222, 333, 1);
            ",
        )
        .expect("create legacy second-only inodes");
    }

    let db = Db::open(&path).expect("open legacy database");
    let attr = file_attr_from_inode(&db.get_inode(2).expect("load legacy file"));

    assert_eq!(attr.atime, UNIX_EPOCH + Duration::new(111, 0));
    assert_eq!(attr.mtime, UNIX_EPOCH + Duration::new(222, 0));
    assert_eq!(attr.ctime, UNIX_EPOCH + Duration::new(333, 0));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

#[test]
fn legacy_directory_nlink_counts_subdirectories() {
    let path = temp_db_path("legacy-nlink");
    {
        let conn = Connection::open(&path).expect("open raw sqlite connection");
        conn.execute_batch(
            "
            CREATE TABLE inodes (
              ino INTEGER PRIMARY KEY,
              kind INTEGER NOT NULL,
              mode INTEGER NOT NULL,
              uid INTEGER NOT NULL,
              gid INTEGER NOT NULL,
              size INTEGER NOT NULL,
              atime INTEGER NOT NULL,
              mtime INTEGER NOT NULL,
              ctime INTEGER NOT NULL,
              nlink INTEGER NOT NULL
            );
            CREATE TABLE dirents (
              parent_ino INTEGER NOT NULL,
              name BLOB NOT NULL,
              child_ino INTEGER NOT NULL,
              PRIMARY KEY (parent_ino, name)
            );
            INSERT INTO inodes
              (ino, kind, mode, uid, gid, size, atime, mtime, ctime, nlink)
              VALUES (1, 2, 493, 1, 1, 0, 1, 1, 1, 1);
            INSERT INTO inodes
              (ino, kind, mode, uid, gid, size, atime, mtime, ctime, nlink)
              VALUES (2, 2, 493, 1, 1, 0, 1, 1, 1, 1);
            INSERT INTO dirents (parent_ino, name, child_ino) VALUES (1, 'docs', 2);
            ",
        )
        .expect("create legacy directories");
    }

    let db = Db::open(&path).expect("open legacy database");
    assert_eq!(db.get_inode(1).expect("root").nlink, 3);
    assert_eq!(db.get_inode(2).expect("docs").nlink, 2);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

#[test]
fn creates_file_and_directory_entries_under_parent() {
    let db = Db::open_in_memory().expect("open in-memory database");

    let docs = db
        .create_dir(1, b"docs", 0o755, 1000, 1000)
        .expect("create directory");
    let readme = db
        .create_file(docs.ino, b"readme.txt", 0o644, 1000, 1000)
        .expect("create file");

    assert_eq!(db.lookup(1, b"docs").expect("lookup docs").ino, docs.ino);
    assert_eq!(
        db.lookup(docs.ino, b"readme.txt")
            .expect("lookup readme")
            .ino,
        readme.ino
    );

    let entries = db.list_dir(docs.ino).expect("list docs");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, b"readme.txt");
    assert_eq!(entries[0].ino, readme.ino);
    assert_eq!(entries[0].kind, FileKind::RegularFile);
}

#[test]
fn rejects_duplicate_names_in_same_directory() {
    let db = Db::open_in_memory().expect("open in-memory database");

    db.create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create first file");

    let duplicate = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect_err("duplicate name should fail");

    assert_eq!(duplicate, DbError::AlreadyExists);
}

#[test]
fn allows_same_name_in_different_directories() {
    let db = Db::open_in_memory().expect("open in-memory database");

    let left = db
        .create_dir(1, b"left", 0o755, 1000, 1000)
        .expect("create left directory");
    let right = db
        .create_dir(1, b"right", 0o755, 1000, 1000)
        .expect("create right directory");

    let left_file = db
        .create_file(left.ino, b"same.txt", 0o644, 1000, 1000)
        .expect("create left file");
    let right_file = db
        .create_file(right.ino, b"same.txt", 0o644, 1000, 1000)
        .expect("create right file");

    assert_ne!(left_file.ino, right_file.ino);
    assert_eq!(
        db.lookup(left.ino, b"same.txt")
            .expect("lookup left file")
            .ino,
        left_file.ino
    );
    assert_eq!(
        db.lookup(right.ino, b"same.txt")
            .expect("lookup right file")
            .ino,
        right_file.ino
    );
}

#[test]
fn writes_and_reads_file_contents() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");

    let written = db
        .write_file(file.ino, 0, b"hello database filesystem")
        .expect("write file");

    assert_eq!(written, 25);
    assert_eq!(
        db.read_file(file.ino, 0, 25).expect("read file"),
        b"hello database filesystem"
    );
    assert_eq!(db.get_inode(file.ino).expect("load inode").size, 25);
}

#[test]
fn batches_multiple_file_writes_in_one_call() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");

    let written = db
        .write_file_batch(file.ino, &[(0, b"hello".to_vec()), (6, b"dbfs".to_vec())])
        .expect("write batch");

    assert_eq!(written, 9);
    assert_eq!(
        db.read_file(file.ino, 0, 10).expect("read file"),
        b"hello\0dbfs"
    );
    assert_eq!(db.get_inode(file.ino).expect("load inode").size, 10);
}

#[test]
fn batched_overlapping_writes_preserve_write_order() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"overlap.txt", 0o644, 1000, 1000)
        .expect("create file");

    db.write_file_batch(
        file.ino,
        &[
            (10, b"BBBB".to_vec()),
            (0, b"abcdefghijklmnopqrst".to_vec()),
        ],
    )
    .expect("write batch");

    assert_eq!(
        db.read_file(file.ino, 0, 20).expect("read file"),
        b"abcdefghijklmnopqrst"
    );
}

#[test]
fn offset_write_zero_fills_gap_on_read() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"sparse-ish.txt", 0o644, 1000, 1000)
        .expect("create file");

    db.write_file(file.ino, 4, b"tail").expect("write file");

    assert_eq!(
        db.read_file(file.ino, 0, 8).expect("read file"),
        b"\0\0\0\0tail"
    );
    assert_eq!(db.get_inode(file.ino).expect("load inode").size, 8);
}

#[test]
fn writes_and_reads_across_chunk_boundary() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"large.txt", 0o644, 1000, 1000)
        .expect("create file");
    let payload = vec![b'x'; 32];

    let written = db
        .write_file(file.ino, 65_536 - 8, &payload)
        .expect("write across chunk boundary");

    assert_eq!(written, payload.len() as u32);
    assert_eq!(
        db.read_file(file.ino, 65_536 - 8, payload.len() as u32)
            .expect("read across chunk boundary"),
        payload
    );
    assert_eq!(
        db.get_inode(file.ino).expect("load inode").size,
        65_536 - 8 + payload.len() as u64
    );
}

#[test]
fn truncates_file_smaller() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    db.write_file(file.ino, 0, b"hello world")
        .expect("write file");

    db.truncate_file(file.ino, 5).expect("truncate file");

    assert_eq!(db.read_file(file.ino, 0, 64).expect("read file"), b"hello");
    assert_eq!(db.get_inode(file.ino).expect("load inode").size, 5);
}

#[test]
fn truncates_file_larger_with_zero_filled_extension() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    db.write_file(file.ino, 0, b"hello").expect("write file");

    db.truncate_file(file.ino, 8).expect("truncate file");

    assert_eq!(
        db.read_file(file.ino, 0, 64).expect("read file"),
        b"hello\0\0\0"
    );
    assert_eq!(db.get_inode(file.ino).expect("load inode").size, 8);
}

#[test]
fn truncating_smaller_removes_chunks_past_new_size() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"large.txt", 0o644, 1000, 1000)
        .expect("create file");
    db.write_file(file.ino, 65_536 + 16, b"tail")
        .expect("write second chunk");

    db.truncate_file(file.ino, 4).expect("truncate file");

    assert_eq!(
        db.read_file(file.ino, 0, 64).expect("read file"),
        b"\0\0\0\0"
    );
    assert_eq!(db.get_inode(file.ino).expect("load inode").size, 4);
}

#[test]
fn unlinks_file_and_removes_its_contents() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    db.write_file(file.ino, 0, b"hello").expect("write file");

    db.unlink_file(1, b"notes.txt").expect("unlink file");

    assert_eq!(db.lookup(1, b"notes.txt"), Err(DbError::NotFound));
    assert_eq!(db.get_inode(file.ino), Err(DbError::NotFound));
    assert_eq!(db.read_file(file.ino, 0, 5), Err(DbError::NotFound));
}

#[test]
fn unlinking_open_hardlink_preserves_remaining_names() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"a.txt", 0o644, 1000, 1000)
        .expect("create file");
    db.write_file(file.ino, 0, b"hello").expect("write file");
    db.link(file.ino, 1, b"b.txt").expect("create hardlink");

    db.open_file(file.ino, dbfs::FileKind::RegularFile);
    db.unlink_file(1, b"a.txt")
        .expect("unlink one hardlink while open");
    db.release_file(file.ino);

    assert_eq!(db.lookup(1, b"a.txt"), Err(DbError::NotFound));
    let remaining = db.lookup(1, b"b.txt").expect("lookup remaining hardlink");
    assert_eq!(remaining.ino, file.ino);
    assert_eq!(remaining.nlink, 1);
    assert_eq!(db.read_file(file.ino, 0, 5).expect("read file"), b"hello");
}

#[test]
fn rejects_unlinking_directory_as_file() {
    let db = Db::open_in_memory().expect("open in-memory database");
    db.create_dir(1, b"docs", 0o755, 1000, 1000)
        .expect("create directory");

    let result = db
        .unlink_file(1, b"docs")
        .expect_err("unlinking a directory as a file should fail");

    assert_eq!(result, DbError::IsDirectory);
    assert!(db.lookup(1, b"docs").is_ok());
}

#[test]
fn removes_empty_directory() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let dir = db
        .create_dir(1, b"empty", 0o755, 1000, 1000)
        .expect("create directory");

    db.remove_dir(1, b"empty").expect("remove directory");

    assert_eq!(db.lookup(1, b"empty"), Err(DbError::NotFound));
    assert_eq!(db.get_inode(dir.ino), Err(DbError::NotFound));
}

#[test]
fn rejects_removing_non_empty_directory() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let dir = db
        .create_dir(1, b"docs", 0o755, 1000, 1000)
        .expect("create directory");
    db.create_file(dir.ino, b"readme.txt", 0o644, 1000, 1000)
        .expect("create file");

    let result = db
        .remove_dir(1, b"docs")
        .expect_err("non-empty directory should not be removed");

    assert_eq!(result, DbError::DirectoryNotEmpty);
    assert!(db.lookup(1, b"docs").is_ok());
}

#[test]
fn renames_entry_within_same_directory() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"old.txt", 0o644, 1000, 1000)
        .expect("create file");

    db.rename(1, b"old.txt", 1, b"new.txt")
        .expect("rename file");

    assert_eq!(db.lookup(1, b"old.txt"), Err(DbError::NotFound));
    assert_eq!(
        db.lookup(1, b"new.txt").expect("lookup new name").ino,
        file.ino
    );
}

#[test]
fn moves_entry_across_directories() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let source = db
        .create_dir(1, b"source", 0o755, 1000, 1000)
        .expect("create source directory");
    let target = db
        .create_dir(1, b"target", 0o755, 1000, 1000)
        .expect("create target directory");
    let file = db
        .create_file(source.ino, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");

    db.rename(source.ino, b"notes.txt", target.ino, b"notes.txt")
        .expect("move file");

    assert_eq!(db.lookup(source.ino, b"notes.txt"), Err(DbError::NotFound));
    assert_eq!(
        db.lookup(target.ino, b"notes.txt")
            .expect("lookup moved file")
            .ino,
        file.ino
    );
}

#[test]
fn rename_replaces_existing_file() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let source = db
        .create_file(1, b"source.txt", 0o644, 1000, 1000)
        .expect("create source file");
    let replaced = db
        .create_file(1, b"target.txt", 0o644, 1000, 1000)
        .expect("create target file");

    db.rename(1, b"source.txt", 1, b"target.txt")
        .expect("replace target file");

    assert_eq!(db.lookup(1, b"source.txt"), Err(DbError::NotFound));
    assert_eq!(
        db.lookup(1, b"target.txt").expect("lookup target name").ino,
        source.ino
    );
    assert_eq!(db.get_inode(replaced.ino), Err(DbError::NotFound));
}

#[test]
fn rename_over_hardlink_only_removes_destination_name() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let source = db
        .create_file(1, b"source.txt", 0o644, 1000, 1000)
        .expect("create source file");
    let target = db
        .create_file(1, b"target.txt", 0o644, 1000, 1000)
        .expect("create target file");
    db.write_file(target.ino, 0, b"target")
        .expect("write target file");
    db.link(target.ino, 1, b"survivor.txt")
        .expect("create hardlink to target");

    db.rename(1, b"source.txt", 1, b"target.txt")
        .expect("replace target file");

    assert_eq!(db.lookup(1, b"source.txt"), Err(DbError::NotFound));
    assert_eq!(
        db.lookup(1, b"target.txt").expect("lookup target name").ino,
        source.ino
    );
    let survivor = db
        .lookup(1, b"survivor.txt")
        .expect("lookup survivor hardlink");
    assert_eq!(survivor.ino, target.ino);
    assert_eq!(survivor.nlink, 1);
    assert_eq!(
        db.read_file(target.ino, 0, 6).expect("read survivor"),
        b"target"
    );
}

#[test]
fn rejects_moving_directory_into_its_descendant() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let parent = db
        .create_dir(1, b"parent", 0o755, 1000, 1000)
        .expect("create parent directory");
    let child = db
        .create_dir(parent.ino, b"child", 0o755, 1000, 1000)
        .expect("create child directory");

    let result = db
        .rename(1, b"parent", child.ino, b"parent")
        .expect_err("moving directory into descendant should fail");

    assert_eq!(result, DbError::InvalidInput);
    assert_eq!(
        db.lookup(1, b"parent").expect("lookup parent").ino,
        parent.ino
    );
}

#[test]
fn updates_inode_metadata_fields() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");

    db.update_metadata(
        file.ino,
        MetadataUpdate {
            mode: Some(0o600),
            uid: Some(2000),
            gid: Some(3000),
            atime: Some(Timestamp { secs: 123, nsec: 0 }),
            mtime: Some(Timestamp { secs: 456, nsec: 0 }),
        },
    )
    .expect("update metadata");

    let updated = db.get_inode(file.ino).expect("load inode");
    assert_eq!(updated.mode, 0o600);
    assert_eq!(updated.uid, 2000);
    assert_eq!(updated.gid, 3000);
    assert_eq!(updated.atime, Timestamp { secs: 123, nsec: 0 });
    assert_eq!(updated.mtime, Timestamp { secs: 456, nsec: 0 });
    assert!(updated.ctime.to_system_time() >= file.ctime.to_system_time());
}

#[test]
fn metadata_update_leaves_unspecified_fields_unchanged() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");

    db.update_metadata(
        file.ino,
        MetadataUpdate {
            mode: Some(0o600),
            ..MetadataUpdate::default()
        },
    )
    .expect("update metadata");

    let updated = db.get_inode(file.ino).expect("load inode");
    assert_eq!(updated.mode, 0o600);
    assert_eq!(updated.uid, file.uid);
    assert_eq!(updated.gid, file.gid);
    assert_eq!(updated.atime, file.atime);
    assert_eq!(updated.mtime, file.mtime);
}

#[test]
fn read_past_4gib_returns_the_marker_near_that_offset() {
    const FOUR_GIB: u64 = 1 << 32;
    let marker_at = FOUR_GIB + 64;
    let path = disk_db_path("read-past-4gib");
    let db = Db::open(&path).expect("open file-backed database");
    let file = db
        .create_file(1, b"sparse.bin", 0o644, 1000, 1000)
        .expect("create file");

    db.truncate_file(file.ino, marker_at + FOUR_GIB)
        .expect("extend sparse file");
    db.write_file(file.ino, marker_at, b"MARK")
        .expect("write marker");

    let data = db.read_file(file.ino, marker_at, 4).expect("read marker");

    assert_eq!(data, b"MARK");
    remove_db(&path);
}

#[test]
fn reopen_drops_inodes_left_unlinked_while_open() {
    let path = disk_db_path("reclaim-unlinked");
    let leaked_ino = {
        let db = Db::open(&path).expect("open file-backed database");
        let keep = db
            .create_file(1, b"keep.txt", 0o644, 1000, 1000)
            .expect("create file that stays linked");
        db.write_file(keep.ino, 0, b"kept")
            .expect("write kept file");
        let leaked = db
            .create_file(1, b"gone.txt", 0o644, 1000, 1000)
            .expect("create file unlinked while open");
        db.write_file(leaked.ino, 0, b"leaked-chunk")
            .expect("write chunk that should not survive");
        db.open_file(leaked.ino, FileKind::RegularFile);
        db.unlink_file(1, b"gone.txt")
            .expect("unlink while the handle is still open");
        assert_eq!(db.get_inode(leaked.ino).expect("inode still open").nlink, 0);
        leaked.ino
    };

    {
        let db = Db::open(&path).expect("reopen after crash");
        assert_eq!(db.get_inode(1).expect("root stays").ino, 1);
        assert_eq!(db.get_inode(leaked_ino), Err(DbError::NotFound));
        assert_eq!(db.lookup(1, b"gone.txt"), Err(DbError::NotFound));
        let keep = db.lookup(1, b"keep.txt").expect("linked file stays");
        assert_eq!(
            db.read_file(keep.ino, 0, 4).expect("read kept file"),
            b"kept"
        );
    }
    assert_eq!(
        count_rows(
            &path,
            "SELECT COUNT(*) FROM file_chunks WHERE ino = ?1",
            leaked_ino
        ),
        0
    );
    assert_eq!(
        count_rows(
            &path,
            "SELECT COUNT(*) FROM inodes WHERE ino = ?1",
            leaked_ino
        ),
        0
    );

    {
        let db = Db::open(&path).expect("second reopen");
        assert_eq!(
            db.get_inode(1).expect("root stays after second open").ino,
            1
        );
        assert_eq!(db.get_inode(leaked_ino), Err(DbError::NotFound));
        let keep = db.lookup(1, b"keep.txt").expect("linked file stays");
        assert_eq!(
            db.read_file(keep.ino, 0, 4).expect("read kept file"),
            b"kept"
        );
    }
    assert_eq!(
        count_rows(
            &path,
            "SELECT COUNT(*) FROM file_chunks WHERE ino = ?1",
            leaked_ino
        ),
        0
    );
    remove_db(&path);
}

#[test]
fn failed_symlink_create_leaves_no_dirent_or_inode() {
    let path = disk_db_path("symlink-rollback");
    {
        let db = Db::open(&path).expect("open file-backed database");
        db.create_file(1, b"keep.txt", 0o644, 1000, 1000)
            .expect("create file that must stay");
    }
    exec_sql(
        &path,
        "CREATE TRIGGER reject_symlink_target_insert
         BEFORE INSERT ON symlink_targets
         BEGIN
           SELECT RAISE(ABORT, 'reject symlink target');
         END;",
    );

    {
        let db = Db::open(&path).expect("reopen with rejecting trigger");
        let created = db.create_symlink(1, b"link", b"target-path", 1000, 1000);
        assert!(
            created.is_err(),
            "symlink create reports the failed target insert"
        );
        assert_eq!(db.lookup(1, b"link"), Err(DbError::NotFound));
        let entries = db.list_dir(1).expect("list root");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, b"keep.txt");
        assert_eq!(
            db.lookup(1, b"keep.txt").expect("kept file").kind,
            FileKind::RegularFile
        );
    }
    assert_eq!(
        query_i64(&path, "SELECT COUNT(*) FROM inodes WHERE ino != 1"),
        1
    );
    assert_eq!(query_i64(&path, "SELECT COUNT(*) FROM dirents"), 1);
    assert_eq!(query_i64(&path, "SELECT COUNT(*) FROM symlink_targets"), 0);

    exec_sql(&path, "DROP TRIGGER reject_symlink_target_insert;");
    {
        let db = Db::open(&path).expect("reopen after dropping trigger");
        let link = db
            .create_symlink(1, b"link", b"target-path", 1000, 1000)
            .expect("symlink create commits the node and the target together");
        assert_eq!(
            db.read_symlink(link.ino).expect("read symlink"),
            b"target-path"
        );
        assert_eq!(link.size, b"target-path".len() as u64);
        assert_eq!(db.lookup(1, b"link").expect("lookup symlink").ino, link.ino);
    }
    remove_db(&path);
}

#[test]
fn new_database_is_created_at_current_schema_version() {
    let path = disk_db_path("schema-new");
    {
        let db = Db::open(&path).expect("open new database");
        assert_eq!(
            db.pragma_i64("user_version").expect("read user_version"),
            SCHEMA_VERSION
        );
    }
    remove_db(&path);
}

#[test]
fn unversioned_legacy_database_migrates_to_current_schema() {
    let path = disk_db_path("schema-legacy");
    exec_sql(
        &path,
        "CREATE TABLE inodes (
           ino INTEGER PRIMARY KEY, kind INTEGER NOT NULL, mode INTEGER NOT NULL,
           uid INTEGER NOT NULL, gid INTEGER NOT NULL, size INTEGER NOT NULL,
           atime INTEGER NOT NULL, mtime INTEGER NOT NULL, ctime INTEGER NOT NULL,
           nlink INTEGER NOT NULL
         );
         INSERT INTO inodes VALUES (1, 2, 493, 1, 1, 0, 1, 1, 1, 2);",
    );
    assert_eq!(query_i64(&path, "PRAGMA user_version"), 0);
    {
        let db = Db::open(&path).expect("open and migrate legacy database");
        db.create_file(1, b"after-migration", 0o644, 1, 1)
            .expect("legacy database is writable after migration");
    }
    assert_eq!(query_i64(&path, "PRAGMA user_version"), SCHEMA_VERSION);
    assert_eq!(
        query_i64(
            &path,
            "SELECT COUNT(*) FROM pragma_table_info('inodes') WHERE name LIKE '%_nsec'"
        ),
        3
    );
    assert_eq!(
        query_i64(
            &path,
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'dirents_child'"
        ),
        1
    );
    // Opening again is a no-op.
    drop(Db::open(&path).expect("reopen migrated database"));
    assert_eq!(query_i64(&path, "PRAGMA user_version"), SCHEMA_VERSION);
    remove_db(&path);
}

#[test]
fn database_from_newer_dbfs_is_rejected_untouched() {
    let path = disk_db_path("schema-newer");
    drop(Db::open(&path).expect("create database"));
    let newer = SCHEMA_VERSION + 1;
    exec_sql(&path, &format!("PRAGMA user_version = {newer}"));
    assert_eq!(
        Db::open(&path).err(),
        Some(DbError::UnsupportedSchema(newer))
    );
    assert_eq!(query_i64(&path, "PRAGMA user_version"), newer);
    remove_db(&path);
}

#[test]
fn child_ino_lookups_use_an_index() {
    let path = disk_db_path("schema-child-index");
    drop(Db::open(&path).expect("create database"));
    let conn = Connection::open(&path).expect("open raw sqlite");
    for sql in [
        "SELECT parent_ino FROM dirents WHERE child_ino = 5",
        "DELETE FROM dirents WHERE child_ino = 5",
    ] {
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("prepare plan")
            .query_map([], |row| row.get::<_, String>(3))
            .expect("query plan")
            .collect::<Result<_, _>>()
            .expect("read plan");
        assert!(
            plan.iter().any(|step| step.contains("dirents_child")),
            "{sql} should use dirents_child, got {plan:?}"
        );
    }
    drop(conn);
    remove_db(&path);
}

#[test]
fn version_2_dirents_are_rebuilt_with_stable_ids() {
    let path = disk_db_path("schema-v2-dirents");
    let (docs, notes) = {
        let db = Db::open(&path).expect("create database");
        let docs = db.create_dir(1, b"docs", 0o755, 1, 1).expect("mkdir");
        let notes = db.create_file(docs.ino, b"notes", 0o644, 1, 1).expect("create");
        db.write_file(notes.ino, 0, b"kept").expect("write");
        (docs, notes)
    };
    // Rewrite dirents in the version 2 layout: composite primary key, no id.
    exec_sql(
        &path,
        "CREATE TABLE dirents_v2 (
           parent_ino INTEGER NOT NULL, name BLOB NOT NULL, child_ino INTEGER NOT NULL,
           PRIMARY KEY (parent_ino, name),
           FOREIGN KEY (parent_ino) REFERENCES inodes(ino) ON DELETE CASCADE,
           FOREIGN KEY (child_ino) REFERENCES inodes(ino) ON DELETE CASCADE
         );
         INSERT INTO dirents_v2 SELECT parent_ino, name, child_ino FROM dirents;
         DROP TABLE dirents;
         ALTER TABLE dirents_v2 RENAME TO dirents;
         CREATE INDEX dirents_child ON dirents(child_ino);
         PRAGMA user_version = 2;",
    );
    {
        let db = Db::open(&path).expect("migrate version 2 database");
        assert_eq!(db.lookup(1, b"docs").expect("docs survives").ino, docs.ino);
        let found = db.lookup(docs.ino, b"notes").expect("notes survives");
        assert_eq!(found.ino, notes.ino);
        assert_eq!(db.read_file(notes.ino, 0, 4).expect("read"), b"kept");
        assert_eq!(
            db.create_file(1, b"docs", 0o644, 1, 1).err(),
            Some(DbError::AlreadyExists),
            "names stay unique per directory"
        );
    }
    assert_eq!(query_i64(&path, "PRAGMA user_version"), SCHEMA_VERSION);
    assert_eq!(
        query_i64(&path, "SELECT COUNT(*) FROM pragma_table_info('dirents') WHERE name = 'id'"),
        1
    );
    remove_db(&path);
}

#[test]
fn directory_listing_query_uses_the_scan_index_without_sorting() {
    let path = disk_db_path("schema-scan-index");
    drop(Db::open(&path).expect("create database"));
    let conn = Connection::open(&path).expect("open raw sqlite");
    let plan: Vec<String> = conn
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT d.id, d.name, d.child_ino, i.kind
             FROM dirents d JOIN inodes i ON i.ino = d.child_ino
             WHERE d.parent_ino = 1 AND d.id > 5 ORDER BY d.id",
        )
        .expect("prepare plan")
        .query_map([], |row| row.get::<_, String>(3))
        .expect("query plan")
        .collect::<Result<_, _>>()
        .expect("read plan");
    assert!(
        plan.iter().any(|step| step.contains("dirents_scan")),
        "listing should use dirents_scan, got {plan:?}"
    );
    assert!(
        !plan.iter().any(|step| step.contains("TEMP B-TREE")),
        "listing should not sort, got {plan:?}"
    );
    drop(conn);
    remove_db(&path);
}

fn disk_db_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".bench")
        .join("correctness");
    std::fs::create_dir_all(&dir).expect("create disk-backed test directory");
    dir.join(format!("dbfs-{name}-{}-{nanos}.sqlite", std::process::id()))
}

fn remove_db(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

fn count_rows(path: &Path, sql: &str, ino: u64) -> i64 {
    let conn = Connection::open(path).expect("open raw sqlite");
    conn.query_row(sql, params![ino as i64], |row| row.get(0))
        .expect("count rows")
}

fn query_i64(path: &Path, sql: &str) -> i64 {
    let conn = Connection::open(path).expect("open raw sqlite");
    conn.query_row(sql, [], |row| row.get(0))
        .expect("query scalar")
}

fn exec_sql(path: &Path, sql: &str) {
    let conn = Connection::open(path).expect("open raw sqlite");
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .expect("set busy timeout");
    conn.execute_batch(sql).expect("execute sql");
}
