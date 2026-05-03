# dbfs perf + flamegraph notes

Date: 2026-05-03

## Workload

Aligned with the existing fio randwrite benchmark (no-fsync) for comparability:

```bash
scripts/bench-fio.sh --job randwrite --size 512M --bs 4k --fsync 0
```

## Build + profiling commands

```bash
CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --release

sudo env PATH="$PATH" \
  DBFS_BIN="/home/runner/work/vibedbfs/vibedbfs/target/release/dbfs" \
  perf record -F 99 --call-graph dwarf -o /tmp/dbfs-perf-root2.data -- \
  scripts/bench-fio.sh --job randwrite --size 512M --bs 4k --fsync 0

sudo chown runner:runner /tmp/dbfs-perf-root2.data
perf script -i /tmp/dbfs-perf-root2.data > /tmp/dbfs-perf.script
/home/runner/.cargo/bin/inferno-collapse-perf /tmp/dbfs-perf.script > /tmp/dbfs-perf.folded
/home/runner/.cargo/bin/inferno-flamegraph /tmp/dbfs-perf.folded > /tmp/dbfs-flamegraph.svg
```

Generated flamegraph: `/tmp/dbfs-flamegraph.svg`

## Observed fio result

```text
write: IOPS=21.4k, BW=83.6MiB/s (512MiB/6.13s)
avg write latency: 45.48 us (p50 43 us, p95 55 us, p99 66 us)
```

## Hottest stacks (dbfs focus)

Top dbfs stacks from the folded profile highlight:

- `fuser::channel::Channel::receive` + kernel read path (FUSE request handling).
- `rusqlite::statement::Statement::bind_parameter` → `sqlite3VdbeMemSetStr`.
- `rusqlite::inner_connection::InnerConnection::prepare` → `sqlite3_prepare_v3`.
- `rusqlite::Connection::execute` / `Statement::execute_with_bound_parameters` → `sqlite3_step` → `sqlite3VdbeExec`.
- `sqlite3BtreeInsert` + `sqlite3PcacheFetchStress` (chunk writes).
- `sqlite3WalFrames` and `sqlite3_wal_checkpoint_v2` (WAL/commit costs).
- `__libc_pwrite` / `__libc_pread` in the SQLite commit path.

## Bottlenecks + optimization targets

1. **SQLite statement prepare/bind churn** in `Db::write_file_batch` (per-chunk SELECT and INSERT/UPDATE).
   - Target: reuse prepared statements or cached statements for chunk reads/writes and inode updates.
2. **Read-modify-write per chunk** in `write_file_batch` (`SELECT data` before every write).
   - Target: coalesce buffered writes into full chunks, avoid readback when overwriting full ranges, or keep dirty chunk buffers per file.
3. **WAL frame + checkpoint overhead** on commit.
   - Target: tune WAL checkpoint behavior and batch commits when possible (already buffered at FUSE level, but SQL batch could be further reduced).
4. **FUSE request handling overhead** (`fuser::channel::Channel::receive`).
   - Target: increase write size / reduce number of FUSE write ops (e.g., larger max write), and keep buffering to reduce flush frequency.
