# dbfs Profiling & Flamegraph Analysis

Profiled on a Cloud VM (kernel 6.1.147, ext4/overlayfs, single-core).
Release build with `CARGO_PROFILE_RELEASE_DEBUG=2` for symbol resolution.
Sampled at 999 Hz via `perf record -g -F 999`.

## Workloads

| Workload | Parameters | Duration | Throughput | Samples |
|---|---|---|---|---|
| randwrite (no fsync) | 256M, 4k blocks, fsync=0 | 1.7 s | 148 MiB/s, 37.9k IOPS | 6,420 |
| randwrite (fsync=1) | 64M, 4k blocks, fsync=1 | 3.0 s | 21.1 MiB/s, 5.4k IOPS | 1,726 |
| randread | 256M, 4k blocks | 2.3 s | 111 MiB/s, 28.5k IOPS | 4,830 |
| sequential write | 512M, 128k blocks, fsync=0 | 1.0 s | 527 MiB/s, 4.2k IOPS | 3,133 |
| metadata (create/stat/readdir/unlink) | 5000 files + 200 dirs | 8.5 s | — | 2,375 |

**Observation**: Deleting 5000 files (6.4 s) was 6× slower than creating them (1.0 s).

---

## Flamegraph Files

- `flamegraph_write-nofsync.svg` — buffered randwrite, no fsync
- `flamegraph_write-fsync.svg` — randwrite with fsync after every write
- `flamegraph_randread.svg` — random 4k reads
- `flamegraph_seqwrite.svg` — sequential 128k writes (flush-heavy)
- `flamegraph_metadata.svg` — create / stat / readdir / unlink

---

## Top Bottlenecks

### 1. No Prepared Statement Caching — `sqlite3Parser` (2–4% of all workloads)

**Evidence**: `sqlite3Parser` → `sqlite3RunParser` → `sqlite3_prepare_v3` → `rusqlite::InnerConnection::prepare` appears in the top 5 functions of every single workload.

| Workload | `sqlite3Parser` overhead |
|---|---|
| randread | 3.64% |
| metadata | 4.17% |
| write-nofsync | 2.01% |
| write-fsync | 2.09% |

**Root cause**: Every `Db` method calls `conn.query_row(SQL, ...)` or `conn.prepare(SQL)` on inline string literals. rusqlite's `Connection::query_row` and `Connection::execute` create a fresh prepared statement every time — parsing and planning the same SQL over and over.

**Fix**: Use `rusqlite::CachedStatement` via `conn.prepare_cached(sql)` instead of `conn.prepare(sql)`. This is a drop-in change that caches the compiled bytecode for each unique SQL string. For the ~12 distinct SQL queries in `db.rs`, this eliminates redundant parsing across all operations.

**Impact**: ~2–4% CPU reduction across all workloads. Biggest impact on metadata-heavy operations (lookup, getattr, list_dir are called most often).

---

### 2. Per-Chunk Read-Modify-Write in `write_file_batch` (dominant in sequential write, ~35%)

**Evidence**: In the `seqwrite` flamegraph, `sqlite3BtreeInsert` (35.6%) → `sqlite3PcacheFetchStress` (34.7%) → `sqlite3WalFrames` dominates. This is the flush path doing an UPSERT per 64 KiB chunk.

**Root cause**: `write_file_batch` iterates chunk-by-chunk:
1. **Read** existing chunk from SQLite (optional SELECT per chunk)
2. **Merge** in-memory data with the read chunk
3. **Write** the merged chunk back (INSERT ON CONFLICT per chunk)

For a 512 MiB sequential write, that's 8,192 chunks × (1 SELECT + 1 INSERT) = ~16,384 SQL statements in one transaction. Each INSERT triggers WAL frame writes.

**Fix options**:
- **Coalesce dirty writes** before flushing: merge overlapping/adjacent `(offset, data)` entries in `DirtyFile.writes` into minimal chunk-aligned spans, reducing the number of chunk round-trips.
- **Batch full-chunk writes**: When a write covers an entire 64 KiB chunk, skip the SELECT (no read-modify-write needed — just INSERT OR REPLACE).
- **Multi-row insert**: SQLite supports `INSERT INTO file_chunks VALUES (?1,?2,?3),(?4,?5,?6),...` which can reduce per-statement overhead for sequential writes.

