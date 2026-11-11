use dbfs::{Db, Dbfs, DbfsDirEntry, FileKind, mount_options};
use fuser::{FileType, MountOption};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
                offset: 1,
            },
            DbfsDirEntry {
                name: b"..".to_vec(),
                ino: 1,
                kind: FileType::Directory,
                offset: 2,
            },
            DbfsDirEntry {
                name: b"docs".to_vec(),
                ino: docs.ino,
                kind: FileType::Directory,
                offset: 3,
            },
            DbfsDirEntry {
                name: b"notes.txt".to_vec(),
                ino: notes.ino,
                kind: FileType::RegularFile,
                offset: 4,
            },
        ]
    );
}

#[test]
fn readdir_resumes_without_skipping_while_entries_are_deleted() {
    // `rm -rf` pattern: read a page, delete what was returned, read the next
    // page from the last cookie. Positional cookies skipped about half the
    // directory here; stable id cookies must visit every entry once.
    let db = Db::open_in_memory().expect("open in-memory database");
    let dir = db.create_dir(1, b"big", 0o755, 1000, 1000).expect("mkdir");
    let total = 1_000;
    for i in 0..total {
        db.create_file(dir.ino, format!("f{i:04}").as_bytes(), 0o644, 1000, 1000)
            .expect("create file");
    }
    let fs = Dbfs::new(db);

    let mut offset = 0;
    let mut seen = std::collections::HashSet::new();
    loop {
        let mut page = Vec::new();
        fs.readdir_stream(dir.ino, offset, |_, next, _, name| {
            page.push((next, name.to_vec()));
            page.len() == 37
        })
        .expect("readdir page");
        if page.is_empty() {
            break;
        }
        for (next, name) in page {
            offset = next;
            if name != b"." && name != b".." {
                assert!(seen.insert(name.clone()), "entry {name:?} returned twice");
                fs.unlink(dir.ino, &name).expect("unlink listed entry");
            }
        }
    }

    assert_eq!(seen.len(), total);
    assert_eq!(fs.rmdir(1, b"big"), Ok(()));
}

#[test]
fn readdir_dotdot_points_at_the_parent_directory() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let outer = db
        .create_dir(1, b"outer", 0o755, 1000, 1000)
        .expect("mkdir outer");
    let inner = db
        .create_dir(outer.ino, b"inner", 0o755, 1000, 1000)
        .expect("mkdir inner");
    let fs = Dbfs::new(db);

    let entries = fs.readdir(inner.ino, 0).expect("readdir inner");
    assert_eq!(entries[1].name, b"..");
    assert_eq!(entries[1].ino, outer.ino);
}

#[test]
fn readdir_cookie_survives_rename_within_the_directory() {
    let db = Db::open_in_memory().expect("open in-memory database");
    for name in [b"a", b"b", b"c"] {
        db.create_file(1, name, 0o644, 1000, 1000).expect("create");
    }
    let fs = Dbfs::new(db);

    let first = fs.readdir(1, 0).expect("readdir");
    let after_a = first
        .iter()
        .find(|e| e.name == b"a")
        .expect("a listed")
        .offset;
    fs.rename(1, b"a", 1, b"z", 0).expect("rename a to z");

    let rest: Vec<_> = fs
        .readdir(1, after_a)
        .expect("resume readdir")
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(rest, vec![b"b".to_vec(), b"c".to_vec()]);
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
              atime_ns INTEGER NOT NULL,
              mtime_ns INTEGER NOT NULL,
              ctime_ns INTEGER NOT NULL,
              nlink INTEGER NOT NULL
            );
            INSERT INTO inodes
              (ino, kind, mode, uid, gid, size, atime_ns, mtime_ns, ctime_ns, nlink)
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
    let _ = std::fs::remove_file(path.with_extension("sqlite.lock"));
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
fn open_returns_a_handle_that_reads_the_file() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    db.write_file(file.ino, 0, b"hello").expect("write file");
    let fs = Dbfs::new(db);

    let handle = fs.open(file.ino).expect("open file");

    assert_eq!(
        fs.read_fh(handle, 0, 5).expect("read through handle"),
        b"hello"
    );
    assert_eq!(fs.open(9_999), Err(libc::ENOENT));
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
    // The create commits within about 5 ms. The buffered write has not reached
    // the database, so the stored size is still 0.
    wait_for_raw_inode_size(&path, file.ino, 0);

    fs.flush_file(file.ino).expect("flush file");

    // Flush writes into the open batch, which commits within about 5 ms.
    wait_for_raw_inode_size(&path, file.ino, 5);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    let _ = std::fs::remove_file(path.with_extension("sqlite.lock"));
}

