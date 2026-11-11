//! Seeded, model-based test of the `Dbfs` API. Nothing is mounted.
//!
//! Random operations run against a `Dbfs` on a real SQLite file and against a
//! small in-memory model. Every return value and errno must match the model.
//! Every 300 operations, and at the end, the database is closed and reopened,
//! either after releasing every handle or with the handles still open. Then the
//! whole tree and every file's bytes are compared with the model, and the raw
//! tables are checked with `PRAGMA integrity_check`.
//!
//! Environment:
//! - `DBFS_MODEL_OPS`: operations per seed (default 3000).
//! - `DBFS_MODEL_SEED`: run only this seed (default: seeds 1 to 4).
//!
//! Failure messages name the seed and the operation index.
//!
//! The generator never issues the operations behind the known bugs. Those are
//! the `#[ignore]` tests at the end of this file, each with a minimal repro.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};

use dbfs::{Db, Dbfs};
use fuser::FileType;
use rusqlite::{Connection, OpenFlags};

const CHUNK: u64 = 65_536;
/// 2^24 chunks of 64 KiB: the largest file `Dbfs` stores.
const MAX_FILE: u64 = 1 << 40;
/// Model files stay below this size, so the reopen comparisons stay fast.
const LIMIT: u64 = 512 * 1024;
const ROOT: u64 = 1;
const DIR: FileType = FileType::Directory;
const FILE: FileType = FileType::RegularFile;
const FALLOC_KEEP: i32 = libc::FALLOC_FL_KEEP_SIZE;
const FALLOC_PUNCH: i32 = libc::FALLOC_FL_PUNCH_HOLE;
const FALLOC_ZERO: i32 = libc::FALLOC_FL_ZERO_RANGE;
const FALLOC_COLLAPSE: i32 = libc::FALLOC_FL_COLLAPSE_RANGE;
const NOREPLACE: u32 = libc::RENAME_NOREPLACE;
const EXCHANGE: u32 = libc::RENAME_EXCHANGE;
const WHITEOUT: u32 = libc::RENAME_WHITEOUT;
const SHORT_NAMES: [&[u8]; 8] = [
    b"a",
    b"b",
    b"c",
    b"dir",
    b"x.txt",
    "caf\u{e9}".as_bytes(),
    b"sp ace",
    b"\xff\xfe",
];

/// Kind, size and link count, as `getattr` reports them.
type Attrs = (FileType, u64, u32);
/// One readdir entry: name, inode, kind and the FUSE cookie to resume after it.
type Listing = Vec<(Vec<u8>, u64, FileType, i64)>;

fn name_ok(name: &[u8]) -> Result<(), i32> {
    if name.len() > 255 {
        Err(libc::ENAMETOOLONG)
    } else {
        Ok(())
    }
}

fn errno_name(errno: i32) -> &'static str {
    match errno {
        libc::ENOENT => "ENOENT",
        libc::EEXIST => "EEXIST",
        libc::ENOTDIR => "ENOTDIR",
        libc::EISDIR => "EISDIR",
        libc::ENOTEMPTY => "ENOTEMPTY",
        libc::EINVAL => "EINVAL",
        libc::EBADF => "EBADF",
        libc::EFBIG => "EFBIG",
        libc::ENODEV => "ENODEV",
        libc::EOPNOTSUPP => "EOPNOTSUPP",
        libc::ENAMETOOLONG => "ENAMETOOLONG",
        libc::ENOSPC => "ENOSPC",
        libc::EIO => "EIO",
        _ => "errno (unnamed)",
    }
}

fn show<T: Debug>(result: &Result<T, i32>) -> String {
    let text = match result {
        Ok(value) => format!("Ok({value:?})"),
        Err(errno) => format!("Err({})", errno_name(*errno)),
    };
    text.chars().take(160).collect()
}

