use std::time::{Duration, UNIX_EPOCH};

use dbfs::{FileKind, Inode, file_attr_from_inode};
use fuser::FileType;

#[test]
fn maps_regular_file_inode_to_fuse_attr() {
    let inode = Inode {
        ino: 42,
        kind: FileKind::RegularFile,
        mode: 0o640,
        uid: 1000,
        gid: 1001,
        size: 12345,
        atime: 10,
        mtime: 20,
        ctime: 30,
        nlink: 1,
    };

    let attr = file_attr_from_inode(&inode);

    assert_eq!(attr.ino, 42);
    assert_eq!(attr.kind, FileType::RegularFile);
    assert_eq!(attr.perm, 0o640);
    assert_eq!(attr.uid, 1000);
    assert_eq!(attr.gid, 1001);
    assert_eq!(attr.size, 12345);
    assert_eq!(attr.nlink, 1);
    assert_eq!(attr.atime, UNIX_EPOCH + Duration::from_secs(10));
    assert_eq!(attr.mtime, UNIX_EPOCH + Duration::from_secs(20));
    assert_eq!(attr.ctime, UNIX_EPOCH + Duration::from_secs(30));
}

#[test]
fn maps_directory_inode_to_fuse_attr() {
    let inode = Inode {
        ino: 1,
        kind: FileKind::Directory,
        mode: 0o755,
        uid: 0,
        gid: 0,
        size: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        nlink: 1,
    };

    let attr = file_attr_from_inode(&inode);

    assert_eq!(attr.kind, FileType::Directory);
    assert_eq!(attr.perm, 0o755);
    assert_eq!(attr.blksize, 65_536);
}
