# dbfs

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

## Storage

The file-backed database is opened with:

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
PRAGMA wal_autocheckpoint = 10000;
```

File data is stored in 64 KiB chunk rows in SQLite. FUSE writes are buffered in memory and flushed to SQLite on `flush`, `fsync`, or `release`, which lets buffered write workloads avoid one SQLite transaction per small write. Overlapping dirty writes are coalesced before flush, and full-chunk writes skip the read-modify-write cycle. All SQL queries use prepared statement caching to avoid re-parsing.

Important caveat: `fsync` currently flushes dbfs dirty writes into SQLite, but SQLite still runs with `synchronous = NORMAL`. This is not the strongest crash-durability mode.

## Test

Run the Rust tests:

```bash
cargo test
```

The mount integration test is skipped unless FUSE tests are explicitly enabled and `fusermount3` is available:

```bash
DBFS_RUN_FUSE_TESTS=1 cargo test --test mount_integration
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

- [Optimization results and methodology](docs/2026-05-03-optimization-results.md)
- [Buffered-write fio results](docs/2026-05-03-fio-buffered-write-results.md)
- [FUSE passthrough comparison (pre-optimization)](docs/2026-05-03-fio-fuse-passthrough-comparison.md)

## Requirements

- Rust toolchain
- Linux with FUSE support
- `fusermount3`
- `fio` for benchmarks
- `pytest` for the Python/libfuse test harness
