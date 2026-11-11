//! Group commit: operations share one open SQLite transaction (the batch) until a
//! commit point, instead of committing, and so syncing the WAL, once per operation.
//!
//! Each operation runs on the batch without a savepoint. So an operation checks
//! everything that can fail (missing or existing names, wrong kinds, size limits)
//! before its first write. A failure that SQLite reports after a write cannot be
//! undone in part: see `Main::undo_failed_op`.
//!
//! A batch commits at a commit point:
//!
//! - `fsync` and `fsyncdir` (`Db::sync_durable`), and `statfs` (`Db::volume_usage`);
//! - the database closing: unmount, shutdown, or the `Db` dropping;
//! - `COMMIT_DELAY` after its first operation, from the commit timer thread;
//! - once it holds `MAX_BATCH_OPS` operations or `MAX_BATCH_BYTES` of chunk data;
//! - before the checkpoint thread's reset handshake (`Main::commit_for_reset`).
//!
//! `flush` and `release` do not commit. Per-operation mode (`--sync-metadata`, and
//! read-only mounts) commits every operation before it returns, and so does every
//! operation while the free space on the database's filesystem is short.
//!
//! Fail-stop. When a COMMIT fails, or SQLite rolls back a batch that held
//! operations already acknowledged to the kernel, those writes are gone. Then
//! `Main` enters a failed state: writes and fsync return EIO, reads still work, the
//! failure is logged once, and it lasts until the filesystem is remounted.

use std::ops::Deref;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::checkpoint::Checkpoints;
use crate::{DbError, stats};

/// A batch commits this long after its first operation, unless a commit point
/// comes first.
pub(crate) const COMMIT_DELAY: Duration = Duration::from_millis(5);
/// A batch commits once it holds this many operations.
pub(crate) const MAX_BATCH_OPS: u64 = 256;
/// A batch commits once its chunk data reaches this many bytes.
pub(crate) const MAX_BATCH_BYTES: u64 = 64 << 20;
/// Free space below this makes every operation commit by itself. Never less than
/// twice `MAX_BATCH_BYTES`, so one full batch always fits.
const MIN_FREE_BYTES: u64 = 256 << 20;
/// How often the free space is measured.
const FREE_SPACE_INTERVAL: Duration = Duration::from_secs(1);

/// The open transaction and what it holds.
struct Batch {
    /// When the batch's first operation ran.
    opened: Instant,
    /// Operations that returned success, so their writes are acknowledged.
    ops: u64,
    /// Chunk data those operations may have written.
    bytes: u64,
}

/// The free-space guard's state.
struct FreeSpace {
    /// When free space was last measured.
    checked: Option<Instant>,
    /// Whether the last measurement was below `floor`.
    low: bool,
    /// Free bytes below which operations commit one by one.
    floor: u64,
}

/// The main connection and its open batch. Shared by the FUSE thread, the
/// checkpoint thread and the commit timer thread, behind one mutex. Dereferences to
/// the connection, for reads.
pub(crate) struct Main {
    conn: Connection,
    /// Commit every operation before it returns.
    per_op: bool,
    batch: Option<Batch>,
    /// Why writes are refused. Set once, by the first failure.
    failed: Option<String>,
    /// The commit hook's state and the checkpoint thread. `None` in memory.
    checkpoints: Option<Arc<Checkpoints>>,
    /// Wakes the commit timer. `None` when batches are not timed.
    timer: Option<Arc<Deadline>>,
    /// How long a batch may stay open after its first operation.
    /// `COMMIT_DELAY`, except in tests.
    commit_delay: Duration,
    /// The database file, for the free-space check. `None` in memory.
    path: Option<PathBuf>,
    free_space: FreeSpace,
}

impl Deref for Main {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl Main {
    /// `per_op` commits every operation. In-memory databases and databases with no
    /// background checkpoints are per-op too.
    pub(crate) fn new(conn: Connection, per_op: bool) -> Self {
        let path = conn
            .path()
            .filter(|path| !path.is_empty() && *path != ":memory:")
            .map(PathBuf::from);
        Self {
            conn,
            per_op,
            batch: None,
            failed: None,
            checkpoints: None,
            timer: None,
            commit_delay: COMMIT_DELAY,
            path,
            free_space: FreeSpace {
                checked: None,
                low: false,
                floor: MIN_FREE_BYTES.max(2 * MAX_BATCH_BYTES),
            },
        }
    }

    /// Hands over the checkpoint state, so each commit can check the WAL.
    pub(crate) fn attach_checkpoints(&mut self, checkpoints: Arc<Checkpoints>) {
        self.checkpoints = Some(checkpoints);
    }