/// SplitMix64: small, seedable, and the same on every platform.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`. `n` must be non-zero.
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Uniform in `0..n`. `n` must be non-zero.
    fn index(&mut self, n: usize) -> usize {
        self.below(n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

#[derive(Debug)]
struct Node {
    kind: FileType,
    /// File contents. The file size is `data.len()`. Empty for directories.
    data: Vec<u8>,
    /// Directory entries by name: (child inode, sequence number). The sequence
    /// fixes readdir order, as the database's dirent ids do.
    entries: BTreeMap<Vec<u8>, (u64, u64)>,
}

impl Node {
    fn new(kind: FileType) -> Self {
        Self {
            kind,
            data: Vec::new(),
            entries: BTreeMap::new(),
        }
    }
}

/// What the database must hold, as a plain tree. Every method returns what the
/// `Dbfs` call must return, and applies the change only on success.
struct Model {
    /// Every inode row. An inode with no names stays only while a handle is open.
    nodes: BTreeMap<u64, Node>,
    /// Live file handles: fh to inode.
    handles: BTreeMap<u64, u64>,
    seq: u64,
}

impl Model {
    fn new() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(ROOT, Node::new(DIR));
        Self {
            nodes,
            handles: BTreeMap::new(),
            seq: 0,
        }
    }

    fn kind(&self, ino: u64) -> Option<FileType> {
        self.nodes.get(&ino).map(|node| node.kind)
    }

    /// The next inode number: SQLite picks max(rowid) + 1.
    fn next_ino(&self) -> u64 {
        self.nodes.keys().next_back().map_or(ROOT, |max| max + 1)
    }

    fn refs(&self, ino: u64) -> usize {
        self.handles.values().filter(|&&open| open == ino).count()
    }

    /// The link count `getattr` reports. A directory has its own "." and its
    /// name in the parent, plus one ".." per subdirectory.
    fn links(&self, ino: u64) -> u32 {
        let Some(node) = self.nodes.get(&ino) else {
            return 0;
        };
        if node.kind == DIR {
            let subdirs = node
                .entries
                .values()
                .filter(|(child, _)| self.kind(*child) == Some(DIR))
                .count();
            return 2 + subdirs as u32;
        }
        self.nodes
            .values()
            .flat_map(|node| node.entries.values())
            .filter(|(child, _)| *child == ino)
            .count() as u32
    }

    fn parent_of(&self, ino: u64) -> u64 {
        self.nodes
            .iter()
            .find(|(_, node)| node.entries.values().any(|(child, _)| *child == ino))
            .map_or(ROOT, |(&parent, _)| parent)
    }

    /// Whether `dir` is `ancestor` or lies below it.
    fn is_under(&self, mut dir: u64, ancestor: u64) -> bool {
        loop {
            if dir == ancestor {
                return true;
            }
            if dir == ROOT {
                return false;
            }
            dir = self.parent_of(dir);
        }
    }

    fn dirs(&self) -> Vec<u64> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.kind == DIR)
            .map(|(&ino, _)| ino)
            .collect()
    }

    fn files(&self) -> Vec<u64> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.kind == FILE)
            .map(|(&ino, _)| ino)
            .collect()
    }

    /// Every (directory, name, child) triple.
    fn all_entries(&self) -> Vec<(u64, Vec<u8>, u64)> {
        self.nodes
            .iter()
            .flat_map(|(&dir, node)| {
                node.entries
                    .iter()
                    .map(move |(name, &(child, _))| (dir, name.clone(), child))
            })
            .collect()
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<u64, i32> {
        name_ok(name)?;
        match self.nodes.get(&parent) {
            Some(node) if node.kind == DIR => node
                .entries
                .get(name)
                .map(|&(child, _)| child)
                .ok_or(libc::ENOENT),
            _ => Err(libc::ENOENT),
        }
    }

    fn getattr(&self, ino: u64) -> Result<Attrs, i32> {
        let node = self.nodes.get(&ino).ok_or(libc::ENOENT)?;
        Ok((node.kind, node.data.len() as u64, self.links(ino)))
    }

    fn add_entry(&mut self, dir: u64, name: &[u8], child: u64) {
        self.seq += 1;
        let seq = self.seq;
        let node = self.nodes.get_mut(&dir).expect("parent exists");
        node.entries.insert(name.to_vec(), (child, seq));
    }

    fn remove_entry(&mut self, dir: u64, name: &[u8]) -> Option<u64> {
        self.nodes
            .get_mut(&dir)?
            .entries
            .remove(name)
            .map(|(child, _)| child)
    }

    fn create(&mut self, parent: u64, name: &[u8], kind: FileType) -> Result<u64, i32> {
        name_ok(name)?;
        let parent_node = self.nodes.get(&parent).ok_or(libc::ENOENT)?;
        if parent_node.kind != DIR {
            return Err(libc::ENOTDIR);
        }
        if parent_node.entries.contains_key(name) {
            return Err(libc::EEXIST);
        }
        let ino = self.next_ino();
        self.nodes.insert(ino, Node::new(kind));
        self.add_entry(parent, name, ino);
        Ok(ino)
    }

    /// `open` accepts any inode. The runner only opens regular files.
    fn open(&self, ino: u64) -> Result<(), i32> {
        self.nodes.get(&ino).map(|_| ()).ok_or(libc::ENOENT)
    }

    fn handle(&self, fh: u64) -> Result<u64, i32> {
        self.handles.get(&fh).copied().ok_or(libc::EBADF)
    }

    fn read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        let node = self.nodes.get(&ino).ok_or(libc::ENOENT)?;
        if node.kind == DIR {
            return Err(libc::EISDIR);
        }
        let len = node.data.len() as u64;
        if offset >= len || size == 0 {
            return Ok(Vec::new());
        }
        let end = (offset + u64::from(size)).min(len);
        Ok(node.data[offset as usize..end as usize].to_vec())
    }

    fn read_fh(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        let ino = self.handle(fh)?;
        self.read(ino, offset, size)
    }

    fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
        if data.is_empty() {
            return Ok(0);
        }
        let node = self.nodes.get_mut(&ino).ok_or(libc::ENOENT)?;
        if node.kind == DIR {
            return Err(libc::EISDIR);
        }
        let end = offset
            .checked_add(data.len() as u64)
            .filter(|&end| end <= MAX_FILE)
            .ok_or(libc::EFBIG)? as usize;
        if node.data.len() < end {
            node.data.resize(end, 0);
        }
        node.data[offset as usize..end].copy_from_slice(data);
        Ok(data.len() as u32)
    }

    fn write_fh(&mut self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
        let ino = self.handle(fh)?;
        self.write(ino, offset, data)
    }

    fn truncate(&mut self, ino: u64, size: u64) -> Result<(), i32> {
        let node = self.nodes.get_mut(&ino).ok_or(libc::ENOENT)?;
        if node.kind == DIR {
            return Err(libc::EISDIR);
        }
        if size > MAX_FILE {
            return Err(libc::EFBIG);
        }
        node.data.resize(size as usize, 0);
        Ok(())
    }

    fn fallocate(&mut self, ino: u64, offset: i64, length: i64, mode: i32) -> Result<(), i32> {
        if offset < 0 || length <= 0 {
            return Err(libc::EINVAL);
        }
        if mode & !FALLOC_KEEP != 0 {
            return Err(libc::EOPNOTSUPP);
        }
        let node = self.nodes.get_mut(&ino).ok_or(libc::ENOENT)?;
        if node.kind == DIR {
            return Err(libc::ENODEV);
        }
        let end = offset.checked_add(length).ok_or(libc::EFBIG)? as u64;
        if end > MAX_FILE {
            return Err(libc::EFBIG);
        }
        if mode & FALLOC_KEEP != 0 {
            return Ok(());
        }
        if (end as usize) > node.data.len() {
            node.data.resize(end as usize, 0);
        }
        Ok(())
    }

    /// Closes one handle. An inode with no names left goes with its last handle.
    fn release(&mut self, fh: u64) -> Result<(), i32> {
        let ino = self.handles.remove(&fh).ok_or(libc::EBADF)?;
        if self.refs(ino) == 0 && self.links(ino) == 0 {
            self.nodes.remove(&ino);
        }
        Ok(())
    }

    fn unlink(&mut self, parent: u64, name: &[u8]) -> Result<(), i32> {
        let ino = self.lookup(parent, name)?;
        if self.kind(ino) == Some(DIR) {
            return Err(libc::EISDIR);
        }
        self.remove_entry(parent, name);
        if self.links(ino) == 0 && self.refs(ino) == 0 {
            self.nodes.remove(&ino);
        }
        Ok(())
    }

    fn rmdir(&mut self, parent: u64, name: &[u8]) -> Result<(), i32> {
        let ino = self.lookup(parent, name)?;
        let node = &self.nodes[&ino];
        if node.kind != DIR {
            return Err(libc::ENOTDIR);
        }
        if !node.entries.is_empty() {
            return Err(libc::ENOTEMPTY);
        }
        self.remove_entry(parent, name);
        self.nodes.remove(&ino);
        Ok(())
    }

    fn rename(
        &mut self,
        parent: u64,
        name: &[u8],
        new_parent: u64,
        new_name: &[u8],
        flags: u32,
    ) -> Result<(), i32> {
        if flags & !NOREPLACE != 0 {
            return Err(libc::EINVAL);
        }
        name_ok(new_name)?;
        let src = self.lookup(parent, name)?;
        let src_kind = self.kind(src).expect("looked up");
        match self.kind(new_parent) {
            None => return Err(libc::ENOENT),
            Some(FILE) => return Err(libc::ENOTDIR),
            Some(_) => {}
        }
        if src_kind == DIR && self.is_under(new_parent, src) {
            return Err(libc::EINVAL);
        }
        if parent == new_parent && name == new_name {
            return Ok(());
        }
        let target = self.nodes[&new_parent]
            .entries
            .get(new_name)
            .map(|&(child, _)| child);
        if flags & NOREPLACE != 0 && target.is_some() {
            return Err(libc::EEXIST);
        }
        if let Some(target) = target {
            match (src_kind, self.kind(target)) {
                (FILE, Some(DIR)) => return Err(libc::EISDIR),
                (DIR, Some(FILE)) => return Err(libc::ENOTDIR),
                (DIR, Some(DIR)) if !self.nodes[&target].entries.is_empty() => {
                    return Err(libc::ENOTEMPTY);
                }
                _ => {}
            }
            // Renaming onto another name of the same inode is the known bug in
            // the `hard_link_rename_onto_its_own_inode_is_a_noop` test. The
            // generator skips that case, so reaching it means a generator bug.
            assert_ne!(target, src, "generator issued a same-inode rename");
        }
        if let Some(target) = target {
            self.remove_entry(new_parent, new_name);
            if self.kind(target) == Some(DIR) || (self.links(target) == 0 && self.refs(target) == 0)
            {
                self.nodes.remove(&target);
            }
        }
        let (child, seq) = self
            .nodes
            .get_mut(&parent)
            .expect("parent exists")
            .entries
            .remove(name)
            .expect("source entry exists");
        self.nodes
            .get_mut(&new_parent)
            .expect("new parent exists")
            .entries
            .insert(new_name.to_vec(), (child, seq));
        Ok(())
    }

    /// Returns the new link count, which the reply carries.
    fn link(&mut self, ino: u64, dir: u64, name: &[u8]) -> Result<u32, i32> {
        name_ok(name)?;
        let kind = self.kind(ino).ok_or(libc::ENOENT)?;
        if kind == DIR {
            return Err(libc::EISDIR);
        }
        match self.kind(dir) {
            None => return Err(libc::ENOENT),
            Some(FILE) => return Err(libc::ENOTDIR),
            Some(_) => {}
        }
        if self.nodes[&dir].entries.contains_key(name) {
            return Err(libc::EEXIST);
        }
        self.add_entry(dir, name, ino);
        Ok(self.links(ino))
    }

    /// The whole directory, from the start: ".", "..", then the entries in order.
    fn readdir(&self, dir: u64) -> Result<Vec<(Vec<u8>, u64, FileType)>, i32> {
        let node = self.nodes.get(&dir).ok_or(libc::ENOENT)?;
        if node.kind != DIR {
            return Err(libc::ENOTDIR);
        }
        let mut out = vec![
            (b".".to_vec(), dir, DIR),
            (b"..".to_vec(), self.parent_of(dir), DIR),
        ];
        let mut entries: Vec<(u64, &Vec<u8>, u64)> = node
            .entries
            .iter()
            .map(|(name, &(child, seq))| (seq, name, child))
            .collect();
        entries.sort_unstable();
        for (_, name, child) in entries {
            out.push((name.clone(), child, self.kind(child).expect("child exists")));
        }
        Ok(out)
    }

    /// Drops inodes that have no names, as the next `Db::open` does.
    fn drop_unnamed(&mut self) {
        let unnamed: Vec<u64> = self
            .nodes
            .keys()
            .copied()
            .filter(|&ino| ino != ROOT && self.links(ino) == 0)
            .collect();
        for ino in unnamed {
            self.nodes.remove(&ino);
        }
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().map(|value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be an unsigned integer, got {value:?}"))
    })
}

