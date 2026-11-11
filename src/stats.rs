//! Built-in counters for `dbfs mount --stats` (or `DBFS_STATS=1`).
//!
//! All counters live in one process-wide [`Stats`]. The hooks in the other
//! modules are either a guard ([`op`], [`sql`], [`flush_transaction`]) or one
//! call (the counters). While stats are off, every hook is one relaxed load of
//! `ENABLED`: no clock is read and nothing is written. While they are on, a guard
//! reads the clock when it is made and adds the elapsed time when it is dropped.
//!
//! When the mount ends, [`report`] prints one line to stderr,
//! `{"dbfs_stats": {...}}`. The keys are documented in `docs/architecture.md`.

use std::cell::Cell;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);
static STATS: Stats = Stats::new();
/// `<database>-wal`, set by [`database_opened`] when stats are on.
static WAL_PATH: OnceLock<PathBuf> = OnceLock::new();

thread_local! {
    /// True while a `Db` method runs on this thread. A method that calls another
    /// one is timed once, by the outer call, so the SQL groups do not overlap.
    static IN_DB_CALL: Cell<bool> = const { Cell::new(false) };
}

/// FUSE operations with their own counters. `Other` collects the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Lookup,
    Getattr,
    Setattr,
    Read,
    Write,
    Create,
    Mknod,
    Mkdir,
    Unlink,
    Rmdir,
    Rename,
    Link,
    Open,
    Release,
    Flush,
    Fsync,
    Fsyncdir,
    Readdir,
    Statfs,
    Getxattr,
    Other,
}

impl Op {
    /// Every variant, in declaration order. `Stats::ops` is indexed by `op as usize`.
    const ALL: [Op; 21] = [
        Op::Lookup,
        Op::Getattr,
        Op::Setattr,
        Op::Read,
        Op::Write,
        Op::Create,
        Op::Mknod,
        Op::Mkdir,
        Op::Unlink,
        Op::Rmdir,
        Op::Rename,
        Op::Link,
        Op::Open,
        Op::Release,
        Op::Flush,
        Op::Fsync,
        Op::Fsyncdir,
        Op::Readdir,
        Op::Statfs,
        Op::Getxattr,
        Op::Other,
    ];

    fn name(self) -> &'static str {
        match self {
            Op::Lookup => "lookup",
            Op::Getattr => "getattr",
            Op::Setattr => "setattr",
            Op::Read => "read",
            Op::Write => "write",
            Op::Create => "create",
            Op::Mknod => "mknod",
            Op::Mkdir => "mkdir",
            Op::Unlink => "unlink",
            Op::Rmdir => "rmdir",
            Op::Rename => "rename",
            Op::Link => "link",
            Op::Open => "open",
            Op::Release => "release",
            Op::Flush => "flush",
            Op::Fsync => "fsync",
            Op::Fsyncdir => "fsyncdir",
            Op::Readdir => "readdir",
            Op::Statfs => "statfs",
            Op::Getxattr => "getxattr",
            Op::Other => "other",
        }
    }
}

/// Groups of SQLite time, by the kind of `Db` method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SqlGroup {
    /// File data reads.
    Read,
    /// Chunk writes and deletes: flush transactions, truncate, inode deletion.
    Write,
    /// Everything else: lookups, directory and inode changes.
    Metadata,
    /// The durable sync of the WAL (`fdatasync`).
    Sync,
}

impl SqlGroup {
    /// Every variant, in declaration order. `Stats::sql` is indexed by `group as usize`.
    const ALL: [SqlGroup; 4] = [
        SqlGroup::Read,
        SqlGroup::Write,
        SqlGroup::Metadata,
        SqlGroup::Sync,
    ];

    fn name(self) -> &'static str {
        match self {
            SqlGroup::Read => "read",
            SqlGroup::Write => "write",
            SqlGroup::Metadata => "metadata",
            SqlGroup::Sync => "sync",
        }
    }
}

/// A count and a total time, both atomic.
struct Timed {
    count: AtomicU64,
    total_ns: AtomicU64,
}

impl Timed {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
        }
    }

    fn add(&self, elapsed: Duration) {
        self.count.fetch_add(1, Relaxed);
        let ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        self.total_ns.fetch_add(ns, Relaxed);
    }

    fn json(&self) -> String {
        object([
            ("count", load(&self.count)),
            ("total_ns", load(&self.total_ns)),
        ])
    }
}

