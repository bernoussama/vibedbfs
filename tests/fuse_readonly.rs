use dbfs::{Db, Dbfs, DbfsDirEntry, FileKind};
use fuser::FileType;

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

    let entries = fs.readdir(1).expect("read root directory");

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