fn work_dir(seed: u64) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("model_ops")
        .join(format!("seed-{seed}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create work dir");
    dir
}

/// Raw SQLite checks that do not go through `Dbfs`. Returns the first problem.
fn check_disk(path: &Path) -> Result<(), String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open database for raw checks");
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity_check runs");
    if verdict != "ok" {
        return Err(format!("integrity_check: {verdict}"));
    }
    // A chunk must belong to a live inode and must not reach past its end of file.
    let stray: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM file_chunks c
             LEFT JOIN inodes i ON i.ino = (c.id >> 24)
             WHERE i.ino IS NULL
                OR length(c.data) > i.size - (c.id & 16777215) * 65536",
            [],
            |row| row.get(0),
        )
        .expect("chunk check runs");
    if stray != 0 {
        return Err(format!(
            "{stray} chunk rows belong to no inode or lie past the end of their file"
        ));
    }
    let unnamed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM inodes WHERE nlink = 0 AND ino != 1",
            [],
            |row| row.get(0),
        )
        .expect("nlink check runs");
    if unnamed != 0 {
        return Err(format!("{unnamed} inodes without names survived reopen"));
    }
    Ok(())
}

struct Run {
    seed: u64,
    op: usize,
    rng: Rng,
    path: PathBuf,
    slots: usize,
    fs: Option<Dbfs>,
    model: Model,
    long_ok: Vec<u8>,
    too_long: Vec<u8>,
    last_released: Option<u64>,
    tallies: BTreeMap<&'static str, u64>,
    skipped_same_inode_renames: u64,
}

