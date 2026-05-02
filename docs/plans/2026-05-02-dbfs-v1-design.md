# DBFS V1 Design

Date: 2026-05-02

## Goal

Build `dbfs`, a mountable filesystem implemented in Rust with FUSE and backed by a single SQLite database file.

V1 should prove the core idea: SQLite is the source of truth for filesystem metadata, directory structure, and file contents. The filesystem should behave like a basic POSIX filesystem for regular files and directories, without trying to support every POSIX feature immediately.

The first command should look like this:

```bash
dbfs mount ./dbfs.sqlite ./mnt
```

## Recommendation

Use an inode-based design with fixed-size file chunks stored in SQLite.

This is the best v1 tradeoff because FUSE APIs are inode-oriented, not path-oriented. Inode identity also makes rename, open handles, directory traversal, and future hard-link support much easier to reason about.

The core tables are:

```text
inodes       filesystem objects and metadata
dirents      names inside directories
file_chunks  regular file content blocks
```

Paths are not stored as identity. A path is resolved by walking `dirents` from the root inode.

## Non-Goals For V1

V1 should not include:

- Symlinks
- Hard links
- Extended attributes
- File locking
- Snapshots
- Deduplication
- Compression
- Encryption
- Sparse-file extent optimization
- Async runtime or connection pooling
- External data files outside SQLite

These can be added later if the basic filesystem model proves correct.

## Crate Structure

Recommended initial layout:

```text
src/
  main.rs          CLI entrypoint
  fs.rs            fuser::Filesystem implementation
  db.rs            SQLite connection, schema, and transactions
  model.rs         filesystem types and FileAttr conversion
  error.rs         database/FUSE errno mapping
```

Keep the FUSE layer thin. It should translate kernel callbacks into database operations and return the right FUSE replies. Most behavior should live in the database layer so it can be tested without mounting FUSE.

## Dependencies

Likely dependencies:

- `fuser` for the FUSE userspace filesystem implementation
- `rusqlite` for SQLite access
- `clap` for the CLI
- `libc` for errno constants used by FUSE replies
- `tempfile` for tests

Do not add dependencies until implementation starts and the exact need is clear.

## SQLite Schema

Initial schema concept:

```sql
CREATE TABLE inodes (
  ino INTEGER PRIMARY KEY,
  kind INTEGER NOT NULL,
  mode INTEGER NOT NULL,
  uid INTEGER NOT NULL,
  gid INTEGER NOT NULL,
  size INTEGER NOT NULL,
  atime INTEGER NOT NULL,
  mtime INTEGER NOT NULL,
  ctime INTEGER NOT NULL,
  nlink INTEGER NOT NULL
);

CREATE TABLE dirents (
  parent_ino INTEGER NOT NULL,
  name BLOB NOT NULL,
  child_ino INTEGER NOT NULL,
  PRIMARY KEY (parent_ino, name),
  FOREIGN KEY (parent_ino) REFERENCES inodes(ino) ON DELETE CASCADE,
  FOREIGN KEY (child_ino) REFERENCES inodes(ino) ON DELETE CASCADE
);

CREATE TABLE file_chunks (
  ino INTEGER NOT NULL,
  chunk_index INTEGER NOT NULL,
  data BLOB NOT NULL,
  PRIMARY KEY (ino, chunk_index),
  FOREIGN KEY (ino) REFERENCES inodes(ino) ON DELETE CASCADE
);
```

Root directory is always inode `1`.

Names should be stored as bytes, not UTF-8 strings. Unix filenames are byte sequences, and Rust/FUSE paths can contain non-UTF-8 names.

## SQLite Pragmas

