//! Self-contained filesystem benchmark used for before/after comparisons.
//!
//! Mirrors the fio jobs in `docs/2026-05-03-*` (sync engine, buffered I/O) and
//! adds metadata workloads. It needs no external tools, so it also runs inside
//! containers and CI.
//!
//! ```text
//! bench_io <workload> <dir> [SIZE]    SIZE is accepted only by seqwrite-long
//! bench_io layout <dir> <SIZE>
//! bench_io read-only <dir> <seq|rand> <SIZE>
//! bench_io tree-layout <dir>
//! bench_io tree-stat <dir>
//! bench_io tree-rm <dir>
//! ```
//!
//! SIZE is a byte count, optionally with a binary K, M or G suffix
//! (case-insensitive): 65536, 64K, 512M, 3G. Prints one JSON object per line
//! to stdout.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::Instant;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

// Tree layout (tree-layout, tree-stat, tree-rm, smallfiles): d00..d19 under the
// root, s00..s19 under each dNN, and f000..f049 under each sNN.
const TREE_DIRS: u64 = 20;
const TREE_SUBDIRS: u64 = 20;
const TREE_FILES: u64 = 50;
/// File `i` is `KIB * (1 + mix64(i) % TREE_MAX_KIB)` bytes.
const TREE_MAX_KIB: u64 = 16;
const TREE_FILE_COUNT: u64 = TREE_DIRS * TREE_SUBDIRS * TREE_FILES;
const TREE_DIR_COUNT: u64 = TREE_DIRS + TREE_DIRS * TREE_SUBDIRS;
const TREE_ENTRIES: u64 = TREE_FILE_COUNT + TREE_DIR_COUNT;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        usage_error("expected <workload> <dir>");
    }
    let dir = Path::new(&args[2]);
    let extra = &args[3..];
    match args[1].as_str() {
        "seqwrite" => {
            no_extra(extra);
            seq_write(dir, "seqwrite", 512 * MIB, 128 * KIB, false);
        }
        "seqwrite-long" => {
            let size = match extra {
                [] => 3 * GIB,
                [text] => parse_size_arg(text),
                _ => usage_error("seqwrite-long takes at most one SIZE"),
            };
            if size < 128 * KIB {
                usage_error("SIZE must be at least 128K");
            }
            seq_write(dir, "seqwrite-long", size, 128 * KIB, true);
        }
        "randwrite" => {
            no_extra(extra);
            rand_write(dir, 256 * MIB, 4 * KIB, false);
        }
        "randwrite-fsync" => {
            no_extra(extra);
            rand_write(dir, 64 * MIB, 4 * KIB, true);
        }
        "seqread" => {
            no_extra(extra);
            read_job(dir, 256 * MIB, 128 * KIB, false);
        }
        "randread" => {
            no_extra(extra);
            read_job(dir, 256 * MIB, 4 * KIB, true);
        }
        "metadata" => {
            no_extra(extra);
            metadata(dir, 200, 5_000);
        }
        "bigdir" => {
            no_extra(extra);
            big_dir(dir, 20_000);
        }
        "smallfiles" => {
            no_extra(extra);
            smallfiles(dir);
        }
        "layout" => layout_job(dir, one_size(extra)),
        "read-only" => {
            let (random, size) = match extra {
                [mode, text] => (read_mode(mode), parse_size_arg(text)),
                _ => usage_error("read-only takes <seq|rand> <SIZE>"),
            };
            read_only(dir, random, size);
        }
        "tree-layout" => {
            no_extra(extra);
            tree_layout(dir);
        }
        "tree-stat" => {
            no_extra(extra);
            tree_stat(dir);
        }
        "tree-rm" => {
            no_extra(extra);
            tree_rm(dir);
        }
        other => usage_error(&format!("unknown workload: {other}")),
    }
}

const WORKLOADS: &[&str] = &[
    "seqwrite",
    "seqwrite-long",
    "randwrite",
    "randwrite-fsync",
    "seqread",
    "randread",
    "metadata",
    "bigdir",
    "smallfiles",
    "layout",
    "read-only",
    "tree-layout",
    "tree-stat",
    "tree-rm",
];

/// Prints the usage text and exits with status 2.
fn usage_error(message: &str) -> ! {
    eprintln!("bench_io: {message}");
    eprintln!("usage: bench_io <workload> <dir> [SIZE]    (SIZE only for seqwrite-long)");
    eprintln!("       bench_io layout <dir> <SIZE>");
    eprintln!("       bench_io read-only <dir> <seq|rand> <SIZE>");
    eprintln!("       bench_io tree-layout <dir>");
    eprintln!("       bench_io tree-stat <dir>");
    eprintln!("       bench_io tree-rm <dir>");
    eprintln!("SIZE: bytes, or with a binary K, M or G suffix (e.g. 65536, 64K, 512M, 3G)");
    eprintln!("workloads: {}", WORKLOADS.join(" "));
    std::process::exit(2);
}

