use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, Error as SqlError, OptionalExtension, params};

use crate::{DbError, DirEntry, FileKind, Inode, MetadataUpdate};

const CHUNK_SIZE: usize = 65_536;

const CACHE_TTL: Duration = Duration::from_secs(5);
const INODE_CACHE_MAX: usize = 16_384;
const LOOKUP_CACHE_MAX: usize = 16_384;

struct InodeCache {
    entries: HashMap<u64, (Instant, Inode)>,
}

impl InodeCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn get(&self, ino: u64) -> Option<Inode> {
        let (cached_at, inode) = self.entries.get(&ino)?;
        if cached_at.elapsed() < CACHE_TTL {
            Some(inode.clone())
        } else {
            None
        }
    }

    fn put(&mut self, inode: Inode) {
        if self.entries.len() >= INODE_CACHE_MAX && !self.entries.contains_key(&inode.ino) {
            self.evict();
        }
        self.entries.insert(inode.ino, (Instant::now(), inode));
    }

    fn evict(&mut self) {
        let evict_count = self.entries.len() / 4;
        let mut by_age: Vec<(u64, Instant)> = self
            .entries
            .iter()
            .map(|(&ino, &(ts, _))| (ino, ts))
            .collect();
        by_age.sort_unstable_by_key(|&(_, ts)| ts);
        for &(ino, _) in by_age.iter().take(evict_count.max(1)) {
            self.entries.remove(&ino);
        }
    }

    fn invalidate(&mut self, ino: u64) {
        self.entries.remove(&ino);
    }
}

struct LookupCache {
    entries: HashMap<u64, HashMap<Vec<u8>, (Instant, u64)>>,
    total: usize,
}

impl LookupCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            total: 0,
        }
    }

    fn get(&self, parent_ino: u64, name: &[u8]) -> Option<u64> {
        let parent = self.entries.get(&parent_ino)?;
        let (cached_at, ino) = parent.get(name)?;
        if cached_at.elapsed() < CACHE_TTL {
            Some(*ino)
        } else {
            None
        }
    }

    fn put(&mut self, parent_ino: u64, name: &[u8], ino: u64) {
        if self.total >= LOOKUP_CACHE_MAX {
            self.evict();
        }
        let parent = self.entries.entry(parent_ino).or_default();
        if !parent.contains_key(name) {
            self.total += 1;
        }
        parent.insert(name.to_vec(), (Instant::now(), ino));
    }

    fn evict(&mut self) {
        let evict_count = (self.total / 4).max(1);
        let mut by_age: Vec<(u64, Vec<u8>, Instant)> = self
            .entries
            .iter()
            .flat_map(|(&pino, children)| {
                children
                    .iter()
                    .map(move |(name, &(ts, _))| (pino, name.clone(), ts))
            })
            .collect();
        by_age.sort_unstable_by_key(|&(_, _, ts)| ts);
        for (pino, name, _) in by_age.into_iter().take(evict_count) {
            if let Some(parent) = self.entries.get_mut(&pino) {
                if parent.remove(&name).is_some() {
                    self.total = self.total.saturating_sub(1);
                }
                if parent.is_empty() {
                    self.entries.remove(&pino);
                }
            }
        }
    }

    fn invalidate_parent(&mut self, parent_ino: u64) {
        if let Some(removed) = self.entries.remove(&parent_ino) {
            self.total = self.total.saturating_sub(removed.len());
        }
    }

    fn invalidate_entry(&mut self, parent_ino: u64, name: &[u8]) {
        if let Some(parent) = self.entries.get_mut(&parent_ino) {
            if parent.remove(name).is_some() {
                self.total = self.total.saturating_sub(1);
            }
        }
    }
}

