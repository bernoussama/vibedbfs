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
 │  ┌──────────────────────────────────┐                           │
 │  │  dirty_files: HashMap<ino, DirtyFile>                        │
 │  │  • Buffers writes in memory                                  │
 │  │  • Coalesces on flush                                        │
 │  └──────────────────────────────────┘                           │
 │  • Maps FUSE ops → Db methods                                   │
 │  • Manages dirty write buffer                                   │
 │  • Caches inode kind from Db.inode_kinds                        │
 │  • Converts DbError → errno                                     │
 └────────────────────────┬─────────────────────────────────────────┘
                          │ Db method calls
                          ▼
 ┌──────────────────────────────────────────────────────────────────┐
 │  Db  (src/db.rs)                       Storage layer            │
 │  ┌──────────────────────────────────┐                           │
 │  │  conn: Connection (rusqlite)     │                           │
 │  │  • prepare_cached for all SQL    │                           │
 │  │  open_counts: HashMap<ino, u64>  │                           │
 │  │  pending_delete: Vec<ino>        │                           │
 │  │  inode_kinds: HashMap<ino, Kind> │                           │
 │  └──────────────────┬───────────────┘                           │
 └─────────────────────┼───────────────────────────────────────────┘
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
 ├── cli.rs           Cli, Command::Mount { database, mountpoint }
 ├── run.rs           run() → Db::open() → Dbfs::new() → fuser::mount2()
 ├── fuse.rs          Dbfs: impl Filesystem, dirty write buffer, FUSE↔Db glue
 ├── db.rs            Db: SQLite storage layer, all SQL queries, chunk I/O
 └── model.rs         FileKind, Inode, DirEntry, MetadataUpdate, DbError
```

## SQLite Schema

```
 ┌─────────────────────────────────────────────┐
 │  inodes                                     │
 │─────────────────────────────────────────────│
 │  ino      INTEGER PRIMARY KEY               │
 │  kind     INTEGER  (1=file, 2=dir, 3=sym)   │
 │  mode     INTEGER  (permission bits)        │
 │  uid      INTEGER                           │
 │  gid      INTEGER                           │
 │  size     INTEGER                           │
 │  atime    INTEGER  (seconds since epoch)    │
 │  mtime    INTEGER                           │
 │  ctime    INTEGER                           │
 │  nlink    INTEGER                           │
 └──────────────────────┬──────────────────────┘
                        │ ino
          ┌─────────────┼─────────────┬────────────────────┐
          │ FK          │ FK          │ FK                  │ FK
          ▼             ▼             ▼                    ▼
 ┌─────────────┐ ┌────────────┐ ┌─────────────────┐ ┌───────────────┐
 │  dirents    │ │  dirents   │ │  file_chunks    │ │symlink_targets│
 │─────────────│ │  (child)   │ │─────────────────│ │───────────────│
 │  parent_ino │ │            │ │  ino            │ │  ino   PK, FK │
 │  name  BLOB │ │            │ │  chunk_index    │ │  target  BLOB │
 │  child_ino  │ │            │ │  data  BLOB     │ │               │
 │─────────────│ │            │ │  (64 KiB each)  │ │               │
 │  PK: (parent│ │            │ │  PK: (ino,      │ │               │
 │       ,name)│ │            │ │    chunk_index)  │ │               │
 └─────────────┘ └────────────┘ └─────────────────┘ └───────────────┘

 Directory tree example:

   inodes:  ino=1 (root dir)    ino=2 (hello.txt)    ino=3 (docs/ dir)
   dirents: (1, "hello.txt", 2)  (1, "docs", 3)
   chunks:  (2, 0, <64K blob>)   (2, 1, <64K blob>)  ...
```

## Write Data Flow

```
 User: write(fd, "hello", 5) at offset 0
                │
                ▼
 ┌──────────────────────────────┐
 │  FUSE kernel module          │
 │  FUSE_WRITE(ino=2, off=0,   │
 │             size=5, "hello") │
 └──────────────┬───────────────┘
                ▼
 ┌──────────────────────────────┐
 │  Dbfs::write(ino=2, 0,      │
 │              "hello")        │
 │                              │
 │  1. Check inode_kinds cache  │◄── cached on open(), no SQL
 │     kind = RegularFile? ✓    │
 │                              │
 │  2. Flush first if the file  │
 │     buffers 64 MiB or more   │
 │                              │
 │  3. Get or create DirtyFile  │
 │     dirty_files[2] = {       │
 │       size: 5,               │
 │       chunks: {0: "hello"}   │
 │     }                        │
 │     Partial write to a new   │
 │     chunk: SELECT it once    │
 │                              │
 │  4. Return Ok(5)             │    ── no SQLite write below 64 MiB ──
 └──────────────────────────────┘
