use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Error as SqlError, ErrorCode, ffi};

/// Seconds and nanoseconds since the Unix epoch, floored: the instant is
/// `secs + nsec / 1e9` with `0 <= nsec < 1e9`. So -0.5 s is
/// `{ secs: -1, nsec: 500_000_000 }`, the same split as the kernel's `timespec64`.
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
            Err(error) => {
                // Floor of -(whole + frac): -(whole + 1) plus (1e9 - frac) nanoseconds.
                let before = error.duration();
                let whole = before.as_secs() as i128;
                let frac = before.subsec_nanos();
                let (secs, nsec) = if frac == 0 {
                    (-whole, 0)
                } else {
                    (-whole - 1, 1_000_000_000 - frac)
                };
                Self {
                    secs: secs.max(i64::MIN as i128) as i64,
                    nsec,
                }
            }
        }
    }

    /// Nanoseconds since the epoch, as stored on disk. Times outside the i64
    /// range (years 1677 to 2262) are clamped to its ends, as ext4 clamps to its
    /// own range, so `utimensat` with an odd time never fails.
    pub fn to_ns(self) -> i64 {
        let ns = self.secs as i128 * 1_000_000_000 + self.nsec as i128;
        ns.clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }

    /// Inverse of [`Timestamp::to_ns`]. Floors, so `nsec` is never negative.
    pub fn from_ns(ns: i64) -> Self {
        Self {
            secs: ns.div_euclid(1_000_000_000),
            nsec: ns.rem_euclid(1_000_000_000) as u32,
        }
    }

    pub fn to_system_time(self) -> SystemTime {
        let ns = self.secs as i128 * 1_000_000_000 + self.nsec as i128;
        let magnitude = ns.unsigned_abs();
        let duration = Duration::new(
            (magnitude / 1_000_000_000) as u64,
            (magnitude % 1_000_000_000) as u32,
        );
        if ns >= 0 {
            UNIX_EPOCH + duration
        } else {
            UNIX_EPOCH - duration
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
    /// The database was written by a newer dbfs with this schema version.
    UnsupportedSchema(i64),
    /// The file would grow past the 1 TiB limit (`EFBIG`).
    FileTooLarge,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_just_before_the_epoch_keep_their_sign() {
        let cases = [
            (
                UNIX_EPOCH - Duration::from_nanos(1),
                Timestamp {
                    secs: -1,
                    nsec: 999_999_999,
                },
                -1,
            ),
            (
                UNIX_EPOCH - Duration::from_millis(500),
                Timestamp {
                    secs: -1,
                    nsec: 500_000_000,
                },
                -500_000_000,
            ),
            (
                UNIX_EPOCH - Duration::from_secs(1),
                Timestamp { secs: -1, nsec: 0 },
                -1_000_000_000,
            ),
            (
                UNIX_EPOCH - Duration::from_millis(1500),
                Timestamp {
                    secs: -2,
                    nsec: 500_000_000,
                },
                -1_500_000_000,
            ),
            (UNIX_EPOCH, Timestamp { secs: 0, nsec: 0 }, 0),
        ];
        for (time, stamp, ns) in cases {
            assert_eq!(Timestamp::from_system_time(time), stamp, "{time:?}");
            assert_eq!(stamp.to_system_time(), time, "{stamp:?}");
            assert_eq!(stamp.to_ns(), ns, "{stamp:?}");
            assert_eq!(Timestamp::from_ns(ns), stamp, "{ns} ns");
        }
    }

    #[test]
    fn ns_range_ends_clamp_and_round_trip() {
        assert_eq!(
            Timestamp {
                secs: i64::MIN,
                nsec: 0
            }
            .to_ns(),
            i64::MIN
        );
        assert_eq!(
            Timestamp {
                secs: i64::MAX,
                nsec: 999_999_999
            }
            .to_ns(),
            i64::MAX
        );
        assert_eq!(Timestamp::from_ns(i64::MIN).to_ns(), i64::MIN);
        assert_eq!(Timestamp::from_ns(i64::MAX).to_ns(), i64::MAX);
        assert_eq!(Timestamp::from_ns(i64::MAX).secs, 9_223_372_036);
    }

    #[test]
    fn times_far_before_the_epoch_neither_overflow_nor_wrap() {
        let far = UNIX_EPOCH - Duration::new(1 << 62, 500_000_000);
        let stamp = Timestamp::from_system_time(far);
        assert_eq!(stamp.secs, -(1 << 62) - 1);
        assert_eq!(stamp.nsec, 500_000_000);
        assert_eq!(stamp.to_system_time(), far);
        assert_eq!(stamp.to_ns(), i64::MIN);
    }
}
