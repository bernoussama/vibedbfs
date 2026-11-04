use std::ffi::OsStr;
use std::ops::Deref;
use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow,
};

use crate::db::{CHUNK_SIZE, ChunkSpan, MAX_FILE_SIZE};
use crate::open_table::{OpenTable, Slot};
use crate::{Db, DbError, FileKind, Inode, MetadataUpdate, Timestamp};

const ATTR_TTL: Duration = Duration::from_secs(1);

/// Default budget for buffered writes, shared by all open files. When it is used
/// up, the file with the most dirty chunks is flushed.
pub const DEFAULT_WRITE_BUFFER: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsStats {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
}

/// The filesystem as the FUSE session sees it. Its state lives in [`Shared`] behind an
/// `Arc`, so the shutdown path can reach the same dirty buffers from another thread.
pub struct Dbfs {
    shared: Arc<Shared>,
}

/// State shared by the FUSE session and the shutdown path. `Dbfs` derefs to it. The
/// `fuse` module is private, so this type is not reachable outside the crate.
///
/// Lock order: `table`, then the database connection (taken inside `Db` methods).
/// Nothing takes `table` while holding the connection.
pub struct Shared {
    db: Db,
    /// Open handles, open inodes and their dirty data. One lock per operation.
    table: Mutex<OpenTable>,
    /// Outcome of the latest flush on a shutdown path (`destroy` or a signal).
    last_flush: Mutex<Result<(), i32>>,
}

impl Deref for Dbfs {
    type Target = Shared;

    fn deref(&self) -> &Shared {
        &self.shared
    }
}

/// Whether a flush also persists an inode that was unlinked while open. Its data
/// dies with the last handle, so closing never needs to write it; only the memory
/// budget and `ftruncate` do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Unlinked {
    Skip,
    Persist,
}

