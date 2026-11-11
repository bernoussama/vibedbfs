# Performance results, November 2026

This page records two measurement campaigns on the development VM. Part A tuned the WAL reset threshold (commit a1c3789). Part B measured 4 KiB and 16 KiB SQLite pages for new databases (commit f3375f8). The command that changes the page size of an existing database is in d9685bd. Tables give the median, then the min–max range across rounds. With three to five rounds, a difference under about 10% is not established.

## Summary

- **Part A.** `FORCED_MULTIPLE` moves from 3 to 4, so a forced reset starts at 256 MiB of WAL instead of 192 MiB. Of the four settings, four times gave the highest 512 MiB sequential write median, 305.6 MiB/s, and its WAL peaked at 292 MiB or less in those runs. The 3 GiB medians of all four settings overlap (83–101 MiB/s), so they did not decide the choice.
- **Part B.** New databases get 16 KiB pages. The 3 GiB sequential write median is 3.0 times the 4 KiB one, with ranges that do not overlap. Random 4 KiB writes are 11% faster with non-overlapping ranges. The 512 MiB sequential median is 37% faster, but its ranges overlap. Cold random reads, fsync-per-write, metadata, bigdir and the small-file trees are within noise. The cost is space: the small-file databases are 18–22% larger. Existing databases keep their page size, and `dbfs migrate --page-size` converts them.
- **Target not met.** No median reached the 420 MiB/s sequential write target in the task brief. The best median was 399 MiB/s (16 KiB, 512 MiB writes), and the best single round was 479 MiB/s.
- **Open.** In 2 of 11 measured 3 GiB runs at the default setting, the WAL reached the 1 GiB `--wal-cap` (1027 and 1039 MiB), so the backstop checkpointed it. The cause is not known. The README and architecture text, which said the WAL stays far below the cap, was corrected in the commit that adds this page.

## Method

Workloads, from `examples/bench_io.rs`:

| Workload | What it measures |
| --- | --- |
| `seqwrite` | 512 MiB written sequentially in 128 KiB blocks |
| `seqwrite-long` | 3 GiB written the same way |
| `randwrite` | 256 MiB of 4 KiB random writes, no fsync |
| `randwrite-fsync` | 64 MiB of 4 KiB random writes, an fsync after each |
| `randread-cold` | 4 KiB random reads of 256 MiB on a cold cache, default mount and `--no-keep-cache` |
| `metadata` | 5,000 files of 4 KiB in 200 directories (create and unlink stages) |
| `bigdir` | 20,000 files in one directory (create, list and remove stages) |
| `smallfiles` | 20,000 files of 1–16 KiB in a 20 × 20 × 50 tree, each closed after its write |
| `tree-cold` | the same tree, a cache drop, then stat (`tree-cold-stat`) and remove (`tree-cold-rm`) |

Each workload ran on a fresh database, on a new FUSE mount started with `DBFS_STATS=1`. The mount was unmounted between workloads. The WAL peak is `wal.high_water_bytes` from the `--stats` line at unmount, sampled after each flush transaction and at report. The database size is the file size after the workload.

Cold reads: after a sync, `posix_fadvise(POSIX_FADV_DONTNEED)` ran on the database, `-wal` and `-shm` files, and the mount was restarted. `drop_caches` needs root, which was not available.

Rounds. Part A: five rounds for `fm3-p64` and `fm4-p64`, three for `fm6-p64` and `fm3-p128`, with the order alternated between rounds. One untimed warm-up ran first, because the first run after a build measured about a third of the steady rate. Part B: three rounds per variant, with the 4 KiB and 16 KiB order alternated (4 KiB first in rounds 1 and 3, 16 KiB first in round 2). There was no warm-up in Part B.

The raw JSON lines were in the worktree's `.bench/agent-5/`, which is removed after this page is written. The tables here are the record.

## Machine