    /// Hands over the commit timer's wake-up, so a batch that opens is timed.
    pub(crate) fn attach_timer(&mut self, timer: Arc<Deadline>) {
        self.timer = Some(timer);
    }

    /// The database file, or `None` in memory.
    pub(crate) fn file_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether writes are refused because a batch failed.
    pub(crate) fn writes_refused(&self) -> bool {
        self.failed.is_some()
    }

    /// Runs one operation on the batch, opening the batch if none is open. `bytes`
    /// is the most chunk data the operation may write. `op` must check every failure
    /// it can before its first write, as described in the module docs.
    pub(crate) fn run_op<T>(
        &mut self,
        bytes: u64,
        op: impl FnOnce(&Connection) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        if let Some(reason) = &self.failed {
            return Err(refused(reason));
        }
        let commit_each = self.per_op || self.low_space();
        if self.batch.is_none() {
            self.begin()?;
        }
        let before = self.conn.total_changes();
        match op(&self.conn) {
            Ok(value) => {
                if let Some(batch) = self.batch.as_mut() {
                    batch.ops += 1;
                    batch.bytes += bytes;
                }
                let full = self.batch.as_ref().is_some_and(|batch| {
                    batch.ops >= MAX_BATCH_OPS || batch.bytes >= MAX_BATCH_BYTES
                });
                if commit_each || full {
                    self.commit_batch(true)?;
                }
                Ok(value)
            }
            Err(error) => {
                self.undo_failed_op(before, &error);
                Err(error)
            }
        }
    }

    /// Commits the open batch, so that everything acknowledged so far is in the WAL.
    /// Fails if writes are refused.
    pub(crate) fn flush_batch(&mut self) -> Result<(), DbError> {
        if let Some(reason) = &self.failed {
            return Err(refused(reason));
        }
        self.commit_batch(true)
    }

    /// Commits the open batch if its deadline has passed. Called by the commit timer.
    pub(crate) fn commit_if_due(&mut self) {
        let due = self
            .batch
            .as_ref()
            .is_some_and(|batch| batch.opened + self.commit_delay <= Instant::now());
        if due {
            let _ = self.commit_batch(true);
        }
    }

    /// Commits the open batch for the checkpoint thread's reset handshake, which runs
    /// with the lock held. Skips the WAL-cap check, which could block.
    pub(crate) fn commit_for_reset(&mut self) {
        let _ = self.commit_batch(false);
    }

    /// Sets how long a batch may stay open after its first operation.
    pub(crate) fn set_commit_delay(&mut self, delay: Duration) {
        self.commit_delay = delay;
    }

    /// Sets the free bytes below which every operation commits by itself.
    pub(crate) fn set_free_space_floor(&mut self, bytes: u64) {
        self.free_space.floor = bytes;
        self.free_space.checked = None;
    }

    fn begin(&mut self) -> Result<(), DbError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let opened = Instant::now();
        self.batch = Some(Batch {
            opened,
            ops: 0,
            bytes: 0,
        });
        if let Some(timer) = &self.timer {
            timer.arm(opened + self.commit_delay);
        }
        Ok(())
    }

    /// Cleans up after a failed operation. SQLite rolls back a failed statement by
    /// itself, and the rest of the batch stays. But an operation that already wrote
    /// something has to be undone, and a rollback also undoes the whole batch.
    /// Acknowledged operations in that batch are then lost: fail-stop.
    fn undo_failed_op(&mut self, before: u64, error: &DbError) {
        let changed = self.conn.total_changes() != before;
        // In per-op mode the batch is only this operation, so it always ends here.
        if !self.conn.is_autocommit() && (changed || self.per_op) {
            self.rollback();
        }
        if self.conn.is_autocommit() {
            // SQLite ended the transaction itself, or the rollback above did.
            let lost = self.batch.take().map_or(0, |batch| batch.ops);
            if lost > 0 {
                self.fail(format!(
                    "SQLite rolled back {lost} batched operations after {error}"
                ));
            }
        }
    }

    /// Commits the open batch, if any. A failed COMMIT rolls back, and then in batch
    /// mode it is fail-stop. Per-op mode just reports it, since the one operation
    /// that failed is the only one lost.
    fn commit_batch(&mut self, wal_check: bool) -> Result<(), DbError> {
        let Some(batch) = self.batch.take() else {
            return Ok(());
        };
        let started = Instant::now();
        match self.conn.execute_batch("COMMIT") {
            Ok(()) => {
                stats::batch_commit(batch.ops, started.elapsed());
                let checkpoints = self.checkpoints.as_ref().filter(|_| wal_check);
                if let Some(checkpoints) = checkpoints {
                    checkpoints.after_write(&self.conn);
                }
                Ok(())
            }
            Err(error) => {
                if !self.conn.is_autocommit() {
                    self.rollback();
                }
                let error = DbError::from(error);
                if !self.per_op {
                    self.fail(format!(
                        "COMMIT of {} batched operations failed: {error}",
                        batch.ops
                    ));
                }
                Err(error)
            }
        }
    }

