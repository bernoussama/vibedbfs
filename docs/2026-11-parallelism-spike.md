# Parallelism spike (Step 6)

Date: 2026-11-11. Base: `7316305`. Branch: `worktree-agent-a8eb999849494b739`.

Scope: an assessment with measurements. No code is committed. The prototype was not built; section 3 explains why. `Cargo.toml`, `Cargo.lock`, `src/` and `tests/` are unchanged.

Environment: a 4 vCPU VM with an ext4 root, shared with other agents. Load average was 2.5 to 5.5 during the final runs. Linux 7.0.0-38-generic, fio 3.41, rustc 1.99.1. `drop_caches` needs root, so the cold-read setup in section 2 uses `posix_fadvise(DONTNEED)` instead.

## Summary

- The FUSE session runs on one thread, and most handler time is SQLite, which runs under one connection mutex. At 4 jobs the session thread is 91 to 98% busy for randread, randwrite and creates. Going from 1 to 4 jobs gains dbfs 1.0 to 1.8x (creates 1.3x), while ext4 gains 2 to 5x.
- fuser 0.16, which dbfs uses, cannot run a second request loop. Its only extra-fd entry point, `Session::from_fd`, starts uninitialized and rejects every request with EIO. Multi-threaded sessions exist from fuser 0.17 on (`Config::n_threads` with `clone_fd`), and that is a breaking API change: a scratch port of the current code to 0.17 gives 107 compile errors.
- No fuser version implements FUSE-over-io_uring. The kernel supports it (`CONFIG_FUSE_IO_URING=y`) but leaves it off (`enable_uring=N`). Passthrough does not apply, because dbfs has no host file behind each inode.
- Under the current design, SQLite is 55 to 76% of a 4 KiB read handler and about 88% of a create. Threads can overlap only the rest, so the ceiling is about 1.3 to 1.8x for reads and 1.14x for creates.
- A shared connection gives a flat SQLite read rate from 1 to 4 threads (medians 67k and 66k reads/s). One read-only connection per thread reaches a median of 116k at 4 threads, about 1.8x, with a wide run-to-run spread. That helps only with concurrent dispatch and a lock split for clean data, and it touches `db.rs`, where group commit is in progress.
- Recommendation: do not add `--threads` now. Land group commit (Step 3), harden the unlink path and state the read-by-inode invariant (section 4), then take a reader pool and a fuser 0.17 port as separate steps (section 5).

## 1. fuser versions

Versions 0.17.0 and 0.18.0 were downloaded into the session scratchpad and read directly. 0.16.0 was read from `~/.cargo/registry`. Line numbers are in the fuser crate sources, not in dbfs.

| | 0.16.0 (current) | 0.17.0 | 0.18.0 |
|---|---|---|---|
| Request loop | One thread: `Session::run(&mut self)` (`session.rs:145`) | `Config::n_threads` loop threads (`session.rs:254`). `Session::run` is private, which is the E0624 error in the port | `Session::run(self)` is public (`session.rs:246`); same threaded loop |
| Clone fd (`FUSE_DEV_IOC_CLONE`) | Not exposed | `Config::clone_fd` (`session.rs:272-275`, `channel.rs:67`) | Same (`session.rs:275`) |
| Session created from an extra fd | `Session::from_fd` sets `initialized = false`; requests get EIO (`session.rs:136`, `request.rs:157-160`) | Clones are created inside `run` (`session.rs:272-275`) | Same |
| `Filesystem` trait | `&mut self`, no `Send`/`Sync` bound (`lib.rs:314`) | `&self`, `Send + Sync + 'static` (`lib.rs:401`) | Same (`lib.rs:400`) |
| Types | Plain `u64` / `i32` | Newtypes such as `INodeNo` and `Errno` (`lib.rs:35`, `lib.rs:47`) | Same |
| Mount call | `mount2(fs, mnt, &[MountOption])` (`lib.rs:969`) | `mount2(fs, mnt, &Config)` (`lib.rs:1052`) | `mount(fs, mnt, &Config)` (`lib.rs:1051`); `mount2` removed |
| `abi-*` features | 15 declared; dbfs uses `abi-7-28` | 14 declared; the scratch port resolves `abi-7-28` | None declared, so dbfs's `abi-7-28` would not resolve (not built) |
| FUSE-over-io_uring | None | Init flag `FUSE_OVER_IO_URING` (`init_flags.rs:92`), no transport | Same (`init_flags.rs:92`) |
| Passthrough | `BackingId` (`passthrough.rs:47`), with `abi-7-40` | `BackingId` (`passthrough.rs:31`) | `BackingId` (`passthrough.rs:31`) |
| Async API | None | Experimental `AsyncFilesystem` behind feature `experimental` (`experimental.rs`, tokio adapter) | Same, behind `experimental` |

