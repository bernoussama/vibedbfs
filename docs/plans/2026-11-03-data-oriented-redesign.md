# Data-Oriented Redesign Plan

Date: 2026-11-03

## Status (2026-11-04)

Every phase is implemented on `refactor/data-oriented-design`:

| PR | Phases | Commit |
|---|---|---|
| 1 | 0 + 1 | earlier commits on the branch |
| 2 | C | earlier commits on the branch |
| 3 | 6 | `0c40b3c`, results in `67a257e` |
| 4 | 2 + 3 | `f8b9396` |
| 5 | 4 | `441a607` |
| 6 | 5 | `8548224` |
| 7 | 7 | `3c3a1bc` |

Decisions made during implementation:

- **Open questions 1 and 2.** `readdir` is keyset-ordered by dirent id, not by
  name. Files are capped at 1 TiB, with `EFBIG` from `write`, `truncate` and
  `fallocate`. Timestamps outside 1677–2262 are clamped, not rejected.
- **xattrs.** `setxattr` stays `EOPNOTSUPP` (open question 5 is unresolved).
- **Unlinked while open.** These files are no longer flushed on close or fsync.
  Their data dies with the last handle, which deletes the inode.
- **`setattr`** flushes buffered data before applying a size or time change.
  Without this, `cp -a` (write, then `utimensat`, then close) produced empty files,
  because the kernel caches the size `setattr` returns.
- **Write buffer.** The arena budget is configurable with `--write-buffer SIZE`
  (default 64 MiB, minimum 64 KiB).
- **Migration to version 4.** It rewrites the chunk table at about 20 s per GiB.
  The database file keeps the freed pages afterwards; run `VACUUM` to reclaim them.

Results are in [the benchmark doc](../2026-11-03-baseline-benchmark.md).

## Goal

Reshape dbfs around how its data is actually accessed. The aim is fewer SQL
round-trips, fewer B-tree writes, fewer heap allocations, and fewer hash
lookups on the read and write hot paths, with no change to POSIX behaviour.

Each phase ships on its own, keeps the full test suite green, and is benchmarked
against the phase before it.

