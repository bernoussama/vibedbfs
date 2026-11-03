use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Error as SqlError, ErrorCode, ffi};

/// Seconds and nanoseconds since the Unix epoch.
///
/// Negative `secs` follows fuser's split. The duration before the epoch is
/// `secs.unsigned_abs()` seconds plus `nsec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    pub secs: i64,
    pub nsec: u32,
}

impl Timestamp {
    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => Self {
                secs: duration.as_secs() as i64,
                nsec: duration.subsec_nanos(),
            },
            Err(error) => Self {
                secs: -(error.duration().as_secs() as i64),
                nsec: error.duration().subsec_nanos(),
            },
        }
    }

    pub fn to_system_time(self) -> SystemTime {
        if self.secs >= 0 {
            UNIX_EPOCH + Duration::new(self.secs as u64, self.nsec)
        } else {
            UNIX_EPOCH - Duration::new(self.secs.unsigned_abs(), self.nsec)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    RegularFile,
    Directory,
    Symlink,
}

impl FileKind {
    pub(crate) fn as_i64(self) -> i64 {
        match self {
            Self::RegularFile => 1,
            Self::Directory => 2,
            Self::Symlink => 3,
        }
    }

    pub(crate) fn from_i64(value: i64) -> Result<Self, DbError> {
        match value {
            1 => Ok(Self::RegularFile),
            2 => Ok(Self::Directory),
            3 => Ok(Self::Symlink),
            _ => Err(DbError::Corrupt),
        }
    }

    pub(crate) fn regular_file_error(self) -> Option<DbError> {
        match self {
            Self::RegularFile => None,
            Self::Directory => Some(DbError::IsDirectory),
            Self::Symlink => Some(DbError::InvalidInput),
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
    pub atime: Timestamp,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
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
    pub atime: Option<Timestamp>,
    pub mtime: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    AlreadyExists,
    DirectoryNotEmpty,
    InvalidInput,
    IsDirectory,
    NameTooLong,
    NoSpace,
    NotDirectory,
    NotFound,
    Corrupt,
    Sqlite(String),
}

impl From<SqlError> for DbError {
    fn from(error: SqlError) -> Self {
        match error {
            SqlError::QueryReturnedNoRows => Self::NotFound,
            SqlError::SqliteFailure(err, message) => sqlite_failure(err, message),
            other => Self::Sqlite(other.to_string()),
        }
    }
}

fn sqlite_failure(err: ffi::Error, message: Option<String>) -> DbError {
    if err.code == ErrorCode::DiskFull || err.extended_code == ffi::SQLITE_FULL {
        return DbError::NoSpace;
    }
    if err.code == ErrorCode::ConstraintViolation
        && matches!(
            err.extended_code,
            ffi::SQLITE_CONSTRAINT_PRIMARYKEY | ffi::SQLITE_CONSTRAINT_UNIQUE
        )
    {
        return DbError::AlreadyExists;
    }
    DbError::Sqlite(SqlError::SqliteFailure(err, message).to_string())
}
