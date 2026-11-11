use std::path::PathBuf;

use clap::Parser;
use dbfs::{
    Cli, Command, MountConfig, RunError, mount_options, mount_options_for, run_with_mounter,
};
use fuser::MountOption;

fn parse_mount(extra_args: &[&str]) -> MountConfig {
    let mut argv = vec!["dbfs", "mount"];
    argv.extend_from_slice(extra_args);
    argv.extend(["dbfs.sqlite", "mnt"]);
    let cli = Cli::try_parse_from(argv).expect("parse mount command");
    let Command::Mount { config, .. } = cli.command;
    config
}

fn options_error(config: &MountConfig) -> String {
    match mount_options_for(config) {
        Err(RunError::MountOptions(message)) => message,
        other => panic!("expected an invalid-options error, got {other:?}"),
    }
}

#[test]
fn parses_mount_command_with_database_and_mountpoint() {
    let cli =
        Cli::try_parse_from(["dbfs", "mount", "dbfs.sqlite", "mnt"]).expect("parse mount command");

    assert_eq!(
        cli.command,
        Command::Mount {
            database: PathBuf::from("dbfs.sqlite"),
            mountpoint: PathBuf::from("mnt"),
            config: MountConfig::default(),
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
            config: MountConfig::default(),
        },
    };
    let mut called = false;

    run_with_mounter(cli, |fs, received_mountpoint, received_options| {
        called = true;
        assert_eq!(received_mountpoint, mountpoint.as_path());
        assert_eq!(received_options, mount_options().as_slice());
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

#[test]
fn default_mount_options_are_fsname_and_default_permissions_only() {
    assert_eq!(
        mount_options(),
        vec![
            MountOption::FSName("dbfs".to_string()),
            MountOption::DefaultPermissions,
        ]
    );
    assert_eq!(
        mount_options_for(&parse_mount(&[])).expect("default options"),
        mount_options()
    );
}

#[test]
fn allow_other_flag_adds_allow_other() {
    let config = parse_mount(&["--allow-other", "--auto-unmount"]);

    let options = mount_options_for(&config).expect("allow-other options");

    assert!(options.contains(&MountOption::AllowOther));
    assert!(options.contains(&MountOption::AutoUnmount));
    assert!(!options.contains(&MountOption::AllowRoot));
    assert!(options.contains(&MountOption::DefaultPermissions));
}

#[test]
fn allow_root_flag_adds_allow_root() {
    let options = mount_options_for(&parse_mount(&["--allow-root"])).expect("allow-root options");

    assert!(options.contains(&MountOption::AllowRoot));
    assert!(!options.contains(&MountOption::AllowOther));
}

#[test]
fn auto_unmount_flag_adds_auto_unmount_when_paired_with_an_allow_option() {
    let options = mount_options_for(&parse_mount(&["--auto-unmount", "--allow-root"]))
        .expect("auto-unmount with allow-root");

    assert!(options.contains(&MountOption::AutoUnmount));
    assert!(options.contains(&MountOption::AllowRoot));
}

#[test]
fn allow_other_and_allow_root_flags_conflict_at_parse_time() {
    for args in [
        ["--allow-other", "--allow-root"],
        ["--allow-root", "--allow-other"],
    ] {
        let mut argv = vec!["dbfs", "mount"];
        argv.extend_from_slice(&args);
        argv.extend(["dbfs.sqlite", "mnt"]);
        let error = Cli::try_parse_from(argv).expect_err("conflicting flags should fail");

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}

#[test]
fn repeated_and_comma_separated_option_arguments_are_all_applied() {
    let config = parse_mount(&["-o", "ro,noatime", "-o", "context=system_u:object_r:t:s0"]);

    assert_eq!(
        config.extra,
        vec!["ro,noatime", "context=system_u:object_r:t:s0"]
    );
    let options = mount_options_for(&config).expect("options");

    assert!(options.contains(&MountOption::RO));
    assert!(options.contains(&MountOption::NoAtime));
    assert!(options.contains(&MountOption::CUSTOM(
        "context=system_u:object_r:t:s0".to_string()
    )));
}

#[test]
fn known_option_names_map_to_typed_options() {
    let config = parse_mount(&[
        "-o",
        "rw,atime,suid,dev,exec,dirsync,sync,nosuid,nodev,noexec,async,noatime,allow_root",
    ]);

    let options = mount_options_for(&config).expect("known options");

    for expected in [
        MountOption::RW,
        MountOption::Atime,
        MountOption::Suid,
        MountOption::Dev,
        MountOption::Exec,
        MountOption::DirSync,
        MountOption::Sync,
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::NoExec,
        MountOption::Async,
        MountOption::NoAtime,
        MountOption::AllowRoot,
    ] {
        assert!(options.contains(&expected), "missing {expected:?}");
    }
    assert!(
        !options
            .iter()
            .any(|option| matches!(option, MountOption::CUSTOM(_)))
    );
}

#[test]
fn unknown_options_are_passed_through_as_custom() {
    let options = mount_options_for(&parse_mount(&["-o", "foo,context=a=b"]))
        .expect("unknown options are allowed");

    assert!(options.contains(&MountOption::CUSTOM("foo".to_string())));
    assert!(options.contains(&MountOption::CUSTOM("context=a=b".to_string())));
}

#[test]
fn fsname_option_replaces_the_default_name() {
    let options = mount_options_for(&parse_mount(&["-o", "fsname=vol1"])).expect("fsname");

    let names: Vec<_> = options
        .iter()
        .filter(|option| matches!(option, MountOption::FSName(_)))
        .collect();
    assert_eq!(names, vec![&MountOption::FSName("vol1".to_string())]);
}

#[test]
fn repeated_options_are_not_duplicated() {
    let options = mount_options_for(&parse_mount(&["--allow-other", "-o", "allow_other,ro,ro"]))
        .expect("duplicate options");

    assert_eq!(
        options
            .iter()
            .filter(|o| **o == MountOption::AllowOther)
            .count(),
        1
    );
    assert_eq!(options.iter().filter(|o| **o == MountOption::RO).count(), 1);
}

#[test]
fn default_permissions_cannot_be_removed() {
    let options = mount_options_for(&parse_mount(&["-o", "default_permissions"]))
        .expect("restating the default is allowed");

    assert_eq!(
        options
            .iter()
            .filter(|option| **option == MountOption::DefaultPermissions)
            .count(),
        1
    );

    for token in [
        "no_default_permissions",
        "nodefault_permissions",
        "default_permissions=0",
        "default_permissions=off",
    ] {
        let message = options_error(&parse_mount(&["-o", token]));
        assert!(
            message.contains("default_permissions"),
            "{token}: {message}"
        );
    }
}

#[test]
fn allow_other_and_allow_root_cannot_be_combined_through_option_arguments() {
    let message = options_error(&parse_mount(&["-o", "allow_other,allow_root"]));
    assert!(message.contains("cannot be used together"), "{message}");

    let message = options_error(&parse_mount(&["--allow-other", "-o", "allow_root"]));
    assert!(message.contains("cannot be used together"), "{message}");
}

#[test]
fn auto_unmount_requires_an_explicit_allow_option() {
    let message = options_error(&parse_mount(&["--auto-unmount"]));
    assert!(
        message.contains("--allow-other or --allow-root"),
        "{message}"
    );

    let message = options_error(&parse_mount(&["-o", "auto_unmount"]));
    assert!(
        message.contains("--allow-other or --allow-root"),
        "{message}"
    );
}

#[test]
fn empty_and_valueless_options_are_rejected() {
    assert!(options_error(&parse_mount(&["-o", ""])).contains("empty"));
    assert!(options_error(&parse_mount(&["-o", "ro,,noatime"])).contains("empty"));
    assert!(options_error(&parse_mount(&["-o", "fsname="])).contains("needs a value"));
}

#[test]
fn run_rejects_invalid_options_before_creating_the_database() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "dbfs-invalid-options-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let cli = Cli {
        command: Command::Mount {
            database: path.clone(),
            mountpoint: PathBuf::from("mnt"),
            config: MountConfig {
                auto_unmount: true,
                ..MountConfig::default()
            },
        },
    };

    let error = run_with_mounter(cli, |_, _, _| panic!("mounter must not run"))
        .expect_err("auto-unmount without an allow option should fail");

    assert!(matches!(error, RunError::MountOptions(_)));
    assert!(!path.exists(), "database was created for invalid options");
}

#[test]
fn run_passes_allow_other_to_the_mounter() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dbfs-allow-other-{}.sqlite", std::process::id()));
    let cli = Cli {
        command: Command::Mount {
            database: path.clone(),
            mountpoint: PathBuf::from("mnt"),
            config: MountConfig {
                allow_other: true,
                ..MountConfig::default()
            },
        },
    };
    let mut seen = Vec::new();

    run_with_mounter(cli, |_, _, options| {
        seen = options.to_vec();
        Ok(())
    })
    .expect("run mount command");

    assert!(seen.contains(&MountOption::AllowOther));
    assert!(seen.contains(&MountOption::DefaultPermissions));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

#[test]
fn write_buffer_accepts_sizes_with_suffixes() {
    assert_eq!(parse_mount(&[]).write_buffer, None);
    assert_eq!(
        parse_mount(&["--write-buffer", "8M"]).write_buffer,
        Some(8 << 20)
    );
    assert_eq!(
        parse_mount(&["--write-buffer", "512k"]).write_buffer,
        Some(512 << 10)
    );
    assert_eq!(
        parse_mount(&["--write-buffer", "1G"]).write_buffer,
        Some(1 << 30)
    );
    assert_eq!(
        parse_mount(&["--write-buffer", "65536"]).write_buffer,
        Some(65_536)
    );
}

#[test]
fn write_buffer_rejects_bad_sizes() {
    for bad in ["", "8X", "M", "1K", "-5M", "99999999999999999999G"] {
        let argv = ["dbfs", "mount", "--write-buffer", bad, "dbfs.sqlite", "mnt"];
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "`{bad}` should be rejected"
        );
    }
}

#[test]
fn kernel_cache_is_off_unless_requested() {
    assert!(!parse_mount(&[]).kernel_cache);
    assert!(parse_mount(&["--kernel-cache"]).kernel_cache);
}

#[test]
fn keep_cache_is_on_unless_no_keep_cache_is_requested() {
    assert!(!parse_mount(&[]).no_keep_cache);
    assert!(parse_mount(&["--no-keep-cache"]).no_keep_cache);
}

#[test]
fn no_keep_cache_conflicts_with_kernel_cache() {
    for argv in [
        [
            "dbfs",
            "mount",
            "--no-keep-cache",
            "--kernel-cache",
            "dbfs.sqlite",
            "mnt",
        ],
        [
            "dbfs",
            "mount",
            "--kernel-cache",
            "--no-keep-cache",
            "dbfs.sqlite",
            "mnt",
        ],
    ] {
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "{argv:?} should conflict"
        );
    }
}