/// Every counter. The process has one, `STATS`; tests build their own.
struct Stats {
    ops: [Timed; 21],
    sql: [Timed; 4],
    /// Flush transactions: one `Db::replace_chunks` call each.
    flush: Timed,
    checkpoints: Timed,
    checkpoint_errors: AtomicU64,
    batch_commits: Timed,
    batch_ops: AtomicU64,
    batch_failures: AtomicU64,
    chunks_flushed: AtomicU64,
    bytes_flushed: AtomicU64,
    reads_before_write: AtomicU64,
    arena_slots_high_water: AtomicU64,
    evictions: AtomicU64,
    evicted_chunks: AtomicU64,
    wal_high_water_bytes: AtomicU64,
}

impl Stats {
    const fn new() -> Self {
        Self {
            ops: [const { Timed::new() }; 21],
            sql: [const { Timed::new() }; 4],
            flush: Timed::new(),
            checkpoints: Timed::new(),
            checkpoint_errors: AtomicU64::new(0),
            batch_commits: Timed::new(),
            batch_ops: AtomicU64::new(0),
            batch_failures: AtomicU64::new(0),
            chunks_flushed: AtomicU64::new(0),
            bytes_flushed: AtomicU64::new(0),
            reads_before_write: AtomicU64::new(0),
            arena_slots_high_water: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            evicted_chunks: AtomicU64::new(0),
            wal_high_water_bytes: AtomicU64::new(0),
        }
    }

    fn record_checkpoint(&self, duration: Duration, ok: bool) {
        self.checkpoints.add(duration);
        if !ok {
            self.checkpoint_errors.fetch_add(1, Relaxed);
        }
    }

    fn record_batch_commit(&self, ops: u64, duration: Duration) {
        self.batch_commits.add(duration);
        self.batch_ops.fetch_add(ops, Relaxed);
    }

    fn record_batch_failure(&self) {
        self.batch_failures.fetch_add(1, Relaxed);
    }

    fn record_eviction(&self, chunks: usize) {
        self.evictions.fetch_add(1, Relaxed);
        self.evicted_chunks.fetch_add(chunks as u64, Relaxed);
    }

    /// The counters as one line of JSON, without a newline.
    fn to_json(&self) -> String {
        let ops = Op::ALL
            .iter()
            .map(|&op| (op.name(), self.ops[op as usize].json()));
        let sql = SqlGroup::ALL
            .iter()
            .map(|&group| (group.name(), self.sql[group as usize].json()));
        let chunks = object([
            ("flushed", load(&self.chunks_flushed)),
            ("bytes_flushed", load(&self.bytes_flushed)),
            ("reads_before_write", load(&self.reads_before_write)),
        ]);
        let write_buffer = object([
            ("slots_high_water", load(&self.arena_slots_high_water)),
            ("evictions", load(&self.evictions)),
            ("evicted_chunks", load(&self.evicted_chunks)),
        ]);
        let checkpoints = object([
            ("count", load(&self.checkpoints.count)),
            ("total_ns", load(&self.checkpoints.total_ns)),
            ("errors", load(&self.checkpoint_errors)),
        ]);
        let batch_commits = object([
            ("count", load(&self.batch_commits.count)),
            ("total_ns", load(&self.batch_commits.total_ns)),
            ("ops", load(&self.batch_ops)),
        ]);
        let body = object([
            ("ops", object(ops)),
            ("sql", object(sql)),
            ("flush_transactions", self.flush.json()),
            ("chunks", chunks),
            ("write_buffer", write_buffer),
            (
                "wal",
                object([("high_water_bytes", load(&self.wal_high_water_bytes))]),
            ),
            ("checkpoints", checkpoints),
            ("batch_commits", batch_commits),
            ("batch_failures", load(&self.batch_failures)),
        ]);
        format!("{{\"dbfs_stats\":{body}}}")
    }
}

fn load(counter: &AtomicU64) -> String {
    counter.load(Relaxed).to_string()
}

/// `{"key":value,...}` from values that are already encoded as JSON.
fn object(fields: impl IntoIterator<Item = (&'static str, String)>) -> String {
    let body: Vec<String> = fields
        .into_iter()
        .map(|(key, value)| format!("\"{key}\":{value}"))
        .collect();
    format!("{{{}}}", body.join(","))
}

/// Times one operation. The time is added to its [`Timed`] when the timer drops.
#[must_use = "a timer records when it is dropped"]
pub(crate) struct Timer<'a> {
    running: Option<(&'a Timed, Instant)>,
    /// The outermost `Db` call on this thread. It clears `IN_DB_CALL` when it ends.
    db_call: bool,
}

impl<'a> Timer<'a> {
    /// Starts the clock only when there is a counter to record into.
    fn start(timed: Option<&'a Timed>) -> Self {
        Self {
            running: timed.map(|timed| (timed, Instant::now())),
            db_call: false,
        }
    }
}

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        if let Some((timed, start)) = self.running.take() {
            timed.add(start.elapsed());
        }
        if self.db_call {
            IN_DB_CALL.set(false);
        }
    }
}

