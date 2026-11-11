//! `dbfs migrate --page-size N <DATABASE>`: rewrites a database with another
//! SQLite page size.
//!
//! SQLite fixes a database's page size when the file is first written, so the only
//! way to change it is to copy the database. The copy is made with `VACUUM INTO`
//! into `<DATABASE>.migrate`, beside the original. It is checked and synced, the
//! original's WAL files are removed, the copy is renamed over the original, and the
//! directory is synced. The original is not changed before the rename, so an
//! interrupted run leaves it as it was. The copy keeps every table, row and the
//! `user_version`; only the page layout changes.
//!
//! The migration takes the same lock as a mount, so it refuses a database that is
//! mounted or being migrated. Its checkpoint also refuses a database that an outside
//! SQLite reader holds open.

use std::ffi::CString;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::db::{SCHEMA_VERSION, lock_database, path_with_suffix, refuse_newer_schema};
use crate::model::DbError;

/// How long the source connection waits for an outside reader before the migration
/// gives up with `DatabaseInUse`.
const SOURCE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// SQLite takes page sizes that are powers of two from 512 to 65536 bytes.
pub(crate) fn is_valid_page_size(bytes: u32) -> bool {
    bytes.is_power_of_two() && (512..=65_536).contains(&bytes)
}

/// Rewrites the database at `path` with `page_size`-byte pages, and returns the page
/// size it had before. A database that already has `page_size` is left as it is.
pub fn migrate_page_size(path: &Path, page_size: u32) -> Result<u32, MigrateError> {
    migrate_with_hook(path, page_size, &mut |_| Ok(()))
}

/// The points in a migration where a test can make it fail, as if it had stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The copy is written to the temporary file, and not yet checked.
    Copied,
    /// The copy is checked and synced. The original is not yet replaced.
    Synced,
}

pub(crate) fn migrate_with_hook(
    path: &Path,
    page_size: u32,
    hook: &mut dyn FnMut(Step) -> Result<(), MigrateError>,
) -> Result<u32, MigrateError> {
    if !is_valid_page_size(page_size) {
        return Err(MigrateError::InvalidPageSize(page_size));
    }
    if !path.is_file() {
        return Err(MigrateError::NoDatabase);
    }
    // Taken before the database is read, so a mounted database is refused untouched.
    let _lock = lock_database(path)?;
    let temp = path_with_suffix(path, ".migrate");
    // A copy left by an interrupted run. The lock is held, so no other run is using it.
    remove_if_exists(&temp)?;
    let result = rewrite(path, &temp, page_size, hook);
    if result.is_err() {
        // Best effort. The original is intact, and a later run removes a leftover copy.
        let _ = fs::remove_file(&temp);
    }
    result
}

fn rewrite(
    path: &Path,
    temp: &Path,
    page_size: u32,
    hook: &mut dyn FnMut(Step) -> Result<(), MigrateError>,
) -> Result<u32, MigrateError> {
    let dir = parent_dir(path);
    let source = Connection::open(path)?;
    source.busy_timeout(SOURCE_BUSY_TIMEOUT)?;
    refuse_newer_schema(&source)?;
    let user_version: i64 = source.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let old_size: i64 = source.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let old_size = u32::try_from(old_size).map_err(|_| DbError::Corrupt)?;
    if old_size == page_size {
        return Ok(old_size);
    }
    let wal_mode: String = source.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    let needed = file_len(path)? + file_len(&path_with_suffix(path, "-wal"))?;
    check_room(needed, available_bytes(dir)?)?;
    checkpoint_truncate(&source)?;
    // `VACUUM INTO` writes the copy with the page size set on this connection.
    source.pragma_update(None, "page_size", page_size)?;
    let temp_name = temp.to_str().ok_or(DbError::InvalidInput)?;
    source.execute("VACUUM INTO ?1", [temp_name])?;
    drop(source);
    hook(Step::Copied)?;
    verify_copy(temp, page_size, user_version, wal_mode == "wal")?;
    sync_file(temp)?;
    hook(Step::Synced)?;
    // The checkpoint left the original's WAL empty, so these files hold nothing that
    // the copy lacks. They are removed before the rename, so the copy never meets them.
    remove_if_exists(&path_with_suffix(path, "-wal"))?;
    remove_if_exists(&path_with_suffix(path, "-shm"))?;
    fs::rename(temp, path)?;
    sync_dir(dir)?;
    Ok(old_size)
}