#[test]
fn release_flushes_buffered_fuse_writes() {
    let path = temp_db_path("release-buffered-write");
    let db = Db::open(&path).expect("open file-backed database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);

    let fh = fs.open(file.ino).expect("open file");
    fs.write(file.ino, 0, b"hello").expect("write file");
    fs.release_file(fh).expect("release file");

    wait_for_raw_inode_size(&path, file.ino, 5);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    let _ = std::fs::remove_file(path.with_extension("sqlite.lock"));
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
fn releasing_a_file_unlinked_while_open_needs_no_flush_and_deletes_it() {
    let path = disk_db_path("release-flush-fail");
    let db = Db::open(&path).expect("open file-backed database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let ino = file.ino;
    let fs = Dbfs::new(db);
    let first = fs.open(ino).expect("open first handle");
    let second = fs.open(ino).expect("open second handle");

    fs.unlink(1, b"notes.txt").expect("unlink while open");
    fs.write(ino, 0, b"dirty").expect("buffer a write");
    // Any chunk write would now fail. Releasing must not need one: the data of an
    // unlinked file dies with its last handle.
    exec_sql(
        &path,
        "CREATE TRIGGER reject_chunk_insert
         BEFORE INSERT ON file_chunks
         BEGIN
           SELECT RAISE(ABORT, 'reject chunk');
         END;",
    );

    assert_eq!(fs.release_file(first), Ok(()), "first release");
    assert_eq!(
        fs.read(ino, 0, 5).expect("read through the open inode"),
        b"dirty",
        "the other handle still sees the buffered data"
    );
    assert_eq!(fs.getattr(ino).expect("still open").nlink, 0);

    assert_eq!(fs.release_file(second), Ok(()), "last release");
    assert_eq!(fs.getattr(ino), Err(libc::ENOENT));
    assert_eq!(fs.release_file(second), Err(libc::EBADF), "double release");
    assert_eq!(fs.flush_all(), Ok(()), "nothing is left dirty");
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
    let _ = std::fs::remove_file(path.with_extension("sqlite.lock"));
}

fn exec_sql(path: &Path, sql: &str) {
    let conn = Connection::open(path).expect("open raw sqlite");
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .expect("set busy timeout");
    conn.execute_batch(sql).expect("execute sql");
}

/// Waits until a raw connection sees `ino` with size `want`. The batch commits
/// within about 5 ms of its first uncommitted operation, so a row can lag the
/// operation that changed it. Panics after five seconds.
fn wait_for_raw_inode_size(path: &std::path::Path, ino: u64, want: i64) {
    let conn = Connection::open(path).expect("open raw sqlite connection");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let size: Option<i64> = conn
            .query_row(
                "SELECT size FROM inodes WHERE ino = ?1",
                params![ino as i64],
                |row| row.get(0),
            )
            .optional()
            .expect("read raw inode size");
        if size == Some(want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "inode {ino} has size {size:?} in the database, want {want}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
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

#[test]
fn rename_while_open_keeps_the_handle_on_the_inode() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"old.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);
    let fh = fs.open(file.ino).expect("open");
    fs.write_fh(fh, 0, b"before").expect("write before rename");

    fs.rename(1, b"old.txt", 1, b"new.txt", 0).expect("rename");
    fs.write_fh(fh, 6, b"-after").expect("write after rename");
    fs.release_file(fh).expect("release");

    let attr = fs.lookup(1, b"new.txt").expect("lookup new name");
    assert_eq!(attr.ino, file.ino);
    assert_eq!(fs.read(file.ino, 0, 32).expect("read"), b"before-after");
}

#[test]
fn rename_over_an_open_file_keeps_it_until_its_last_release() {
    let path = disk_db_path("rename-over-open");
    let db = Db::open(&path).expect("open database");
    let victim = db
        .create_file(1, b"victim.txt", 0o644, 1000, 1000)
        .expect("create victim");
    db.create_file(1, b"source.txt", 0o644, 1000, 1000)
        .expect("create source");
    let fs = Dbfs::new(db);
    let fh = fs.open(victim.ino).expect("open victim");
    fs.write_fh(fh, 0, b"still readable").expect("write victim");

    fs.rename(1, b"source.txt", 1, b"victim.txt", 0)
        .expect("rename over the open file");

    assert_eq!(fs.getattr(victim.ino).expect("victim kept").nlink, 0);
    assert_eq!(
        fs.read_fh(fh, 0, 14).expect("read victim"),
        b"still readable"
    );
    fs.release_file(fh).expect("release victim");
    assert_eq!(fs.getattr(victim.ino), Err(libc::ENOENT));
    assert_eq!(
        count_chunk_rows(&path, victim.ino),
        0,
        "no chunk of the replaced file is left"
    );
    remove_db(&path);
}

#[test]
fn rename_over_a_dirty_closed_file_drops_its_buffer() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let victim = db
        .create_file(1, b"victim.txt", 0o644, 1000, 1000)
        .expect("create victim");
    db.create_file(1, b"source.txt", 0o644, 1000, 1000)
        .expect("create source");
    let fs = Dbfs::new(db);
    fs.write(victim.ino, 0, b"unflushed")
        .expect("buffer a write");

    fs.rename(1, b"source.txt", 1, b"victim.txt", 0)
        .expect("rename over the dirty file");

    assert_eq!(fs.getattr(victim.ino), Err(libc::ENOENT));
    assert_eq!(
        fs.flush_all(),
        Ok(()),
        "the dropped inode's buffer is not flushed into a missing inode"
    );
}

#[test]
fn truncate_through_setattr_updates_the_open_size() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"t.bin", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);
    let fh = fs.open(file.ino).expect("open");
    fs.write_fh(fh, 0, &[7u8; 100_000]).expect("write");

    fs.setattr(file.ino, None, None, None, Some(10), None, None)
        .expect("truncate");
    assert_eq!(fs.getattr(file.ino).expect("attr").size, 10);
    assert_eq!(fs.read_fh(fh, 0, 64).expect("read").len(), 10);

    fs.write_fh(fh, 20, b"x").expect("write past the new end");
    let data = fs.read_fh(fh, 0, 64).expect("read");
    assert_eq!(data.len(), 21);
    assert_eq!(&data[..10], &[7u8; 10]);
    assert!(data[10..20].iter().all(|&b| b == 0));
    fs.release_file(fh).expect("release");
    assert_eq!(fs.getattr(file.ino).expect("attr").size, 21);
}

#[test]
fn two_handles_share_one_inode_until_both_are_released() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"shared.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);
    let a = fs.open(file.ino).expect("open a");
    let b = fs.open(file.ino).expect("open b");
    assert_ne!(a, b);

    fs.write_fh(a, 0, b"from a").expect("write a");
    assert_eq!(fs.read_fh(b, 0, 6).expect("read b"), b"from a");
    fs.release_file(a).expect("release a");
    assert_eq!(fs.read_fh(b, 0, 6).expect("read b after a"), b"from a");
    assert_eq!(fs.read_fh(a, 0, 6), Err(libc::EBADF));
    fs.release_file(b).expect("release b");
    assert_eq!(fs.read(file.ino, 0, 6).expect("read stored"), b"from a");
}