impl Run {
    fn new(seed: u64, path: PathBuf, slots: usize) -> Self {
        let db = Db::open(&path).expect("open database");
        Self {
            seed,
            op: 0,
            rng: Rng::new(seed),
            path,
            slots,
            fs: Some(Dbfs::with_write_buffer(db, slots * CHUNK as usize)),
            model: Model::new(),
            long_ok: vec![b'L'; 255],
            too_long: vec![b'T'; 256],
            last_released: None,
            tallies: BTreeMap::new(),
            skipped_same_inode_renames: 0,
        }
    }

    fn fs(&self) -> &Dbfs {
        self.fs.as_ref().expect("filesystem is open")
    }

    fn fail(&self, what: &str) -> ! {
        panic!("model_ops seed {} op {}: {what}", self.seed, self.op)
    }

    fn tally(&mut self, key: &'static str) {
        *self.tallies.entry(key).or_default() += 1;
    }

    fn check<T: PartialEq + Debug>(
        &mut self,
        what: &str,
        got: Result<T, i32>,
        want: Result<T, i32>,
    ) {
        if let Err(errno) = &got {
            self.tally(errno_name(*errno));
        }
        if got != want {
            self.fail(&format!(
                "{what}: got {}, model expects {}",
                show(&got),
                show(&want)
            ));
        }
    }

    fn check_bytes(&mut self, what: &str, got: Result<Vec<u8>, i32>, want: Result<Vec<u8>, i32>) {
        if let (Ok(g), Ok(w)) = (&got, &want) {
            if g != w {
                let at = g
                    .iter()
                    .zip(w.iter())
                    .position(|(a, b)| a != b)
                    .unwrap_or(g.len().min(w.len()));
                self.fail(&format!(
                    "{what}: {} bytes read, model expects {} bytes, first difference at byte {at}",
                    g.len(),
                    w.len()
                ));
            }
            return;
        }
        self.check(what, got, want);
    }

    fn register(&mut self, fh: u64, ino: u64) {
        if self.model.handles.insert(fh, ino).is_some() {
            self.fail(&format!("fh {fh} handed out while still open"));
        }
    }

    fn attrs(&self, ino: u64) -> Result<Attrs, i32> {
        self.fs().getattr(ino).map(|a| (a.kind, a.size, a.nlink))
    }

    fn listing(&self, dir: u64) -> Result<Listing, i32> {
        self.fs().readdir(dir, 0).map(|entries| {
            entries
                .into_iter()
                .map(|e| (e.name, e.ino, e.kind, e.offset))
                .collect()
        })
    }

    fn size_of(&self, ino: u64) -> u64 {
        self.model
            .nodes
            .get(&ino)
            .map_or(0, |node| node.data.len() as u64)
    }

    /// An existing inode about 90% of the time, otherwise an inode that does not exist.
    fn pick_node(&mut self) -> u64 {
        if self.rng.chance(10) {
            return self.ghost_ino();
        }
        let inos: Vec<u64> = self.model.nodes.keys().copied().collect();
        inos[self.rng.index(inos.len())]
    }

    fn pick_dir(&mut self) -> u64 {
        if self.rng.chance(3) {
            return self.ghost_ino();
        }
        let dirs = self.model.dirs();
        dirs[self.rng.index(dirs.len())]
    }

    fn pick_file(&mut self) -> Option<u64> {
        let files = self.model.files();
        if files.is_empty() {
            None
        } else {
            Some(files[self.rng.index(files.len())])
        }
    }

    /// A regular file most of the time, sometimes a directory (EISDIR) or a missing inode.
    fn pick_target(&mut self) -> u64 {
        match self.rng.below(100) {
            0..=84 => self.pick_file().unwrap_or(ROOT),
            85..=91 => self.pick_dir(),
            _ => self.ghost_ino(),
        }
    }

    /// A file that still has a name. Linking an unnamed (open, unlinked) inode is
    /// the known bug, so the generator avoids it.
    fn pick_linkable(&mut self) -> Option<u64> {
        let candidates: Vec<u64> = self
            .model
            .files()
            .into_iter()
            .filter(|&ino| self.model.links(ino) > 0)
            .collect();
        if candidates.is_empty() {
            None
        } else {
            Some(candidates[self.rng.index(candidates.len())])
        }
    }

    fn ghost_ino(&mut self) -> u64 {
        let candidates = [u64::MAX, self.model.next_ino(), 1 << 30];
        let ino = candidates[self.rng.index(candidates.len())];
        if self.model.nodes.contains_key(&ino) {
            u64::MAX
        } else {
            ino
        }
    }

    fn pick_fh(&mut self) -> u64 {
        let fhs: Vec<u64> = self.model.handles.keys().copied().collect();
        fhs[self.rng.index(fhs.len())]
    }

    /// A handle that is not open: the last one released (if still free), or a random one.
    fn ghost_fh(&mut self) -> u64 {
        if let Some(fh) = self.last_released
            && !self.model.handles.contains_key(&fh)
            && self.rng.chance(50)
        {
            return fh;
        }
        loop {
            let fh = self.rng.next_u64() | (1 << 62);
            if !self.model.handles.contains_key(&fh) {
                return fh;
            }
        }
    }

    /// A (directory, name) pair: an existing entry about 75% of the time, so that
    /// lookups, renames and unlinks mostly succeed.
    fn pick_entry(&mut self) -> (u64, Vec<u8>) {
        let all = self.model.all_entries();
        if !all.is_empty() && self.rng.chance(75) {
            let (dir, name, _) = all[self.rng.index(all.len())].clone();
            return (dir, name);
        }
        let dir = self.pick_dir();
        (dir, self.pick_name())
    }

    fn pick_name(&mut self) -> Vec<u8> {
        match self.rng.below(100) {
            0..=1 => self.too_long.clone(),
            2..=4 => self.long_ok.clone(),
            _ => SHORT_NAMES[self.rng.index(SHORT_NAMES.len())].to_vec(),
        }
    }

    fn pick_len(&mut self) -> usize {
        match self.rng.below(100) {
            0..=39 => 1 + self.rng.index(64),
            40..=69 => 1 + self.rng.index(8 * 1024),
            70..=89 => 1 + self.rng.index(3 * CHUNK as usize),
            _ => 1 + self.rng.index(LIMIT as usize / 2),
        }
    }

