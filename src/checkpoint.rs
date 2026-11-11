//! WAL checkpoints kept off the write path.
//!
//! The main connection never checkpoints on commit (`wal_autocheckpoint = 0`).
//! Instead:
//!
//! - A commit hook on the main connection records how many frames the WAL holds.
//!   The hook only stores that count: no lock, no SQL.
//! - A background thread, with its own connection to the same file, runs PASSIVE
//!   checkpoints when the WAL is over `passive_at`, or when no commit has come for
//!   `IDLE` while there is work it has not reset.
//! - Backfilling alone does not reset the WAL under sustained writes. A writer
//!   restarts the WAL only when every frame is backfilled and nothing is in its way.
//!   So after a PASSIVE that backfilled everything, the thread catches up without
//!   the database lock, takes the main connection's mutex (no write transaction can
//!   be open while it is held), runs PASSIVE once more, and releases it. The next
//!   writer then restarts the WAL at its beginning. This is the reset handshake.
//! - The catch-up also stops once the WAL reaches `forced_at`: three times
//!   `passive_at`, and at most three quarters of the cap. The WAL cannot grow while
//!   the lock is held, so the handshake runs at once, and the lock is held only for
//!   the frames still unbackfilled.
//! - Hard cap: a writer that leaves the WAL over `wal_cap` runs
//!   `wal_checkpoint(RESTART)` on the main connection before it returns. That blocks,
//!   but it bounds the WAL when the thread falls far behind.
//! - A checkpoint error is sticky: the next fsync or fsyncdir reports it once as EIO.
//!
//! A reset rewinds the WAL without shrinking the file. The next writer after a reset
//! restarts the WAL, and SQLite then truncates the file to `JOURNAL_SIZE_LIMIT`, so
//! an idle database keeps at most that much WAL on disk. Unmount truncates it to zero.

use std::ffi::c_void;
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, ffi};

use crate::DbError;

/// The background thread checkpoints (PASSIVE) once the WAL is this large.
const PASSIVE_THRESHOLD: u64 = 64 << 20;
/// The handshake is forced at this many `PASSIVE_THRESHOLD`s (see `WalState::forced_at`).
const FORCED_MULTIPLE: u64 = 3;
/// A WAL with unreset work, and no commit for this long, is checkpointed.
const IDLE: Duration = Duration::from_secs(1);
/// The reset handshake catches up without the database lock until at most this
/// much of the WAL is left to backfill.
const CATCH_UP_BYTES: u64 = 1 << 20;
/// Most unlocked catch-up rounds before the handshake takes the lock regardless.
const CATCH_UP_ROUNDS: usize = 8;
/// Busy timeout of the connections that checkpoint (the main one has its own).
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause between RESTART attempts while the WAL is busy.
const RESTART_RETRY: Duration = Duration::from_millis(1);
/// Pause after a round that did not reset the WAL, so busy rounds do not spin while
/// writers keep kicking the thread.
const ROUND_RETRY: Duration = Duration::from_millis(1);
/// How long a writer over the cap keeps retrying a busy RESTART before it gives up.
const RESTART_WAIT: Duration = Duration::from_secs(60);
/// While the checkpoint thread waits for the database lock, a writer pauses this long
/// before taking it again. See `Checkpoints::yield_to_handshake`.
const HANDOFF_PAUSE: Duration = Duration::from_micros(50);
/// `PRAGMA journal_size_limit`: when the next writer restarts the WAL after a
/// reset, SQLite shrinks the WAL file to at most this size.
pub(crate) const JOURNAL_SIZE_LIMIT: u64 = 64 << 20;
/// `--wal-cap` when none is given.
pub const DEFAULT_WAL_CAP: u64 = 1 << 30;
/// Smallest `--wal-cap`: one 64 KiB chunk.
pub const MIN_WAL_CAP: u64 = 64 << 10;
const WAL_HEADER_BYTES: u64 = 32;
const WAL_FRAME_HEADER_BYTES: u64 = 24;

