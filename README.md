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
dbfs mount [--allow-other | --allow-root] [--auto-unmount] [--write-buffer SIZE] [--mmap-size SIZE] [--kernel-cache] [-o OPT[,OPT...]]... <DATABASE> <MOUNTPOINT>
```

By default only the user who mounted dbfs can access the filesystem. `dbfs mount --help` lists every option.

| Option | Effect |
| --- | --- |
| `--allow-other` | Lets every user access the mount, including root and the Docker daemon. Needed for bind mounts into containers. Mutually exclusive with `--allow-root`. |
| `--allow-root` | Lets root access the mount, in addition to the mounting user. |
| `--auto-unmount` | Unmounts the filesystem when dbfs exits, including after a crash or SIGKILL, so no stale mount is left behind. Requires `--allow-other` or `--allow-root`. On Ubuntu, the AppArmor profile for `fusermount3` blocks this cleanup (`fusermount3: failed to mark mounts private` in the log, an `apparmor="DENIED" ... flags="rw, rprivate"` line in the kernel log), so the mount stays until `fusermount3 -u`. This affects every FUSE filesystem, not just dbfs. |
| `--write-buffer SIZE` | Memory for buffered writes, shared by all open files. Default `64M`, minimum `64K`; accepts `K`, `M` and `G` suffixes. Memory is reserved as it is used, so set `8M` in a container with a small memory limit. |
| `--mmap-size SIZE` | Largest part of the database file that SQLite may map into memory (`PRAGMA mmap_size`). Default `2G` on 64-bit systems and `0` on 32-bit. `0` turns the map off. Accepts the same suffixes as `--write-buffer`. Random reads are faster with the map. Applies to read-only mounts too. |
| `--kernel-cache` | Let the kernel cache more: writeback cache, parallel directory operations, 1 MiB requests, cached symlinks, attributes cached for a day, and page cache kept across opens. Repeated reads run at page-cache speed (6–28× faster in the benchmarks). Off by default because, with the writeback cache, data written to a file that is still open can sit in the kernel until close or fsync, so a crash or `docker stop` can lose it. |
| `-o OPT[,OPT...]` | Extra options. Repeatable, and each value may be a comma-separated list. Known names map to typed FUSE options: `ro`, `rw`, `atime`, `noatime`, `suid`, `nosuid`, `dev`, `nodev`, `exec`, `noexec`, `dirsync`, `sync`, `async`, `fsname=NAME`, `subtype=NAME`. Any other option is passed to `fusermount3`, which accepts only the options it knows, such as `max_read=N`. An unknown option, such as `context=...`, makes the mount fail with `unknown option`. |

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
PRAGMA wal_autocheckpoint = 10000;
PRAGMA mmap_size = 2147483648;  -- --mmap-size; SQLite caps it at its own maximum
```

File data is stored in 64 KiB chunk rows in SQLite, keyed by one integer, `(ino << 24) | chunk_index`. A file can hold up to 1 TiB (2^24 chunks); writing past that returns `EFBIG`. A read that covers part of one chunk reads only the bytes it asks for, through the chunk's blob; a read across chunks, or of a whole chunk, uses one range query. Timestamps are stored as i64 nanoseconds. Times outside 1677–2262 are clamped, never rejected, the way ext4 clamps to its own range.

FUSE writes are buffered in memory and flushed to SQLite on `flush`, `fsync` or `release`. All open files share one write buffer of 64 KiB slots (`--write-buffer`, default 64 MiB). When it is full, the file with the most buffered data is flushed. This lets buffered write workloads avoid one SQLite transaction per small write. Overlapping dirty writes merge in their chunk buffer, and full-chunk writes skip the read-modify-write cycle. A file unlinked while open is never flushed on close: its data goes away with its last handle. All SQL queries use prepared statement caching to avoid re-parsing.

The schema version is kept in `PRAGMA user_version` (currently 4). Older databases are upgraded in place on first mount, in one transaction, so a failed or interrupted upgrade leaves the old version intact. The upgrade from version 3 rewrites every data chunk: about 20 s per GiB on a laptop SSD. It prints a note for databases over 256 MiB. The old pages stay in the file as free pages, which SQLite reuses and `statfs` counts as free. A database written by a newer dbfs is refused, not modified.

`flush` and `release` commit under `synchronous = NORMAL`. `fsync` also calls `fdatasync` on the WAL file, or on the main database file when no WAL file is present, so the committed bytes reach the host.

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

Use the fio helper script:

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

## Current fio Results

### dbfs vs Native vs FUSE Passthrough

Three-way comparison on overlay/ext4, interleaved runs, `fio` with `ioengine=sync`, `numjobs=1`:

| Workload | Native | FUSE Passthrough | dbfs | dbfs vs Passthrough |
| --- | ---: | ---: | ---: | ---: |
| `randwrite`, `4k`, `fsync=0` | 1455 MiB/s | 152 MiB/s | 217 MiB/s | **1.43x** |
| `randwrite`, `4k`, `fsync=1` | 3.8 MiB/s | 2.1 MiB/s | 29.7 MiB/s | **14.2x** |
| `randread`, `4k` | 100 MiB/s | 120 MiB/s | 137 MiB/s | **1.14x** |
| `seqwrite`, `128k`, `fsync=0` | 2535 MiB/s | 535 MiB/s | 1056 MiB/s | **1.97x** |
| create 5000 files | 0.219s | 0.837s | 0.769s | **1.09x** |
| delete 5000 files | 4.256s | 5.003s | 5.852s | 0.85x |

dbfs beats FUSE passthrough in 5 of 6 workloads. On fsync-heavy writes, dbfs is 14.2x faster than passthrough and 7.8x faster than native ext4 because SQLite WAL batches per-write fsyncs into efficient journal commits.

Full benchmark notes:

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