    /// Unaligned offsets, offsets that straddle a chunk boundary, and sparse
    /// offsets beyond the end of the file.
    fn pick_offset(&mut self, size: u64, len: usize) -> u64 {
        let raw = match self.rng.below(100) {
            0..=39 => self.rng.below(size + 1),
            40..=59 => {
                let boundary = self.rng.below(size / CHUNK + 1) * CHUNK;
                boundary
                    .saturating_sub(self.rng.below(32))
                    .saturating_add(self.rng.below(32))
            }
            _ => size + self.rng.below(2 * CHUNK),
        };
        raw.min(LIMIT - len as u64)
    }

    fn pick_size(&mut self, size: u64) -> u64 {
        let raw = match self.rng.below(100) {
            0..=39 => self.rng.below(size + 1),
            40..=59 => {
                let boundary = self.rng.below(LIMIT / CHUNK + 1) * CHUNK;
                if self.rng.chance(50) {
                    boundary.saturating_sub(1)
                } else {
                    boundary
                }
            }
            _ => size + self.rng.below(2 * CHUNK),
        };
        raw.min(LIMIT)
    }

    fn payload(&mut self, len: usize) -> Vec<u8> {
        // Values 1..=254, never zero, so a hole (zeros) cannot pass for data.
        let base = self.rng.below(250) as u8 + 1;
        (0..len).map(|i| base + (i % 5) as u8).collect()
    }

    fn pick_fallocate(&mut self, size: u64) -> (i64, i64, i32) {
        let mode = [
            0,
            FALLOC_KEEP,
            FALLOC_PUNCH,
            FALLOC_ZERO,
            FALLOC_COLLAPSE,
            0x03,
        ][self.rng.index(6)];
        match self.rng.below(100) {
            0..=2 => (i64::MAX - 10, 100, mode),
            3..=4 => ((MAX_FILE - 5) as i64, 100, mode),
            5..=6 => (-1, 10, mode),
            7..=8 => (0, 0, mode),
            _ => {
                let length = 1 + self.rng.below(96 * 1024) as i64;
                let offset = (self.rng.below(size + CHUNK) as i64).min(LIMIT as i64 - length);
                (offset, length, mode)
            }
        }
    }

    fn step(&mut self) {
        let crowded = self.model.nodes.len() > 80;
        let many_handles = self.model.handles.len() > 40;
        match self.rng.below(1000) {
            0..=79 if !crowded => self.op_create(),
            0..=79 => self.op_unlink(),
            80..=119 if !crowded => self.op_mkdir(),
            80..=119 => self.op_rmdir(),
            120..=179 if !many_handles => self.op_open(),
            120..=219 => self.op_release(),
            220..=379 => self.op_write(),
            380..=499 => self.op_read(),
            500..=559 => self.op_truncate(),
            560..=599 => self.op_fallocate(),
            600..=619 => self.op_flush_file(),
            620..=629 => self.op_fsync(),
            630..=689 => self.op_rename(),
            690..=739 => self.op_unlink(),
            740..=759 => self.op_link(),
            760..=789 => self.op_rmdir(),
            790..=879 => self.op_readdir(),
            880..=939 => self.op_attrs(),
            940..=949 => self.op_statfs(),
            _ => self.op_bad_handles(),
        }
        self.spot_check();
    }

    fn spot_check(&mut self) {
        let ino = self.pick_node();
        let want = self.model.getattr(ino);
        let got = self.attrs(ino);
        self.check("getattr (spot check)", got, want);
    }

    fn op_create(&mut self) {
        self.tally("create");
        let parent = self.pick_dir();
        let name = self.pick_name();
        let want = self.model.create(parent, &name, FILE);
        let got = if self.rng.chance(50) {
            self.fs()
                .create_open(parent, &name, 0o666, 0o022, 0, 0)
                .map(|(attr, fh)| (attr.ino, Some(fh)))
        } else {
            self.fs()
                .create(parent, &name, 0o666, 0o022, 0, 0)
                .map(|attr| (attr.ino, None))
        };
        let (got_ino, fh) = match got {
            Ok((ino, fh)) => (Ok(ino), fh),
            Err(errno) => (Err(errno), None),
        };
        self.check("create", got_ino, want);
        if let (Some(fh), Ok(ino)) = (fh, got_ino) {
            self.register(fh, ino);
        }
    }

    fn op_mkdir(&mut self) {
        self.tally("mkdir");
        let parent = self.pick_dir();
        let name = self.pick_name();
        let want = self.model.create(parent, &name, DIR);
        let got = self
            .fs()
            .mkdir(parent, &name, 0o777, 0o022, 0, 0)
            .map(|attr| attr.ino);
        self.check("mkdir", got, want);
    }

    fn op_open(&mut self) {
        self.tally("open");
        let ino = match self.pick_file() {
            Some(file) if self.rng.chance(90) => file,
            _ => self.ghost_ino(),
        };
        let want = self.model.open(ino);
        let got = self.fs().open(ino);
        let fh = got.as_ref().ok().copied();
        self.check("open", got.map(|_| ()), want);
        if let Some(fh) = fh {
            self.register(fh, ino);
        }
    }

    fn op_release(&mut self) {
        self.tally("release");
        let fh = if !self.model.handles.is_empty() && !self.rng.chance(10) {
            self.pick_fh()
        } else {
            self.ghost_fh()
        };
        let want = self.model.release(fh);
        let got = self.fs().release_file(fh);
        if got.is_ok() {
            self.last_released = Some(fh);
        }
        self.check("release", got, want);
    }

    fn op_write(&mut self) {
        self.tally("write");
        let len = if self.rng.chance(4) {
            0
        } else {
            self.pick_len()
        };
        let data = self.payload(len);
        if !self.model.handles.is_empty() && self.rng.chance(50) {
            self.tally("write-fh");
            let fh = if self.rng.chance(5) {
                self.ghost_fh()
            } else {
                self.pick_fh()
            };
            let ino = self.model.handles.get(&fh).copied().unwrap_or(ROOT);
            let size = self.size_of(ino);
            let offset = self.pick_offset(size, len);
            let want = self.model.write_fh(fh, offset, &data);
            let got = self.fs().write_fh(fh, offset, &data);
            self.check("write_fh", got, want);
        } else {
            let ino = self.pick_target();
            let size = self.size_of(ino);
            let offset = self.pick_offset(size, len);
            let want = self.model.write(ino, offset, &data);
            let got = self.fs().write(ino, offset, &data);
            self.check("write", got, want);
        }
    }