#[test]
fn mmap_size_accepts_zero_and_the_write_buffer_suffixes() {
    assert_eq!(parse_mount(&[]).mmap_size, None);
    assert_eq!(parse_mount(&["--mmap-size", "0"]).mmap_size, Some(0));
    assert_eq!(
        parse_mount(&["--mmap-size", "512M"]).mmap_size,
        Some(512 << 20)
    );
    assert_eq!(parse_mount(&["--mmap-size", "1G"]).mmap_size, Some(1 << 30));
}

#[test]
fn mmap_size_rejects_bad_sizes() {
    for bad in ["", "2X", "M", "-1", "99999999999999999999G"] {
        let argv = ["dbfs", "mount", "--mmap-size", bad, "dbfs.sqlite", "mnt"];
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "`{bad}` should be rejected"
        );
    }
}

#[test]
fn parses_wal_cap_with_a_size_suffix() {
    assert_eq!(parse_mount(&["--wal-cap", "512K"]).wal_cap, Some(512 << 10));
    assert_eq!(parse_mount(&["--wal-cap", "1G"]).wal_cap, Some(1 << 30));
    assert_eq!(parse_mount(&[]).wal_cap, None);
}

#[test]
fn rejects_wal_cap_below_one_chunk() {
    let error = Cli::try_parse_from(["dbfs", "mount", "--wal-cap", "1K", "dbfs.sqlite", "mnt"])
        .expect_err("a 1K WAL cap should be refused");

    assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
}

#[test]
fn sync_metadata_is_off_unless_requested() {
    assert!(!parse_mount(&[]).sync_metadata);
    assert!(parse_mount(&["--sync-metadata"]).sync_metadata);
}
