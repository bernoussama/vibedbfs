use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use rusqlite::{Connection, Error as SqlError, OptionalExtension, params};

use crate::blocks::{self, ALL_BLOCKS, BlockMask, block_range};
use crate::{DbError, DirEntry, FileKind, Inode, MetadataUpdate, Timestamp};

pub(crate) const CHUNK_SIZE: usize = 65_536;

/// `PRAGMA mmap_size` when none is given: a 2 GiB map of the database file on
/// 64-bit targets. 32-bit targets get none, since a large map competes for their
/// small address space.
#[cfg(target_pointer_width = "64")]
const DEFAULT_MMAP_SIZE: u64 = 2 * 1024 * 1024 * 1024;
#[cfg(not(target_pointer_width = "64"))]
const DEFAULT_MMAP_SIZE: u64 = 0;

/// Chunk rows are keyed by `(ino << CHUNK_BITS) | chunk_index`.
const CHUNK_BITS: u32 = 24;
const MAX_CHUNKS: u64 = 1 << CHUNK_BITS;
/// 2^24 chunks of 64 KiB: 1 TiB. Larger files get `EFBIG`.
pub(crate) const MAX_FILE_SIZE: u64 = MAX_CHUNKS * CHUNK_SIZE as u64;
/// Largest inode number whose chunk keys fit in a positive i64.
const MAX_INO: u64 = (1 << (63 - CHUNK_BITS)) - 1;

/// Row id of chunk `chunk_index` of `ino`.
fn chunk_key(ino: u64, chunk_index: u64) -> i64 {
    debug_assert!(ino <= MAX_INO && chunk_index < MAX_CHUNKS);
    ((ino << CHUNK_BITS) | chunk_index) as i64
}

/// Inclusive row id range of every chunk of `ino`.
fn chunk_keys(ino: u64) -> (i64, i64) {
    (chunk_key(ino, 0), chunk_key(ino, MAX_CHUNKS - 1))
}

fn chunk_index_of(key: i64) -> u64 {
    key as u64 & (MAX_CHUNKS - 1)
}

/// On-disk schema version, stored in `PRAGMA user_version`.
///
/// - 0: databases created before versioning existed (may lack `*_nsec` columns)
/// - 1: `inodes` has `atime_nsec`, `mtime_nsec`, `ctime_nsec`
/// - 2: index `dirents_child` on `dirents(child_ino)`
/// - 3: `dirents.id` is a stable per-entry id, used as the readdir cookie
/// - 4: `file_chunks` keyed by one integer, `(ino << 24) | chunk_index`; one
///   `*_ns` column per timestamp
/// - 5: `inodes.ino` is AUTOINCREMENT, so an inode number is never reused, even
///   after the highest one is deleted
pub const SCHEMA_VERSION: i64 = 5;

pub(crate) struct VolumeUsage {
    pub page_size: u64,
    pub page_count: u64,
    pub freelist_count: u64,
    pub files: u64,
}

/// SQL storage. Holds no per-process file state: which inodes are open lives
/// in the FUSE layer's open-inode table, and callers say so where it matters
/// (`unlink_entry`, `rename_entry`).
pub struct Db {
    conn: Mutex<Connection>,
    /// Held as long as the `Db` is alive. Declared after `conn`, so the
    /// connection closes before the lock is released.
    _lock: Option<File>,
}