    fn rollback(&mut self) {
        // The failure that made us roll back is what the caller reports.
        let _ = self.conn.execute_batch("ROLLBACK");
    }

    /// Enters the failed state, once. Logs the reason, drops the batch, and rolls
    /// back any open transaction, so reads see only committed data.
    fn fail(&mut self, reason: String) {
        if self.failed.is_some() {
            return;
        }
        if !self.conn.is_autocommit() {
            self.rollback();
        }
        self.batch = None;
        stats::batch_failed();
        eprintln!(
            "dbfs: {reason}; writes are refused until the filesystem is remounted, reads still work"
        );
        self.failed = Some(reason);
    }

    /// Whether the free space on the database's filesystem is below the floor.
    /// Measured at most once per `FREE_SPACE_INTERVAL`. Always false in memory.
    fn low_space(&mut self) -> bool {
        let Some(path) = &self.path else {
            return false;
        };
        let due = self
            .free_space
            .checked
            .is_none_or(|at| at.elapsed() >= FREE_SPACE_INTERVAL);
        if due {
            self.free_space.checked = Some(Instant::now());
            if let Some(free) = free_bytes(path) {
                self.free_space.low = free < self.free_space.floor;
            }
        }
        self.free_space.low
    }
}

/// The error for an operation that arrives after a batch failed.
fn refused(reason: &str) -> DbError {
    DbError::Sqlite(format!(
        "metadata writes are refused after a failed batch: {reason}"
    ))
}

/// Bytes available to unprivileged users on the filesystem holding `path`.
#[allow(clippy::unnecessary_cast)]
fn free_bytes(path: &Path) -> Option<u64> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` writes only into `stat`, which is zeroed and outlives the call.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// When the open batch is due to commit. Shared with the commit timer thread.
pub(crate) struct Deadline {
    state: Mutex<DeadlineState>,
    changed: Condvar,
}

#[derive(Default)]
struct DeadlineState {
    /// When the timer should commit the batch, if one is open.
    at: Option<Instant>,
    stop: bool,
}

impl Deadline {
    fn arm(&self, at: Instant) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.at = Some(at);
        self.changed.notify_all();
    }

    fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.stop = true;
        self.changed.notify_all();
    }
}

/// The commit timer thread, which commits a batch `COMMIT_DELAY` after it opens.
pub(crate) struct CommitTimer {
    deadline: Arc<Deadline>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl CommitTimer {
    /// Starts the thread. It locks `main` only to commit, never while it holds the
    /// deadline's lock, so the two cannot deadlock.
    pub(crate) fn start(main: Arc<Mutex<Main>>) -> Result<Self, DbError> {
        let deadline = Arc::new(Deadline {
            state: Mutex::new(DeadlineState::default()),
            changed: Condvar::new(),
        });
        let thread_deadline = Arc::clone(&deadline);
        let handle = thread::Builder::new()
            .name("dbfs-commit".to_owned())
            .spawn(move || run(&thread_deadline, &main))
            .map_err(|error| {
                DbError::Sqlite(format!("cannot start the commit timer thread: {error}"))
            })?;
        Ok(Self {
            deadline,
            thread: Mutex::new(Some(handle)),
        })
    }

    /// The wake-up that `Main` arms when a batch opens.
    pub(crate) fn deadline(&self) -> Arc<Deadline> {
        Arc::clone(&self.deadline)
    }

    /// Stops and joins the thread. Later calls do nothing. Must not run while the
    /// caller holds the `Main` lock.
    pub(crate) fn stop(&self) {
        let handle = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(handle) = handle else {
            return;
        };
        self.deadline.stop();
        if handle.join().is_err() {
            eprintln!("dbfs: the commit timer thread panicked");
        }
    }
}

fn run(deadline: &Deadline, main: &Mutex<Main>) {
    let mut state = deadline
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    loop {
        if state.stop {
            return;
        }
        let Some(at) = state.at else {
            state = deadline
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            continue;
        };
        let now = Instant::now();
        if now < at {
            state = deadline
                .changed
                .wait_timeout(state, at - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            continue;
        }
        state.at = None;
        drop(state);
        main.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .commit_if_due();
        state = deadline
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
    }
}
