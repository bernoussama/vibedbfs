# AGENTS.md

## Cursor Cloud specific instructions

**dbfs** is a single-crate Rust project — a FUSE filesystem backed by SQLite. No network services, no Docker, no monorepo complexity.

### System dependencies (pre-installed on the Cloud VM)

- Rust stable ≥1.85 (edition 2024); managed via `rustup`
- `libfuse3-dev`, `fuse3` (provides `fusermount3`)
- `libsqlite3-dev` (for the `rusqlite` crate's system linkage)
- `pkg-config`
- Python 3 + `pytest` (for the libfuse-adapted test harness)

### Build & test commands

See `README.md` for full details. Quick reference:

| Task | Command |
|---|---|
| Build (dev) | `cargo build` |
| Build (release) | `cargo build --release` |
| Unit/integration tests | `cargo test` |
| FUSE mount integration | `DBFS_RUN_FUSE_TESTS=1 cargo test --test mount_integration` |
| Python/libfuse tests | `pytest tests/test_with_libfuse.py -v -k "not syscall"` |
| Run (mount) | `cargo run -- mount <db_path> <mount_dir>` |
| Unmount | `fusermount3 -u <mount_dir>` |

### Non-obvious notes

- The Cargo.toml uses `edition = "2024"` which requires Rust 1.85+. The VM's default Rust may be older; the update script handles `rustup update stable && rustup default stable`.
- `rusqlite` links against the system `libsqlite3`; without `libsqlite3-dev`, the link step fails with `-lsqlite3` not found.
- FUSE integration tests and the pytest harness require `/dev/fuse` and `fusermount3`. Both are available in Cloud VMs.
- The pytest `TestSyscalls` class requires a libfuse source clone at `../../clones/libfuse` and `gcc`; this is optional and can be skipped with `-k "not syscall"`.
- The release binary is required for pytest tests (it looks for `target/release/dbfs`). Run `cargo build --release` before running pytest.
- When mounting dbfs for manual testing, remember to unmount with `fusermount3 -u <mnt>` or `fusermount3 -z -u <mnt>` (lazy unmount) before cleaning up.
- Do not use `/tmp` for filesystem benchmarks. On this machine `/tmp` is `tmpfs`
  and measures RAM-backed storage, not disk. Use a disk-backed directory under
  the repository, such as `.bench/`, unless the user explicitly asks for tmpfs.