impl Shared {
    fn table(&self) -> MutexGuard<'_, OpenTable> {
        self.table.lock().unwrap()
    }

    /// Writes the dirty chunks of `slot` to the database. On failure they stay
    /// buffered, so a later flush can retry.
    fn flush_slot(&self, t: &mut OpenTable, slot: Slot, unlinked: Unlinked) -> Result<(), i32> {
        let i = slot as usize;
        let OpenTable { inodes, arena, .. } = t;
        if inodes.dirty[i].is_empty() || (inodes.unlinked[i] && unlinked == Unlinked::Skip) {
            return Ok(());
        }
        let chunks = inodes.dirty[i]
            .entries()
            .iter()
            .map(|&(chunk_index, a)| (chunk_index, arena.chunk(a)));
        self.db
            .replace_chunks(inodes.ino[i], inodes.size[i], chunks, inodes.mtime[i])
            .map_err(errno_from_db_error)?;
        inodes.dirty[i].release_into(arena);
        Ok(())
    }

    /// Frees `slot` if nothing needs it any more: no handles and no dirty data.
    fn free_if_idle(t: &mut OpenTable, slot: Slot) {
        let i = slot as usize;
        if t.inodes.refs[i] == 0 && t.inodes.dirty[i].is_empty() {
            t.inodes.remove(slot);
        }
    }

    /// Drops `slot` and its dirty data without writing anything: its inode is gone.
    fn discard_slot(t: &mut OpenTable, slot: Slot) {
        t.inodes.dirty[slot as usize].release_into(&mut t.arena);
        t.inodes.remove(slot);
    }

    /// Writes every dirty inode to the database and then makes the commit durable.
    /// A failing inode stays dirty and the loop moves on, so one bad inode does not
    /// hold back the rest. Returns the first error. Inodes unlinked while open are
    /// skipped: the next start deletes them anyway.
    fn flush_all(&self) -> Result<(), i32> {
        let mut first_error = None;
        let mut t = self.table();
        for slot in t.inodes.live_slots() {
            match self.flush_slot(&mut t, slot, Unlinked::Skip) {
                Ok(()) => Self::free_if_idle(&mut t, slot),
                Err(errno) => {
                    first_error.get_or_insert(errno);
                }
            }
        }
        drop(t);
        let synced = self.db.sync_durable().map_err(errno_from_db_error);
        match first_error {
            Some(errno) => Err(errno),
            None => synced,
        }
    }

    fn refuse_writes(&self) {
        // Holding the lock means no write is half-way through when the flag changes.
        self.table().closing = true;
    }

    fn record_flush(&self, result: Result<(), i32>) {
        *self.last_flush.lock().unwrap() = result;
    }

    /// The slot of `ino`, loading kind and size from the database if it has none.
    fn slot_for(&self, t: &mut OpenTable, ino: u64) -> Result<Slot, i32> {
        if let Some(slot) = t.inodes.find(ino) {
            return Ok(slot);
        }
        let (kind, size) = self.db.inode_head(ino).map_err(errno_from_db_error)?;
        Ok(t.inodes.insert(ino, kind, size))
    }

    /// Adds a handle to `slot` and returns it as the FUSE fh.
    fn open_slot(t: &mut OpenTable, slot: Slot) -> u64 {
        t.inodes.refs[slot as usize] += 1;
        t.handles.alloc(slot)
    }

    fn read_slot(&self, t: &OpenTable, slot: Slot, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        let i = slot as usize;
        let inodes = &t.inodes;
        if let Some(error) = inodes.kind[i].regular_file_error() {
            return Err(errno_from_db_error(error));
        }
        let file_size = inodes.size[i];
        if offset >= file_size || size == 0 {
            return Ok(Vec::new());
        }
        let mut output = vec![0; (size as u64).min(file_size - offset) as usize];
        self.db
            .read_into(inodes.ino[i], offset, &mut output)
            .map_err(errno_from_db_error)?;
        let span = ChunkSpan::new(offset, output.len()).expect("non-empty read");
        for &(chunk_index, a) in inodes.dirty[i].range(span.first, span.last) {
            span.copy_out(chunk_index, t.arena.chunk(a), &mut output);
        }
        Ok(output)
    }

    /// An empty arena slot. When the write budget is used up, the inode with the
    /// most dirty chunks (possibly `writing`, the one being written) is flushed
    /// first.
    fn alloc_chunk(&self, t: &mut OpenTable, writing: Slot) -> Result<u32, i32> {
        loop {
            if let Some(a) = t.arena.alloc() {
                return Ok(a);
            }
            let victim = t.inodes.largest_dirty().ok_or(libc::EIO)?;
            self.flush_slot(t, victim, Unlinked::Persist)?;
            if victim != writing {
                Self::free_if_idle(t, victim);
            }
        }
    }

    fn write_slot(&self, t: &mut OpenTable, slot: Slot, offset: u64, data: &[u8]) -> Result<u32, i32> {
        let i = slot as usize;
        if let Some(error) = t.inodes.kind[i].regular_file_error() {
            return Err(errno_from_db_error(error));
        }
        if t.closing {
            // Shutdown has already flushed the buffers. Accepting this data now would
            // acknowledge a write that nothing will persist.
            return Err(libc::EIO);
        }
        let Some(span) = ChunkSpan::new(offset, data.len()) else {
            return Ok(0);
        };
        if span.end > MAX_FILE_SIZE {
            return Err(libc::EFBIG);
        }
        t.inodes.mtime[i] = Timestamp::from_system_time(SystemTime::now());

        let ino = t.inodes.ino[i];
        for chunk_index in span.first..=span.last {
            let piece = span.piece(chunk_index);
            let a = match t.inodes.dirty[i].find(chunk_index) {
                Ok(position) => t.inodes.dirty[i].slot_at(position),
                Err(_) => {
                    // May flush this inode too, so look the position up again after.
                    let a = self.alloc_chunk(t, slot)?;
                    if !piece.is_full_chunk() {
                        let buffer = t.arena.buffer_mut(a);
                        match self.db.read_chunk_into(ino, chunk_index, buffer) {
                            Ok(len) => t.arena.set_len(a, len),
                            Err(error) => {
                                t.arena.release(a);
                                return Err(errno_from_db_error(error));
                            }
                        }
                    }
                    let position = t.inodes.dirty[i].find(chunk_index).unwrap_err();
                    t.inodes.dirty[i].insert(position, chunk_index, a);
                    a
                }
            };
            t.arena.write(a, piece.chunk.clone(), &data[piece.buf]);
            // Grow the size chunk by chunk, so an eviction in the middle of this
            // write stores a size that covers every chunk it stores.
            let chunk_end = chunk_index * CHUNK_SIZE as u64 + piece.chunk.end as u64;
            t.inodes.size[i] = t.inodes.size[i].max(chunk_end);
        }
        Ok(data.len() as u32)
    }

    /// Drops one handle. The last handle of an inode unlinked while open deletes
    /// it; otherwise the dirty data is flushed and an idle slot is freed. A failed
    /// flush keeps the data for a later retry and reports the error.
    fn release_fh(&self, fh: u64) -> Result<(), i32> {
        let mut t = self.table();
        let slot = t.handles.release(fh).ok_or(libc::EBADF)?;
        let i = slot as usize;
        t.inodes.refs[i] -= 1;
        if t.inodes.refs[i] == 0 && t.inodes.unlinked[i] {
            let ino = t.inodes.ino[i];
            Self::discard_slot(&mut t, slot);
            return self.db.delete_inode(ino).map_err(errno_from_db_error);
        }
        self.flush_slot(&mut t, slot, Unlinked::Skip)?;
        Self::free_if_idle(&mut t, slot);
        Ok(())
    }

    /// After the entry for `target` was removed: forget an inode the database
    /// deleted, or mark one that stays because it is still open.
    fn entry_removed(t: &mut OpenTable, target: &Inode) {
        if target.kind == FileKind::Directory || target.nlink > 1 {
            return;
        }
        let Some(slot) = t.inodes.find(target.ino) else {
            return;
        };
        if t.inodes.is_open(slot) {
            t.inodes.unlinked[slot as usize] = true;
        } else {
            Self::discard_slot(t, slot);
        }
    }

    fn is_open(t: &OpenTable, ino: u64) -> bool {
        t.inodes.find(ino).is_some_and(|slot| t.inodes.is_open(slot))
    }

    /// Size and mtime overlay for an inode with buffered writes.
    fn overlay_attr(&self, attr: &mut FileAttr) {
        let t = self.table();
        let Some(slot) = t.inodes.find(attr.ino) else {
            return;
        };
        let i = slot as usize;
        attr.size = t.inodes.size[i];
        attr.blocks = attr.size.div_ceil(512);
        if !t.inodes.dirty[i].is_empty() {
            let modified = t.inodes.mtime[i].to_system_time();
            attr.mtime = modified;
            attr.ctime = modified;
        }
    }
}