Notes:

- dbfs's manifest: `fuser = "0.16.0"` with `abi-7-28`, `rusqlite` with `blob`, and a `bundled-sqlite` feature.
- Passthrough does not apply. It hands the kernel a host fd per open file, and dbfs has none, since file data lives in `file_chunks` rows.
- io_uring check: `uname -r` gives `7.0.0-38-generic`. `/boot/config-7.0.0-38-generic` has `CONFIG_FUSE_FS=y`, `CONFIG_FUSE_PASSTHROUGH=y` and `CONFIG_FUSE_IO_URING=y`. `/sys/module/fuse/parameters/enable_uring` is `N`. The kernel path exists and is off by default. No crate implements the userspace side, so it would be custom transport code.
- Migration to 0.17 (measured): a copy of the current `src/` with the manifest changed to `fuser = "=0.17.0"` and the same features, then `cargo check --lib`. Result: `could not compile dbfs (lib) due to 107 previous errors`. Most are in `src/fuse.rs` (the `Filesystem` impl). The rest are in `src/run.rs` (`MountOption::AllowOther`/`AllowRoot`, and the private `Session::run`) and `src/open_table.rs`. Four `fuser::consts` names no longer resolve in 0.17 (`FUSE_ASYNC_DIO`, `FUSE_CACHE_SYMLINKS`, `FUSE_PARALLEL_DIROPS`, `FUSE_WRITEBACK_CACHE`), and so does `FOPEN_KEEP_CACHE`. The port does not count the semantic work: `Dbfs` must be `Sync`, and the ordering rules in section 4 apply. 0.18 was not ported. Its manifest declares no `abi-*` features, so dbfs's `abi-7-28` would not resolve there.

## 2. Measured serialization

Method:

- Each run used a fresh database, mounted with `dbfs mount <db> <mnt> --stats` and default options. fio 3.41 with `ioengine=sync`. ext4 runs used a directory under `.bench/agent-6/run` on the same root filesystem.
- randread: four 64 MiB files laid out by fio (1 MiB writes, `end_fsync=1`). For dbfs, a clean unmount (which checkpoints the WAL), then `posix_fadvise(DONTNEED)` on `db.sqlite`, `-wal` and `-shm`, then a remount. For ext4, `DONTNEED` on the data files. Timed: 4 KiB `--direct=1` random reads, `numjobs` 1 or 4, 1 s ramp, 10 s measured.
- Cache caveat: ext4 direct reads go to the device (mean latency 92 to 105 us at 1 job). dbfs reads go through the database page cache, which warms during the run. An earlier pass without the drop read database pages from the cache: dbfs was 25 to 27k IOPS at 1 job and 29 to 48k at 4 jobs, close to the cold runs. That pass was discarded. The dbfs numbers are therefore CPU and lock bound, and the dbfs-to-ext4 IOPS comparison is not a device comparison. The scaling ratios (4 jobs over 1 job) are what this spike uses.
- randwrite: 4 KiB buffered random writes into new files, 64 MiB per job, `end_fsync=0`. The files are new, so writes do not overwrite existing chunk data.
- creates: a loop of 5,000 creates per process, each process in its own directory. 1 or 4 processes, timed from the first start to the last finish.
- Session busy: the sum of handler times from `--stats`, divided by elapsed time. Elapsed is 10 s for randread (the stats window matched fio IOPS × 10 s within 1% at 4 jobs), ops divided by fio IOPS for randwrite, and the creates wall time. Handler time includes reply sends and lock waits, and excludes time when the loop is idle in `read()`.
- Two repetitions each.