/// Checkpoint activity since the database was opened. See `Db::checkpoint_counts`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointCounts {
    /// WAL resets done by the reset handshake.
    pub resets: u64,
    /// Writer RESTARTs at the cap that completed.
    pub writer_restarts: u64,
}

/// What the writers, the commit hook and the thread share. The hook gets a pointer
/// to it, so `Checkpoints` keeps it alive for as long as the hook is installed.
pub(crate) struct WalState {
    /// Frames in the WAL as of the latest commit, as the commit hook reports them.
    /// Zero after a reset: the next writer restarts the WAL at its beginning.
    frames: AtomicU32,
    /// Commits seen by the hook.
    commits: AtomicU64,
    /// Bytes per WAL frame: the page size plus the frame header.
    frame_bytes: u64,
    /// The largest WAL a writer leaves in place. Above it the writer checkpoints.
    wal_cap: u64,
    /// The WAL size above which the thread checkpoints.
    passive_at: u64,
    /// The WAL size at which the handshake stops catching up and takes the lock.
    forced_at: u64,
    /// Handshakes that reset the WAL, and writer RESTARTs that completed.
    resets: AtomicU64,
    writer_restarts: AtomicU64,
    /// The thread is about to wait for the database lock. See `yield_to_handshake`.
    handshake_wanted: AtomicBool,
    wake: Mutex<Wake>,
    woken: Condvar,
    /// Set by a failed checkpoint. Read without the lock as a fast path; the lock
    /// guards `failure`, and both change together.
    failed: AtomicBool,
    failure: Mutex<Option<String>>,
}

#[derive(Default)]
struct Wake {
    /// A writer found the WAL over `passive_at`.
    kicked: bool,
    stop: bool,
}

struct Wakeup {
    stop: bool,
    /// No kick came within `IDLE`.
    timed_out: bool,
}

impl WalState {
    fn new(page_size: u64, wal_cap: u64) -> Self {
        let passive_at = PASSIVE_THRESHOLD.min(wal_cap / 2);
        Self {
            frames: AtomicU32::new(0),
            commits: AtomicU64::new(0),
            frame_bytes: page_size + WAL_FRAME_HEADER_BYTES,
            wal_cap,
            passive_at,
            forced_at: (FORCED_MULTIPLE * passive_at).min(wal_cap / 4 * 3),
            resets: AtomicU64::new(0),
            writer_restarts: AtomicU64::new(0),
            handshake_wanted: AtomicBool::new(false),
            wake: Mutex::new(Wake::default()),
            woken: Condvar::new(),
            failed: AtomicBool::new(false),
            failure: Mutex::new(None),
        }
    }

    /// The WAL's size in use: its header plus the frames it holds.
    fn wal_bytes(&self) -> u64 {
        match u64::from(self.frames.load(Ordering::Acquire)) {
            0 => 0,
            frames => WAL_HEADER_BYTES + frames * self.frame_bytes,
        }
    }

    fn kick(&self) {
        self.wake.lock().unwrap().kicked = true;
        self.woken.notify_one();
    }

    fn stop(&self) {
        self.wake.lock().unwrap().stop = true;
        self.woken.notify_all();
    }

    fn wait_for_work(&self) -> Wakeup {
        let wake = self.wake.lock().unwrap();
        let (mut wake, timeout) = self
            .woken
            .wait_timeout_while(wake, IDLE, |wake| !wake.kicked && !wake.stop)
            .unwrap();
        wake.kicked = false;
        Wakeup {
            stop: wake.stop,
            timed_out: timeout.timed_out(),
        }
    }

    fn fail(&self, message: String) {
        eprintln!("dbfs: WAL checkpoint failed: {message}");
        let mut slot = self.failure.lock().unwrap();
        *slot = Some(message);
        self.failed.store(true, Ordering::Release);
    }

    /// The sticky checkpoint error, if one happened since the last call. A caller
    /// that gets `Some` reports it, and later callers get `None`.
    pub(crate) fn take_failure(&self) -> Option<String> {
        if !self.failed.load(Ordering::Acquire) {
            return None;
        }
        let mut slot = self.failure.lock().unwrap();
        self.failed.store(false, Ordering::Release);
        slot.take()
    }
}