- CPU: Intel Core i5-7200U at 2.50 GHz, 4 logical CPUs.
- Memory: 7.0 GiB RAM and 8 GiB swap. About 3.9 GiB of swap was in use by other processes at the time of the check.
- Disk: NVMe (`nvme0n1`) under LVM ext4, the root filesystem.
- Software: kernel 7.0.0-38-generic, rustc and cargo 1.99.1, SQLite 3.46.1 (system `libsqlite3`).
- Binaries (first 12 hex digits of SHA-256): `fm3-p64` 117e3d010c8e, `fm4-p64` 53b85ebab546, `fm6-p64` a3afc00d2faa, `fm3-p128` ac73f5c7eedc, Part B 4 KiB f7d3af91cdfd, Part B 16 KiB 905ebf9e2042. The Part A builds used the a1c3789 source with the constants changed. The Part B 4 KiB build is the a1c3789 source unchanged. The Part B 16 KiB build adds `PRAGMA page_size = 16384` before the WAL switch.

## Part A: checkpoint tuning

Four settings were compared. `forced_at` is `min(FORCED_MULTIPLE × passive_at, 3/4 × 1 GiB)`.

| Variant | `FORCED_MULTIPLE` | `passive_at` | `forced_at` | Rounds |
| --- | --- | --- | --- | --- |
| `fm3-p64` | 3 | 64 MiB | 192 MiB | 5 |
| `fm4-p64` (chosen) | 4 | 64 MiB | 256 MiB | 5 |
| `fm6-p64` | 6 | 64 MiB | 384 MiB | 3 |
| `fm3-p128` | 3 | 128 MiB | 384 MiB | 3 |

Sequential 512 MiB writes (`seqwrite`), MiB/s:

| Variant | Median | Range | Rounds, in order | WAL peak per round (MiB) |
| --- | --- | --- | --- | --- |
| `fm3-p64` | 260.1 | 163.8–347.1 | 269.1, 163.8, 243.3, 260.1, 347.1 | 243, 308, 260, 292, 518 |
| `fm4-p64` | **305.6** | 297.0–307.6 | 307.6, 307.6, 301.6, 297.0, 305.6 | 292, 292, 260, 276, 260 |
| `fm6-p64` | 285.8 | 47.4–289.4 | 289.4, 47.4, 285.8 | 373, 438, 390 |
| `fm3-p128` | 291.0 | 274.8–348.9 | 291.0, 274.8, 348.9 | 518, 438, 518 |

Sequential 3 GiB writes (`seqwrite-long`), MiB/s:

| Variant | Median | Range | Rounds, in order | WAL peak per round (MiB) |
| --- | --- | --- | --- | --- |
| `fm3-p64` | 90.2 | 59.7–152.4 | 111.6, 59.7, 81.7, 152.4, 90.2 | 243, 503, **1039**, 325, **1039** |
| `fm4-p64` | 83.0 | 56.8–99.2 | 98.8, 68.7, 56.8, 99.2, 83.0 | 308, **730**, 341, 503, 373 |
| `fm6-p64` | 96.9 | 77.5–113.7 | 113.7, 77.5, 96.9 | 761, 682, 373 |
| `fm3-p128` | 101.1 | 41.5–113.8 | 41.5, 101.1, 113.8 | **1039**, 438, 422 |

Bold WAL peaks are at or near the 1 GiB cap. The cap hits were 3 of 8 runs at the three-times setting and none at six times. At four times, Part A had none in 5 runs, and the page-size builds below, which use the same reset code, had 2 of 6.

**Decision.** The rule was the highest 512 MiB sequential throughput with a WAL peak of about 512 MiB or less. Four times meets it: 305.6 MiB/s median, peaks 260–292 MiB. Three times at 64 MiB had a 260 MiB/s median and a peak of 518 MiB. Six times had an outlier round at 47 MiB/s and peaks to 438 MiB. Three times at 128 MiB reached 518 MiB. In the 3 GiB runs, four times had the lowest median (83 MiB/s) and one run peaked at 730 MiB, above the rule's bound. The 3 GiB medians do not separate the settings, so the choice rests on the 512 MiB runs. This is commit a1c3789.

## Part B: page size for new databases

Both variants run the same code. The 16 KiB variant sets `PRAGMA page_size = 16384` on an empty file before `journal_mode = WAL`. Ratios are 16 KiB over 4 KiB medians. For MiB/s, a ratio above 1 is faster. For seconds, a ratio below 1 is faster.

