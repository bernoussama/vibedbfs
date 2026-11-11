use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use fuser::{MountOption, Session, SessionUnmounter};

use crate::fuse::DEFAULT_WRITE_BUFFER;
use crate::fuse::ShutdownHandle;
use crate::stats;
use crate::{Cli, Command, Db, DbError, Dbfs, MountConfig, OpenOptions};

const ALLOW_OTHER_HINT: &str = "hint: allow_other requires 'user_allow_other' in /etc/fuse.conf";

/// How long `run` waits for the FUSE session to finish after a shutdown signal. The
/// session ends at once when nothing holds the mount. If a process still has a file
/// open, the kernel keeps the session alive, and dbfs exits when this runs out. The
/// data is already flushed by then and new writes fail, so nothing is lost.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// Signals that ask for a clean unmount: Ctrl-C, `kill`, `docker stop`, and a closed terminal.
const SHUTDOWN_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

pub fn run(cli: Cli) -> Result<(), RunError> {
    // Blocked before the database opens, because its checkpoint thread starts there
    // and inherits this mask. Only the signal waiter in `mount_dbfs` may take these
    // signals: a thread that did would apply their default action and end the process.
    block_shutdown_signals().map_err(RunError::Session)?;
    run_with_mounter(cli, mount_dbfs)
}

pub fn run_with_mounter(
    cli: Cli,
    mounter: impl FnOnce(Dbfs, &Path, &[MountOption]) -> Result<(), RunError>,
) -> Result<(), RunError> {
    match cli.command {
        Command::Mount {
            database,
            mountpoint,
            config,
        } => {
            // Validate options before the database is opened or created.
            let options = mount_options_for(&config)?;
            stats::enable_if_requested(config.stats);
            let read_only = options.contains(&MountOption::RO);
            let db = open_database(database, &config, read_only)?;
            let write_buffer = config.write_buffer.unwrap_or(DEFAULT_WRITE_BUFFER);
            mounter(
                Dbfs::with_write_buffer(db, write_buffer)
                    .with_kernel_cache(config.kernel_cache)
                    .with_keep_cache(!config.no_keep_cache),
                &mountpoint,
                &options,
            )
        }
    }
}

/// Opens the database with the SQLite settings the mount asks for. These do not
/// depend on the mount options, so `-o ro` gets the same settings. A read-only mount
/// gets no checkpoint thread and no WAL truncation at unmount, since those write the
/// database file.
fn open_database(
    path: impl AsRef<Path>,
    config: &MountConfig,
    read_only: bool,
) -> Result<Db, RunError> {
    let defaults = OpenOptions::default();
    let options = OpenOptions {
        wal_cap: config
            .wal_cap
            .map_or(defaults.wal_cap, |bytes| bytes as u64),
        background_checkpoints: !read_only,
    };
    let db = Db::open_with(path, options)?;
    if let Some(bytes) = config.mmap_size {
        db.set_mmap_size(bytes as u64)?;
    }
    Ok(db)
}