```

## Flush Data Flow (on close, fsync, or 64 MiB buffered)

```
 User: close(fd)  or  fsync(fd)  or  a write past 64 MiB buffered
                │
                ▼
 ┌──────────────────────────────────────────────────────────┐
 │  Dbfs::flush_file(ino=2)                                │
 │                                                         │
 │  1. Remove dirty_files[2]                               │
 │     dirty = { size: 300000,                             │
 │               chunks: {0: <64K>, 4: <4K>, ...} }        │
 │                                                         │
 │  2. Call Db::replace_chunks(ino=2, size, &chunks)       │
 └────────────────────┬────────────────────────────────────┘
                      ▼
 ┌──────────────────────────────────────────────────────────┐
 │  Db::replace_chunks                                     │
 │                                                         │
 │  1. BEGIN TRANSACTION                                   │
 │                                                         │
 │  2. For each buffered chunk:                            │
 │     INSERT OR REPLACE the whole row (no SELECT)         │
 │                                                         │
 │  3. UPDATE inodes SET size, mtime, ctime                │
 │                                                         │
 │  4. COMMIT                                              │
 │     └──► SQLite appends WAL frames                      │
 └─────────────────────────────────────────────────────────┘
```

## Read Data Flow

```
 User: read(fd, buf, 4096) at offset 0
                │
                ▼
 ┌──────────────────────────────────────────────────┐
 │  Dbfs::read(ino=2, offset=0, size=4096)         │
 │                                                  │
 │  Has dirty_files[2]?                             │
 │  ├── NO:  Db::read_file(2, 0, 4096) ──────┐    │
 │  │        return persisted data             │    │
 │  │                                          │    │
 │  └── YES: (dirty overlay read)              │    │
 │       1. Db::read_file(2, 0, 4096)         │    │
 │          fill output from persisted chunks  │    │
 │       2. For each buffered chunk in range:  │    │
 │          overlay its bytes onto output      │    │
 │       3. Return merged output               │    │
 └──────────────────────┬──────────────────────┘    │
                        │                           │
                        ▼                           ▼
 ┌──────────────────────────────────────────────────────┐
 │  Db::read_file(ino=2, offset=0, size=4096)          │
 │                                                      │
 │  1. get_inode(2)  (prepare_cached)                   │
 │  2. Compute chunk range:                             │
 │     chunk 0 covers bytes [0..65535]                  │
 │  3. SELECT data FROM file_chunks                     │
 │     WHERE ino=2 AND chunk_index=0  (prepare_cached)  │
 │  4. Copy requested range into output                 │
 └──────────────────────────────────────────────────────┘
```

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
 │                                                    │
 │  2. flush_file(2)    ◄── flush dirty data first   │
 │                                                    │
 │  3. Db::unlink_inode(1, "hello.txt", &inode)      │
 │     (reuses resolved inode, no double lookup)      │
 └──────────────────────┬─────────────────────────────┘
                        ▼
 ┌────────────────────────────────────────────────────┐
 │  Db::unlink_inode                                  │
 │                                                    │
 │  BEGIN TRANSACTION                                 │
 │  ├── DELETE FROM dirents                           │
 │  │   WHERE parent_ino=1 AND name='hello.txt'      │
 │  ├── nlink > 1?                                    │
 │  │    YES → UPDATE inodes SET nlink = nlink - 1    │
 │  │    NO  → open_count > 0?                        │
 │  │           YES → defer to pending_delete         │
 │  │           NO  → DELETE FROM inodes WHERE ino=2  │
 │  │                 (CASCADE deletes file_chunks)    │
 │  └── UPDATE parent mtime/ctime                     │
 │  COMMIT                                            │
 └────────────────────────────────────────────────────┘
```

## Readdir Data Flow