/// Chunk rows stored for `ino` (keys are `(ino << 24) | chunk_index`).
fn count_chunk_rows(path: &Path, ino: u64) -> i64 {
    let conn = Connection::open(path).expect("open raw sqlite");
    conn.query_row(
        "SELECT COUNT(*) FROM file_chunks WHERE id BETWEEN ?1 << 24 AND (?1 << 24) | 16777215",
        params![ino as i64],
        |row| row.get(0),
    )
    .expect("count chunk rows")
}

#[test]
fn setattr_after_a_buffered_write_reports_and_keeps_both() {
    // `cp -a`: write, then utimensat, then close.
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"copy.txt", 0o600, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);
    let fh = fs.open(file.ino).expect("open");
    fs.write_fh(fh, 0, b"hello\n").expect("write");
    let mtime = UNIX_EPOCH + Duration::from_secs(1_000_000);

    let attr = fs
        .setattr(file.ino, None, None, None, None, None, Some(mtime))
        .expect("set mtime");
    assert_eq!(attr.size, 6, "setattr reports the buffered size");
    assert_eq!(attr.mtime, mtime);

    fs.release_file(fh).expect("release");
    let attr = fs.getattr(file.ino).expect("attr after close");
    assert_eq!(attr.size, 6);
    assert_eq!(
        attr.mtime, mtime,
        "closing does not overwrite the set mtime"
    );
    assert_eq!(fs.read(file.ino, 0, 16).expect("read"), b"hello\n");
}

