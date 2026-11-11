# dbfs Architecture

## System Overview

```
 ┌──────────────────────────────────────────────────────────────────┐
 │  User Process  (echo hello > /mnt/hello.txt)                    │
 └──────────────────────┬───────────────────────────────────────────┘
                        │ syscall (write, read, open, stat, ...)
                        ▼
 ┌──────────────────────────────────────────────────────────────────┐
 │  Linux VFS                                                      │
 │  ┌────────────────────────────────────────────────────────────┐  │
 │  │  FUSE kernel module  (/dev/fuse)                           │  │
 │  │  • Translates VFS ops into FUSE requests                   │  │
 │  │  • Queues requests on /dev/fuse fd                         │  │
 │  │  • Receives replies and completes syscalls                 │  │
 │  └────────────────────┬───────────────────────────────────────┘  │
 └───────────────────────┼──────────────────────────────────────────┘
                         │ /dev/fuse read/write
                         ▼
 ┌──────────────────────────────────────────────────────────────────┐
 │  fuser crate  (userspace FUSE library)                          │
 │  • Reads requests from /dev/fuse                                │
 │  • Dispatches to Filesystem trait methods                       │
 │  • Serializes replies back to /dev/fuse                         │
 └────────────────────────┬─────────────────────────────────────────┘
                          │ Filesystem trait calls
                          ▼
 ┌──────────────────────────────────────────────────────────────────┐
 │  Dbfs  (src/fuse.rs)                   FUSE interface layer     │
 │  • Maps FUSE ops → Db methods, DbError → errno                  │
 │  • table: Mutex<OpenTable>   one lock per operation, shared     │
 │    with the shutdown path (signal thread)                       │
 │  ┌────────────────────────────────────────────────────────────┐ │
 │  │  OpenTable  (src/open_table.rs, src/arena.rs)              │ │
 │  │  handles: fh → slot          (FUSE fh is an array index)   │ │
 │  │  inodes:  struct of arrays indexed by slot                 │ │
 │  │           ino, kind, size, mtime, refs, unlinked, dirty    │ │
 │  │           by_ino: HashMap  (open/unlink/getattr only)      │ │
 │  │  arena:   ChunkArena, 64 KiB slots, global budget          │ │
 │  │           (--write-buffer, default 256 MiB, grown lazily)  │ │
 │  └────────────────────────────────────────────────────────────┘ │
 └────────────────────────┬─────────────────────────────────────────┘
                          │ Db method calls
                          ▼
 ┌──────────────────────────────────────────────────────────────────┐
 │  Db  (src/db.rs)                       Storage layer            │
 │  • conn: Mutex<Connection> (rusqlite), prepare_cached for SQL   │
 │  • No per-process file state: callers say whether an inode is   │
 │    open (unlink_entry, rename_entry)                            │
 │  • Schema migrations via PRAGMA user_version (currently 5)      │
 └─────────────────────┬────────────────────────────────────────────┘
                       │ SQL via rusqlite (prepare_cached)
                       ▼
 ┌──────────────────────────────────────────────────────────────────┐
 │  SQLite  (WAL mode, synchronous=NORMAL)                         │
 │  ┌─────────────────────────────────────────────────────────┐    │
 │  │  database.sqlite          database.sqlite-wal           │    │
 │  │  ┌─────────────┐         ┌───────────────────────┐      │    │
 │  │  │ B-tree pages│ ◄───────│ WAL frames            │      │    │
 │  │  │ (main db)   │  ckpt   │ (append-only journal) │      │    │
 │  │  └─────────────┘         └───────────────────────┘      │    │
 │  └─────────────────────────────────────────────────────────┘    │
 └──────────────────────┬──────────────────────────────────────────┘
                        │ pread/pwrite syscalls
                        ▼
 ┌──────────────────────────────────────────────────────────────────┐
 │  Host Filesystem  (ext4 / btrfs / ...)                          │
 └──────────────────────────────────────────────────────────────────┘
```

## Module Structure

```
 src/
 ├── main.rs          Entry point: Cli::parse() → run()
 ├── cli.rs           Cli, Command::Mount { database, mountpoint, MountConfig }
 ├── run.rs           run() → Db::open() → Dbfs::with_write_buffer() → fuser::Session
 │                    on a thread; main thread waits for SIGTERM/SIGINT/SIGHUP or
 │                    session end
 ├── fuse.rs          Dbfs: impl Filesystem, FUSE↔Db glue, flush and eviction
 ├── open_table.rs    OpenTable: handle table and struct-of-arrays open-inode table
 ├── arena.rs         ChunkArena (dirty chunk bytes) and DirtyIdx (per-inode index)
 ├── blocks.rs        4 KiB block masks of buffered chunks, merge of dirty blocks into a row
 ├── stats.rs         Opt-in counters (--stats), printed as one JSON line at exit
 ├── db.rs            Db: SQLite storage layer, all SQL, chunk I/O, migrations
 ├── checkpoint.rs    WAL checkpoints: commit hook, background thread, reset, --wal-cap
├── batch.rs         Metadata batch: the open transaction, commit points, fail-stop, commit timer
 └── model.rs         FileKind, Inode, DirEntry, MetadataUpdate, Timestamp, DbError
```

