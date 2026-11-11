//! 4 KiB block bookkeeping for buffered chunks.
//!
//! A buffered chunk, one arena slot, has [`BLOCKS_PER_CHUNK`] blocks of
//! [`BLOCK_SIZE`] bytes. A [`BlockMask`] records which blocks are *dirty*. A dirty
//! block's bytes in the slot are its complete contents, and a flush stores them.
//! Every other block is unknown: its bytes in the slot are stale or zero, so a
//! reader takes it from the database and a flush keeps the stored bytes.
//!
//! A write marks every block it touches dirty. A block that the write covers only
//! in part is read from the database first, so a dirty block is always complete.

use std::ops::Range;

use crate::db::CHUNK_SIZE;

/// Bytes per block: the unit of partial-chunk writes, reads and flush patches.
pub(crate) const BLOCK_SIZE: usize = 4096;
pub(crate) const BLOCKS_PER_CHUNK: usize = CHUNK_SIZE / BLOCK_SIZE;

/// Bit `b` is set when block `b` (the bytes `b * BLOCK_SIZE..`) of a chunk is dirty.
pub(crate) type BlockMask = u16;

/// Every block of a chunk is dirty.
pub(crate) const ALL_BLOCKS: BlockMask = BlockMask::MAX;

const _: () = assert!(BLOCKS_PER_CHUNK == BlockMask::BITS as usize);

/// The bytes of block `block` within its chunk.
pub(crate) fn block_range(block: usize) -> Range<usize> {
    block * BLOCK_SIZE..(block + 1) * BLOCK_SIZE
}

/// The blocks that the byte range `bytes` (within one chunk) touches at all.
pub(crate) fn touched(bytes: Range<usize>) -> BlockMask {
    if bytes.is_empty() {
        return 0;
    }
    bits(bytes.start / BLOCK_SIZE, (bytes.end - 1) / BLOCK_SIZE)
}

/// The blocks that `bytes` covers completely.
pub(crate) fn covered(bytes: Range<usize>) -> BlockMask {
    let first = bytes.start.div_ceil(BLOCK_SIZE);
    let end = bytes.end / BLOCK_SIZE;
    if first < end { bits(first, end - 1) } else { 0 }
}

/// The blocks that `bytes` touches but does not cover completely. A write there
/// must read those blocks before it can mark them dirty.
pub(crate) fn partly_covered(bytes: Range<usize>) -> BlockMask {
    touched(bytes.clone()) & !covered(bytes)
}

/// Bits `first..=last`.
fn bits(first: usize, last: usize) -> BlockMask {
    debug_assert!(first <= last && last < BLOCKS_PER_CHUNK);
    // Computed in 32 bits, so that all 16 blocks do not overflow the shift.
    let width = last - first + 1;
    (((1u32 << width) - 1) << first) as BlockMask
}

/// The dirty blocks of `mask`, in ascending order.
pub(crate) fn iter(mask: BlockMask) -> impl Iterator<Item = usize> {
    (0..BLOCKS_PER_CHUNK).filter(move |&block| (mask >> block) & 1 != 0)
}

/// How far into the chunk the dirty blocks reach: the end of the highest one.
pub(crate) fn reach(mask: BlockMask) -> usize {
    (BlockMask::BITS - mask.leading_zeros()) as usize * BLOCK_SIZE
}

