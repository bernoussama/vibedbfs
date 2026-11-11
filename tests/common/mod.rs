//! Helpers shared by the test files that mount dbfs. Each test file includes this
//! module with `mod common;`.

// Each test file uses only part of this module.
#![allow(dead_code)]

/// Cache modes. They conflict with each other on the command line.
const CACHE_MODES: [&str; 2] = ["--kernel-cache", "--no-keep-cache"];

/// The arguments for a `dbfs mount` that a test runs, in addition to its own.
///
/// `DBFS_TEST_MOUNT_ARGS` holds extra arguments, separated by spaces, that go on
/// every mount. That runs the whole mount suite in another mode, for example
/// `DBFS_TEST_MOUNT_ARGS="--no-keep-cache"` or `DBFS_TEST_MOUNT_ARGS="--kernel-cache"`.
///
/// `explicit` holds the arguments the test passes itself. An environment argument
/// that `explicit` already has is not repeated. A test that asks for a cache mode
/// gets that mode, so the environment's conflicting mode is left out.
pub fn mount_args(explicit: &[&str]) -> Vec<String> {
    let env = std::env::var("DBFS_TEST_MOUNT_ARGS").unwrap_or_default();
    let explicit_mode = explicit.iter().any(|arg| CACHE_MODES.contains(arg));
    env.split_whitespace()
        .filter(|arg| !explicit.contains(arg))
        .filter(|arg| !(explicit_mode && CACHE_MODES.contains(arg)))
        .map(str::to_string)
        .collect()
}

/// Whether the page cache survives an open in the mount that `mount_args(&[])`
/// describes. It does unless `--no-keep-cache` is among the mount arguments.
pub fn keeps_page_cache() -> bool {
    !mount_args(&[]).iter().any(|arg| arg == "--no-keep-cache")
}
