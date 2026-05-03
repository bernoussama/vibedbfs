# dbfs

dbfs is a FUSE filesystem backed by SQLite.

It stores filesystem metadata and file contents in a SQLite database, exposes them through FUSE, and currently supports basic file, directory, symlink, hardlink, rename, truncate, read, and write operations.

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
```

File data is stored in chunk rows in SQLite. FUSE writes are buffered in memory and flushed to SQLite on `flush`, `fsync`, or `release`, which lets buffered write workloads avoid one SQLite transaction per small write.

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

## Profiling / Flamegraphs

On Linux, you can generate a CPU flamegraph for `dbfs` using `perf` + `flamegraph`.

Prereqs:

- `perf` (Linux perf tooling)
- `cargo install flamegraph rustfilt`

Then run:

```bash
scripts/profile-perf.sh -- <dbfs args>
```

This writes `/tmp/dbfs-prof/dbfs.svg`.

## Current fio Results

Buffered dbfs writes on disk-backed btrfs:

| Workload | Runtime | Bandwidth | IOPS |
| --- | ---: | ---: | ---: |
| `randwrite`, `4k`, `fsync=0` | 3.17s | 161 MiB/s | 41.3k |
| `randwrite`, `4k`, `fsync=1` | 51.80s | 9.88 MiB/s | 2,530 |

FUSE passthrough comparison on btrfs, no fsync:

| Target | Runtime | Bandwidth | IOPS |
| --- | ---: | ---: | ---: |
| native btrfs | 0.770s | 665 MiB/s | 170k |
| FUSE passthrough btrfs | 2.322s | 220 MiB/s | 56.4k |
| dbfs on btrfs | 3.166s | 162 MiB/s | 41.4k |

dbfs reached about 74% of FUSE passthrough btrfs throughput for this no-fsync random-write workload.

Full benchmark notes:

- [Buffered-write fio results](docs/2026-05-03-fio-buffered-write-results.md)
- [FUSE passthrough comparison](docs/2026-05-03-fio-fuse-passthrough-comparison.md)

## Requirements

- Rust toolchain
- Linux with FUSE support
- `fusermount3`
- `fio` for benchmarks
- `pytest` for the Python/libfuse test harness
