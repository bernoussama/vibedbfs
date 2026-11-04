# Baseline Benchmark (before the data-oriented redesign)

Date: 2026-11-03
Commit: `8c75dde` (master), saved as `.bench/bin/dbfs-baseline-8c75dde`
Plan: [plans/2026-11-03-data-oriented-redesign.md](plans/2026-11-03-data-oriented-redesign.md)

## How to Reproduce

```bash
DBFS_BIN=$PWD/.bench/bin/dbfs-baseline-8c75dde scripts/bench.sh --label baseline-8c75dde --repeat 3
scripts/bench.sh --label raw-ext4 --raw --repeat 3
```

`scripts/bench.sh` mounts a fresh database under `.bench/run/` (ext4, disk-backed)
for every run and drives [`examples/bench_io.rs`](../examples/bench_io.rs).
Raw JSON lines are in `.bench/results/*.jsonl` (gitignored).

This harness is **not fio**: fio isn't installable on this host (no sudo, no C
compiler). The workloads copy the fio jobs in the `2026-05-03-*` docs (sync I/O,
buffered, same sizes and block sizes), but the absolute numbers are not comparable
with those docs. Compare only runs from this harness.

## Environment

| | |
|---|---|
| CPU | Intel Core i5-7200U @ 2.50 GHz, 4 threads |
| RAM | 7 GiB |
| Kernel | Linux 7.0.0-38-generic |
| Filesystem under `.bench/` | ext4 on LVM |
| fuse | fusermount3 3.18.2 |
| SQLite | system `libsqlite3.so.0.8.6` |
| Build | `scripts/cargo-docker.sh build --release` (rust:1-trixie) |

## Results (median of 3 runs)

| Workload | Parameters | dbfs | raw ext4 | dbfs / ext4 |
|---|---|---|---|---|
| seqwrite | 512 MiB, 128 KiB writes, close | 3.33 s · **154 MiB/s** | 0.13 s · 4,048 MiB/s | 26× slower |
| randwrite | 256 MiB, 4 KiB, no fsync | 11.15 s · **23.0 MiB/s** · 5,877 IOPS | 0.26 s · 990 MiB/s | 43× |
| randwrite-fsync | 64 MiB, 4 KiB, fsync each | 75.8 s · **216 IOPS** | 84.0 s · 195 IOPS | ≈ 1× (disk-bound) |
| seqread | 256 MiB, 128 KiB | 0.22 s · **1,141 MiB/s** | 0.03 s · 9,543 MiB/s | 8× |
| randread | 256 MiB, 4 KiB | 3.01 s · **85 MiB/s** · 21,795 IOPS | 0.09 s · 761k IOPS | 35× |

### Metadata (5,000 files × 4 KiB over 200 dirs), seconds

| Stage | dbfs | raw ext4 |
|---|---|---|
| mkdir | 0.019 | 0.005 |
| create + write | 1.236 | 0.091 |
| stat | 0.103 | 0.011 |
| readdir | 0.030 | 0.004 |
| **unlink** | **2.777** | 0.051 |
| rmdir | 0.021 | 0.005 |
| **total** | **4.146** | 0.165 |

Unlink is **2.2× slower than create**. That fits the full `SCAN dirents` that the
FK cascade runs on every inode delete (plan item P1).

### Large directory (20,000 empty files in one directory), seconds

| Stage | dbfs | raw ext4 |
|---|---|---|
| create | 2.898 | 0.463 |
| list (full `readdir`) | 0.099 | 0.007 |
| delete-while-listing (`rm -rf` pattern) | 17.452 | 0.220 |
| **`rm -rf` succeeded** | **no, in 3 of 3 runs** | yes |
| entries left behind | **9,826 of 20,000** | 0 |

## Findings

1. **Confirmed Linux compatibility bug (plan item C6).** Deleting entries while
   listing a directory, which is what `rm -rf`, `find -delete` and many cleanup scripts do,
   skips about half the entries. Positional `OFFSET` cookies shift after each delete. The
   directory is left non-empty, and `rmdir` fails with `ENOTEMPTY`. Phase 6 (keyset
   cookies) fixes this, and the plan schedules it right after Phase C.
2. **Unlink cost grows with the size of the filesystem.** 20,000 unlinks took 17.5 s
   (0.87 ms each), against 0.55 ms each for 5,000 files. Each delete scans all of
   `dirents`. Phase 1 adds the index.
3. **Fsync-heavy writes are disk-bound.** dbfs matches raw ext4 (216 vs 195 IOPS), so
   the gains there are small until fewer fsyncs are issued.