| Workload | Metric | 4 KiB median (range) | 16 KiB median (range) | Ratio | Database, 4 KiB → 16 KiB (MiB) |
| --- | --- | --- | --- | --- | --- |
| `seqwrite` 512 MiB | MiB/s | 291.4 (229.4–350.2) | 399.0 (307.7–479.4) | 1.37 | 516 → 528 |
| `seqwrite-long` 3 GiB | MiB/s | 60.2 (54.4–61.9) | 181.9 (111.7–237.9) | 3.02 | 3096 → 3168 |
| `randwrite` 256 MiB | MiB/s | 151.2 (149.3–153.2) | 167.8 (160.0–174.2) | 1.11 | 258 → 264 |
| `randread-cold` 256 MiB | MiB/s | 100.5 (25.2–103.5) | 99.3 (33.8–112.6) | 0.99 | 258 → 264 |
| `randread-cold`, `--no-keep-cache` | MiB/s | 39.1 (36.7–109.1) | 38.9 (30.0–47.7) | 0.99 | 258 → 264 |
| `randwrite-fsync` 64 MiB | seconds | 73.97 (72.15–80.35) | 73.48 (72.39–74.99) | 0.99 | 65 → 76 |
| `metadata` 5,000 files | seconds | 1.32 (1.25–1.36) | 1.32 (1.29–1.33) | 1.00 | 23 → 27 |
| `bigdir` 20,000 files | seconds | 3.33 (3.24–3.48) | 3.43 (3.41–3.45) | 1.03 | 2.3 → 2.4 |
| `tree-cold-stat` 20,000 files | seconds | 0.82 (0.69–0.98) | 0.76 (0.74–0.78) | 0.93 | 185 → 226 |
| `tree-cold-rm` 20,000 files | seconds | 2.23 (1.99–5.90) | 2.14 (2.05–5.38) | 0.96 | 185 → 226 |
| `smallfiles` 20,000 files | seconds | 3.88 (3.87–4.76) | 3.98 (3.96–4.76) | 1.03 | 185 → 226 |

`smallfiles` throughput was 42.7 MiB/s (34.8–42.9) at 4 KiB and 41.6 MiB/s (34.9–41.8) at 16 KiB. The database sizes are medians of the three rounds.

WAL peak per round (MiB), for the workloads that write:

| Workload | 4 KiB | 16 KiB |
| --- | --- | --- |
| `seqwrite` | 518, 518, 292 | 265, 315, **530** |
| `seqwrite-long` | 292, **1039**, 292 | 331, 331, **1027** |
| `randwrite` | 258, 258, 260 | 263, 265, 263 |
| `randwrite-fsync` | 65, 65, 65 | 65, 65, 65 |

**Decision.** The rule was: if 16 KiB wins, new databases get 16 KiB, and an existing database keeps its page size. 16 KiB wins clearly on the 3 GiB sequential write, and on random writes by 11%. It probably wins on the 512 MiB sequential write, where the ranges overlap. It does not lose on the other workloads beyond three-round noise. The cost is space: the metadata and small-file databases are 18–22% larger, and the fsync workload is 17% larger. Commit f3375f8 makes the change for new databases. Commit d9685bd adds `dbfs migrate --page-size` for existing ones.

## Part D: before and after, Docker comparison

`scripts/bench-docker/compare.sh --configs "ext4 pt-ext4-user dbfs-ext4 dbfs-ext4-kc" --rounds 3 --fio-rounds 2` on 2026-11-11, git c8ec73c, kernel 7.0.0-38. Results in `.bench/fscmp2/final-2026-11-11/`. Medians of 3 bench_io rounds (fio: mean of 2). "Before" is the same harness on the branch before this work (dbfs default options, 4 KiB pages). `pt-ext4-user` is libfuse `passthrough_hp --nopassthrough`, the cost of FUSE itself with no storage engine.