/// Lets `run.rs` flush and read back the outcome while the FUSE session owns the `Dbfs`.
pub(crate) struct ShutdownHandle {
    shared: Arc<Shared>,
}

impl ShutdownHandle {
    /// Stops accepting buffered writes, then flushes every dirty inode. The outcome
    /// is recorded, so `flush_failure` reports it later.
    pub(crate) fn flush_for_shutdown(&self) -> Result<(), i32> {
        self.shared.refuse_writes();
        let result = self.shared.flush_all();
        self.shared.record_flush(result);
        result
    }

    /// The errno from the latest shutdown flush, or `None` if that flush succeeded.
    pub(crate) fn flush_failure(&self) -> Option<i32> {
        self.shared.last_flush.lock().unwrap().err()
    }
}

impl Dbfs {
    pub fn new(db: Db) -> Self {
        Self::with_write_buffer(db, DEFAULT_WRITE_BUFFER)
    }

    /// A filesystem whose buffered writes use at most `write_buffer` bytes
    /// (at least one 64 KiB chunk). The memory is reserved as it is used.
    pub fn with_write_buffer(db: Db, write_buffer: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                db,
                table: Mutex::new(OpenTable::new(write_buffer)),
                last_flush: Mutex::new(Ok(())),
            }),
        }
    }

    /// Writes every dirty buffer to the database and makes it durable. Keeps going
    /// after a failure and returns the first error; a failed buffer stays dirty.
    pub fn flush_all(&self) -> Result<(), i32> {
        self.shared.flush_all()
    }

    /// A handle for the shutdown path in `run.rs`. It keeps working after this
    /// `Dbfs` moves into the FUSE session.
    pub(crate) fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    pub fn statfs(&self) -> Result<FsStats, i32> {
        let usage = self.db.volume_usage().map_err(errno_from_db_error)?;
        let bsize = usage.page_size as u32;
        Ok(FsStats {
            blocks: usage.page_count,
            bfree: usage.freelist_count,
            bavail: usage.freelist_count,
            files: usage.files,
            ffree: 0,
            bsize,
            namelen: 255,
            frsize: bsize,
        })
    }

    pub fn getattr(&self, ino: u64) -> Result<FileAttr, i32> {
        let mut attr = self
            .db
            .get_inode(ino)
            .map(|inode| file_attr_from_inode(&inode))
            .map_err(errno_from_db_error)?;
        self.overlay_attr(&mut attr);
        Ok(attr)
    }

    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<FileAttr, i32> {
        let mut attr = self
            .db
            .lookup(parent_ino, name)
            .map(|inode| file_attr_from_inode(&inode))
            .map_err(errno_from_db_error)?;
        self.overlay_attr(&mut attr);
        Ok(attr)
    }

    /// Streams directory entries starting after FUSE offset `offset` to
    /// `emit(ino, next_offset, kind, name)`, stopping when it returns true
    /// (the reply buffer is full).
    ///
    /// Offsets are stable cookies, not positions: `.` is 1, `..` is 2, and an
    /// entry with dirent id `n` is `n + 2`. Removing entries between calls (as
    /// `rm -rf` does) therefore never skips or repeats the remaining entries.
    pub fn readdir_stream(
        &self,
        ino: u64,
        offset: i64,
        mut emit: impl FnMut(u64, i64, FileType, &[u8]) -> bool,
    ) -> Result<(), i32> {
        const DOT: i64 = 1;
        const DOTDOT: i64 = 2;
        let parent = match self.db.parent_of_dir(ino) {
            Ok(parent) => parent,
            Err(DbError::NotFound) => ino,
            Err(error) => return Err(errno_from_db_error(error)),
        };
        if offset < DOT && emit(ino, DOT, FileType::Directory, b".") {
            return Ok(());
        }
        if offset < DOTDOT && emit(parent, DOTDOT, FileType::Directory, b"..") {
            return Ok(());
        }
        self.db
            .read_dir_after(ino, (offset - DOTDOT).max(0), |id, name, child, kind| {
                emit(child, id + DOTDOT, DbfsDirEntry::file_type(kind), name)
            })
            .map_err(errno_from_db_error)
    }

    /// Collects every entry after `offset`. See `readdir_stream`.
    pub fn readdir(&self, ino: u64, offset: i64) -> Result<Vec<DbfsDirEntry>, i32> {
        let mut entries = Vec::new();
        self.readdir_stream(ino, offset, |ino, next_offset, kind, name| {
            entries.push(DbfsDirEntry {
                name: name.to_vec(),
                ino,
                kind,
                offset: next_offset,
            });
            false
        })?;
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
        atime: Option<SystemTime>,
        mtime: Option<SystemTime>,
    ) -> Result<FileAttr, i32> {
        // Persist buffered writes first: the returned attributes must include
        // them, and a later flush must not overwrite the mtime set here (`cp -a`
        // writes, then calls utimensat, then closes).
        let mut t = self.table();
        if let Some(slot) = t.inodes.find(ino) {
            self.flush_slot(&mut t, slot, Unlinked::Persist)?;
            Shared::free_if_idle(&mut t, slot);
        }
        if let Some(size) = size {
            self.truncate_locked(&mut t, ino, size)?;
        }

        self.db
            .update_metadata(
                ino,
                MetadataUpdate {
                    mode,
                    uid,
                    gid,
                    atime: atime.map(Timestamp::from_system_time),
                    mtime: mtime.map(Timestamp::from_system_time),
                },
            )
            .map(|inode| file_attr_from_inode(&inode))
            .map_err(errno_from_db_error)
    }

    /// Extended attributes are not stored, so no name exists on any inode. An
    /// existing inode answers `ENODATA`, which tools read as "no such attribute".
    pub fn getxattr(&self, ino: u64, _name: &[u8]) -> Result<Vec<u8>, i32> {
        self.getattr(ino)?;
        Err(libc::ENODATA)
    }

    /// The name list is always empty: there are no extended attributes to list.
    pub fn listxattr(&self, ino: u64) -> Result<Vec<u8>, i32> {
        self.getattr(ino)?;
        Ok(Vec::new())
    }

    /// Attribute storage is not implemented, so setting one is refused.
    pub fn setxattr(&self, ino: u64, _name: &[u8]) -> Result<(), i32> {
        self.getattr(ino)?;
        Err(libc::EOPNOTSUPP)
    }

    /// Nothing can be removed, because nothing is stored: `ENODATA`.
    pub fn removexattr(&self, ino: u64, _name: &[u8]) -> Result<(), i32> {
        self.getattr(ino)?;
        Err(libc::ENODATA)
    }

    /// Opens `ino` and returns a new file handle for it.
    pub fn open(&self, ino: u64) -> Result<u64, i32> {
        let mut t = self.table();
        let slot = self.slot_for(&mut t, ino)?;
        Ok(Shared::open_slot(&mut t, slot))
    }

    /// Reads through the open-inode table if `ino` has a slot, else from the database.
    pub fn read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        let t = self.table();
        match t.inodes.find(ino) {
            Some(slot) => self.read_slot(&t, slot, offset, size),
            None => {
                drop(t);
                self.db
                    .read_file(ino, offset, size)
                    .map_err(errno_from_db_error)
            }
        }
    }

    /// Reads through file handle `fh`: no hashing and no inode query.
    pub fn read_fh(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        let t = self.table();
        let slot = t.handles.get(fh).ok_or(libc::EBADF)?;
        self.read_slot(&t, slot, offset, size)
    }

    /// Buffers a write to `ino`, whether or not it is open.
    pub fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
        if data.is_empty() {
            return Ok(0);
        }
        let mut t = self.table();
        let slot = self.slot_for(&mut t, ino)?;
        let result = self.write_slot(&mut t, slot, offset, data);
        Shared::free_if_idle(&mut t, slot);
        result
    }

    /// Buffers a write through file handle `fh`.
    pub fn write_fh(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
        let mut t = self.table();
        let slot = t.handles.get(fh).ok_or(libc::EBADF)?;
        self.write_slot(&mut t, slot, offset, data)
    }

    /// `fallocate` for regular files. Checks run in the order of Linux's
    /// `vfs_fallocate`. Holes already read as zeros, so mode 0 only extends
    /// `size`. The file never shrinks, and no space is reserved: SQLite
    /// allocates pages on write.
    pub fn fallocate(&self, ino: u64, offset: i64, length: i64, mode: i32) -> Result<(), i32> {
        if offset < 0 || length <= 0 {
            return Err(libc::EINVAL);
        }
        if mode & !libc::FALLOC_FL_KEEP_SIZE != 0 {
            return Err(libc::EOPNOTSUPP);
        }
        if self.db.inode_kind_of(ino).map_err(errno_from_db_error)? != FileKind::RegularFile {
            return Err(libc::ENODEV);
        }
        // The end must fit in the i64 size that SQLite stores.
        let end = offset.checked_add(length).ok_or(libc::EFBIG)? as u64;
        if end > MAX_FILE_SIZE {
            return Err(libc::EFBIG);
        }
        if mode & libc::FALLOC_FL_KEEP_SIZE != 0 {
            return Ok(());
        }

        let mut t = self.table();
        let size = match t.inodes.find(ino) {
            Some(slot) => t.inodes.size[slot as usize],
            None => self.db.inode_head(ino).map_err(errno_from_db_error)?.1,
        };
        if end <= size {
            return Ok(());
        }
        self.truncate_locked(&mut t, ino, end)
    }

    fn truncate_locked(&self, t: &mut OpenTable, ino: u64, size: u64) -> Result<(), i32> {
        let slot = t.inodes.find(ino);
        if let Some(slot) = slot {
            self.flush_slot(t, slot, Unlinked::Persist)?;
        }
        self.db
            .truncate_file(ino, size)
            .map_err(errno_from_db_error)?;
        if let Some(slot) = slot {
            t.inodes.size[slot as usize] = size;
            Shared::free_if_idle(t, slot);
        }
        Ok(())
    }

    /// Writes the buffered data of `ino` to the database (not yet durable).
    pub fn flush_file(&self, ino: u64) -> Result<(), i32> {
        let mut t = self.table();
        let Some(slot) = t.inodes.find(ino) else {
            return Ok(());
        };
        self.flush_slot(&mut t, slot, Unlinked::Skip)?;
        Shared::free_if_idle(&mut t, slot);
        Ok(())
    }

    fn flush_fh(&self, fh: u64) -> Result<(), i32> {
        let mut t = self.table();
        let slot = t.handles.get(fh).ok_or(libc::EBADF)?;
        self.flush_slot(&mut t, slot, Unlinked::Skip)
    }

    fn sync_fh(&self, fh: u64) -> Result<(), i32> {
        self.flush_fh(fh)?;
        self.db.sync_durable().map_err(errno_from_db_error)
    }

    /// Releases file handle `fh` (from `open` or `create`).
    pub fn release_file(&self, fh: u64) -> Result<(), i32> {
        self.release_fh(fh)
    }

    pub fn unlink(&self, parent_ino: u64, name: &[u8]) -> Result<(), i32> {
        let inode = self
            .db
            .lookup(parent_ino, name)
            .map_err(errno_from_db_error)?;
        let mut t = self.table();
        self.db
            .unlink_entry(parent_ino, name, &inode, Shared::is_open(&t, inode.ino))
            .map_err(errno_from_db_error)?;
        Shared::entry_removed(&mut t, &inode);
        Ok(())
    }

    pub fn rmdir(&self, parent_ino: u64, name: &[u8]) -> Result<(), i32> {
        self.db
            .remove_dir(parent_ino, name)
            .map_err(errno_from_db_error)
    }

    /// Renames an entry. Open handles follow the inode, not the name, so nothing
    /// is flushed; a replaced inode is handled like an unlink.
    pub fn rename(
        &self,
        parent_ino: u64,
        name: &[u8],
        new_parent_ino: u64,
        new_name: &[u8],
        flags: u32,
    ) -> Result<(), i32> {
        let noreplace = libc::RENAME_NOREPLACE;
        if flags & !noreplace != 0 {
            return Err(libc::EINVAL);
        }
        let mut t = self.table();
        let replaced = self
            .db
            .rename_entry(
                parent_ino,
                name,
                new_parent_ino,
                new_name,
                flags & noreplace != 0,
                |ino| Shared::is_open(&t, ino),
            )
            .map_err(errno_from_db_error)?;
        if let Some(target) = replaced {
            Shared::entry_removed(&mut t, &target);
        }
        Ok(())
    }

    /// Creates a regular file and opens it, returning its attributes and handle.
    #[allow(clippy::too_many_arguments)]
    pub fn create_open(
        &self,
        parent_ino: u64,
        name: &[u8],
        mode: u32,
        umask: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(FileAttr, u64), i32> {
        let attr = self.create(parent_ino, name, mode, umask, uid, gid)?;
        let mut t = self.table();
        let slot = t.inodes.insert(attr.ino, FileKind::RegularFile, 0);
        Ok((attr, Shared::open_slot(&mut t, slot)))
    }
}