Table 1: randread, 4 KiB direct, after a cache drop, 10 s measured (rep 1 / rep 2)

| backend | jobs | IOPS | MiB/s | session busy |
|---|---|---|---|---|
| dbfs | 1 | 30,379 / 31,711 | 118.7 / 123.9 | 67% / 56% |
| dbfs | 4 | 35,745 / 58,130 | 139.6 / 227.1 | 96% / 98% |
| ext4 | 1 | 10,272 / 9,272 | 40.1 / 36.2 | n/a |
| ext4 | 4 | 43,468 / 47,642 | 169.8 / 186.1 | n/a |

Table 2: randwrite, 4 KiB buffered, 64 MiB per job, no fsync (rep 1 / rep 2)

| backend | jobs | IOPS | MiB/s | session busy |
|---|---|---|---|---|
| dbfs | 1 | 39,864 / 39,103 | 155.7 / 152.7 | 54% / 76% |
| dbfs | 4 | 39,033 / 59,796 | 152.5 / 233.6 | 95% / 95% |
| ext4 | 1 | 160,627 / 163,840 | 627.5 / 640.0 | n/a (page cache) |
| ext4 | 4 | 341,333 / 360,088 | 1333.3 / 1406.6 | n/a (page cache) |

Table 3: creates, 5,000 per process, each process in its own directory (rep 1 / rep 2)

| backend | processes | creates/s | wall | session busy |
|---|---|---|---|---|
| dbfs | 1 | 6,173 / 6,279 | 0.81 / 0.80 s | 63% / 64% |
| dbfs | 4 | 8,075 / 8,148 | 2.48 / 2.45 s | 91% / 91% |
| ext4 | 1 | 31,365 / 34,650 | 0.16 / 0.14 s | n/a |
| ext4 | 4 | 71,560 / 73,162 | 0.28 / 0.27 s | n/a |

Table 4: where the handler time goes (from `--stats`)

| workload | jobs | handler mean (rep 1 / rep 2) | SQLite share of the handler or op time |
|---|---|---|---|
| randread | 1 | 20.8 / 16.4 us | 55% / 69% (SQL read 11.4 / 11.3 us) |
| randread | 4 | 26.8 / 16.8 us | 70% / 76% (SQL read 18.9 / 12.7 us) |
| creates | 1 | create 75 / 71 us | 88% of op time in metadata SQL |
| creates | 4 | create 84 / 78 us | 87% of op time in metadata SQL |

The 1-process creates run (rep 1) recorded 5,000 creates, 5,001 lookups, 5,000 flushes and 5,000 releases, with 15,003 metadata SQL calls in total.

Reading the results:

- At 4 jobs the session loop is 91 to 98% busy. At 1 job it is 54 to 76% busy. Throughput rises little, because the loop is serialized and both locks sit on the request path. ext4 scales 2 to 5x on the same workloads.
- The locks: `read_slot` calls `read_into` while it holds the OpenTable mutex (`src/fuse.rs:213`, reached from `read_fh` at line 800). `flush_chunks` calls `replace_chunks` while it holds that mutex (`src/fuse.rs:133`). Every database call takes the one `Mutex<Connection>` (`src/db.rs:93`, locked in `write_conn` at line 209, `read_into` at line 640, and `sync_durable` at line 239), so SQL is serialized across all threads.
- `sync_durable` takes the connection mutex only to read the database path. The `fdatasync` runs outside it, so fsync does not hold up reads on the connection.
- Caveat: at 4 jobs fio uses most of the 4 vCPUs, and another agent's job used about one CPU. dbfs at 4 jobs varied by 63% between reps (randread 35.7k and 58.1k IOPS). Read the ratios as indicative.

## 3. Prototype decision

No prototype was built, because the feasibility test failed:

1. fuser 0.16 cannot host a second request loop. A cloned fd needs its own `Session`, and `Session::from_fd` returns one that rejects every request. The clone would need the INIT state (`initialized`, and the negotiated settings) copied in. Those fields are crate-private (`pub(crate) initialized`), so this would mean forking fuser.
2. A thread design that works on the 0.16 public API is a dispatcher. The session thread hands read and write requests to a worker pool, and each worker replies itself. This needs `Dbfs` shared with the workers (a `fuse.rs` change), copies of write data (fuser reuses its receive buffer), per-inode ordering lanes, and reply handling outside the trait. Its ceiling is the locked share: about 1.3 to 1.8x for randread and 1.14x for creates. The randwrite locked share was not measured. That gain is modest, and the design is not clearly safe (section 4).
3. A reader pool has more headroom, but it also needs concurrent dispatch. It touches `db.rs` (reader connections, busy timeout, the checkpoint interaction) and `fuse.rs` (releasing the table lock before SQLite for clean spans). `db.rs` is the file that group commit (Step 3) is changing.

What was measured instead: `readers.c`, a C program with pthreads and the system SQLite. It is kept in the session scratchpad and not committed. It runs the same 4 KiB chunk read as dbfs against a randread layout database (`dbfs-randread-j4-r2`: 4,096 `file_chunks` rows, 256 MiB, WAL mode), 5 s per run, with a 5 s busy timeout on each connection. The table gives medians of 7 runs at the default mmap size, with the min to max range in brackets.