/// Writes the bytes that one chunk's row should hold after a flush to `out`.
///
/// `stored` is the row (empty for a hole) and `buffer` the slot's chunk bytes.
/// `mask` names the dirty blocks, and `limit` is how many bytes of the chunk lie
/// inside the file. The row keeps its length when that already reaches the dirty
/// data. Otherwise it is extended with zeros, only as far as the highest dirty
/// block. The result never extends past `limit`, so the row never holds a byte
/// beyond the end of the file.
pub(crate) fn merge(
    stored: &[u8],
    mask: BlockMask,
    buffer: &[u8],
    limit: usize,
    out: &mut Vec<u8>,
) {
    let reach = reach(mask).min(limit);
    let kept = stored.len().min(limit);
    let len = kept.max(reach);
    out.clear();
    out.extend_from_slice(&stored[..len.min(stored.len())]);
    out.resize(len, 0);
    for block in iter(mask) {
        let bytes = block_range(block);
        let end = bytes.end.min(limit);
        if bytes.start < end {
            out[bytes.start..end].copy_from_slice(&buffer[bytes.start..end]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(blocks: &[usize]) -> BlockMask {
        blocks.iter().fold(0, |m, &b| m | (1 << b))
    }

    #[test]
    fn touched_covered_and_partly_covered_blocks() {
        // (range, touched, covered, partly covered)
        type Case = (
            Range<usize>,
            &'static [usize],
            &'static [usize],
            &'static [usize],
        );
        let cases: [Case; 8] = [
            (0..4096, &[0], &[0], &[]),
            (100..200, &[0], &[], &[0]),
            (4095..4097, &[0, 1], &[], &[0, 1]),
            (4000..8200, &[0, 1, 2], &[1], &[0, 2]),
            (8192..16384, &[2, 3], &[2, 3], &[]),
            (
                0..CHUNK_SIZE,
                &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
                &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
                &[],
            ),
            (60_000..CHUNK_SIZE, &[14, 15], &[15], &[14]),
            (5000..5000, &[], &[], &[]),
        ];
        for (range, want_touched, want_covered, want_partly) in cases {
            assert_eq!(
                touched(range.clone()),
                mask(want_touched),
                "touched {range:?}"
            );
            assert_eq!(
                covered(range.clone()),
                mask(want_covered),
                "covered {range:?}"
            );
            assert_eq!(
                partly_covered(range.clone()),
                mask(want_partly),
                "partly {range:?}"
            );
        }
    }

    #[test]
    fn reach_is_the_end_of_the_highest_dirty_block() {
        assert_eq!(reach(0), 0);
        assert_eq!(reach(mask(&[0])), BLOCK_SIZE);
        assert_eq!(reach(mask(&[0, 2])), 3 * BLOCK_SIZE);
        assert_eq!(reach(mask(&[15])), CHUNK_SIZE);
        assert_eq!(reach(ALL_BLOCKS), CHUNK_SIZE);
    }

    #[test]
    fn iter_lists_dirty_blocks_in_order() {
        assert_eq!(iter(0).count(), 0);
        assert_eq!(iter(mask(&[3, 0, 9])).collect::<Vec<_>>(), [0, 3, 9]);
        assert_eq!(iter(ALL_BLOCKS).count(), BLOCKS_PER_CHUNK);
    }

    fn merged(stored: &[u8], mask: BlockMask, buffer: &[u8], limit: usize) -> Vec<u8> {
        let mut out = Vec::new();
        merge(stored, mask, buffer, limit, &mut out);
        out
    }

    #[test]
    fn a_row_that_reaches_the_dirty_data_keeps_its_length() {
        let stored = vec![1u8; CHUNK_SIZE];
        let buffer = vec![2u8; CHUNK_SIZE];
        let out = merged(&stored, mask(&[1]), &buffer, CHUNK_SIZE);
        assert_eq!(
            out.len(),
            CHUNK_SIZE,
            "same length, so SQLite can overwrite in place"
        );
        assert_eq!(
            out[..BLOCK_SIZE],
            stored[..BLOCK_SIZE],
            "other blocks keep the stored bytes"
        );
        assert_eq!(
            out[BLOCK_SIZE..2 * BLOCK_SIZE],
            buffer[BLOCK_SIZE..2 * BLOCK_SIZE]
        );
        assert_eq!(out[2 * BLOCK_SIZE..], stored[2 * BLOCK_SIZE..]);
    }

    #[test]
    fn a_hole_is_extended_only_to_the_highest_dirty_block() {
        let buffer = vec![7u8; CHUNK_SIZE];
        let out = merged(&[], mask(&[0, 2]), &buffer, CHUNK_SIZE);
        assert_eq!(out.len(), 3 * BLOCK_SIZE);
        assert_eq!(out[..BLOCK_SIZE], buffer[..BLOCK_SIZE]);
        assert!(
            out[BLOCK_SIZE..2 * BLOCK_SIZE].iter().all(|&b| b == 0),
            "block 1 is a hole"
        );
        assert_eq!(
            out[2 * BLOCK_SIZE..],
            buffer[2 * BLOCK_SIZE..3 * BLOCK_SIZE]
        );
    }

    #[test]
    fn a_short_row_is_extended_with_zeros_up_to_the_dirty_data() {
        let stored = vec![4u8; 10];
        let buffer = vec![9u8; CHUNK_SIZE];
        let out = merged(&stored, mask(&[1]), &buffer, CHUNK_SIZE);
        assert_eq!(out.len(), 2 * BLOCK_SIZE);
        assert_eq!(out[..10], stored[..]);
        assert!(
            out[10..BLOCK_SIZE].iter().all(|&b| b == 0),
            "a hole between"
        );
        assert!(out[BLOCK_SIZE..].iter().all(|&b| b == 9));
    }

    #[test]
    fn nothing_past_the_end_of_the_file_is_stored() {
        let stored = vec![4u8; 2 * BLOCK_SIZE];
        let buffer = vec![9u8; CHUNK_SIZE];
        // The file ends 5000 bytes into the chunk, in the middle of block 1.
        let out = merged(&stored, mask(&[0, 1]), &buffer, 5000);
        assert_eq!(out.len(), 5000, "clipped to the end of the file");
        assert!(out[BLOCK_SIZE..].iter().all(|&b| b == 9));

        // A row longer than the file is cut back to it.
        let out = merged(&vec![4u8; CHUNK_SIZE], 0, &buffer, 5000);
        assert_eq!(out.len(), 5000);
        assert!(out.iter().all(|&b| b == 4));
    }

    #[test]
    fn a_full_mask_stores_the_whole_buffer() {
        let buffer: Vec<u8> = (0..CHUNK_SIZE).map(|i| (i % 251) as u8 + 1).collect();
        let out = merged(&[9u8; 17], ALL_BLOCKS, &buffer, CHUNK_SIZE);
        assert_eq!(out, buffer);
    }

    /// Against a byte-by-byte model: every byte inside the file is what the
    /// block rules say, and the row never grows past the file.
    #[test]
    fn merge_matches_a_byte_model_on_random_inputs() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed ^= seed >> 12;
            seed ^= seed << 25;
            seed ^= seed >> 27;
            seed.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for round in 0..400 {
            let stored_len = next() as usize % (CHUNK_SIZE + 1);
            let stored: Vec<u8> = (0..stored_len).map(|_| (next() as u8) | 1).collect();
            let buffer: Vec<u8> = (0..CHUNK_SIZE).map(|_| (next() as u8) | 0x80).collect();
            let dirty = match round % 4 {
                0 => next() as BlockMask,
                1 => 1 << (next() % 16),
                2 => ALL_BLOCKS,
                _ => 0,
            };
            let limit = if round % 3 == 0 {
                CHUNK_SIZE
            } else {
                next() as usize % (CHUNK_SIZE + 1)
            };

            let out = merged(&stored, dirty, &buffer, limit);
            let reach = reach(dirty).min(limit);
            assert!(out.len() <= limit, "round {round}: row runs past the file");
            assert!(out.len() >= reach, "round {round}: dirty data cut off");
            let kept = stored.len().min(limit);
            assert_eq!(out.len(), kept.max(reach), "round {round}: length rule");
            for p in 0..out.len() {
                let block = p / BLOCK_SIZE;
                let want = if (dirty >> block) & 1 != 0 {
                    buffer[p]
                } else {
                    stored.get(p).copied().unwrap_or(0)
                };
                assert_eq!(out[p], want, "round {round}: byte {p}");
            }
        }
    }
}
