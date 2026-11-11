use std::ffi::OsStr;
use std::ops::Deref;
use std::os::raw::c_int;
use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, Notifier, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow,
};

use crate::blocks::{self, BlockMask, block_range};
use crate::db::{CHUNK_SIZE, ChunkPiece, ChunkSpan, ChunkUpdate, MAX_FILE_SIZE};
use crate::open_table::{OpenTable, Slot};
use crate::{Db, DbError, FileKind, Inode, MetadataUpdate, Timestamp};

/// Attribute and entry TTL used by the default mode and by `--kernel-cache`, where
/// the kernel keeps attributes and names for a day. This is safe because dbfs is the
/// only writer of its database and every change goes through the kernel. A change
/// that no reply carries (a rename over a file with other links or open handles, an
/// extending `fallocate`) is handled by `Shared::invalidate_attr`.
const KERNEL_CACHE_TTL: Duration = Duration::from_secs(86_400);
/// Attribute and entry TTL with `--no-keep-cache`: the kernel asks again after one second.
const ATTR_TTL: Duration = Duration::from_secs(1);
/// Largest read or write request without `--kernel-cache`: the kernel default
/// before FUSE_MAX_PAGES, kept so the default mount behaves as it always has.
const DEFAULT_MAX_REQUEST: u32 = 128 * 1024;
/// Largest request with `--kernel-cache` (the kernel caps FUSE_MAX_PAGES at 256 pages).
const KERNEL_CACHE_MAX_REQUEST: u32 = 1024 * 1024;

/// Default budget for buffered writes, shared by all open files. Memory is taken
/// as dirty chunks need it and is never given back. When the budget is used up,
/// the file with the most dirty chunks is partly written out (see
/// `EVICTION_SLICE_CHUNKS`).
pub const DEFAULT_WRITE_BUFFER: usize = 256 * 1024 * 1024;