/// The checkpoint machinery of one file-backed database: the commit hook on the
/// main connection, and when `background` is set, the checkpoint thread.
pub(crate) struct Checkpoints {
    state: Arc<WalState>,
    path: PathBuf,
    background: bool,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Checkpoints {
    /// Installs the commit hook on `conn`. Call it after `PRAGMA wal_autocheckpoint = 0`,
    /// which replaces any hook. The thread is started separately by `start`.
    pub(crate) fn install(
        conn: &Connection,
        path: &Path,
        wal_cap: u64,
        background: bool,
    ) -> Result<Self, DbError> {
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        let state = Arc::new(WalState::new(page_size as u64, wal_cap));
        // SAFETY: the hook gets a pointer to `state`. This `Checkpoints` keeps that
        // `Arc` alive until `uninstall` has removed the hook.
        unsafe {
            ffi::sqlite3_wal_hook(
                conn.handle(),
                Some(on_commit),
                Arc::as_ptr(&state).cast_mut().cast::<c_void>(),
            );
        }
        Ok(Self {
            state,
            path: path.to_path_buf(),
            background,
            worker: Mutex::new(None),
        })
    }

    /// Starts the checkpoint thread, when this database has one. `main` is the lock
    /// of the main connection, which the reset handshake takes. The thread inherits
    /// the calling thread's signal mask, so the caller must block any signals it
    /// wants handled elsewhere first (`run` does this for the shutdown signals).
    pub(crate) fn start(&self, main: Arc<Mutex<Connection>>) -> Result<(), DbError> {
        if !self.background {
            return Ok(());
        }
        let conn = open_thread_connection(&self.path)?;
        let state = Arc::clone(&self.state);
        let handle = thread::Builder::new()
            .name("dbfs-checkpoint".to_owned())
            .spawn(move || run(state, main, conn))
            .map_err(|error| {
                DbError::Sqlite(format!("cannot start the checkpoint thread: {error}"))
            })?;
        *self.worker.lock().unwrap() = Some(handle);
        Ok(())
    }

    /// Stops and joins the checkpoint thread. Returns true if it was running, so
    /// the caller knows this is the first close. Later calls return false.
    pub(crate) fn stop(&self) -> bool {
        let Some(handle) = self.worker.lock().unwrap().take() else {
            return false;
        };
        self.state.stop();
        if handle.join().is_err() {
            eprintln!("dbfs: the checkpoint thread panicked");
        }
        true
    }

    /// Removes the commit hook. Call it before the connection closes, so no commit
    /// can reach the state after it is freed.
    pub(crate) fn uninstall(&self, conn: &Connection) {
        // SAFETY: the handle belongs to `conn`, which the caller holds.
        unsafe { ffi::sqlite3_wal_hook(conn.handle(), None, ptr::null_mut()) };
    }

    /// Called by a writer before it takes the main connection's lock. While the thread
    /// is waiting for that lock, the writer pauses, so the thread gets it. A std mutex
    /// does not hand over to a waiter, and a writer that relocks at once after each
    /// slice could hold the thread off for the whole burst, so the WAL would grow past
    /// `forced_at`.
    pub(crate) fn yield_to_handshake(&self) {
        while self.state.handshake_wanted.load(Ordering::Acquire) {
            thread::sleep(HANDOFF_PAUSE);
        }
    }

    /// Runs after every write, while the main connection is still locked and the
    /// commit is done. Above the cap it checkpoints the WAL itself. Otherwise, past
    /// `passive_at`, it wakes the thread.
    pub(crate) fn after_write(&self, conn: &Connection) {
        let bytes = self.state.wal_bytes();
        if bytes > self.state.wal_cap {
            self.restart_wal(conn);
        } else if self.background && bytes > self.state.passive_at {
            self.state.kick();
        }
    }

    /// Runs RESTART until it completes, so the WAL is back under the cap when the
    /// writer returns. A busy result means the checkpoint thread is in a PASSIVE pass
    /// over a large WAL, which can take seconds. Gives up after `RESTART_WAIT`, and
    /// the next commit tries again. Each give-up lets one more commit past the cap.
    fn restart_wal(&self, conn: &Connection) {
        let deadline = Instant::now() + RESTART_WAIT;
        loop {
            match checkpoint(conn, "RESTART") {
                Ok((false, log, backfilled)) => {
                    // A completed RESTART leaves nothing for the next writer to backfill.
                    if backfilled == log {
                        self.state.frames.store(0, Ordering::Release);
                        self.state.writer_restarts.fetch_add(1, Ordering::Relaxed);
                    }
                    return;
                }
                Ok((true, _, _)) => {}
                Err(error) if is_busy(&error) => {}
                Err(error) => {
                    self.state.fail(error.to_string());
                    return;
                }
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "dbfs: the WAL is over its cap and RESTART stayed busy; the next commit tries again"
                );
                return;
            }
            thread::sleep(RESTART_RETRY);
        }
    }