/// The stats, if they are on. This is the only check a hook makes while they are off.
#[inline]
fn enabled() -> Option<&'static Stats> {
    ENABLED.load(Relaxed).then_some(&STATS)
}

/// Turns stats on for `--stats` or `DBFS_STATS=1`. Called before the database is
/// opened, so [`database_opened`] sees it.
pub(crate) fn enable_if_requested(flag: bool) {
    let from_env = std::env::var_os("DBFS_STATS").is_some_and(|value| value == "1");
    if flag || from_env {
        ENABLED.store(true, Relaxed);
    }
}

/// Times a FUSE operation. Bind the result to a named variable: `let _op = op(..)`.
pub(crate) fn op(op: Op) -> Timer<'static> {
    Timer::start(enabled().map(|stats| &stats.ops[op as usize]))
}

/// Times a `Db` method in `group`. A call made while another `Db` method runs on
/// this thread is not timed: the outer call already covers it.
pub(crate) fn sql(group: SqlGroup) -> Timer<'static> {
    match enabled() {
        Some(stats) => sql_in(stats, group),
        None => Timer::start(None),
    }
}

fn sql_in(stats: &Stats, group: SqlGroup) -> Timer<'_> {
    if IN_DB_CALL.replace(true) {
        return Timer::start(None);
    }
    Timer {
        running: Some((&stats.sql[group as usize], Instant::now())),
        db_call: true,
    }
}

/// Times one flush transaction: a `Db::replace_chunks` call.
pub(crate) fn flush_transaction() -> Timer<'static> {
    Timer::start(enabled().map(|stats| &stats.flush))
}

/// A chunk was read so that a partial write could be merged into it.
pub(crate) fn read_before_write() {
    if let Some(stats) = enabled() {
        stats.reads_before_write.fetch_add(1, Relaxed);
    }
}

/// A chunk of `bytes` bytes was written by a flush.
pub(crate) fn chunk_flushed(bytes: usize) {
    if let Some(stats) = enabled() {
        stats.chunks_flushed.fetch_add(1, Relaxed);
        stats.bytes_flushed.fetch_add(bytes as u64, Relaxed);
    }
}

/// The write buffer holds `slots` slots in use. Only the high-water mark is kept.
pub(crate) fn arena_slots_in_use(slots: usize) {
    if let Some(stats) = enabled() {
        stats
            .arena_slots_high_water
            .fetch_max(slots as u64, Relaxed);
    }
}

/// A full write buffer forced a flush of `chunks` chunks from one file (an eviction).
pub(crate) fn eviction(chunks: usize) {
    if let Some(stats) = enabled() {
        stats.record_eviction(chunks);
    }
}

/// A database was opened at `path`. Remembers its WAL file for size samples.
pub(crate) fn database_opened(path: &Path) {
    if enabled().is_none() || path.as_os_str().is_empty() || path == Path::new(":memory:") {
        return;
    }
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let _ = WAL_PATH.set(PathBuf::from(wal));
}

/// Records the current size of the WAL file as a high-water mark. Called after each
/// flush transaction and when the mount ends.
pub(crate) fn sample_wal() {
    let (Some(stats), Some(path)) = (enabled(), WAL_PATH.get()) else {
        return;
    };
    if let Ok(metadata) = std::fs::metadata(path) {
        stats
            .wal_high_water_bytes
            .fetch_max(metadata.len(), Relaxed);
    }
}

/// A checkpoint ran for `duration`. `ok` is false when it failed.
///
/// Hook for the background checkpointer (not built yet): call it once per run.
#[allow(dead_code)]
pub(crate) fn checkpoint_done(duration: Duration, ok: bool) {
    if let Some(stats) = enabled() {
        stats.record_checkpoint(duration, ok);
    }
}

/// A group commit of `ops` operations took `duration`.
pub(crate) fn batch_commit(ops: u64, duration: Duration) {
    if let Some(stats) = enabled() {
        stats.record_batch_commit(ops, duration);
    }
}

/// A batch failed and writes are now refused.
pub(crate) fn batch_failed() {
    if let Some(stats) = enabled() {
        stats.record_batch_failure();
    }
}