#[derive(Clone, Copy)]
enum NewNode<'a> {
    Directory { mode: u32 },
    File { mode: u32 },
    Symlink { target: &'a [u8] },
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let path = path.as_ref();
        // Before the database file is opened, so a refused process never touches it.
        let lock = lock_database(path)?;
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;
            ",
        )?;
        // Before the WAL switch and the table creation in `initialize`, both of
        // which write to the file.
        refuse_newer_schema(&conn)?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA wal_autocheckpoint = 10000;
            ",
        )?;
        apply_mmap_size(&conn, DEFAULT_MMAP_SIZE)?;
        Self::initialize(lock, conn)
    }

    /// Sets `PRAGMA mmap_size`, the most of the database file that SQLite maps
    /// into memory, and returns the size in force. SQLite caps the request at its
    /// compiled-in maximum, so the result can be smaller. `0` turns the map off.
    pub fn set_mmap_size(&self, bytes: u64) -> Result<u64, DbError> {
        apply_mmap_size(&self.conn.lock().unwrap(), bytes)
    }

    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Self::initialize(None, conn)
    }

    pub(crate) fn sync_durable(&self) -> Result<(), DbError> {
        let path = {
            let conn = self.conn.lock().unwrap();
            match conn.path() {
                Some(path) if !path.is_empty() && path != ":memory:" => path.to_owned(),
                _ => return Ok(()),
            }
        };
        sync_database_file(Path::new(&path)).map_err(|error| DbError::Sqlite(error.to_string()))
    }

    pub fn pragma_i64(&self, name: &str) -> Result<i64, DbError> {
        let sql = pragma_query(name)?;
        self.conn
            .lock()
            .unwrap()
            .query_row(&sql, [], |row| row.get(0))
            .map_err(Into::into)
    }

    pub(crate) fn volume_usage(&self) -> Result<VolumeUsage, DbError> {
        let conn = self.conn.lock().unwrap();
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let freelist_count: i64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        let files: i64 = conn.query_row("SELECT COUNT(*) FROM inodes", [], |row| row.get(0))?;
        Ok(VolumeUsage {
            page_size: page_size as u64,
            page_count: page_count as u64,
            freelist_count: freelist_count as u64,
            files: files as u64,
        })
    }

    pub fn pragma_string(&self, name: &str) -> Result<String, DbError> {
        let sql = pragma_query(name)?;
        self.conn
            .lock()
            .unwrap()
            .query_row(&sql, [], |row| row.get(0))
            .map_err(Into::into)
    }

    /// Sets `PRAGMA max_page_count`. A write that needs a page beyond the limit
    /// fails with `SQLITE_FULL`, which is `DbError::NoSpace` (`ENOSPC`). The
    /// pragma is per connection, so `Dbfs::set_max_page_count` is the public hook.
    pub(crate) fn set_max_page_count(&self, pages: u64) -> Result<(), DbError> {
        let pages = i64::try_from(pages).map_err(|_| DbError::InvalidInput)?;
        self.conn
            .lock()
            .unwrap()
            .pragma_update(None, "max_page_count", pages)?;
        Ok(())
    }

    fn initialize(lock: Option<File>, conn: Connection) -> Result<Self, DbError> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS inodes (
              ino INTEGER PRIMARY KEY AUTOINCREMENT,
              kind INTEGER NOT NULL,
              mode INTEGER NOT NULL,
              uid INTEGER NOT NULL,
              gid INTEGER NOT NULL,
              size INTEGER NOT NULL,
              atime_ns INTEGER NOT NULL,
              mtime_ns INTEGER NOT NULL,
              ctime_ns INTEGER NOT NULL,
              nlink INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS dirents (
              id INTEGER PRIMARY KEY,
              parent_ino INTEGER NOT NULL,
              name BLOB NOT NULL,
              child_ino INTEGER NOT NULL,
              FOREIGN KEY (parent_ino) REFERENCES inodes(ino) ON DELETE CASCADE,
              FOREIGN KEY (child_ino) REFERENCES inodes(ino) ON DELETE CASCADE
            );

            -- id = (ino << 24) | chunk_index. No foreign key: deleting an inode
            -- deletes its chunk range explicitly (delete_inode_rows).
            CREATE TABLE IF NOT EXISTS file_chunks (
              id INTEGER PRIMARY KEY,
              data BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS symlink_targets (
              ino INTEGER PRIMARY KEY,
              target BLOB NOT NULL,
              FOREIGN KEY (ino) REFERENCES inodes(ino) ON DELETE CASCADE
            );
            ",
        )?;
        migrate(&conn)?;

        let now = now_ts();
        let mount_uid = unsafe { libc::getuid() } as i64;
        let mount_gid = unsafe { libc::getgid() } as i64;
        conn.execute(
            "INSERT OR IGNORE INTO inodes
             (ino, kind, mode, uid, gid, size, atime_ns, mtime_ns, ctime_ns, nlink)
             VALUES (1, ?1, ?2, ?3, ?4, 0, ?5, ?5, ?5, 2)",
            params![
                FileKind::Directory.as_i64(),
                0o755_i64,
                mount_uid,
                mount_gid,
                now.to_ns(),
            ],
        )?;
        conn.execute(
            "UPDATE inodes
             SET uid = ?1, gid = ?2, ctime_ns = ?3
             WHERE ino = 1 AND uid = 0 AND gid = 0",
            params![mount_uid, mount_gid, now.to_ns()],
        )?;
        recompute_directory_nlink(&conn)?;
        // Inodes unlinked while open, left by a crash.
        let orphans: Vec<i64> = conn
            .prepare("SELECT ino FROM inodes WHERE nlink = 0 AND ino != 1")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for ino in orphans {
            delete_inode_rows(&conn, ino as u64)?;
        }

        Ok(Self {
            conn: Mutex::new(conn),
            _lock: lock,
        })
    }

    pub fn get_inode(&self, ino: u64) -> Result<Inode, DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT ino, kind, mode, uid, gid, size, atime_ns, mtime_ns, ctime_ns, nlink
             FROM inodes
             WHERE ino = ?1",
        )?;
        stmt.query_row(params![ino as i64], inode_from_row)
            .map_err(Into::into)
    }

    /// Kind and size of `ino`: the two columns the data paths need, without
    /// decoding the rest of the row.
    pub fn inode_head(&self, ino: u64) -> Result<(FileKind, u64), DbError> {
        let conn = self.conn.lock().unwrap();
        let (kind, size) = conn
            .prepare_cached("SELECT kind, size FROM inodes WHERE ino = ?1")?
            .query_row(params![ino as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?;
        Ok((FileKind::from_i64(kind)?, size as u64))
    }

    /// Kind of `ino` only, for "is this a directory?" checks.
    pub fn inode_kind_of(&self, ino: u64) -> Result<FileKind, DbError> {
        let conn = self.conn.lock().unwrap();
        let kind = conn
            .prepare_cached("SELECT kind FROM inodes WHERE ino = ?1")?
            .query_row(params![ino as i64], |row| row.get::<_, i64>(0))?;
        FileKind::from_i64(kind)
    }

    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<Inode, DbError> {
        check_name(name)?;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT i.ino, i.kind, i.mode, i.uid, i.gid, i.size,
                    i.atime_ns, i.mtime_ns, i.ctime_ns, i.nlink
             FROM dirents d
             JOIN inodes i ON i.ino = d.child_ino
             WHERE d.parent_ino = ?1 AND d.name = ?2",
        )?;
        stmt.query_row(params![parent_ino as i64, name], inode_from_row)
            .map_err(Into::into)
    }

    pub fn list_dir(&self, ino: u64) -> Result<Vec<DirEntry>, DbError> {
        if self.inode_kind_of(ino)? != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT d.name, i.ino, i.kind
             FROM dirents d
             JOIN inodes i ON i.ino = d.child_ino
             WHERE d.parent_ino = ?1
             ORDER BY d.name",
        )?;

        let entries = stmt
            .query_map(params![ino as i64], |row| {
                let kind = FileKind::from_i64(row.get(2)?).map_err(|err| {
                    SqlError::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Integer,
                        Box::new(std::io::Error::other(format!("{err:?}"))),
                    )
                })?;

                Ok(DirEntry {
                    name: row.get(0)?,
                    ino: row.get::<_, i64>(1)? as u64,
                    kind,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    /// Streams the entries of directory `ino` whose id is greater than `after_id`,
    /// in id order, to `emit(id, name, child_ino, kind)`. Stops early when `emit`
    /// returns true.
    ///
    /// Entry ids never change for the life of an entry (rename within a directory
    /// keeps it), so a readdir cookie built from the id stays valid while other
    /// entries are added or removed. Names are borrowed from SQLite, not copied.
    pub fn read_dir_after(
        &self,
        ino: u64,
        after_id: i64,
        mut emit: impl FnMut(i64, &[u8], u64, FileKind) -> bool,
    ) -> Result<(), DbError> {
        if self.inode_kind_of(ino)? != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT d.id, d.name, d.child_ino, i.kind
             FROM dirents d
             JOIN inodes i ON i.ino = d.child_ino
             WHERE d.parent_ino = ?1 AND d.id > ?2
             ORDER BY d.id",
        )?;
        let mut rows = stmt.query(params![ino as i64, after_id])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let name = row.get_ref(1)?.as_blob().map_err(SqlError::from)?;
            let child = row.get::<_, i64>(2)? as u64;
            let kind = FileKind::from_i64(row.get(3)?)?;
            if emit(id, name, child, kind) {
                break;
            }
        }
        Ok(())
    }

    /// Returns the parent directory of directory `ino` (the root is its own parent).
    pub fn parent_of_dir(&self, ino: u64) -> Result<u64, DbError> {
        if ino == 1 {
            return Ok(1);
        }
        let conn = self.conn.lock().unwrap();
        conn.prepare_cached("SELECT parent_ino FROM dirents WHERE child_ino = ?1 LIMIT 1")?
            .query_row(params![ino as i64], |row| row.get::<_, i64>(0))
            .map(|parent| parent as u64)
            .map_err(Into::into)
    }

    pub fn create_dir(
        &self,
        parent_ino: u64,
        name: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Inode, DbError> {
        self.create_node(parent_ino, name, NewNode::Directory { mode }, uid, gid)
    }

    pub fn create_file(
        &self,
        parent_ino: u64,
        name: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Inode, DbError> {
        self.create_node(parent_ino, name, NewNode::File { mode }, uid, gid)
    }

    pub fn create_symlink(
        &self,
        parent_ino: u64,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<Inode, DbError> {
        self.create_node(parent_ino, name, NewNode::Symlink { target }, uid, gid)
    }

    pub fn read_symlink(&self, ino: u64) -> Result<Vec<u8>, DbError> {
        if self.inode_kind_of(ino)? != FileKind::Symlink {
            return Err(DbError::InvalidInput);
        }

        let conn = self.conn.lock().unwrap();
        conn.prepare_cached("SELECT target FROM symlink_targets WHERE ino = ?1")?
            .query_row(params![ino as i64], |row| row.get(0))
            .map_err(Into::into)
    }

    /// Deletes inode `ino` and, through the foreign-key cascade, its chunks,
    /// symlink target and any remaining entries. Used when the last handle of an
    /// inode unlinked while open is released.
    pub(crate) fn delete_inode(&self, ino: u64) -> Result<(), DbError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        delete_inode_rows(&tx, ino)?;
        tx.commit()?;
        Ok(())
    }

    pub fn link(&self, ino: u64, new_parent_ino: u64, new_name: &[u8]) -> Result<Inode, DbError> {
        check_name(new_name)?;
        let inode = self.get_inode(ino)?;
        if inode.kind == FileKind::Directory {
            return Err(DbError::IsDirectory);
        }
        if self.inode_kind_of(new_parent_ino)? != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }

        let now = now_ts();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        tx.prepare_cached("INSERT INTO dirents (parent_ino, name, child_ino) VALUES (?1, ?2, ?3)")?
            .execute(params![new_parent_ino as i64, new_name, ino as i64])?;

        let new_nlink = inode.nlink + 1;
        tx.prepare_cached("UPDATE inodes SET nlink = ?1, ctime_ns = ?2 WHERE ino = ?3")?
            .execute(params![new_nlink as i64, now.to_ns(), ino as i64])?;
        touch_dir(&tx, new_parent_ino, now)?;
        tx.commit()?;

        Ok(Inode {
            nlink: new_nlink,
            ctime: now,
            ..inode
        })
    }

    pub fn read_file(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, DbError> {
        let (kind, file_size) = self.inode_head(ino)?;
        if let Some(error) = kind.regular_file_error() {
            return Err(error);
        }
        if offset >= file_size || size == 0 {
            return Ok(Vec::new());
        }

        let mut output = vec![0; (size as u64).min(file_size - offset) as usize];
        self.read_into(ino, offset, &mut output)?;
        Ok(output)
    }

    /// Copies the stored bytes of `ino` at `offset..offset + out.len()` into
    /// `out`. Holes (missing chunks, or short trailing chunks) are left as they
    /// are, so the caller passes a zeroed buffer. Does not check the inode kind
    /// or size.
    ///
    /// A read that covers part of one chunk reads only those bytes, through the
    /// chunk's blob (see [`read_chunk_part`]). Other reads use one range query;
    /// blobs are borrowed from SQLite, not copied into a `Vec` first.
    pub(crate) fn read_into(&self, ino: u64, offset: u64, out: &mut [u8]) -> Result<(), DbError> {
        let Some(span) = ChunkSpan::new(offset, out.len()) else {
            return Ok(());
        };
        if span.first >= MAX_CHUNKS {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap();
        if span.first == span.last && !span.piece(span.first).is_full_chunk() {
            return read_chunk_part(&conn, ino, span, out);
        }
        let last = span.last.min(MAX_CHUNKS - 1);
        let mut stmt = conn.prepare_cached(
            "SELECT id, data FROM file_chunks WHERE id BETWEEN ?1 AND ?2 ORDER BY id",
        )?;
        let mut rows = stmt.query(params![chunk_key(ino, span.first), chunk_key(ino, last)])?;
        while let Some(row) = rows.next()? {
            let chunk_index = chunk_index_of(row.get(0)?);
            let data = row.get_ref(1)?.as_blob().map_err(SqlError::from)?;
            span.copy_out(chunk_index, data, out);
        }
        Ok(())
    }

    pub fn write_file(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32, DbError> {
        self.write_file_batch(ino, &[(offset, data)])
    }

    pub fn write_file_batch(&self, ino: u64, writes: &[(u64, &[u8])]) -> Result<u32, DbError> {
        self.write_file_batch_at(ino, writes, now_ts())
    }

    pub(crate) fn write_file_batch_at(
        &self,
        ino: u64,
        writes: &[(u64, &[u8])],
        modified: Timestamp,
    ) -> Result<u32, DbError> {
        let (kind, mut new_size) = self.inode_head(ino)?;
        if let Some(error) = kind.regular_file_error() {
            return Err(error);
        }
        if writes.iter().all(|(_, data)| data.is_empty()) {
            return Ok(0);
        }
        if writes
            .iter()
            .any(|&(offset, data)| offset.saturating_add(data.len() as u64) > MAX_FILE_SIZE)
        {
            return Err(DbError::FileTooLarge);
        }

        let mut written = 0usize;
        // One scratch buffer per call, reused for every partial chunk.
        let mut scratch = Vec::with_capacity(CHUNK_SIZE);

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        {
            let mut read_stmt = tx.prepare_cached("SELECT data FROM file_chunks WHERE id = ?1")?;
            let mut write_stmt = tx.prepare_cached(UPSERT_CHUNK)?;

            for &(offset, data) in writes {
                let Some(span) = ChunkSpan::new(offset, data.len()) else {
                    continue;
                };

                for chunk_index in span.first..=span.last {
                    let piece = span.piece(chunk_index);
                    let input = &data[piece.buf.clone()];

                    let key = chunk_key(ino, chunk_index);
                    if piece.is_full_chunk() {
                        write_stmt.execute(params![key, input])?;
                        continue;
                    }

                    scratch.clear();
                    let mut rows = read_stmt.query(params![key])?;
                    if let Some(row) = rows.next()? {
                        scratch
                            .extend_from_slice(row.get_ref(0)?.as_blob().map_err(SqlError::from)?);
                    }
                    drop(rows);
                    let required_len = piece.chunk.end;
                    if scratch.len() < required_len {
                        scratch.resize(required_len, 0);
                    }
                    scratch[piece.chunk.clone()].copy_from_slice(input);
                    write_stmt.execute(params![key, &scratch[..]])?;
                }

                new_size = new_size.max(offset + data.len() as u64);
                written += data.len();
            }
        }

        set_size(&tx, ino, new_size, modified)?;
        tx.commit()?;

        Ok(written as u32)
    }

    /// Stores `chunks` and sets size and mtime, in one transaction. A chunk whose
    /// blocks are all dirty is stored whole. Any other chunk is patched: its stored
    /// bytes are read, the dirty blocks are copied over them and the result is
    /// stored. Only the blocks that were dirty change, and the row keeps its length
    /// when it already covers them (see `blocks::merge`).
    pub(crate) fn replace_chunks<'a>(
        &self,
        ino: u64,
        size: u64,
        chunks: impl IntoIterator<Item = ChunkUpdate<'a>>,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        {
            let mut read_stmt = tx.prepare_cached("SELECT data FROM file_chunks WHERE id = ?1")?;
            let mut write_stmt = tx.prepare_cached(UPSERT_CHUNK)?;
            let mut merged = Vec::new();
            for update in chunks {
                let key = chunk_key(ino, update.chunk_index);
                let limit = chunk_limit(size, update.chunk_index);
                debug_assert!(limit > 0, "a dirty chunk lies inside its file");
                if update.dirty == ALL_BLOCKS {
                    write_stmt.execute(params![key, &update.buffer[..limit]])?;
                    continue;
                }
                {
                    let mut rows = read_stmt.query(params![key])?;
                    let stored: &[u8] = match rows.next()? {
                        Some(row) => row.get_ref(0)?.as_blob().map_err(SqlError::from)?,
                        None => &[],
                    };
                    blocks::merge(stored, update.dirty, update.buffer, limit, &mut merged);
                }
                write_stmt.execute(params![key, &merged[..]])?;
            }
        }

        set_size(&tx, ino, size, now)?;
        tx.commit()?;

        Ok(())
    }

    pub fn truncate_file(&self, ino: u64, size: u64) -> Result<(), DbError> {
        if let Some(error) = self.inode_kind_of(ino)?.regular_file_error() {
            return Err(error);
        }
        if size > MAX_FILE_SIZE {
            return Err(DbError::FileTooLarge);
        }

        let now = now_ts();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (first_key, last_key) = chunk_keys(ino);

        if size == 0 {
            tx.prepare_cached("DELETE FROM file_chunks WHERE id BETWEEN ?1 AND ?2")?
                .execute(params![first_key, last_key])?;
        } else {
            let final_chunk_index = (size - 1) / CHUNK_SIZE as u64;
            let final_chunk_len = ((size - 1) as usize % CHUNK_SIZE) + 1;
            let final_key = chunk_key(ino, final_chunk_index);

            tx.prepare_cached("DELETE FROM file_chunks WHERE id > ?1 AND id <= ?2")?
                .execute(params![final_key, last_key])?;

            // Cut the final chunk in SQL; the blob never leaves SQLite.
            tx.prepare_cached(
                "UPDATE file_chunks SET data = substr(data, 1, ?1)
                 WHERE id = ?2 AND length(data) > ?1",
            )?
            .execute(params![final_chunk_len as i64, final_key])?;
        }

        set_size(&tx, ino, size, now)?;
        tx.commit()?;

        Ok(())
    }

    pub fn unlink_file(&self, parent_ino: u64, name: &[u8]) -> Result<(), DbError> {
        let target = self.lookup(parent_ino, name)?;
        self.unlink_entry(parent_ino, name, &target, false)
    }

    /// Removes entry `name` of an already resolved `target`. When this removes
    /// the last name and `is_open` is set, the inode stays with `nlink = 0`
    /// until the caller deletes it (`delete_inode`); a crash leaves it for the
    /// startup sweep.
    pub(crate) fn unlink_entry(
        &self,
        parent_ino: u64,
        name: &[u8],
        target: &Inode,
        is_open: bool,
    ) -> Result<(), DbError> {
        if target.kind == FileKind::Directory {
            return Err(DbError::IsDirectory);
        }

        let now = now_ts();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        tx.prepare_cached("DELETE FROM dirents WHERE parent_ino = ?1 AND name = ?2")?
            .execute(params![parent_ino as i64, name])?;

        drop_unlinked_inode(&tx, target, now, is_open)?;
        touch_dir(&tx, parent_ino, now)?;
        tx.commit()?;

        Ok(())
    }

    pub fn remove_dir(&self, parent_ino: u64, name: &[u8]) -> Result<(), DbError> {
        let target = self.lookup(parent_ino, name)?;
        if target.kind != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }

        let now = now_ts();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let child_count: i64 = tx
            .prepare_cached("SELECT COUNT(*) FROM dirents WHERE parent_ino = ?1")?
            .query_row(params![target.ino as i64], |row| row.get(0))?;
        if child_count != 0 {
            return Err(DbError::DirectoryNotEmpty);
        }

        tx.prepare_cached("DELETE FROM dirents WHERE parent_ino = ?1 AND name = ?2")?
            .execute(params![parent_ino as i64, name])?;
        delete_inode_rows(&tx, target.ino)?;
        touch_dir(&tx, parent_ino, now)?;
        add_directory_nlink(&tx, parent_ino, -1)?;
        tx.commit()?;

        Ok(())
    }

    pub fn rename(
        &self,
        parent_ino: u64,
        name: &[u8],
        new_parent_ino: u64,
        new_name: &[u8],
    ) -> Result<(), DbError> {
        self.rename_entry(parent_ino, name, new_parent_ino, new_name, false, |_| false)
            .map(drop)
    }

    /// Renames an entry. Returns the inode whose entry was replaced, if any.
    /// `is_open(ino)` says whether the replaced inode has open handles, which
    /// keeps it (with `nlink = 0`) when this was its last name.
    pub(crate) fn rename_entry(
        &self,
        parent_ino: u64,
        name: &[u8],
        new_parent_ino: u64,
        new_name: &[u8],
        noreplace: bool,
        is_open: impl Fn(u64) -> bool,
    ) -> Result<Option<Inode>, DbError> {
        check_name(new_name)?;
        let source = self.lookup(parent_ino, name)?;
        if self.inode_kind_of(new_parent_ino)? != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }
        if source.kind == FileKind::Directory && self.is_descendant(new_parent_ino, source.ino)? {
            return Err(DbError::InvalidInput);
        }

        if parent_ino == new_parent_ino && name == new_name {
            return Ok(None);
        }

        let existing_target = match self.lookup(new_parent_ino, new_name) {
            Ok(target) => Some(target),
            Err(DbError::NotFound) => None,
            Err(error) => return Err(error),
        };

        if noreplace && existing_target.is_some() {
            return Err(DbError::AlreadyExists);
        }

        // The target is another name for the source inode. POSIX makes this a
        // no-op: both names stay, and nothing is removed.
        if existing_target
            .as_ref()
            .is_some_and(|target| target.ino == source.ino)
        {
            return Ok(None);
        }

        if let Some(target) = &existing_target {
            match (source.kind, target.kind) {
                (FileKind::RegularFile, FileKind::Directory)
                | (FileKind::Symlink, FileKind::Directory) => return Err(DbError::IsDirectory),
                (FileKind::Directory, FileKind::RegularFile)
                | (FileKind::Directory, FileKind::Symlink) => return Err(DbError::NotDirectory),
                (FileKind::Directory, FileKind::Directory)
                    if self.directory_has_entries(target.ino)? =>
                {
                    return Err(DbError::DirectoryNotEmpty);
                }
                _ => {}
            }
        }

        let source_is_dir = source.kind == FileKind::Directory;
        let target_is_dir = existing_target
            .as_ref()
            .is_some_and(|target| target.kind == FileKind::Directory);

        let now = now_ts();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        if let Some(target) = &existing_target {
            tx.prepare_cached("DELETE FROM dirents WHERE parent_ino = ?1 AND name = ?2")?
                .execute(params![new_parent_ino as i64, new_name])?;
            drop_unlinked_inode(&tx, target, now, is_open(target.ino))?;
        }

        tx.prepare_cached(
            "UPDATE dirents SET parent_ino = ?1, name = ?2 WHERE parent_ino = ?3 AND name = ?4",
        )?
        .execute(params![
            new_parent_ino as i64,
            new_name,
            parent_ino as i64,
            name
        ])?;
        touch_dir(&tx, parent_ino, now)?;
        if new_parent_ino != parent_ino {
            touch_dir(&tx, new_parent_ino, now)?;
        }
        if source_is_dir && parent_ino != new_parent_ino {
            add_directory_nlink(&tx, parent_ino, -1)?;
            add_directory_nlink(&tx, new_parent_ino, 1)?;
        }
        if target_is_dir {
            add_directory_nlink(&tx, new_parent_ino, -1)?;
        }
        tx.commit()?;

        Ok(existing_target)
    }

    pub fn update_metadata(&self, ino: u64, update: MetadataUpdate) -> Result<Inode, DbError> {
        let inode = self.get_inode(ino)?;
        let now = now_ts();
        let mode = update.mode.unwrap_or(inode.mode);
        let uid = update.uid.unwrap_or(inode.uid);
        let gid = update.gid.unwrap_or(inode.gid);
        let atime = update.atime.unwrap_or(inode.atime);
        let mtime = update.mtime.unwrap_or(inode.mtime);

        let conn = self.conn.lock().unwrap();
        conn.prepare_cached(
            "UPDATE inodes
             SET mode = ?1, uid = ?2, gid = ?3, atime_ns = ?4, mtime_ns = ?5, ctime_ns = ?6
             WHERE ino = ?7",
        )?
        .execute(params![
            mode as i64,
            uid as i64,
            gid as i64,
            atime.to_ns(),
            mtime.to_ns(),
            now.to_ns(),
            ino as i64
        ])?;
        drop(conn);

        self.get_inode(ino)
    }

    fn create_node(
        &self,
        parent_ino: u64,
        name: &[u8],
        node: NewNode<'_>,
        uid: u32,
        gid: u32,
    ) -> Result<Inode, DbError> {
        check_name(name)?;
        if self.inode_kind_of(parent_ino)? != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }

        let now = now_ts();
        let (kind, mode, size) = match node {
            NewNode::Directory { mode } => (FileKind::Directory, mode, 0),
            NewNode::File { mode } => (FileKind::RegularFile, mode, 0),
            NewNode::Symlink { target } => (FileKind::Symlink, 0o777, target.len() as u64),
        };
        let nlink: i64 = if kind == FileKind::Directory { 2 } else { 1 };
        let parent_delta: i64 = if kind == FileKind::Directory { 1 } else { 0 };
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        tx.prepare_cached(
            "INSERT INTO inodes (kind, mode, uid, gid, size, atime_ns, mtime_ns, ctime_ns, nlink)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?6, ?7)",
        )?
        .execute(params![
            kind.as_i64(),
            mode as i64,
            uid as i64,
            gid as i64,
            size as i64,
            now.to_ns(),
            nlink
        ])?;
        let ino = tx.last_insert_rowid() as u64;
        if ino > MAX_INO {
            // Chunk keys need the inode number to fit in 39 bits.
            return Err(DbError::NoSpace);
        }

        if let Err(error) = tx
            .prepare_cached("INSERT INTO dirents (parent_ino, name, child_ino) VALUES (?1, ?2, ?3)")
            .and_then(|mut s| s.execute(params![parent_ino as i64, name, ino as i64]))
        {
            return Err(error.into());
        }

        if let NewNode::Symlink { target } = node {
            tx.prepare_cached("INSERT INTO symlink_targets (ino, target) VALUES (?1, ?2)")?
                .execute(params![ino as i64, target])?;
        }

        touch_dir(&tx, parent_ino, now)?;
        if parent_delta != 0 {
            add_directory_nlink(&tx, parent_ino, parent_delta)?;
        }
        tx.commit()?;

        Ok(Inode {
            ino,
            kind,
            mode,
            uid,
            gid,
            size,
            atime: now,
            mtime: now,
            ctime: now,
            nlink: nlink as u32,
        })
    }

    /// Whether directory `ino` has any entry. Stops at the first row, so a large
    /// directory is not listed just to learn that it is not empty.
    fn directory_has_entries(&self, ino: u64) -> Result<bool, DbError> {
        let conn = self.conn.lock().unwrap();
        conn.prepare_cached("SELECT EXISTS (SELECT 1 FROM dirents WHERE parent_ino = ?1)")?
            .query_row(params![ino as i64], |row| row.get::<_, bool>(0))
            .map_err(Into::into)
    }

    fn is_descendant(&self, ino: u64, possible_ancestor: u64) -> Result<bool, DbError> {
        if ino == possible_ancestor {
            return Ok(true);
        }

        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare_cached("SELECT parent_ino FROM dirents WHERE child_ino = ?1")?;
        let mut current = ino;
        while current != 1 {
            let parent = stmt
                .query_row(params![current as i64], |row| row.get::<_, i64>(0))
                .optional()?;

            let Some(parent) = parent else {
                return Ok(false);
            };

            let parent = parent as u64;
            if parent == possible_ancestor {
                return Ok(true);
            }
            current = parent;
        }

        Ok(false)
    }
}

