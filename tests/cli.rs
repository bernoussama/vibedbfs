use std::path::PathBuf;

use clap::Parser;
use dbfs::{Cli, Command};

#[test]
fn parses_mount_command_with_database_and_mountpoint() {
    let cli =
        Cli::try_parse_from(["dbfs", "mount", "dbfs.sqlite", "mnt"]).expect("parse mount command");

    assert_eq!(
        cli.command,
        Command::Mount {
            database: PathBuf::from("dbfs.sqlite"),
            mountpoint: PathBuf::from("mnt"),
        }
    );
}

#[test]
fn rejects_mount_command_without_mountpoint() {
    let error = Cli::try_parse_from(["dbfs", "mount", "dbfs.sqlite"])
        .expect_err("missing mountpoint should fail");

    assert_eq!(
        error.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
}