    /// See `WalState::take_failure`.
    pub(crate) fn take_failure(&self) -> Option<String> {
        self.state.take_failure()
    }

    /// Size of the `-wal` file on disk, or 0 when there is none.
    pub(crate) fn wal_file_bytes(&self) -> u64 {
        std::fs::metadata(wal_path(&self.path)).map_or(0, |metadata| metadata.len())
    }

    /// See `Db::checkpoint_counts`.
    pub(crate) fn counts(&self) -> CheckpointCounts {
        CheckpointCounts {
            resets: self.state.resets.load(Ordering::Relaxed),
            writer_restarts: self.state.writer_restarts.load(Ordering::Relaxed),
        }
    }
}

/// `SQLITE_CHECKPOINT_*` callback: runs after each commit on the main connection.
/// It only stores the frame count, so it runs no SQL and takes no lock.
unsafe extern "C" fn on_commit(
    context: *mut c_void,
    _db: *mut ffi::sqlite3,
    _database: *const c_char,
    frames: c_int,
) -> c_int {
    // SAFETY: `context` points to the `WalState` that `Checkpoints` keeps alive while
    // this hook is installed.
    let state = unsafe { &*context.cast::<WalState>() };
    state.frames.store(frames.max(0) as u32, Ordering::Release);
    state.commits.fetch_add(1, Ordering::Release);
    ffi::SQLITE_OK
}

/// The checkpoint thread. Wakes on a kick, on a stop, or every `IDLE`, and
/// checkpoints when the WAL is over the threshold or has been quiet with work left.
fn run(state: Arc<WalState>, main: Arc<Mutex<Connection>>, conn: Connection) {
    let mut seen = state.commits.load(Ordering::Acquire);
    // Commits as of the last reset. Commits after it are work the thread has left.
    let mut reset_at = seen;
    loop {
        let wakeup = state.wait_for_work();
        if wakeup.stop {
            return;
        }
        let commits = state.commits.load(Ordering::Acquire);
        let quiet = wakeup.timed_out && commits == seen;
        seen = commits;
        let over = state.wal_bytes() > state.passive_at;
        let due = over || (quiet && commits != reset_at);
        if !due {
            continue;
        }
        if round(&state, &conn, &main) {
            reset_at = commits;
        } else {
            thread::sleep(ROUND_RETRY);
        }
    }
}

/// One checkpoint attempt. Returns true if the WAL is reset, or was empty, afterwards.
fn round(state: &WalState, conn: &Connection, main: &Mutex<Connection>) -> bool {
    match try_round(state, conn, main) {
        Ok(reset) => reset,
        Err(error) => {
            state.fail(error.to_string());
            false
        }
    }
}

