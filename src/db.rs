use std::cell::RefCell;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, Error as SqlError, OptionalExtension, params};

use crate::{DbError, DirEntry, FileKind, Inode, MetadataUpdate};

const CHUNK_SIZE: usize = 65_536;

pub struct Db {
    conn: RefCell<Connection>,
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
            ",
        )?;

        let now = now_secs();
        conn.execute(
            "INSERT OR IGNORE INTO inodes
             (ino, kind, mode, uid, gid, size, atime, mtime, ctime, nlink)
             VALUES (1, ?1, ?2, 0, 0, 0, ?3, ?3, ?3, 1)",
            params![FileKind::Directory.as_i64(), 0o755_i64, now],
        )?;

        Ok(Self {
            conn: RefCell::new(conn),
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

        let start = offset as usize;
        let end = start + read_len;
        let start_chunk = start / CHUNK_SIZE;
        let end_chunk = (end - 1) / CHUNK_SIZE;

        for chunk_index in start_chunk..=end_chunk {
            let chunk = conn
                .query_row(
                    "SELECT data FROM file_chunks WHERE ino = ?1 AND chunk_index = ?2",
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
        let inode = self.get_inode(ino)?;
        if inode.kind != FileKind::RegularFile {
            return Err(DbError::NotFound);
        }
        if data.is_empty() {
            return Ok(0);
        }

        let now = now_secs();
        let start = offset as usize;
        let end = start + data.len();
        let start_chunk = start / CHUNK_SIZE;
        let end_chunk = (end - 1) / CHUNK_SIZE;

        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;

        for chunk_index in start_chunk..=end_chunk {
            let chunk_start = chunk_index * CHUNK_SIZE;
            let write_start = start.max(chunk_start);
            let write_end = end.min(chunk_start + CHUNK_SIZE);
            let chunk_write_start = write_start - chunk_start;
            let input_start = write_start - start;
            let input_end = write_end - start;

            let mut chunk = tx
                .query_row(
                    "SELECT data FROM file_chunks WHERE ino = ?1 AND chunk_index = ?2",
                    params![ino as i64, chunk_index as i64],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()?
                .unwrap_or_default();

            let required_len = chunk_write_start + (input_end - input_start);
            if chunk.len() < required_len {
                chunk.resize(required_len, 0);
            }

            chunk[chunk_write_start..required_len].copy_from_slice(&data[input_start..input_end]);

            tx.execute(
                "INSERT INTO file_chunks (ino, chunk_index, data) VALUES (?1, ?2, ?3)
                 ON CONFLICT (ino, chunk_index) DO UPDATE SET data = excluded.data",
                params![ino as i64, chunk_index as i64, chunk],
            )?;
        }

        let new_size = inode.size.max(offset + data.len() as u64);
        tx.execute(
            "UPDATE inodes SET size = ?1, mtime = ?2, ctime = ?2 WHERE ino = ?3",
            params![new_size as i64, now, ino as i64],
        )?;
        tx.commit()?;

        Ok(data.len() as u32)
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
                (FileKind::RegularFile, FileKind::Directory) => return Err(DbError::IsDirectory),
                (FileKind::Directory, FileKind::RegularFile) => return Err(DbError::NotDirectory),
                (FileKind::Directory, FileKind::Directory) => {
                    if !self.list_dir(target.ino)?.is_empty() {
                        return Err(DbError::DirectoryNotEmpty);
                    }
                }
                (FileKind::RegularFile, FileKind::RegularFile) => {}
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
            tx.execute(
                "DELETE FROM inodes WHERE ino = ?1",
                params![target.ino as i64],
            )?;
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
