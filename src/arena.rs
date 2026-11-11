//! Memory for buffered (dirty) chunk data.
//!
//! All dirty chunks of all open inodes share one [`ChunkArena`]: a byte buffer
//! carved into `CHUNK_SIZE` slots, grown on demand up to a global budget and
//! never shrunk, so steady-state writes do not allocate. Each inode keeps a
//! [`DirtyIdx`], a vector of `(chunk_index, arena_slot)` sorted by chunk index.

use std::ops::Range;

use crate::db::CHUNK_SIZE;
use crate::stats;

/// Index of a `CHUNK_SIZE` slot in a [`ChunkArena`].
pub(crate) type ArenaSlot = u32;

pub(crate) struct ChunkArena {
    /// `len.len() * CHUNK_SIZE` bytes, grown one slot at a time.
    bytes: Vec<u8>,
    /// Valid bytes in each slot.
    len: Vec<u32>,
    free: Vec<ArenaSlot>,
    /// Slot limit: the write buffer budget divided by `CHUNK_SIZE`.
    max_slots: usize,
}

impl ChunkArena {
    /// An empty arena that grows to at most `budget` bytes (at least one chunk).
    pub fn new(budget: usize) -> Self {
        Self {
            bytes: Vec::new(),
            len: Vec::new(),
            free: Vec::new(),
            max_slots: (budget / CHUNK_SIZE).max(1),
        }
    }

    /// A slot with no valid bytes, or `None` when the budget is used up.
    pub fn alloc(&mut self) -> Option<ArenaSlot> {
        let slot = match self.free.pop() {
            Some(slot) => slot,
            None if self.len.len() < self.max_slots => {
                self.bytes.resize(self.bytes.len() + CHUNK_SIZE, 0);
                self.len.push(0);
                (self.len.len() - 1) as ArenaSlot
            }
            None => return None,
        };
        self.len[slot as usize] = 0;
        stats::arena_slots_in_use(self.len.len() - self.free.len());
        Some(slot)
    }

    pub fn release(&mut self, slot: ArenaSlot) {
        self.free.push(slot);
    }

    /// The valid bytes of `slot`.
    pub fn chunk(&self, slot: ArenaSlot) -> &[u8] {
        let start = slot as usize * CHUNK_SIZE;
        &self.bytes[start..start + self.len[slot as usize] as usize]
    }

    /// The whole `CHUNK_SIZE` buffer of `slot`, to fill from storage. Follow
    /// with `set_len`.
    pub fn buffer_mut(&mut self, slot: ArenaSlot) -> &mut [u8] {
        let start = slot as usize * CHUNK_SIZE;
        &mut self.bytes[start..start + CHUNK_SIZE]
    }

    pub fn set_len(&mut self, slot: ArenaSlot, len: usize) {
        debug_assert!(len <= CHUNK_SIZE);
        self.len[slot as usize] = len as u32;
    }

    /// Copies `data` to `range` of `slot`. A gap between the old end and
    /// `range.start` is zeroed, so a reused slot never shows stale bytes.
    pub fn write(&mut self, slot: ArenaSlot, range: Range<usize>, data: &[u8]) {
        let base = slot as usize * CHUNK_SIZE;
        let len = self.len[slot as usize] as usize;
        if range.start > len {
            self.bytes[base + len..base + range.start].fill(0);
        }
        self.bytes[base + range.start..base + range.end].copy_from_slice(data);
        self.len[slot as usize] = len.max(range.end) as u32;
    }

    /// Bytes reserved so far. Never shrinks.
    #[cfg(test)]
    pub fn capacity_bytes(&self) -> usize {
        self.bytes.len()
    }

    #[cfg(test)]
    pub fn slots_in_use(&self) -> usize {
        self.len.len() - self.free.len()
    }
}

/// Dirty chunks of one inode: `(chunk_index, arena_slot)` sorted by chunk index.
#[derive(Debug, Default)]
pub(crate) struct DirtyIdx(Vec<(u64, ArenaSlot)>);

impl DirtyIdx {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// `Ok(position)` of `chunk_index`, or `Err(insert position)`. Sequential
    /// writes hit the last entry, so that is checked before the binary search.
    pub fn find(&self, chunk_index: u64) -> Result<usize, usize> {
        match self.0.last() {
            None => Err(0),
            Some(&(last, _)) if last == chunk_index => Ok(self.0.len() - 1),
            Some(&(last, _)) if last < chunk_index => Err(self.0.len()),
            Some(_) => self
                .0
                .binary_search_by_key(&chunk_index, |&(index, _)| index),
        }
    }

    pub fn slot_at(&self, position: usize) -> ArenaSlot {
        self.0[position].1
    }

    pub fn insert(&mut self, position: usize, chunk_index: u64, slot: ArenaSlot) {
        self.0.insert(position, (chunk_index, slot));
    }