On open, configure SQLite with:

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
```

Rationale:

- Foreign keys protect inode, dirent, and chunk relationships.
- WAL improves normal read/write behavior.
- `synchronous = NORMAL` is a pragmatic default for a local development filesystem.
- `busy_timeout` avoids immediately failing during short lock contention.

## Chunk Storage

Store regular file content in fixed-size chunks. Use `64 KiB` chunks initially.

Chunk addressing:

```text
chunk_index = offset / CHUNK_SIZE
chunk_offset = offset % CHUNK_SIZE
```

Reads assemble data from one or more chunks. Missing chunks should read as zeroes only if sparse-file behavior is intentionally supported. For v1, avoid promising sparse files; writes should create the chunks needed for the current file size.

Writes happen in a transaction and may update multiple rows in `file_chunks`. After writing, update inode `size`, `mtime`, and `ctime` as needed.

Truncation rules:

- Shrinking a file deletes chunks past the new end.
- Shrinking inside a chunk trims that chunk.
- Growing a file updates size; zero-fill behavior must be respected on later reads.

## FUSE Operations

Implement these for v1:

```text
init
lookup
getattr
readdir
mkdir
create
open
read
write
setattr
unlink
rmdir
rename
statfs
destroy
```

Defer these:

```text
symlink
readlink
link
xattr operations
locking operations
advanced fsync behavior
```

## Operation Semantics

### lookup

Given `parent_ino` and `name`, find the matching `dirents` row and return the child inode attributes.

Return `ENOENT` if the entry does not exist.

### getattr

Fetch the inode row and convert it into a FUSE file attribute.

Return `ENOENT` if the inode does not exist.

### readdir

Return `.` and `..`, then entries from `dirents` for the requested directory inode.

Use stable offsets so repeated `readdir` calls can continue correctly.

### mkdir

In one transaction:

- Validate the parent exists and is a directory.
- Validate the name does not already exist in the parent.
- Insert a new directory inode.
- Insert the new dirent.
- Update parent `mtime` and `ctime`.

### create

In one transaction:

- Validate the parent exists and is a directory.
- Validate the name does not already exist in the parent.
- Insert a new regular-file inode.
- Insert the new dirent.
- Update parent `mtime` and `ctime`.

### read

Read the requested byte range from `file_chunks`, clamped to inode `size`.

Return empty data when the offset is at or past EOF.

### write

In one transaction:

- Validate the inode exists and is a regular file.
- Load affected chunks.
- Patch chunk bytes.
- Upsert affected chunks.
- Update file size if the write extends EOF.
- Update `mtime` and `ctime`.

### setattr

Support the v1 subset:

- mode changes
- uid/gid changes
- size changes/truncation
- atime/mtime changes

Unsupported fields should be ignored or rejected according to FUSE expectations.

### unlink

In one transaction:

- Validate the entry exists and points to a regular file.
- Delete the dirent.
- Delete the inode and chunks if no other references exist.
- Update parent `mtime` and `ctime`.

Since v1 does not support hard links, deleting the inode immediately is acceptable after removing its only dirent.

### rmdir

In one transaction:

- Validate the entry exists and points to a directory.
- Validate the target directory is empty.
- Delete the dirent.
- Delete the target inode.
- Update parent `mtime` and `ctime`.

Return `ENOTEMPTY` for non-empty directories.

### rename

In one transaction:

- Validate the source exists.
- Validate the destination parent exists and is a directory.
- Apply replacement rules carefully.
- Prevent moving a directory into itself or below itself.
- Update the source dirent to the new parent/name.
- Update affected parent timestamps.

Keep this implementation conservative. It is better to return a correct error for unsupported edge cases than to corrupt the tree.

## Error Mapping

The database layer should return domain errors. The FUSE layer should map those to errno values.

Examples:

```text
not found                ENOENT
already exists           EEXIST
not a directory          ENOTDIR
is a directory           EISDIR
directory not empty      ENOTEMPTY
invalid input            EINVAL
permission unsupported   EACCES or EPERM
database failure         EIO
```

Avoid leaking raw SQLite errors directly into FUSE behavior except as `EIO`.

## Correctness Invariants

The code should preserve these invariants:

- Inode `1` always exists and is a directory.
- Every `dirents.parent_ino` points to a directory inode.
- Every `dirents.child_ino` points to an existing inode.
- No two entries in the same directory have the same name.
- Regular file chunks only belong to regular-file inodes.
- Directory removal is only allowed when the directory has no children.
- File size matches the logical readable size, not necessarily the sum of chunk lengths.
- Mutating operations are transactional.

Some invariants require application checks because SQLite foreign keys alone cannot express them.

## Testing Strategy

Every behavior should be testable at the database layer before relying on FUSE integration tests.

### Unit Tests

Test the database layer using temporary SQLite files or in-memory databases:

- Initializes root inode exactly once.
- Creates files and directories.
- Rejects duplicate names in the same directory.
- Allows the same name in different directories.
- Lists directory entries.
- Reads and writes within one chunk.
- Reads and writes across chunk boundaries.
- Extends file size on write.
- Truncates files smaller and larger.
- Deletes file chunks on unlink.
- Rejects `rmdir` on non-empty directories.
- Renames within a directory.
- Renames across directories.
- Rejects invalid directory moves.

### Integration Tests

Add mount-based tests once the DB layer is stable:

- Mount a temporary DB.
- Use normal filesystem calls to create, read, write, rename, and delete files.
- Unmount cleanly.
- Reopen the DB and verify persistence.

These tests may need to be skipped automatically when FUSE is unavailable in CI or the local environment.

## Milestones

Status markers:

- `[x]` Done
- `[~]` Partially done
- `[ ]` Not started

### Milestone 1: Storage Foundation

- [x] Add DB schema initialization.
- [x] Create root inode.
- [x] Add typed DB operations for inode lookup, dirent lookup, listing, create, delete, and rename.
- [x] Add chunk read/write/truncate helpers.
- [x] Cover DB behavior with unit tests.
- [x] Add file-backed `Db::open(path)` for real database files.
- [x] Apply on-disk SQLite pragmas: WAL, synchronous mode, busy timeout, and foreign keys.
- [x] Split storage code into focused modules once behavior stabilizes.

### Milestone 2: Read-Only Mount

- [x] Add CLI parsing for `dbfs mount <db> <mountpoint>`.
- [ ] Implement FUSE `init`, `lookup`, `getattr`, `readdir`, and `statfs`.
- [x] Map DB inode metadata to FUSE file attributes.
- [ ] Verify mounting an empty root directory works.

### Milestone 3: Directory And File Creation

- [x] Implement storage-layer directory creation.
- [x] Implement storage-layer file creation.
- [ ] Wire FUSE `mkdir` to storage.
- [ ] Wire FUSE `create` to storage.
- [ ] Verify created files/directories appear through normal shell commands.

### Milestone 4: File I/O

- [x] Implement storage-layer `read_file`.
- [x] Implement storage-layer `write_file` and size updates.
- [x] Add cross-chunk read/write tests.
- [x] Implement storage-layer truncation.
- [ ] Wire FUSE `open`, `read`, `write`, and `setattr(size)` to storage.
- [ ] Verify persistence after unmount/remount.

### Milestone 5: Mutation Semantics

- [x] Implement storage-layer `unlink`.
- [x] Implement storage-layer `rmdir`.
- [x] Implement storage-layer `rename`.
- [~] Add tests for edge cases and errno behavior.
- [x] Implement storage-layer metadata updates for mode, uid, gid, atime, and mtime.
- [ ] Wire FUSE `unlink`, `rmdir`, `rename`, and `setattr` to storage.

### Milestone 6: Hardening

- [ ] Review crash consistency boundaries.
- [~] Improve errno mapping.
- [ ] Add mount integration test gating.
- [x] Run formatting, clippy, and tests for completed storage slices.

## Performance Position

V1 should prioritize correctness and simplicity over performance, while making cheap performance-conscious choices that do not complicate the design.

Do now:

- Use WAL mode.
- Use fixed-size chunks.
- Use transactions for mutations.
- Use indexed primary keys for lookup paths.
- Avoid absolute path storage.
- Keep inode IDs stable.

Do later only after benchmarks:

- Compression
- Deduplication
- Extents
- Async execution
- Connection pooling
- Complex cache layers
- Snapshotting

The v1 target is reliability for thousands of files and small-to-medium regular files, not maximum throughput for huge files or database-inside-database workloads.

## Open Questions

- Should file permissions be enforced by `dbfs`, delegated to the kernel/FUSE mount options, or mostly stored as metadata in v1?
- Should v1 use `synchronous = NORMAL` or a stricter durability mode by default?
- Should chunk size be fixed forever per database, or stored in a metadata table for future compatibility?
- Should mount options include read-only mode in v1?

## Next Step

Start the read-only FUSE mount by adding CLI parsing and mapping database inode metadata to FUSE attributes.
