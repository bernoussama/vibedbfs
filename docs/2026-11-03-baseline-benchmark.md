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

## After Phase 6 (stable dirent ids as readdir cookies)

Binary `.bench/bin/dbfs-phase6-0c40b3c`, interleaved with the baseline and Phase C
binaries (3 runs each with `scripts/bench.sh`), seconds (median):

| Stage | Baseline | Phase C | Phase 6 |
|---|---|---|---|
| metadata: create 5,000 | 1.23 | 1.38 | 1.48 |
| metadata: unlink 5,000 | 2.67 | 0.957 | 1.03 |
| metadata: total | 4.06 | 2.55 | 2.68 |
| bigdir: create 20,000 | 2.79 | 3.19 | 3.26 |
| bigdir: list | 0.093 | 0.100 | **0.014** (7× faster, streamed into the reply) |
| bigdir: delete-while-listing | 17.36 | 1.46 | 2.90 |
| bigdir: **`rm -rf` succeeds** | no (9,826 left) | no (9,826 left) | **yes (0 left)** |

Phase 6 delete-while-listing is slower than Phase C only because it now deletes all
20,000 entries instead of about half. The schema moves to version 3 (`dirents.id`
as the cookie; `dirents_name`, `dirents_child`, `dirents_scan` indexes); older
databases are rebuilt in place on first mount.

Docker compatibility matrix: PASS 13, FAIL 0, SKIP 3, XFAIL 0. S3 `rm -rf` of a
2,000-entry tree now passes, and `tests/compat/run.sh` reports any `rm -rf`
failure as FAIL instead of XFAIL.

## After Phases 2–5 and 7 (data path and schema)

Binaries `.bench/bin/dbfs-{phase6-0c40b3c,pr4,pr5,pr6,pr7}`, interleaved, 3 runs each
with `scripts/bench.sh` on `.bench/` (disk, not tmpfs), median:

| Workload | Phase 6 | PR 4 (2+3) | PR 5 (4) | PR 6 (5) | PR 7 (7) | Phase 6 → PR 7 |
|---|---|---|---|---|---|---|
| seqwrite (MiB/s) | 184.7 | 176.6 | 173.7 | 184.4 | **204.3** | +11% |
| randwrite (MiB/s) | 21.3 | 22.1 | 21.1 | 23.3 | **24.8** | +16% |
| seqread (MiB/s) | 1,168 | 1,146 | 1,288 | 1,369 | **1,452** | +24% |
| randread (MiB/s) | 81.8 | 86.8 | 95.6 | 95.8 | **98.4** | +20% |
| metadata total (s) | 2.62 | 2.60 | 2.54 | 2.60 | 2.51 | −4% |
| metadata: unlink 5,000 (s) | 1.04 | 1.03 | 0.98 | 1.06 | 0.92 | −12% |
| bigdir total (s) | 6.15 | 5.99 | 5.96 | 6.13 | 6.19 | ≈ |
| bigdir: list 20,000 (s) | 0.014 | 0.014 | 0.014 | 0.013 | 0.013 | ≈ |
| bigdir: `rm -rf` succeeds | yes | yes | yes | yes | yes | |

fio, 2 runs each (Phase 6 / PR 7):

| Job | Phase 6 | PR 7 | Change |
|---|---|---|---|
| randwrite 4k, no fsync (MiB/s) | 21.9 / 21.9 | 25.4 / 25.4 | +16% |
| seqwrite 128k (MiB/s) | 188 / 204 | 217 / 215 | +10% |
| randread 4k (MiB/s) | 62.2 / 64.2 | 73.4 / 73.3 | +16% |
| randwrite 4k, fsync (IOPS) | 203 / 210 | 214 / 210 | disk-bound, ≈ |

Where the gains come from:

- **Reads.** These improve in PR 5 (`fh` indexes the open table, and the read path
  makes no inode query) and in PR 7 (one range statement over the integer chunk keys).
- **Writes.** These improve in PR 6 (shared arena, no allocation per chunk) and in
  PR 7 (a rowid upsert instead of a composite-key B-tree plus its index). PR 4 and
  PR 5 alone sit within noise of Phase 6 on writes.
- **The C1 locking cost** recorded under Phase C (5–8%) is recovered and exceeded.
- **Metadata and directory workloads** are unchanged, as expected: these phases did
  not touch them.

