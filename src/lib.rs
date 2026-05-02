use std::cell::RefCell;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, Error as SqlError, ErrorCode, params};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    RegularFile,
    Directory,
}

impl FileKind {
    fn as_i64(self) -> i64 {
        match self {
            Self::RegularFile => 1,
            Self::Directory => 2,
        }
    }

    fn from_i64(value: i64) -> Result<Self, DbError> {
        match value {
            1 => Ok(Self::RegularFile),
            2 => Ok(Self::Directory),
            _ => Err(DbError::Corrupt),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inode {
    pub ino: u64,
    pub kind: FileKind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub nlink: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: Vec<u8>,
    pub ino: u64,
    pub kind: FileKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    AlreadyExists,
    NotDirectory,
    NotFound,
    Corrupt,
    Sqlite(String),
}

impl From<SqlError> for DbError {
    fn from(error: SqlError) -> Self {
        match error {
            SqlError::QueryReturnedNoRows => Self::NotFound,
            SqlError::SqliteFailure(err, _) if err.code == ErrorCode::ConstraintViolation => {
                Self::AlreadyExists
            }
            other => Self::Sqlite(other.to_string()),
        }
    }
}

pub struct Db {
    conn: RefCell<Connection>,
}

impl Db {
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;

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
