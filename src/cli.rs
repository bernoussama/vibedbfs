use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(name = "dbfs")]
#[command(about = "Mount a SQLite-backed filesystem")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    Mount {
        database: PathBuf,
        mountpoint: PathBuf,
    },
}