**Linux and Docker compatibility is the top constraint.** No performance phase may
ship if it regresses any scenario in
[Compatibility Requirements](#linux-and-docker-compatibility-requirements). Where
a speed-up conflicts with compatibility, compatibility wins.

## Non-Goals

- No new filesystem features beyond what Phase C needs for compatibility (no locks
  implementation, no multi-threaded dispatch).
- No change to durability semantics (`flush` on close, `fdatasync` on `fsync`).
- No dropping of the existing public `Db` API used by `tests/`. Old methods stay
  as thin wrappers until a later cleanup.
- dbfs as Docker's `data-root` (overlay2 upperdir on FUSE) is not supported by the
  kernel's overlayfs and stays out of scope. Volumes and bind mounts are in scope.

## Linux and Docker Compatibility Requirements

### Target scenarios

| ID | Scenario | Example |
|---|---|---|
| S1 | Plain Linux host mount, non-root user | `dbfs mount db.sqlite ~/mnt` |
| S2 | Host mount bind-mounted into a container | `dbfs mount --allow-other db.sqlite /srv/dbfs` then `docker run -v /srv/dbfs:/data ...` |
| S3 | dbfs running inside a container, mounting inside it | `docker run --device /dev/fuse --cap-add SYS_ADMIN dbfs-image mount /data/db.sqlite /mnt` |
| S4 | dbfs in a container, mount shared with the host or sibling containers | `-v /srv/dbfs:/mnt:rshared` |
| S5 | Rootless Docker / Podman (FUSE in a user namespace, kernel ≥ 4.18) | `podman run --device /dev/fuse ...` |
| S6 | Lifecycle: `docker stop`, `systemctl stop`, Ctrl-C | clean unmount, no lost writes |

Supported: x86_64 and aarch64 (Docker Desktop on macOS and Windows runs an arm64/amd64
Linux VM), kernel ≥ 4.18, glibc and musl images.

### Gaps found in the current code

| # | Gap | Where | Breaks |
|---|---|---|---|
| C1 | No signal handling and no `Filesystem::destroy`. SIGTERM kills the process without flushing dirty buffers and leaves a stale mount (`Transport endpoint is not connected`). As PID 1 in a container, SIGTERM is **ignored** (no handler), so `docker stop` always ends in SIGKILL after 10 s | `run.rs`, `fuse.rs` | S3, S4, S6 |
| C2 | No `allow_other`, and `tests/fuse_readonly.rs:283` asserts it stays off. Root and the Docker daemon get `EACCES` on the mount, so bind mounts into containers fail | `run.rs::mount_options` | S2, S4 |
| C3 | `rusqlite` links the system `libsqlite3`, so images need a matching `.so` and musl/static builds are awkward | `Cargo.toml` | S3, S5 |
| C4 | xattr ops fall through to fuser's default `ENOSYS`. Most tools cope, but behaviour should be explicit and tested (`cp -a`, `tar --xattrs`, `rsync -X`, SELinux `:z` volume relabel) | `fuse.rs` | S2, S3 |
| C5 | `fallocate` is unimplemented (`ENOSYS` → `EOPNOTSUPP`). Package managers, databases and `fio` inside containers call `posix_fallocate`; glibc falls back to writes, but musl doesn't | `fuse.rs` | S2, S3 |
| C6 | `readdir` uses positional `OFFSET` cookies, so deleting entries between `readdir` calls shifts positions. `rm -rf` on a large directory (where `getdents` takes several batches) can **skip entries** and fail with `ENOTEMPTY` | `list_dir_offset` | S1–S5 |
| C7 | No `Dockerfile`, no documented `docker run` flags, and no CI coverage of S2/S3 | repo | all |

### Compatibility test matrix (gate for every PR)

`tests/compat/` holds shell-driven tests, skipped unless `DBFS_RUN_DOCKER_TESTS=1`:

1. **S1:** the existing mount integration tests and pytest.
2. **S2:** host mount with `--allow-other`; `docker run --rm -v $MNT:/data alpine sh -c '...'`
   writes, reads, `chown`s, hard-links, renames, then `rm -rf`s a 10,000-entry tree.
   Verify from the host.
3. **S3:** build the image; `docker run --device /dev/fuse --cap-add SYS_ADMIN
   --security-opt apparmor=unconfined`; run the same workload inside.
4. **S6:** start a large write without closing the file, `docker stop` (and `kill -TERM`
   on the host). Assert the bytes are in SQLite after restart and `mountpoint -q` is false.
5. **Workloads inside a container on dbfs:** `git clone` + `git status`,
   `tar -xpf` of a real image layer (with timestamps and modes), `apk add` or `apt-get install`
   into a chroot on dbfs, and `sqlite3` creating a database on dbfs (exercises locks
   and `fsync`).
6. **S5** (best effort, nightly): Podman rootless with `--device /dev/fuse`.

## Current Problems (evidence)

| # | Problem | Where | Cost |
|---|---|---|---|
| P1 | No index on `dirents.child_ino`. `EXPLAIN QUERY PLAN` shows `SCAN dirents` for `is_descendant`, and the FK `ON DELETE CASCADE` scans the same way on every inode delete | `src/db.rs` schema | O(total dirents) per unlink, rmdir, rename-over; likely cause of the 6× slower unlink in `docs/2026-05-03-profiling-results.md` |
| P2 | `file_chunks` has a composite PK on a rowid table: a hidden rowid B-tree plus an autoindex | schema | 2 B-tree writes per chunk upsert |
| P3 | `get_inode` decodes all 13 columns when callers need only `kind` and maybe `size` | `read_file`, `write_file_batch_at`, `list_dir*`, `create_node`, `link`, `rename_entry`, `Dbfs::read`, `Dbfs::open` | Extra column decode and an extra SELECT per op |
| P4 | Open-file state is spread over 4 containers in 2 layers (`open_counts`, `inode_kinds`, `pending_delete: Vec` with linear search, `dirty_files`) | `Db`, `Dbfs` | About 4 hash lookups and several `RefCell` borrows per `write` |
| P5 | Dirty buffer is a `BTreeMap<u64, Vec<u8>>`: one allocation per chunk, growth through `resize`, everything freed on each flush | `fuse.rs` `DirtyFile` | Allocator churn and pointer chasing |
| P6 | Chunk reads copy twice (`row.get::<Vec<u8>>` and then `copy_from_slice`); the dirty read path allocates 2 output buffers; one `query_row` per chunk | `read_file`, `Dbfs::read`, `read_chunk`, `truncate_file` | Allocations and memcpy; N statements per read |
| P7 | `readdir` fetches all remaining entries (`LIMIT -1 OFFSET n`), allocating a `Vec<u8>` per name, while `reply.add` keeps about 4 KB | `list_dir_offset`, `Dbfs::readdir` | O(n²) rows to list a large directory |
| P8 | `write_file` calls `data.to_vec()` to fit `&[(u64, Vec<u8>)]` | `db.rs` | 1 allocation per direct `Db` write |

## Benchmark Protocol (every phase)

Per `AGENTS.md`, do **not** benchmark on `/tmp` (it's tmpfs). Always pass a
disk-backed path:

```bash
cargo build --release
mkdir -p .bench
scripts/bench-fio.sh --job suite --database .bench/db.sqlite --mountpoint .bench/mnt
scripts/bench-fio.sh --job randwrite --fsync 0 --size 256M --database .bench/db.sqlite --mountpoint .bench/mnt
```

Add a metadata benchmark to match the profiling doc: create, stat, readdir and
unlink 5,000 files across 200 dirs, timed per stage. Add a `--job metadata` mode to
`scripts/bench-fio.sh` or a small `scripts/bench-metadata.sh`. Also time
`ls -f` on one directory with 50,000 entries, for P7.

Record results in `docs/2026-11-xx-dod-phase-N-results.md` with the baseline
from Phase 0.

---

## Phase 0: Baseline and Guard Rails

1. Capture baseline numbers with the protocol above on the current `master`.
2. Add a schema version: `PRAGMA user_version`. The current schema counts as
   version 1. `Db::initialize` reads it and runs ordered migrations. This
   replaces the ad-hoc `ensure_nsec_columns` check, which becomes migration
   0 → 1 for pre-nsec databases.
3. Add a test helper `assert_query_plan_uses_index(sql, index_name)` in
   `tests/storage.rs` so plan regressions are caught.

**Done when:** the baseline doc is committed and `user_version` migrations exist
with a test that opens a v0 (legacy) database and lands on the current version.

---

## Phase C: Linux and Docker Compatibility Baseline (C1–C5, C7)

This phase lands **before** any performance phase. Phases 4–5 hold more data in
memory, so a clean shutdown must exist first.

**C1: Graceful shutdown**
- Implement `Filesystem::destroy`: flush every dirty inode, then `sync_durable`. The kernel
  sends `FUSE_DESTROY` on unmount.
- Replace the blocking `fuser::mount2` with `fuser::spawn_mount2` (a `BackgroundSession`).
  The main thread waits on SIGTERM, SIGINT and SIGHUP (through `signal-hook`, or `sigwait` on a masked
  set via `nix`). On a signal, drop the session to unmount, which triggers `destroy`,
  then exit 0.
- If the kernel unmounts first (`fusermount3 -u`), the session ends. Exit 0 as well.
- Flush errors during shutdown go to stderr with a non-zero exit code and are never silently dropped.
- Works as PID 1: a handler is installed, so SIGTERM is no longer ignored. The image
  uses `ENTRYPOINT ["dbfs"]` (exec form), and the docs suggest `--init` / `tini`
  for zombie reaping.
- Test: S6 in the matrix, plus a unit test that `destroy` flushes all dirty slots.

**C2: Opt-in `allow_other`**
- CLI: `dbfs mount [--allow-other | --allow-root] [-o key=val ...] <db> <mnt>`. The default
  stays the same, so the existing test keeps asserting it's off by default. Add a test that
  the flag adds `MountOption::AllowOther`.
- With `allow_other`, keep `default_permissions` on. The kernel then enforces mode bits
  for container users, which is essential once other uids can reach the mount.
- If mounting fails with `allow_other` as non-root, print a hint about
  `user_allow_other` in `/etc/fuse.conf`.
- `--auto-unmount` flag (`MountOption::AutoUnmount`, needs fusermount3) so a crashed or
  SIGKILLed dbfs doesn't leave a stale mount on the host.

**C3: Portable builds**
- Cargo feature `bundled-sqlite = ["rusqlite/bundled"]`. Images and release artifacts
  use it, and developers keep system linkage. fuser 0.16 is already built without
  libfuse (it uses `fusermount3` at runtime, or mounts directly as root), so no libfuse
  link is needed.
- Release targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and both
  `*-linux-musl` with `bundled-sqlite`.
- `Dockerfile`: multi-stage build, a `rust:1-alpine` (musl) builder, and a
  `alpine` runtime with `fuse3` (for `fusermount3`) and nothing else. Add a
  `docs/docker.md` with the exact `docker run` / compose flags:
  `--device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined`,
  and `:rshared` propagation for S4.
- Update `AGENTS.md`: the line "no Docker" is no longer true.

**C4: Explicit xattr behaviour**
- `getxattr` and `listxattr` return `ENODATA` and an empty list, and `setxattr` returns `EOPNOTSUPP`.
  Tools treat these as "no xattrs on this filesystem" and carry on. Document that SELinux
  `:z`/`:Z` relabel isn't supported, and to use `--security-opt label=disable` instead.
  (Implemented. `-o context=` was considered, but `fusermount3` rejects it as an unknown
  option for non-root mounts, so the docs recommend `label=disable` only.)
- Real xattr storage (an `xattrs(ino, name, value)` table) is a follow-up if a workload
  needs it, such as `security.capability` on binaries installed into dbfs.

**C5: `fallocate`**
- Mode 0: extend `size` if needed (sparse, because holes already read as zeros) and return 0.
  `FALLOC_FL_KEEP_SIZE`: return 0 and do nothing. Other modes: `EOPNOTSUPP`.
  Space isn't reserved; document that SQLite allocates on write.

**C7: CI**
- A GitHub Actions job runs the S1–S4 and S6 matrix on `ubuntu-latest` (it has
  `/dev/fuse` and Docker), plus a nightly aarch64 run and the Podman rootless run.

**Done when:** the whole compatibility matrix passes, the image is under 20 MB, and
`docker stop` on a container with a 100 MiB unflushed write exits within 2 s
and keeps every byte.

---

## Phase 1: Index `dirents.child_ino` (P1)

**Changes**
- Migration 1 → 2: `CREATE INDEX IF NOT EXISTS dirents_child ON dirents(child_ino);`

**Tests**
- Query plan for `SELECT parent_ino FROM dirents WHERE child_ino = ?` uses
  `dirents_child`.
- The existing unlink, rename and rmdir tests in `tests/storage.rs` and
  `tests/mount_integration.rs` pass unchanged.

**Done when:** unlinking 5,000 files is no slower than creating them (the
baseline is 6×).

**Risk:** small; it's a one-line, additive migration.

---

## Phase 2: Narrow Inode Reads (P3)

**Changes**
- Add `Db::inode_head(ino) -> Result<(FileKind, u64 /*size*/), DbError>` using
  `SELECT kind, size FROM inodes WHERE ino = ?1`.
- Add `Db::inode_kind_of(ino)` (kind only) for the "is this a directory?"
  checks in `list_dir*`, `create_node`, `link` and `rename_entry` (new parent).
- `read_file` and `write_file_batch_at` use `inode_head`.
- `Dbfs::read` (dirty path) and `Dbfs::open` use `inode_head` or `inode_kind_of`.
- `get_inode` stays for `getattr`, `lookup`, `setattr` and `update_metadata`.

**Tests:** no behaviour change, so the existing suite covers it. Add a unit test that
`inode_head` returns `NotFound` for a missing inode and `Corrupt` for a bad kind.

**Risk:** small.

---

## Phase 3: Fill Caller Buffers Instead of Returning Vecs (P6, P8)

**Changes in `src/db.rs`**
- New `pub(crate) fn read_into(&self, ino, offset, out: &mut [u8]) -> Result<usize, DbError>`:
  - one range statement:
    `SELECT chunk_index, data FROM file_chunks WHERE ino = ?1 AND chunk_index BETWEEN ?2 AND ?3 ORDER BY chunk_index`
  - borrow each blob through `row.get_ref(1)?.as_blob()?` and copy straight into `out`
  - leave holes zeroed (the caller zero-initialises `out`)
- `read_file` becomes a wrapper: `inode_head` → allocate `out` once → `read_into`.
- `read_chunk` is replaced by `read_chunk_into(ino, idx, dst: &mut Vec<u8>)`
  (reuses the caller's capacity).
- `truncate_file`: the final chunk is truncated in SQL
  (`UPDATE file_chunks SET data = substr(data, 1, ?len) ...`), with no blob round-trip.
- `write_file_batch_at` reads partial chunks through `get_ref` into a reusable
  scratch `Vec` (one per call, not one per chunk).
- `write_file_batch` signature becomes `&[(u64, &[u8])]`; `write_file` passes
  `&[(offset, data)]` without `to_vec()`. Update the 18 test call sites.

**Changes in `src/fuse.rs`**
- `Dbfs::read` allocates `output` once, calls `db.read_into(..., &mut output)`,
  then overlays dirty chunks. Drop the second buffer and the copy.

**Tests**
- Existing: `writes_and_reads_across_chunk_boundary`,
  `offset_write_zero_fills_gap_on_read`, `read_past_4gib_returns_the_marker_near_that_offset`,
  truncate tests.
- New: a read spanning a hole chunk between two present chunks returns zeros for
  the hole. A read over a short trailing chunk (shorter than `CHUNK_SIZE`) returns
  zeros up to `size`.

**Done when:** randread and the read-while-dirty path show no `Vec` allocation per chunk
in a heaptrack or flamegraph sample, and randread throughput is ≥ baseline.

**Risk:** medium; boundary arithmetic. Keep the math in one helper
(`chunk_span(offset, len) -> (first, last, ...)`) shared by read, write and dirty overlay.

---

## Phase 4: Open-Inode Table Indexed by File Handle (P4)

This is the core structural change. Process-lifetime state moves out of `Db`, so
`Db` becomes stateless SQL storage.

**New module `src/open_table.rs`**

```rust
pub(crate) type Slot = u32;

/// Per-inode state for inodes with ≥1 open handle. Struct-of-arrays, indexed by Slot.
pub(crate) struct OpenInodes {
    ino:      Vec<u64>,
    kind:     Vec<FileKind>,
    size:     Vec<u64>,        // authoritative while open; dbfs is the only writer
    mtime:    Vec<Timestamp>,  // valid when dirty
    refs:     Vec<u32>,
    unlinked: Vec<bool>,       // replaces Db::pending_delete
    dirty:    Vec<DirtyIdx>,   // Phase 5; until then wraps the existing BTreeMap
    by_ino:   HashMap<u64, Slot>, // only touched on open/create/unlink/rename/getattr
    free:     Vec<Slot>,
}

/// FUSE fh -> Slot. fh is the index; several fhs may map to one Slot.
pub(crate) struct Handles { slot: Vec<Slot>, free: Vec<u32> }
```

**Wiring**
- `open` and `create`: look up or insert a slot (fills `kind` and `size` from
  `inode_head`), increment `refs`, allocate an fh, and reply with `opened(fh, 0)` or
  `created(..., fh, 0)`. **The fh is no longer the ino.**
- `read`, `write`, `flush`, `fsync` and `release` use `fh` → `slot`: plain array
  indexing with no hashing and no SQL kind check.
- `write` updates `size[slot]` and `mtime[slot]` in place.
- `getattr` and `lookup` still go through `by_ino` to overlay size and mtime for open
  dirty inodes (one hash lookup, but off the data path).
- `unlink` and `rename`-over: if `by_ino` has the target and the last name is
  removed, set `unlinked[slot] = true` and write `nlink = 0`. `release` with
  `refs → 0 && unlinked` deletes the inode. `Db::drop_unlinked_inode` takes an
  `is_open: bool` argument instead of reading `open_counts`.
- Remove `Db::open_counts`, `Db::inode_kinds`, `Db::pending_delete`,
  `Db::open_file`, `Db::inode_kind` and `Db::release_file`. For tests that call
  `db.open_file(...)` directly (for example `tests/fuse_readonly.rs:615`), add a
  `Dbfs::open_ino(ino) -> fh` test helper and port the call sites.
- `Filesystem` methods already take `&mut self`, so drop the `RefCell`s in `Db` and
  `Dbfs` and make the internal methods take `&mut self`. `Db::conn` becomes a plain
  `Connection`.

**Edge cases to test**
- Two fhs on one inode: release of the first keeps the slot; release of the second
  frees it (port the existing two-handle test in `tests/fuse_readonly.rs`).
- Unlink while open, then write through the fh, then release: the inode and its chunks are gone,
  with no leak after reopen (`reopen_drops_inodes_left_unlinked_while_open`).
- Rename while open: the slot follows the inode, not the name.
- A hard link removed while the other name stays: `unlinked` stays false
  (`unlinking_open_hardlink_preserves_remaining_names`).
- Truncate through `setattr` with an open slot updates `size[slot]`.
- Handle reuse: an fh freed and reallocated never sees stale slot data.

**Done when:** the full suite passes (cargo, `DBFS_RUN_FUSE_TESTS=1` mount tests,
pytest), and the write path in a flamegraph shows no `HashMap` or `RefCell` frames.

**Compatibility notes**
- `destroy` (Phase C) must iterate over every live slot, not just one file.
- Bind-mounted containers (S2) share the same FUSE session, so fhs from host and
  container processes live in one `Handles` table. Add a matrix test with the host
  and a container holding the same file open at once.

**Risk:** high, because it touches every handle-based op. Land it as its own PR, after Phases
1–3, with the dirty buffer still a `BTreeMap` behind the slot so data layout and
ownership change in separate steps.

---

## Phase 5: Chunk Arena for Dirty Data (P5)

**New types (in `src/open_table.rs` or `src/arena.rs`)**

```rust
pub(crate) struct ChunkArena {
    bytes: Vec<u8>,   // capacity = ARENA_CHUNKS * CHUNK_SIZE, allocated once
    len:   Vec<u32>,  // valid bytes in each arena slot
    free:  Vec<u32>,
}

/// Dirty chunks of one open inode, sorted by chunk_index.
pub(crate) struct DirtyIdx(Vec<(u64 /*chunk_index*/, u32 /*arena slot*/)>);
```

**Behaviour**
- `ARENA_CHUNKS = FLUSH_THRESHOLD / CHUNK_SIZE` (1,024 for 64 MiB). The threshold
  becomes a **global** memory budget instead of a per-file one.
- Write lookup: check the last entry first (the sequential fast path), then binary search.
  A new chunk gets an arena slot from `free`. If `free` is empty, flush the slot with
  the most dirty chunks and retry.
- A partial write to a new chunk fills the arena slot through `read_chunk_into` (Phase 3).
- A full-chunk write skips the read.
- Flush: walk `DirtyIdx` in order, pass `&bytes[slot*CHUNK..][..len]` to the
  upsert, return slots to `free`, and clear the `Vec` (keeping its capacity).
- The dirty overlay in `read` walks `DirtyIdx` over the chunk range, with no `BTreeMap::range`.
- Flush failure (`ENOSPC`, trigger abort): leave `DirtyIdx` and the arena slots intact
  (same semantics as today's re-insert).

**Tests**
- Existing: `flush_returns_enospc_when_the_database_is_full`,
  `batched_overlapping_writes_preserve_write_order`, the failed-flush-on-release
  test in `tests/fuse_readonly.rs`.
- New: more than one file's worth of dirty data across several open files triggers
  budget eviction and every file reads back correctly. Write → flush → write reuses
  arena slots (`bytes.capacity()` unchanged).

**Done when:** randwrite (fsync=0) and seqwrite throughput is ≥ the Phase 4 numbers, and steady-state
writes show zero allocations in heaptrack.

**Compatibility notes**
- **Hard dependency on Phase C1.** A global 64 MiB arena means up to 64 MiB of
  acknowledged writes live only in memory. Graceful shutdown must already flush them
  on SIGTERM and `docker stop`.
- Containers often run under a memory cgroup limit. Allocating the arena up front
  counts against it immediately, so allocate it lazily (grow up to the budget,
  never shrink) and expose `--write-buffer` so small containers can set 8M.
- Docker's default stop timeout is 10 s. Flushing 64 MiB must fit comfortably; the S6
  test asserts under 2 s.

**Risk:** medium. The eviction policy is the only new behaviour, so keep it simple
(evict the largest dirty set).

---

## Phase 6: Streaming, Keyset-Paginated readdir (P7, C6)

This phase is a **correctness fix for Linux compatibility** as well as a speed-up:
positional cookies can skip entries when `rm -rf` deletes while it reads (C6). It
ships early (see Sequencing), right after Phase C. Before changing anything, add a
failing test: create 10,000 files in one directory, run `rm -rf` on the mount, and
assert success. Run it on the host and in a bind-mounted container.

**Changes**
- Add a callback API, `Db::for_each_dirent(ino, after: i64, f: impl FnMut(i64 cookie, &[u8] name, u64 ino, FileKind) -> bool)`,
  that steps rows and stops when `f` returns true (reply buffer full). There are no
  per-entry allocations.
- Cookie: give `dirents` a stable `id`. Migration 2 → 3 rebuilds `dirents` as:

  ```sql
  CREATE TABLE dirents (
    id INTEGER PRIMARY KEY,
    parent_ino INTEGER NOT NULL REFERENCES inodes(ino) ON DELETE CASCADE,
    name BLOB NOT NULL,
    child_ino INTEGER NOT NULL REFERENCES inodes(ino) ON DELETE CASCADE
  );
  CREATE UNIQUE INDEX dirents_name  ON dirents(parent_ino, name);
  CREATE INDEX        dirents_child ON dirents(child_ino);
  ```

  Query: `WHERE parent_ino = ?1 AND id > ?2 ORDER BY id`. `dirents_name` can't
  serve this range, because its rowid suffix sorts after `name`. Add
  `CREATE INDEX dirents_scan ON dirents(parent_ino, id)` and verify the plan.
- `rename` within a directory keeps the `id` (an UPDATE, not delete plus insert), so a
  concurrent readdir doesn't see the entry twice.
- FUSE offsets: `.` = 1, `..` = 2, and entries use `id + 2`, so offsets stay monotonic.
- `readdir` output is no longer sorted by name. POSIX doesn't require sorting, but
  check that tests and the pytest harness don't assume it (`list_dir` can keep
  `ORDER BY name` for the `Db`-level tests).

**Tests**
- 50,000-entry directory: the full listing returns each name exactly once.
- Unlinking entries mid-listing doesn't skip surviving entries.
- Query plan for the scan uses `dirents_scan`.

**Done when:** `ls -f` on 50,000 entries scales linearly. Doubling the entries should
roughly double the time.

**Risk:** medium, because the table rebuild touches existing databases. Test the migration on
a copy of a populated database.

---

## Phase 7: Chunk and Timestamp Schema Compaction (P2)

Do this last. It's the only phase that rewrites bulk data on migration.

**Changes (migration 3 → 4)**
- `file_chunks` → a single B-tree keyed by a computed rowid:

  ```sql
  CREATE TABLE file_chunks_v2 (id INTEGER PRIMARY KEY, data BLOB NOT NULL);
  -- id = (ino << 24) | chunk_index   (24 bits × 64 KiB = 1 TiB max file size)
  INSERT INTO file_chunks_v2 SELECT (ino << 24) | chunk_index, data FROM file_chunks;
  DROP TABLE file_chunks; ALTER TABLE file_chunks_v2 RENAME TO file_chunks;
  ```

  - Range read: `WHERE id BETWEEN (ino<<24)|first AND (ino<<24)|last`.
  - The FK cascade is gone, so inode deletion runs
    `DELETE FROM file_chunks WHERE id BETWEEN ino<<24 AND (ino<<24)|0xFFFFFF`
    explicitly in `drop_unlinked_inode`, `remove_dir`, `release` (unlinked) and the
    startup `nlink = 0` sweep. Put it in one helper, `delete_inode_rows(tx, ino)`.
  - `write` past 1 TiB returns `EFBIG`. Add `inode_chunk_key` checks and a test.
  - The ino space shrinks to 2^39, which is fine.
- Timestamps: replace each `(secs, nsec)` column pair with a single `*_ns INTEGER`
  (i64 nanoseconds, range 1677–2262). Times outside that range from `utimensat`
  are **clamped**, not rejected. `tar -x` of image layers and `cp -p` must never fail
  on odd timestamps; ext4 also clamps rather than erroring. Test with `touch -d @-1`,
  `@0`, and the far future.
- Linux compatibility: files above 1 TiB return `EFBIG`, the same errno ext4 uses
  past its limit, so tools report it correctly.
- Migration safety on volumes: the database often lives on a Docker volume and can be
  killed mid-migration (`docker stop` timeout). Run the migration in a single
  transaction, and don't start it while handling a shutdown signal. Log progress on
  large databases so it doesn't look hung.
- Test coupling: `tests/storage.rs` (`count_rows ... FROM file_chunks WHERE ino = ?1`)
  and the `BEFORE INSERT ON file_chunks` trigger in `tests/fuse_readonly.rs` need
  updating. Add `count_chunks(path, ino)` test helpers so tests stop hard-coding
  the layout.

**Done when:** seqwrite and randwrite show fewer B-tree pages written per chunk in the
`sqlite3BtreeInsert` share of the flamegraph, and the migration on a 1 GiB database
finishes and preserves contents (byte-for-byte checksum test).

**Risk:** the highest of all phases (data migration). Run it inside one transaction. If it fails, the database
stays on v3.

---

## Sequencing and PRs

Compatibility work comes first, then the performance phases.

| PR | Phases | Depends on |
|---|---|---|
| 1 | 0 + 1 | nothing; ship first (migrations framework plus the index) |
| 2 | C (shutdown, `--allow-other`, xattr, fallocate, bundled sqlite, Dockerfile, compat CI) | PR 1 for the baseline only; can be developed in parallel |
| 3 | 6 (readdir, the `rm -rf` correctness fix) | PR 1 (migrations), PR 2 (compat CI to prove the fix in containers) |
| 4 | 2 + 3 | nothing |
| 5 | 4 | PR 2 (`destroy` exists), PR 4 (`inode_head`, `read_into`) |
| 6 | 5 | PR 5, plus C1 graceful shutdown (hard requirement) |
| 7 | 7 | last; needs PR 1 and PR 5 (single delete path) |

Each PR includes its benchmark results doc and passes:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
cargo build --release --features bundled-sqlite --target x86_64-unknown-linux-musl
DBFS_RUN_FUSE_TESTS=1 cargo test --test mount_integration -- --ignored
pytest tests/test_with_libfuse.py -v -k "not syscall"
DBFS_RUN_DOCKER_TESTS=1 tests/compat/run.sh      # S1–S4, S6 and container workloads
```

A performance PR that regresses any compatibility test doesn't merge, whatever its
benchmark numbers.

## Open Questions

1. Phase 6 drops name-sorted `readdir`. Is anything downstream relying on sorted
   output? (Linux tools don't; `ls` sorts on its own.)
2. Phase 7 caps files at 1 TiB and clamps timestamps to 1677–2262. Acceptable?
3. Which Docker modes matter most: bind-mounting a host dbfs (S2), running dbfs
   inside the container (S3), or both? This decides whether PR 2 also ships a Docker
   volume plugin (`docker volume create -d dbfs`), which isn't in this plan.
4. Is rootless Podman (S5) a must-have or best-effort?
5. Do binaries installed onto dbfs need file capabilities (`security.capability` xattr)?
   If yes, real xattr storage moves from follow-up into Phase C.
