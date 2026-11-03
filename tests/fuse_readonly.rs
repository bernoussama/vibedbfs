use dbfs::{Db, Dbfs, DbfsDirEntry, FileKind, mount_options};
use fuser::{FileType, MountOption};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
fn write_read_and_truncate_use_posix_errno_for_non_regular_files() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let fs = Dbfs::new(db);
    let dir = fs.mkdir(1, b"docs", 0o755, 0, 1000, 1000).expect("mkdir");
    let link = fs
        .symlink(1, b"link", b"target", 1000, 1000)
        .expect("symlink");

    assert_eq!(
        fs.write(dir.ino, 0, b"x").expect_err("write dir"),
        libc::EISDIR
    );
    assert_eq!(fs.read(dir.ino, 0, 4).expect_err("read dir"), libc::EISDIR);
    assert_eq!(
        fs.setattr(dir.ino, None, None, None, Some(0), None, None)
            .expect_err("truncate dir"),
        libc::EISDIR
    );
    assert_eq!(
        fs.write(link.ino, 0, b"x").expect_err("write symlink"),
        libc::EINVAL
    );
    assert_eq!(
        fs.read(link.ino, 0, 4).expect_err("read symlink"),
        libc::EINVAL
    );
    assert_eq!(
        fs.setattr(link.ino, None, None, None, Some(0), None, None)
            .expect_err("truncate symlink"),
        libc::EINVAL
    );
    assert_eq!(
        fs.write(9_999, 0, b"x").expect_err("write missing"),
        libc::ENOENT
    );
}

#[test]
fn names_longer_than_255_bytes_return_enametoolong() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let fs = Dbfs::new(db);
    let too_long = vec![b'a'; 256];
    let max_name = vec![b'b'; 255];

    assert_eq!(
        fs.create(1, &too_long, 0o644, 0, 1000, 1000)
            .expect_err("create long name"),
        libc::ENAMETOOLONG
    );
    assert_eq!(
        fs.mkdir(1, &too_long, 0o755, 0, 1000, 1000)
            .expect_err("mkdir long name"),
        libc::ENAMETOOLONG
    );
    assert_eq!(
        fs.symlink(1, &too_long, b"target", 1000, 1000)
            .expect_err("symlink long name"),
        libc::ENAMETOOLONG
    );
    assert_eq!(
        fs.lookup(1, &too_long).expect_err("lookup long name"),
        libc::ENAMETOOLONG
    );
    assert_eq!(
        fs.lookup(1, &max_name)
            .expect_err("lookup missing max name"),
        libc::ENOENT
    );

    let created = fs
        .create(1, &max_name, 0o644, 0, 1000, 1000)
        .expect("create 255-byte name");
    assert_eq!(
        fs.rename(1, &max_name, 1, &too_long, 0)
            .expect_err("rename to long name"),
        libc::ENAMETOOLONG
    );
    assert_eq!(
        fs.lookup(1, &max_name).expect("original name remains").ino,
        created.ino
    );
}