4. **Buffered random writes are the largest CPU-side gap (43×).** A 4 KiB write into
   an unbuffered chunk reads it from SQLite first, and the 64 MiB per-file buffer
   flushes 64 KiB rows repeatedly. Phases 3–5 target this.

## fio Baseline (added after fio became available)

`scripts/bench-fio-ab.sh --repeat 2 baseline=.bench/bin/dbfs-baseline-8c75dde pr1=.bench/bin/dbfs-pr1`
runs the four workloads from `docs/2026-05-03-profiling-results.md` (fio 3.41,
`ioengine=sync`, a fresh database per run, binaries interleaved). Values are both runs.

| Workload | Parameters | Baseline 8c75dde | After PR 1 |
|---|---|---|---|
| randwrite, no fsync | 256M, 4k, fsync=0 | 23.7 / 24.1 MiB/s · 6.1k IOPS · p99 153 µs | 23.9 / 24.3 MiB/s · 6.2k IOPS · p99 153 µs |
| randwrite, fsync | 64M, 4k, fsync=1 | 217 / 223 IOPS · p99 fsync 6.7–6.8 ms | 217 / 214 IOPS · p99 fsync 6.7–6.9 ms |
| randread | 256M, 4k | 62.1 / 63.8 MiB/s · 16.1k IOPS · p99 97–105 µs | 62.3 / 63.3 MiB/s · 16.1k IOPS · p99 98–101 µs |
| seqwrite | 512M, 128k, fsync=0 | 182.5 / 188.5 MiB/s | 171.5 / 198.6 MiB/s |

The May fio docs report much higher randwrite (148 MiB/s) and randread (111 MiB/s)
figures from a different VM with an older kernel. Compare only with this table.

## After PR 1 (schema versioning + `dirents_child` index)

Interleaved A/B with `scripts/bench.sh`, 3 runs per binary, seconds (median):

| Stage | Baseline | PR 1 | Change |
|---|---|---|---|
| metadata: create 5,000 | 1.245 | 1.227 | none |
| metadata: stat 5,000 | 0.116 | 0.107 | none |
| metadata: **unlink 5,000** | 2.891 | 0.884 | **3.3× faster** |
| metadata: total | 4.393 | 2.283 | **1.9× faster** |
| bigdir: create 20,000 in one dir | 2.732 | 3.079 | **13% slower** (index maintenance) |
| bigdir: list | 0.099 | 0.095 | none |
| bigdir: **delete-while-listing** | 17.402 | 1.423 | **12× faster** |
| bigdir: total | 20.137 | 4.597 | **4.4× faster** |
| bigdir: `rm -rf` succeeds | no (9,826 left) | **no (9,826 left)** | unchanged; needs Phase 6 |

fio data workloads are unchanged (table above), as expected: PR 1 only touches
`dirents`.

## After Phase C (shutdown, mount options, xattr/fallocate, Docker)

Binary `.bench/bin/dbfs-phasec-56eba17`, interleaved with PR 1 (3 runs each with
`scripts/bench.sh`, then `scripts/bench-fio-ab.sh --repeat 2`).

| Workload | PR 1 | Phase C | Change |
|---|---|---|---|
| harness randwrite 4k (MiB/s, median) | 23.1 | 21.3 | −8% |
| fio randwrite 4k, no fsync (MiB/s) | 22.9 / 24.0 | 22.1 / 22.2 | −6% |
| fio seqwrite 128k (MiB/s) | 202 / 207 | 193 / 193 | −5% |
| fio randread 4k (MiB/s) | 61.8 / 65.4 | 61.5 / 61.0 | −3% (overlapping) |
| fio randwrite 4k, fsync (IOPS) | 220 / 226 | 217 / 216 | disk-bound, ≈ |
| metadata total (s) | 2.49 | 2.48 | ≈ |
| bigdir total (s) | 4.42 | 4.48 | ≈ |

The 5–8% write-path cost comes from C1, where `RefCell` became `Mutex` so the signal thread
can flush. Each write now takes several uncontended locks, plus a shutdown-flag check.
Phase 4 (one open-inode table, indexed by file handle) removes most of those per-write
lookups and should recover this. Compatibility gain from the same change: `docker stop`
with 16 MiB unflushed exits 0 in 1.3 s with all data kept, against SIGKILL after 10.2 s with
all data lost before.

Docker compatibility matrix on the merged tree: PASS 12, FAIL 0, SKIP 3 (S2 needs
`user_allow_other`; S4 needs a shared host mount), XFAIL 1 (`rm -rf`, needs Phase 6).