/// Mounts `fs` and runs the FUSE session on its own thread. The calling thread
/// waits for either a shutdown signal or the end of the session.
///
/// On a signal, dirty data is flushed while the mount is still up, then the mount is
/// detached. A process may still hold a file open on it, so the session is not
/// assumed to end. The session ends when nothing holds the mount, and then
/// `Filesystem::destroy` runs. A flush failure at any point makes this return an error.
fn mount_dbfs(fs: Dbfs, mountpoint: &Path, options: &[MountOption]) -> Result<(), RunError> {
    let shutdown = fs.shutdown_handle();
    let notify = fs.notify_handle();
    let mut session = Session::new(fs, mountpoint, options)
        .map_err(|error| mount_failure(error, options, euid()))?;
    notify.attach(session.notifier());
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
    stats::report();
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

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// The options used when no flags are given: the mount is accessible only to
/// the user who mounted it.
pub fn mount_options() -> Vec<MountOption> {
    vec![
        MountOption::FSName("dbfs".to_string()),
        // Let the kernel enforce file permission checks based on inode mode/uid/gid.
        // Without this, the kernel allows all accesses regardless of file permissions.
        MountOption::DefaultPermissions,
    ]
}

/// Builds the full option list from the command line. The defaults from
/// [`mount_options`] are always included, so `default_permissions` cannot be
/// removed.
pub fn mount_options_for(config: &MountConfig) -> Result<Vec<MountOption>, RunError> {
    let mut requested = Vec::new();
    if config.allow_other {
        requested.push(MountOption::AllowOther);
    }
    if config.allow_root {
        requested.push(MountOption::AllowRoot);
    }
    if config.auto_unmount {
        requested.push(MountOption::AutoUnmount);
    }
    for raw in &config.extra {
        for token in raw.split(',') {
            requested.push(parse_option(token)?);
        }
    }

    let mut options = mount_options();
    for option in requested {
        match option {
            // An explicit fsname or subtype replaces the default instead of
            // adding a second one.
            MountOption::FSName(_) => {
                options.retain(|existing| !matches!(existing, MountOption::FSName(_)));
                options.push(option);
            }
            MountOption::Subtype(_) => {
                options.retain(|existing| !matches!(existing, MountOption::Subtype(_)));
                options.push(option);
            }
            _ if !options.contains(&option) => options.push(option),
            _ => {}
        }
    }

    let allow_other = options.contains(&MountOption::AllowOther);
    let allow_root = options.contains(&MountOption::AllowRoot);
    if allow_other && allow_root {
        return Err(RunError::MountOptions(
            "--allow-other and --allow-root cannot be used together".to_string(),
        ));
    }
    // fusermount3 turns on allow_other for auto-unmount. A plain auto-unmount
    // mount fails for non-root users and would silently widen access, so the
    // user must ask for allow_other or allow_root explicitly.
    if options.contains(&MountOption::AutoUnmount) && !(allow_other || allow_root) {
        return Err(RunError::MountOptions(
            "auto_unmount requires --allow-other or --allow-root \
             (fusermount3 requests allow_other for auto-unmount)"
                .to_string(),
        ));
    }
    Ok(options)
}

fn parse_option(token: &str) -> Result<MountOption, RunError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(RunError::MountOptions("empty mount option".to_string()));
    }
    if token.contains("default_permissions") && token != "default_permissions" {
        return Err(RunError::MountOptions(format!(
            "'{token}' would disable kernel permission checks; \
             default_permissions is always enabled"
        )));
    }

    Ok(match token {
        "ro" => MountOption::RO,
        "rw" => MountOption::RW,
        "allow_other" => MountOption::AllowOther,
        "allow_root" => MountOption::AllowRoot,
        "auto_unmount" => MountOption::AutoUnmount,
        "default_permissions" => MountOption::DefaultPermissions,
        "atime" => MountOption::Atime,
        "noatime" => MountOption::NoAtime,
        "suid" => MountOption::Suid,
        "nosuid" => MountOption::NoSuid,
        "dev" => MountOption::Dev,
        "nodev" => MountOption::NoDev,
        "exec" => MountOption::Exec,
        "noexec" => MountOption::NoExec,
        "dirsync" => MountOption::DirSync,
        "sync" => MountOption::Sync,
        "async" => MountOption::Async,
        _ => match token.split_once('=') {
            Some(("fsname", "")) | Some(("subtype", "")) => {
                return Err(RunError::MountOptions(format!("'{token}' needs a value")));
            }
            Some(("fsname", name)) => MountOption::FSName(name.to_string()),
            Some(("subtype", name)) => MountOption::Subtype(name.to_string()),
            _ => MountOption::CUSTOM(token.to_string()),
        },
    })
}

/// Adds the `user_allow_other` hint when a non-root user requested allow_other
/// and the mount was refused. `euid` is passed in so the check can be tested.
fn mount_failure(error: io::Error, options: &[MountOption], euid: u32) -> RunError {
    let allow_other_refused = euid != 0
        && options.contains(&MountOption::AllowOther)
        && error.kind() == io::ErrorKind::PermissionDenied;
    if allow_other_refused {
        RunError::Mount(io::Error::new(
            error.kind(),
            format!("{}\n{ALLOW_OTHER_HINT}", error.to_string().trim_end()),
        ))
    } else {
        RunError::Mount(error)
    }
}

#[derive(Debug)]
pub enum RunError {
    Db(DbError),
    Mount(std::io::Error),
    /// The FUSE session or one of run's threads failed.
    Session(std::io::Error),
    /// Dirty data could not be written to the database during shutdown.
    ShutdownFlush(i32),
    MountOptions(String),
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
            Self::Db(error @ DbError::DatabaseInUse) => write!(f, "{error}"),
            Self::Db(error) => write!(f, "database error: {error:?}"),
            // fusermount3 output ends with a newline; don't print a blank line.
            Self::Mount(error) => write!(f, "mount error: {}", error.to_string().trim_end()),
            Self::MountOptions(message) => write!(f, "invalid mount options: {message}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn refused() -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fusermount3: option allow_other only allowed if 'user_allow_other' is set in /etc/fuse.conf\n",
        )
    }

    #[test]
    fn non_root_allow_other_refusal_gets_the_fuse_conf_hint() {
        let options = mount_options_for(&MountConfig {
            allow_other: true,
            ..MountConfig::default()
        })
        .expect("options");

        let message = mount_failure(refused(), &options, 1000).to_string();

        assert!(message.contains("user_allow_other"), "{message}");
        assert!(
            message.contains("allow_other requires 'user_allow_other' in /etc/fuse.conf"),
            "{message}"
        );
        assert!(!message.ends_with('\n'), "{message:?}");
    }

    #[test]
    fn root_and_default_mounts_get_no_hint() {
        let allow_other = mount_options_for(&MountConfig {
            allow_other: true,
            ..MountConfig::default()
        })
        .expect("options");
        let default = mount_options();

        assert!(
            !mount_failure(refused(), &allow_other, 0)
                .to_string()
                .contains("hint")
        );
        assert!(
            !mount_failure(refused(), &default, 1000)
                .to_string()
                .contains("hint")
        );
    }

    #[test]
    fn other_mount_errors_get_no_hint() {
        let options = mount_options_for(&MountConfig {
            allow_other: true,
            ..MountConfig::default()
        })
        .expect("options");
        let error = io::Error::new(io::ErrorKind::NotFound, "no such mountpoint");

        assert_eq!(
            mount_failure(error, &options, 1000).to_string(),
            "mount error: no such mountpoint"
        );
    }
}
