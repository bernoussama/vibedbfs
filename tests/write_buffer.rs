//! Buffered writes through the `Dbfs` API, without mounting. The write buffer is
//! small enough that evictions happen all the time. Every file is checked against
//! an in-memory model while it is buffered, and again after a flush and a reopen.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use dbfs::{Db, Dbfs};

const CHUNK: usize = 64 * 1024;

/// A database file that is removed when the test ends, even after a panic.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        // The target directory is on disk; /tmp may be a RAM disk.
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        Self(dir.join(format!(
            "write-buffer-{name}-{}-{nanos}.sqlite",
            std::process::id()
        )))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.0.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

/// xorshift64*: deterministic, so a failure can be reproduced.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// Non-zero bytes, so a hole (zeros) cannot pass for written data.
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next() % 255) as u8 + 1).collect()
    }
}

/// What a file should contain: the bytes written so far, zero-filled where a
/// write starts past the end.
#[derive(Default)]
struct Model(Vec<u8>);

impl Model {
    fn write(&mut self, offset: usize, data: &[u8]) {
        if self.0.len() < offset + data.len() {
            self.0.resize(offset + data.len(), 0);
        }
        self.0[offset..offset + data.len()].copy_from_slice(data);
    }
}

#[test]
fn many_small_files_survive_constant_eviction_and_reopen() {
    let temp = TempDb::new("many-files");
    const FILES: usize = 60;
    let names: Vec<String> = (0..FILES).map(|n| format!("f{n:02}.bin")).collect();
    let mut models: Vec<Model> = (0..FILES).map(|_| Model::default()).collect();

    {
        let db = Db::open(&temp.0).expect("open database");
        let inos: Vec<u64> = names
            .iter()
            .map(|name| {
                db.create_file(1, name.as_bytes(), 0o644, 1000, 1000)
                    .expect("create file")
                    .ino
            })
            .collect();
        // Eight slots for 60 open files: nearly every write evicts something.
        let fs = Dbfs::with_write_buffer(db, 8 * CHUNK);
        let handles: Vec<u64> = inos
            .iter()
            .map(|&ino| fs.open(ino).expect("open"))
            .collect();

        let mut rng = Rng(0x5EED_0001);
        for _ in 0..1_500 {
            let n = rng.below(FILES as u64) as usize;
            let offset = rng.below(3 * CHUNK as u64) as usize;
            let len = 1 + rng.below(CHUNK as u64 + 512) as usize;
            let data = rng.bytes(len);
            let written = fs
                .write_fh(handles[n], offset as u64, &data)
                .expect("buffer write");
            assert_eq!(written as usize, len);
            models[n].write(offset, &data);

            // Read a random range of a random file while data is buffered.
            let m = rng.below(FILES as u64) as usize;
            let model = &models[m].0;
            if !model.is_empty() {
                let start = rng.below(model.len() as u64) as usize;
                let end = (start + 1 + rng.below(CHUNK as u64) as usize).min(model.len());
                let got = fs
                    .read_fh(handles[m], start as u64, (end - start) as u32)
                    .expect("read buffered");
                assert_eq!(got, model[start..end], "file {m} while buffered");
            }
        }

        for (n, &fh) in handles.iter().enumerate() {
            let got = fs
                .read_fh(fh, 0, models[n].0.len() as u32)
                .expect("read whole file");
            assert_eq!(got, models[n].0, "file {n} before flush");
        }
        for &fh in &handles {
            fs.release_file(fh).expect("release");
        }
        fs.flush_all().expect("flush_all");
    }

    let db = Db::open(&temp.0).expect("reopen database");
    for (n, name) in names.iter().enumerate() {
        let ino = db.lookup(1, name.as_bytes()).expect("lookup").ino;
        let stored = db.read_file(ino, 0, u32::MAX).expect("read stored file");
        assert_eq!(stored, models[n].0, "file {n} after reopen");
    }
}

#[test]
fn a_large_file_written_through_slices_reads_back_after_reopen() {
    let temp = TempDb::new("large-file");
    const SIZE: usize = 40 * 1024 * 1024;
    let ino = {
        let db = Db::open(&temp.0).expect("open database");
        db.create_file(1, b"big.bin", 0o644, 1000, 1000)
            .expect("create file")
            .ino
    };

    // 20 MiB of slots: one file with 320 dirty chunks forces slice evictions
    // (each writes 256 chunks, so the file never fits in the buffer).
    let fs = {
        let db = Db::open(&temp.0).expect("reopen database");
        Dbfs::with_write_buffer(db, 20 * 1024 * 1024)
    };
    let fh = fs.open(ino).expect("open");
    let mut model = Model::default();
    let mut rng = Rng(0x5EED_0002);

    // Sequential, unaligned writes across the whole file.
    let piece = 96 * 1024 + 7;
    let mut offset = 0;
    while offset < SIZE {
        let len = piece.min(SIZE - offset);
        let data = rng.bytes(len);
        fs.write_fh(fh, offset as u64, &data)
            .expect("sequential write");
        model.write(offset, &data);
        offset += len;
    }
    // Random 4 KiB overwrites: most touch a chunk that was already written out.
    for _ in 0..4_096 {
        let block = rng.below((SIZE / 4096) as u64) as usize;
        let data = rng.bytes(4096);
        fs.write_fh(fh, (block * 4096) as u64, &data)
            .expect("random write");
        model.write(block * 4096, &data);
    }

    let mut offset = 0;
    while offset < SIZE {
        let len = (1024 * 1024).min(SIZE - offset);
        let got = fs
            .read_fh(fh, offset as u64, len as u32)
            .expect("read buffered");
        assert!(
            got == model.0[offset..offset + len],
            "range {offset} while buffered"
        );
        offset += len;
    }

    fs.release_file(fh).expect("release");
    fs.flush_all().expect("flush_all");
    drop(fs);

    let db = Db::open(&temp.0).expect("reopen database");
    let stored = db.read_file(ino, 0, u32::MAX).expect("read stored file");
    assert_eq!(stored.len(), SIZE);
    assert!(stored == model.0, "stored contents match the model");
}
