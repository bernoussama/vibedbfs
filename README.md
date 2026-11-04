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
dbfs mount [--allow-other | --allow-root] [--auto-unmount] [-o OPT[,OPT...]]... <DATABASE> <MOUNTPOINT>
```

By default only the user who mounted dbfs can access the filesystem. `dbfs mount --help` lists every option.

| Option | Effect |
| --- | --- |
| `--allow-other` | Lets every user access the mount, including root and the Docker daemon. Needed for bind mounts into containers. Mutually exclusive with `--allow-root`. |
| `--allow-root` | Lets root access the mount, in addition to the mounting user. |
| `--auto-unmount` | Unmounts the filesystem when dbfs exits, including after a crash or SIGKILL, so no stale mount is left behind. Requires `--allow-other` or `--allow-root`. |
| `-o OPT[,OPT...]` | Extra options. Repeatable, and each value may be a comma-separated list. Known names map to typed FUSE options: `ro`, `rw`, `atime`, `noatime`, `suid`, `nosuid`, `dev`, `nodev`, `exec`, `noexec`, `dirsync`, `sync`, `async`, `fsname=NAME`, `subtype=NAME`. Any other option is passed to `fusermount3`, which accepts only the options it knows, such as `max_read=N`. An unknown option, such as `context=...`, makes the mount fail with `unknown option`. |

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
```

File data is stored in 64 KiB chunk rows in SQLite. FUSE writes are buffered in memory, one 64 KiB buffer per chunk, and flushed to SQLite on `flush`, `fsync`, or `release`, or once a file buffers 64 MiB. This lets buffered write workloads avoid one SQLite transaction per small write. Overlapping dirty writes merge in their chunk buffer, and full-chunk writes skip the read-modify-write cycle. All SQL queries use prepared statement caching to avoid re-parsing.

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