**Impact**: Could improve sequential write throughput by 20–40% by eliminating unnecessary SELECT round-trips for full-chunk overwrites.

---

### 3. WAL Checkpoint During Reads — `sqlite3WalCheckpoint` (1–18% depending on workload)

**Evidence**: `sqlite3WalCheckpoint` appears in the callchain under `sqlite3_step` in the read profile (1.04%) and dominates the sequential write profile (~17.6% under `copy_user_enhanced_fast_string` → WAL write path).

**Root cause**: SQLite's default WAL auto-checkpoint triggers after ~1000 pages of WAL. During heavy writes, the WAL grows quickly and checkpoints trigger even during read operations, stalling them.

**Fix options**:
- **Increase WAL auto-checkpoint threshold**: `PRAGMA wal_autocheckpoint = 10000` (or disable with `= 0` and checkpoint manually). This reduces checkpoint interruptions during write-heavy workloads.
- **Explicit checkpoint after flush**: Call `PRAGMA wal_checkpoint(TRUNCATE)` at explicit points (e.g., after unmount or large flushes) instead of relying on auto-checkpoint during normal operations.

**Impact**: Could reduce tail latency for reads during write-heavy workloads. The p99.5 sync latency of 9.9 ms (fsync workload) likely includes checkpoint I/O.

---

### 4. Double Lookup in `unlink` Path — `flush_file` + `lookup` + `lookup` (metadata)

**Evidence**: `Dbfs::unlink` (fuse.rs:268–275) calls:
1. `self.lookup(parent_ino, name)` — to get the ino for `flush_file`
2. `self.flush_file(attr.ino)` — flushes any dirty data
3. `self.db.unlink_file(parent_ino, name)` — which calls `self.lookup(parent_ino, name)` **again** internally

This is 2 lookups per unlink. With 5000 unlinks, that's 10,000 lookups × (1 JOIN query each) = 10,000 extra SQL statements.

**Root cause**: `Db::unlink_file` calls `self.lookup(parent_ino, name)` to validate the target, repeating work already done in `Dbfs::unlink`.

**Fix**: Pass the already-resolved inode to `Db::unlink_file` instead of re-resolving it. Or add an `unlink_by_ino` method that takes the inode directly.

**Impact**: Directly explains why deleting 5000 files (6.4 s) was 6× slower than creating them (1.0 s). Eliminating the redundant lookup would roughly halve the SQL work per unlink.

---

### 5. `get_inode` Called Redundantly on Write Path (write-nofsync, ~every write)

**Evidence**: `Dbfs::write` (fuse.rs:228) calls `self.db.get_inode(ino)` on every FUSE write — but the inode was already validated on `open`. For the no-fsync workload (65,536 writes), that's 65,536 extra SELECT queries on the `inodes` table just to confirm the inode is a regular file.

**Root cause**: Defensive programming — verifying inode type on every write even though the file handle is already open and validated.

**Fix**: Cache the inode kind in the `DirtyFile` struct (or alongside the file handle) to avoid re-querying on every write. A single `get_inode` on `open` is sufficient — the inode kind cannot change while the file is open.

**Impact**: ~2–5% CPU reduction on write-heavy workloads. Every `get_inode` compiles+executes a SELECT even though the answer is always the same.

---

### 6. Unbounded Dirty Write Vec — Linear Scan on Read (read-while-dirty)

**Evidence**: In the `write-nofsync` flamegraph, the dirty-overlay read path in `Dbfs::read` (fuse.rs:209–223) iterates over `dirty.writes` linearly. For a workload with many small 4k writes to the same inode, this vector can grow to thousands of entries.

**Root cause**: `DirtyFile.writes` is a `Vec<(u64, Vec<u8>)>` that only grows (writes are appended, never merged). Reading while dirty iterates the entire vec for each read. With 65,536 4k writes to one file (the fio test), the vec has 65,536 entries.