## SQLite Schema (version 5)

```
 ┌─────────────────────────────────────────────┐
 │  inodes                                     │
 │─────────────────────────────────────────────│
 │  ino       INTEGER PRIMARY KEY AUTOINCREMENT│
 │            (never reused; FUSE node id)     │
 │  kind      INTEGER  (1=file, 2=dir, 3=sym)  │
 │  mode, uid, gid, size, nlink   INTEGER      │
 │  atime_ns, mtime_ns, ctime_ns  INTEGER      │
 │            (i64 ns since epoch; clamped to  │
 │             1677..2262, never rejected)     │
 └──────────────────────┬──────────────────────┘
                        │ ino
          ┌─────────────┴──────────────┬───────────────────────┐
          │ FK (cascade)               │ key range (explicit)  │ FK (cascade)
          ▼                            ▼                       ▼
 ┌───────────────────────┐ ┌──────────────────────────┐ ┌───────────────┐
 │  dirents              │ │  file_chunks             │ │symlink_targets│
 │───────────────────────│ │──────────────────────────│ │───────────────│
 │  id  INTEGER PK       │ │  id  INTEGER PK          │ │  ino   PK, FK │
 │      (readdir cookie) │ │      = ino << 24         │ │  target  BLOB │
 │  parent_ino  FK       │ │        | chunk_index     │ │               │
 │  name        BLOB     │ │  data  BLOB (≤ 64 KiB)   │ │               │
 │  child_ino   FK       │ │                          │ │               │
 │───────────────────────│ │  one B-tree; a file is   │ │               │
 │  UNIQUE dirents_name  │ │  one key range; max      │ │               │
 │    (parent_ino, name) │ │  2^24 chunks = 1 TiB     │ │               │
 │  dirents_scan         │ │  (EFBIG past it)         │ │               │
 │    (parent_ino[, id]) │ │                          │ │               │
 │  dirents_child        │ │                          │ │               │
 │    (child_ino)        │ │                          │ │               │
 └───────────────────────┘ └──────────────────────────┘ └───────────────┘

 Directory tree example:

   inodes:  ino=1 (root dir)    ino=2 (hello.txt)    ino=3 (docs/ dir)
   dirents: (id=1, 1, "hello.txt", 2)  (id=2, 1, "docs", 3)
   chunks:  (2<<24 | 0, <64K blob>)    (2<<24 | 1, <64K blob>)  ...
```

Deleting an inode (`delete_inode_rows`) deletes its chunk key range and then the
inode row; entries and the symlink target follow through their cascades.

## Write Data Flow

```
 User: write(fd, "hello", 5) at offset 0
                │
                ▼
 ┌──────────────────────────────────────┐
 │  FUSE_WRITE(fh=0, off=0, "hello")    │
 └──────────────┬───────────────────────┘
                ▼
 ┌──────────────────────────────────────────────────────────┐
 │  Dbfs::write_fh(fh=0, 0, "hello")   (table lock held)    │
 │                                                          │
 │  1. slot = handles[fh]            array index, no hash   │
 │     kind[slot] = RegularFile? ✓   no SQL                 │
 │                                                          │
 │  2. For each chunk the write covers (ChunkSpan):         │
 │     dirty[slot].find(chunk)       last entry, then       │
 │                                   binary search          │
 │     ├── found: write, mark the touched blocks dirty      │
 │     └── new:   arena.alloc()                             │
 │                ├── full? write up to 256 of the inode's  │
 │                │   lowest dirty chunks (16 MiB), retry   │
 │                ├── partly covered blocks: blob read      │
 │                │   into the arena slot (holes: zeros)    │
 │                └── insert (chunk, arena slot), sorted    │
 │                                                          │
 │  3. size[slot] grows chunk by chunk; mtime[slot] = now   │
 │  4. Return Ok(5)          ── no SQLite write, no alloc ──│
 └──────────────────────────────────────────────────────────┘
```

Each buffered chunk keeps a 16-bit mask with one bit per 4 KiB block (`blocks.rs`). A write sets the bits of the blocks it touches. A block that the write covers only in part is first read from SQLite, one blob read of that block (zeros for a hole), so a dirty block always holds its complete contents. A block the write covers whole needs no read. The blocks that are not dirty are unknown: their bytes in the slot may be stale or zero. Reads take them from SQLite, and a flush keeps their stored bytes.

## Flush Data Flow (on close, fsync, setattr, or a full write buffer)

