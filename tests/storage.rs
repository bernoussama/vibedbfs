use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use dbfs::{Db, DbError, FileKind, MetadataUpdate};
use rusqlite::Connection;

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
    assert_eq!(root.nlink, 1);
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
            atime: Some(123),
            mtime: Some(456),
        },
    )
    .expect("update metadata");

    let updated = db.get_inode(file.ino).expect("load inode");
    assert_eq!(updated.mode, 0o600);
    assert_eq!(updated.uid, 2000);
    assert_eq!(updated.gid, 3000);
    assert_eq!(updated.atime, 123);
    assert_eq!(updated.mtime, 456);
    assert!(updated.ctime >= file.ctime);
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
