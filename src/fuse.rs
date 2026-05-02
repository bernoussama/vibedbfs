use std::time::{Duration, UNIX_EPOCH};

use fuser::{FileAttr, FileType};

use crate::{FileKind, Inode};

pub fn file_attr_from_inode(inode: &Inode) -> FileAttr {
    FileAttr {
        ino: inode.ino,
        size: inode.size,
        blocks: inode.size.div_ceil(512),
        atime: unix_time(inode.atime),
        mtime: unix_time(inode.mtime),
        ctime: unix_time(inode.ctime),
        crtime: UNIX_EPOCH,
        kind: match inode.kind {
            FileKind::RegularFile => FileType::RegularFile,
            FileKind::Directory => FileType::Directory,
        },
        perm: inode.mode as u16,
        nlink: inode.nlink,
        uid: inode.uid,
        gid: inode.gid,
        rdev: 0,
        blksize: 65_536,
        flags: 0,
    }
}

fn unix_time(secs: i64) -> std::time::SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}