#[test]
fn check_constraint_is_not_reported_as_eexist() {
    let path = temp_db_path("check-constraint");
    {
        let conn = Connection::open(&path).expect("open raw sqlite connection");
        conn.execute_batch(
            "
            CREATE TABLE inodes (
              ino INTEGER PRIMARY KEY,
              kind INTEGER NOT NULL,
              mode INTEGER NOT NULL,
              uid INTEGER NOT NULL CHECK (uid = 1),
              gid INTEGER NOT NULL,
              size INTEGER NOT NULL,
              atime INTEGER NOT NULL,
              mtime INTEGER NOT NULL,
              ctime INTEGER NOT NULL,
              nlink INTEGER NOT NULL
            );
            INSERT INTO inodes
              (ino, kind, mode, uid, gid, size, atime, mtime, ctime, nlink)
              VALUES (1, 2, 493, 1, 1, 0, 1, 1, 1, 2);
            ",
        )
        .expect("create schema with a check constraint");
    }

    let db = Db::open(&path).expect("open database");
    let fs = Dbfs::new(db);
    let error = fs
        .create(1, b"notes.txt", 0o644, 0, 1000, 1000)
        .expect_err("check constraint should fail");

    assert_eq!(error, libc::EIO);
    assert_eq!(fs.lookup(1, b"notes.txt"), Err(libc::ENOENT));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
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
fn directory_nlink_counts_subdirectories() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let fs = Dbfs::new(db);

    assert_eq!(fs.getattr(1).expect("root").nlink, 2);

    let docs = fs
        .mkdir(1, b"docs", 0o755, 0, 1000, 1000)
        .expect("mkdir docs");
    assert_eq!(docs.nlink, 2);
    assert_eq!(fs.getattr(1).expect("root after docs").nlink, 3);

    fs.mkdir(docs.ino, b"nested", 0o755, 0, 1000, 1000)
        .expect("mkdir nested");
    assert_eq!(fs.getattr(docs.ino).expect("docs after nested").nlink, 3);
    assert_eq!(fs.getattr(1).expect("root ignores nested").nlink, 3);

    fs.create(1, b"notes.txt", 0o644, 0, 1000, 1000)
        .expect("create file");
    assert_eq!(fs.getattr(1).expect("root ignores file").nlink, 3);

    let other = fs
        .mkdir(1, b"other", 0o755, 0, 1000, 1000)
        .expect("mkdir other");
    assert_eq!(fs.getattr(1).expect("root after other").nlink, 4);

    fs.rename(docs.ino, b"nested", other.ino, b"nested", 0)
        .expect("move nested");
    assert_eq!(fs.getattr(docs.ino).expect("docs after move").nlink, 2);
    assert_eq!(fs.getattr(other.ino).expect("other after move").nlink, 3);
    assert_eq!(fs.getattr(1).expect("root after move").nlink, 4);

    fs.rmdir(other.ino, b"nested").expect("remove nested");
    assert_eq!(fs.getattr(other.ino).expect("other after rmdir").nlink, 2);

    fs.rename(1, b"docs", 1, b"other", 0)
        .expect("replace empty directory");
    assert_eq!(fs.getattr(1).expect("root after replace").nlink, 3);

    fs.rmdir(1, b"other").expect("remove remaining directory");
    assert_eq!(fs.getattr(1).expect("root after rmdir").nlink, 2);
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
            Some(UNIX_EPOCH + Duration::from_secs(123)),
            Some(UNIX_EPOCH + Duration::from_secs(456)),
        )
        .expect("set attrs");

    assert_eq!(attr.perm, 0o600);
    assert_eq!(attr.uid, 2000);
    assert_eq!(attr.gid, 3000);
    assert_eq!(attr.size, 5);
    assert_eq!(attr.atime, UNIX_EPOCH + Duration::from_secs(123));
    assert_eq!(attr.mtime, UNIX_EPOCH + Duration::from_secs(456));
}

#[test]
fn setattr_round_trips_nanosecond_timestamps() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);
    let atime = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
    let mtime = UNIX_EPOCH + Duration::new(1_700_000_111, 987_654_321);

    let attr = fs
        .setattr(file.ino, None, None, None, None, Some(atime), Some(mtime))
        .expect("set times");

    assert_eq!(attr.atime, atime);
    assert_eq!(attr.mtime, mtime);
    let again = fs.getattr(file.ino).expect("get file attr");
    assert_eq!(again.atime, atime);
    assert_eq!(again.mtime, mtime);
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
fn write_updates_mtime_before_flush() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);
    let old = UNIX_EPOCH + Duration::from_secs(10);
    fs.setattr(file.ino, None, None, None, None, Some(old), Some(old))
        .expect("set old times");

    fs.write(file.ino, 0, b"hello").expect("write file");
    let during = fs.getattr(file.ino).expect("getattr before flush");
    assert!(during.mtime > old, "mtime advances at write");
    assert!(during.ctime > old, "ctime advances at write");

    fs.flush_file(file.ino).expect("flush file");
    let after = fs.getattr(file.ino).expect("getattr after flush");
    assert_eq!(after.mtime, during.mtime);
    assert_eq!(after.ctime, during.ctime);
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

#[test]
fn buffered_overlapping_writes_read_back_in_write_order() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let persisted = vec![b'a'; 70_000];
    db.write_file(file.ino, 0, &persisted)
        .expect("write persisted data");
    let fs = Dbfs::new(db);

    let writes: [(usize, &[u8]); 4] = [
        (65_530, b"BBBBBBBBBBBB"),
        (65_534, b"CC"),
        (10, b"DDDD"),
        (80_000, b"EE"),
    ];
    let mut expected = persisted;
    for (offset, data) in writes {
        fs.write(file.ino, offset as u64, data)
            .expect("buffer write");
        if expected.len() < offset + data.len() {
            expected.resize(offset + data.len(), 0);
        }
        expected[offset..offset + data.len()].copy_from_slice(data);
    }

    assert_eq!(
        fs.read(file.ino, 0, 100_000).expect("read buffered file"),
        expected
    );
    fs.flush_file(file.ino).expect("flush file");
    assert_eq!(
        fs.read(file.ino, 0, 100_000).expect("read flushed file"),
        expected
    );
    assert_eq!(
        fs.getattr(file.ino).expect("flushed attr").size,
        expected.len() as u64
    );
}

