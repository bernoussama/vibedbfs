//! Reads of part of one 64 KiB chunk go through incremental blob I/O
//! (`read_chunk_part` in src/db.rs). Each read is checked against a plain model
//! of the file, and a read inside one chunk is also checked against a
//! whole-chunk read, which takes the range-query path.

use std::path::PathBuf;

use dbfs::Db;

const CHUNK: usize = 65_536;

/// Deterministic xorshift generator, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    /// Odd bytes only, so a zero read is always a hole, never data.
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next() as u8) | 1).collect()
    }
}

/// Runs `test` on an in-memory database and on a file database. The file one
/// has WAL and the default memory map, as `dbfs mount` opens it.
fn each_database(name: &str, mut test: impl FnMut(&'static str, &Db)) {
    test("memory", &Db::open_in_memory().expect("in-memory database"));

    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("partial-read-{name}-{}.sqlite", std::process::id()));
    remove_database(&path);
    {
        let db = Db::open(&path).expect("file database");
        test("file", &db);
    }
    remove_database(&path);
}

fn remove_database(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

/// A file under test, next to a plain model of its contents.
struct Subject<'a> {
    kind: &'static str,
    db: &'a Db,
    ino: u64,
    model: Vec<u8>,
}

impl<'a> Subject<'a> {
    fn new(kind: &'static str, db: &'a Db) -> Self {
        let ino = db
            .create_file(1, b"data", 0o644, 0, 0)
            .expect("create file")
            .ino;
        Self {
            kind,
            db,
            ino,
            model: Vec::new(),
        }
    }

    fn write(&mut self, offset: usize, data: &[u8]) {
        self.db
            .write_file(self.ino, offset as u64, data)
            .expect("write");
        let end = offset + data.len();
        if self.model.len() < end {
            self.model.resize(end, 0);
        }
        self.model[offset..end].copy_from_slice(data);
    }

    fn truncate(&mut self, size: usize) {
        self.db
            .truncate_file(self.ino, size as u64)
            .expect("truncate");
        self.model.resize(size, 0);
    }

    fn check_read(&self, offset: usize, len: usize) {
        let got = self
            .db
            .read_file(self.ino, offset as u64, len as u32)
            .expect("read");
        let end = (offset + len).min(self.model.len());
        let want = self.model.get(offset..end).unwrap_or(&[]);
        assert_eq!(got, want, "{}: {len} bytes at {offset}", self.kind);

        let chunk_start = offset / CHUNK * CHUNK;
        if len > 0 && offset + len <= chunk_start + CHUNK {
            let whole = self
                .db
                .read_file(self.ino, chunk_start as u64, CHUNK as u32)
                .expect("whole-chunk read");
            let from = (offset - chunk_start).min(whole.len());
            let to = (offset - chunk_start + len).min(whole.len());
            assert_eq!(
                got,
                &whole[from..to],
                "{}: {len} bytes at {offset}, against a whole-chunk read",
                self.kind
            );
        }
    }
}

#[test]
fn partial_read_in_the_middle_of_a_chunk() {
    each_database("middle", |kind, db| {
        let mut f = Subject::new(kind, db);
        f.write(0, &Rng(1).bytes(2 * CHUNK));
        for (offset, len) in [
            (100, 50),
            (CHUNK / 2 - 1, 2),
            (CHUNK + 4000, 4096),
            (3, 1),
            (CHUNK + 1, CHUNK - 2),
        ] {
            f.check_read(offset, len);
        }
    });
}

#[test]
fn read_across_the_end_of_a_short_blob() {
    each_database("short", |kind, db| {
        let mut f = Subject::new(kind, db);
        // Chunk 0's blob is 10 bytes. Growing the file leaves that blob alone.
        f.write(0, &Rng(7).bytes(10));
        f.truncate(CHUNK);
        for (offset, len) in [
            (0, 10),
            (5, 20),
            (9, 1),
            (10, 30),
            (CHUNK - 20, 20),
            (CHUNK - 1, 1),
            (0, CHUNK),
        ] {
            f.check_read(offset, len);
        }

        // Chunk 1 is cut to 100 bytes, then the file grows past it. Chunk 2 is a hole.
        f.write(CHUNK, &Rng(8).bytes(CHUNK));
        f.truncate(CHUNK + 100);
        f.truncate(2 * CHUNK + 50);
        for (offset, len) in [
            (CHUNK + 90, 20),
            (CHUNK + 99, 2),
            (CHUNK + 100, 200),
            (CHUNK + 50, 1000),
            (2 * CHUNK + 40, 30),
        ] {
            f.check_read(offset, len);
        }
    });
}

#[test]
fn read_of_a_hole_is_zeros() {
    each_database("hole", |kind, db| {
        let mut f = Subject::new(kind, db);
        // Chunks 0 and 1 are holes. Chunk 2 is a 15-byte blob: 10 zeros, then data.
        f.write(2 * CHUNK + 10, &Rng(9).bytes(5));
        for (offset, len) in [
            (100, 50),
            (CHUNK + 100, 50),
            (CHUNK - 10, 20),
            (2 * CHUNK + 5, 20),
            (2 * CHUNK + 12, 100),
            (0, 3 * CHUNK),
        ] {
            f.check_read(offset, len);
        }
    });
}

#[test]
fn reads_at_chunk_boundaries() {
    each_database("boundary", |kind, db| {
        let mut f = Subject::new(kind, db);
        f.write(0, &Rng(10).bytes(3 * CHUNK));
        for (offset, len) in [
            (0, 1),
            (CHUNK - 1, 1),
            (CHUNK - 10, 10),
            (CHUNK, 10),
            (CHUNK - 5, 10),
            (CHUNK, CHUNK),
            (2 * CHUNK - 1, 2),
            (3 * CHUNK - 1, 2),
            (1, CHUNK),
            (0, CHUNK + 1),
        ] {
            f.check_read(offset, len);
        }
    });
}

#[test]
fn single_byte_reads_match_the_model() {
    each_database("bytes", |kind, db| {
        let mut f = Subject::new(kind, db);
        let size = 2 * CHUNK + 1000;
        f.write(0, &Rng(11).bytes(size));
        let offsets = (CHUNK - 40..CHUNK + 40)
            .chain((0..size).step_by(4099))
            .chain([size - 1, size, size + 1]);
        for offset in offsets {
            f.check_read(offset, 1);
        }
    });
}

#[test]
fn random_reads_and_writes_match_the_model() {
    each_database("random", |kind, db| {
        let mut f = Subject::new(kind, db);
        let mut rng = Rng(0x5EED_0001);
        for _ in 0..1500 {
            match rng.below(10) {
                0..=4 => {
                    let offset = rng.below(3 * CHUNK);
                    let len = 1 + rng.below(CHUNK + 3000);
                    f.write(offset, &rng.bytes(len));
                }
                5 => {
                    let size = rng.below(3 * CHUNK + 100);
                    f.truncate(size);
                }
                _ => {
                    let offset = rng.below(f.model.len() + 100);
                    let len = 1 + rng.below(2 * CHUNK);
                    f.check_read(offset, len);
                }
            }
        }
        f.check_read(0, f.model.len() + 1);
    });
}