impl Filesystem for Dbfs {
    fn destroy(&mut self) {
        // Runs on unmount, or when the session is dropped. The outcome goes to
        // `ShutdownHandle::flush_failure`, which `run` checks before exiting.
        let result = self.flush_all();
        self.shared.record_flush(result);
    }

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
        let result = self.readdir_stream(ino, offset, |ino, next_offset, kind, name| {
            reply.add(ino, next_offset, kind, OsStr::from_bytes(name))
        });
        match result {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        match Dbfs::statfs(self) {
            Ok(stat) => reply.statfs(
                stat.blocks,
                stat.bfree,
                stat.bavail,
                stat.files,
                stat.ffree,
                stat.bsize,
                stat.namelen,
                stat.frsize,
            ),
            Err(errno) => reply.error(errno),
        }
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
        match self.create_open(parent, name.as_bytes(), mode, umask, req.uid(), req.gid()) {
            // Pass 0 for FOPEN flags (not syscall open flags). The kernel validates
            // these against known FOPEN_* constants and rejects other values with EIO.
            Ok((attr, fh)) => reply.created(&ATTR_TTL, &attr, 0, fh, 0),
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
            time_or_now(atime),
            time_or_now(mtime),
        ) {
            Ok(attr) => reply.attr(&ATTR_TTL, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn getxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        match Dbfs::getxattr(self, ino, name.as_bytes()) {
            Ok(value) => reply_xattr(&value, size, reply),
            Err(errno) => reply.error(errno),
        }
    }

    fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: ReplyXattr) {
        match Dbfs::listxattr(self, ino) {
            Ok(names) => reply_xattr(&names, size, reply),
            Err(errno) => reply.error(errno),
        }
    }

    fn setxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        match Dbfs::setxattr(self, ino, name.as_bytes()) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn removexattr(&mut self, _req: &Request<'_>, ino: u64, name: &OsStr, reply: ReplyEmpty) {
        match Dbfs::removexattr(self, ino, name.as_bytes()) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        match Dbfs::open(self, ino) {
            Ok(fh) => reply.opened(fh, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.release_fh(fh) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        match self.flush_fh(fh) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.sync_fh(fh) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
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

        match self.read_fh(fh, offset as u64, size) {
            Ok(data) => reply.data(&data),
            Err(errno) => reply.error(errno),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
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

        match self.write_fh(fh, offset as u64, data) {
            Ok(written) => reply.written(written),
            Err(errno) => reply.error(errno),
        }
    }

    fn fallocate(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        length: i64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        match Dbfs::fallocate(self, ino, offset, length, mode) {
            Ok(()) => reply.ok(),
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
        let file_type = mode & libc::S_IFMT;
        const S_IFREG: u32 = libc::S_IFREG;
        match file_type {
            // Regular file: delegate to create
            0 | S_IFREG => {
                match Dbfs::create(
                    self,
                    parent,
                    name.as_bytes(),
                    mode & !libc::S_IFMT,
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
        match Dbfs::rename(
            self,
            parent,
            name.as_bytes(),
            newparent,
            newname.as_bytes(),
            flags,
        ) {
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
    /// The FUSE offset of the next entry: pass it to `readdir` to resume after this one.
    pub offset: i64,
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
        atime: inode.atime.to_system_time(),
        mtime: inode.mtime.to_system_time(),
        ctime: inode.ctime.to_system_time(),
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
        DbError::NameTooLong => libc::ENAMETOOLONG,
        DbError::NoSpace => libc::ENOSPC,
        DbError::NotDirectory => libc::ENOTDIR,
        DbError::FileTooLarge => libc::EFBIG,
        DbError::Corrupt | DbError::UnsupportedSchema(_) | DbError::Sqlite(_) => libc::EIO,
    }
}

fn time_or_now(time: Option<TimeOrNow>) -> Option<SystemTime> {
    match time? {
        TimeOrNow::SpecificTime(time) => Some(time),
        TimeOrNow::Now => Some(SystemTime::now()),
    }
}

/// Answers `getxattr` and `listxattr` with FUSE's size protocol. `size == 0`
/// asks only for the length. A buffer that is too small gets `ERANGE`.
fn reply_xattr(value: &[u8], size: u32, reply: ReplyXattr) {
    if size == 0 {
        reply.size(value.len() as u32);
    } else if value.len() > size as usize {
        reply.error(libc::ERANGE);
    } else {
        reply.data(value);
    }
}

#[cfg(test)]
impl Dbfs {
    fn dirty_inodes(&self) -> usize {
        let t = self.table();
        t.inodes
            .live_slots()
            .into_iter()
            .filter(|&slot| !t.inodes.dirty[slot as usize].is_empty())
            .count()
    }

    fn arena_stats(&self) -> (usize, usize) {
        let t = self.table();
        (t.arena.capacity_bytes(), t.arena.slots_in_use())
    }

    fn is_dirty(&self, ino: u64) -> bool {
        let t = self.table();
        t.inodes
            .find(ino)
            .is_some_and(|slot| !t.inodes.dirty[slot as usize].is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statfs_matches_sqlite_pages_and_inode_count() {
        let db = Db::open_in_memory().expect("open in-memory database");
        db.create_file(1, b"notes.txt", 0o644, 1, 1)
            .expect("create file");
        let usage = db.volume_usage().expect("read sqlite volume");
        let fs = Dbfs::new(db);
        let stat = fs.statfs().expect("statfs");

        assert_eq!(stat.bsize as u64, usage.page_size);
        assert_eq!(stat.frsize, stat.bsize);
        assert_eq!(stat.blocks, usage.page_count);
        assert_eq!(stat.bfree, usage.freelist_count);
        assert_eq!(stat.bavail, usage.freelist_count);
        assert_eq!(stat.files, usage.files);
        assert!(stat.blocks > 0);
        assert!(stat.files >= 2);
        assert_eq!(stat.namelen, 255);
    }

    #[test]
    fn flush_returns_enospc_when_the_database_is_full() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db
            .create_file(1, b"big.bin", 0o644, 1, 1)
            .expect("create file");
        db.cap_page_count_at_current()
            .expect("cap sqlite page count");
        let fs = Dbfs::new(db);
        let payload = vec![1u8; 256 * 1024];
        fs.write(file.ino, 0, &payload).expect("buffer write");
        let error = fs.flush_file(file.ino).expect_err("flush full database");
        assert_eq!(error, libc::ENOSPC);
    }

    #[test]
    fn flush_all_persists_dirty_data_for_every_inode() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let small = db
            .create_file(1, b"small.txt", 0o644, 1, 1)
            .expect("create small file");
        let large = db
            .create_file(1, b"large.bin", 0o644, 1, 1)
            .expect("create large file");
        let fs = Dbfs::new(db);
        fs.write(small.ino, 0, b"alpha")
            .expect("buffer small write");
        let large_data = vec![9u8; CHUNK_SIZE + 10];
        fs.write(large.ino, 0, &large_data)
            .expect("buffer large write");

        fs.flush_all().expect("flush_all");

        assert_eq!(fs.dirty_inodes(), 0, "no buffer stays dirty");
        assert_eq!(
            fs.db.read_file(small.ino, 0, 5).expect("read small"),
            b"alpha"
        );
        assert_eq!(
            fs.db
                .read_file(large.ino, 0, large_data.len() as u32)
                .expect("read large"),
            large_data
        );
    }

    #[test]
    fn destroy_flushes_dirty_data_and_records_success() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db
            .create_file(1, b"notes.txt", 0o644, 1, 1)
            .expect("create file");
        let mut fs = Dbfs::new(db);
        let handle = fs.shutdown_handle();
        fs.write(file.ino, 0, b"kept").expect("buffer write");

        Filesystem::destroy(&mut fs);

        assert_eq!(fs.db.read_file(file.ino, 0, 4).expect("read"), b"kept");
        assert_eq!(handle.flush_failure(), None);
    }

    #[test]
    fn shutdown_flush_failure_keeps_data_dirty_and_is_reported() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db
            .create_file(1, b"big.bin", 0o644, 1, 1)
            .expect("create file");
        db.cap_page_count_at_current()
            .expect("cap sqlite page count");
        let fs = Dbfs::new(db);
        let handle = fs.shutdown_handle();
        fs.write(file.ino, 0, &vec![1u8; 256 * 1024])
            .expect("buffer write");

        assert_eq!(handle.flush_for_shutdown(), Err(libc::ENOSPC));
        assert_eq!(handle.flush_failure(), Some(libc::ENOSPC));
        assert!(fs.is_dirty(file.ino), "failed data stays buffered");
    }

    #[test]
    fn writes_are_refused_once_shutdown_has_started() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db
            .create_file(1, b"notes.txt", 0o644, 1, 1)
            .expect("create file");
        let fs = Dbfs::new(db);
        let handle = fs.shutdown_handle();
        fs.write(file.ino, 0, b"before")
            .expect("write before shutdown");

        handle.flush_for_shutdown().expect("shutdown flush");

        assert_eq!(fs.write(file.ino, 0, b"after"), Err(libc::EIO));
        assert_eq!(handle.flush_failure(), None);
        assert_eq!(fs.db.read_file(file.ino, 0, 6).expect("read"), b"before");
    }

    #[test]
    fn a_full_write_buffer_evicts_the_largest_file_and_every_file_reads_back() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let inos: Vec<u64> = (0..3)
            .map(|n| {
                db.create_file(1, format!("f{n}").as_bytes(), 0o644, 1, 1)
                    .expect("create file")
                    .ino
            })
            .collect();
        // Room for four chunks across all files.
        let fs = Dbfs::with_write_buffer(db, 4 * CHUNK_SIZE);
        let handles: Vec<u64> = inos.iter().map(|&ino| fs.open(ino).expect("open")).collect();
        let pattern = |n: usize, len: usize| -> Vec<u8> {
            (0..len).map(|i| (i * 7 + n * 31) as u8).collect()
        };
        for (n, &fh) in handles.iter().enumerate() {
            // Unaligned, so partial chunks take the read-modify-write path.
            fs.write_fh(fh, 100, &pattern(n, 3 * CHUNK_SIZE)).expect("buffer write");
        }
        let (capacity, in_use) = fs.arena_stats();
        assert_eq!(capacity, 4 * CHUNK_SIZE, "the arena never exceeds its budget");
        assert!(in_use <= 4);

        for (n, &fh) in handles.iter().enumerate() {
            let len = 3 * CHUNK_SIZE + 100;
            let mut expected = vec![0u8; 100];
            expected.extend(pattern(n, 3 * CHUNK_SIZE));
            assert_eq!(
                fs.read_fh(fh, 0, len as u32 + 10).expect("read back"),
                expected,
                "file {n} reads back while buffered"
            );
        }
        for &fh in &handles {
            fs.release_file(fh).expect("release");
        }
        for (n, &ino) in inos.iter().enumerate() {
            let mut expected = vec![0u8; 100];
            expected.extend(pattern(n, 3 * CHUNK_SIZE));
            assert_eq!(
                fs.db.read_file(ino, 0, u32::MAX).expect("read stored"),
                expected,
                "file {n} is stored intact"
            );
        }
    }

    #[test]
    fn write_flush_write_reuses_arena_memory() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db.create_file(1, b"reuse.bin", 0o644, 1, 1).expect("create file");
        let fs = Dbfs::new(db);
        let fh = fs.open(file.ino).expect("open");
        fs.write_fh(fh, 0, &vec![1u8; 8 * CHUNK_SIZE]).expect("first write");
        let (capacity, _) = fs.arena_stats();
        fs.flush_fh(fh).expect("flush");
        assert_eq!(fs.arena_stats().1, 0, "a flush returns every slot");
        fs.write_fh(fh, 0, &vec![2u8; 8 * CHUNK_SIZE]).expect("second write");
        assert_eq!(fs.arena_stats().0, capacity, "the arena did not grow");
        fs.release_file(fh).expect("release");
        assert_eq!(
            fs.db.read_file(file.ino, 0, 16).expect("read"),
            vec![2u8; 16]
        );
    }

    #[test]
    fn a_failed_eviction_keeps_the_buffered_data() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db.create_file(1, b"full.bin", 0o644, 1, 1).expect("create file");
        db.cap_page_count_at_current().expect("cap sqlite page count");
        let fs = Dbfs::with_write_buffer(db, 2 * CHUNK_SIZE);
        let fh = fs.open(file.ino).expect("open");
        let first = vec![5u8; 2 * CHUNK_SIZE];
        fs.write_fh(fh, 0, &first).expect("fills the budget");

        assert_eq!(
            fs.write_fh(fh, 2 * CHUNK_SIZE as u64, b"more"),
            Err(libc::ENOSPC),
            "eviction cannot flush into a full database"
        );
        assert_eq!(
            fs.read_fh(fh, 0, first.len() as u32).expect("read"),
            first,
            "the earlier buffered data is intact"
        );
    }
}
