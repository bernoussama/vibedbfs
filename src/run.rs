use std::fmt;
use std::path::Path;

use fuser::MountOption;

use crate::{Cli, Command, Db, DbError, Dbfs};

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

fn mount_dbfs(fs: Dbfs, mountpoint: &Path) -> Result<(), RunError> {
    fuser::mount2(fs, mountpoint, &mount_options()).map_err(RunError::Mount)
}

pub fn mount_options() -> Vec<MountOption> {
    vec![MountOption::FSName("dbfs".to_string())]
}

#[derive(Debug)]
pub enum RunError {
    Db(DbError),
    Mount(std::io::Error),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Db(error) => write!(f, "database error: {error:?}"),
            Self::Mount(error) => write!(f, "mount error: {error}"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<DbError> for RunError {
    fn from(error: DbError) -> Self {
        Self::Db(error)
    }
}