Migration from version 3: a 978 MiB database (11 files) written by `dbfs-pr6` was
migrated by `dbfs-pr7` in 20.8 s.

- Every file had an identical sha256 and identical metadata afterwards.
- `PRAGMA integrity_check` returned ok, with 0 foreign-key violations.
- The database file grew to 2.0 GB, because the freed pages stay on the freelist
  until a `VACUUM`.

Docker compatibility matrix on `3c3a1bc`: PASS 13, FAIL 0, SKIP 3, XFAIL 0. S6
(`docker stop` with 16 MiB unflushed) exits 0 after 1.29 s and the data is restored
byte for byte. The image is 18.67 MB. The log is in `.bench/compat-final.log`.

## dbfs Compared with ext4 and btrfs

This compares dbfs at `3c3a1bc` (`.bench/bin/dbfs-pr7`) with three kernel
filesystems:

- **ext4 (native):** the host's ext4 on NVMe/LVM. dbfs keeps its SQLite database
  on this same filesystem.
- **ext4 (loop):** a 16 GiB image file on that disk, attached with `losetup
  --direct-io=on`, freshly formatted for each run.
- **btrfs (loop):** the same setup as ext4 (loop), with btrfs and its default
  options (`ssd`, `discard=async`, `space_cache=v2`).

sudo needs a password on this machine, so btrfs can't be mounted on the host. The
two loop filesystems are mounted inside a privileged container
(`docker run --privileged -v /:/host alpine chroot /host`) and benchmarked with the
same host binaries. The ext4 loop run measures what the loop device itself costs,
so compare btrfs with ext4 (loop), not with native ext4.

Each filesystem ran 3 interleaved rounds of `bench_io`; the medians are below. fio
ran 2 rounds of the same jobs used for the A/B runs. The driver script and raw
results are in `.bench/fscmp/`.

| Workload | dbfs | ext4 | ext4 (loop) | btrfs (loop) |
|---|---|---|---|---|
| seqwrite (MiB/s) | 206 | 4,110 | 3,148 | 1,490 |
| randwrite (MiB/s) | 25.6 | 996 | 985 | 188 |
| seqread (MiB/s) | 1,291 | 10,430 | 10,090 | 7,899 |
| randread (MiB/s) | 101 | 3,097 | 3,056 | 2,758 |
| metadata total (s) | 2.48 | 0.164 | 0.156 | 0.349 |
| metadata: create 5,000 (s) | 1.31 | 0.090 | 0.085 | 0.182 |
| metadata: unlink 5,000 (s) | 0.98 | 0.050 | 0.047 | 0.146 |
| bigdir total (s) | 6.00 | 0.729 | 0.676 | 0.730 |
| bigdir: list 20,000 (s) | 0.0145 | 0.0068 | 0.0068 | 0.0048 |
| bigdir: `rm -rf` 20,000 (s) | 2.75 | 0.225 | 0.206 | 0.410 |

| fio job (2 runs) | dbfs | ext4 | ext4 (loop) | btrfs (loop) |
|---|---|---|---|---|
| randwrite 4k, no fsync (MiB/s) | 23.2 / 25.5 | 905 / 905 | 1,024 / 938 | 228 / 241 |
| randwrite 4k, fsync each write (IOPS) | 215 / 207 | 206 / 204 | 93 / 94 | 83 / 84 |
| randread 4k (MiB/s) | 72.0 / 73.5 | 38.5 / 38.9 | 33.1 / 34.3 | 31.9 / 32.8 |
| seqwrite 128k (MiB/s) | 205 / 221 | 4,571 / 3,969 | 4,830 / 4,741 | 1,373 / 1,395 |

How to read these numbers:

- **Durable writes.** With an fsync after every write, dbfs matches native ext4
  (about 210 IOPS each). Both are bound by the disk's flush latency, and a dbfs
  fsync is one `fdatasync` of the WAL on that same ext4.
- **Buffered I/O.** Without fsync, ext4 and btrfs write into the kernel page cache
  and read from it. They are 10–40× faster than dbfs, whose every operation is a
  FUSE round trip into a single-threaded user-space process, followed by SQLite.