fn no_extra(extra: &[String]) {
    if let Some(arg) = extra.first() {
        usage_error(&format!("unexpected argument: {arg}"));
    }
}

fn one_size(extra: &[String]) -> u64 {
    match extra {
        [text] => parse_size_arg(text),
        _ => usage_error("expected <SIZE>"),
    }
}

fn read_mode(mode: &str) -> bool {
    match mode {
        "seq" => false,
        "rand" => true,
        other => usage_error(&format!("read-only mode must be seq or rand, not {other}")),
    }
}

/// Parses SIZE, exiting with a usage error if it is invalid or zero.
fn parse_size_arg(text: &str) -> u64 {
    parse_size(text).unwrap_or_else(|| usage_error(&format!("invalid SIZE: {text}")))
}

/// A byte count with an optional binary K, M or G suffix. Zero is rejected.
fn parse_size(text: &str) -> Option<u64> {
    let (digits, unit) = match text.as_bytes().last().copied()? {
        b'k' | b'K' => (&text[..text.len() - 1], KIB),
        b'm' | b'M' => (&text[..text.len() - 1], MIB),
        b'g' | b'G' => (&text[..text.len() - 1], GIB),
        _ => (text, 1),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let size = digits.parse::<u64>().ok()?.checked_mul(unit)?;
    (size > 0).then_some(size)
}

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Random permutation of block indices, like fio's default random map
/// (every block touched exactly once).
fn permutation(blocks: u64) -> Vec<u64> {
    let mut order: Vec<u64> = (0..blocks).collect();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for i in (1..order.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    order
}

fn payload(bs: u64) -> Vec<u8> {
    let mut rng = Rng(42);
    (0..bs).map(|_| rng.next() as u8).collect()
}

fn report_io(workload: &str, secs: f64, bytes: u64, ops: u64) {
    println!(
        "{{\"workload\":\"{workload}\",\"secs\":{secs:.4},\"mib_s\":{:.1},\"iops\":{:.0}}}",
        bytes as f64 / MIB as f64 / secs,
        ops as f64 / secs
    );
}

/// Like `report_io` for walks that move no data: entries in place of bytes.
fn report_tree(workload: &str, secs: f64, entries: u64) {
    println!(
        "{{\"workload\":\"{workload}\",\"secs\":{secs:.4},\"iops\":{:.0},\"entries\":{entries}}}",
        entries as f64 / secs
    );
}

/// Sequential `bs`-byte writes to `<dir>/<workload>.dat`, totalling `size`
/// rounded down to whole blocks. With `fsync`, the file is synced before close.
/// The timer covers the writes, the optional fsync and the close.
fn seq_write(dir: &Path, workload: &str, size: u64, bs: u64, fsync: bool) {
    let buf = payload(bs);
    let ops = size / bs;
    let total = ops * bs;
    let start = Instant::now();
    let mut file = File::create(dir.join(format!("{workload}.dat"))).expect("create");
    for _ in 0..ops {
        file.write_all(&buf).expect("write");
    }
    if fsync {
        file.sync_all().expect("fsync");
    }
    drop(file); // close -> FUSE flush, included like fio's end_fsync=0 close
    report_io(workload, start.elapsed().as_secs_f64(), total, ops);
}

fn rand_write(dir: &Path, size: u64, bs: u64, fsync_each: bool) {
    let buf = payload(bs);
    let order = permutation(size / bs);
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(dir.join("randwrite.dat"))
        .expect("create");
    // fio lays the file out before the timed phase.
    file.set_len(size).expect("set_len");
    let start = Instant::now();
    for block in &order {
        file.write_all_at(&buf, block * bs).expect("pwrite");
        if fsync_each {
            file.sync_all().expect("fsync");
        }
    }
    drop(file);
    let name = if fsync_each {
        "randwrite-fsync"
    } else {
        "randwrite"
    };
    report_io(name, start.elapsed().as_secs_f64(), size, size / bs);
}

/// Creates `path` with `size` bytes from `buf` (at least 1 MiB), written
/// sequentially in 1 MiB writes; the last write may be short. Returns the
/// number of writes. The file is closed on return.
fn write_layout(path: &Path, size: u64, buf: &[u8]) -> u64 {
    let mut file = File::create(path).expect("create");
    let ops = size.div_ceil(MIB);
    for i in 0..ops {
        let n = (size - i * MIB).min(MIB) as usize;
        file.write_all(&buf[..n]).expect("write");
    }
    ops
}

/// The `layout` helper: timed creation of `<dir>/read.dat` with `size` bytes.
fn layout_job(dir: &Path, size: u64) {
    let buf = payload(MIB);
    let start = Instant::now();
    let ops = write_layout(&dir.join("read.dat"), size, &buf);
    report_io("layout", start.elapsed().as_secs_f64(), size, ops);
}

/// Block indices `0..blocks`, in order or in the random permutation.
fn block_order(blocks: u64, random: bool) -> Vec<u64> {
    if random {
        permutation(blocks)
    } else {
        (0..blocks).collect()
    }
}

/// Reads `bs`-byte blocks of `file` in `order` with pread and returns the
/// elapsed seconds. Buffer allocation is outside the timed region.
fn read_blocks(file: &File, bs: u64, order: &[u64]) -> f64 {
    let mut buf = vec![0u8; bs as usize];
    let start = Instant::now();
    for block in order {
        file.read_exact_at(&mut buf, block * bs).expect("pread");
    }
    start.elapsed().as_secs_f64()
}

fn read_job(dir: &Path, size: u64, bs: u64, random: bool) {
    let path = dir.join("read.dat");
    // Untimed layout phase.
    write_layout(&path, size, &payload(MIB));
    // dbfs replies to open without FOPEN_KEEP_CACHE, so reopening drops the
    // kernel page cache for the file and reads go to the filesystem.
    let file = File::open(&path).expect("open");
    let blocks = size / bs;
    let order = block_order(blocks, random);
    let secs = read_blocks(&file, bs, &order);
    let name = if random { "randread" } else { "seqread" };
    report_io(name, secs, size, blocks);
}

fn missing_layout() -> ! {
    eprintln!("read.dat missing or too small; run `bench_io layout <dir> <SIZE>` first");
    std::process::exit(1);
}

/// The cold read phase over an existing `<dir>/read.dat`. The file is opened
/// read-only and never created or truncated. The caller drops caches first.
fn read_only(dir: &Path, random: bool, size: u64) {
    let bs = if random { 4 * KIB } else { 128 * KIB };
    if size < bs {
        let min = if random { "4K" } else { "128K" };
        usage_error(&format!("SIZE must be at least {min}"));
    }
    let path = dir.join("read.dat");
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => missing_layout(),
        Err(e) => panic!("open {}: {e}", path.display()),
    };
    if file.metadata().expect("stat").len() < size {
        missing_layout();
    }
    let blocks = size / bs;
    let order = block_order(blocks, random);
    let secs = read_blocks(&file, bs, &order);
    let name = if random {
        "randread-cold"
    } else {
        "seqread-cold"
    };
    report_io(name, secs, size, blocks);
}

fn stage<T>(stages: &mut Vec<(&'static str, f64)>, name: &'static str, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let out = f();
    stages.push((name, start.elapsed().as_secs_f64()));
    out
}

fn report_stages(workload: &str, stages: &[(&str, f64)], extra: &str) {
    let total: f64 = stages.iter().map(|(_, s)| s).sum();
    let parts: Vec<String> = stages
        .iter()
        .map(|(name, secs)| format!("\"{name}\":{secs:.4}"))
        .collect();
    println!(
        "{{\"workload\":\"{workload}\",\"secs\":{total:.4},{}{extra}}}",
        parts.join(",")
    );
}

/// The profiling doc's metadata workload: create/stat/readdir/unlink of
/// `files` small files spread over `dirs` directories.
fn metadata(dir: &Path, dirs: usize, files: usize) {
    let root = dir.join("meta");
    let data = payload(4 * KIB);
    let paths: Vec<_> = (0..files)
        .map(|i| {
            root.join(format!("d{:03}", i % dirs))
                .join(format!("f{i:05}"))
        })
        .collect();
    let mut stages = Vec::new();

    stage(&mut stages, "mkdir", || {
        fs::create_dir(&root).expect("mkdir root");
        for d in 0..dirs {
            fs::create_dir(root.join(format!("d{d:03}"))).expect("mkdir");
        }
    });
    stage(&mut stages, "create", || {
        for path in &paths {
            let mut f = File::create(path).expect("create");
            f.write_all(&data).expect("write");
        }
    });
    stage(&mut stages, "stat", || {
        for path in &paths {
            fs::metadata(path).expect("stat");
        }
    });
    let listed = stage(&mut stages, "readdir", || {
        let mut n = 0;
        for d in 0..dirs {
            n += fs::read_dir(root.join(format!("d{d:03}")))
                .expect("readdir")
                .count();
        }
        n
    });
    assert_eq!(listed, files, "readdir returned wrong entry count");
    stage(&mut stages, "unlink", || {
        for path in &paths {
            fs::remove_file(path).expect("unlink");
        }
    });
    stage(&mut stages, "rmdir", || {
        for d in 0..dirs {
            fs::remove_dir(root.join(format!("d{d:03}"))).expect("rmdir");
        }
        fs::remove_dir(&root).expect("rmdir root");
    });
    report_stages("metadata", &stages, "");
}

/// One large directory: create, list, then `rm -rf` it. Measures readdir
/// paging cost and checks that deleting while listing removes everything.
fn big_dir(dir: &Path, files: usize) {
    let root = dir.join("bigdir");
    let mut stages = Vec::new();
    stage(&mut stages, "create", || {
        fs::create_dir(&root).expect("mkdir");
        for i in 0..files {
            File::create(root.join(format!("entry-{i:06}"))).expect("create");
        }
    });
    let listed = stage(&mut stages, "list", || {
        fs::read_dir(&root).expect("readdir").count()
    });
    // Like `rm -rf`: read the directory and delete entries as they stream in.
    let rm_ok = stage(&mut stages, "rm_rf", || {
        for entry in fs::read_dir(&root).expect("readdir") {
            let entry = entry.expect("dirent");
            fs::remove_file(entry.path()).expect("unlink");
        }
        fs::remove_dir(&root).is_ok()
    });
    let leftover = if rm_ok {
        0
    } else {
        fs::read_dir(&root).map(|it| it.count()).unwrap_or(0)
    };
    report_stages(
        "bigdir",
        &stages,
        &format!(
            ",\"listed\":{listed},\"expected\":{files},\"rm_rf_ok\":{rm_ok},\"leftover\":{leftover}"
        ),
    );
}

/// splitmix64 finalizer: a stateless 64-bit hash.
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Size in bytes of file `i` (creation order) in the tree layout.
fn tree_file_size(i: u64) -> u64 {
    KIB * (1 + mix64(i) % TREE_MAX_KIB)
}

/// Creates the tree layout at `root`, which must not exist yet. Contents are
/// the first `size` bytes of `payload(16 KiB)`. Returns (total bytes, files).
/// Shared by tree-layout and smallfiles.
fn build_tree(root: &Path) -> (u64, u64) {
    let buf = payload(TREE_MAX_KIB * KIB);
    fs::create_dir(root).expect("mkdir root");
    let mut bytes = 0;
    let mut index = 0;
    for d in 0..TREE_DIRS {
        let dir_d = root.join(format!("d{d:02}"));
        fs::create_dir(&dir_d).expect("mkdir");
        for s in 0..TREE_SUBDIRS {
            let dir_s = dir_d.join(format!("s{s:02}"));
            fs::create_dir(&dir_s).expect("mkdir");
            for f in 0..TREE_FILES {
                let size = tree_file_size(index);
                let mut file = File::create(dir_s.join(format!("f{f:03}"))).expect("create");
                file.write_all(&buf[..size as usize]).expect("write");
                bytes += size;
                index += 1;
            }
        }
    }
    (bytes, index)
}

fn tree_layout(dir: &Path) {
    let start = Instant::now();
    let (bytes, files) = build_tree(&dir.join("tree"));
    report_io("tree-layout", start.elapsed().as_secs_f64(), bytes, files);
}

fn smallfiles(dir: &Path) {
    let start = Instant::now();
    let (bytes, files) = build_tree(&dir.join("smallfiles"));
    report_io("smallfiles", start.elapsed().as_secs_f64(), bytes, files);
}

/// Counts every entry below `path`: read_dir on each directory and
/// symlink_metadata on each entry, recursing into subdirectories.
fn walk_count(path: &Path, entries: &mut u64) {
    for entry in fs::read_dir(path).expect("readdir") {
        let child = entry.expect("dirent").path();
        let meta = fs::symlink_metadata(&child).expect("lstat");
        *entries += 1;
        if meta.is_dir() {
            walk_count(&child, entries);
        }
    }
}

fn tree_stat(dir: &Path) {
    let mut entries = 0;
    let start = Instant::now();
    walk_count(&dir.join("tree"), &mut entries);
    let secs = start.elapsed().as_secs_f64();
    if entries != TREE_ENTRIES {
        eprintln!("tree-stat: found {entries} entries, expected {TREE_ENTRIES}");
        std::process::exit(1);
    }
    report_tree("tree-cold-stat", secs, entries);
}

/// `rm -rf` of `<dir>/tree`, timed.
fn tree_rm(dir: &Path) {
    let root = dir.join("tree");
    let start = Instant::now();
    fs::remove_dir_all(&root).expect("remove_dir_all");
    let secs = start.elapsed().as_secs_f64();
    if root.symlink_metadata().is_ok() {
        eprintln!("tree-rm: {} still exists", root.display());
        std::process::exit(1);
    }
    report_tree("tree-cold-rm", secs, TREE_ENTRIES);
}