/// The chunks covered by the byte range `offset..offset + len` (`len > 0`).
/// The one place that does chunk boundary arithmetic for reads, writes and the
/// dirty overlay.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChunkSpan {
    pub start: u64,
    pub end: u64,
    pub first: u64,
    pub last: u64,
}

/// The part of one chunk that a span touches.
#[derive(Debug, Clone)]
pub(crate) struct ChunkPiece {
    /// Byte range within the chunk.
    pub chunk: std::ops::Range<usize>,
    /// The same bytes as a range within the caller's buffer.
    pub buf: std::ops::Range<usize>,
}

impl ChunkPiece {
    pub fn is_full_chunk(&self) -> bool {
        self.chunk.start == 0 && self.chunk.end == CHUNK_SIZE
    }
}

impl ChunkSpan {
    pub fn new(offset: u64, len: usize) -> Option<Self> {
        if len == 0 {
            return None;
        }
        let end = offset + len as u64;
        let chunk = CHUNK_SIZE as u64;
        Some(Self {
            start: offset,
            end,
            first: offset / chunk,
            last: (end - 1) / chunk,
        })
    }

    /// The piece of chunk `chunk_index` (which must be in `first..=last`).
    pub fn piece(&self, chunk_index: u64) -> ChunkPiece {
        let chunk_start = chunk_index * CHUNK_SIZE as u64;
        let from = self.start.max(chunk_start);
        let to = self.end.min(chunk_start + CHUNK_SIZE as u64);
        ChunkPiece {
            chunk: (from - chunk_start) as usize..(to - chunk_start) as usize,
            buf: (from - self.start) as usize..(to - self.start) as usize,
        }
    }

