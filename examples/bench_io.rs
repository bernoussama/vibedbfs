//! Self-contained filesystem benchmark used for before/after comparisons.
//!
//! Mirrors the fio jobs in `docs/2026-05-03-*` (sync engine, buffered I/O) and
//! adds metadata workloads. It needs no external tools, so it also runs inside
//! containers and CI.
//!
//! Usage: bench_io <workload> <dir>
//! Prints one JSON object per line to stdout.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::Instant;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: bench_io <workload> <dir>");
        eprintln!("workloads: {}", WORKLOADS.join(" "));
        std::process::exit(2);
    }
    let dir = Path::new(&args[2]);
    match args[1].as_str() {
        "seqwrite" => seq_write(dir, 512 * MIB, 128 * KIB),
        "randwrite" => rand_write(dir, 256 * MIB, 4 * KIB, false),
        "randwrite-fsync" => rand_write(dir, 64 * MIB, 4 * KIB, true),
        "seqread" => read_job(dir, 256 * MIB, 128 * KIB, false),
        "randread" => read_job(dir, 256 * MIB, 4 * KIB, true),
        "metadata" => metadata(dir, 200, 5_000),
        "bigdir" => big_dir(dir, 20_000),
        other => {
            eprintln!("unknown workload: {other}");
            std::process::exit(2);
        }
    }
}

const WORKLOADS: &[&str] = &[
    "seqwrite",
    "randwrite",
    "randwrite-fsync",
    "seqread",
    "randread",
    "metadata",
    "bigdir",
];

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

fn seq_write(dir: &Path, size: u64, bs: u64) {
    let buf = payload(bs);
    let start = Instant::now();
    let mut file = File::create(dir.join("seqwrite.dat")).expect("create");
    for _ in 0..size / bs {
        file.write_all(&buf).expect("write");
    }
    drop(file); // close -> FUSE flush, included like fio's end_fsync=0 close
    report_io("seqwrite", start.elapsed().as_secs_f64(), size, size / bs);
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

fn read_job(dir: &Path, size: u64, bs: u64, random: bool) {
    let path = dir.join("read.dat");
    {
        // Untimed layout phase, written sequentially in 1 MiB blocks.
        let buf = payload(MIB);
        let mut file = File::create(&path).expect("create");
        for _ in 0..size / MIB {
            file.write_all(&buf).expect("write");
        }
    }
    // dbfs replies to open without FOPEN_KEEP_CACHE, so reopening drops the
    // kernel page cache for the file and reads go to the filesystem.
    let file = File::open(&path).expect("open");
    let mut buf = vec![0u8; bs as usize];
    let blocks = size / bs;
    let order: Vec<u64> = if random {
        permutation(blocks)
    } else {
        (0..blocks).collect()
    };
    let start = Instant::now();
    for block in &order {
        file.read_exact_at(&mut buf, block * bs).expect("pread");
    }
    let name = if random { "randread" } else { "seqread" };
    report_io(name, start.elapsed().as_secs_f64(), size, blocks);
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
