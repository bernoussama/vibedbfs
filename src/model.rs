use rusqlite::{Error as SqlError, ErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    RegularFile,
    Directory,
}

impl FileKind {
    pub(crate) fn as_i64(self) -> i64 {
        match self {
            Self::RegularFile => 1,
            Self::Directory => 2,
        }
    }

    pub(crate) fn from_i64(value: i64) -> Result<Self, DbError> {
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataUpdate {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub atime: Option<i64>,
    pub mtime: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    AlreadyExists,
    DirectoryNotEmpty,
    InvalidInput,
    IsDirectory,
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