    fn op_read(&mut self) {
        self.tally("read");
        let size: u32 = match self.rng.below(100) {
            0..=9 => 0,
            10..=69 => 1 + self.rng.below(16 * 1024) as u32,
            _ => 1 + self.rng.below(256 * 1024) as u32,
        };
        if !self.model.handles.is_empty() && self.rng.chance(50) {
            self.tally("read-fh");
            let fh = if self.rng.chance(5) {
                self.ghost_fh()
            } else {
                self.pick_fh()
            };
            let ino = self.model.handles.get(&fh).copied().unwrap_or(ROOT);
            let offset = self.pick_offset(self.size_of(ino), size as usize);
            let want = self.model.read_fh(fh, offset, size);
            let got = self.fs().read_fh(fh, offset, size);
            self.check_bytes("read_fh", got, want);
        } else {
            let ino = self.pick_target();
            let offset = self.pick_offset(self.size_of(ino), size as usize);
            let want = self.model.read(ino, offset, size);
            let got = self.fs().read(ino, offset, size);
            self.check_bytes("read", got, want);
        }
    }

    fn op_truncate(&mut self) {
        self.tally("truncate");
        let ino = self.pick_target();
        let new_size = self.pick_size(self.size_of(ino));
        let want = self.model.truncate(ino, new_size).map(|()| new_size);
        let got = self
            .fs()
            .setattr(ino, None, None, None, Some(new_size), None, None)
            .map(|attr| attr.size);
        self.check("truncate", got, want);
    }

    fn op_fallocate(&mut self) {
        self.tally("fallocate");
        let ino = self.pick_target();
        let (offset, length, mode) = self.pick_fallocate(self.size_of(ino));
        let want = self.model.fallocate(ino, offset, length, mode);
        let got = self.fs().fallocate(ino, offset, length, mode);
        self.check("fallocate", got, want);
    }

    fn op_flush_file(&mut self) {
        self.tally("flush");
        let ino = self.pick_node();
        let got = self.fs().flush_file(ino);
        self.check("flush_file", got, Ok(()));
    }

    fn op_fsync(&mut self) {
        self.tally("fsync");
        let got = self.fs().flush_all();
        self.check("flush_all (fsync)", got, Ok(()));
    }

    fn op_rename(&mut self) {
        self.tally("rename");
        let (parent, name) = self.pick_entry();
        let (new_parent, new_name) = if self.rng.chance(60) {
            self.pick_entry()
        } else {
            (self.pick_dir(), self.pick_name())
        };
        let flags = match self.rng.below(100) {
            0..=4 => NOREPLACE,
            5..=7 => EXCHANGE,
            8..=9 => WHITEOUT,
            _ => 0,
        };
        // Two names of one inode (hard links): the known bug. Skip it.
        if flags == 0 && (parent, &name) != (new_parent, &new_name) {
            let source = self.model.lookup(parent, &name);
            let target = self.model.lookup(new_parent, &new_name);
            if source.is_ok() && source == target {
                self.skipped_same_inode_renames += 1;
                return;
            }
        }
        let want = self
            .model
            .rename(parent, &name, new_parent, &new_name, flags);
        let got = self
            .fs()
            .rename(parent, &name, new_parent, &new_name, flags);
        self.check("rename", got, want);
    }

    fn op_unlink(&mut self) {
        self.tally("unlink");
        let (parent, name) = self.pick_entry();
        let want = self.model.unlink(parent, &name);
        let got = self.fs().unlink(parent, &name);
        self.check("unlink", got, want);
    }

    fn op_rmdir(&mut self) {
        self.tally("rmdir");
        let subdirs: Vec<(u64, Vec<u8>)> = self
            .model
            .all_entries()
            .into_iter()
            .filter(|(_, _, child)| self.model.kind(*child) == Some(DIR))
            .map(|(dir, name, _)| (dir, name))
            .collect();
        let (parent, name) = if !subdirs.is_empty() && self.rng.chance(85) {
            subdirs[self.rng.index(subdirs.len())].clone()
        } else {
            self.pick_entry()
        };
        let want = self.model.rmdir(parent, &name);
        let got = self.fs().rmdir(parent, &name);
        self.check("rmdir", got, want);
    }

    fn op_link(&mut self) {
        self.tally("link");
        let src = match self.rng.below(100) {
            0..=84 => self.pick_linkable().unwrap_or(ROOT),
            85..=92 => self.pick_dir(),
            _ => self.ghost_ino(),
        };
        let dir = self.pick_dir();
        let name = self.pick_name();
        let want = self.model.link(src, dir, &name);
        let got = self.fs().link(src, dir, &name).map(|attr| attr.nlink);
        self.check("link", got, want);
    }

    /// `(name, ino, kind)` for every entry, in order.
    fn readdir_without_cookies(entries: &Listing) -> Vec<(Vec<u8>, u64, FileType)> {
        entries
            .iter()
            .map(|(name, ino, kind, _)| (name.clone(), *ino, *kind))
            .collect()
    }

    fn op_readdir(&mut self) {
        self.tally("readdir");
        let dir = match self.rng.below(100) {
            0..=89 => self.pick_dir(),
            90..=94 => self.pick_file().unwrap_or(ROOT),
            _ => self.ghost_ino(),
        };
        let want = self.model.readdir(dir);
        let full = self.listing(dir);
        let got = full
            .as_ref()
            .map(Self::readdir_without_cookies)
            .map_err(|e| *e);
        self.check("readdir", got, want);

        let Ok(full) = full else {
            return;
        };
        for pair in full.windows(2) {
            if pair[0].3 >= pair[1].3 {
                self.fail(&format!(
                    "readdir cookies are not increasing: {} then {}",
                    pair[0].3, pair[1].3
                ));
            }
        }
        if full.is_empty() {
            return;
        }
        // Resume after a random entry, and after a random offset: both must
        // return exactly the entries whose cookie is larger.
        let k = self.rng.index(full.len());
        let cookie = full[k].3;
        let rest: Vec<_> = full[k + 1..].to_vec();
        let resumed = self.fs().readdir(dir, cookie).map(|v| {
            v.into_iter()
                .map(|e| (e.name, e.ino, e.kind, e.offset))
                .collect::<Listing>()
        });
        self.check("readdir resume after entry", resumed, Ok(rest));

        let max_cookie = full.last().map_or(0, |entry| entry.3);
        let offset = self.rng.below(max_cookie as u64 + 2) as i64;
        let expected: Listing = full
            .iter()
            .filter(|entry| entry.3 > offset)
            .cloned()
            .collect();
        let resumed = self.fs().readdir(dir, offset).map(|v| {
            v.into_iter()
                .map(|e| (e.name, e.ino, e.kind, e.offset))
                .collect::<Listing>()
        });
        self.check("readdir from arbitrary offset", resumed, Ok(expected));
    }