| connections | 1 thread | 2 threads | 4 threads |
|---|---|---|---|
| one shared connection behind a mutex (today's `Db`) | 67.2k (53.8k to 88.3k) | not run | 65.8k (63.6k to 82.7k) |
| one read-only connection per thread | 54.7k (33.0k to 80.9k) | 99.8k (40.6k to 156.2k) | 116.2k (77.2k to 146.1k) |

Reads per second.

- A shared connection is flat from 1 to 4 threads, as expected from one mutex.
- One connection per thread reaches a median of about 1.8x the shared connection at 4 threads (116k against 66k). The spread across runs is wide, because load averaged 2.5 to 5.5 on 4 vCPUs during these runs. Treat the ratio as rough.
- At 1 thread the two designs are within run-to-run noise.
- `mmap_size=0` cuts throughput to 32 to 41% of the default-mmap medians. The shared connection drops to 32% at 1 thread and 34% at 4 threads (medians of 3 runs: 21.6k and 22.5k). Four per-thread connections drop to 41% (47.3k against 116k).
- Without a busy timeout, opening four read-only connections at once failed with `database is locked`, and every 4-thread pool run failed in the first pass. With a 5 s busy timeout every run succeeded. Readers need a busy timeout, and they should open after the WAL is initialized.

## 4. Ordering, races and other risks

These apply to any concurrency design. Items marked as kernel behaviour come from the Linux FUSE client, as I know it; they were not re-checked in this spike.

- Flush against write. A synchronous `write()` returns after its reply, and the kernel waits for in-flight writeback before FLUSH and FSYNC (kernel behaviour). So with replies sent only after the data is in the arena, write-then-flush order holds. With `--kernel-cache` (writeback cache), writes are sent asynchronously. An inline `getattr` or `truncate` can then overtake a queued write unless operations on one inode stay in order, for example in per-inode lanes.
- Release. FUSE_RELEASE can be sent after close without a reply wait (kernel behaviour). It must run after the flush of the same handle, so it belongs in the same lane as that inode's writes.
- Unlink path. `Dbfs::unlink` (`src/fuse.rs:905`) looks up the name before it takes the table lock. `Db::unlink_entry` (`src/db.rs:844`) then runs `DELETE FROM dirents WHERE parent_ino = ?1 AND name = ?2` with no inode check, and drops the inode it was given from the earlier lookup. If a rename replaces the name between the lookup and the delete, the new entry is deleted and the stale inode is dropped. The kernel should prevent this: the VFS holds the parent directory lock across unlink and rename requests, which serializes both on one directory (kernel behaviour, not re-checked here). `FUSE_PARALLEL_DIROPS`, which dbfs requests only in `--kernel-cache` mode (`src/fuse.rs:980`, `:992`), relaxes that lock for lookups, not for unlink. The code should still not depend on the kernel. The lookup belongs under the table lock, and the delete should check the inode.
- Read by inode. `Dbfs::read` (`src/fuse.rs:786`) checks for a slot under the lock, drops the lock, and then calls `db.read_file`. That is correct while requests are serial. With concurrent requests, a write that overlaps the read can be buffered between the check and the read. That is allowed for overlapping operations, but the code relies on the invariant "no slot means the database is current". The invariant should be stated and tested before threads land.
- Shared OpenTable. One mutex covers all open-file state. `alloc_chunk` (`src/fuse.rs:231`) evicts up to 16 MiB (`EVICTION_SLICE_CHUNKS`, `src/fuse.rs:43`) through `flush_chunks` while the table lock is held. A flush or eviction therefore holds the lock across SQLite, and data requests on any file wait for it. A reader design needs a per-span dirty check under the lock, then the lock is released, then the database read.
- Writer stalls. Every write transaction and every read takes the same connection mutex (`src/db.rs:209`, `src/db.rs:640`), so a long flush delays any reader that shares it. A separate reader pool removes that wait for clean data.

## 5. Recommendation

1. Do not add `--threads` now. With the current design the ceiling is 1.3 to 1.8x for randread and about 1.1x for creates. The ordering rules need work, and fuser 0.16 cannot host the design without a fork.
2. Sequence the work instead:
   a. Step 3 (group commit) first. It changes which transaction each operation runs in, and a reader pool must route reads to the writer while a batch is open.
   b. Harden the unlink path: do the lookup under the table lock and make the delete check the inode in SQL. The kernel's directory lock should already serialize these operations, but the code does not depend on it. Also state and test the invariant behind the read-by-inode path. A model test in `tests/model_ops.rs` can cover both.
   c. A reader pool for clean spans with no open batch: per-thread read-only connections, a busy timeout, and all readers closed before the unmount checkpoint (`db.rs`).
   d. Release the table lock before SQLite for clean spans (`fuse.rs`).
   e. Only then the fuser 0.17 port (107 errors today) with `n_threads` of 2 to 4 and `clone_fd`, first without `--kernel-cache`.
   f. Re-measure with the same fio jobs at 1 and 4 jobs, and with the `readers.c` harness.
3. Expected gains (estimates, not measured end to end): the Amdahl ceiling for randread at 4 jobs is +30 to 80% (1.3 to 1.8x), and it is not reached without steps c and d. Creates spend about 88% of their time in SQL, so threads alone gain about 1.1x there; fewer SQL calls per create, which Step 3's batching may provide, is the lever. randwrite's lock share was not measured. io_uring is not worth pursuing now. The session loop spends 95 to 98% of its time in handlers (reply sends included), so the transport share it could change is only the 2 to 5% outside them.

## Reproduction

- Mount: `dbfs mount <db> <mnt> --stats` (stats are written at unmount).
- randread layout: `fio --name=rr --directory=<dir> --ioengine=sync --rw=write --bs=1M --size=64M --numjobs=4 --end_fsync=1`. For dbfs, a clean unmount, `posix_fadvise(DONTNEED)` on the database files, then a remount. Timed: `fio --name=rr --directory=<dir> --ioengine=sync --rw=randread --bs=4k --direct=1 --size=64M --numjobs=<1|4> --time_based --runtime=10 --ramp_time=1`.
- randwrite: `fio --name=rw --directory=<dir> --ioengine=sync --rw=randwrite --bs=4k --size=64M --numjobs=<1|4> --end_fsync=0`.
- creates: 5,000 `O_CREAT|O_EXCL` opens and closes per process, each process in its own directory, timed across processes.
- readers: `gcc -O2 -pthread readers.c -lsqlite3 -o readers`, then `readers <db> <threads> <shared|pool> <seconds>`. `MMAP_SIZE=0` disables the mmap. The harness is not committed. Raw JSON and stats were kept under `.bench/agent-6/` during the spike and removed afterwards.
