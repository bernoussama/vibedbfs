use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use fuser::{MountOption, Session, SessionUnmounter};

use crate::fuse::ShutdownHandle;
use crate::{Cli, Command, Db, DbError, Dbfs};

/// How long `run` waits for the FUSE session to finish after a shutdown signal. The
/// session ends at once when nothing holds the mount. If a process still has a file
/// open, the kernel keeps the session alive, and dbfs exits when this runs out. The
/// data is already flushed by then and new writes fail, so nothing is lost.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// Signals that ask for a clean unmount: Ctrl-C, `kill`, `docker stop`, and a closed terminal.
const SHUTDOWN_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

pub fn run(cli: Cli) -> Result<(), RunError> {
    run_with_mounter(cli, mount_dbfs)
}

pub fn run_with_mounter(
    cli: Cli,
    mounter: impl FnOnce(Dbfs, &Path) -> Result<(), RunError>,
) -> Result<(), RunError> {
    match cli.command {
        Command::Mount {
            database,
            mountpoint,
        } => {
            let db = Db::open(database)?;
            mounter(Dbfs::new(db), &mountpoint)
        }
    }
}

/// Mounts `fs` and runs the FUSE session on its own thread. The calling thread
/// waits for either a shutdown signal or the end of the session.
///
/// On a signal, dirty data is flushed while the mount is still up, then the mount is
/// detached. A process may still hold a file open on it, so the session is not
/// assumed to end. The session ends when nothing holds the mount, and then
/// `Filesystem::destroy` runs. A flush failure at any point makes this return an error.
fn mount_dbfs(fs: Dbfs, mountpoint: &Path) -> Result<(), RunError> {
    let shutdown = fs.shutdown_handle();
    let mut session = Session::new(fs, mountpoint, &mount_options()).map_err(RunError::Mount)?;
    let mut unmounter = session.unmount_callable();
    let (events_tx, events) = mpsc::channel();

    // Block the signals before any thread starts, so every thread inherits the mask
    // and only the waiter below receives them.
    let signals = block_shutdown_signals().map_err(RunError::Session)?;
    spawn_signal_waiter(signals, events_tx.clone()).map_err(RunError::Session)?;
    spawn_session(session, events_tx).map_err(RunError::Session)?;

    let session_result = match events.recv() {
        Ok(Event::SessionEnded(result)) => result,
        Ok(Event::Shutdown) => shut_down(&shutdown, &mut unmounter, &events),
        Err(_) => Err(io::Error::other("event channel closed")),
    };
    session_result.map_err(RunError::Session)?;

    match shutdown.flush_failure() {
        Some(errno) => Err(RunError::ShutdownFlush(errno)),
        None => Ok(()),
    }
}

enum Event {
    /// A shutdown signal arrived.
    Shutdown,
    /// The FUSE session returned and was dropped, so `destroy` has run.
    SessionEnded(io::Result<()>),
}

/// Flushes dirty data, detaches the mount, and waits up to `SHUTDOWN_GRACE` for the
/// session to end. A flush error is not returned from here. It is recorded on
/// `shutdown`, and `mount_dbfs` reports it.
fn shut_down(
    shutdown: &ShutdownHandle,
    unmounter: &mut SessionUnmounter,
    events: &Receiver<Event>,
) -> io::Result<()> {
    let _ = shutdown.flush_for_shutdown();
    unmounter.unmount()?;

    let deadline = Instant::now() + SHUTDOWN_GRACE;
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::SessionEnded(result)) => return result,
            Ok(Event::Shutdown) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// Blocks the shutdown signals in the calling thread and returns the set, for `sigwait`.
fn block_shutdown_signals() -> io::Result<libc::sigset_t> {
    // SAFETY: `sigset_t` is plain data. The sigset calls and `pthread_sigmask` only
    // write to the set passed in, which lives in this frame.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in SHUTDOWN_SIGNALS {
            libc::sigaddset(&mut set, signal);
        }
        let error = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        Ok(set)
    }
}

fn spawn_signal_waiter(signals: libc::sigset_t, events: Sender<Event>) -> io::Result<()> {
    thread::Builder::new()
        .name("dbfs-signals".to_owned())
        .spawn(move || {
            loop {
                let mut signal = 0;
                // SAFETY: `signals` and `signal` are valid for the duration of the call.
                let error = unsafe { libc::sigwait(&signals, &mut signal) };
                // sigwait only fails for an invalid set, and this set is fixed.
                assert_eq!(error, 0, "sigwait on the shutdown signal set failed");
                if events.send(Event::Shutdown).is_err() {
                    return;
                }
            }
        })
        .map(|_| ())
}

fn spawn_session(mut session: Session<Dbfs>, events: Sender<Event>) -> io::Result<()> {
    thread::Builder::new()
        .name("dbfs-session".to_owned())
        .spawn(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(move || {
                let result = session.run();
                // Dropping the session runs `destroy` unless FUSE_DESTROY already did.
                drop(session);
                result
            }))
            .unwrap_or_else(|_| Err(io::Error::other("filesystem session panicked")));
            let _ = events.send(Event::SessionEnded(result));
        })
        .map(|_| ())
}

pub fn mount_options() -> Vec<MountOption> {
    vec![
        MountOption::FSName("dbfs".to_string()),
        // Let the kernel enforce file permission checks based on inode mode/uid/gid.
        // Without this, the kernel allows all accesses regardless of file permissions.
        MountOption::DefaultPermissions,
    ]
}

#[derive(Debug)]
pub enum RunError {
    Db(DbError),
    Mount(std::io::Error),
    /// The FUSE session or one of run's threads failed.
    Session(std::io::Error),
    /// Dirty data could not be written to the database during shutdown.
    ShutdownFlush(i32),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Db(DbError::UnsupportedSchema(version)) => write!(
                f,
                "database schema version {version} is newer than this dbfs supports \
                 ({}); upgrade dbfs",
                crate::SCHEMA_VERSION
            ),
            Self::Db(error) => write!(f, "database error: {error:?}"),
            Self::Mount(error) => write!(f, "mount error: {error}"),
            Self::Session(error) => write!(f, "filesystem session error: {error}"),
            Self::ShutdownFlush(errno) => write!(
                f,
                "failed to flush unsaved data on shutdown; recent writes may be lost: {}",
                io::Error::from_raw_os_error(*errno)
            ),
        }
    }
}

impl std::error::Error for RunError {}

impl From<DbError> for RunError {
    fn from(error: DbError) -> Self {
        Self::Db(error)
    }
}