    fn op_attrs(&mut self) {
        self.tally("getattr+lookup");
        let ino = self.pick_node();
        let want = self.model.getattr(ino);
        let got = self.attrs(ino);
        self.check("getattr", got, want);

        let (parent, name) = self.pick_entry();
        let want = self.model.lookup(parent, &name);
        let got = self.fs().lookup(parent, &name).map(|attr| attr.ino);
        self.check("lookup", got, want);
    }

    fn op_statfs(&mut self) {
        self.tally("statfs");
        let stats = match self.fs().statfs() {
            Ok(stats) => stats,
            Err(errno) => self.fail(&format!("statfs failed with {}", errno_name(errno))),
        };
        if stats.bfree > stats.blocks || stats.bavail > stats.blocks {
            self.fail(&format!(
                "statfs: free blocks {} exceed total {}",
                stats.bfree, stats.blocks
            ));
        }
        let want = Ok(self.model.nodes.len() as u64);
        self.check("statfs files", Ok(stats.files), want);
    }

    /// Handles and inodes that do not exist.
    fn op_bad_handles(&mut self) {
        self.tally("bad-handles");
        let fh = self.ghost_fh();
        let data = self.payload(7);
        let want = self.model.write_fh(fh, 0, &data);
        let got = self.fs().write_fh(fh, 0, &data);
        self.check("write_fh (not open)", got, want);

        let want = self.model.read_fh(fh, 0, 7);
        let got = self.fs().read_fh(fh, 0, 7);
        self.check_bytes("read_fh (not open)", got, want);

        let want = self.model.release(fh);
        let got = self.fs().release_file(fh);
        self.check("release (not open)", got, want);

        let ino = self.ghost_ino();
        let want = self.model.write(ino, 0, &data);
        let got = self.fs().write(ino, 0, &data);
        self.check("write (missing inode)", got, want);

        let want = self.model.write(ino, 0, &[]);
        let got = self.fs().write(ino, 0, &[]);
        self.check("empty write (missing inode)", got, want);

        let want = self.model.read(ino, 0, 7);
        let got = self.fs().read(ino, 0, 7);
        self.check_bytes("read (missing inode)", got, want);

        let got = self.fs().flush_file(ino);
        self.check("flush_file (missing inode)", got, Ok(()));
    }

    /// Reopens the database. Every other reopen first releases all handles.
    fn reopen(&mut self) {
        self.tally("reopen");
        let fs = self.fs.take().expect("filesystem is open");
        if self.rng.chance(50) {
            self.tally("reopen (release all handles)");
            let fhs: Vec<u64> = self.model.handles.keys().copied().collect();
            for fh in fhs {
                let want = self.model.release(fh);
                let got = fs.release_file(fh);
                self.check("release before reopen", got, want);
            }
        } else {
            self.tally("reopen (handles kept open)");
        }
        let got = fs.flush_all();
        self.check("flush_all before reopen", got, Ok(()));
        drop(fs);

        // Dropping the filesystem closes every handle. The next open deletes the
        // inodes that have no names left.
        self.model.handles.clear();
        self.model.drop_unnamed();
        let db = match Db::open(&self.path) {
            Ok(db) => db,
            Err(error) => self.fail(&format!("reopen failed: {error:?}")),
        };
        self.fs = Some(Dbfs::with_write_buffer(db, self.slots * CHUNK as usize));
        if let Err(problem) = check_disk(&self.path) {
            self.fail(&problem);
        }
        self.full_compare();
    }

    /// Every inode, every name and every byte must match the model.
    fn full_compare(&mut self) {
        let inos: Vec<u64> = self.model.nodes.keys().copied().collect();
        for ino in inos {
            let want = self.model.getattr(ino);
            let got = self.attrs(ino);
            self.check("getattr after reopen", got, want);
            if self.model.kind(ino) == Some(FILE) {
                let len = self.size_of(ino) as u32;
                let want = self.model.read(ino, 0, len.max(1));
                let got = self.fs().read(ino, 0, len.max(1));
                self.check_bytes("contents after reopen", got, want);
            } else {
                let want = self.model.readdir(ino);
                let got = self
                    .listing(ino)
                    .map(|full| Self::readdir_without_cookies(&full));
                self.check("listing after reopen", got, want);
            }
        }
        let got = self.fs().statfs().map(|stats| stats.files);
        let want = Ok(self.model.nodes.len() as u64);
        self.check("statfs files after reopen", got, want);
        for (dir, name, child) in self.model.all_entries() {
            let got = self.fs().lookup(dir, &name).map(|attr| attr.ino);
            self.check("lookup after reopen", got, Ok(child));
        }
    }
}