#[test]
fn read_with_dirty_size_past_4gib_returns_the_marker() {
    const FOUR_GIB: u64 = 1 << 32;
    let marker_at = FOUR_GIB + 64;
    let path = disk_db_path("dirty-read-past-4gib");
    let db = Db::open(&path).expect("open file-backed database");
    let file = db
        .create_file(1, b"sparse.bin", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    fs.write(file.ino, marker_at, b"MARK")
        .expect("buffer marker");
    fs.write(file.ino, marker_at + FOUR_GIB, b"X")
        .expect("extend dirty size by 4 GiB");

    let data = fs.read(file.ino, marker_at, 4).expect("read marker");

    assert_eq!(data, b"MARK");
    remove_db(&path);
}

#[test]
fn release_after_failed_flush_deletes_inode_unlinked_while_open() {
    let path = disk_db_path("release-flush-fail");
    let db = Db::open(&path).expect("open file-backed database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let ino = file.ino;
    db.open_file(ino, FileKind::RegularFile);
    db.open_file(ino, FileKind::RegularFile);
    let fs = Dbfs::new(db);

    fs.unlink(1, b"notes.txt").expect("unlink while open");
    fs.write(ino, 0, b"dirty").expect("buffer a write");
    exec_sql(
        &path,
        "CREATE TRIGGER reject_chunk_insert
         BEFORE INSERT ON file_chunks
         BEGIN
           SELECT RAISE(ABORT, 'reject chunk');
         END;",
    );

    assert!(
        fs.release_file(ino).is_err(),
        "first release reports the flush error"
    );
    assert!(
        fs.getattr(ino).is_ok(),
        "the other handle still keeps the inode"
    );

    assert!(
        fs.release_file(ino).is_err(),
        "last release still reports the flush error"
    );
    assert_eq!(fs.getattr(ino), Err(libc::ENOENT));

    exec_sql(&path, "DROP TRIGGER reject_chunk_insert;");
    assert!(
        fs.release_file(ino).is_ok(),
        "a later release finishes with the dirty buffer gone"
    );
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
    dir.join(format!(
        "dbfs-fuse-{name}-{}-{nanos}.sqlite",
        std::process::id()
    ))
}

fn remove_db(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

fn exec_sql(path: &Path, sql: &str) {
    let conn = Connection::open(path).expect("open raw sqlite");
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .expect("set busy timeout");
    conn.execute_batch(sql).expect("execute sql");
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
fn rename_noreplace_leaves_an_existing_target_in_place() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let source = db
        .create_file(1, b"source.txt", 0o644, 1000, 1000)
        .expect("create source");
    let target = db
        .create_file(1, b"target.txt", 0o644, 1000, 1000)
        .expect("create target");
    db.write_file(source.ino, 0, b"source")
        .expect("write source");
    db.write_file(target.ino, 0, b"target")
        .expect("write target");
    let fs = Dbfs::new(db);

    let error = fs
        .rename(1, b"source.txt", 1, b"target.txt", libc::RENAME_NOREPLACE)
        .expect_err("noreplace existing");
    assert_eq!(error, libc::EEXIST);
    assert_eq!(
        fs.lookup(1, b"source.txt").expect("source remains").ino,
        source.ino
    );
    assert_eq!(fs.read(target.ino, 0, 6).expect("target bytes"), b"target");

    fs.rename(1, b"source.txt", 1, b"fresh.txt", libc::RENAME_NOREPLACE)
        .expect("noreplace missing");
    assert_eq!(fs.lookup(1, b"source.txt"), Err(libc::ENOENT));
    assert_eq!(fs.lookup(1, b"fresh.txt").expect("fresh").ino, source.ino);

    let error = fs
        .rename(1, b"fresh.txt", 1, b"target.txt", libc::RENAME_EXCHANGE)
        .expect_err("unknown flag");
    assert_eq!(error, libc::EINVAL);
    assert_eq!(
        fs.lookup(1, b"fresh.txt").expect("fresh kept").ino,
        source.ino
    );
    assert_eq!(fs.read(target.ino, 0, 6).expect("target kept"), b"target");
}

#[test]
fn renames_entry_through_fuse_helper() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"old.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    fs.rename(1, b"old.txt", 1, b"new.txt", 0)
        .expect("rename file");

    assert_eq!(fs.lookup(1, b"old.txt"), Err(libc::ENOENT));
    assert_eq!(fs.lookup(1, b"new.txt").expect("lookup new").ino, file.ino);
}
