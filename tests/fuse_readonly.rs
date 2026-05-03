use dbfs::{Db, Dbfs, DbfsDirEntry, FileKind, mount_options};
use fuser::{FileType, MountOption};
use rusqlite::{Connection, params};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_db_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    path.push(format!(
        "dbfs-fuse-{name}-{}-{nanos}.sqlite",
        std::process::id()
    ));
    path
}

#[test]
fn gets_fuse_attributes_by_inode() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o640, 1000, 1001)
        .expect("create file");
    let fs = Dbfs::new(db);

    let attr = fs.getattr(file.ino).expect("get file attr");

    assert_eq!(attr.ino, file.ino);
    assert_eq!(attr.kind, FileType::RegularFile);
    assert_eq!(attr.perm, 0o640);
    assert_eq!(attr.uid, 1000);
    assert_eq!(attr.gid, 1001);
}

#[test]
fn looks_up_child_attributes_by_parent_and_name() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let dir = db
        .create_dir(1, b"docs", 0o755, 1000, 1000)
        .expect("create directory");
    let fs = Dbfs::new(db);

    let attr = fs.lookup(1, b"docs").expect("lookup docs");

    assert_eq!(attr.ino, dir.ino);
    assert_eq!(attr.kind, FileType::Directory);
}

#[test]
fn reads_directory_entries_with_dot_entries_first() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let docs = db
        .create_dir(1, b"docs", 0o755, 1000, 1000)
        .expect("create directory");
    let notes = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    let entries = fs.readdir(1, 0).expect("read root directory");

    assert_eq!(
        entries,
        vec![
            DbfsDirEntry {
                name: b".".to_vec(),
                ino: 1,
                kind: FileType::Directory,
            },
            DbfsDirEntry {
                name: b"..".to_vec(),
                ino: 1,
                kind: FileType::Directory,
            },
            DbfsDirEntry {
                name: b"docs".to_vec(),
                ino: docs.ino,
                kind: FileType::Directory,
            },
            DbfsDirEntry {
                name: b"notes.txt".to_vec(),
                ino: notes.ino,
                kind: FileType::RegularFile,
            },
        ]
    );
}

#[test]
fn maps_storage_errors_to_errno() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let fs = Dbfs::new(db);

    let error = fs.lookup(1, b"missing").expect_err("lookup should fail");

    assert_eq!(error, libc::ENOENT);
}

#[test]
fn maps_duplicate_name_to_eexist() {
    let db = Db::open_in_memory().expect("open in-memory database");
    db.create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    let error = fs
        .create(1, b"notes.txt", 0o644, 0, 1000, 1000)
        .expect_err("duplicate create should fail");

    assert_eq!(error, libc::EEXIST);
}

#[test]
fn maps_non_empty_directory_removal_to_enotempty() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let docs = db
        .create_dir(1, b"docs", 0o755, 1000, 1000)
        .expect("create directory");
    db.create_file(docs.ino, b"readme.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    let error = fs.rmdir(1, b"docs").expect_err("rmdir should fail");

    assert_eq!(error, libc::ENOTEMPTY);
}

#[test]
fn maps_create_under_file_to_enotdir() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    let error = fs
        .create(file.ino, b"child.txt", 0o644, 0, 1000, 1000)
        .expect_err("create under file should fail");

    assert_eq!(error, libc::ENOTDIR);
}

#[test]
fn converts_storage_file_kind_to_fuse_file_type() {
    assert_eq!(
        DbfsDirEntry::file_type(FileKind::RegularFile),
        FileType::RegularFile
    );
    assert_eq!(
        DbfsDirEntry::file_type(FileKind::Directory),
        FileType::Directory
    );
}

#[test]
fn mount_options_do_not_require_allow_other() {
    let options = mount_options();

    assert!(options.contains(&MountOption::FSName("dbfs".to_string())));
    assert!(!options.contains(&MountOption::AutoUnmount));
    assert!(!options.contains(&MountOption::AllowOther));
    assert!(!options.contains(&MountOption::AllowRoot));
}

#[test]
fn creates_directory_through_fuse_helper() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let fs = Dbfs::new(db);

    let attr = fs.mkdir(1, b"docs", 0o755, 0, 1000, 1001).expect("mkdir");

    assert_eq!(attr.kind, FileType::Directory);
    assert_eq!(attr.perm, 0o755);
    assert_eq!(attr.uid, 1000);
    assert_eq!(attr.gid, 1001);
    assert_eq!(fs.lookup(1, b"docs").expect("lookup docs").ino, attr.ino);
}

