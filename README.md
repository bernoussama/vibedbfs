# dbfs

> **Experimental:** dbfs is an experimental project and is not ready for
> production or trusted data storage.

dbfs is a FUSE filesystem backed by SQLite.

It stores filesystem metadata and file contents in a SQLite database, exposes them through FUSE, and currently supports basic file, directory, symlink, hardlink, rename, truncate, read, and write operations.

See [Architecture](docs/architecture.md) for ASCII diagrams of the system structure and data flows.

## Build

```bash
cargo build
cargo build --release
```

## Run

Create a mountpoint and mount a database-backed filesystem:

```bash
mkdir -p /tmp/dbfs-mnt
cargo run -- mount /tmp/dbfs.sqlite /tmp/dbfs-mnt
```

In another shell:

```bash
echo hello > /tmp/dbfs-mnt/hello.txt
cat /tmp/dbfs-mnt/hello.txt
```

Unmount when finished:

```bash
fusermount3 -u /tmp/dbfs-mnt
```

Stopping dbfs with Ctrl-C, or sending it SIGTERM or SIGHUP, also unmounts cleanly. It writes out buffered data before it exits, with exit code 0. If that write fails, dbfs prints the error and exits 1. See [Shutdown](docs/architecture.md#shutdown).

## Mount options

```bash
dbfs mount [--allow-other | --allow-root] [--auto-unmount] [--write-buffer SIZE] [--mmap-size SIZE] [--wal-cap SIZE] [--kernel-cache | --no-keep-cache] [--stats] [--sync-metadata] [-o OPT[,OPT...]]... <DATABASE> <MOUNTPOINT>
```

By default only the user who mounted dbfs can access the filesystem. `dbfs mount --help` lists every option.

| Option | Effect |
| --- | --- |
| `--allow-other` | Lets every user access the mount, including root and the Docker daemon. Needed for bind mounts into containers. Mutually exclusive with `--allow-root`. |
| `--allow-root` | Lets root access the mount, in addition to the mounting user. |
| `--auto-unmount` | Unmounts the filesystem when dbfs exits, including after a crash or SIGKILL, so no stale mount is left behind. Requires `--allow-other` or `--allow-root`. On Ubuntu, the AppArmor profile for `fusermount3` blocks this cleanup (`fusermount3: failed to mark mounts private` in the log, an `apparmor="DENIED" ... flags="rw, rprivate"` line in the kernel log), so the mount stays until `fusermount3 -u`. This affects every FUSE filesystem, not just dbfs. |
| `--write-buffer SIZE` | Memory for buffered writes, shared by all open files. Default `256M`, minimum `64K`; accepts `K`, `M` and `G` suffixes. Memory is reserved as it is used, never up front, and is kept until unmount, so a mount that has filled the budget holds that much RSS. Set `8M` in a container with a small memory limit. |
| `--mmap-size SIZE` | Largest part of the database file that SQLite may map into memory (`PRAGMA mmap_size`). Default `2G` on 64-bit systems and `0` on 32-bit. `0` turns the map off. Accepts the same suffixes as `--write-buffer`. Random reads are faster with the map. Applies to read-only mounts too. |
| `--wal-cap SIZE` | Backstop for how large the write-ahead log may grow. Default `1G`, minimum `64K`; accepts the same suffixes as `--write-buffer`. Under sustained writes a background reset keeps the WAL at about 260 to 500 MiB in most measured runs, well below the cap. In 2 of 11 measured 3 GiB runs the WAL reached the cap, so the cap did act; see [the performance results](docs/2026-11-performance-results.md). A writer that leaves the WAL over the cap checkpoints it itself and waits. Read-only mounts do not checkpoint. See [Storage](#storage). |
| `--kernel-cache` | Let the kernel cache more: writeback cache, parallel directory operations, 1 MiB requests, cached symlinks and async direct I/O. Off by default. With the writeback cache, written data can sit in the kernel until the file is closed or synced, so a crash or `docker stop` with files still open can lose data that dbfs never saw. Conflicts with `--no-keep-cache`. Each item is explained in [Kernel caching](docs/architecture.md#kernel-caching). |
| `--no-keep-cache` | Restore the caching of earlier releases: the kernel drops a file's cached data on every open, and attributes and names expire after one second, so repeated reads go through FUSE and SQLite again. Conflicts with `--kernel-cache`. See [Kernel caching](docs/architecture.md#kernel-caching). |
| `--sync-metadata` | Commit every metadata change (create, rename, unlink, and so on) as its operation returns, instead of sharing a commit with the operations that follow it within 5 ms. Slower for metadata-heavy work. A crash (kill -9) then loses no completed operation, even one not yet fsynced. Off by default. Read-only mounts always commit this way. See [Metadata Batches](docs/architecture.md#metadata-batches). |
| `--stats` | Counts FUSE operations, SQLite time and buffer activity, and prints them to stderr as one JSON line, `{"dbfs_stats": {...}}`, when the mount ends (unmount, or SIGTERM, SIGINT or SIGHUP). `DBFS_STATS=1` also turns it on. Off by default. See [Statistics](docs/architecture.md#statistics). |
| `-o OPT[,OPT...]` | Extra options. Repeatable, and each value may be a comma-separated list. Known names map to typed FUSE options: `ro`, `rw`, `atime`, `noatime`, `suid`, `nosuid`, `dev`, `nodev`, `exec`, `noexec`, `dirsync`, `sync`, `async`, `fsname=NAME`, `subtype=NAME`. Any other option is passed to `fusermount3`, which accepts only the options it knows, such as `max_read=N`. An unknown option, such as `context=...`, makes the mount fail with `unknown option`. |

By default the kernel keeps a file's page cache across opens and caches attributes and names for a day. That is safe because dbfs is the only writer of its database and every change goes through the kernel. The same rule covers the database file itself: see [Storage](#storage) before editing it with `sqlite3`.

`--mmap-size` and SIGBUS: with the memory map, an I/O error on the database file (a failing disk, or the file being truncated by another process) can make the kernel send dbfs SIGBUS. That kills the daemon instead of returning EIO to the caller, and writes still buffered in dbfs are lost. `--mmap-size 0` returns EIO instead, at the cost of slower random reads.

`default_permissions` is always on and cannot be turned off. The kernel therefore enforces file mode bits and owners, even for other users.

`--allow-other` as a non-root user needs one line in `/etc/fuse.conf`:

```text
user_allow_other
```

Without it, the mount fails with `allow_other requires 'user_allow_other' in /etc/fuse.conf`. Root does not need this line.

Example: a mount that a Docker container can use, and that unmounts if dbfs crashes:

```bash
dbfs mount --allow-other --auto-unmount -o noatime /srv/dbfs.sqlite /srv/dbfs
```

### Using dbfs with Docker bind mounts

The Docker daemon runs as root and needs access to the mount. Without `--allow-other`, `docker run -v /srv/dbfs:/data` fails with `EACCES`. `--allow-root` gives root access but not other users, so use `--allow-other` for containers.

1. If dbfs runs as a non-root user, add `user_allow_other` to `/etc/fuse.conf` (see above).
2. Mount with `--allow-other`. Add `--auto-unmount` so a crashed dbfs doesn't leave a stale mount on the host.
3. Bind-mount the mountpoint into the container:

   ```bash
   docker run --rm -v /srv/dbfs:/data alpine sh -c 'echo hi > /data/hello && cat /data/hello'
   ```

`default_permissions` stays on, so the kernel checks file mode bits and owners for container users, as it does for host users.

## Storage

The file-backed database is opened with:

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
PRAGMA wal_autocheckpoint = 0;       -- checkpoints run in a background thread, see below
PRAGMA journal_size_limit = 67108864;
PRAGMA mmap_size = 2147483648;  -- --mmap-size; SQLite caps it at its own maximum
```

A new database file gets 16 KiB SQLite pages. dbfs sets `PRAGMA page_size = 16384` only when the file has no pages yet, before the WAL switch, because SQLite ignores `page_size` on a non-empty file. An existing database keeps the page size it was created with, and `statfs` reports the page size as the block size. The larger page costs space for trees of many small files: in the measured small-file workloads the database was 18 to 22 % larger than with 4 KiB pages.

To change the page size of an existing database, unmount it and run:

```bash
dbfs migrate --page-size 4K /path/to/dbfs.sqlite
```

`--page-size` takes a power of two from 512 bytes to 64K. The command copies the database with `VACUUM INTO` to `<DATABASE>.migrate`, checks the copy's page size, `user_version` and `quick_check`, syncs it, removes the old `-wal` and `-shm` files, and renames the copy over the database. The original is not changed before that rename, so an interrupted run leaves it as it was, and a leftover copy is removed by the next run. The tree, file contents, symlinks, hard links, timestamps and `user_version` are copied as they are. The migration takes the database lock, so it refuses a mounted database, and it refuses when the free space is less than the database plus its WAL. A copy at larger pages can be larger than the original, so a full disk can still stop it; then the original is unchanged.

File data is stored in 64 KiB chunk rows in SQLite, keyed by one integer, `(ino << 24) | chunk_index`. A file can hold up to 1 TiB (2^24 chunks); writing past that returns `EFBIG`. A read that covers part of one chunk reads only the bytes it asks for, through the chunk's blob; a read across chunks, or of a whole chunk, uses one range query. Timestamps are stored as i64 nanoseconds. Times outside 1677–2262 are clamped, never rejected, the way ext4 clamps to its own range.

FUSE writes are buffered in memory and flushed to SQLite on `flush`, `fsync` or `release`. All open files share one write buffer of 64 KiB slots (`--write-buffer`, default 256 MiB). Slots are taken as dirty chunks need them and are never given back, so the buffer costs RAM only once a workload has filled it. When it is full, the file with the most buffered data has up to 16 MiB of its lowest dirty chunks written to SQLite in one transaction, and the rest of that file stays buffered. A large file is therefore written out in bounded steps, not in one long transaction. `fsync`, `flush` and `release` still write all of a file's buffered data. An eviction is not an `fsync`: after a crash, some evicted slices can survive while the rest of the unsynced data is lost. This lets buffered write workloads avoid one SQLite transaction per small write. Overlapping dirty writes merge in their chunk buffer. A write reads from SQLite only the 4 KiB blocks it covers in part, so a write of whole blocks reads nothing. A chunk whose blocks are all dirty is stored without reading its row. A partly dirty chunk is stored by reading its row, patching its dirty blocks and writing the row back. A file unlinked while open is never flushed on close: its data goes away with its last handle. All SQL queries use prepared statement caching to avoid re-parsing.

The schema version is kept in `PRAGMA user_version` (currently 5). Older databases are upgraded in place on first mount, in one transaction, so a failed or interrupted upgrade leaves the old version intact. The upgrade from version 3 rewrites every data chunk: about 20 s per GiB on a laptop SSD. It prints a note for databases over 256 MiB. The upgrade from version 4 rebuilds only the `inodes` table and does not rewrite file data; after it, an inode number is never reused. The old pages stay in the file as free pages, which SQLite reuses and `statfs` counts as free. A database written by a newer dbfs is refused before any write. Only one dbfs can have a database open at a time: `mount` holds an exclusive lock on a `<db>.lock` file beside it and refuses to start if another dbfs holds it. The lock does not stop sqlite3. Editing the database with sqlite3 while it is mounted is unsupported: the kernel and dbfs cache its data and attributes, so the mount may show stale data, and a later dbfs write to the same chunk overwrites the edit.

Metadata changes and flushed data share one open SQLite transaction, the batch. It commits at these points: `fsync` and `fsyncdir`, `statfs`, unmount, 5 ms after its first change, once it holds 256 operations or 64 MiB of chunk data, and before the checkpoint thread resets the WAL. `flush` and `release` do not commit. Commits run under `synchronous = NORMAL`. `fsync` and `fsyncdir` also call `fdatasync` on the WAL file, or on the main database file when no WAL file is present, so the committed bytes reach the host.

### Durability

- **kill -9.** The process can lose the last 5 ms, or the last 256 operations, of acknowledged changes that no commit point has covered. Everything that `fsync` or `fsyncdir` covered survives. Data still in the write buffer is lost on a kill, as before. `--sync-metadata` commits every metadata change before its operation returns, so a kill -9 loses no completed metadata operation and no flushed data.
- **Power loss.** Unchanged. Unsynced changes may be lost, and the database stays consistent, because each commit is atomic.
- **Fail-stop.** If a COMMIT fails, or SQLite rolls back a batch that holds acknowledged operations (for example, the disk fills during a batch), dbfs prints the error once to stderr, counts it in `batch_failures` under `--stats`, and refuses further changes and `fsync` with `EIO` until the filesystem is remounted. Reads and unmount still work, and data committed before the failure is kept. When operations commit one by one (`--sync-metadata`, or the free-space guard), a failed COMMIT fails only the operation that caused it.
- **Free space.** While less than 256 MiB is free on the database's filesystem, every operation commits by itself, so an out-of-space error is reported by the operation that caused it. Free space is checked at most once a second.
- **Read-only mounts** commit every operation, as `--sync-metadata` does.

The design is described in [Metadata Batches](docs/architecture.md#metadata-batches).

Checkpoints run off the write path. Commits never checkpoint. A background thread with its own connection runs a PASSIVE checkpoint when the WAL is over 64 MiB (or half of `--wal-cap`, if that is smaller), and after a second without a commit when the WAL holds frames it has not yet reset. Under sustained writes a PASSIVE pass alone does not reset the WAL, so once the WAL reaches 256 MiB (four times the passive threshold, or three quarters of `--wal-cap` if that is smaller) the thread takes the database lock: writers wait while the last frames are backfilled, and the WAL is reset. The next writer starts it again from the top, so the `-wal` file does not keep growing. In measured sequential writes on the development VM, over five rounds, the WAL peaked at 260 to 292 MiB for 512 MiB writes, and at 310 to 730 MiB (median 373) for 3 GiB writes. Two of the five 3 GiB rounds reached about 500 MiB or more. Across the 3 GiB runs at this setting, two of eleven reached the 1 GiB cap (1027 and 1039 MiB), so the WAL is not held to 512 MiB. The full measurements are in [the performance results](docs/2026-11-performance-results.md). `--wal-cap` (default `1G`) is a backstop: a writer that leaves the WAL larger runs a RESTART checkpoint before it returns, so that one write waits. The cap is checked after each commit, and a flush transaction writes at most 16 MiB, so the WAL can pass the cap by at most that much. Unmount checkpoints everything and truncates the WAL to zero bytes. A checkpoint that fails is reported once: the next `fsync` on the mount returns `EIO`, and dbfs prints the error to stderr. Read-only mounts run no checkpoints and do not write the database file. `statfs` counts the WAL's size as used space.

## Test

Run the Rust tests:

```bash
cargo test
```

The mount integration test is ignored unless you select ignored tests and FUSE is available:

```bash
DBFS_RUN_FUSE_TESTS=1 cargo test --test mount_integration -- --ignored
```

The shutdown tests (signals and external unmount) use the same gate:

```bash
DBFS_RUN_FUSE_TESTS=1 cargo test --test shutdown -- --ignored

The mount-option tests (`-o`, `--allow-other`, `--auto-unmount`) use the same gate. The `allow_other` tests are skipped, with a message, unless you run as root or `/etc/fuse.conf` contains `user_allow_other`:

```bash
DBFS_RUN_FUSE_TESTS=1 cargo test --test mount_options -- --ignored
```

There is also a libfuse-oriented pytest test harness:

```bash
pytest tests/test_with_libfuse.py -v
```

## Benchmark

Use the fio helper script. It mounts dbfs on a fresh directory under `.bench/` (disk-backed, never `/tmp`) and removes that directory afterwards unless `--keep` is given:

```bash
scripts/bench-fio.sh
```

Default workload:

```text
512M, 4k, randwrite, fsync=1
```

No-fsync random-write benchmark:

```bash
scripts/bench-fio.sh --job randwrite --size 512M --bs 4k --fsync 0
```

Run a small suite:

```bash
scripts/bench-fio.sh --job suite --size 512M --bs 4k --fsync 0
```

Pass extra fio arguments after `--`:

```bash
scripts/bench-fio.sh --job randwrite --fsync 0 -- --group_reporting --output-format=json
```

Choose the database and mountpoint yourself:

```bash
scripts/bench-fio.sh --database /srv/bench/dbfs.sqlite --mountpoint /srv/bench/mnt
```

The script refuses to run if the database directory or the mountpoint is on `tmpfs` or `ramfs`, because those measure RAM. It also refuses a mountpoint that is already mounted, and it fails unless the mountpoint is a FUSE mount, so another filesystem cannot be benchmarked as dbfs. To benchmark RAM on purpose, pass `--allow-tmpfs` or set `DBFS_BENCH_ALLOW_TMPFS=1`.

### Local harness: `scripts/bench.sh`

Runs `bench_io` workloads (see below) against a fresh database and mount for every run, on disk under `.bench/`. Results are appended to `.bench/results/<label>.jsonl`. The harness refuses to run when `.bench/` is on `tmpfs` or `ramfs`, because that measures RAM. Pass `--allow-tmpfs` (or set `DBFS_BENCH_ALLOW_TMPFS=1`) to benchmark RAM on purpose. Each line records `target` (the path benchmarked), `target_fstype` (the filesystem mounted there, which must be FUSE for dbfs) and `storage_fstype` (the filesystem holding the database), so a mislabelled run is easy to spot.

```bash
scripts/bench.sh --label mine --repeat 3 --workloads "smallfiles tree-cold"
```

`--raw` runs the same workloads on the plain `.bench/` directory instead of dbfs. The cold workloads drop the page cache, which needs root or passwordless `sudo`.

### bench_io workloads

`examples/bench_io.rs` prints one JSON line per measurement (`workload`, `secs`, and `mib_s` and `iops` where they apply).

| Workload | What is timed |
| --- | --- |
| `seqwrite`, `randwrite`, `randwrite-fsync`, `seqread`, `randread` | the fio-style jobs with sizes of 256–512 MiB |
| `seqwrite-long [SIZE]` | 128 KiB sequential writes totalling 3 GiB (or `SIZE`), with the final fsync included |
| `seqread-cold`, `randread-cold` | reads of 256 MiB after the data is laid out and the caches are dropped |
| `smallfiles` | creating 20,000 files of 1–16 KiB in a 20 × 20 × 50 directory tree, each closed after its write |
| `tree-cold` | on the same 20,000-file tree, after a remount and a cache drop: a timed `lstat` walk, then a timed `rm -rf` |
| `metadata`, `bigdir` | create, stat, readdir and unlink of 5,000 small files, and create, list and `rm -rf` of 20,000 entries |

The cold workloads run in three steps, so the caller can drop caches and remount in between: `bench_io layout <dir> <SIZE>`, `bench_io read-only <dir> <seq|rand> <SIZE>`, and `bench_io tree-layout|tree-stat|tree-rm <dir>`. `compare.sh` and `bench.sh` run them in that order.

### Docker comparison: `scripts/bench-docker/compare.sh`

Runs dbfs, ext4, btrfs and libfuse's `passthrough_hp` from one privileged container, on the same disk. It needs Docker with `--privileged`.

```bash
scripts/bench-docker/compare.sh --tag baseline
scripts/bench-docker/compare.sh --quick --tag smoke
scripts/bench-docker/compare.sh --tag write-buffer-8M --configs dbfs-ext4 --workloads "seqwrite randwrite" --dbfs-args "--write-buffer 8M"
```

| Option | Effect |
| --- | --- |
| `--quick` | configs `ext4 pt-ext4-user dbfs-ext4 dbfs-ext4-kc`, one bench_io round and one fio round |
| `--rounds N`, `--fio-rounds N` | bench_io rounds (default 3) and fio rounds (default 2) |
| `--configs "LIST"` | configs to run (default: all ten) |
| `--workloads "LIST"` | workloads by name. A name runs the bench_io workload and the fio job of the same name (default: all) |
| `--dbfs-args "ARGS"` | extra arguments for every dbfs mount, placed after the config's own flags and before the database path, for A/B tests of one flag |
| `--tag NAME` | results go to `.bench/fscmp2/NAME/` (default: a timestamp). The directory must not exist yet |

fio jobs: `randwrite-nofsync`, `randwrite-fsync`, `randread-cold`, `seqwrite`, and `randwrite-4jobs` (four jobs of 64 MiB each, 4 KiB random writes, no fsync, `group_reporting`).

Each tag directory holds `run.txt` (the command lines, git revision, the dbfs version or, while dbfs has no `--version` flag, its binary's SHA-256, the kernel and the options), `results/<config>.jsonl`, `raw/` (bench_io output as printed), `fio/<config>/` (fio JSON), `fio/targets.tsv` (target and filesystem types of each fio file), `logs/` (FUSE server stderr) and `summary.txt`. Everything under `.bench/` is chowned back to the host user when the container exits, failures included.

Summarize one tag, or compare two (`dir2` against `dir1`; above 1 means `dir2` is better for both MiB/s and seconds):

```bash
scripts/bench-docker/summarize.py .bench/fscmp2/baseline
scripts/bench-docker/summarize.py .bench/fscmp2/baseline .bench/fscmp2/write-buffer-8M
```

## Benchmark results

These numbers come from [the 2026-11-03 baseline benchmark](docs/2026-11-03-baseline-benchmark.md). It ran on ext4 under `.bench/` with `examples/bench_io.rs`, through `scripts/bench.sh`. fio was not available on that host when the main table was measured. The first table is the baseline before the data-oriented redesign (`8c75dde`). Later sections measure the redesigned build (`3c3a1bc`) and other intermediate builds. Compare numbers only within one table of that document.

- **Durable writes are on par with ext4.** With an fsync after every 4 KiB write, the baseline does 216 IOPS on dbfs against 195 on ext4. The redesigned build, in fio over 2 runs, does 215 and 207 IOPS against 206 and 204. Both are bound by the disk's flush latency. A dbfs fsync is one `fdatasync` of the WAL on the same ext4 (commit `89da6ec`).
- **Buffered writes are much slower than ext4.** In the baseline, 4 KiB random writes without fsync run at 23.0 MiB/s on dbfs against 990 MiB/s on ext4, which is 43× slower. Sequential 512 MiB writes in 128 KiB blocks run at 154 MiB/s against 4,048 MiB/s, which is 26× slower. The redesigned build does 25.6 MiB/s random (against 996) and 206 MiB/s sequential (against 4,110).
- **Metadata is slower.** In the baseline, 5,000 files of 4 KiB over 200 directories take 4.146 s on dbfs and 0.165 s on ext4. Unlink alone takes 2.777 s against 0.051 s. The redesigned build takes 2.48 s against 0.164 s.
- **Reads are not compared directly.** ext4 reads come from the page cache. The baseline ran with the old caching, which `--no-keep-cache` restores: dbfs dropped its page cache on every open, so its reads went through FUSE and SQLite. The default now keeps the page cache, so warm re-reads of an unchanged file are served by the kernel. With `--kernel-cache`, warm sequential reads reach about the speed of `passthrough_hp --nopassthrough` on ext4 (8,545 against 8,429 MiB/s), because the kernel serves them from its page cache.
- **FUSE passthrough.** The like-for-like comparison with libfuse's `passthrough_hp`, with every filesystem run from one container, is in the same document. `scripts/bench-docker/compare.sh` runs it.

### Earlier results, measured before durable fsync

The fio runs from 2026-05-03 predate commit `89da6ec`. Before that commit, dbfs `fsync` returned without a host sync. Their fsync numbers are therefore not durable-write numbers, and the 14.2x and 7.8x fsync ratios against passthrough and native ext4 should not be cited. Their passthrough comparisons also do not use the same I/O path as dbfs. Each document has a dated note at the top:

- [Disk-backed dbfs vs FUSE passthrough benchmark](docs/2026-05-03-disk-backed-fuse-passthrough-benchmark.md)
- [Optimization results and methodology](docs/2026-05-03-optimization-results.md)
- [Buffered-write fio results](docs/2026-05-03-fio-buffered-write-results.md)
- [FUSE passthrough comparison (pre-optimization)](docs/2026-05-03-fio-fuse-passthrough-comparison.md)

## Requirements

- Rust toolchain
- Linux with FUSE support
- `fusermount3`
- `fio` for benchmarks
- `pytest` for the Python/libfuse test harness
