use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyDirectory, ReplyEntry,
    ReplyStatfs, Request, TimeOrNow,
};

use crate::{Db, DbError, FileKind, Inode, MetadataUpdate};

const ATTR_TTL: Duration = Duration::from_secs(1);

pub struct Dbfs {
    db: Db,
}

impl Dbfs {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn getattr(&self, ino: u64) -> Result<FileAttr, i32> {
        self.db
            .get_inode(ino)
            .map(|inode| file_attr_from_inode(&inode))
            .map_err(errno_from_db_error)
    }

    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<FileAttr, i32> {
        self.db
            .lookup(parent_ino, name)
            .map(|inode| file_attr_from_inode(&inode))
            .map_err(errno_from_db_error)
    }

    pub fn readdir(&self, ino: u64) -> Result<Vec<DbfsDirEntry>, i32> {
        let mut entries = vec![
            DbfsDirEntry {
                name: b".".to_vec(),
                ino,
                kind: FileType::Directory,
            },
            DbfsDirEntry {
                name: b"..".to_vec(),
                ino: self.parent_ino(ino).unwrap_or(ino),
                kind: FileType::Directory,
            },
        ];

        entries.extend(
            self.db
                .list_dir(ino)
                .map_err(errno_from_db_error)?
                .into_iter()
                .map(|entry| DbfsDirEntry {
                    name: entry.name,
                    ino: entry.ino,
                    kind: DbfsDirEntry::file_type(entry.kind),
                }),
        );

        Ok(entries)
    }

    pub fn mkdir(
        &self,
        parent_ino: u64,
        name: &[u8],
        mode: u32,
        umask: u32,
        uid: u32,
        gid: u32,
    ) -> Result<FileAttr, i32> {
        self.db
            .create_dir(parent_ino, name, mode & !umask, uid, gid)
            .map(|inode| file_attr_from_inode(&inode))
            .map_err(errno_from_db_error)
    }

    pub fn create(
        &self,
        parent_ino: u64,
        name: &[u8],
        mode: u32,
        umask: u32,
        uid: u32,
        gid: u32,
    ) -> Result<FileAttr, i32> {
        self.db
            .create_file(parent_ino, name, mode & !umask, uid, gid)
            .map(|inode| file_attr_from_inode(&inode))
            .map_err(errno_from_db_error)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn setattr(
        &self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<i64>,
        mtime: Option<i64>,
    ) -> Result<FileAttr, i32> {
        if let Some(size) = size {
            self.db
                .truncate_file(ino, size)
                .map_err(errno_from_db_error)?;
        }

        self.db
            .update_metadata(
                ino,
                MetadataUpdate {
                    mode,
                    uid,
                    gid,
                    atime,
                    mtime,
                },
            )
            .map(|inode| file_attr_from_inode(&inode))
            .map_err(errno_from_db_error)
    }

    fn parent_ino(&self, ino: u64) -> Option<u64> {
        if ino == 1 { Some(1) } else { None }
    }
}

impl Filesystem for Dbfs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match Dbfs::lookup(self, parent, name.as_bytes()) {
            Ok(attr) => reply.entry(&ATTR_TTL, &attr, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match Dbfs::getattr(self, ino) {
            Ok(attr) => reply.attr(&ATTR_TTL, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        match Dbfs::readdir(self, ino) {
            Ok(entries) => {
                for (index, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                    let next_offset = (index + 1) as i64;
                    if reply.add(
                        entry.ino,
                        next_offset,
                        entry.kind,
                        OsStr::from_bytes(&entry.name),
                    ) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(errno) => reply.error(errno),
        }
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, 512, 255, 65_536);
    }

    fn mkdir(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        match Dbfs::mkdir(
            self,
            parent,
            name.as_bytes(),
            mode,
            umask,
            req.uid(),
            req.gid(),
        ) {
            Ok(attr) => reply.entry(&ATTR_TTL, &attr, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn create(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        match Dbfs::create(
            self,
            parent,
            name.as_bytes(),
            mode,
            umask,
            req.uid(),
            req.gid(),
        ) {
            Ok(attr) => reply.created(&ATTR_TTL, &attr, 0, attr.ino, flags as u32),
            Err(errno) => reply.error(errno),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        match Dbfs::setattr(
            self,
            ino,
            mode,
            uid,
            gid,
            size,
            time_or_now_secs(atime),
            time_or_now_secs(mtime),
        ) {
            Ok(attr) => reply.attr(&ATTR_TTL, &attr),
            Err(errno) => reply.error(errno),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbfsDirEntry {
    pub name: Vec<u8>,
    pub ino: u64,
    pub kind: FileType,
}

impl DbfsDirEntry {
    pub fn file_type(kind: FileKind) -> FileType {
        match kind {
            FileKind::RegularFile => FileType::RegularFile,
            FileKind::Directory => FileType::Directory,
        }
    }
}

pub fn file_attr_from_inode(inode: &Inode) -> FileAttr {
    FileAttr {
        ino: inode.ino,
        size: inode.size,
        blocks: inode.size.div_ceil(512),
        atime: unix_time(inode.atime),
        mtime: unix_time(inode.mtime),
        ctime: unix_time(inode.ctime),
        crtime: UNIX_EPOCH,
        kind: DbfsDirEntry::file_type(inode.kind),
        perm: inode.mode as u16,
        nlink: inode.nlink,
        uid: inode.uid,
        gid: inode.gid,
        rdev: 0,
        blksize: 65_536,
        flags: 0,
    }
}

fn errno_from_db_error(error: DbError) -> i32 {
    match error {
        DbError::NotFound => libc::ENOENT,
        DbError::AlreadyExists => libc::EEXIST,
        DbError::DirectoryNotEmpty => libc::ENOTEMPTY,
        DbError::InvalidInput => libc::EINVAL,
        DbError::IsDirectory => libc::EISDIR,
        DbError::NotDirectory => libc::ENOTDIR,
        DbError::Corrupt | DbError::Sqlite(_) => libc::EIO,
    }
}

fn unix_time(secs: i64) -> std::time::SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

fn time_or_now_secs(time: Option<TimeOrNow>) -> Option<i64> {
    match time? {
        TimeOrNow::SpecificTime(time) => system_time_secs(time),
        TimeOrNow::Now => system_time_secs(SystemTime::now()),
    }
}

fn system_time_secs(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => Some(duration.as_secs() as i64),
        Err(error) => Some(-(error.duration().as_secs() as i64)),
    }
}