    /// Copies the bytes of a stored chunk that fall in this span into `out`
    /// (the span's buffer). Bytes past the end of a short chunk are untouched.
    pub fn copy_out(&self, chunk_index: u64, data: &[u8], out: &mut [u8]) {
        let piece = self.piece(chunk_index);
        let avail = data.len().min(piece.chunk.end);
        if avail <= piece.chunk.start {
            return;
        }
        let len = avail - piece.chunk.start;
        out[piece.buf.start..piece.buf.start + len]
            .copy_from_slice(&data[piece.chunk.start..avail]);
    }

    /// Copies the dirty blocks of chunk `chunk_index` that this span covers from
    /// `buffer` (the chunk's bytes in the write buffer) into `out` (the span's
    /// buffer). Other blocks are left alone.
    pub fn copy_dirty_blocks(
        &self,
        chunk_index: u64,
        dirty: BlockMask,
        buffer: &[u8],
        out: &mut [u8],
    ) {
        let piece = self.piece(chunk_index);
        for block in blocks::iter(dirty) {
            let bytes = block_range(block);
            let from = bytes.start.max(piece.chunk.start);
            let to = bytes.end.min(piece.chunk.end);
            if from < to {
                let at = piece.buf.start + (from - piece.chunk.start);
                out[at..at + (to - from)].copy_from_slice(&buffer[from..to]);
            }
        }
    }
}