**Fix options**:
- **Coalesce on write**: Merge adjacent/overlapping writes in the `DirtyFile` on the write path, keeping the vec small.
- **Use a BTreeMap**: Replace the vec with a `BTreeMap<u64, Vec<u8>>` keyed by chunk index, allowing O(log n) range lookups.
- **Periodic flush**: Auto-flush when `dirty.writes.len()` exceeds a threshold (e.g., 1024 entries).

**Impact**: Without this fix, read latency during dirty writes grows linearly with write count. Not a problem for fsync=1 workloads (vec is flushed after each write), but critical for buffered write workloads.

---

### 7. Kernel Copy Overhead — `copy_user_enhanced_fast_string` (13–24%)

**Evidence**: This kernel function is the #1 self-cost symbol in every workload. It appears under two callpaths:
1. `pread64` → reading SQLite DB file pages from ext4
2. `pwrite64` → writing WAL frames back to ext4

This is not a dbfs code issue — it's the fundamental cost of moving data between userspace and the SQLite database file via the kernel page cache. It represents the irreducible I/O cost.

**Relevance**: Reducing the number of SQLite round-trips (fixes #1, #2, #4, #5) would directly reduce the number of times this kernel copy runs.

---

### 8. `readdir` Loads All Entries Then Skips — No Offset Push-Down

**Evidence**: In `fuse.rs:326–328`, `Filesystem::readdir` calls `Dbfs::readdir` which calls `Db::list_dir` to load ALL entries, then uses `.skip(offset)` to handle kernel-provided pagination.

**Root cause**: The FUSE kernel module calls `readdir` multiple times with increasing offsets. Each call loads all `N` entries from SQLite and discards the first `offset` entries. For a directory with 5000 entries and typical readdir buffer sizes, this means loading 5000 rows multiple times.

**Fix**: Push the offset down to the SQL query: `WHERE d.parent_ino = ?1 ORDER BY d.name LIMIT ?2 OFFSET ?3`. Or cache the directory listing across readdir calls for the same directory handle.

**Impact**: Significant for large directories. The metadata workload showed readdir of 5000 entries completed in 0.19 s — acceptable but would degrade with larger directories.

---

## Priority Ranking

| Priority | Bottleneck | Estimated Impact | Complexity |
|---|---|---|---|
| **P0** | #1 Prepared statement caching | 2–4% all workloads | Trivial (s/prepare/prepare_cached/) |
| **P0** | #4 Double lookup in unlink | ~50% of unlink cost | Small (pass resolved inode) |
| **P1** | #5 Redundant `get_inode` on write | 2–5% write workloads | Small (cache kind on open) |
| **P1** | #2 Per-chunk read-modify-write | 20–40% seqwrite | Medium (coalesce dirty writes) |
| **P2** | #6 Unbounded dirty vec | High for read-while-dirty | Medium (BTreeMap or merge) |
| **P2** | #3 WAL checkpoint tuning | Tail latency | Trivial (PRAGMA change) |
| **P3** | #8 readdir offset push-down | Large directories | Small (SQL OFFSET) |
| — | #7 Kernel copy overhead | Irreducible | N/A (reduced by other fixes) |

---

## Reproduction

```bash
# Build with debug symbols
CARGO_PROFILE_RELEASE_DEBUG=2 cargo build --release

# Profile a workload (example: randwrite no-fsync)
mkdir -p /tmp/dbfs-prof/mnt
./target/release/dbfs mount /tmp/dbfs-prof/db.sqlite /tmp/dbfs-prof/mnt &
DBFS_PID=$!
sleep 1
perf record -g -F 999 -p $DBFS_PID -o perf.data &
fio --name=test --directory=/tmp/dbfs-prof/mnt --size=256M --bs=4k --rw=randwrite --ioengine=sync --fsync=0
kill %2  # stop perf
fusermount3 -u /tmp/dbfs-prof/mnt

# Generate flamegraph
perf script -i perf.data | inferno-collapse-perf | inferno-flamegraph > flamegraph.svg
```
