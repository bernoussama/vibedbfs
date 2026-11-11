//! Mount-level tests for `--stats` (and `DBFS_STATS=1`): the counters are printed as
//! one JSON line when the mount ends, and nothing is printed without the flag. They
//! need FUSE, so they are ignored by default. Run them with:
//!
//! ```text
//! DBFS_RUN_FUSE_TESTS=1 cargo test --test stats -- --ignored
//! ```
//!
//! Work directories live under the repository's `.bench/` directory. Never use
//! `/tmp`, which is tmpfs on this machine.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const POLL: Duration = Duration::from_millis(50);
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(20);
const MIB: usize = 1024 * 1024;

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn stats_flag_prints_one_line_with_nonzero_write_counters_on_unmount() {
    require_fuse_gate();
    let mut mount = Mount::start("stats-unmount", &["--stats"], None);
    write_and_fsync(&mount.mountpoint.join("data.bin"), MIB);

    mount.unmount();
    let stderr = mount.stderr();

    let stats = only_stats_line(&stderr).expect("a dbfs_stats line on unmount");
    assert_write_counters(&stats);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn stats_line_is_printed_when_dbfs_is_terminated_by_sigterm() {
    require_fuse_gate();
    let mut mount = Mount::start("stats-sigterm", &["--stats"], None);
    write_and_fsync(&mount.mountpoint.join("data.bin"), MIB);

    mount.terminate();
    let stderr = mount.stderr();

    let stats = only_stats_line(&stderr).expect("a dbfs_stats line after SIGTERM");
    assert_write_counters(&stats);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn stats_are_printed_when_enabled_by_the_environment() {
    require_fuse_gate();
    let mut mount = Mount::start("stats-env", &[], Some(("DBFS_STATS", "1")));
    write_and_fsync(&mount.mountpoint.join("data.bin"), MIB);

    mount.unmount();
    let stderr = mount.stderr();

    let stats = only_stats_line(&stderr).expect("a dbfs_stats line with DBFS_STATS=1");
    assert_write_counters(&stats);
}

#[test]
#[ignore = "requires DBFS_RUN_FUSE_TESTS=1 and fusermount3"]
fn nothing_is_printed_without_the_stats_flag() {
    require_fuse_gate();
    let mut mount = Mount::start("stats-off", &[], None);
    write_and_fsync(&mount.mountpoint.join("data.bin"), MIB);

    mount.unmount();
    let stderr = mount.stderr();

    assert!(
        !stderr.contains("dbfs_stats"),
        "stats must stay silent without --stats: {stderr}"
    );
}

/// The counters that a 1 MiB write followed by `fsync` must move. Each one is
/// checked for a non-zero value, so a dead hook fails the test.
fn assert_write_counters(stats: &Json) {
    let nonzero: [&[&str]; 10] = [
        &["ops", "write", "count"],
        &["ops", "create", "count"],
        &["ops", "fsync", "count"],
        &["chunks", "flushed"],
        &["chunks", "bytes_flushed"],
        &["flush_transactions", "count"],
        &["sql", "write", "count"],
        &["sql", "sync", "count"],
        &["write_buffer", "slots_high_water"],
        &["wal", "high_water_bytes"],
    ];
    for path in nonzero {
        let full: Vec<&str> = std::iter::once("dbfs_stats")
            .chain(path.iter().copied())
            .collect();
        assert!(stats.number(&full) > 0, "{full:?} is zero");
    }
    let flushed_bytes = stats.number(&["dbfs_stats", "chunks", "bytes_flushed"]);
    assert!(flushed_bytes >= MIB as u64, "flushed {flushed_bytes} bytes");
    assert!(
        stats.number(&["dbfs_stats", "sql", "write", "total_ns"]) > 0,
        "flush time is recorded"
    );
}

fn write_and_fsync(path: &Path, len: usize) {
    let mut file = File::create(path).expect("create file");
    file.write_all(&vec![7u8; len]).expect("write");
    file.sync_all().expect("fsync");
}

/// The one `{"dbfs_stats": ...}` line in `stderr`, parsed. Panics on a second one.
fn only_stats_line(stderr: &str) -> Option<Json> {
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("{\"dbfs_stats\""))
        .collect();
    assert!(
        lines.len() <= 1,
        "expected one dbfs_stats line, got {lines:?}"
    );
    let line = lines.first()?;
    let json = parse_json(line).unwrap_or_else(|error| panic!("not JSON ({error}): {line}"));
    Some(json)
}

/// A dbfs mount in `.bench/` whose stderr goes to a file. `Drop` unmounts it and
/// removes the directory, but only once the mount is gone.
struct Mount {
    dir: PathBuf,
    mountpoint: PathBuf,
    stderr_path: PathBuf,
    child: Option<Child>,
}

impl Mount {
    fn start(name: &str, extra_args: &[&str], env: Option<(&str, &str)>) -> Self {
        let dir = bench_dir(name);
        let db_path = dir.join("fs.sqlite");
        let mountpoint = dir.join("mnt");
        let stderr_path = dir.join("stderr.txt");
        fs::create_dir(&mountpoint).expect("create mountpoint");

        let mut command = Command::new(env!("CARGO_BIN_EXE_dbfs"));
        command
            .arg("mount")
            .args(extra_args)
            .arg(&db_path)
            .arg(&mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                File::create(&stderr_path).expect("stderr file"),
            ))
            // A stats setting in the test runner's environment must not leak in.
            .env_remove("DBFS_STATS");
        if let Some((key, value)) = env {
            command.env(key, value);
        }
        let child = command.spawn().expect("start dbfs mount");

        let mut mount = Self {
            dir,
            mountpoint,
            stderr_path,
            child: Some(child),
        };
        mount.wait_until_mounted();
        mount
    }

    fn wait_until_mounted(&mut self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        while Instant::now() < deadline {
            if mountinfo_line(&self.mountpoint).is_some() {
                return;
            }
            if let Some(status) = self.try_exit() {
                panic!("dbfs exited before mounting: {status}");
            }
            sleep(POLL);
        }
        panic!("mount did not become ready");
    }

    /// Unmounts with `fusermount3 -u` and waits for dbfs to exit.
    fn unmount(&mut self) {
        let status = Command::new("fusermount3")
            .arg("-u")
            .arg(&self.mountpoint)
            .status()
            .expect("run fusermount3");
        assert!(status.success(), "fusermount3 -u failed");
        self.wait_for_exit();
    }

    /// Sends SIGTERM, the signal `docker stop` sends, and waits for dbfs to exit.
    fn terminate(&mut self) {
        let child = self.child.as_ref().expect("dbfs is running");
        // SAFETY: `kill` only sends a signal to a process id we started.
        let sent = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        assert_eq!(sent, 0, "kill(SIGTERM) failed");
        self.wait_for_exit();
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + EXIT_TIMEOUT;
        loop {
            if let Some(status) = self.try_exit() {
                assert!(status.success(), "dbfs exited with {status}");
                return status;
            }
            assert!(Instant::now() < deadline, "dbfs did not exit in time");
            sleep(POLL);
        }
    }

    fn try_exit(&mut self) -> Option<ExitStatus> {
        let child = self.child.as_mut()?;
        let status = child.try_wait().expect("poll dbfs")?;
        self.child = None;
        Some(status)
    }

    fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr_path).expect("read dbfs stderr")
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if mountinfo_line(&self.mountpoint).is_some() {
            let _ = Command::new("fusermount3")
                .arg("-z")
                .arg("-u")
                .arg(&self.mountpoint)
                .status();
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if mountinfo_line(&self.mountpoint).is_none() {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

fn require_fuse_gate() {
    if std::env::var_os("DBFS_RUN_FUSE_TESTS").is_none() {
        panic!("FUSE mount tests require DBFS_RUN_FUSE_TESTS=1 and fusermount3");
    }
}

/// The `/proc/self/mountinfo` line for `mountpoint`, if it is mounted. The
/// caller passes an absolute path, which matches what the kernel records.
fn mountinfo_line(mountpoint: &Path) -> Option<String> {
    let target = mountpoint.to_str()?;
    fs::read_to_string("/proc/self/mountinfo")
        .ok()?
        .lines()
        .find(|line| line.split_whitespace().nth(4) == Some(target))
        .map(str::to_string)
}

/// A fresh, disk-backed directory under `.bench/`, created with an absolute
/// path so it matches `/proc/self/mountinfo`.
fn bench_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".bench")
        .join(format!("{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create disk-backed test directory");
    dir
}

/// A JSON value, as far as the stats line needs: objects, strings and unsigned
/// integers. Anything else is a parse error, which is what the test checks for.
#[derive(Debug, PartialEq)]
enum Json {
    Object(Vec<(String, Json)>),
    String(String),
    Number(u64),
}

impl Json {
    /// The value at `path`, from the root object down. Panics if a key is missing.
    fn at(&self, path: &[&str]) -> &Json {
        let mut node = self;
        for key in path {
            node = match node {
                Json::Object(fields) => {
                    &fields
                        .iter()
                        .find(|(name, _)| name == key)
                        .unwrap_or_else(|| panic!("no key {key} in {path:?}"))
                        .1
                }
                other => panic!("{key} is inside a non-object: {other:?}"),
            };
        }
        node
    }

    fn number(&self, path: &[&str]) -> u64 {
        match self.at(path) {
            Json::Number(value) => *value,
            other => panic!("{path:?} is not a number: {other:?}"),
        }
    }
}

fn parse_json(text: &str) -> Result<Json, String> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    let value = parser.value()?;
    parser.skip_space();
    if parser.pos != parser.bytes.len() {
        return Err(format!("trailing text at byte {}", parser.pos));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn value(&mut self) -> Result<Json, String> {
        self.skip_space();
        match self.bytes.get(self.pos) {
            Some(b'{') => self.object(),
            Some(b'"') => self.string().map(Json::String),
            Some(b'0'..=b'9') => self.number(),
            other => Err(format!("unexpected {other:?} at byte {}", self.pos)),
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut fields = Vec::new();
        self.skip_space();
        if self.eat(b'}') {
            return Ok(Json::Object(fields));
        }
        loop {
            self.skip_space();
            let key = self.string()?;
            self.skip_space();
            self.expect(b':')?;
            fields.push((key, self.value()?));
            self.skip_space();
            if self.eat(b'}') {
                return Ok(Json::Object(fields));
            }
            self.expect(b',')?;
        }
    }

    /// Our keys and strings have no escapes, so a string is the bytes up to the next quote.
    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let start = self.pos;
        while self.bytes.get(self.pos).is_some_and(|&byte| byte != b'"') {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|error| error.to_string())?
            .to_string();
        self.expect(b'"')?;
        Ok(text)
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        let digits =
            std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|e| e.to_string())?;
        digits
            .parse()
            .map(Json::Number)
            .map_err(|error| format!("{digits}: {error}"))
    }

    fn skip_space(&mut self) {
        while self
            .bytes
            .get(self.pos)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.pos += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.pos) == Some(&byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.eat(byte) {
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", byte as char, self.pos))
        }
    }
}

#[test]
fn json_reader_accepts_the_stats_shape_and_rejects_the_rest() {
    let parsed =
        parse_json(r#"{"dbfs_stats":{"ops":{"write":{"count":2,"total_ns":30}}}}"#).expect("valid");
    assert_eq!(
        parsed.number(&["dbfs_stats", "ops", "write", "total_ns"]),
        30
    );
    assert!(
        parse_json(r#"{"dbfs_stats":{"a":1,}}"#).is_err(),
        "trailing comma"
    );
    assert!(
        parse_json(r#"{"a":-1}"#).is_err(),
        "negative numbers never occur"
    );
    assert!(parse_json(r#"{"a":1} x"#).is_err(), "trailing text");
}