pub struct Db {
    conn: Mutex<Connection>,
    /// Tracks how many open file handles reference each inode.
    /// When unlink removes the last directory entry but the inode is still
    /// open (open_count > 0), we defer deletion until the file is released.
    open_counts: Mutex<HashMap<u64, u64>>,
    /// Inodes pending deletion once all handles are closed.
    pending_delete: Mutex<Vec<u64>>,
    /// Cached inode kinds for open files (set on open, cleared on release).
    /// Avoids redundant get_inode calls on every write/read.
    inode_kinds: Mutex<HashMap<u64, FileKind>>,
    inode_cache: Mutex<InodeCache>,
    lookup_cache: Mutex<LookupCache>,
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
            PRAGMA mmap_size = 268435456;
            PRAGMA cache_size = -65536;
            PRAGMA temp_store = MEMORY;
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
            .lock()
            .unwrap()
            .query_row(&sql, [], |row| row.get(0))
            .map_err(Into::into)
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
            conn: Mutex::new(conn),
            open_counts: Mutex::new(HashMap::new()),
            pending_delete: Mutex::new(Vec::new()),
            inode_kinds: Mutex::new(HashMap::new()),
            inode_cache: Mutex::new(InodeCache::new()),
            lookup_cache: Mutex::new(LookupCache::new()),
        })
    }

    pub fn get_inode(&self, ino: u64) -> Result<Inode, DbError> {
        if let Some(inode) = self.inode_cache.lock().unwrap().get(ino) {
            return Ok(inode);
        }

        let inode = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare_cached(
                "SELECT ino, kind, mode, uid, gid, size, atime, mtime, ctime, nlink
                 FROM inodes
                 WHERE ino = ?1",
            )?;
            stmt.query_row(params![ino as i64], inode_from_row)?
        };

        self.inode_cache.lock().unwrap().put(inode.clone());
        Ok(inode)
    }

    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<Inode, DbError> {
        if let Some(ino) = self.lookup_cache.lock().unwrap().get(parent_ino, name) {
            if let Some(inode) = self.inode_cache.lock().unwrap().get(ino) {
                return Ok(inode);
            }
        }

        let inode = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare_cached(
                "SELECT i.ino, i.kind, i.mode, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime, i.nlink
                 FROM dirents d
                 JOIN inodes i ON i.ino = d.child_ino
                 WHERE d.parent_ino = ?1 AND d.name = ?2",
            )?;
            stmt.query_row(params![parent_ino as i64, name], inode_from_row)?
        };

        self.inode_cache.lock().unwrap().put(inode.clone());
        self.lookup_cache
            .lock()
            .unwrap()
            .put(parent_ino, name, inode.ino);
        Ok(inode)
    }

    pub fn list_dir(&self, ino: u64) -> Result<Vec<DirEntry>, DbError> {
        if self.get_inode(ino)?.kind != FileKind::Directory {
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

    /// Like `list_dir` but skips the first `offset` rows in SQL.
    pub fn list_dir_offset(&self, ino: u64, offset: u64) -> Result<Vec<DirEntry>, DbError> {
        if self.get_inode(ino)?.kind != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT d.name, i.ino, i.kind
             FROM dirents d
             JOIN inodes i ON i.ino = d.child_ino
             WHERE d.parent_ino = ?1
             ORDER BY d.name
             LIMIT -1 OFFSET ?2",
        )?;

        let entries = stmt
            .query_map(params![ino as i64, offset as i64], |row| {
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

        {
            let conn = self.conn.lock().unwrap();
            conn.prepare_cached(
                "INSERT INTO symlink_targets (ino, target) VALUES (?1, ?2)",
            )?
            .execute(params![inode.ino as i64, target])?;

            conn.prepare_cached("UPDATE inodes SET size = ?1 WHERE ino = ?2")?
                .execute(params![target.len() as i64, inode.ino as i64])?;
        }

        self.inode_cache.lock().unwrap().invalidate(inode.ino);

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

        let conn = self.conn.lock().unwrap();
        conn.prepare_cached("SELECT target FROM symlink_targets WHERE ino = ?1")?
            .query_row(params![ino as i64], |row| row.get(0))
            .map_err(Into::into)
    }

    /// Track that a file handle has been opened for this inode.
    pub fn open_file(&self, ino: u64, kind: FileKind) {
        *self.open_counts.lock().unwrap().entry(ino).or_insert(0) += 1;
        self.inode_kinds.lock().unwrap().insert(ino, kind);
    }

    /// Return the cached inode kind for an open file, if available.
    pub fn inode_kind(&self, ino: u64) -> Option<FileKind> {
        self.inode_kinds.lock().unwrap().get(&ino).copied()
    }

    /// Release a file handle. If this was the last handle and the inode
    /// is pending deletion, delete it now.
    pub fn release_file(&self, ino: u64) {
        let mut open_counts = self.open_counts.lock().unwrap();
        let count = open_counts.entry(ino).or_insert(0);
        if *count > 0 {
            *count -= 1;
        }

        if *count == 0 {
            open_counts.remove(&ino);
            self.inode_kinds.lock().unwrap().remove(&ino);
            drop(open_counts);

            let mut pending = self.pending_delete.lock().unwrap();
            if let Some(pos) = pending.iter().position(|&p| p == ino) {
                pending.swap_remove(pos);
                drop(pending);
                let _ = self
                    .conn
                    .lock()
                    .unwrap()
                    .prepare_cached("DELETE FROM inodes WHERE ino = ?1")
                    .and_then(|mut s| s.execute(params![ino as i64]));
                self.inode_cache.lock().unwrap().invalidate(ino);
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
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        tx.prepare_cached(
            "INSERT INTO dirents (parent_ino, name, child_ino) VALUES (?1, ?2, ?3)",
        )?
        .execute(params![new_parent_ino as i64, new_name, ino as i64])?;

        let new_nlink = inode.nlink + 1;
        tx.prepare_cached("UPDATE inodes SET nlink = ?1, ctime = ?2 WHERE ino = ?3")?
            .execute(params![new_nlink as i64, now, ino as i64])?;
        tx.prepare_cached("UPDATE inodes SET mtime = ?1, ctime = ?1 WHERE ino = ?2")?
            .execute(params![now, new_parent_ino as i64])?;
        tx.commit()?;
        drop(conn);

        {
            let mut icache = self.inode_cache.lock().unwrap();
            icache.invalidate(ino);
            icache.invalidate(new_parent_ino);
        }

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
        let conn = self.conn.lock().unwrap();

        let start = offset as usize;
        let end = start + read_len;
        let start_chunk = start / CHUNK_SIZE;
        let end_chunk = (end - 1) / CHUNK_SIZE;

        let mut stmt = conn.prepare_cached(
            "SELECT data FROM file_chunks WHERE ino = ?1 AND chunk_index = ?2",
        )?;

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

        let coalesced = coalesce_writes(writes);

        let now = now_secs();
        let mut new_size = inode.size;
        let mut written = 0usize;

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

            for (offset, data) in &coalesced {
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
                    let write_len = input_end - input_start;

                    let is_full_chunk = chunk_write_start == 0 && write_len == CHUNK_SIZE;

                    let chunk_data: Vec<u8> = if is_full_chunk {
                        data[input_start..input_end].to_vec()
                    } else {
                        let mut chunk = read_stmt
                            .query_row(
                                params![ino as i64, chunk_index as i64],
                                |row| row.get::<_, Vec<u8>>(0),
                            )
                            .optional()?
                            .unwrap_or_default();

                        let required_len = chunk_write_start + write_len;
                        if chunk.len() < required_len {
                            chunk.resize(required_len, 0);
                        }

                        chunk[chunk_write_start..required_len]
                            .copy_from_slice(&data[input_start..input_end]);
                        chunk
                    };

                    write_stmt
                        .execute(params![ino as i64, chunk_index as i64, chunk_data])?;
                }

                new_size = new_size.max(*offset + data.len() as u64);
                written += data.len();
            }
        }

        tx.prepare_cached(
            "UPDATE inodes SET size = ?1, mtime = ?2, ctime = ?2 WHERE ino = ?3",
        )?
        .execute(params![new_size as i64, now, ino as i64])?;
        tx.commit()?;
        drop(conn);

        self.inode_cache.lock().unwrap().invalidate(ino);

        Ok(written as u32)
    }

    pub fn truncate_file(&self, ino: u64, size: u64) -> Result<(), DbError> {
        let inode = self.get_inode(ino)?;
        if inode.kind != FileKind::RegularFile {
            return Err(DbError::NotFound);
        }

        let now = now_secs();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        if size == 0 {
            tx.prepare_cached("DELETE FROM file_chunks WHERE ino = ?1")?
                .execute(params![ino as i64])?;
        } else {
            let final_chunk_index = ((size - 1) as usize) / CHUNK_SIZE;
            let final_chunk_len = ((size - 1) as usize % CHUNK_SIZE) + 1;

            tx.prepare_cached(
                "DELETE FROM file_chunks WHERE ino = ?1 AND chunk_index > ?2",
            )?
            .execute(params![ino as i64, final_chunk_index as i64])?;

            let final_chunk = tx
                .prepare_cached(
                    "SELECT data FROM file_chunks WHERE ino = ?1 AND chunk_index = ?2",
                )?
                .query_row(
                    params![ino as i64, final_chunk_index as i64],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()?;

            if let Some(mut final_chunk) = final_chunk {
                final_chunk.truncate(final_chunk_len);
                tx.prepare_cached(
                    "UPDATE file_chunks SET data = ?1 WHERE ino = ?2 AND chunk_index = ?3",
                )?
                .execute(params![
                    final_chunk,
                    ino as i64,
                    final_chunk_index as i64
                ])?;
            }
        }

        tx.prepare_cached(
            "UPDATE inodes SET size = ?1, mtime = ?2, ctime = ?2 WHERE ino = ?3",
        )?
        .execute(params![size as i64, now, ino as i64])?;
        tx.commit()?;
        drop(conn);

        self.inode_cache.lock().unwrap().invalidate(ino);

        Ok(())
    }

    pub fn unlink_file(&self, parent_ino: u64, name: &[u8]) -> Result<(), DbError> {
        let target = self.lookup(parent_ino, name)?;
        self.unlink_inode(parent_ino, name, &target)
    }

    /// Unlink a file whose inode has already been resolved (avoids redundant lookup).
    pub fn unlink_inode(
        &self,
        parent_ino: u64,
        name: &[u8],
        target: &Inode,
    ) -> Result<(), DbError> {
        if target.kind == FileKind::Directory {
            return Err(DbError::IsDirectory);
        }

        let now = now_secs();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        tx.prepare_cached("DELETE FROM dirents WHERE parent_ino = ?1 AND name = ?2")?
            .execute(params![parent_ino as i64, name])?;

        self.drop_unlinked_inode(&tx, target, now)?;

        tx.prepare_cached("UPDATE inodes SET mtime = ?1, ctime = ?1 WHERE ino = ?2")?
            .execute(params![now, parent_ino as i64])?;
        tx.commit()?;
        drop(conn);

        {
            let mut icache = self.inode_cache.lock().unwrap();
            icache.invalidate(target.ino);
            icache.invalidate(parent_ino);
        }
        self.lookup_cache
            .lock()
            .unwrap()
            .invalidate_entry(parent_ino, name);

        Ok(())
    }

    pub fn remove_dir(&self, parent_ino: u64, name: &[u8]) -> Result<(), DbError> {
        let target = self.lookup(parent_ino, name)?;
        if target.kind != FileKind::Directory {
            return Err(DbError::NotDirectory);
        }

        let now = now_secs();
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
        tx.prepare_cached("UPDATE inodes SET mtime = ?1, ctime = ?1 WHERE ino = ?2")?
            .execute(params![now, parent_ino as i64])?;
        tx.commit()?;
        drop(conn);

        {
            let mut icache = self.inode_cache.lock().unwrap();
            icache.invalidate(target.ino);
            icache.invalidate(parent_ino);
        }
        {
            let mut lcache = self.lookup_cache.lock().unwrap();
            lcache.invalidate_entry(parent_ino, name);
            lcache.invalidate_parent(target.ino);
        }

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
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        if let Some(target) = &existing_target {
            tx.prepare_cached("DELETE FROM dirents WHERE parent_ino = ?1 AND name = ?2")?
                .execute(params![new_parent_ino as i64, new_name])?;
            self.drop_unlinked_inode(&tx, target, now)?;
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
            "UPDATE inodes SET mtime = ?1, ctime = ?1 WHERE ino = ?2 OR ino = ?3",
        )?
        .execute(params![now, parent_ino as i64, new_parent_ino as i64])?;
        tx.commit()?;
        drop(conn);

        {
            let mut icache = self.inode_cache.lock().unwrap();
            icache.invalidate(source.ino);
            icache.invalidate(parent_ino);
            icache.invalidate(new_parent_ino);
            if let Some(target) = &existing_target {
                icache.invalidate(target.ino);
            }
        }
        {
            let mut lcache = self.lookup_cache.lock().unwrap();
            lcache.invalidate_entry(parent_ino, name);
            lcache.invalidate_entry(new_parent_ino, new_name);
        }

        Ok(())
    }

    fn drop_unlinked_inode(
        &self,
        tx: &rusqlite::Transaction<'_>,
        target: &Inode,
        now: i64,
    ) -> Result<(), DbError> {
        if target.kind == FileKind::Directory {
            tx.prepare_cached("DELETE FROM inodes WHERE ino = ?1")?
                .execute(params![target.ino as i64])?;
            return Ok(());
        }

        if target.nlink > 1 {
            tx.prepare_cached("UPDATE inodes SET nlink = ?1, ctime = ?2 WHERE ino = ?3")?
                .execute(params![(target.nlink - 1) as i64, now, target.ino as i64])?;
            return Ok(());
        }

        let open_count = self
            .open_counts
            .lock()
            .unwrap()
            .get(&target.ino)
            .copied()
            .unwrap_or(0);
        if open_count > 0 {
            tx.prepare_cached("UPDATE inodes SET nlink = 0, ctime = ?1 WHERE ino = ?2")?
                .execute(params![now, target.ino as i64])?;
            let mut pending = self.pending_delete.lock().unwrap();
            if !pending.contains(&target.ino) {
                pending.push(target.ino);
            }
        } else {
            tx.prepare_cached("DELETE FROM inodes WHERE ino = ?1")?
                .execute(params![target.ino as i64])?;
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

        self.conn
            .lock()
            .unwrap()
            .prepare_cached(
                "UPDATE inodes
             SET mode = ?1, uid = ?2, gid = ?3, atime = ?4, mtime = ?5, ctime = ?6
             WHERE ino = ?7",
            )?
            .execute(params![
                mode as i64,
                uid as i64,
                gid as i64,
                atime,
                mtime,
                now,
                ino as i64
            ])?;

        self.inode_cache.lock().unwrap().invalidate(ino);

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
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        tx.prepare_cached(
            "INSERT INTO inodes (kind, mode, uid, gid, size, atime, mtime, ctime, nlink)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, ?5, 1)",
        )?
        .execute(params![kind.as_i64(), mode, uid, gid, now])?;
        let ino = tx.last_insert_rowid() as u64;

        if let Err(error) = tx
            .prepare_cached(
                "INSERT INTO dirents (parent_ino, name, child_ino) VALUES (?1, ?2, ?3)",
            )
            .and_then(|mut s| s.execute(params![parent_ino as i64, name, ino as i64]))
        {
            return Err(error.into());
        }

        tx.prepare_cached("UPDATE inodes SET mtime = ?1, ctime = ?1 WHERE ino = ?2")?
            .execute(params![now, parent_ino as i64])?;
        tx.commit()?;
        drop(conn);

        self.inode_cache.lock().unwrap().invalidate(parent_ino);

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

fn pragma_query(name: &str) -> Result<String, DbError> {
    match name {
        "busy_timeout" | "cache_size" | "foreign_keys" | "journal_mode" | "mmap_size"
        | "synchronous" | "temp_store" => Ok(format!("PRAGMA {name}")),
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

/// Merge overlapping and adjacent writes into non-overlapping spans.
///
/// Later writes in the input overwrite earlier ones where they overlap,
/// preserving correct write-ordering semantics. The returned vec is
/// sorted by offset with no overlaps.
fn coalesce_writes(writes: &[(u64, Vec<u8>)]) -> Vec<(u64, Vec<u8>)> {
    if writes.len() <= 1 {
        return writes.to_vec();
    }

    let mut result: Vec<(u64, Vec<u8>)> = Vec::new();

    for (offset, data) in writes {
        if data.is_empty() {
            continue;
        }

        let end = *offset + data.len() as u64;
        let mut next = Vec::with_capacity(result.len() + 1);

        for (span_offset, span_data) in result {
            let span_end = span_offset + span_data.len() as u64;

            if span_end <= *offset || span_offset >= end {
                next.push((span_offset, span_data));
                continue;
            }

            if span_offset < *offset {
                let left_len = (*offset - span_offset) as usize;
                next.push((span_offset, span_data[..left_len].to_vec()));
            }

            if span_end > end {
                let right_start = (end - span_offset) as usize;
                next.push((end, span_data[right_start..].to_vec()));
            }
        }

        next.push((*offset, data.to_vec()));
        result = next;
    }

    result.sort_by_key(|(offset, _)| *offset);

    let mut merged: Vec<(u64, Vec<u8>)> = Vec::with_capacity(result.len());
    for (offset, data) in result {
        if let Some((last_offset, last_data)) = merged.last_mut() {
            let last_end = *last_offset + last_data.len() as u64;
            if offset == last_end {
                last_data.extend_from_slice(&data);
                continue;
            }
        }

        merged.push((offset, data));
    }

    merged
}