/// Refuses a copy that the free space cannot hold. The bound is the database plus its
/// WAL. A copy at larger pages can be bigger than that; then SQLite fails the copy
/// with a full disk, and the original stays as it was.
fn check_room(needed: u64, available: u64) -> Result<(), MigrateError> {
    if needed > available {
        return Err(MigrateError::InsufficientSpace { needed, available });
    }
    Ok(())
}

/// Moves every WAL frame into the database file and empties the WAL. A busy result
/// means an outside reader holds a snapshot, so the database is in use.
fn checkpoint_truncate(conn: &Connection) -> Result<(), MigrateError> {
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(MigrateError::Db(DbError::DatabaseInUse));
    }
    Ok(())
}

/// Checks the copy before it replaces the original. Its page size and `user_version`
/// must match, and `quick_check` must pass. A WAL original gets a WAL copy, and the
/// copy's WAL is checkpointed away when the connection closes.
fn verify_copy(
    temp: &Path,
    page_size: u32,
    user_version: i64,
    wal: bool,
) -> Result<(), MigrateError> {
    let conn = Connection::open(temp)?;
    if wal {
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    }
    let copied_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let copied_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let check: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    conn.close()
        .map_err(|(_, error)| MigrateError::from(error))?;
    if copied_size != i64::from(page_size) {
        return Err(MigrateError::Check("page size"));
    }
    if copied_version != user_version {
        return Err(MigrateError::Check("user_version"));
    }
    if check != "ok" {
        return Err(MigrateError::Check("quick_check"));
    }
    Ok(())
}

