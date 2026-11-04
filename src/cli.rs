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

    /// Memory for buffered writes, shared by all open files (default 64M).
    ///
    /// Takes a byte count with an optional K, M or G suffix (powers of 1024),
    /// for example `8M` in a small container. The memory is reserved as it is
    /// used, never up front. When it is full, the file with the most buffered
    /// data is written to the database. Minimum 64K.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub write_buffer: Option<usize>,
}

/// Parses `N`, `NK`, `NM` or `NG` (powers of 1024), at least 64 KiB.
fn parse_size(text: &str) -> Result<usize, String> {
    let text = text.trim();
    let (digits, shift) = match text.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&text[..text.len() - 1], 10),
        Some('M') => (&text[..text.len() - 1], 20),
        Some('G') => (&text[..text.len() - 1], 30),
        _ => (text, 0),
    };
    let value: usize = digits
        .parse()
        .map_err(|_| format!("`{text}` is not a size such as 65536, 512K, 8M or 1G"))?;
    let bytes = value
        .checked_mul(1 << shift)
        .ok_or_else(|| format!("`{text}` is too large"))?;
    if bytes < 64 * 1024 {
        return Err(format!("`{text}` is below the 64K minimum (one chunk)"));
    }
    Ok(bytes)
}