- **Metadata.** Operations are 10–15× slower than ext4 and 4–7× slower than btrfs.
  Each create or unlink is its own SQLite transaction.
- **The fio randread result is not a dbfs win.** Before each job, fio drops the
  page cache of the file it is about to test. On ext4 and btrfs, that forces cold
  4 KiB reads from the NVMe disk. Under dbfs, it only drops the FUSE cache, while
  SQLite's database file stays in the host page cache. The `bench_io` randread row,
  where all four filesystems read with warm caches, is the like-for-like number.
- **Loop overhead.** The loop device costs little for buffered I/O. It halves
  durable-write IOPS (93 against 206), because each flush passes through two
  filesystems.
- **btrfs against ext4, both on loop.** btrfs is slower at buffered writes and
  file creation, which is copy-on-write and checksum cost. It is faster at mkdir,
  directory listing and creating files in a large directory.

All four filesystems completed `rm -rf` of the 20,000-entry directory in every
round.

## Like-for-Like: Every Filesystem in One Container, with FUSE Pass-Through

The comparison above measured dbfs through FUSE against ext4 and btrfs without
FUSE. So the gap mixed what FUSE itself costs with what dbfs's design costs.
This run adds libfuse's `passthrough_hp`, which separates the two:

- **ext4 → passthrough_hp over ext4** is the cost of FUSE itself.
- **passthrough_hp → dbfs** is the cost of dbfs's design.

Everything ran from one privileged container. The script is
`scripts/bench-docker/compare.sh`, the image is `scripts/bench-docker/Dockerfile`
(Ubuntu 26.04 with libfuse 3.18.2 built with liburing), and the kernel is
7.0.0-38. Every filesystem used the same NVMe disk.

- **ext4:** the host filesystem under `.bench/`.
- **btrfs:** a 24 GiB loop image on that disk (`losetup --direct-io=on`). There is
  no ext4 loop control in this run, so the btrfs-backed rows include the loop
  device's cost.
- **FUSE speed-ups.** Every FUSE mount got every speed-up available to it.

| Config | Speed-ups |
|---|---|
| `pt` | `passthrough_hp`: kernel FUSE passthrough for reads and writes, multithreaded, `--clone-fd`, splice, FUSE over io_uring, writeback cache, 1-day TTLs, keep_cache, 4 MiB requests |
| `pt-user` | The same, with `--nopassthrough`, so data goes through user space as it does in dbfs |
| `dbfs` | dbfs defaults |
| `dbfs-kc` | dbfs `--kernel-cache` (new): writeback cache, parallel dirops, 1 MiB requests, cached symlinks, 1-day TTLs, keep_cache. fuser 0.16 can't run multithreaded, and doesn't support io_uring or kernel passthrough. |

The script switched FUSE over io_uring on for the run
(`/sys/module/fuse/parameters/enable_uring=Y`) and back off afterwards. Each
config ran 3 rounds of `bench_io`, interleaved, with a fresh mount for each
workload; the medians are below. fio ran 2 rounds.

The dbfs columns for ext4 and btrfs say where dbfs keeps its database.

| Workload | ext4 | pt ext4 | pt-user ext4 | dbfs ext4 | dbfs-kc ext4 | btrfs | pt btrfs | pt-user btrfs | dbfs btrfs | dbfs-kc btrfs |
|---|---|---|---|---|---|---|---|---|---|---|
| seqwrite (MiB/s) | 4,030 | 2,631 | 777 | 174 | 195 | 1,573 | 1,372 | 553 | 148 | 132 |
| randwrite (MiB/s) | 978 | 178 | 190 | 25.4 | 29.0 | 160 | 122 | 121 | 18.3 | 19.9 |
| seqread, warm (MiB/s) | 9,572 | 9,829 | 8,429 | 1,397 | **8,545** | 7,203 | 7,388 | 8,129 | 1,424 | **8,080** |
| randread, warm (MiB/s) | 2,754 | 2,629 | 2,912 | 105 | **2,959** | 2,647 | 2,542 | 2,856 | 106 | **2,926** |
| metadata total (s) | 0.18 | 1.01 | 1.14 | 2.39 | 2.50 | 0.40 | 1.17 | 1.27 | 3.00 | 3.38 |
| metadata: create 5,000 (s) | 0.098 | 0.663 | 0.678 | 1.25 | 1.42 | 0.206 | 0.745 | 0.726 | 1.60 | 1.84 |
| metadata: unlink 5,000 (s) | 0.056 | 0.198 | 0.302 | 0.929 | 0.947 | 0.164 | 0.289 | 0.405 | 1.19 | 1.33 |
| bigdir total (s) | 0.74 | 2.99 | 3.27 | 5.52 | 5.50 | 0.82 | 3.02 | 3.18 | 6.92 | 6.80 |
| bigdir: `rm -rf` 20,000 (s) | 0.23 | 0.88 | 1.33 | 2.57 | 2.52 | 0.44 | 1.01 | 1.49 | 3.18 | 3.20 |

