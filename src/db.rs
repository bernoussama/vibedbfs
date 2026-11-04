use std::fs::File;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use rusqlite::{Connection, Error as SqlError, OptionalExtension, params};

use crate::{DbError, DirEntry, FileKind, Inode, MetadataUpdate, Timestamp};

pub(crate) const CHUNK_SIZE: usize = 65_536;

/// On-disk schema version, stored in `PRAGMA user_version`.
///
/// - 0: databases created before versioning existed (may lack `*_nsec` columns)
/// - 1: `inodes` has `atime_nsec`, `mtime_nsec`, `ctime_nsec`
/// - 2: index `dirents_child` on `dirents(child_ino)`
/// - 3: `dirents.id` is a stable per-entry id, used as the readdir cookie
pub const SCHEMA_VERSION: i64 = 3;

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
}

#[derive(Clone, Copy)]
enum NewNode<'a> {
    Directory { mode: u32 },
    File { mode: u32 },
    Symlink { target: &'a [u8] },
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA busy_timeout = 5000;
            PRAGMA wal_autocheckpoint = 10000;
            ",
        )?;
        Self::initialize(conn)
    }

    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Self::initialize(conn)
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

    fn initialize(conn: Connection) -> Result<Self, DbError> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS inodes (
              ino INTEGER PRIMARY KEY,
              kind INTEGER NOT NULL,
              mode INTEGER NOT NULL,
              uid INTEGER NOT NULL,
              gid INTEGER NOT NULL,
              size INTEGER NOT NULL,
              atime INTEGER NOT NULL,
              mtime INTEGER NOT NULL,
              ctime INTEGER NOT NULL,
              atime_nsec INTEGER NOT NULL DEFAULT 0,
              mtime_nsec INTEGER NOT NULL DEFAULT 0,
              ctime_nsec INTEGER NOT NULL DEFAULT 0,
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

            CREATE TABLE IF NOT EXISTS file_chunks (
              ino INTEGER NOT NULL,
              chunk_index INTEGER NOT NULL,
              data BLOB NOT NULL,
              PRIMARY KEY (ino, chunk_index),
              FOREIGN KEY (ino) REFERENCES inodes(ino) ON DELETE CASCADE
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
             (ino, kind, mode, uid, gid, size, atime, mtime, ctime,
              atime_nsec, mtime_nsec, ctime_nsec, nlink)
             VALUES (1, ?1, ?2, ?3, ?4, 0, ?5, ?5, ?5, ?6, ?6, ?6, 2)",
            params![
                FileKind::Directory.as_i64(),
                0o755_i64,
                mount_uid,
                mount_gid,
                now.secs,
                now.nsec as i64,
            ],
        )?;
        conn.execute(
            "UPDATE inodes
             SET uid = ?1, gid = ?2, ctime = ?3, ctime_nsec = ?4
             WHERE ino = 1 AND uid = 0 AND gid = 0",
            params![mount_uid, mount_gid, now.secs, now.nsec as i64],
        )?;
        recompute_directory_nlink(&conn)?;
        conn.execute("DELETE FROM inodes WHERE nlink = 0 AND ino != 1", [])?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn get_inode(&self, ino: u64) -> Result<Inode, DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT ino, kind, mode, uid, gid, size, atime, mtime, ctime, nlink,
                    atime_nsec, mtime_nsec, ctime_nsec
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
            "SELECT i.ino, i.kind, i.mode, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime, i.nlink,
                    i.atime_nsec, i.mtime_nsec, i.ctime_nsec
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
        self.conn
            .lock()
            .unwrap()
            .prepare_cached("DELETE FROM inodes WHERE ino = ?1")?
            .execute(params![ino as i64])?;
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
        tx.prepare_cached(
            "UPDATE inodes SET nlink = ?1, ctime = ?2, ctime_nsec = ?3 WHERE ino = ?4",
        )?
        .execute(params![
            new_nlink as i64,
            now.secs,
            now.nsec as i64,
            ino as i64
        ])?;
        tx.prepare_cached(
            "UPDATE inodes SET mtime = ?1, mtime_nsec = ?2, ctime = ?1, ctime_nsec = ?2 WHERE ino = ?3",
        )?.execute(params![now.secs, now.nsec as i64, new_parent_ino as i64])?;
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
    /// `out`, with one range query. Blobs are borrowed from SQLite, not copied
    /// into a `Vec` first. Holes (missing chunks, or short trailing chunks) are
    /// left as they are, so the caller passes a zeroed buffer. Does not check
    /// the inode kind or size.
    pub(crate) fn read_into(&self, ino: u64, offset: u64, out: &mut [u8]) -> Result<(), DbError> {
        let Some(span) = ChunkSpan::new(offset, out.len()) else {
            return Ok(());
        };
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT chunk_index, data FROM file_chunks
             WHERE ino = ?1 AND chunk_index BETWEEN ?2 AND ?3
             ORDER BY chunk_index",
        )?;
        let mut rows = stmt.query(params![ino as i64, span.first as i64, span.last as i64])?;
        while let Some(row) = rows.next()? {
            let chunk_index = row.get::<_, i64>(0)? as u64;
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

        let mut written = 0usize;
        // One scratch buffer per call, reused for every partial chunk.
        let mut scratch = Vec::with_capacity(CHUNK_SIZE);

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        {
            let mut read_stmt = tx.prepare_cached(
                "SELECT data FROM file_chunks WHERE ino = ?1 AND chunk_index = ?2",
            )?;
            let mut write_stmt = tx.prepare_cached(
                "INSERT INTO file_chunks (ino, chunk_index, data) VALUES (?1, ?2, ?3)
                 ON CONFLICT (ino, chunk_index) DO UPDATE SET data = excluded.data",
            )?;

            for &(offset, data) in writes {
                let Some(span) = ChunkSpan::new(offset, data.len()) else {
                    continue;
                };

                for chunk_index in span.first..=span.last {
                    let piece = span.piece(chunk_index);
                    let input = &data[piece.buf.clone()];

                    if piece.is_full_chunk() {
                        write_stmt.execute(params![ino as i64, chunk_index as i64, input])?;
                        continue;
                    }

                    scratch.clear();
                    let mut rows = read_stmt.query(params![ino as i64, chunk_index as i64])?;
                    if let Some(row) = rows.next()? {
                        scratch.extend_from_slice(
                            row.get_ref(0)?.as_blob().map_err(SqlError::from)?,
                        );
                    }
                    drop(rows);
                    let required_len = piece.chunk.end;
                    if scratch.len() < required_len {
                        scratch.resize(required_len, 0);
                    }
                    scratch[piece.chunk.clone()].copy_from_slice(input);
                    write_stmt.execute(params![ino as i64, chunk_index as i64, &scratch[..]])?;
                }

                new_size = new_size.max(offset + data.len() as u64);
                written += data.len();
            }
        }

        tx.prepare_cached(
            "UPDATE inodes SET size = ?1, mtime = ?2, mtime_nsec = ?3, ctime = ?2, ctime_nsec = ?3
             WHERE ino = ?4",
        )?
        .execute(params![
            new_size as i64,
            modified.secs,
            modified.nsec as i64,
            ino as i64
        ])?;
        tx.commit()?;

        Ok(written as u32)
    }

    /// Copies the stored bytes of chunk `chunk_index` into the start of `dst`
    /// and returns how many there were (0 for a hole).
    pub(crate) fn read_chunk_into(
        &self,
        ino: u64,
        chunk_index: u64,
        dst: &mut [u8],
    ) -> Result<usize, DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare_cached("SELECT data FROM file_chunks WHERE ino = ?1 AND chunk_index = ?2")?;
        let mut rows = stmt.query(params![ino as i64, chunk_index as i64])?;
        let Some(row) = rows.next()? else {
            return Ok(0);
        };
        let data = row.get_ref(0)?.as_blob().map_err(SqlError::from)?;
        let len = data.len().min(dst.len());
        dst[..len].copy_from_slice(&data[..len]);
        Ok(len)
    }

    /// Upserts `chunks` (`(chunk_index, bytes)`) and sets size and mtime, in one
    /// transaction.
    pub(crate) fn replace_chunks<'a>(
        &self,
        ino: u64,
        size: u64,
        chunks: impl IntoIterator<Item = (u64, &'a [u8])>,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        {
            let mut write_stmt = tx.prepare_cached(
                "INSERT INTO file_chunks (ino, chunk_index, data) VALUES (?1, ?2, ?3)
                 ON CONFLICT (ino, chunk_index) DO UPDATE SET data = excluded.data",
            )?;
            for (chunk_index, data) in chunks {
                write_stmt.execute(params![ino as i64, chunk_index as i64, data])?;
            }
        }

        tx.prepare_cached(
            "UPDATE inodes SET size = ?1, mtime = ?2, mtime_nsec = ?3, ctime = ?2, ctime_nsec = ?3 WHERE ino = ?4",
        )?.execute(params![size as i64, now.secs, now.nsec as i64, ino as i64])?;
        tx.commit()?;

        Ok(())
    }

    pub fn truncate_file(&self, ino: u64, size: u64) -> Result<(), DbError> {
        if let Some(error) = self.inode_kind_of(ino)?.regular_file_error() {
            return Err(error);
        }

        let now = now_ts();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        if size == 0 {
            tx.prepare_cached("DELETE FROM file_chunks WHERE ino = ?1")?
                .execute(params![ino as i64])?;
        } else {
            let final_chunk_index = ((size - 1) as usize) / CHUNK_SIZE;
            let final_chunk_len = ((size - 1) as usize % CHUNK_SIZE) + 1;

            tx.prepare_cached("DELETE FROM file_chunks WHERE ino = ?1 AND chunk_index > ?2")?
                .execute(params![ino as i64, final_chunk_index as i64])?;

            // Cut the final chunk in SQL; the blob never leaves SQLite.
            tx.prepare_cached(
                "UPDATE file_chunks SET data = substr(data, 1, ?1)
                 WHERE ino = ?2 AND chunk_index = ?3 AND length(data) > ?1",
            )?
            .execute(params![
                final_chunk_len as i64,
                ino as i64,
                final_chunk_index as i64
            ])?;
        }

        tx.prepare_cached(
            "UPDATE inodes SET size = ?1, mtime = ?2, mtime_nsec = ?3, ctime = ?2, ctime_nsec = ?3
             WHERE ino = ?4",
        )?
        .execute(params![size as i64, now.secs, now.nsec as i64, ino as i64])?;
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

        tx.prepare_cached(
            "UPDATE inodes SET mtime = ?1, mtime_nsec = ?2, ctime = ?1, ctime_nsec = ?2 WHERE ino = ?3",
        )?.execute(params![now.secs, now.nsec as i64, parent_ino as i64])?;
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
        tx.prepare_cached("DELETE FROM inodes WHERE ino = ?1")?
            .execute(params![target.ino as i64])?;
        tx.prepare_cached(
            "UPDATE inodes
             SET mtime = ?1, mtime_nsec = ?2, ctime = ?1, ctime_nsec = ?2, nlink = nlink - 1
             WHERE ino = ?3",
        )?
        .execute(params![now.secs, now.nsec as i64, parent_ino as i64])?;
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

        if let Some(target) = &existing_target {
            match (source.kind, target.kind) {
                (FileKind::RegularFile, FileKind::Directory)
                | (FileKind::Symlink, FileKind::Directory) => return Err(DbError::IsDirectory),
                (FileKind::Directory, FileKind::RegularFile)
                | (FileKind::Directory, FileKind::Symlink) => return Err(DbError::NotDirectory),
                (FileKind::Directory, FileKind::Directory)
                    if !self.list_dir(target.ino)?.is_empty() =>
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
        tx.prepare_cached(
            "UPDATE inodes
             SET mtime = ?1, mtime_nsec = ?2, ctime = ?1, ctime_nsec = ?2
             WHERE ino = ?3 OR ino = ?4",
        )?
        .execute(params![
            now.secs,
            now.nsec as i64,
            parent_ino as i64,
            new_parent_ino as i64
        ])?;
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
             SET mode = ?1, uid = ?2, gid = ?3,
                 atime = ?4, atime_nsec = ?5,
                 mtime = ?6, mtime_nsec = ?7,
                 ctime = ?8, ctime_nsec = ?9
             WHERE ino = ?10",
        )?
        .execute(params![
            mode as i64,
            uid as i64,
            gid as i64,
            atime.secs,
            atime.nsec as i64,
            mtime.secs,
            mtime.nsec as i64,
            now.secs,
            now.nsec as i64,
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
            "INSERT INTO inodes
             (kind, mode, uid, gid, size, atime, mtime, ctime, atime_nsec, mtime_nsec, ctime_nsec, nlink)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?6, ?7, ?7, ?7, ?8)",
        )?.execute(params![
            kind.as_i64(),
            mode as i64,
            uid as i64,
            gid as i64,
            size as i64,
            now.secs,
            now.nsec as i64,
            nlink
        ])?;
        let ino = tx.last_insert_rowid() as u64;

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

        tx.prepare_cached(
            "UPDATE inodes
             SET mtime = ?1, mtime_nsec = ?2, ctime = ?1, ctime_nsec = ?2, nlink = nlink + ?3
             WHERE ino = ?4",
        )?
        .execute(params![
            now.secs,
            now.nsec as i64,
            parent_delta,
            parent_ino as i64
        ])?;
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
        tx.prepare_cached("DELETE FROM inodes WHERE ino = ?1")?
            .execute(params![target.ino as i64])?;
        return Ok(());
    }

    if target.nlink > 1 {
        tx.prepare_cached(
            "UPDATE inodes SET nlink = ?1, ctime = ?2, ctime_nsec = ?3 WHERE ino = ?4",
        )?
        .execute(params![
            (target.nlink - 1) as i64,
            now.secs,
            now.nsec as i64,
            target.ino as i64
        ])?;
        return Ok(());
    }

    if is_open {
        tx.prepare_cached(
            "UPDATE inodes SET nlink = 0, ctime = ?1, ctime_nsec = ?2 WHERE ino = ?3",
        )?
        .execute(params![now.secs, now.nsec as i64, target.ino as i64])?;
    } else {
        tx.prepare_cached("DELETE FROM inodes WHERE ino = ?1")?
            .execute(params![target.ino as i64])?;
    }

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

fn pragma_query(name: &str) -> Result<String, DbError> {
    match name {
        "busy_timeout" | "foreign_keys" | "journal_mode" | "synchronous" | "user_version" => {
            Ok(format!("PRAGMA {name}"))
        }
        _ => Err(DbError::InvalidInput),
    }
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
        atime: timestamp_from_row(row, 6, 10)?,
        mtime: timestamp_from_row(row, 7, 11)?,
        ctime: timestamp_from_row(row, 8, 12)?,
        nlink: row.get::<_, i64>(9)? as u32,
    })
}

fn timestamp_from_row(
    row: &rusqlite::Row<'_>,
    secs_idx: usize,
    nsec_idx: usize,
) -> rusqlite::Result<Timestamp> {
    let secs: i64 = row.get(secs_idx)?;
    let nsec: i64 = row.get(nsec_idx)?;
    if !(0..1_000_000_000).contains(&nsec) {
        return Err(SqlError::FromSqlConversionFailure(
            nsec_idx,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::other("timestamp nsec out of range")),
        ));
    }
    Ok(Timestamp {
        secs,
        nsec: nsec as u32,
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

/// Bring an existing database up to `SCHEMA_VERSION`, one step at a time,
/// in a single transaction. A failure leaves the database at its old version.
fn migrate(conn: &Connection) -> Result<(), DbError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(DbError::UnsupportedSchema(version));
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }

    let tx = conn.unchecked_transaction()?;
    if version < 1 {
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
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
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
