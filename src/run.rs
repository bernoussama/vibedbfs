use std::fmt;
use std::io;
use std::path::Path;

use fuser::MountOption;

use crate::{Cli, Command, Db, DbError, Dbfs, MountConfig};

const ALLOW_OTHER_HINT: &str = "hint: allow_other requires 'user_allow_other' in /etc/fuse.conf";

pub fn run(cli: Cli) -> Result<(), RunError> {
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
            let db = Db::open(database)?;
            mounter(Dbfs::new(db), &mountpoint, &options)
        }
    }
}

fn mount_dbfs(fs: Dbfs, mountpoint: &Path, options: &[MountOption]) -> Result<(), RunError> {
    fuser::mount2(fs, mountpoint, options).map_err(|error| mount_failure(error, options, euid()))
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
            Self::Db(error) => write!(f, "database error: {error:?}"),
            // fusermount3 output ends with a newline; don't print a blank line.
            Self::Mount(error) => write!(f, "mount error: {}", error.to_string().trim_end()),
            Self::MountOptions(message) => write!(f, "invalid mount options: {message}"),
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