    /// Entries with chunk index in `first..=last`.
    pub fn range(&self, first: u64, last: u64) -> &[(u64, ArenaSlot)] {
        let start = self.0.partition_point(|&(index, _)| index < first);
        let end = self.0.partition_point(|&(index, _)| index <= last);
        &self.0[start..end]
    }

    pub fn entries(&self) -> &[(u64, ArenaSlot)] {
        &self.0
    }

    /// Returns every slot to `arena` and empties the index, keeping its capacity.
    pub fn release_into(&mut self, arena: &mut ChunkArena) {
        for &(_, slot) in &self.0 {
            arena.release(slot);
        }
        self.0.clear();
    }

    /// Returns the slots of the first `n` entries (the lowest chunk indexes) to
    /// `arena` and removes those entries. Asking for more than there are empties
    /// the index. The rest stay sorted.
    pub fn release_front(&mut self, n: usize, arena: &mut ChunkArena) {
        for (_, slot) in self.0.drain(..n.min(self.0.len())) {
            arena.release(slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_grows_lazily_up_to_the_budget_and_reuses_slots() {
        let mut arena = ChunkArena::new(2 * CHUNK_SIZE);
        assert_eq!(arena.capacity_bytes(), 0, "nothing is reserved up front");
        let a = arena.alloc().expect("first slot");
        let b = arena.alloc().expect("second slot");
        assert_eq!(arena.alloc(), None, "the budget is two chunks");
        assert_eq!(arena.capacity_bytes(), 2 * CHUNK_SIZE);

        arena.write(a, 0..4, b"abcd");
        arena.release(a);
        let c = arena.alloc().expect("reused slot");
        assert_eq!(c, a);
        assert_eq!(arena.chunk(c), b"", "a reused slot starts empty");
        arena.write(c, 2..3, b"x");
        assert_eq!(arena.chunk(c), b"\0\0x", "stale bytes are zeroed");
        assert_eq!(arena.capacity_bytes(), 2 * CHUNK_SIZE, "no growth on reuse");
        assert_eq!(arena.slots_in_use(), 2);
        let _ = b;
    }

    #[test]
    fn tiny_budgets_still_get_one_chunk() {
        let mut arena = ChunkArena::new(1);
        assert!(arena.alloc().is_some());
        assert_eq!(arena.alloc(), None);
    }

    #[test]
    fn dirty_index_stays_sorted_and_finds_ranges() {
        let mut idx = DirtyIdx::default();
        for (chunk, slot) in [(5, 0), (1, 1), (9, 2), (3, 3)] {
            let position = idx.find(chunk).expect_err("new chunk");
            idx.insert(position, chunk, slot);
        }
        let chunks: Vec<u64> = idx.entries().iter().map(|&(c, _)| c).collect();
        assert_eq!(chunks, [1, 3, 5, 9]);
        assert_eq!(idx.find(9), Ok(3), "last-entry fast path");
        assert_eq!(idx.find(3), Ok(1));
        assert_eq!(idx.find(4), Err(2));
        let range: Vec<u64> = idx.range(2, 5).iter().map(|&(c, _)| c).collect();
        assert_eq!(range, [3, 5]);
        assert!(idx.range(10, 20).is_empty());
    }

    #[test]
    fn release_front_frees_the_lowest_entries_and_keeps_the_rest_sorted() {
        let mut arena = ChunkArena::new(8 * CHUNK_SIZE);
        let mut idx = DirtyIdx::default();
        for chunk in [7u64, 2, 9, 4, 1] {
            let slot = arena.alloc().expect("slot");
            arena.write(slot, 0..1, &[chunk as u8]);
            let position = idx.find(chunk).expect_err("new chunk");
            idx.insert(position, chunk, slot);
        }
        assert_eq!(arena.slots_in_use(), 5);

        idx.release_front(2, &mut arena);

        assert_eq!(arena.slots_in_use(), 3, "two slots went back to the arena");
        let chunks: Vec<u64> = idx.entries().iter().map(|&(c, _)| c).collect();
        assert_eq!(chunks, [4, 7, 9], "the two lowest chunks left the index");
        assert_eq!(idx.find(7), Ok(1));
        assert_eq!(idx.find(2), Err(0));
        let reused = arena.alloc().expect("a released slot");
        assert_eq!(arena.chunk(reused), b"", "a released slot starts empty");
        assert_eq!(arena.capacity_bytes(), 5 * CHUNK_SIZE, "no growth on reuse");
    }

    #[test]
    fn release_front_with_more_than_the_length_empties_the_index() {
        let mut arena = ChunkArena::new(2 * CHUNK_SIZE);
        let mut idx = DirtyIdx::default();
        let slot = arena.alloc().expect("slot");
        idx.insert(0, 3, slot);
        idx.release_front(5, &mut arena);
        assert!(idx.is_empty());
        assert_eq!(arena.slots_in_use(), 0);
    }
}