/// One chunk to store: its write-buffer bytes and which blocks of them are dirty.
pub(crate) struct ChunkUpdate<'a> {
    pub chunk_index: u64,
    /// The whole chunk buffer. Only the blocks in `dirty` are meaningful.
    pub buffer: &'a [u8],
    pub dirty: BlockMask,
}

/// How many bytes of chunk `chunk_index` lie inside a file of `size` bytes.
fn chunk_limit(size: u64, chunk_index: u64) -> usize {
    size.saturating_sub(chunk_index * CHUNK_SIZE as u64)
        .min(CHUNK_SIZE as u64) as usize
}

/// Copies the bytes that `span` asks for from one chunk into `out`, reading
/// them through the chunk's blob. The span must lie within one chunk and not
/// cover all of it. Only the requested bytes are read, so SQLite does not load
/// the rest of the row's overflow pages. A missing row is a hole: `out` keeps
/// its zeros. Bytes past the end of a short blob are holes too.
fn read_chunk_part(
    conn: &Connection,
    ino: u64,
    span: ChunkSpan,
    out: &mut [u8],
) -> Result<(), DbError> {
    let piece = span.piece(span.first);
    let blob = match conn.blob_open(
        "main",
        "file_chunks",
        "data",
        chunk_key(ino, span.first),
        true,
    ) {
        Ok(blob) => blob,
        Err(error) if is_missing_row(&error) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let avail = blob.len().min(piece.chunk.end);
    if avail > piece.chunk.start {
        let len = avail - piece.chunk.start;
        let dst = &mut out[piece.buf.start..piece.buf.start + len];
        blob.read_at_exact(dst, piece.chunk.start)?;
    }
    Ok(())
}

/// `blob_open` reports a missing row as a generic SQLite error, "no such rowid: N".
fn is_missing_row(error: &SqlError) -> bool {
    matches!(error, SqlError::SqliteFailure(_, Some(message)) if message.starts_with("no such rowid"))
}

/// Removes the inode behind a deleted entry: deletes directories and inodes
/// whose last name went away, unless `is_open`, which keeps them with
/// `nlink = 0`; otherwise drops one link.
fn drop_unlinked_inode(
    tx: &rusqlite::Transaction<'_>,
    target: &Inode,
    now: Timestamp,
    is_open: bool,
) -> Result<(), DbError> {
    if target.kind == FileKind::Directory {
        return delete_inode_rows(tx, target.ino);
    }

    if target.nlink > 1 || is_open {
        let nlink = target.nlink.saturating_sub(1);
        tx.prepare_cached("UPDATE inodes SET nlink = ?1, ctime_ns = ?2 WHERE ino = ?3")?
            .execute(params![nlink as i64, now.to_ns(), target.ino as i64])?;
        return Ok(());
    }

    delete_inode_rows(tx, target.ino)
}

const UPSERT_CHUNK: &str = "INSERT INTO file_chunks (id, data) VALUES (?1, ?2)
     ON CONFLICT (id) DO UPDATE SET data = excluded.data";

/// Deletes inode `ino` and its chunk range. Entries and the symlink target go
/// through their foreign-key cascades; chunks have no foreign key.
fn delete_inode_rows(conn: &Connection, ino: u64) -> Result<(), DbError> {
    let (first, last) = chunk_keys(ino);
    conn.prepare_cached("DELETE FROM file_chunks WHERE id BETWEEN ?1 AND ?2")?
        .execute(params![first, last])?;
    conn.prepare_cached("DELETE FROM inodes WHERE ino = ?1")?
        .execute(params![ino as i64])?;
    Ok(())
}

/// Sets size, mtime and ctime of `ino` after a data change.
fn set_size(conn: &Connection, ino: u64, size: u64, modified: Timestamp) -> Result<(), DbError> {
    conn.prepare_cached(
        "UPDATE inodes SET size = ?1, mtime_ns = ?2, ctime_ns = ?2 WHERE ino = ?3",
    )?
    .execute(params![size as i64, modified.to_ns(), ino as i64])?;
    Ok(())
}

/// Sets mtime and ctime of directory `ino` after an entry change.
fn touch_dir(conn: &Connection, ino: u64, now: Timestamp) -> Result<(), DbError> {
    conn.prepare_cached("UPDATE inodes SET mtime_ns = ?1, ctime_ns = ?1 WHERE ino = ?2")?
        .execute(params![now.to_ns(), ino as i64])?;
    Ok(())
}

const NAME_MAX: usize = 255;

fn check_name(name: &[u8]) -> Result<(), DbError> {
    if name.len() > NAME_MAX {
        Err(DbError::NameTooLong)
    } else {
        Ok(())
    }
}

fn sync_database_file(db_path: &Path) -> std::io::Result<()> {
    let wal_path = path_with_suffix(db_path, "-wal");
    match File::open(&wal_path) {
        Ok(file) => file.sync_data(),
        Err(error) if error.kind() == ErrorKind::NotFound => File::open(db_path)?.sync_data(),
        Err(error) => Err(error),
    }
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Takes the lock that keeps a second dbfs off the same database: an exclusive,
/// non-blocking `flock` on the sibling file `<db>.lock`, held while the returned
/// file is open. `flock` belongs to the open file description, so closing other
/// descriptors (SQLite's own) cannot release it. The lock file is never deleted:
/// a deleted lock file would let the next process lock a different inode.
/// In-memory databases have no file to share, so they take no lock.
fn lock_database(path: &Path) -> Result<Option<File>, DbError> {
    if path.as_os_str().is_empty() || path == Path::new(":memory:") {
        return Ok(None);
    }
    let lock_path = path_with_suffix(path, ".lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| {
            DbError::Sqlite(format!("cannot open {}: {error}", lock_path.display()))
        })?;
    // SAFETY: `flock` only uses the descriptor, which `file` keeps open.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == ErrorKind::WouldBlock {
        return Err(DbError::DatabaseInUse);
    }
    Err(DbError::Sqlite(format!(
        "cannot lock {}: {error}",
        lock_path.display()
    )))
}