fn run_seed(seed: u64, ops: usize) {
    let dir = work_dir(seed);
    let path = dir.join("model.db");
    // 2, 3 or 4 slots of 64 KiB: the write buffer is always small, so evictions happen.
    let slots = 2 + (seed % 3) as usize;
    let mut run = Run::new(seed, path, slots);
    for op in 0..ops {
        run.op = op;
        run.step();
        if (op + 1).is_multiple_of(300) {
            run.reopen();
        }
    }
    run.op = ops;
    run.reopen();
    let summary: Vec<String> = run
        .tallies
        .iter()
        .map(|(name, count)| format!("{name}={count}"))
        .collect();
    eprintln!(
        "model_ops seed {seed}: {ops} ops, {} same-inode renames skipped; {}",
        run.skipped_same_inode_renames,
        summary.join(" ")
    );
    drop(run);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn random_operations_match_the_model() {
    let ops = env_u64("DBFS_MODEL_OPS").unwrap_or(3000) as usize;
    let seeds = match env_u64("DBFS_MODEL_SEED") {
        Some(seed) => vec![seed],
        None => vec![1, 2, 3, 4],
    };
    for seed in seeds {
        run_seed(seed, ops);
    }
}

/// A file with one name, in a fresh in-memory filesystem.
fn file_fs(name: &[u8]) -> (Dbfs, u64) {
    let db = Db::open_in_memory().expect("open in-memory database");
    let fs = Dbfs::new(db);
    let ino = fs.create(1, name, 0o644, 0, 0, 0).expect("create file").ino;
    (fs, ino)
}

#[test]
fn write_and_truncate_stop_exactly_at_one_tib() {
    let (fs, file) = file_fs(b"big");
    const MAX: u64 = 1 << 40;

    assert_eq!(fs.write(file, MAX - 1, b"x"), Ok(1));
    assert_eq!(fs.write(file, MAX - 1, b"xy").err(), Some(libc::EFBIG));
    assert_eq!(fs.write(file, MAX, b"z").err(), Some(libc::EFBIG));
    assert_eq!(fs.getattr(file).expect("getattr").size, MAX);

    fs.flush_all().expect("flush");
    assert_eq!(fs.read(file, MAX - 1, 8), Ok(b"x".to_vec()));
    assert_eq!(
        fs.setattr(file, None, None, None, Some(MAX + 1), None, None)
            .err(),
        Some(libc::EFBIG)
    );
    assert_eq!(
        fs.setattr(file, None, None, None, Some(MAX), None, None)
            .expect("truncate to the limit")
            .size,
        MAX
    );
}

#[test]
fn readdir_resumes_correctly_when_entries_change_between_pages() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let fs = Dbfs::new(db);
    let dir = fs.mkdir(1, b"d", 0o755, 0, 0, 0).expect("mkdir").ino;
    for i in 0..10 {
        fs.create(dir, format!("f{i}").as_bytes(), 0o644, 0, 0, 0)
            .expect("create");
    }
    let names = |entries: &[dbfs::DbfsDirEntry]| -> Vec<Vec<u8>> {
        entries.iter().map(|e| e.name.clone()).collect()
    };
    // `all` starts with "." and "..", so the third file is `all[4]`.
    let all = fs.readdir(dir, 0).expect("readdir");
    let after_third = all[4].offset;
    assert_eq!(
        names(&fs.readdir(dir, after_third).expect("page")),
        names(&all[5..])
    );

    // Remove two entries, rename one, add one: the next page still resumes right.
    fs.unlink(dir, b"f4").expect("unlink f4");
    fs.unlink(dir, b"f7").expect("unlink f7");
    fs.rename(dir, b"f5", dir, b"z", 0).expect("rename f5");
    fs.create(dir, b"new", 0o644, 0, 0, 0).expect("create new");

    let rest = fs.readdir(dir, after_third).expect("resumed page");
    let expected: Vec<Vec<u8>> = [&b"f3"[..], b"z", b"f6", b"f8", b"f9", b"new"]
        .iter()
        .map(|name| name.to_vec())
        .collect();
    assert_eq!(names(&rest), expected);
    for pair in rest.windows(2) {
        assert!(pair[0].offset < pair[1].offset, "cookies must increase");
    }
}

#[test]
#[ignore = "known bug: rename(a, b) where a and b are hard links to one inode deletes b and renames a, instead of doing nothing (POSIX). Repro: create a; link a b; rename a b; then lookup a fails."]
fn hard_link_rename_onto_its_own_inode_is_a_noop() {
    let (fs, a) = file_fs(b"a");
    fs.link(a, 1, b"b").expect("link");

    assert_eq!(fs.rename(1, b"a", 1, b"b", 0).err(), None);
    assert!(fs.lookup(1, b"a").is_ok(), "a must still exist");
    assert!(fs.lookup(1, b"b").is_ok(), "b must still exist");
    assert_eq!(fs.getattr(a).expect("getattr").nlink, 2);
}

#[test]
#[ignore = "known bug: link() replies with the size stored in the database, so a buffered (unflushed) write is missing from the reply. Repro: create f; write f 0 'abcdef'; link f; reply size is 0, getattr says 6."]
fn link_reply_size_includes_buffered_writes() {
    let (fs, f) = file_fs(b"f");
    fs.write(f, 0, b"abcdef").expect("buffered write");

    let reply = fs.link(f, 1, b"g").expect("link");
    assert_eq!(reply.size, 6);
}

#[test]
#[ignore = "known bug: link() accepts an inode whose last name was removed while it is still open (nlink 0). Linux returns ENOENT. Repro: create f; open f; unlink f; link f g returns Ok(nlink 1); lookup g works; release f then makes lookup g fail (ENOENT), so the new name vanishes."]
fn link_to_an_unlinked_open_file_is_enoent() {
    let (fs, f) = file_fs(b"f");
    let fh = fs.open(f).expect("open");
    fs.unlink(1, b"f").expect("unlink");

    assert_eq!(fs.link(f, 1, b"g").err(), Some(libc::ENOENT));
    fs.release_file(fh).expect("release");
}

#[test]
#[ignore = "known bug (debug builds only; the kernel never sends OPEN for a directory): open() accepts a directory and rmdir() does not clear its open-table slot. The next create reuses the inode number and OpenInodes::insert trips its debug_assert at src/open_table.rs:46. Repro: mkdir d; open d; rmdir d; create_open x."]
fn open_directory_then_rmdir_then_create_panics_in_debug() {
    let db = Db::open_in_memory().expect("open in-memory database");
    let fs = Dbfs::new(db);
    let dir = fs.mkdir(1, b"d", 0o755, 0, 0, 0).expect("mkdir").ino;
    let _fh = fs.open(dir).expect("open directory");
    fs.rmdir(1, b"d").expect("rmdir");

    let (attr, _fh) = fs
        .create_open(1, b"x", 0o644, 0, 0, 0)
        .expect("create after rmdir");
    assert_eq!(attr.kind, FILE);
}

#[test]
#[ignore = "known bug (API only, not reachable through a mount: the kernel caps writes below the file size limit): write() computes offset + length in ChunkSpan::new, so an offset near u64::MAX overflows (debug panic, release wraps). Repro: write(file, u64::MAX - 1, b\"abc\")."]
fn write_near_u64_max_is_efbig_not_an_overflow() {
    let (fs, file) = file_fs(b"f");
    assert_eq!(
        fs.write(file, u64::MAX - 1, b"abc").err(),
        Some(libc::EFBIG)
    );
}
