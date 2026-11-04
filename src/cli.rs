use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(name = "dbfs")]
#[command(about = "Mount a SQLite-backed filesystem")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Mount a database-backed filesystem.
    Mount {
        /// SQLite database file that stores the filesystem.
        database: PathBuf,
        /// Directory to mount the filesystem on.
        mountpoint: PathBuf,
        #[command(flatten)]
        config: MountConfig,
    },
}

/// Opt-in mount options. With no flags, the mount is accessible only to the
/// user who mounted it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
pub struct MountConfig {
    /// Allow every user to access the mount, including root and the Docker daemon.
    ///
    /// Needed to bind-mount dbfs into containers. When dbfs runs as a non-root
    /// user, /etc/fuse.conf must contain `user_allow_other`. Permission bits
    /// are still enforced because `default_permissions` stays on.
    #[arg(long, conflicts_with = "allow_root")]
    pub allow_other: bool,

    /// Allow root to access the mount, in addition to the user who mounted it.
    #[arg(long, conflicts_with = "allow_other")]
    pub allow_root: bool,

    /// Unmount automatically when dbfs exits, including after a crash or SIGKILL.
    ///
    /// Requires fusermount3. Combine with --allow-other or --allow-root.
    #[arg(long)]
    pub auto_unmount: bool,

    /// Extra mount options as a comma-separated list, for example `-o ro,noatime`.
    ///
    /// Repeatable. Known names map to typed FUSE options. Any other option is
    /// passed to fusermount3, which accepts only the options it knows, such as
    /// `max_read=N`. An unknown option makes the mount fail.
    #[arg(short = 'o', long = "option", value_name = "OPT[,OPT...]")]
    pub extra: Vec<String>,
}