| fio (2 runs) | ext4 | pt ext4 | pt-user ext4 | dbfs ext4 | dbfs-kc ext4 | btrfs | pt btrfs | pt-user btrfs | dbfs btrfs | dbfs-kc btrfs |
|---|---|---|---|---|---|---|---|---|---|---|
| randwrite 4k, no fsync (MiB/s) | 977 / 908 | 167 / 175 | 169 / 170 | 10.1 / 27.1 | 30.5 / 29.8 | 280 / 178 | 142 / 134 | 142 / 137 | 19.9 / 12.1 | 13.4 / 22.6 |
| randwrite 4k, fsync each write (IOPS) | 206 / 205 | 189 / 190 | 180 / 178 | 202 / 203 | 207 / 195 | 87 / 87 | 80 / 80 | 79 / 79 | 78 / 67 | 65 / 76 |
| randread 4k, all caches dropped (MiB/s) | 36.6 / 35.8 | 65.2 / 66.9 | 55.5 / 57.8 | 47.9 / 45.9 | 45.6 / 46.3 | 31.2 / 30.7 | 70.6 / 78.1 | 63.6 / 65.8 | 62.1 / 66.4 | 67.5 / 53.0 |
| seqwrite 128k (MiB/s) | 3,684 / 4,697 | 2,994 / 2,133 | 760 / 694 | 203 / 218 | 187 / 195 | 1,340 / 1,344 | 1,138 / 1,138 | 535 / 514 | 167 / 172 | 149 / 147 |

Every config completed `rm -rf` of the 20,000-entry directory in every round.

### What FUSE costs (ext4 → pt over ext4)

- **Warm reads.** Free once the kernel caches pages (`keep_cache`).
- **Sequential writes.** About 35% with kernel passthrough. About 80% when data
  goes through user space.
- **4 KiB random writes.** 5.5×. Kernel passthrough doesn't help here, because
  every write is still a FUSE request.
- **Metadata.** 4–5×: every create, stat and unlink is a FUSE round trip.

### What dbfs's design costs (pt-user → dbfs, the same FUSE data path)

- **Reads with `--kernel-cache`.** On par with `passthrough_hp` (8.5 against
  8.4 GiB/s sequential; 2.96 against 2.91 GiB/s random). Without the flag,
  dbfs drops its page cache on every open, and warm reads are 6× (sequential)
  to 28× (random) slower.
- **Writes.** 4× slower sequentially and 6–7× slower for 4 KiB random writes. The
  bottleneck is dbfs's own write path: a single thread, one lock per request, a
  copy into the arena, and SQLite chunk upserts on flush. The number of requests
  is not the limit: `--kernel-cache` raises the request size to 1 MiB and adds
  the writeback cache, yet random writes improve only 14% and sequential writes
  not at all.
- **Metadata.** About 2× slower. Each create or unlink is a SQLite transaction on
  one thread, while `passthrough_hp` runs multithreaded system calls on ext4.
- **Durable writes.** At the disk's limit, like everything else: about 200 IOPS
  on ext4, matching both ext4 and `passthrough_hp`.

### Not like for like even here

- **Cold random reads.** Both FUSE stacks beat native ext4. Each layer reads more
  than fio asks for: FUSE readahead, and dbfs's whole 64 KiB chunks. Later
  random reads then hit pages that are already cached. Native ext4 detects the
  random pattern and reads 4 KiB only. Treat this row as how each stack behaves,
  not as a ranking.
- **Loop device cost.** The btrfs-backed numbers include it. Earlier runs measured
  it as halving fsync IOPS.
