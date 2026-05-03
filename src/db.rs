use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, Error as SqlError, OptionalExtension, params};

use crate::{DbError, DirEntry, FileKind, Inode, MetadataUpdate};

const CHUNK_SIZE: usize = 65_536;

#[derive(Clone, Copy)]
struct ChunkPatch<'a> {
    offset: usize,
    data: &'a [u8],
}

pub struct Db {
    conn: RefCell<Connection>,
    /// Tracks how many open file handles reference each inode.
    /// When unlink removes the last directory entry but the inode is still
    /// open (open_count > 0), we defer deletion until the file is released.
    open_counts: RefCell<HashMap<u64, u64>>,
    /// Inodes pending deletion once all handles are closed.
    pending_delete: RefCell<Vec<u64>>,
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

    pub fn pragma_i64(&self, name: &str) -> Result<i64, DbError> {
        let sql = pragma_query(name)?;
        self.conn
            .borrow()
            .query_row(&sql, [], |row| row.get(0))
            .map_err(Into::into)
    }

    pub fn pragma_string(&self, name: &str) -> Result<String, DbError> {
        let sql = pragma_query(name)?;
        self.conn
            .borrow()
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
              nlink INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS dirents (
              parent_ino INTEGER NOT NULL,
              name BLOB NOT NULL,
              child_ino INTEGER NOT NULL,
              PRIMARY KEY (parent_ino, name),
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

        let now = now_secs();
        let mount_uid = unsafe { libc::getuid() } as i64;
        let mount_gid = unsafe { libc::getgid() } as i64;
        conn.execute(
            "INSERT OR IGNORE INTO inodes
             (ino, kind, mode, uid, gid, size, atime, mtime, ctime, nlink)
             VALUES (1, ?1, ?2, ?3, ?4, 0, ?5, ?5, ?5, 1)",
            params![
                FileKind::Directory.as_i64(),
                0o755_i64,
                mount_uid,
                mount_gid,
                now
            ],
        )?;
        conn.execute(
            "UPDATE inodes
             SET uid = ?1, gid = ?2, ctime = ?3
             WHERE ino = 1 AND uid = 0 AND gid = 0",
            params![mount_uid, mount_gid, now],
        )?;

        Ok(Self {
            conn: RefCell::new(conn),
            open_counts: RefCell::new(HashMap::new()),
            pending_delete: RefCell::new(Vec::new()),
        })
    }

    pub fn get_inode(&self, ino: u64) -> Result<Inode, DbError> {
        let conn = self.conn.borrow();
        conn.query_row(
            "SELECT ino, kind, mode, uid, gid, size, atime, mtime, ctime, nlink
             FROM inodes
             WHERE ino = ?1",
            params![ino as i64],
            inode_from_row,
        )
        .map_err(Into::into)
    }

    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<Inode, DbError> {
        let conn = self.conn.borrow();
        conn.query_row(
            "SELECT i.ino, i.kind, i.mode, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime, i.nlink
             FROM dirents d
             JOIN inodes i ON i.ino = d.child_ino
             WHERE d.parent_ino = ?1 AND d.name = ?2",
            params![parent_ino as i64, name],
            inode_from_row,
        )
        .map_err(Into::into)
    }

    pub fn list_dir(&self, ino: u64) -> Result<Vec<DirEntry>, DbError> {
        if self.get_inode(ino)?.kind != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }

        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
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

    pub fn create_dir(
        &self,
        parent_ino: u64,
        name: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Inode, DbError> {
        self.create_node(parent_ino, name, FileKind::Directory, mode, uid, gid)
    }

    pub fn create_file(
        &self,
        parent_ino: u64,
        name: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Inode, DbError> {
        self.create_node(parent_ino, name, FileKind::RegularFile, mode, uid, gid)
    }

    pub fn create_symlink(
        &self,
        parent_ino: u64,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<Inode, DbError> {
        let inode = self.create_node(parent_ino, name, FileKind::Symlink, 0o777, uid, gid)?;

        self.conn.borrow().execute(
            "INSERT INTO symlink_targets (ino, target) VALUES (?1, ?2)",
            params![inode.ino as i64, target],
        )?;

        // Update size to reflect target length
        let _now = now_secs();
        self.conn.borrow().execute(
            "UPDATE inodes SET size = ?1 WHERE ino = ?2",
            params![target.len() as i64, inode.ino as i64],
        )?;

        Ok(Inode {
            size: target.len() as u64,
            ..inode
        })
    }

    pub fn read_symlink(&self, ino: u64) -> Result<Vec<u8>, DbError> {
        let inode = self.get_inode(ino)?;
        if inode.kind != FileKind::Symlink {
            return Err(DbError::InvalidInput);
        }

        self.conn
            .borrow()
            .query_row(
                "SELECT target FROM symlink_targets WHERE ino = ?1",
                params![ino as i64],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Track that a file handle has been opened for this inode.
    pub fn open_file(&self, ino: u64) {
        *self.open_counts.borrow_mut().entry(ino).or_insert(0) += 1;
    }

    /// Release a file handle. If this was the last handle and the inode
    /// is pending deletion, delete it now.
    pub fn release_file(&self, ino: u64) {
        let mut open_counts = self.open_counts.borrow_mut();
        let count = open_counts.entry(ino).or_insert(0);
        if *count > 0 {
            *count -= 1;
        }

        if *count == 0 {
            open_counts.remove(&ino);
            drop(open_counts);

            // Check if this inode is pending deletion
            let mut pending = self.pending_delete.borrow_mut();
            if let Some(pos) = pending.iter().position(|&p| p == ino) {
                pending.swap_remove(pos);
                drop(pending);
                // Safe to delete: no more open handles, already unlinked
                let _ = self
                    .conn
                    .borrow()
                    .execute("DELETE FROM inodes WHERE ino = ?1", params![ino as i64]);
            }
        }
    }

    pub fn link(&self, ino: u64, new_parent_ino: u64, new_name: &[u8]) -> Result<Inode, DbError> {
        let inode = self.get_inode(ino)?;
        if inode.kind == FileKind::Directory {
            return Err(DbError::IsDirectory);
        }
        if self.get_inode(new_parent_ino)?.kind != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }

        let now = now_secs();
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO dirents (parent_ino, name, child_ino) VALUES (?1, ?2, ?3)",
            params![new_parent_ino as i64, new_name, ino as i64],
        )?;

        let new_nlink = inode.nlink + 1;
        tx.execute(
            "UPDATE inodes SET nlink = ?1, ctime = ?2 WHERE ino = ?3",
            params![new_nlink as i64, now, ino as i64],
        )?;
        tx.execute(
            "UPDATE inodes SET mtime = ?1, ctime = ?1 WHERE ino = ?2",
            params![now, new_parent_ino as i64],
        )?;
        tx.commit()?;

        Ok(Inode {
            nlink: new_nlink,
            ctime: now,
            ..inode
        })
    }

    pub fn read_file(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, DbError> {
        let inode = self.get_inode(ino)?;
        if inode.kind != FileKind::RegularFile {
            return Err(DbError::NotFound);
        }
        if offset >= inode.size || size == 0 {
            return Ok(Vec::new());
        }

        let read_len = size.min((inode.size - offset) as u32) as usize;
        let mut output = vec![0; read_len];
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare_cached(
            "SELECT data FROM file_chunks WHERE ino = ?1 AND chunk_index = ?2",
        )?;

        let start = offset as usize;
        let end = start + read_len;
        let start_chunk = start / CHUNK_SIZE;
        let end_chunk = (end - 1) / CHUNK_SIZE;

        for chunk_index in start_chunk..=end_chunk {
            let chunk = stmt
                .query_row(
                    params![ino as i64, chunk_index as i64],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()?;

            let Some(chunk) = chunk else {
                continue;
            };

            let chunk_start = chunk_index * CHUNK_SIZE;
            let copy_start = start.max(chunk_start);
            let copy_end = end.min(chunk_start + chunk.len());
            if copy_start >= copy_end {
                continue;
            }

            let output_start = copy_start - start;
            let chunk_copy_start = copy_start - chunk_start;
            let copy_len = copy_end - copy_start;
            output[output_start..output_start + copy_len]
                .copy_from_slice(&chunk[chunk_copy_start..chunk_copy_start + copy_len]);
        }

        Ok(output)
    }

    pub fn write_file(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32, DbError> {
        self.write_file_batch(ino, &[(offset, data.to_vec())])
    }

    pub fn write_file_batch(&self, ino: u64, writes: &[(u64, Vec<u8>)]) -> Result<u32, DbError> {
        let inode = self.get_inode(ino)?;
        if inode.kind != FileKind::RegularFile {
            return Err(DbError::NotFound);
        }
        if writes.iter().all(|(_, data)| data.is_empty()) {
            return Ok(0);
        }

        let now = now_secs();
        let mut new_size = inode.size;
        let mut written = 0usize;
        let mut chunk_patches: BTreeMap<usize, Vec<ChunkPatch<'_>>> = BTreeMap::new();

        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;

        for (offset, data) in writes {
            if data.is_empty() {
                continue;
            }

            let start = *offset as usize;
            let end = start + data.len();
            let start_chunk = start / CHUNK_SIZE;
            let end_chunk = (end - 1) / CHUNK_SIZE;

            for chunk_index in start_chunk..=end_chunk {
                let chunk_start = chunk_index * CHUNK_SIZE;
                let write_start = start.max(chunk_start);
                let write_end = end.min(chunk_start + CHUNK_SIZE);
                let chunk_write_start = write_start - chunk_start;
                let input_start = write_start - start;
                let input_end = write_end - start;
                chunk_patches
                    .entry(chunk_index)
                    .or_default()
                    .push(ChunkPatch {
                        offset: chunk_write_start,
                        data: &data[input_start..input_end],
                    });
            }

            new_size = new_size.max(*offset + data.len() as u64);
            written += data.len();
        }

        let mut select_stmt = tx.prepare_cached(
            "SELECT data FROM file_chunks WHERE ino = ?1 AND chunk_index = ?2",
        )?;
        let mut upsert_stmt = tx.prepare_cached(
            "INSERT INTO file_chunks (ino, chunk_index, data) VALUES (?1, ?2, ?3)
             ON CONFLICT (ino, chunk_index) DO UPDATE SET data = excluded.data",
        )?;

        for (chunk_index, patches) in chunk_patches {
            let existing_len = chunk_len_for_size(inode.size, chunk_index);
            let required_len = patches
                .iter()
                .map(|patch| patch.offset + patch.data.len())
                .max()
                .unwrap_or(0);
            if required_len == 0 {
                continue;
            }
            let cover_len = existing_len.max(required_len);

            let mut chunk = if existing_len == 0 || patches_cover_range(&patches, cover_len) {
                vec![0; cover_len]
            } else {
                select_stmt
                    .query_row(
                        params![ino as i64, chunk_index as i64],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .optional()?
                    .unwrap_or_default()
            };

            if chunk.len() < cover_len {
                chunk.resize(cover_len, 0);
            }

            for patch in patches {
                let end = patch.offset + patch.data.len();
                if chunk.len() < end {
                    chunk.resize(end, 0);
                }
                chunk[patch.offset..end].copy_from_slice(patch.data);
            }

            upsert_stmt.execute(params![ino as i64, chunk_index as i64, chunk])?;
        }

        drop(select_stmt);
        drop(upsert_stmt);

        tx.execute(
            "UPDATE inodes SET size = ?1, mtime = ?2, ctime = ?2 WHERE ino = ?3",
            params![new_size as i64, now, ino as i64],
        )?;
        tx.commit()?;

        Ok(written as u32)
    }

    pub fn truncate_file(&self, ino: u64, size: u64) -> Result<(), DbError> {
        let inode = self.get_inode(ino)?;
        if inode.kind != FileKind::RegularFile {
            return Err(DbError::NotFound);
        }

        let now = now_secs();
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;

        if size == 0 {
            tx.execute(
                "DELETE FROM file_chunks WHERE ino = ?1",
                params![ino as i64],
            )?;
        } else {
            let final_chunk_index = ((size - 1) as usize) / CHUNK_SIZE;
            let final_chunk_len = ((size - 1) as usize % CHUNK_SIZE) + 1;

            tx.execute(
                "DELETE FROM file_chunks WHERE ino = ?1 AND chunk_index > ?2",
                params![ino as i64, final_chunk_index as i64],
            )?;

            let final_chunk = tx
                .query_row(
                    "SELECT data FROM file_chunks WHERE ino = ?1 AND chunk_index = ?2",
                    params![ino as i64, final_chunk_index as i64],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()?;

            if let Some(mut final_chunk) = final_chunk {
                final_chunk.truncate(final_chunk_len);
                tx.execute(
                    "UPDATE file_chunks SET data = ?1 WHERE ino = ?2 AND chunk_index = ?3",
                    params![final_chunk, ino as i64, final_chunk_index as i64],
                )?;
            }
        }

        tx.execute(
            "UPDATE inodes SET size = ?1, mtime = ?2, ctime = ?2 WHERE ino = ?3",
            params![size as i64, now, ino as i64],
        )?;
        tx.commit()?;

        Ok(())
    }

    pub fn unlink_file(&self, parent_ino: u64, name: &[u8]) -> Result<(), DbError> {
        let target = self.lookup(parent_ino, name)?;
        if target.kind == FileKind::Directory {
            return Err(DbError::IsDirectory);
        }

        let now = now_secs();
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;

        // Remove the directory entry
        tx.execute(
            "DELETE FROM dirents WHERE parent_ino = ?1 AND name = ?2",
            params![parent_ino as i64, name],
        )?;

        self.drop_unlinked_inode(&tx, &target, now)?;

        tx.execute(
            "UPDATE inodes SET mtime = ?1, ctime = ?1 WHERE ino = ?2",
            params![now, parent_ino as i64],
        )?;
        tx.commit()?;

        Ok(())
    }

    pub fn remove_dir(&self, parent_ino: u64, name: &[u8]) -> Result<(), DbError> {
        let target = self.lookup(parent_ino, name)?;
        if target.kind != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }

        let now = now_secs();
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        let child_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM dirents WHERE parent_ino = ?1",
            params![target.ino as i64],
            |row| row.get(0),
        )?;
        if child_count != 0 {
            return Err(DbError::DirectoryNotEmpty);
        }

        tx.execute(
            "DELETE FROM dirents WHERE parent_ino = ?1 AND name = ?2",
            params![parent_ino as i64, name],
        )?;
        tx.execute(
            "DELETE FROM inodes WHERE ino = ?1",
            params![target.ino as i64],
        )?;
        tx.execute(
            "UPDATE inodes SET mtime = ?1, ctime = ?1 WHERE ino = ?2",
            params![now, parent_ino as i64],
        )?;
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
        let source = self.lookup(parent_ino, name)?;
        if self.get_inode(new_parent_ino)?.kind != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }
        if source.kind == FileKind::Directory && self.is_descendant(new_parent_ino, source.ino)? {
            return Err(DbError::InvalidInput);
        }

        if parent_ino == new_parent_ino && name == new_name {
            return Ok(());
        }

        let existing_target = match self.lookup(new_parent_ino, new_name) {
            Ok(target) => Some(target),
            Err(DbError::NotFound) => None,
            Err(error) => return Err(error),
        };

        if let Some(target) = &existing_target {
            match (source.kind, target.kind) {
                (FileKind::RegularFile, FileKind::Directory)
                | (FileKind::Symlink, FileKind::Directory) => return Err(DbError::IsDirectory),
                (FileKind::Directory, FileKind::RegularFile)
                | (FileKind::Directory, FileKind::Symlink) => return Err(DbError::NotDirectory),
                (FileKind::Directory, FileKind::Directory) => {
                    if !self.list_dir(target.ino)?.is_empty() {
                        return Err(DbError::DirectoryNotEmpty);
                    }
                }
                _ => {}
            }
        }

        let now = now_secs();
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;

        if let Some(target) = existing_target {
            tx.execute(
                "DELETE FROM dirents WHERE parent_ino = ?1 AND name = ?2",
                params![new_parent_ino as i64, new_name],
            )?;
            self.drop_unlinked_inode(&tx, &target, now)?;
        }

        tx.execute(
            "UPDATE dirents SET parent_ino = ?1, name = ?2 WHERE parent_ino = ?3 AND name = ?4",
            params![new_parent_ino as i64, new_name, parent_ino as i64, name],
        )?;
        tx.execute(
            "UPDATE inodes SET mtime = ?1, ctime = ?1 WHERE ino = ?2 OR ino = ?3",
            params![now, parent_ino as i64, new_parent_ino as i64],
        )?;
        tx.commit()?;

        Ok(())
    }

    fn drop_unlinked_inode(
        &self,
        tx: &rusqlite::Transaction<'_>,
        target: &Inode,
        now: i64,
    ) -> Result<(), DbError> {
        if target.kind == FileKind::Directory {
            tx.execute(
                "DELETE FROM inodes WHERE ino = ?1",
                params![target.ino as i64],
            )?;
            return Ok(());
        }

        if target.nlink > 1 {
            tx.execute(
                "UPDATE inodes SET nlink = ?1, ctime = ?2 WHERE ino = ?3",
                params![(target.nlink - 1) as i64, now, target.ino as i64],
            )?;
            return Ok(());
        }

        let open_count = self
            .open_counts
            .borrow()
            .get(&target.ino)
            .copied()
            .unwrap_or(0);
        if open_count > 0 {
            tx.execute(
                "UPDATE inodes SET nlink = 0, ctime = ?1 WHERE ino = ?2",
                params![now, target.ino as i64],
            )?;
            if !self.pending_delete.borrow().contains(&target.ino) {
                self.pending_delete.borrow_mut().push(target.ino);
            }
        } else {
            tx.execute(
                "DELETE FROM inodes WHERE ino = ?1",
                params![target.ino as i64],
            )?;
        }

        Ok(())
    }

    pub fn update_metadata(&self, ino: u64, update: MetadataUpdate) -> Result<Inode, DbError> {
        let inode = self.get_inode(ino)?;
        let now = now_secs();
        let mode = update.mode.unwrap_or(inode.mode);
        let uid = update.uid.unwrap_or(inode.uid);
        let gid = update.gid.unwrap_or(inode.gid);
        let atime = update.atime.unwrap_or(inode.atime);
        let mtime = update.mtime.unwrap_or(inode.mtime);

        self.conn.borrow().execute(
            "UPDATE inodes
             SET mode = ?1, uid = ?2, gid = ?3, atime = ?4, mtime = ?5, ctime = ?6
             WHERE ino = ?7",
            params![
                mode as i64,
                uid as i64,
                gid as i64,
                atime,
                mtime,
                now,
                ino as i64
            ],
        )?;

        self.get_inode(ino)
    }

    fn create_node(
        &self,
        parent_ino: u64,
        name: &[u8],
        kind: FileKind,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Inode, DbError> {
        if self.get_inode(parent_ino)?.kind != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }

        let now = now_secs();
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO inodes (kind, mode, uid, gid, size, atime, mtime, ctime, nlink)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, ?5, 1)",
            params![kind.as_i64(), mode, uid, gid, now],
        )?;
        let ino = tx.last_insert_rowid() as u64;

        if let Err(error) = tx.execute(
            "INSERT INTO dirents (parent_ino, name, child_ino) VALUES (?1, ?2, ?3)",
            params![parent_ino as i64, name, ino as i64],
        ) {
            return Err(error.into());
        }

        tx.execute(
            "UPDATE inodes SET mtime = ?1, ctime = ?1 WHERE ino = ?2",
            params![now, parent_ino as i64],
        )?;
        tx.commit()?;

        Ok(Inode {
            ino,
            kind,
            mode,
            uid,
            gid,
            size: 0,
            atime: now,
            mtime: now,
            ctime: now,
            nlink: 1,
        })
    }

    fn is_descendant(&self, ino: u64, possible_ancestor: u64) -> Result<bool, DbError> {
        if ino == possible_ancestor {
            return Ok(true);
        }

        let conn = self.conn.borrow();
        let mut current = ino;
        while current != 1 {
            let parent = conn
                .query_row(
                    "SELECT parent_ino FROM dirents WHERE child_ino = ?1",
                    params![current as i64],
                    |row| row.get::<_, i64>(0),
                )
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

fn pragma_query(name: &str) -> Result<String, DbError> {
    match name {
        "busy_timeout" | "foreign_keys" | "journal_mode" | "synchronous" => {
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
        atime: row.get(6)?,
        mtime: row.get(7)?,
        ctime: row.get(8)?,
        nlink: row.get::<_, i64>(9)? as u32,
    })
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_secs() as i64
}

fn chunk_len_for_size(size: u64, chunk_index: usize) -> usize {
    if size == 0 {
        return 0;
    }
    let last_chunk_index = ((size - 1) as usize) / CHUNK_SIZE;
    if chunk_index < last_chunk_index {
        CHUNK_SIZE
    } else if chunk_index == last_chunk_index {
        ((size - 1) as usize % CHUNK_SIZE) + 1
    } else {
        0
    }
}

fn patches_cover_range(patches: &[ChunkPatch<'_>], cover_len: usize) -> bool {
    if cover_len == 0 {
        return true;
    }
    let mut spans: Vec<(usize, usize)> = patches
        .iter()
        .map(|patch| (patch.offset, patch.offset + patch.data.len()))
        .collect();
    spans.sort_by_key(|(start, _)| *start);

    let mut covered_end = 0usize;
    for (start, end) in spans {
        if start > covered_end {
            return false;
        }
        if end > covered_end {
            covered_end = end;
        }
        if covered_end >= cover_len {
            return true;
        }
    }
    covered_end >= cover_len
}
