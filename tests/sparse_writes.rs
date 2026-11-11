//! Partial-chunk writes with a tiny write buffer, against an in-memory model.
//!
//! A write that covers part of a 4 KiB block of a chunk that is not buffered
//! reads that block from the database first. These tests make many such writes
//! at unaligned offsets: inside a block, across block and chunk boundaries, ending
//! at the end of the file in the middle of a block, and past the end. Reads,
//! truncates, `fallocate`, flushes, releases and reopens are mixed in. With two to
//! four slots of write buffer, most writes evict something. Every read, every
//! size and every stored byte must match the model. The raw rows must satisfy
//! the length rule: no row runs past the end of its file.
//!
//! Environment:
//! - `DBFS_MODEL_OPS`: operations per seed (default 3000).
//! - `DBFS_MODEL_SEED`: run only this seed (default: seeds 1 to 4).
//!
//! Failure messages name the seed and the operation index.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};

use dbfs::{Db, Dbfs};
use rusqlite::{Connection, OpenFlags};

const CHUNK: usize = 65_536;
const BLOCK: usize = 4096;
/// Files stay below this size, so that comparing whole files stays cheap.
const LIMIT: usize = 8 * CHUNK;
const FILES: usize = 3;
const ROOT: u64 = 1;
const KEEP_SIZE: i32 = libc::FALLOC_FL_KEEP_SIZE;
const CATEGORIES: [&str; 7] = [
    "short write, any offset",
    "whole blocks",
    "whole chunk",
    "across a chunk boundary",
    "ending at end of file",
    "past end of file",
    "long write, any offset",
];

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`. `n` must be non-zero.
    fn index(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().map(|value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be an unsigned integer, got {value:?}"))
    })
}

/// Raw SQLite checks that do not go through `Dbfs`. Returns the first problem.
fn check_disk(path: &Path) -> Result<(), String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open database for raw checks");
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity_check runs");
    if verdict != "ok" {
        return Err(format!("integrity_check: {verdict}"));
    }
    // A row must belong to a live inode and must not run past the end of its file.
    let stray: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM file_chunks c
             LEFT JOIN inodes i ON i.ino = (c.id >> 24)
             WHERE i.ino IS NULL
                OR length(c.data) > i.size - (c.id & 16777215) * 65536",
            [],
            |row| row.get(0),
        )
        .expect("chunk check runs");
    if stray != 0 {
        return Err(format!(
            "{stray} chunk rows belong to no inode or lie past the end of their file"
        ));
    }
    Ok(())
}

/// The chunk index and stored length of every row of `ino`.
fn stored_rows(path: &Path, ino: u64) -> Vec<(i64, i64)> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open database for raw checks");
    let mut stmt = conn
        .prepare(
            "SELECT id & 16777215, length(data) FROM file_chunks
             WHERE (id >> 24) = ?1 ORDER BY id",
        )
        .expect("prepare row query");
    stmt.query_map([ino as i64], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query rows")
        .map(|row| row.expect("read row"))
        .collect()
}

struct Run {
    label: String,
    seed: u64,
    op: usize,
    rng: Rng,
    dir: PathBuf,
    path: PathBuf,
    slots: usize,
    inos: Vec<u64>,
    /// The bytes each file should hold.
    model: Vec<Vec<u8>>,
    /// The open handles of each file.
    handles: Vec<Vec<u64>>,
    tallies: BTreeMap<&'static str, usize>,
    fs: Option<Dbfs>,
}

impl Run {
    /// A fresh database with `FILES` empty files, and a filesystem whose write
    /// buffer holds `slots` chunks.
    fn new(label: &str, seed: u64, slots: usize) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("sparse_writes")
            .join(format!("{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create work dir");
        let path = dir.join("sparse.db");
        let db = Db::open(&path).expect("open database");
        let inos = (0..FILES)
            .map(|i| {
                let name = format!("file{i}");
                db.create_file(ROOT, name.as_bytes(), 0o644, 1, 1)
                    .expect("create file")
                    .ino
            })
            .collect();
        Self {
            label: label.to_string(),
            seed,
            op: 0,
            rng: Rng::new(seed),
            dir,
            path,
            slots,
            inos,
            model: vec![Vec::new(); FILES],
            handles: vec![Vec::new(); FILES],
            tallies: BTreeMap::new(),
            fs: Some(Dbfs::with_write_buffer(db, slots * CHUNK)),
        }
    }

    fn fs(&self) -> &Dbfs {
        self.fs.as_ref().expect("filesystem is open")
    }

    fn fail(&self, what: &str) -> ! {
        panic!("{} seed {} op {}: {what}", self.label, self.seed, self.op);
    }

    fn ok<T>(&self, what: &str, result: Result<T, i32>) -> T {
        match result {
            Ok(value) => value,
            Err(errno) => self.fail(&format!("{what} failed with errno {errno}")),
        }
    }

    fn expect_eq<T: PartialEq + Debug>(&self, what: &str, got: T, want: T) {
        if got != want {
            self.fail(&format!("{what}: got {got:?}, want {want:?}"));
        }
    }

    /// Reports the first byte where `got` and `want` differ. Both start at `offset`.
    fn expect_bytes(&self, what: &str, offset: usize, got: &[u8], want: &[u8]) {
        if got != want {
            let at = got
                .iter()
                .zip(want)
                .position(|(a, b)| a != b)
                .unwrap_or(got.len().min(want.len()));
            self.fail(&format!(
                "{what}: bytes differ at {} (got {} bytes, want {})",
                offset + at,
                got.len(),
                want.len()
            ));
        }
    }

    fn tally(&mut self, name: &'static str) {
        *self.tallies.entry(name).or_default() += 1;
    }

    /// A handle of `file`, opened if it has none.
    fn handle(&mut self, file: usize) -> u64 {
        if self.handles[file].is_empty() {
            let got = self.fs().open(self.inos[file]);
            let fh = self.ok("open", got);
            self.handles[file].push(fh);
        }
        let i = self.rng.index(self.handles[file].len());
        self.handles[file][i]
    }

    fn write(&mut self, file: usize, offset: usize, data: &[u8]) {
        let fh = self.handle(file);
        let got = self.fs().write_fh(fh, offset as u64, data);
        self.expect_eq("write", got, Ok(data.len() as u32));
        let model = &mut self.model[file];
        if model.len() < offset + data.len() {
            model.resize(offset + data.len(), 0);
        }
        model[offset..offset + data.len()].copy_from_slice(data);
    }

    fn read(&mut self, file: usize, offset: usize, size: usize) {
        let fh = self.handle(file);
        let got = self.fs().read_fh(fh, offset as u64, size as u32);
        let got = self.ok("read", got);
        let model = &self.model[file];
        let want = &model[offset.min(model.len())..(offset + size).min(model.len())];
        self.expect_bytes("read", offset, &got, want);
    }

    fn truncate(&mut self, file: usize, size: usize) {
        let got = self.fs().setattr(
            self.inos[file],
            None,
            None,
            None,
            Some(size as u64),
            None,
            None,
        );
        let attr = self.ok("truncate", got);
        self.expect_eq("size after truncate", attr.size, size as u64);
        self.model[file].resize(size, 0);
    }

    fn fallocate(&mut self, file: usize, offset: usize, length: usize, mode: i32) {
        let got = self
            .fs()
            .fallocate(self.inos[file], offset as i64, length as i64, mode);
        self.ok("fallocate", got);
        let end = offset + length;
        let model = &mut self.model[file];
        if (mode & KEEP_SIZE) == 0 && end > model.len() {
            model.resize(end, 0);
        }
    }

    fn flush(&mut self, file: usize) {
        let got = self.fs().flush_file(self.inos[file]);
        self.ok("flush", got);
    }

    fn open(&mut self, file: usize) {
        let got = self.fs().open(self.inos[file]);
        let fh = self.ok("open", got);
        self.handles[file].push(fh);
    }

    fn release(&mut self, file: usize) {
        if self.handles[file].is_empty() {
            return;
        }
        let i = self.rng.index(self.handles[file].len());
        let fh = self.handles[file].swap_remove(i);
        let got = self.fs().release_file(fh);
        self.ok("release", got);
    }

    /// The size and every byte of `file`, as the filesystem reports them.
    fn compare(&self, file: usize) {
        let ino = self.inos[file];
        let got = self.fs().getattr(ino);
        let attr = self.ok("getattr", got);
        let len = self.model[file].len();
        self.expect_eq("size", attr.size, len as u64);
        let got = self.fs().read(ino, 0, len.max(1) as u32);
        let got = self.ok("read whole file", got);
        self.expect_bytes("contents", 0, &got, &self.model[file]);
    }

    /// Flushes every handle, drops the filesystem, reopens the database and
    /// compares every file with the model. Releasing the handles first is random.
    fn reopen(&mut self) {
        self.tally("reopen");
        let fs = self.fs.take().expect("filesystem is open");
        if self.rng.chance(50) {
            for file in 0..FILES {
                while let Some(fh) = self.handles[file].pop() {
                    let got = fs.release_file(fh);
                    self.ok("release before reopen", got);
                }
            }
        }
        let got = fs.flush_all();
        self.ok("flush_all before reopen", got);
        drop(fs);
        // Dropping the filesystem closes the handles that are still open.
        for handles in &mut self.handles {
            handles.clear();
        }
        let db = match Db::open(&self.path) {
            Ok(db) => db,
            Err(error) => self.fail(&format!("reopen failed: {error:?}")),
        };
        self.fs = Some(Dbfs::with_write_buffer(db, self.slots * CHUNK));
        if let Err(problem) = check_disk(&self.path) {
            self.fail(&problem);
        }
        for file in 0..FILES {
            self.compare(file);
        }
    }

    fn step(&mut self) {
        let file = self.rng.index(FILES);
        match self.rng.index(100) {
            0..=39 => {
                let (category, offset, data) = self.random_write(file);
                self.tally(category);
                self.write(file, offset, &data);
            }
            40..=59 => {
                let (offset, size) = self.random_read(file);
                self.tally("read");
                self.read(file, offset, size);
            }
            60..=64 => {
                let size = self.random_size(file);
                self.tally("truncate");
                self.truncate(file, size);
            }
            65..=69 => {
                let (offset, length, mode) = self.random_fallocate(file);
                self.tally("fallocate");
                self.fallocate(file, offset, length, mode);
            }
            70..=79 => {
                self.tally("flush");
                self.flush(file);
            }
            80..=84 => {
                self.tally("open");
                self.open(file);
            }
            85..=89 => {
                self.tally("release");
                self.release(file);
            }
            _ => {
                self.tally("flush and compare");
                self.flush(file);
                self.compare(file);
            }
        }
    }

    /// A write of random non-zero bytes, with its category, offset and data. The
    /// write ends by `LIMIT`.
    fn random_write(&mut self, file: usize) -> (&'static str, usize, Vec<u8>) {
        let len = self.model[file].len();
        let mut category = self.rng.index(CATEGORIES.len());
        if category == 4 && len == 0 {
            category = 0;
        }
        let (offset, size) = match category {
            0 => (self.rng.index(len + BLOCK), 1 + self.rng.index(300)),
            1 => {
                let block = self.rng.index(len / BLOCK + 2);
                (block * BLOCK, (1 + self.rng.index(3)) * BLOCK)
            }
            2 => (self.rng.index(LIMIT / CHUNK) * CHUNK, CHUNK),
            3 => {
                // Starts up to 3000 bytes before a chunk boundary and ends past it.
                let boundary = (1 + self.rng.index(LIMIT / CHUNK - 1)) * CHUNK;
                let before = 1 + self.rng.index(3000);
                (boundary - before, before + 1 + self.rng.index(3000))
            }
            4 => {
                // Ends exactly at the end of the file, which is usually mid-block.
                let size = 1 + self.rng.index(len.min(3000));
                (len - size, size)
            }
            5 => (len + 1 + self.rng.index(3 * CHUNK), 1 + self.rng.index(500)),
            _ => (self.rng.index(LIMIT), 1 + self.rng.index(2 * CHUNK)),
        };
        let offset = offset.min(LIMIT - 1);
        let size = size.min(LIMIT - offset);
        let data: Vec<u8> = (0..size).map(|_| (self.rng.index(255) + 1) as u8).collect();
        (CATEGORIES[category], offset, data)
    }

    fn random_read(&mut self, file: usize) -> (usize, usize) {
        let len = self.model[file].len();
        (self.rng.index(len + BLOCK), 1 + self.rng.index(2 * CHUNK))
    }

    /// A size to truncate to: a shrink to any size, or a grow of up to two chunks.
    fn random_size(&mut self, file: usize) -> usize {
        let len = self.model[file].len();
        if self.rng.chance(50) {
            self.rng.index(len + 1)
        } else {
            (len + self.rng.index(2 * CHUNK)).min(LIMIT)
        }
    }

    fn random_fallocate(&mut self, file: usize) -> (usize, usize, i32) {
        let len = self.model[file].len();
        let offset = self.rng.index(len + BLOCK).min(LIMIT - 1);
        let length = (1 + self.rng.index(CHUNK + CHUNK / 2)).min(LIMIT - offset);
        let mode = if self.rng.chance(50) { 0 } else { KEEP_SIZE };
        (offset, length, mode)
    }

    fn report(&self) {
        let summary: Vec<String> = self
            .tallies
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect();
        eprintln!(
            "sparse_writes {}: {} ops; {}",
            self.label,
            self.op,
            summary.join(" ")
        );
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.fs = None;
        // A failed run keeps its database for inspection.
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

#[test]
fn random_partial_writes_match_the_model() {
    let ops = env_u64("DBFS_MODEL_OPS").unwrap_or(3000) as usize;
    let seeds = match env_u64("DBFS_MODEL_SEED") {
        Some(seed) => vec![seed],
        None => vec![1, 2, 3, 4],
    };
    for seed in seeds {
        // 2, 3 or 4 slots: the buffer is always small, so evictions happen.
        let slots = 2 + (seed % 3) as usize;
        let mut run = Run::new(&format!("seed-{seed}"), seed, slots);
        for op in 0..ops {
            run.op = op;
            run.step();
            if (op + 1).is_multiple_of(300) {
                run.reopen();
            }
        }
        run.op = ops;
        run.reopen();
        run.report();
    }
}

#[test]
fn a_whole_chunk_write_after_a_partial_write_stores_the_whole_chunk() {
    let mut run = Run::new("whole-after-partial", 0, 2);
    run.write(0, 5_000, &[1; 10]);
    run.write(0, 0, &vec![2u8; CHUNK]);
    run.read(0, 0, CHUNK);
    run.flush(0);
    run.reopen();
    run.read(0, 0, CHUNK);
    let rows = stored_rows(&run.path, run.inos[0]);
    run.expect_eq("stored rows", rows, vec![(0, CHUNK as i64)]);
}

#[test]
fn a_write_that_ends_at_the_end_of_the_file_mid_block() {
    let mut run = Run::new("eof-mid-block", 0, 2);
    // 10000 bytes: the file ends in the middle of block 2.
    run.write(0, 0, &vec![1u8; 10_000]);
    run.flush(0);
    run.write(0, 9_900, &[2u8; 100]);
    run.write(0, 9_999, &[3]);
    run.read(0, 9_000, 2_000);
    run.flush(0);
    run.reopen();
    run.read(0, 0, 10_000);
    let rows = stored_rows(&run.path, run.inos[0]);
    run.expect_eq("stored rows", rows, vec![(0, 10_000)]);
}

#[test]
fn a_write_into_the_block_that_a_truncate_cut_reads_zeros_past_the_cut() {
    let mut run = Run::new("truncate-cut", 0, 2);
    run.write(0, 0, &vec![7u8; 3 * BLOCK]);
    run.flush(0);
    // The cut is in the middle of block 1.
    run.truncate(0, 5_000);
    // Past the cut, inside the cut block. Bytes 5000..6000 must read as zeros.
    run.write(0, 6_000, &[8; 10]);
    run.read(0, 0, 6_010);
    // In the next block, which the cut emptied.
    run.write(0, 9_000, &[9]);
    run.read(0, 0, 9_001);
    run.flush(0);
    run.reopen();
    run.read(0, 0, 9_001);
}

#[test]
fn shrinking_and_growing_over_a_partly_valid_chunk_reads_zeros() {
    let mut run = Run::new("shrink-grow", 0, 2);
    run.write(0, 0, &vec![5u8; CHUNK]);
    run.flush(0);
    // Block 1 is dirty and only partly covered.
    run.write(0, 4_100, &[6; 50]);
    run.truncate(0, 4_120);
    run.read(0, 0, CHUNK);
    run.truncate(0, 9_000);
    run.read(0, 0, 9_000);
    run.reopen();
    run.read(0, 0, 9_000);
}

#[test]
fn fallocate_over_a_partly_valid_chunk_keeps_its_bytes() {
    let mut run = Run::new("fallocate", 0, 2);
    run.write(0, 0, &vec![4u8; CHUNK]);
    run.flush(0);
    // Blocks 0 and 3 are dirty and only partly covered, and not flushed.
    run.write(0, 100, &[9]);
    run.write(0, 3 * BLOCK + 10, &[9; 20]);
    // KEEP_SIZE changes neither the size nor the bytes.
    run.fallocate(0, 0, 2 * CHUNK, KEEP_SIZE);
    run.read(0, 0, CHUNK);
    // Mode 0 extends the file with zeros past its end.
    run.fallocate(0, 0, CHUNK + 5_000, 0);
    run.read(0, 0, CHUNK + 5_000);
    run.reopen();
    run.read(0, 0, CHUNK + 5_000);
}

#[test]
fn chunks_that_are_holes_store_only_the_blocks_written() {
    let mut run = Run::new("holes", 0, 2);
    // Chunks 0 to 2 are holes. Chunk 3 gets one byte in block 1.
    run.write(0, 3 * CHUNK + 5_000, &[1]);
    run.flush(0);
    // The row is as long as the file allows, and no other chunk has a row.
    let rows = stored_rows(&run.path, run.inos[0]);
    run.expect_eq("stored rows", rows, vec![(3, 5_001)]);
    run.read(0, 0, 3 * CHUNK + 5_001);
    run.write(0, 3 * CHUNK + 10, &[2; 4]);
    run.reopen();
    run.read(0, 0, 3 * CHUNK + 5_001);
}

#[test]
fn evictions_store_partly_valid_chunks_intact() {
    let mut run = Run::new("evictions", 0, 2);
    for round in 0..60 {
        let file = round % FILES;
        let chunk = (round * 5) % (LIMIT / CHUNK);
        let offset = chunk * CHUNK + (round * 4_099) % (CHUNK - 100);
        let size = 37 + (round * 613) % 5_000;
        run.write(file, offset, &vec![(round % 250 + 1) as u8; size]);
        if round % 5 == 4 {
            run.read(file, offset.saturating_sub(10), 200);
        }
    }
    for file in 0..FILES {
        run.compare(file);
    }
    run.reopen();
    for file in 0..FILES {
        run.compare(file);
    }
}
