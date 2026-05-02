use dbfs::{Db, DbError, FileKind};

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
