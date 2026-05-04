use std::path::PathBuf;

use clap::Parser;
use dbfs::{Cli, Command, run_with_mounter};

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

#[test]
fn run_mount_opens_database_and_invokes_mounter() {
    let path = std::env::temp_dir().join(format!(
        "dbfs-run-mount-{}-{}.sqlite",
        std::process::id(),
        1
    ));
    let mountpoint = PathBuf::from("mnt");
    let cli = Cli {
        command: Command::Mount {
            database: path.clone(),
            mountpoint: mountpoint.clone(),
        },
    };
    let mut called = false;

    run_with_mounter(cli, |fs, received_mountpoint| {
        called = true;
        assert_eq!(received_mountpoint, mountpoint.as_path());
        assert_eq!(fs.getattr(1).expect("root attr").ino, 1);
        Ok(())
    })
    .expect("run mount command");

    assert!(called);
    assert!(path.exists());

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}
