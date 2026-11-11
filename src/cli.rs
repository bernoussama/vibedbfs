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

    /// Memory for buffered writes, shared by all open files (default 256M).
    ///
    /// Takes a byte count with an optional K, M or G suffix (powers of 1024),
    /// for example `8M` in a small container. The memory is reserved as it is
    /// used, never up front, and is kept until unmount. When it is full, up to
    /// 16 MiB of the file with the most buffered data is written to the database,
    /// and the rest of that file stays buffered. Minimum 64K.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub write_buffer: Option<usize>,

    /// Let the kernel cache more: writeback cache, parallel directory
    /// operations, 1 MiB requests, cached symlinks and async direct I/O.
    ///
    /// With the writeback cache, written data can sit in the kernel's page cache
    /// until the file is closed or synced, so a crash or `docker stop` with files
    /// still open can lose data that dbfs never saw. Off by default. Conflicts with
    /// `--no-keep-cache`.
    #[arg(long, conflicts_with = "no_keep_cache")]
    pub kernel_cache: bool,

    /// Restore the caching of earlier releases: the kernel drops a file's cached
    /// data on every open, and attributes and names expire after one second.
    ///
    /// Repeated reads then go through FUSE and SQLite again. By default the
    /// kernel keeps the page cache across opens and caches attributes and names
    /// for a day, which is safe because dbfs is the only writer of its database.
    /// Conflicts with `--kernel-cache`.
    #[arg(long, conflicts_with = "kernel_cache")]
    pub no_keep_cache: bool,

    /// Largest part of the database file that SQLite may map into memory
    /// (`PRAGMA mmap_size`). Takes the same sizes as `--write-buffer`, but `0` is
    /// allowed and turns the map off. Default 2G on 64-bit systems, 0 on 32-bit.
    ///
    /// Random reads are faster with the map. An I/O error on the database file
    /// can then end dbfs with SIGBUS instead of returning EIO; `--mmap-size 0`
    /// avoids that.
    #[arg(long, value_name = "SIZE", value_parser = parse_bytes)]
    pub mmap_size: Option<usize>,

    /// Largest the write-ahead log may grow before a writer checkpoints it itself
    /// (default 1G). Normally a background thread checkpoints the WAL, so writers
    /// do not wait for it. Past this size, the writer that finds it checkpoints it
    /// and waits, so the WAL stays bounded. Takes the same sizes as `--write-buffer`.
    /// Minimum 64K. Read-only mounts do not checkpoint.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub wal_cap: Option<usize>,
}

/// Parses `N`, `NK`, `NM` or `NG` (powers of 1024), at least 64 KiB.
fn parse_size(text: &str) -> Result<usize, String> {
    let bytes = parse_bytes(text)?;
    if bytes < 64 * 1024 {
        return Err(format!("`{text}` is below the 64K minimum (one chunk)"));
    }
    Ok(bytes)
}

/// Parses `N`, `NK`, `NM` or `NG` (powers of 1024) with no minimum, so `0` is valid.
fn parse_bytes(text: &str) -> Result<usize, String> {
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
    value
        .checked_mul(1 << shift)
        .ok_or_else(|| format!("`{text}` is too large"))
}