#[test]
fn directory_sync_is_ok_on_an_in_memory_database() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let fs = Dbfs::new(db);

    assert_eq!(fs.sync_dir(), Ok(()), "there is no file to sync");
}

#[test]
fn directory_sync_makes_commits_durable_and_reports_a_failed_sync() {
    let path = disk_db_path("dir-sync");
    let db = Db::open(&path).expect("open file-backed database");
    db.create_dir(1, b"docs", 0o755, 1000, 1000)
        .expect("create directory");
    let fs = Dbfs::new(db);
    assert_eq!(fs.sync_dir(), Ok(()), "a file database syncs");

    // Hide the database and its WAL, so the durable sync can open neither file.
    let wal = path.with_extension("sqlite-wal");
    let hidden = path.with_extension("hidden");
    let hidden_wal = path.with_extension("hidden-wal");
    let had_wal = wal.exists();
    std::fs::rename(&path, &hidden).expect("hide database");
    if had_wal {
        std::fs::rename(&wal, &hidden_wal).expect("hide wal");
    }
    let hidden_result = fs.sync_dir();
    std::fs::rename(&hidden, &path).expect("restore database");
    if had_wal {
        std::fs::rename(&hidden_wal, &wal).expect("restore wal");
    }

    assert_eq!(hidden_result, Err(libc::EIO), "the failed sync is reported");
    assert_eq!(
        fs.sync_dir(),
        Ok(()),
        "syncing works once the files are back"
    );
    remove_db(&path);
}

#[test]
fn link_reports_the_buffered_size_and_a_ctime_after_the_write() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);
    let fh = fs.open(file.ino).expect("open");
    fs.write(file.ino, 0, b"0123456789").expect("buffer write");
    let before_link = fs.getattr(file.ino).expect("attr before link");

    let attr = fs.link(file.ino, 1, b"alias.txt").expect("link");

    assert_eq!(attr.size, 10, "the buffered size, not the stored 0");
    assert_eq!(attr.nlink, 2);
    assert!(
        attr.ctime > before_link.ctime,
        "the link is a change after the write"
    );
    assert_eq!(
        fs.getattr(file.ino).expect("attr after link").ctime,
        attr.ctime
    );
    fs.release_file(fh).expect("release");
}

#[test]
fn lookup_reports_the_buffered_size_of_an_open_file() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, b"notes.txt", 0o644, 1000, 1000)
        .expect("create file");
    let fs = Dbfs::new(db);
    let fh = fs.open(file.ino).expect("open");
    fs.write_fh(fh, 0, b"0123456789").expect("buffer write");

    let attr = fs.lookup(1, b"notes.txt").expect("lookup");

    assert_eq!(attr.size, 10, "the buffered size, not the stored 0");
    fs.release_file(fh).expect("release");
}