/// Free bytes available to unprivileged writers on the filesystem holding `dir`.
fn available_bytes(dir: &Path) -> io::Result<u64> {
    let c_dir = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut stat = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c_dir` is a NUL-terminated path that lives for the call, and `statvfs`
    // writes `stat` only when it returns 0.
    if unsafe { libc::statvfs(c_dir.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `statvfs` returned 0, so `stat` is initialised.
    let stat = unsafe { stat.assume_init() };
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

fn file_len(path: &Path) -> io::Result<u64> {
    match fs::metadata(path) {
        Ok(meta) => Ok(meta.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn sync_file(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// The directory that holds `path`. A bare file name is in the current directory.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

#[derive(Debug)]
pub enum MigrateError {
    /// A database error. `DatabaseInUse` means a mount, another migration, or an
    /// outside SQLite reader has the database open.
    Db(DbError),
    Io(io::Error),
    /// The database file does not exist.
    NoDatabase,
    /// The page size is not a power of two from 512 to 65536 bytes.
    InvalidPageSize(u32),
    /// The free space cannot hold the database and its WAL.
    InsufficientSpace {
        needed: u64,
        available: u64,
    },
    /// The copy failed a check, and the original was not replaced.
    Check(&'static str),
}

impl fmt::Display for MigrateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Db(DbError::DatabaseInUse) => write!(
                f,
                "database is in use by a mounted dbfs or another process; unmount it first"
            ),
            Self::Db(DbError::UnsupportedSchema(version)) => write!(
                f,
                "database schema version {version} is newer than this dbfs supports \
                 ({SCHEMA_VERSION}); upgrade dbfs"
            ),
            Self::Db(error) => write!(f, "database error: {error}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::NoDatabase => write!(f, "database file does not exist"),
            Self::InvalidPageSize(bytes) => write!(
                f,
                "{bytes} is not a SQLite page size; use a power of two from 512 to 64K"
            ),
            Self::InsufficientSpace { needed, available } => write!(
                f,
                "not enough free space: the copy needs up to {needed} bytes and {available} \
                 are free"
            ),
            Self::Check(what) => write!(
                f,
                "the copy failed its {what} check; the database is unchanged"
            ),
        }
    }
}

impl std::error::Error for MigrateError {}

impl From<DbError> for MigrateError {
    fn from(error: DbError) -> Self {
        Self::Db(error)
    }
}

impl From<rusqlite::Error> for MigrateError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(DbError::from(error))
    }
}

impl From<io::Error> for MigrateError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use std::path::PathBuf;

    fn temp_db_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        path.push(format!(
            "dbfs-migrate-{name}-{}-{nanos}.sqlite",
            std::process::id()
        ));
        path
    }

    fn remove_database(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(path_with_suffix(path, "-wal"));
        let _ = fs::remove_file(path_with_suffix(path, "-shm"));
        let _ = fs::remove_file(path_with_suffix(path, ".lock"));
        let _ = fs::remove_file(path_with_suffix(path, ".migrate"));
    }

    fn raw_page_size(path: &Path) -> i64 {
        let conn = Connection::open(path).expect("open raw connection");
        conn.query_row("PRAGMA page_size", [], |row| row.get(0))
            .expect("read page size")
    }

    #[test]
    fn page_sizes_are_powers_of_two_from_512_to_64k() {
        for bytes in [512, 1024, 4096, 16384, 65536] {
            assert!(is_valid_page_size(bytes), "{bytes}");
        }
        for bytes in [0, 256, 3000, 4095, 131_072] {
            assert!(!is_valid_page_size(bytes), "{bytes}");
        }
    }

    #[test]
    fn copy_that_does_not_fit_in_the_free_space_is_refused() {
        assert!(matches!(
            check_room(10, 9),
            Err(MigrateError::InsufficientSpace {
                needed: 10,
                available: 9
            })
        ));
        assert!(check_room(9, 9).is_ok());
        assert!(check_room(9, 10).is_ok());
    }

    #[test]
    fn interrupted_migration_leaves_the_original_intact() {
        let path = temp_db_path("interrupted");
        {
            let db = Db::open(&path).expect("open new database");
            db.create_file(1, b"kept", 0o644, 1000, 1000)
                .expect("create file");
        }
        let before = fs::read(&path).expect("read original");

        for stop_at in [Step::Copied, Step::Synced] {
            let result = migrate_with_hook(&path, 4096, &mut |reached| {
                if reached == stop_at {
                    Err(MigrateError::Io(io::Error::other("injected failure")))
                } else {
                    Ok(())
                }
            });
            assert!(
                matches!(result, Err(MigrateError::Io(_))),
                "{stop_at:?}: {result:?}"
            );
            assert!(
                !path_with_suffix(&path, ".migrate").exists(),
                "no copy left after {stop_at:?}"
            );
            assert_eq!(
                fs::read(&path).expect("read original"),
                before,
                "original bytes after {stop_at:?}"
            );
        }

        {
            let db = Db::open(&path).expect("reopen original");
            assert!(db.lookup(1, b"kept").is_ok(), "file survives the failures");
        }
        assert_eq!(raw_page_size(&path), 16384);
        assert_eq!(migrate_page_size(&path, 4096).expect("migrate"), 16384);
        assert_eq!(raw_page_size(&path), 4096);
        remove_database(&path);
    }

    #[test]
    fn leftover_copy_from_an_earlier_run_is_replaced() {
        let path = temp_db_path("leftover");
        drop(Db::open(&path).expect("open new database"));
        fs::write(path_with_suffix(&path, ".migrate"), b"not a database")
            .expect("write leftover copy");

        assert_eq!(migrate_page_size(&path, 4096).expect("migrate"), 16384);
        assert!(!path_with_suffix(&path, ".migrate").exists());
        assert_eq!(raw_page_size(&path), 4096);
        remove_database(&path);
    }
}