fn try_round(
    state: &WalState,
    conn: &Connection,
    main: &Mutex<Connection>,
) -> rusqlite::Result<bool> {
    let (busy, log, mut backfilled) = checkpoint(conn, "PASSIVE")?;
    if busy || backfilled < log {
        return Ok(false);
    }
    if log == 0 {
        return Ok(true);
    }

    // Catch up without the lock, so the lock is held for only the last few frames.
    // The WAL grows while a pass runs, so stop at `forced_at` and take the lock: the
    // writers are then blocked and the WAL stops growing.
    for _ in 0..CATCH_UP_ROUNDS {
        let behind = u64::from(state.frames.load(Ordering::Acquire)).saturating_sub(backfilled);
        if behind * state.frame_bytes <= CATCH_UP_BYTES || state.wal_bytes() >= state.forced_at {
            break;
        }
        backfilled = checkpoint(conn, "PASSIVE")?.2;
    }
    // No write transaction is open while the lock is held, so the WAL can finish
    // backfilling and then restart. Writers see `handshake_wanted` and let this in.
    state.handshake_wanted.store(true, Ordering::Release);
    let guard = main.lock().unwrap_or_else(PoisonError::into_inner);
    state.handshake_wanted.store(false, Ordering::Release);
    let (busy, log, backfilled) = checkpoint(conn, "PASSIVE")?;
    let reset = !busy && backfilled == log;
    if reset {
        state.frames.store(0, Ordering::Release);
        state.resets.fetch_add(1, Ordering::Relaxed);
    }
    drop(guard);
    Ok(reset)
}

/// `PRAGMA wal_checkpoint(mode)` as (busy, frames in the WAL, frames backfilled).
/// `mode` is always one of this module's literals.
fn checkpoint(conn: &Connection, mode: &str) -> rusqlite::Result<(bool, u64, u64)> {
    let start = Instant::now();
    let result: rusqlite::Result<(i64, i64, i64)> =
        conn.query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        });
    crate::stats::checkpoint_done(start.elapsed(), result.is_ok());
    let (busy, log, backfilled) = result?;
    Ok((busy != 0, log.max(0) as u64, backfilled.max(0) as u64))
}

/// Moves every frame into the database file and empties the WAL. Called once, at
/// unmount. An error is only logged: nothing can report it after unmount, and the
/// data is already durable in the WAL.
pub(crate) fn truncate_wal(conn: &Connection) {
    match checkpoint(conn, "TRUNCATE") {
        Ok((false, _, _)) => {}
        Ok((true, _, _)) => eprintln!("dbfs: could not truncate the WAL at unmount: busy"),
        Err(error) => eprintln!("dbfs: could not truncate the WAL at unmount: {error}"),
    }
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _) if failure.code == ErrorCode::DatabaseBusy
    )
}

fn open_thread_connection(path: &Path) -> Result<Connection, DbError> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch(&format!(
        "PRAGMA synchronous = NORMAL;
         PRAGMA journal_size_limit = {JOURNAL_SIZE_LIMIT};"
    ))?;
    Ok(conn)
}

fn wal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_is_reported_once() {
        let state = WalState::new(4096, DEFAULT_WAL_CAP);
        assert_eq!(state.take_failure(), None);
        state.fail("disk I/O error".to_owned());
        assert_eq!(state.take_failure().as_deref(), Some("disk I/O error"));
        assert_eq!(state.take_failure(), None);
    }

    #[test]
    fn wal_size_counts_the_header_and_each_frame() {
        let state = WalState::new(4096, DEFAULT_WAL_CAP);
        assert_eq!(state.wal_bytes(), 0);
        state.frames.store(10, Ordering::Release);
        assert_eq!(state.wal_bytes(), 32 + 10 * (4096 + 24));
    }

    #[test]
    fn passive_threshold_stays_below_the_cap() {
        assert_eq!(
            WalState::new(4096, DEFAULT_WAL_CAP).passive_at,
            PASSIVE_THRESHOLD
        );
        assert_eq!(WalState::new(4096, 1 << 20).passive_at, 512 << 10);
    }

    #[test]
    fn the_handshake_is_forced_well_below_the_cap() {
        // Three passive thresholds with the default cap.
        let default = WalState::new(4096, DEFAULT_WAL_CAP);
        assert_eq!(default.forced_at, 192 << 20);
        // A small cap cannot fit three thresholds, so it takes three quarters of it.
        let small = WalState::new(4096, 1 << 20);
        assert_eq!(small.forced_at, 768 << 10);
        assert!(small.passive_at < small.forced_at && small.forced_at < small.wal_cap);
    }
}
