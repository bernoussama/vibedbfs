//! Process-lifetime state for inodes that have open handles or unflushed data.
//!
//! Everything lives in one [`OpenTable`] behind one lock. Per-inode state is a
//! struct of arrays indexed by [`Slot`]; FUSE file handles index straight into
//! [`Handles`], so the data path (`read`, `write`, `flush`, `release`) never
//! hashes or queries SQLite for the inode kind or size.

use std::collections::HashMap;

use crate::arena::{ChunkArena, DirtyIdx};
use crate::{FileKind, Timestamp};

/// Index into the per-inode arrays of [`OpenInodes`].
pub(crate) type Slot = u32;

const NO_SLOT: Slot = Slot::MAX;

/// Per-inode state, one entry per slot. A slot is live while its inode has an
/// open handle (`refs > 0`) or unflushed data.
#[derive(Default)]
pub(crate) struct OpenInodes {
    pub ino: Vec<u64>,
    pub kind: Vec<FileKind>,
    /// Authoritative while the slot is live: dbfs is the only writer, and every
    /// size change (write, truncate, fallocate) goes through the slot.
    pub size: Vec<u64>,
    /// Time of the latest buffered write; meaningful while `dirty` is non-empty.
    pub mtime: Vec<Timestamp>,
    pub refs: Vec<u32>,
    /// The last name was removed while handles were open (`nlink = 0` on disk).
    /// The inode is deleted when the last handle is released.
    pub unlinked: Vec<bool>,
    pub dirty: Vec<DirtyIdx>,
    /// Off the data path: open, create, unlink, rename, getattr, lookup.
    by_ino: HashMap<u64, Slot>,
    free: Vec<Slot>,
}

impl OpenInodes {
    pub fn find(&self, ino: u64) -> Option<Slot> {
        self.by_ino.get(&ino).copied()
    }

    /// Adds a slot for `ino` with no handles and no dirty data.
    pub fn insert(&mut self, ino: u64, kind: FileKind, size: u64) -> Slot {
        debug_assert!(self.find(ino).is_none());
        let slot = match self.free.pop() {
            Some(slot) => {
                let i = slot as usize;
                self.ino[i] = ino;
                self.kind[i] = kind;
                self.size[i] = size;
                self.mtime[i] = Timestamp { secs: 0, nsec: 0 };
                self.refs[i] = 0;
                self.unlinked[i] = false;
                debug_assert!(self.dirty[i].is_empty());
                slot
            }
            None => {
                self.ino.push(ino);
                self.kind.push(kind);
                self.size.push(size);
                self.mtime.push(Timestamp { secs: 0, nsec: 0 });
                self.refs.push(0);
                self.unlinked.push(false);
                self.dirty.push(DirtyIdx::default());
                (self.ino.len() - 1) as Slot
            }
        };
        self.by_ino.insert(ino, slot);
        slot
    }

    /// Frees `slot`. The caller has already dealt with (flushed or released)
    /// its dirty chunks.
    pub fn remove(&mut self, slot: Slot) {
        let i = slot as usize;
        debug_assert!(self.dirty[i].is_empty());
        self.by_ino.remove(&self.ino[i]);
        self.refs[i] = 0;
        self.unlinked[i] = false;
        self.free.push(slot);
    }

    pub fn is_open(&self, slot: Slot) -> bool {
        self.refs[slot as usize] > 0
    }

    /// Live slots, in no particular order. Used by the shutdown flush.
    pub fn live_slots(&self) -> Vec<Slot> {
        self.by_ino.values().copied().collect()
    }

    /// The live slot with the most dirty chunks: the one to flush when the
    /// arena is full. Only runs when the write buffer budget is used up.
    pub fn largest_dirty(&self) -> Option<Slot> {
        self.by_ino
            .values()
            .copied()
            .filter(|&slot| !self.dirty[slot as usize].is_empty())
            .max_by_key(|&slot| self.dirty[slot as usize].len())
    }
}

/// FUSE file handle -> slot. The handle is the index; several handles can map
/// to one slot.
#[derive(Default)]
pub(crate) struct Handles {
    slot: Vec<Slot>,
    free: Vec<u32>,
}

impl Handles {
    pub fn alloc(&mut self, slot: Slot) -> u64 {
        match self.free.pop() {
            Some(fh) => {
                self.slot[fh as usize] = slot;
                fh as u64
            }
            None => {
                self.slot.push(slot);
                (self.slot.len() - 1) as u64
            }
        }
    }

    pub fn get(&self, fh: u64) -> Option<Slot> {
        match self.slot.get(usize::try_from(fh).ok()?) {
            Some(&slot) if slot != NO_SLOT => Some(slot),
            _ => None,
        }
    }

    /// Frees `fh` and returns the slot it pointed at.
    pub fn release(&mut self, fh: u64) -> Option<Slot> {
        let slot = self.get(fh)?;
        self.slot[fh as usize] = NO_SLOT;
        self.free.push(fh as u32);
        Some(slot)
    }
}

pub(crate) struct OpenTable {
    pub inodes: OpenInodes,
    pub handles: Handles,
    /// Bytes of every dirty chunk, shared by all inodes.
    pub arena: ChunkArena,
    /// Set when shutdown starts; later buffered writes are refused.
    pub closing: bool,
}

impl OpenTable {
    /// A table whose dirty data may use up to `write_buffer` bytes.
    pub fn new(write_buffer: usize) -> Self {
        Self {
            inodes: OpenInodes::default(),
            handles: Handles::default(),
            arena: ChunkArena::new(write_buffer),
            closing: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freed_handles_and_slots_are_reused_without_stale_state() {
        let mut table = OpenTable::new(1 << 20);
        let a = table.inodes.insert(10, FileKind::RegularFile, 5);
        table.inodes.refs[a as usize] = 1;
        table.inodes.unlinked[a as usize] = true;
        let fh = table.handles.alloc(a);

        assert_eq!(table.handles.release(fh), Some(a));
        assert_eq!(table.handles.get(fh), None, "a released handle is invalid");
        assert_eq!(table.handles.release(fh), None, "double release is caught");
        table.inodes.remove(a);
        assert_eq!(table.inodes.find(10), None);

        let b = table.inodes.insert(20, FileKind::RegularFile, 7);
        assert_eq!(b, a, "the slot is reused");
        assert_eq!(table.inodes.refs[b as usize], 0);
        assert!(!table.inodes.unlinked[b as usize]);
        assert_eq!(table.inodes.size[b as usize], 7);
        let fh2 = table.handles.alloc(b);
        assert_eq!(fh2, fh, "the handle is reused");
        assert_eq!(table.handles.get(fh2), Some(b));
    }

    #[test]
    fn unknown_handles_are_rejected() {
        let table = OpenTable::new(1 << 20);
        assert_eq!(table.handles.get(0), None);
        assert_eq!(table.handles.get(u64::MAX), None);
    }
}