```
 User: ls /mnt/
                │
                ▼
 Kernel calls readdir multiple times with increasing offset:

   readdir(ino=1, offset=0)
     │
     ▼
   ┌─────────────────────────────────────────────┐
   │  Dbfs::readdir(ino=1, offset=0)             │
   │                                             │
   │  offset < 1 → emit "."  (ino=1)            │
   │  offset < 2 → emit ".." (ino=1)            │
   │                                             │
   │  db_offset = max(0 - 2, 0) = 0             │
   │  Db::list_dir_offset(1, 0)                 │
   │    SELECT d.name, i.ino, i.kind            │
   │    FROM dirents d JOIN inodes i ...         │
   │    WHERE parent_ino=1                       │
   │    ORDER BY name                            │
   │    LIMIT -1 OFFSET 0   ◄── pushed to SQL   │
   │                                             │
   │  Return: [".", "..", "docs", "hello.txt"]   │
   └─────────────────────────────────────────────┘

   readdir(ino=1, offset=4)
     │
     ▼
   ┌─────────────────────────────────────────────┐
   │  Dbfs::readdir(ino=1, offset=4)             │
   │                                             │
   │  offset >= 2 → skip "." and ".."           │
   │  db_offset = 4 - 2 = 2                     │
   │  Db::list_dir_offset(1, 2)                 │
   │    ... OFFSET 2  ◄── skip in SQL, not Rust  │
   │                                             │
   │  Return: []  (no more entries)              │
   └─────────────────────────────────────────────┘
```

## Optimization Points

```
 ┌─────────── Userspace ──────────────────────────────────────────┐
 │                                                                │
 │  FUSE write                                                    │
 │  ┌──────────────────────────────────────────────────────────┐  │
 │  │                        ╔══════════════╗                  │  │
 │  │  write() ─────────────►║ inode_kinds  ║──► skip DB hit   │  │
 │  │                        ║   cache      ║   on every write │  │
 │  │                        ╚══════════════╝                  │  │
 │  │          │                                               │  │
 │  │          ▼                                               │  │
 │  │  ╔═══════════════╗                                       │  │
 │  │  ║  dirty_files  ║  64K chunk buffers in RAM             │  │
 │  │  ║  (in-memory)  ║  full-chunk write: no SELECT          │  │
 │  │  ║               ║  partial write: SELECT chunk once     │  │
 │  │  ╚═══════╤═══════╝                                       │  │
 │  │          │ flush past 64 MiB, on fsync, on close         │  │
 │  │          ▼                                               │  │
 │  │  ╔═══════════════╗                                       │  │
 │  │  ║  replace_     ║  INSERT OR REPLACE whole chunks       │  │
 │  │  ║  chunks()     ║  no SELECT, one transaction           │  │
 │  │  ╚═══════╤═══════╝                                       │  │
 │  │          │                                               │  │
 │  └──────────┼───────────────────────────────────────────────┘  │
 │             ▼                                                  │
 │  ┌─────────────────────────────────────────────────────────┐   │
 │  │  SQLite  (all queries via prepare_cached)               │   │
 │  │  ┌───────────────────────────────────────────────────┐  │   │
 │  │  │  WAL mode                                         │  │   │
 │  │  │  • wal_autocheckpoint = 10000 (reduced stalls)    │  │   │
 │  │  │  • Writes append to WAL (fast, sequential)        │  │   │
 │  │  │  • Checkpoint consolidates WAL → main DB          │  │   │
 │  │  └───────────────────────────────────────────────────┘  │   │
 │  └─────────────────────────────────────────────────────────┘   │
 │                                                                │
 │  FUSE unlink                                                   │
 │  ┌──────────────────────────────────────────────────────────┐  │
 │  │  lookup(parent, name) ───► resolved inode                │  │
 │  │          │                      │                        │  │
 │  │          │              ╔═══════╧═══════╗                │  │
 │  │          │              ║  pass inode   ║ skip           │  │
 │  │          │              ║  directly to  ║ double         │  │
 │  │          │              ║  unlink_inode ║ lookup         │  │
 │  │          │              ╚═══════════════╝                │  │
 │  └──────────┼───────────────────────────────────────────────┘  │
 │             │                                                  │
 │  FUSE readdir                                                  │
 │  ┌──────────────────────────────────────────────────────────┐  │
 │  │  ╔═══════════════╗                                       │  │
 │  │  ║ SQL OFFSET    ║  skip entries in SQLite, not Rust     │  │
 │  │  ║  push-down    ║  LIMIT -1 OFFSET ?                   │  │
 │  │  ╚═══════════════╝                                       │  │
 │  └──────────────────────────────────────────────────────────┘  │
 │                                                                │
 └────────────────────────────────────────────────────────────────┘
```
