use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};

use crate::{Db, DbError, FileKind, Inode, MetadataUpdate};

const ATTR_TTL: Duration = Duration::from_secs(1);

pub struct Dbfs {
    db: Db,
    dirty_files: RefCell<HashMap<u64, DirtyFile>>,
}

#[derive(Debug, Clone)]
struct DirtyFile {
    size: u64,
    writes: Vec<(u64, Vec<u8>)>,
}

impl Dbfs {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            dirty_files: RefCell::new(HashMap::new()),
        }
    }

    pub fn getattr(&self, ino: u64) -> Result<FileAttr, i32> {
        let mut attr = self
            .db
            .get_inode(ino)
            .map(|inode| file_attr_from_inode(&inode))
            .map_err(errno_from_db_error)?;
        if let Some(dirty) = self.dirty_files.borrow().get(&ino) {
            attr.size = dirty.size;
            attr.blocks = dirty.size.div_ceil(512);
        }
        Ok(attr)
    }

    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<FileAttr, i32> {
        let mut attr = self
            .db
            .lookup(parent_ino, name)
            .map(|inode| file_attr_from_inode(&inode))
            .map_err(errno_from_db_error)?;
        if let Some(dirty) = self.dirty_files.borrow().get(&attr.ino) {
            attr.size = dirty.size;
            attr.blocks = dirty.size.div_ceil(512);
        }
        Ok(attr)
    }

    pub fn readdir(&self, ino: u64, offset: i64) -> Result<Vec<DbfsDirEntry>, i32> {
        let mut entries = Vec::new();
        let dot_count: i64 = 2;

        if offset < 1 {
            entries.push(DbfsDirEntry {
                name: b".".to_vec(),
                ino,
                kind: FileType::Directory,
            });
        }
        if offset < 2 {
            entries.push(DbfsDirEntry {
                name: b"..".to_vec(),
                ino: self.parent_ino(ino).unwrap_or(ino),
                kind: FileType::Directory,
            });
        }

        let db_offset = (offset - dot_count).max(0) as u64;
        entries.extend(
            self.db
                .list_dir_offset(ino, db_offset)
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

    pub fn symlink(
        &self,
        parent_ino: u64,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<FileAttr, i32> {
        self.db
            .create_symlink(parent_ino, name, target, uid, gid)
            .map(|inode| file_attr_from_inode(&inode))
            .map_err(errno_from_db_error)
    }

    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>, i32> {
        self.db.read_symlink(ino).map_err(errno_from_db_error)
    }

    pub fn link(&self, ino: u64, new_parent_ino: u64, new_name: &[u8]) -> Result<FileAttr, i32> {
        self.db
            .link(ino, new_parent_ino, new_name)
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
        self.flush_file(ino)?;
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

    pub fn open(&self, ino: u64) -> Result<u64, i32> {
        self.db
            .get_inode(ino)
            .map(|_| ino)
            .map_err(errno_from_db_error)
    }

    pub fn read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        let dirty_files = self.dirty_files.borrow();
        let Some(dirty) = dirty_files.get(&ino) else {
            return self
                .db
                .read_file(ino, offset, size)
                .map_err(errno_from_db_error);
        };

        self.db.get_inode(ino).map_err(errno_from_db_error)?;
        if offset >= dirty.size || size == 0 {
            return Ok(Vec::new());
        }

        let read_len = size.min((dirty.size - offset) as u32) as usize;
        let mut output = vec![0; read_len];
        let persisted = self
            .db
            .read_file(ino, offset, read_len as u32)
            .map_err(errno_from_db_error)?;
        output[..persisted.len()].copy_from_slice(&persisted);

        let read_start = offset;
        let read_end = offset + read_len as u64;
        for (write_offset, data) in &dirty.writes {
            let write_start = *write_offset;
            let write_end = write_start + data.len() as u64;
            let copy_start = read_start.max(write_start);
            let copy_end = read_end.min(write_end);
            if copy_start >= copy_end {
                continue;
            }

            let output_start = (copy_start - read_start) as usize;
            let data_start = (copy_start - write_start) as usize;
            let copy_len = (copy_end - copy_start) as usize;
            output[output_start..output_start + copy_len]
                .copy_from_slice(&data[data_start..data_start + copy_len]);
        }

        Ok(output)
    }

    pub fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
        if data.is_empty() {
            return Ok(0);
        }

        let kind = match self.db.inode_kind(ino) {
            Some(k) => k,
            None => self.db.get_inode(ino).map_err(errno_from_db_error)?.kind,
        };
        if kind != FileKind::RegularFile {
            return Err(libc::ENOENT);
        }

        let mut dirty_files = self.dirty_files.borrow_mut();
        let dirty = dirty_files.entry(ino).or_insert_with(|| {
            let size = self.db.get_inode(ino).map(|i| i.size).unwrap_or(0);
            DirtyFile {
                size,
                writes: Vec::new(),
            }
        });
        dirty.size = dirty.size.max(offset + data.len() as u64);
        dirty.writes.push((offset, data.to_vec()));

        Ok(data.len() as u32)
    }

    pub fn flush_file(&self, ino: u64) -> Result<(), i32> {
        let Some(dirty) = self.dirty_files.borrow_mut().remove(&ino) else {
            return Ok(());
        };

        match self.db.write_file_batch(ino, &dirty.writes) {
            Ok(_) => Ok(()),
            Err(error) => {
                self.dirty_files.borrow_mut().insert(ino, dirty);
                Err(errno_from_db_error(error))
            }
        }
    }

    pub fn release_file(&self, ino: u64) -> Result<(), i32> {
        self.flush_file(ino)?;
        self.db.release_file(ino);
        Ok(())
    }

    pub fn unlink(&self, parent_ino: u64, name: &[u8]) -> Result<(), i32> {
        let inode = self.db.lookup(parent_ino, name).map_err(errno_from_db_error)?;
        self.flush_file(inode.ino)?;
        self.db
            .unlink_inode(parent_ino, name, &inode)
            .map_err(errno_from_db_error)
    }

    pub fn rmdir(&self, parent_ino: u64, name: &[u8]) -> Result<(), i32> {
        self.db
            .remove_dir(parent_ino, name)
            .map_err(errno_from_db_error)
    }

    pub fn rename(
        &self,
        parent_ino: u64,
        name: &[u8],
        new_parent_ino: u64,
        new_name: &[u8],
    ) -> Result<(), i32> {
        if let Ok(inode) = self.db.lookup(parent_ino, name) {
            self.flush_file(inode.ino)?;
        }
        self.db
            .rename(parent_ino, name, new_parent_ino, new_name)
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
        match Dbfs::readdir(self, ino, offset) {
            Ok(entries) => {
                for (i, entry) in entries.into_iter().enumerate() {
                    let next_offset = offset + (i as i64) + 1;
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
        _flags: i32,
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
            // FIX: Pass 0 for FOPEN flags (not syscall open flags).
            // The kernel validates these against known FOPEN_* constants
            // and rejects the reply with EIO for invalid values.
            Ok(attr) => {
                self.db.open_file(attr.ino, FileKind::RegularFile);
                reply.created(&ATTR_TTL, &attr, 0, attr.ino, 0);
            }
            Err(errno) => reply.error(errno),
        }
    }

    fn symlink(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        match Dbfs::symlink(
            self,
            parent,
            link_name.as_bytes(),
            target.as_os_str().as_bytes(),
            req.uid(),
            req.gid(),
        ) {
            Ok(attr) => reply.entry(&ATTR_TTL, &attr, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        match Dbfs::readlink(self, ino) {
            Ok(target) => reply.data(&target),
            Err(errno) => reply.error(errno),
        }
    }

    fn link(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        match Dbfs::link(self, ino, newparent, newname.as_bytes()) {
            Ok(attr) => reply.entry(&ATTR_TTL, &attr, 0),
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

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.db.get_inode(ino) {
            Ok(inode) => {
                self.db.open_file(ino, inode.kind);
                reply.opened(ino, 0);
            }
            Err(e) => reply.error(errno_from_db_error(e)),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.release_file(ino) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        match self.flush_file(ino) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.flush_file(ino) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }

        match Dbfs::read(self, ino, offset as u64, size) {
            Ok(data) => reply.data(&data),
            Err(errno) => reply.error(errno),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }

        match Dbfs::write(self, ino, offset as u64, data) {
            Ok(written) => reply.written(written),
            Err(errno) => reply.error(errno),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match Dbfs::unlink(self, parent, name.as_bytes()) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match Dbfs::rmdir(self, parent, name.as_bytes()) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn mknod(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        let file_type = mode & libc::S_IFMT as u32;
        const S_IFREG: u32 = libc::S_IFREG as u32;
        match file_type {
            // Regular file: delegate to create
            0 | S_IFREG => {
                match Dbfs::create(
                    self,
                    parent,
                    name.as_bytes(),
                    mode & !(libc::S_IFMT as u32),
                    umask,
                    req.uid(),
                    req.gid(),
                ) {
                    Ok(attr) => reply.entry(&ATTR_TTL, &attr, 0),
                    Err(errno) => reply.error(errno),
                }
            }
            // FIFOs, devices, sockets — not supported
            _ => reply.error(libc::EPERM),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        if flags != 0 {
            reply.error(libc::EINVAL);
            return;
        }

        match Dbfs::rename(self, parent, name.as_bytes(), newparent, newname.as_bytes()) {
            Ok(()) => reply.ok(),
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
            FileKind::Symlink => FileType::Symlink,
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