#[test]
fn creates_file_through_fuse_helper() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let fs = Dbfs::new(db);

    let attr = fs
        .create(1, b"notes.txt", 0o644, 0, 1000, 1001)
        .expect("create file");

    assert_eq!(attr.kind, FileType::RegularFile);
    assert_eq!(attr.perm, 0o644);
    assert_eq!(attr.uid, 1000);
    assert_eq!(attr.gid, 1001);
    assert_eq!(
        fs.lookup(1, b"notes.txt").expect("lookup file").ino,
        attr.ino
    );
}

#[test]
fn creation_helpers_apply_umask() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let fs = Dbfs::new(db);

    let attr = fs
        .create(1, b"private.txt", 0o666, 0o027, 1000, 1000)
        .expect("create file");

    assert_eq!(attr.perm, 0o640);
}

#[test]
fn setattr_helper_updates_metadata_and_size() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    db.write_file(file.ino, 0, b"hello world")
        .expect("write file");
    let fs = Dbfs::new(db);

    let attr = fs
        .setattr(
            file.ino,
            Some(0o600),
            Some(2000),
            Some(3000),
            Some(5),
            Some(123),
            Some(456),
        )
        .expect("set attrs");

    assert_eq!(attr.perm, 0o600);
    assert_eq!(attr.uid, 2000);
    assert_eq!(attr.gid, 3000);
    assert_eq!(attr.size, 5);
}

#[test]
fn opens_regular_file_with_inode_file_handle() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    let handle = fs.open(file.ino).expect("open file");

    assert_eq!(handle, file.ino);
}

#[test]
fn reads_regular_file_through_fuse_helper() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    db.write_file(file.ino, 0, b"hello dbfs")
        .expect("write file");
    let fs = Dbfs::new(db);

    let data = fs.read(file.ino, 6, 4).expect("read file");

    assert_eq!(data, b"dbfs");
}

#[test]
fn writes_regular_file_through_fuse_helper() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    let written = fs.write(file.ino, 0, b"hello").expect("write file");

    assert_eq!(written, 5);
    assert_eq!(fs.read(file.ino, 0, 5).expect("read file"), b"hello");
}

#[test]
fn buffers_fuse_writes_until_flush() {
    let path = temp_db_path("buffered-write");
    let db = Db::open(&path).expect("open file-backed database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    fs.write(file.ino, 0, b"hello").expect("write file");

    assert_eq!(
        fs.read(file.ino, 0, 5).expect("read buffered file"),
        b"hello"
    );
    assert_eq!(fs.getattr(file.ino).expect("dirty attr").size, 5);
    assert_eq!(raw_inode_size(&path, file.ino), 0);

    fs.flush_file(file.ino).expect("flush file");

    assert_eq!(raw_inode_size(&path, file.ino), 5);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

#[test]
fn release_flushes_buffered_fuse_writes() {
    let path = temp_db_path("release-buffered-write");
    let db = Db::open(&path).expect("open file-backed database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    fs.open(file.ino).expect("open file");
    fs.write(file.ino, 0, b"hello").expect("write file");
    fs.release_file(file.ino).expect("release file");

    assert_eq!(raw_inode_size(&path, file.ino), 5);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

fn raw_inode_size(path: &std::path::Path, ino: u64) -> i64 {
    Connection::open(path)
        .expect("open raw sqlite connection")
        .query_row(
            "SELECT size FROM inodes WHERE ino = ?1",
            params![ino as i64],
            |row| row.get(0),
        )
        .expect("read raw inode size")
}

#[test]
fn unlinks_file_through_fuse_helper() {
    let db = Db::open_in_memory().expect("open in-memory database");
    db.create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    fs.unlink(1, b"notes.txt").expect("unlink file");

    assert_eq!(fs.lookup(1, b"notes.txt"), Err(libc::ENOENT));
}

#[test]
fn removes_directory_through_fuse_helper() {
    let db = Db::open_in_memory().expect("open in-memory database");
    db.create_dir(1, b"docs", 0o755, 1000, 1000)
        .expect("create directory");
    let fs = Dbfs::new(db);

    fs.rmdir(1, b"docs").expect("remove directory");

    assert_eq!(fs.lookup(1, b"docs"), Err(libc::ENOENT));
}

#[test]
fn renames_entry_through_fuse_helper() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"old.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    fs.rename(1, b"old.txt", 1, b"new.txt")
        .expect("rename file");

    assert_eq!(fs.lookup(1, b"old.txt"), Err(libc::ENOENT));
    assert_eq!(fs.lookup(1, b"new.txt").expect("lookup new").ino, file.ino);
}