fn pragma_query(name: &str) -> Result<String, DbError> {
    match name {
        "busy_timeout" | "foreign_keys" | "journal_mode" | "mmap_size" | "synchronous"
        | "user_version" => Ok(format!("PRAGMA {name}")),
        _ => Err(DbError::InvalidInput),
    }
}

/// Sets `PRAGMA mmap_size` on `conn` and reads back the size SQLite applied.
fn apply_mmap_size(conn: &Connection, bytes: u64) -> Result<u64, DbError> {
    let requested = i64::try_from(bytes).map_err(|_| DbError::InvalidInput)?;
    conn.pragma_update(None, "mmap_size", requested)?;
    let effective: i64 = conn.query_row("PRAGMA mmap_size", [], |row| row.get(0))?;
    Ok(effective.max(0) as u64)
}

fn inode_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Inode> {
    let kind_value = row.get(1)?;
    let kind = FileKind::from_i64(kind_value).map_err(|err| {
        SqlError::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::other(format!("{err:?}"))),
        )
    })?;

    Ok(Inode {
        ino: row.get::<_, i64>(0)? as u64,
        kind,
        mode: row.get::<_, i64>(2)? as u32,
        uid: row.get::<_, i64>(3)? as u32,
        gid: row.get::<_, i64>(4)? as u32,
        size: row.get::<_, i64>(5)? as u64,
        atime: Timestamp::from_ns(row.get(6)?),
        mtime: Timestamp::from_ns(row.get(7)?),
        ctime: Timestamp::from_ns(row.get(8)?),
        nlink: row.get::<_, i64>(9)? as u32,
    })
}