/// Most chunks one eviction writes in one transaction (16 MiB). A file with more
/// dirty data is written out over several evictions, so a request that needs a
/// free slot waits for at most this much data to be stored.
const EVICTION_SLICE_CHUNKS: usize = 16 * 1024 * 1024 / CHUNK_SIZE;

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
    /// `--kernel-cache`: kernel writeback cache, 1 MiB requests and the other
    /// options in `Filesystem::init`. Implies `keep_cache`.
    kernel_cache: bool,
    /// Keep the kernel's page cache across opens and cache attributes and names for
    /// a day. On by default; `--no-keep-cache` turns it off.
    keep_cache: bool,
    /// Sends kernel notifications. Set by `run.rs` once the session exists.
    notifier: OnceLock<Notifier>,
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

    /// Writes every dirty chunk of `slot` to the database, in one transaction. On
    /// failure they stay buffered, so a later flush can retry.
    fn flush_slot(&self, t: &mut OpenTable, slot: Slot, unlinked: Unlinked) -> Result<(), i32> {
        self.flush_chunks(t, slot, unlinked, usize::MAX).map(|_| ())
    }

    /// Writes the `limit` lowest-indexed dirty chunks of `slot`, in one
    /// transaction, and returns how many it wrote. On failure they stay buffered.
    fn flush_chunks(
        &self,
        t: &mut OpenTable,
        slot: Slot,
        unlinked: Unlinked,
        limit: usize,
    ) -> Result<usize, i32> {
        let i = slot as usize;
        let OpenTable { inodes, arena, .. } = t;
        if inodes.dirty[i].is_empty() || (inodes.unlinked[i] && unlinked == Unlinked::Skip) {
            return Ok(0);
        }
        let entries = &inodes.dirty[i].entries()[..limit.min(inodes.dirty[i].len())];
        let chunks = entries.iter().map(|&(chunk_index, a)| ChunkUpdate {
            chunk_index,
            buffer: arena.buffer(a),
            dirty: arena.dirty(a),
        });
        self.db
            .replace_chunks(inodes.ino[i], inodes.size[i], chunks, inodes.mtime[i])
            .map_err(errno_from_db_error)?;
        let written = entries.len();
        inodes.dirty[i].release_front(written, arena);
        Ok(written)
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
            span.copy_dirty_blocks(
                chunk_index,
                t.arena.dirty(a),
                t.arena.buffer(a),
                &mut output,
            );
        }
        Ok(output)
    }

    /// An empty arena slot. When the write budget is used up, the inode with the
    /// most dirty chunks (possibly `writing`, the one being written) gives up its
    /// lowest `EVICTION_SLICE_CHUNKS` dirty chunks, in one transaction, which
    /// frees at least one slot.
    fn alloc_chunk(&self, t: &mut OpenTable, writing: Slot) -> Result<u32, i32> {
        loop {
            if let Some(a) = t.arena.alloc() {
                return Ok(a);
            }
            let victim = t.inodes.largest_dirty().ok_or(libc::EIO)?;
            let written = self.flush_chunks(t, victim, Unlinked::Persist, EVICTION_SLICE_CHUNKS)?;
            debug_assert!(written > 0, "a victim has dirty chunks");
            #[cfg(test)]
            t.evicted_chunks.push(written);
            if victim != writing {
                Self::free_if_idle(t, victim);
            }
        }
    }

    /// Buffers `data` at `offset` and returns how many bytes it applied. If a chunk
    /// fails after earlier chunks were applied, those stay buffered and visible, so
    /// the write is reported as short: the caller reissues the rest and then gets the
    /// error. Only a write that applies nothing returns the error itself.
    fn write_slot(
        &self,
        t: &mut OpenTable,
        slot: Slot,
        offset: u64,
        data: &[u8],
    ) -> Result<u32, i32> {
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
        // Set before the first chunk: a flush inside this write stores the new mtime.
        let previous_mtime = t.inodes.mtime[i];
        t.inodes.mtime[i] = Timestamp::from_system_time(SystemTime::now());

        let mut applied = 0;
        for chunk_index in span.first..=span.last {
            let piece = span.piece(chunk_index);
            if let Err(errno) =
                self.write_piece(t, slot, chunk_index, &piece, &data[piece.buf.clone()])
            {
                if applied == 0 {
                    // Nothing changed, so the modification time stays as it was.
                    t.inodes.mtime[i] = previous_mtime;
                    return Err(errno);
                }
                break;
            }
            applied = piece.buf.end;
            // Grow the size chunk by chunk, so an eviction in the middle of this
            // write stores a size that covers every chunk it stores.
            let chunk_end = chunk_index * CHUNK_SIZE as u64 + piece.chunk.end as u64;
            t.inodes.size[i] = t.inodes.size[i].max(chunk_end);
        }
        Ok(applied as u32)
    }

    /// Copies `bytes` into the part of chunk `chunk_index` that `piece` covers,
    /// giving the chunk a slot first if it has none. The blocks that `piece` covers
    /// only in part are read from the database, unless already dirty. Blocks it
    /// covers completely are not read. On error nothing visible has changed.
    fn write_piece(
        &self,
        t: &mut OpenTable,
        slot: Slot,
        chunk_index: u64,
        piece: &ChunkPiece,
        bytes: &[u8],
    ) -> Result<(), i32> {
        let i = slot as usize;
        let (a, is_new) = match t.inodes.dirty[i].find(chunk_index) {
            Ok(position) => (t.inodes.dirty[i].slot_at(position), false),
            // May flush this inode too, so the index is searched again below.
            Err(_) => (self.alloc_chunk(t, slot)?, true),
        };
        let unknown = blocks::partly_covered(piece.chunk.clone()) & !t.arena.dirty(a);
        let ino = t.inodes.ino[i];
        if let Err(errno) = self.load_blocks(t, ino, chunk_index, a, unknown) {
            if is_new {
                t.arena.release(a);
            }
            return Err(errno);
        }
        if is_new {
            let position = t.inodes.dirty[i].find(chunk_index).unwrap_err();
            t.inodes.dirty[i].insert(position, chunk_index, a);
        }
        t.arena.write(a, piece.chunk.clone(), bytes);
        Ok(())
    }

    /// Reads the blocks in `mask` of chunk `chunk_index` from the database into slot
    /// `a`. A hole reads as zeros. The blocks stay unknown until a write marks them
    /// dirty, so the caller must not read them as data before then.
    fn load_blocks(
        &self,
        t: &mut OpenTable,
        ino: u64,
        chunk_index: u64,
        a: u32,
        mask: BlockMask,
    ) -> Result<(), i32> {
        let chunk_start = chunk_index * CHUNK_SIZE as u64;
        let buffer = t.arena.buffer_mut(a);
        for block in blocks::iter(mask) {
            let range = block_range(block);
            buffer[range.clone()].fill(0);
            self.db
                .read_into(ino, chunk_start + range.start as u64, &mut buffer[range])
                .map_err(errno_from_db_error)?;
        }
        Ok(())
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
        t.inodes
            .find(ino)
            .is_some_and(|slot| t.inodes.is_open(slot))
    }

    /// Makes the kernel forget its cached attributes of `ino`, for a change that the
    /// request's reply does not carry, so the next access asks dbfs again. Only
    /// attributes are dropped (offset -1). A notification that also drops pages can
    /// wait for page locks, and a write in flight holds them until this thread
    /// replies, which would deadlock. If the send fails, the mount is going away.
    fn invalidate_attr(&self, ino: u64) {
        if let Some(notifier) = self.notifier.get() {
            let _ = notifier.inval_inode(ino, -1, 0);
        }
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
            // The stored ctime can be later than the buffered write, as after a
            // `link`. Report the later of the two.
            attr.ctime = attr.ctime.max(modified);
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

/// Hands the session's notifier to the filesystem. The session takes the
/// filesystem by value, so the notifier can only be attached after it exists.
pub(crate) struct NotifyHandle {
    shared: Arc<Shared>,
}

impl NotifyHandle {
    /// Attaches the notifier. Call it before the session runs. A second call keeps
    /// the first notifier.
    pub(crate) fn attach(&self, notifier: Notifier) {
        let _ = self.shared.notifier.set(notifier);
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
                kernel_cache: false,
                keep_cache: true,
                notifier: OnceLock::new(),
            }),
        }
    }

    /// Turns on the kernel-side caches (`--kernel-cache`). Must be called before
    /// the filesystem is mounted. It implies `with_keep_cache(true)`.
    pub fn with_kernel_cache(mut self, enabled: bool) -> Self {
        let shared = Arc::get_mut(&mut self.shared)
            .expect("kernel cache is set before the filesystem is shared");
        shared.kernel_cache = enabled;
        shared.keep_cache |= enabled;
        self
    }

    /// Whether the kernel keeps file data across opens and caches attributes and
    /// names for a day (the default). `false` (`--no-keep-cache`) drops the page
    /// cache on every open and expires attributes and names after one second.
    /// Must be called before the filesystem is mounted.
    pub fn with_keep_cache(mut self, enabled: bool) -> Self {
        let shared = Arc::get_mut(&mut self.shared)
            .expect("keep cache is set before the filesystem is shared");
        shared.keep_cache = enabled || shared.kernel_cache;
        self
    }

    /// The handle `run.rs` uses to attach the session's notifier.
    pub(crate) fn notify_handle(&self) -> NotifyHandle {
        NotifyHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    fn ttl(&self) -> Duration {
        if self.keep_cache {
            KERNEL_CACHE_TTL
        } else {
            ATTR_TTL
        }
    }

    /// FOPEN flags for `open` and `create` replies. `FOPEN_KEEP_CACHE` keeps the
    /// page cache across opens. Without it, every open drops the cached data.
    fn open_flags(&self) -> u32 {
        if self.keep_cache {
            fuser::consts::FOPEN_KEEP_CACHE
        } else {
            0
        }
    }

    /// Writes every dirty buffer to the database and makes it durable. Keeps going
    /// after a failure and returns the first error; a failed buffer stays dirty.
    pub fn flush_all(&self) -> Result<(), i32> {
        self.shared.flush_all()
    }

    /// Makes committed directory changes durable for `fsyncdir`. Directories hold
    /// no buffered data and every change is a database commit, so syncing the WAL
    /// is enough. The handle is not used: `opendir` returns 0 and nothing is kept
    /// per directory.
    pub fn sync_dir(&self) -> Result<(), i32> {
        self.db.sync_durable().map_err(errno_from_db_error)
    }

    /// Test hook for disk-full tests: caps the database at `pages` pages. A write
    /// that needs a page past the cap fails with `ENOSPC`. A higher value lifts it.
    #[doc(hidden)]
    pub fn set_max_page_count(&self, pages: u64) -> Result<(), i32> {
        self.db
            .set_max_page_count(pages)
            .map_err(errno_from_db_error)
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
        let mut attr = self
            .db
            .link(ino, new_parent_ino, new_name)
            .map(|inode| file_attr_from_inode(&inode))
            .map_err(errno_from_db_error)?;
        self.overlay_attr(&mut attr);
        Ok(attr)
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

    /// Extended attributes are not stored. An existing inode answers `ENOSYS` on
    /// purpose: the kernel then sets `fc->no_getxattr` and stops sending
    /// `getxattr` for the mount, so the `security.capability` probe no longer
    /// costs a round trip per write. Callers see `EOPNOTSUPP`.
    pub fn getxattr(&self, ino: u64, _name: &[u8]) -> Result<Vec<u8>, i32> {
        self.getattr(ino)?;
        Err(libc::ENOSYS)
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
        self.truncate_locked(&mut t, ino, end)?;
        // The reply carries no attributes, and the extension also set the mtime and
        // ctime in the database, so the kernel must ask again.
        self.invalidate_attr(ino);
        Ok(())
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
            // The reply carries no attributes. The target's link count dropped (and
            // its ctime moved, if it lives on under another name or an open handle),
            // and the kernel cannot see that from this request.
            self.invalidate_attr(target.ino);
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
    fn init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), c_int> {
        if !self.kernel_cache {
            // fuser asks for FUSE_MAX_PAGES whenever the kernel offers it; keep the
            // request size the kernel used before that, so defaults do not change.
            let _ = config.set_max_write(DEFAULT_MAX_REQUEST);
            return Ok(());
        }
        // Each capability is best effort: an older kernel without it still mounts.
        use fuser::consts::{
            FUSE_ASYNC_DIO, FUSE_CACHE_SYMLINKS, FUSE_PARALLEL_DIROPS, FUSE_WRITEBACK_CACHE,
        };
        for capability in [
            FUSE_WRITEBACK_CACHE,
            FUSE_PARALLEL_DIROPS,
            FUSE_CACHE_SYMLINKS,
            FUSE_ASYNC_DIO,
        ] {
            let _ = config.add_capabilities(capability);
        }
        let _ = config.set_max_write(KERNEL_CACHE_MAX_REQUEST);
        let _ = config.set_max_readahead(KERNEL_CACHE_MAX_REQUEST);
        let _ = config.set_max_background(64);
        let _ = config.set_congestion_threshold(48);
        Ok(())
    }

    fn destroy(&mut self) {
        // Runs on unmount, or when the session is dropped. The outcome goes to
        // `ShutdownHandle::flush_failure`, which `run` checks before exiting.
        let result = self.flush_all();
        self.shared.record_flush(result);
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match Dbfs::lookup(self, parent, name.as_bytes()) {
            Ok(attr) => reply.entry(&self.ttl(), &attr, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match Dbfs::getattr(self, ino) {
            Ok(attr) => reply.attr(&self.ttl(), &attr),
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
            Ok(attr) => reply.entry(&self.ttl(), &attr, 0),
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
            Ok((attr, fh)) => reply.created(&self.ttl(), &attr, 0, fh, self.open_flags()),
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
            Ok(attr) => reply.entry(&self.ttl(), &attr, 0),
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
            Ok(attr) => reply.entry(&self.ttl(), &attr, 0),
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
            Ok(attr) => reply.attr(&self.ttl(), &attr),
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
            Ok(fh) => reply.opened(fh, self.open_flags()),
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

    fn fsyncdir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.sync_dir() {
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
                    Ok(attr) => reply.entry(&self.ttl(), &attr, 0),
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
        DbError::DatabaseInUse => libc::EBUSY,
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
        let handles: Vec<u64> = inos
            .iter()
            .map(|&ino| fs.open(ino).expect("open"))
            .collect();
        let pattern = |n: usize, len: usize| -> Vec<u8> {
            (0..len).map(|i| (i * 7 + n * 31) as u8).collect()
        };
        for (n, &fh) in handles.iter().enumerate() {
            // Unaligned, so partial chunks take the read-modify-write path.
            fs.write_fh(fh, 100, &pattern(n, 3 * CHUNK_SIZE))
                .expect("buffer write");
        }
        let (capacity, in_use) = fs.arena_stats();
        assert_eq!(
            capacity,
            4 * CHUNK_SIZE,
            "the arena never exceeds its budget"
        );
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
        let file = db
            .create_file(1, b"reuse.bin", 0o644, 1, 1)
            .expect("create file");
        let fs = Dbfs::new(db);
        let fh = fs.open(file.ino).expect("open");
        fs.write_fh(fh, 0, &vec![1u8; 8 * CHUNK_SIZE])
            .expect("first write");
        let (capacity, _) = fs.arena_stats();
        fs.flush_fh(fh).expect("flush");
        assert_eq!(fs.arena_stats().1, 0, "a flush returns every slot");
        fs.write_fh(fh, 0, &vec![2u8; 8 * CHUNK_SIZE])
            .expect("second write");
        assert_eq!(fs.arena_stats().0, capacity, "the arena did not grow");
        fs.release_file(fh).expect("release");
        assert_eq!(
            fs.db.read_file(file.ino, 0, 16).expect("read"),
            vec![2u8; 16]
        );
    }

    /// One byte at the start of each of `chunks` chunks, so a write needs a new
    /// slot per chunk and stores almost nothing.
    fn one_byte_per_chunk(fs: &Dbfs, fh: u64, chunks: usize) {
        for c in 0..chunks {
            let byte = [(c % 251) as u8 + 1];
            fs.write_fh(fh, (c * CHUNK_SIZE) as u64, &byte)
                .expect("buffer write");
        }
    }

    #[test]
    fn an_eviction_writes_one_slice_and_the_rest_stays_buffered() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db
            .create_file(1, b"big.bin", 0o644, 1, 1)
            .expect("create file");
        let budget = EVICTION_SLICE_CHUNKS + 44;
        let fs = Dbfs::with_write_buffer(db, budget * CHUNK_SIZE);
        let fh = fs.open(file.ino).expect("open");

        // Chunks 0..=budget fill the arena. Writing chunk `budget` evicts.
        one_byte_per_chunk(&fs, fh, budget + 1);
        assert_eq!(fs.table().evicted_chunks, [EVICTION_SLICE_CHUNKS]);
        let stored = |c: usize| fs.db.read_file(file.ino, (c * CHUNK_SIZE) as u64, 1);
        assert_eq!(
            stored(EVICTION_SLICE_CHUNKS - 1).expect("read"),
            [((EVICTION_SLICE_CHUNKS - 1) % 251) as u8 + 1],
            "the lowest slice reached the database"
        );
        assert_eq!(
            stored(EVICTION_SLICE_CHUNKS).expect("read"),
            [0],
            "the next chunk is still buffered, so the database has a hole"
        );
        assert_eq!(fs.arena_stats().1, budget + 1 - EVICTION_SLICE_CHUNKS);

        // Keep going well past the budget: every eviction is one slice.
        let total = 3 * budget;
        for c in budget + 1..total {
            fs.write_fh(fh, (c * CHUNK_SIZE) as u64, &[(c % 251) as u8 + 1])
                .expect("buffer write");
        }
        let evicted = fs.table().evicted_chunks.clone();
        assert!(evicted.len() >= 3, "{evicted:?}");
        assert!(
            evicted.iter().all(|&n| n == EVICTION_SLICE_CHUNKS),
            "every eviction writes one slice: {evicted:?}"
        );

        fs.release_file(fh).expect("release");
        for c in 0..total {
            let byte = (c % 251) as u8 + 1;
            assert_eq!(
                fs.db
                    .read_file(file.ino, (c * CHUNK_SIZE) as u64, 1)
                    .expect("read stored"),
                [byte],
                "chunk {c} is stored"
            );
        }
    }

    #[test]
    fn a_failed_slice_keeps_every_buffered_chunk() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db
            .create_file(1, b"full.bin", 0o644, 1, 1)
            .expect("create file");
        db.cap_page_count_at_current()
            .expect("cap sqlite page count");
        let budget = EVICTION_SLICE_CHUNKS + 44;
        let fs = Dbfs::with_write_buffer(db, budget * CHUNK_SIZE);
        let fh = fs.open(file.ino).expect("open");
        // Half-chunk writes, so the slice needs new database pages and the cap bites.
        for c in 0..budget {
            let half = vec![(c % 251) as u8 + 1; CHUNK_SIZE / 2];
            fs.write_fh(fh, (c * CHUNK_SIZE) as u64, &half)
                .expect("buffer write");
        }

        assert_eq!(
            fs.write_fh(fh, (budget * CHUNK_SIZE) as u64, b"x"),
            Err(libc::ENOSPC),
            "the slice cannot be stored"
        );
        assert!(fs.table().evicted_chunks.is_empty(), "nothing was counted");
        assert_eq!(fs.arena_stats().1, budget, "no slot was freed");
        for c in 0..budget {
            assert_eq!(
                fs.read_fh(fh, (c * CHUNK_SIZE) as u64, 1).expect("read"),
                [(c % 251) as u8 + 1],
                "chunk {c} is still buffered"
            );
        }
    }

    #[test]
    fn a_failed_eviction_keeps_the_buffered_data() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db
            .create_file(1, b"full.bin", 0o644, 1, 1)
            .expect("create file");
        db.cap_page_count_at_current()
            .expect("cap sqlite page count");
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

    #[test]
    fn a_write_that_fails_part_way_reports_the_bytes_already_applied() {
        // Offset 0 starts on a chunk boundary; offset 100 starts with a partial chunk.
        for offset in [0u64, 100] {
            let db = Db::open_in_memory().expect("open in-memory database");
            let file = db
                .create_file(1, b"partial.bin", 0o644, 1, 1)
                .expect("create file");
            db.cap_page_count_at_current()
                .expect("cap sqlite page count");
            // Room for one chunk. The second chunk needs a flush of the first, which fails.
            let fs = Dbfs::with_write_buffer(db, CHUNK_SIZE);
            let fh = fs.open(file.ino).expect("open");
            let data = vec![7u8; 2 * CHUNK_SIZE];

            let written = fs
                .write_fh(fh, offset, &data)
                .expect("the applied chunk is reported as a short write");

            let applied = CHUNK_SIZE - offset as usize;
            assert_eq!(written as usize, applied, "offset {offset}");
            assert_eq!(
                fs.getattr(file.ino).expect("attr").size,
                offset + written as u64,
                "offset {offset}: size covers exactly the reported bytes"
            );
            assert_eq!(
                fs.read_fh(fh, offset, data.len() as u32).expect("read"),
                &data[..applied],
                "offset {offset}: the reported bytes are visible"
            );
        }
    }

    #[test]
    fn a_write_that_applies_nothing_leaves_size_and_mtime_alone() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db
            .create_file(1, b"full.bin", 0o644, 1, 1)
            .expect("create file");
        db.cap_page_count_at_current()
            .expect("cap sqlite page count");
        let fs = Dbfs::with_write_buffer(db, CHUNK_SIZE);
        let fh = fs.open(file.ino).expect("open");
        // A whole chunk, so flushing it needs pages the capped database does not have.
        fs.write_fh(fh, 0, &vec![5u8; CHUNK_SIZE])
            .expect("fills the budget");
        let before = fs.getattr(file.ino).expect("attr before");

        // The next chunk needs a flush of the first, which fails: nothing is applied.
        assert_eq!(
            fs.write_fh(fh, CHUNK_SIZE as u64, b"more"),
            Err(libc::ENOSPC)
        );

        let after = fs.getattr(file.ino).expect("attr after");
        assert_eq!(after.size, before.size);
        assert_eq!(after.mtime, before.mtime, "a failed write keeps the mtime");
    }

    #[test]
    fn keep_cache_is_the_default_and_no_keep_cache_restores_one_second_timeouts() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let fs = Dbfs::new(db);
        assert_eq!(fs.ttl(), KERNEL_CACHE_TTL);
        assert_eq!(fs.open_flags(), fuser::consts::FOPEN_KEEP_CACHE);

        let fs = fs.with_keep_cache(false);
        assert_eq!(fs.ttl(), ATTR_TTL);
        assert_eq!(fs.open_flags(), 0);
    }

    #[test]
    fn kernel_cache_implies_keep_cache() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let fs = Dbfs::new(db).with_kernel_cache(true).with_keep_cache(false);
        assert_eq!(fs.ttl(), KERNEL_CACHE_TTL);
        assert_eq!(fs.open_flags(), fuser::consts::FOPEN_KEEP_CACHE);
    }

    /// The dirty blocks of chunk `chunk` of `ino`, or `None` if it has no slot.
    fn chunk_dirty(fs: &Dbfs, ino: u64, chunk: u64) -> Option<BlockMask> {
        let t = fs.table();
        let idx = &t.inodes.dirty[t.inodes.find(ino)? as usize];
        let position = idx.find(chunk).ok()?;
        Some(t.arena.dirty(idx.slot_at(position)))
    }

    #[test]
    fn partial_writes_mark_only_the_blocks_they_touch() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db
            .create_file(1, b"sparse.bin", 0o644, 1, 1)
            .expect("create file");
        let fs = Dbfs::new(db);
        let fh = fs.open(file.ino).expect("open");

        // Block 1 is covered whole, so it needs no read. Block 0 is covered in part.
        fs.write_fh(fh, CHUNK_SIZE as u64 + 4096, &[1; 4096])
            .expect("whole block");
        assert_eq!(chunk_dirty(&fs, file.ino, 1), Some(0b10));
        fs.write_fh(fh, CHUNK_SIZE as u64 + 100, &[2; 10])
            .expect("part of block 0");
        assert_eq!(chunk_dirty(&fs, file.ino, 1), Some(0b11));

        // Blocks 0 and 2 are covered in part, block 1 whole.
        fs.write_fh(fh, 2 * CHUNK_SIZE as u64 + 4000, &[3; 6000])
            .expect("crosses three blocks");
        assert_eq!(chunk_dirty(&fs, file.ino, 2), Some(0b111));
        assert_eq!(
            chunk_dirty(&fs, file.ino, 0),
            None,
            "chunk 0 is not buffered"
        );

        let mut want = vec![0u8; 4096];
        want[100..110].fill(2);
        assert_eq!(
            fs.read_fh(fh, CHUNK_SIZE as u64 + 4096, 4096)
                .expect("read"),
            vec![1u8; 4096]
        );
        assert_eq!(
            fs.read_fh(fh, CHUNK_SIZE as u64, 4096).expect("read"),
            want,
            "block 0 keeps the zeros around the bytes written"
        );
    }

    #[test]
    fn a_reused_slot_never_shows_the_stale_bytes_of_an_unknown_block() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db
            .create_file(1, b"reuse.bin", 0o644, 1, 1)
            .expect("create file");
        // One slot. Chunk 0 fills it with 0x77, and the flush releases it with
        // those bytes still in the buffer.
        let fs = Dbfs::with_write_buffer(db, CHUNK_SIZE);
        let fh = fs.open(file.ino).expect("open");
        fs.write_fh(fh, 0, &vec![0x77; CHUNK_SIZE])
            .expect("chunk 0");
        fs.flush_file(file.ino).expect("flush");

        // Chunk 1 is a hole that takes the same slot. Only its block 0 is loaded,
        // and only its bytes 5 and 9000 are written. Block 1 is unknown.
        let chunk1 = CHUNK_SIZE as u64;
        fs.write_fh(fh, chunk1 + 5, &[0x42]).expect("first byte");
        fs.write_fh(fh, chunk1 + 9000, &[0x43])
            .expect("second byte");
        assert_eq!(chunk_dirty(&fs, file.ino, 1), Some(0b101));

        let mut want = vec![0u8; 9001];
        want[5] = 0x42;
        want[9000] = 0x43;
        assert_eq!(
            fs.read_fh(fh, chunk1, 9001).expect("read while buffered"),
            want,
            "the unknown block 1 reads as a hole, not as stale 0x77"
        );
        fs.release_file(fh).expect("release");
        fs.flush_all().expect("flush");
        assert_eq!(
            fs.db
                .read_file(file.ino, chunk1, 9001)
                .expect("read stored"),
            want
        );
    }

    #[test]
    fn a_partial_flush_patches_the_stored_chunk_and_keeps_its_length() {
        let db = Db::open_in_memory().expect("open in-memory database");
        let file = db
            .create_file(1, b"patch.bin", 0o644, 1, 1)
            .expect("create file");
        let fs = Dbfs::new(db);
        let fh = fs.open(file.ino).expect("open");
        let original: Vec<u8> = (0..CHUNK_SIZE).map(|i| (i % 253) as u8 + 1).collect();
        fs.write_fh(fh, 0, &original).expect("whole chunk");
        fs.flush_file(file.ino).expect("store it");

        // One byte in block 3. The flush stores the chunk, patched in place.
        fs.write_fh(fh, 3 * 4096 + 7, &[0xEE]).expect("patch");
        fs.flush_file(file.ino).expect("flush the patch");
        let mut want = original;
        want[3 * 4096 + 7] = 0xEE;
        assert_eq!(fs.db.read_file(file.ino, 0, u32::MAX).expect("read"), want);
        fs.release_file(fh).expect("release");
    }
}