/// Prints the counters to stderr as one JSON line. Does nothing unless stats are on.
pub(crate) fn report() {
    if let Some(stats) = enabled() {
        sample_wal();
        // A closed stderr must not stop the unmount, so the write error is ignored.
        let _ = writeln!(std::io::stderr(), "{}", stats.to_json());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // No unit test turns stats on, so the global hooks stay inert here. Tests that
    // check recording use their own `Stats`.

    fn counts(timed: &Timed) -> (u64, u64) {
        (timed.count.load(Relaxed), timed.total_ns.load(Relaxed))
    }

    #[test]
    fn hooks_are_inert_while_stats_are_off() {
        assert!(enabled().is_none());
        assert!(op(Op::Read).running.is_none());
        assert!(sql(SqlGroup::Write).running.is_none());
        assert!(flush_transaction().running.is_none());
    }

    #[test]
    fn enum_tables_match_their_indexes() {
        for (index, op) in Op::ALL.iter().enumerate() {
            assert_eq!(*op as usize, index, "{op:?}");
        }
        for (index, group) in SqlGroup::ALL.iter().enumerate() {
            assert_eq!(*group as usize, index, "{group:?}");
        }
    }

    #[test]
    fn a_timer_adds_one_count_and_its_elapsed_time() {
        let stats = Stats::new();
        let timed = &stats.ops[Op::Write as usize];
        {
            let _timer = Timer::start(Some(timed));
            std::thread::sleep(Duration::from_millis(2));
        }
        let (count, total_ns) = counts(timed);
        assert_eq!(count, 1);
        assert!(total_ns >= 2_000_000, "{total_ns}");

        timed.add(Duration::from_nanos(1500));
        assert_eq!(counts(timed), (2, total_ns + 1500));
    }

    #[test]
    fn a_db_call_inside_another_is_not_counted_twice() {
        let stats = Stats::new();
        {
            let _outer = sql_in(&stats, SqlGroup::Metadata);
            let _inner = sql_in(&stats, SqlGroup::Read);
        }
        assert_eq!(counts(&stats.sql[SqlGroup::Metadata as usize]).0, 1);
        assert_eq!(counts(&stats.sql[SqlGroup::Read as usize]).0, 0);

        drop(sql_in(&stats, SqlGroup::Read));
        assert_eq!(
            counts(&stats.sql[SqlGroup::Read as usize]).0,
            1,
            "timing resumes once the outer call has ended"
        );
    }

    #[test]
    fn the_documented_hooks_record_into_their_counters() {
        let stats = Stats::new();
        stats.record_checkpoint(Duration::from_nanos(40), true);
        stats.record_checkpoint(Duration::from_nanos(60), false);
        stats.record_batch_commit(7, Duration::from_nanos(90));

        let line = stats.to_json();

        assert!(
            line.contains(r#""checkpoints":{"count":2,"total_ns":100,"errors":1}"#),
            "{line}"
        );
        assert!(
            line.contains(r#""batch_commits":{"count":1,"total_ns":90,"ops":7}"#),
            "{line}"
        );
    }

    #[test]
    fn evictions_count_events_and_chunks() {
        let stats = Stats::new();
        stats.record_eviction(256);
        stats.record_eviction(3);

        let line = stats.to_json();

        assert!(
            line.contains(
                r#""write_buffer":{"slots_high_water":0,"evictions":2,"evicted_chunks":259}"#
            ),
            "{line}"
        );
    }

    #[test]
    fn json_is_one_object_with_every_counter() {
        let stats = Stats::new();
        stats.ops[Op::Write as usize].add(Duration::from_nanos(30));
        stats.chunks_flushed.store(2, Relaxed);
        stats.bytes_flushed.store(131_072, Relaxed);
        stats.reads_before_write.store(1, Relaxed);
        stats.arena_slots_high_water.store(5, Relaxed);
        stats.wal_high_water_bytes.store(4096, Relaxed);
        stats.checkpoint_errors.store(1, Relaxed);
        stats.batch_ops.store(7, Relaxed);

        let line = stats.to_json();

        assert!(
            line.starts_with(r#"{"dbfs_stats":{"ops":{"lookup":{"count":0,"total_ns":0}"#),
            "{line}"
        );
        assert!(
            line.contains(r#""write":{"count":1,"total_ns":30}"#),
            "{line}"
        );
        assert!(
            line.contains(
                r#""chunks":{"flushed":2,"bytes_flushed":131072,"reads_before_write":1}"#
            ),
            "{line}"
        );
        assert!(
            line.contains(r#""write_buffer":{"slots_high_water":5,"#),
            "{line}"
        );
        assert!(
            line.contains(r#""wal":{"high_water_bytes":4096}"#),
            "{line}"
        );
        assert!(
            line.contains(r#""checkpoints":{"count":0,"total_ns":0,"errors":1}"#),
            "{line}"
        );
        assert!(
            line.contains(r#""batch_commits":{"count":0,"total_ns":0,"ops":7}"#),
            "{line}"
        );
        assert!(line.ends_with("}}"), "{line}");
        assert!(!line.contains('\n'));
        assert_eq!(
            line.matches('{').count(),
            line.matches('}').count(),
            "{line}"
        );
    }
}