fn recompute_directory_nlink(conn: &Connection) -> Result<(), DbError> {
    let directory = FileKind::Directory.as_i64();
    conn.execute(
        "UPDATE inodes
         SET nlink = 2 + (
           SELECT COUNT(*)
           FROM dirents d
           JOIN inodes child ON child.ino = d.child_ino
           WHERE d.parent_ino = inodes.ino AND child.kind = ?1
         )
         WHERE kind = ?1",
        [directory],
    )?;
    Ok(())
}

fn add_directory_nlink(
    tx: &rusqlite::Transaction<'_>,
    ino: u64,
    delta: i64,
) -> Result<(), DbError> {
    tx.prepare_cached("UPDATE inodes SET nlink = nlink + ?1 WHERE ino = ?2")?
        .execute(params![delta, ino as i64])?;
    Ok(())
}

/// Refuses a database written by a newer dbfs. Only reads `user_version`.
fn refuse_newer_schema(conn: &Connection) -> Result<(), DbError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(DbError::UnsupportedSchema(version));
    }
    Ok(())
}

/// Bring an existing database up to `SCHEMA_VERSION`, one step at a time,
/// in a single transaction. A failure leaves the database at its old version.
///
/// Steps look at the actual table layout, not only at the version number: a
/// database created by this dbfs starts at version 0 with the current layout
/// (from `CREATE TABLE IF NOT EXISTS`), and must not be "upgraded" again.
fn migrate(conn: &Connection) -> Result<(), DbError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    // `open` refuses newer schemas before writing; this keeps migrate from ever
    // stamping an older version over one.
    if version > SCHEMA_VERSION {
        return Err(DbError::UnsupportedSchema(version));
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }

    // Rebuilding `inodes` drops the old table. With foreign keys on, that would
    // cascade into `dirents` and `symlink_targets`. SQLite ignores this pragma
    // inside a transaction, so it is set around it.
    let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    conn.pragma_update(None, "foreign_keys", 0)?;
    let result = migrate_steps(conn, version);
    conn.pragma_update(None, "foreign_keys", foreign_keys)?;
    result
}

fn migrate_steps(conn: &Connection, version: i64) -> Result<(), DbError> {
    let tx = conn.unchecked_transaction()?;
    let split_timestamps = !table_has_column(&tx, "inodes", "mtime_ns")?;
    if version < 1 && split_timestamps {
        ensure_nsec_columns(&tx)?;
    }
    if version < 2 {
        // Without this index, `is_descendant` and the `child_ino` foreign-key
        // cascade on every inode delete scan the whole `dirents` table.
        tx.execute_batch("CREATE INDEX IF NOT EXISTS dirents_child ON dirents(child_ino);")?;
    }
    if version < 3 {
        // Give every entry a stable id for readdir cookies. Positional cookies
        // (LIMIT/OFFSET) shift when entries are deleted between readdir calls,
        // so `rm -rf` skipped about half of a large directory.
        if !table_has_column(&tx, "dirents", "id")? {
            tx.execute_batch(
                "CREATE TABLE dirents_v3 (
                   id INTEGER PRIMARY KEY,
                   parent_ino INTEGER NOT NULL,
                   name BLOB NOT NULL,
                   child_ino INTEGER NOT NULL,
                   FOREIGN KEY (parent_ino) REFERENCES inodes(ino) ON DELETE CASCADE,
                   FOREIGN KEY (child_ino) REFERENCES inodes(ino) ON DELETE CASCADE
                 );
                 INSERT INTO dirents_v3 (parent_ino, name, child_ino)
                   SELECT parent_ino, name, child_ino FROM dirents ORDER BY parent_ino, name;
                 DROP TABLE dirents;
                 ALTER TABLE dirents_v3 RENAME TO dirents;",
            )?;
        }
        // Every SQLite index ends with the rowid, so `dirents_scan` on
        // (parent_ino) is ordered by id within a directory and serves the
        // keyset query in `read_dir_after` without a sort.
        tx.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS dirents_name ON dirents(parent_ino, name);
             CREATE INDEX IF NOT EXISTS dirents_child ON dirents(child_ino);
             CREATE INDEX IF NOT EXISTS dirents_scan ON dirents(parent_ino);",
        )?;
    }
    if version < 4 {
        if split_timestamps {
            merge_timestamp_columns(&tx)?;
        }
        if table_has_column(&tx, "file_chunks", "chunk_index")? {
            rekey_file_chunks(&tx)?;
        }
    }
    if version < 5 && !inodes_is_autoincrement(&tx)? {
        autoincrement_inodes(&tx)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

/// Version 4: one i64 nanosecond column per timestamp instead of a
/// (seconds, nanoseconds) pair. Out-of-range times are clamped.
fn merge_timestamp_columns(conn: &Connection) -> Result<(), DbError> {
    fn ns(secs: &str, nsec: &str) -> String {
        format!(
            "CASE WHEN {secs} > 9223372035 THEN 9223372036854775807
                  WHEN {secs} < -9223372035 THEN (-9223372036854775807 - 1)
                  WHEN {secs} < 0 THEN {secs} * 1000000000 - {nsec}
                  ELSE {secs} * 1000000000 + {nsec} END"
        )
    }
    conn.execute_batch(&format!(
        "CREATE TABLE inodes_v4 (
           ino INTEGER PRIMARY KEY,
           kind INTEGER NOT NULL,
           mode INTEGER NOT NULL,
           uid INTEGER NOT NULL,
           gid INTEGER NOT NULL,
           size INTEGER NOT NULL,
           atime_ns INTEGER NOT NULL,
           mtime_ns INTEGER NOT NULL,
           ctime_ns INTEGER NOT NULL,
           nlink INTEGER NOT NULL
         );
         INSERT INTO inodes_v4
           SELECT ino, kind, mode, uid, gid, size, {}, {}, {}, nlink FROM inodes;
         DROP TABLE inodes;
         ALTER TABLE inodes_v4 RENAME TO inodes;",
        ns("atime", "atime_nsec"),
        ns("mtime", "mtime_nsec"),
        ns("ctime", "ctime_nsec"),
    ))?;
    Ok(())
}