Close flushes dirty bytes into the open metadata batch, which does not commit them
(see [Metadata Batches](#metadata-batches)). `fsync` does that flush, commits the
batch, and then calls `fdatasync` on the WAL file, or on the main database file when
no WAL file is present. A file unlinked while open is not flushed on close or fsync: its data
dies with its last handle, which deletes the inode.

```
 User: close(fd)  or  fsync(fd)  or  a write that finds the arena full
                │
                ▼
 ┌──────────────────────────────────────────────────────────┐
 │  Shared::flush_slot(slot)                               │
 │                                                         │
 │  Db::replace_chunks(ino, size[slot],                    │
 │      dirty[slot] as (chunk, bytes, mask), mtime)        │
 │  On success: arena slots back to the free list,         │
 │  dirty[slot] cleared (capacity kept).                   │
 │  On failure: everything stays buffered for a retry.     │
 └────────────────────┬────────────────────────────────────┘
                      ▼
 ┌──────────────────────────────────────────────────────────┐
 │  Db::replace_chunks                                     │
 │                                                         │
 │  1. BEGIN TRANSACTION                                   │
 │  2. For each buffered chunk:                            │
 │     all 16 blocks dirty: upsert the slot's bytes        │
 │     else: SELECT the stored row, merge the dirty blocks │
 │     (blocks::merge), then upsert the result             │
 │  3. UPDATE inodes SET size, mtime_ns, ctime_ns          │
 │  4. COMMIT  ──► SQLite appends WAL frames               │
 └─────────────────────────────────────────────────────────┘
```

A chunk with all 16 blocks dirty is stored straight from its slot. Any other chunk gets its stored row with the dirty blocks merged in. The row keeps its length when that already reaches the dirty data. A shorter row is extended with zeros only as far as the end of its highest dirty block, and never past the end of the file. It is not extended to the end of the chunk, because a sparse one-byte write would then store a full 64 KiB row. The model tests check that no row runs past the end of its file.

Patching the blob in place, without rewriting the row, is a possible follow-up. It applies only when the row already reaches the dirty data. On the development VM it took about 51 µs per partly dirty chunk, against 65–77 µs for read, patch and upsert, with about the same WAL size (14–15 KiB). Before this change, a partly dirty chunk was upserted whole: about 200 µs and 65 KiB of WAL.

### Eviction slices

When the arena has no free slot, a write does not flush a whole file. The inode
with the most dirty chunks gives up its lowest-indexed dirty chunks, at most 256
(16 MiB), in one transaction (`Shared::flush_chunks` with `FLUSH_SLICE_CHUNKS`),
and their slots return to the arena. The rest of that file stays buffered and
reads still overlay it. A large file is therefore written out over many bounded
evictions, and no request waits for a whole large file. The eviction policy is
otherwise unchanged. `fsync`, `flush`, `release`, truncate and shutdown still write
all of a file's buffered chunks, in one transaction.

A failed slice leaves every chunk buffered and fails the write that needed the
slot with the database's errno (for example `ENOSPC`). Each slice also stores the
file's current size. After a crash, a file can therefore show a size whose
unflushed chunks read as zeros, and some evicted slices can be kept while others
are lost. That is the same status as any unsynced data: only `fsync` or unmount
make it durable.

Memory: the arena reserves a 64 KiB slot when the write path needs one, up to
`--write-buffer`, and never gives it back until unmount. A mount that has
buffered little uses little RAM. One that has filled the budget keeps that much
RSS for its buffers, even after the data is flushed, because the slots are reused. Each slot also keeps a 2-byte dirty-block mask.

`release` drops one handle. The last handle of an inode unlinked while open
deletes the inode; otherwise the slot is flushed and, with no handles and no dirty
data left, freed.

## Metadata Batches

Every change to the database runs inside one open SQLite transaction, the batch, held on
the main connection (`src/batch.rs`). The first operation after a commit runs
`BEGIN IMMEDIATE`. Later operations join the batch with no savepoint. A batch commits at
a commit point:

- `fsync` and `fsyncdir` (`Db::sync_durable`), and `statfs`;
- unmount, shutdown, and the `Db` closing;
- 5 ms after the batch's first operation, from the commit timer thread (`dbfs-commit`);
- once it holds 256 operations or 64 MiB of chunk data;
- before the checkpoint thread's reset handshake, which then runs PASSIVE.

`flush` and `release` write into the batch and do not commit. Every operation commits as
it returns in four cases: `--sync-metadata`; read-only mounts, which open the database
without background checkpoints; in-memory databases; and while free space on the
database's filesystem is below 256 MiB. The free-space check uses `statvfs` and runs at
most once a second.

**Validation before writes.** An operation checks every failure it can before its first
write: missing or existing names, non-empty directories, wrong kinds, the file size
limit, and the name length. A failed check therefore changes nothing, and the batch goes
on. A failure that SQLite reports after a write is different. The operation then rolls
the transaction back, detected by `total_changes()` moving or `is_autocommit()` turning
true. SQLite rolls back the whole transaction by itself on `SQLITE_FULL` (checked with a
probe), so a full disk loses every operation in the batch, not only the failing one.

**Fail-stop.** A failed COMMIT, or a rollback that discards acknowledged operations, sets
a sticky failed state in `Main`. dbfs logs the reason once to stderr, counts it in
`batch_failures`, and then refuses later mutating operations and `fsync`/`fsyncdir` with
`EIO`. Reads continue, and unmount still works. The state lasts until the filesystem is
remounted. When operations commit one by one, a failed COMMIT fails only the operation
that caused it, as `ENOSPC` did before batching.

**Kill and power loss.** A kill -9 can lose the changes since the last commit point, which
is up to 5 ms or 256 operations of acknowledged, unsynced changes. `--sync-metadata` removes
that window for metadata operations. Power loss is unchanged, since each commit is atomic
and synced only by `fsync` and `fsyncdir`.

**Locks.** The FUSE thread takes the table lock and then the `Main` mutex. The commit timer
and the checkpoint thread take only the `Main` mutex, so they never wait for the table.

## WAL Checkpoints

Commits never checkpoint (`wal_autocheckpoint = 0`), so a write waits for a checkpoint only when the WAL is over its cap or while a reset handshake holds the database lock (see below). The code is in `src/checkpoint.rs`.

- **Frame count.** A commit hook on the main connection stores how many frames the WAL holds. The hook runs no SQL and takes no lock, so a commit pays for a few atomic stores.
- **Background thread.** A thread with its own connection runs `PRAGMA wal_checkpoint(PASSIVE)` when the WAL is over 64 MiB, or half of `--wal-cap` if that is smaller, so a small cap still gets background work. It also runs one after a second without a commit, when the WAL holds frames it has not reset.
- **Reset handshake.** SQLite restarts a WAL only when every frame is backfilled and no reader is in the way. Under sustained writes a PASSIVE pass seldom ends exactly at the last frame, and on the development VM a pass does not outrun the writers: each pass backfills about what was appended during the previous one. The thread therefore catches up without the database lock, for at most 8 passes, and stops once the WAL reaches `forced_at`, which is `min(3 × passive_at, 3/4 × wal_cap)`: 192 MiB by default. It then takes the main connection's mutex, commits the open batch so that no write transaction is open, runs PASSIVE once more, and releases the mutex. The next writer restarts the WAL at offset 0. While the thread waits for the mutex it sets a flag, and writers pause briefly before they take the mutex, so the thread gets it; a std mutex otherwise lets the writer that holds it relock at once. Measured with `forced_at` at 256 MiB, the lock was held 130 to 250 ms per handshake under a 512 MiB sequential write, and writers waited 0 to 30 ms. The hold is the backlog the passes could not catch up. SQLite can also restart the WAL on its own when it happens to be fully backfilled; the handshake makes the reset happen when the thread has caught up.
- **Size bound (`--wal-cap SIZE`).** The default is 1 GiB and the minimum 64 KiB. The handshake keeps the WAL far below the cap under sustained writes, so this is a backstop. A writer that leaves the WAL larger than the cap runs `PRAGMA wal_checkpoint(RESTART)` on its own connection before it returns, so that one write waits. The cap is checked after each commit, so the WAL can exceed it by the transaction that crossed it. Flush transactions write at most 16 MiB, so the overshoot is at most 16 MiB. A busy RESTART, which happens while the thread is in a long PASSIVE pass, is retried for up to 60 s. Read-only mounts never checkpoint.
- **File size.** When the next writer restarts the WAL after a reset, SQLite truncates the `-wal` file to `journal_size_limit`, which is 64 MiB. Measured on SQLite 3.46.1 with an 8 MiB limit: a 22 MB WAL, fully backfilled, shrank to 8 MiB at the next write. So an idle database keeps at most 64 MiB of WAL on disk, and unmount truncates it to zero (see [Shutdown](#shutdown)).
- **Sticky errors.** A checkpoint that fails, other than by being busy, is logged to stderr and recorded. The next `fsync` or `fsyncdir` on the mount returns `EIO`, once. The error therefore reaches an application that syncs, even though the failure happened in the background.
- **Not checkpointed.** In-memory databases have no WAL. Read-only mounts run no thread and do not truncate the WAL at unmount.
- **`statfs`.** Free space is the free pages minus the pages the WAL occupies, so the WAL counts as used space.

## Shutdown

Buffered writes exist only in memory until they are flushed, so shutdown has to flush them. On SIGTERM, SIGINT or SIGHUP, `run` does the following:

1. Refuses new buffered writes. They fail with `EIO`, so nothing is acknowledged and then dropped.
2. Calls `Dbfs::flush_all`, which writes every dirty inode to SQLite, then runs `sync_durable`.
3. Detaches the mount with `fusermount -u -z`.
4. Waits up to 1 s for the FUSE session to end. The session ends at once when nothing has a file open. The kernel then sends `FUSE_DESTROY`, and `Filesystem::destroy` flushes again (normally a no-op). Then it stops and joins the checkpoint thread, and truncates the WAL to zero bytes with `PRAGMA wal_checkpoint(TRUNCATE)`. The thread stops before the connection closes, so it never runs on a closed connection.
5. Exits 0. If a flush failed, it prints the error to stderr and exits 1.

Step 2 runs on the signal path, not only in `destroy`, because a process that still has a file open keeps the session alive after a lazy detach. An external `fusermount3 -u` skips steps 1–3: the session ends, `destroy` flushes, truncates the WAL and stops the thread, and dbfs exits 0.

## Read Data Flow

The kernel answers a repeated read from its page cache when the file has not changed, so this flow runs only for reads the cache misses. See [Kernel caching](#kernel-caching).

```
 User: read(fd, buf, 4096) at offset 0
                │
                ▼
 ┌──────────────────────────────────────────────────────────┐
 │  Dbfs::read_fh(fh, offset=0, size=4096)                 │
 │                                                         │
 │  1. slot = handles[fh]; size[slot] caps the read        │
 │     (authoritative while open: no inode query)          │
 │  2. output = vec![0; len]       one allocation          │
 │  3. Db::read_into(ino, 0, &mut output)                  │
 │  4. For each dirty chunk in range (DirtyIdx::range):    │
 │     overlay its dirty blocks onto output (mask)         │
 └────────────────────┬────────────────────────────────────┘
                      ▼
 ┌──────────────────────────────────────────────────────────┐
 │  Db::read_into(ino, offset, out)                        │
 │                                                         │
 │  Inside one chunk, not the whole of it (4 KiB read):    │
 │    blob_open(file_chunks.data, key(ino, chunk))         │
 │    read_at_exact(out, offset in chunk, len)             │
 │    only those bytes are read; no row (hole): zeros      │
 │                                                         │
 │  Otherwise (several chunks, or a whole chunk):          │
 │  SELECT id, data FROM file_chunks                       │
 │    WHERE id BETWEEN key(ino, first) AND key(ino, last)  │
 │    ORDER BY id                  one range statement     │
 │  Each blob is borrowed (get_ref) and copied straight    │
 │  into out; holes and short chunks stay zero.            │
 └──────────────────────────────────────────────────────────┘
```

The overlay copies only the dirty 4 KiB blocks of each buffered chunk. Every other block comes from SQLite, so stale bytes left in a reused slot are never returned.

## Lookup / Path Resolution

```
 User: stat("/mnt/docs/notes.txt")
                │
                ▼
 Kernel splits path into components and calls FUSE lookup
 for each one:

   lookup(parent=1, name="docs")
     │
     ▼
   Db::lookup(1, "docs")
     SELECT i.* FROM dirents d JOIN inodes i
       ON i.ino = d.child_ino
       WHERE d.parent_ino = 1 AND d.name = 'docs'
     → Inode { ino=3, kind=Directory, ... }

   lookup(parent=3, name="notes.txt")
     │
     ▼
   Db::lookup(3, "notes.txt")
     SELECT i.* FROM dirents d JOIN inodes i ...
       WHERE d.parent_ino = 3 AND d.name = 'notes.txt'
     → Inode { ino=7, kind=RegularFile, ... }

   getattr(ino=7)
     │
     ▼
   Db::get_inode(7) → FileAttr { size, mode, uid, ... }
```

## Unlink Data Flow

```
 User: rm /mnt/hello.txt
                │
                ▼
 ┌────────────────────────────────────────────────────┐
 │  Dbfs::unlink(parent=1, name="hello.txt")         │
 │                                                    │
 │  1. Db::lookup(1, "hello.txt")                    │
 │     → inode { ino=2, nlink=1, ... }               │
 │  2. is_open = by_ino[2] has refs > 0              │
 │  3. Db::unlink_entry(1, "hello.txt", &inode,      │
 │                      is_open)                     │
 │  4. Last name removed?                            │
 │     open   → unlinked[slot] = true (deleted on    │
 │              the last release)                    │
 │     closed → drop its slot and dirty data unwritten│
 └──────────────────────┬─────────────────────────────┘
                        ▼
 ┌────────────────────────────────────────────────────┐
 │  Db::unlink_entry                                  │
 │                                                    │
 │  BEGIN TRANSACTION                                 │
 │  ├── DELETE FROM dirents                           │
 │  │   WHERE parent_ino=1 AND name='hello.txt'      │
 │  ├── nlink > 1 or is_open?                         │
 │  │    YES → UPDATE inodes SET nlink = nlink - 1    │
 │  │    NO  → delete_inode_rows(2): chunk key range, │
 │  │          then the inode (cascade: dirents,      │
 │  │          symlink target)                        │
 │  └── UPDATE parent mtime_ns/ctime_ns               │
 │  COMMIT                                            │
 └────────────────────────────────────────────────────┘
```

A crash leaves inodes unlinked while open with `nlink = 0`; the next start
deletes them.

`rename` over an existing name follows the same rules for the replaced inode.
Open handles follow the inode, not the name, so a rename flushes nothing.

## Readdir Data Flow

Offsets are stable cookies, not positions: `.` is 1, `..` is 2, and an entry
with dirent id `n` is `n + 2`. Deleting entries between calls, as `rm -rf`
does, never skips or repeats the remaining ones.

```
 User: ls /mnt/
                │
                ▼
   readdir(ino=1, offset=0)
     │
     ▼
   ┌──────────────────────────────────────────────────┐
   │  Dbfs::readdir_stream(ino=1, offset=0, emit)     │
   │                                                  │
   │  offset < 1 → emit "."  (ino=1)                 │
   │  offset < 2 → emit ".." (parent_of_dir)         │
   │  Db::read_dir_after(1, after_id=0, emit)        │
   │    SELECT d.id, d.name, d.child_ino, i.kind     │
   │    FROM dirents d JOIN inodes i ...              │
   │    WHERE d.parent_ino=1 AND d.id > 0            │
   │    ORDER BY d.id        ◄── dirents_scan, no sort│
   │  Names are borrowed from SQLite and go straight │
   │  into reply.add(); stops when the reply is full. │
   └──────────────────────────────────────────────────┘

   readdir(ino=1, offset=last cookie)  → resumes at id > cookie - 2
```

## Extended Attributes and fallocate

dbfs stores no extended attributes and reserves no space. Both are answered
explicitly, so tools see "no xattrs on this filesystem" rather than the `ENOSYS`
that fuser returns for unimplemented operations. The one deliberate `ENOSYS` is
`getxattr`'s own reply, explained below.

### Extended attributes

| Call | Existing inode | Missing inode |
|---|---|---|
| `getxattr` | dbfs replies `ENOSYS`; the caller sees `EOPNOTSUPP` | `ENOENT` |
| `listxattr` | empty list (size probe returns 0) | `ENOENT` |
| `setxattr` | `EOPNOTSUPP` | `ENOENT` |
| `removexattr` | `ENODATA` | `ENOENT` |

`getxattr` replies `ENOSYS` on purpose. The kernel reads that as "not implemented",
sets `fc->no_getxattr` for the mount, and from then on answers `getxattr(2)` with
`EOPNOTSUPP` itself without sending a request. This matters because
`file_remove_privs` probes `security.capability` with `getxattr` before every
`write(2)`. With the earlier `ENODATA` reply, each write cost a round trip to the
daemon (measured: 4 KiB random writes +11%, creating 5,000 files 12% less time with
`ENOSYS`). Through a mount, the first `getxattr` is the only one dbfs sees. The
`ENOENT` row is what the `Dbfs` API returns for a missing inode. `listxattr`,
`setxattr` and `removexattr` are unchanged. `setxattr` returns `EOPNOTSUPP`, not
`ENOSYS`, so it sets no latch.

On a real mount, `cp -a`, `cp --preserve=all`, `tar --xattrs` (create and extract),
and `rsync -aX` all succeed on trees without xattrs. Python's `os.listxattr` returns
`[]`, and an explicit `os.setxattr` fails with `EOPNOTSUPP` (errno 95, also
`ENOTSUP` on Linux).

Trees that do carry xattrs (for example a host file with `user.k`) behave as follows:

| Tool | Result on the mount |
|---|---|
| `cp -a` | exit 0, xattr dropped without a message (observed) |
| `cp --preserve=xattr` | exit 1, `setting attributes ...: Operation not supported` |
| `tar --xattrs -xf` | `Cannot set 'user.k' extended attribute ...: Operation not supported`, exit 0 |
| `rsync -aX` | exit 23, `lsetxattr(...) failed: Operation not supported` |

Those errors come from the explicit `setxattr` refusal. Accepting and discarding
writes would make the tools quieter, but it would also report success for data that
was not stored.

SELinux relabelling (`:z` / `:Z` on a volume) writes `security.selinux` through
`setxattr`, which is refused. Relabelling is therefore unsupported. Use
`--security-opt label=disable` on the container instead. Real attribute storage
(an `xattrs(ino, name, value)` table) is a follow-up, needed only by workloads
such as `security.capability` on installed binaries.

### fallocate

```
fallocate(ino, offset, length, mode)
  ├─ offset < 0 or length <= 0 ─────────────► EINVAL
  ├─ mode has bits other than KEEP_SIZE ────► EOPNOTSUPP
  ├─ inode missing ─────────────────────────► ENOENT
  ├─ not a regular file ────────────────────► ENODEV
  ├─ offset + length overflows i64 ─────────► EFBIG
  ├─ offset + length > 1 TiB ───────────────► EFBIG
  ├─ mode == KEEP_SIZE ─────────────────────► OK, nothing changes
  └─ end = offset + length; size = size[slot] if open, else stored size
       end <= size ─────────────────────────► OK, nothing changes (never shrinks)
       end >  size ─► flush the slot, truncate_file(ino, end), size[slot] = end
                       size = end, mtime and ctime = now
                       holes are sparse and read as zeros
```

| Mode | Result |
|---|---|
| `0` | Extends the file to `offset + length` when that is past the end. Otherwise no change. |
| `FALLOC_FL_KEEP_SIZE` alone | Succeeds without changing anything. |
| Any other bit (`PUNCH_HOLE`, `ZERO_RANGE`, `COLLAPSE_RANGE`, `INSERT_RANGE`, `UNSHARE_RANGE`) | `EOPNOTSUPP` |

When the call extends the file, the reply carries no attributes, although dbfs has already set the size, mtime and ctime. dbfs therefore tells the kernel to drop its cached attributes for the inode, so the next `stat` asks dbfs again (see [Kernel caching](#kernel-caching)).

Space is not reserved. SQLite allocates pages when chunks are written, so a
`fallocate` that succeeds can still be followed by `ENOSPC` on write. The `ENODEV`
check mirrors Linux's `vfs_fallocate`, which refuses non-regular files before the
call reaches FUSE, so user space should not normally see it.

## Kernel Caching

A dbfs mount has two kernel caches: the page cache for file data, and the cached attributes and names (the attribute and entry timeouts). The mount mode sets both.

| Mode | Page cache across opens | Attribute and name timeout | Request and capability options |
|---|---|---|---|
| Default | Kept (`FOPEN_KEEP_CACHE`) | 1 day | 128 KiB requests |
| `--no-keep-cache` | Dropped on every open | 1 second | 128 KiB requests |
| `--kernel-cache` | Kept | 1 day | Writeback cache, parallel directory operations, 1 MiB requests, cached symlinks, async direct I/O |

`--kernel-cache` always keeps the page cache. It conflicts with `--no-keep-cache`, and the command line rejects the two together.

### Why a day is safe

dbfs is the only writer of its database (the `<db>.lock` file keeps out a second dbfs), and every change to the filesystem goes through the kernel. The kernel therefore learns of each change from its own request or from the reply to it. Three rules keep the caches correct:

- Every operation that changes attributes (size, mtime, ctime, mode, owner or link count) or the entries of a directory replies with the fresh attributes or entry. The kernel updates its own size and mtime for its writes, and it drops a directory's cached attributes when it changes the directory's entries.
- Two changes come with no attributes in the reply, so dbfs invalidates the inode with `Notifier::inval_inode` (`Shared::invalidate_attr`): a rename over a name whose target has other links or an open handle (the target's link count and ctime change), and an `fallocate` that extends the file (size, mtime and ctime change). The notification drops attributes only (offset -1). A notification that also drops pages can wait for page locks, and a write in flight holds those locks until this thread replies. That would deadlock the single session thread.
- Inode numbers are never reused: `inodes.ino` is `AUTOINCREMENT` (schema version 5). A cached attribute or name cannot later describe a different file.

The mount tests in `tests/mount_integration.rs` check these cases in every mode: the warm re-read after a reopen, size and mtime seen through another fd, rename over a hard link and over an open file, directory link counts, and fallocate.

### What `--kernel-cache` turns on

Each item is gated because it changes how the kernel buffers data or how many requests are in flight.

- **Writeback cache** (`FUSE_WRITEBACK_CACHE`). The kernel holds written data and sends it to dbfs later. A write that has returned can exist only in kernel memory until the file is flushed, synced or closed, so a crash, SIGKILL or `docker stop` can lose it, and dbfs never saw those bytes. This is the main reason the flag is off by default.
- **Parallel directory operations** (`FUSE_PARALLEL_DIROPS`). Lookups and readdir calls on one directory can run at the same time. Without it, the kernel sends them one at a time per directory.
- **1 MiB requests** (`max_write`, `max_readahead`, `FUSE_MAX_PAGES`). Large transfers take fewer requests, and each request can hold up to 1 MiB of buffers. The default stays at 128 KiB.
- **Cached symlinks** (`FUSE_CACHE_SYMLINKS`). The kernel keeps `readlink` targets. dbfs never changes a symlink's target after creation, and inode numbers are never reused, so a cached target cannot go stale.
- **Async direct I/O** (`FUSE_ASYNC_DIO`). The kernel can have several direct I/O requests to one file in flight. Without it, the kernel waits for each request before it sends the next.

### Editing the database

Do not edit the database file with `sqlite3` while dbfs has it mounted. The lock stops a second dbfs, not sqlite3. dbfs does not invalidate its caches for changes made behind its back, so the mount can show stale data, and a later dbfs write to the same chunk overwrites the edit.

## Statistics

`dbfs mount --stats` or `DBFS_STATS=1` turns on process-wide counters (`src/stats.rs`).
When the mount ends (unmount, `fusermount3 -u`, or SIGTERM, SIGINT or SIGHUP), dbfs
writes one line to stderr:

```text
{"dbfs_stats":{"ops":{"lookup":{"count":0,"total_ns":0},...,"other":{...}},"sql":{"read":{...},"write":{...},"metadata":{...},"sync":{...}},"flush_transactions":{"count":0,"total_ns":0},"chunks":{"flushed":0,"bytes_flushed":0,"reads_before_write":0},"write_buffer":{"slots_high_water":0,"evictions":0,"evicted_chunks":0},"wal":{"high_water_bytes":0},"checkpoints":{"count":0,"total_ns":0,"errors":0},"batch_commits":{"count":0,"total_ns":0,"ops":0},"batch_failures":0}}
```

- `ops`: calls and total time for each FUSE operation. `other` covers symlink, readlink,
  listxattr, setxattr, removexattr and fallocate. Operations dbfs does not implement
  (access, locking, ioctl, opendir) are not counted.
- `sql`: time in each group of `Db` calls: `read` (file data), `write` (chunk writes,
  flushes, truncate, inode deletion), `metadata` (lookups and directory and inode
  changes) and `sync` (the WAL `fdatasync`). Only the outermost call on a thread is
  counted. These totals sit inside the FUSE operations, so they overlap with `ops`.
- `flush_transactions`: transactions that wrote dirty chunks (on close, fsync, setattr,
  a full write buffer, or shutdown).
- `chunks`: chunk rows and bytes written by flushes, and 4 KiB blocks read from SQLite before
  a partial write could be buffered (only the partly covered blocks at a write's edges).
- `write_buffer.slots_high_water`: most chunk buffers in use at once.
  `evictions` counts the times a full buffer forced a flush, and `evicted_chunks` the
  chunks those flushes wrote.
- `wal.high_water_bytes`: largest `-wal` file size, sampled after each flush and at exit.
- `checkpoints`: every `wal_checkpoint` call (background PASSIVE passes, writer RESTARTs at
  the WAL cap, and the TRUNCATE at unmount), with time and failures.
- `batch_commits`: commits of the metadata batch, with the time they took (`count`, `total_ns`), and
  the operations they committed (`ops`). `batch_failures` counts batches that failed and put the
  mount into fail-stop.

While stats are off, each hook is one relaxed atomic load: no clock is read and nothing
is counted.

## Optimization Points

| Path | What keeps it cheap |
|---|---|
| `read`, `write`, `flush`, `release` | `fh` indexes the handle table; per-inode state is a struct of arrays indexed by slot. No hashing, no SQL for the inode kind or size. |
| Buffered writes | One `ChunkArena` for all inodes, grown lazily to `--write-buffer` (default 256 MiB), never shrunk, and reused, so steady-state writes do not allocate. A full arena writes up to 16 MiB of the inode with the most dirty chunks (see [Eviction slices](#eviction-slices)). |
| Dirty chunk lookup | `DirtyIdx` is sorted by chunk index; sequential writes hit the last entry before any binary search. |
| Flush | `replace_chunks` upserts borrowed arena bytes by integer key in one transaction. A chunk with every block dirty needs no SELECT; a partly dirty one reads its stored row and patches only its dirty blocks. |
| Read | A read inside one chunk reads only its bytes, through the chunk's blob. Other reads use one range statement over the chunk keys; each blob is borrowed and copied straight into the reply buffer. |
| Chunk table | `file_chunks` is a rowid table keyed `(ino << 24) \| chunk_index`: one B-tree, no separate index, and a file is one contiguous key range. |
| Unlink | The resolved inode is passed to `unlink_entry` (no second lookup); an unlinked closed file's dirty data is dropped, not flushed. |
| Readdir | Keyset scan `id > cookie` on `dirents_scan`; names are borrowed from SQLite into the reply. |
| SQLite | WAL mode, `wal_autocheckpoint = 0` with checkpoints on a background thread bounded by `--wal-cap` (see [WAL Checkpoints](#wal-checkpoints)), `mmap_size` 2 GiB by default (`--mmap-size`; with the map an I/O error can raise SIGBUS, see README), every statement through `prepare_cached`. |