| Workload | dbfs before | dbfs now | passthrough_hp | ext4 |
| --- | --- | --- | --- | --- |
| Sequential write, 512 MiB (MiB/s) | 200 | 379 | 763 | 4,021 |
| Random 4 KiB write, 256 MiB (MiB/s) | 25.4 | 189 | 199 | 976 |
| Random 4 KiB write, fio (MiB/s) | – | 218 | 180 | 1,034 |
| Random 4 KiB write + fsync, fio (IOPS) | 212 | 217 | 176 | 207 |
| Sequential read, warm (MiB/s) | 1,456 | 7,160 | 7,534 | 10,656 |
| Random 4 KiB read, warm (MiB/s) | 105 | 2,635 | 2,888 | 3,009 |
| Create 5,000 files (s) | 1.24 | 0.62 | 0.80 | 0.10 |
| Unlink 5,000 files (s) | 0.95 | 0.29 | 0.30 | 0.06 |
| `rm -rf` 20,000 entries (s) | 2.66 | 0.98 | 1.28 | 0.23 |
| 20,000 small files (s) | – | 3.30 | 3.27 | 0.52 |
| Cold tree stat / rm, 20,000 (s) | – | 0.59 / 1.79 | 0.36 / 1.21 | 0.15 / 0.25 |

- dbfs now matches or beats the FUSE passthrough on random writes, fsync, warm reads and metadata. Sequential writes are about half of passthrough: every byte is written twice (WAL, then checkpoint).
- `seqwrite-long` (3 GiB) is bimodal: 355–375 MiB/s in one run, 100–225 MiB/s in a later one. It writes about 6 GiB to a disk that sustains about 256 MB/s with `fdatasync` on a 7 GiB machine, so it depends on how much the host page cache absorbs.
- Cold reads (`randread-cold`, fio `randread-cold`) show dbfs ahead of ext4. That is an artifact: the native target is read from the device after the drop, while the dbfs database file stays partly in the host page cache. Do not read it as dbfs being faster.
- `--kernel-cache` random writes were 21.6 MiB/s in this run: timestamp-only `setattr` calls from the kernel's writeback flushed the whole file each time. Fixed in bf096c2 (`setattr` flushes only on a size change). After the fix (`.bench/fscmp2/kcfix/`): randwrite 291 MiB/s with `--kernel-cache` vs 199 without; fio random write 434 vs 212.

## Caveats

- **Few rounds.** Three rounds for Part B, and three or five for Part A. Overlapping ranges mean the differences under about 10% are not established.
- **Cold reads.** The cold cache came from `posix_fadvise` on the database files, not `drop_caches`. `randread-cold` ranged from 25 to 113 MiB/s in both variants, so its cold figures carry little information.
- **Swap and memory.** The VM had 7.0 GiB RAM and 3.9 GiB of swap in use by other processes. The 3 GiB runs are larger than the RAM available for page cache. The effect on the variance was not measured.
- **WAL cap hits.** 2 of 11 3 GiB runs at four times reached the 1 GiB cap. A hypothesis, that the handshake's early return starves the forced lock path, was tested with two traced 3 GiB runs. Those runs had no early returns, so the hypothesis is not supported by them. The cause is not known.
- **Target.** The best median was 399 MiB/s, below the 420 MiB/s target in the task brief.
- **Space.** The space cost was measured on these synthetic trees. Real trees with many small files should be measured separately.
- **Migration growth.** A copy at 16 KiB can be larger than the 4 KiB original. `dbfs migrate` checks free space against the database plus its WAL, which does not cover that growth. If the copy runs out of space, SQLite reports a full disk, and the original is unchanged.
- **Test race.** `an_idle_database_resets_its_wal` failed 2 of 18 runs at four times. It read the WAL size before the group-commit timer had committed the burst. Commit 2d96bb8 reads the size after the idle wait. The assertion is unchanged.

## Commits

- a1c3789 Force the reset handshake at 4 passive thresholds (256 MiB), not 3 (192 MiB)
- 2d96bb8 Read the idle WAL size after the idle wait in an_idle_database_resets_its_wal
- f3375f8 Create new database files with 16 KiB pages
- d9685bd Add dbfs migrate --page-size to rewrite a database with another page size