/// Version 4: chunks in one B-tree keyed by `(ino << 24) | chunk_index`,
/// instead of a rowid table plus a (ino, chunk_index) primary-key index.
/// Rewrites every chunk, so large databases print a note first.
fn rekey_file_chunks(conn: &Connection) -> Result<(), DbError> {
    let too_large: i64 = conn.query_row(
        "SELECT COUNT(*) FROM file_chunks WHERE chunk_index >= ?1 OR ino > ?2",
        params![MAX_CHUNKS as i64, MAX_INO as i64],
        |row| row.get(0),
    )?;
    if too_large > 0 {
        return Err(DbError::Sqlite(format!(
            "cannot upgrade to schema version {SCHEMA_VERSION}: {too_large} chunks belong to \
             data past the 1 TiB file size limit"
        )));
    }
    let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM file_chunks", [], |row| row.get(0))?;
    if chunks > 4_096 {
        eprintln!(
            "dbfs: upgrading {chunks} data chunks ({} MiB) to schema version {SCHEMA_VERSION}; \
             this runs once and leaves the old pages free for reuse",
            chunks * CHUNK_SIZE as i64 / (1 << 20)
        );
    }
    conn.execute_batch(&format!(
        "CREATE TABLE file_chunks_v4 (id INTEGER PRIMARY KEY, data BLOB NOT NULL);
         INSERT INTO file_chunks_v4 (id, data)
           SELECT (ino << {CHUNK_BITS}) | chunk_index, data FROM file_chunks
           WHERE ino IN (SELECT ino FROM inodes)
           ORDER BY ino, chunk_index;
         DROP TABLE file_chunks;
         ALTER TABLE file_chunks_v4 RENAME TO file_chunks;"
    ))?;
    Ok(())
}

/// Version 5: gives `inodes` AUTOINCREMENT, so the sequence remembers the highest
/// number ever used and a deleted number is not handed out again.
///
/// The table is rebuilt from its stored definition with AUTOINCREMENT added after
/// the primary key, so every other column and constraint is kept as it was, and
/// so are indexes and triggers on the table. Every `ino` is copied unchanged, so
/// the root and all references stay valid. The sequence starts at the highest
/// number still present. Numbers freed before this step are not recorded
/// anywhere, which is safe: this runs at mount start, and the kernel has dropped
/// every inode of the previous mount.
fn autoincrement_inodes(conn: &Connection) -> Result<(), DbError> {
    let stored: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'inodes'",
        [],
        |row| row.get(0),
    )?;
    let unexpected =
        || DbError::Sqlite("cannot add AUTOINCREMENT to inodes: unexpected definition".into());
    // Everything from the column list on; the name is replaced by `inodes_v5`.
    let columns = &stored[stored.find('(').ok_or_else(unexpected)?..];
    let key_end = columns
        .to_ascii_uppercase()
        .find("PRIMARY KEY")
        .ok_or_else(unexpected)?
        + "PRIMARY KEY".len();
    // If the key cannot take AUTOINCREMENT, SQLite refuses the CREATE and the
    // whole upgrade rolls back.
    let definition = format!(
        "{} AUTOINCREMENT{}",
        &columns[..key_end],
        &columns[key_end..]
    );

    // Indexes and triggers on `inodes` are dropped with the old table; recreate them.
    let attached: Vec<String> = conn
        .prepare(
            "SELECT sql FROM sqlite_master
             WHERE tbl_name = 'inodes' AND type IN ('index', 'trigger') AND sql IS NOT NULL",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    conn.execute_batch(&format!(
        "CREATE TABLE inodes_v5 {definition};
         INSERT INTO inodes_v5 SELECT * FROM inodes;
         DROP TABLE inodes;
         ALTER TABLE inodes_v5 RENAME TO inodes;"
    ))?;
    for sql in attached {
        conn.execute_batch(&sql)?;
    }
    Ok(())
}

/// Whether `inodes` already has AUTOINCREMENT. Checks the table's definition,
/// not the version: `initialize` creates it that way, so a fresh database must
/// not be rebuilt.
fn inodes_is_autoincrement(conn: &Connection) -> Result<bool, DbError> {
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'inodes'",
        [],
        |row| row.get(0),
    )?;
    Ok(sql.to_ascii_uppercase().contains("AUTOINCREMENT"))
}

fn ensure_nsec_columns(conn: &Connection) -> Result<(), DbError> {
    for column in ["atime_nsec", "mtime_nsec", "ctime_nsec"] {
        if table_has_column(conn, "inodes", column)? {
            continue;
        }
        conn.execute(
            &format!("ALTER TABLE inodes ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"),
            [],
        )?;
    }
    Ok(())
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, DbError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn now_ts() -> Timestamp {
    Timestamp::from_system_time(SystemTime::now())
}

#[cfg(test)]
impl Db {
    pub(crate) fn cap_page_count_at_current(&self) -> Result<(), DbError> {
        let pages: i64 = self
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA page_count", [], |row| row.get(0))?;
        self.conn
            .lock()
            .unwrap()
            .pragma_update(None, "max_page_count", pages)?;
        Ok(())
    }
}
