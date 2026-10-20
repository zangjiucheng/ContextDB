//! All durable I/O goes through `Durability` (ADR 0006 §1). `StdDisk` is the
//! production filesystem; `SimDisk` models synced versus unsynced state so crash
//! tests can tell a missing fsync from a durable write (ADR 0006 §9).
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// Exclusive lock held for the lifetime of an open store.
pub trait LockGuard: Send {}

pub trait Durability: Send + Sync {
    /// Identity used for the process-local poison registry (ADR 0006 §3 step 5).
    fn instance(&self) -> String;
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;
    /// File names (not paths) directly inside `dir`, sorted.
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;
    fn exists(&self, path: &Path) -> bool;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Reads exactly `len` bytes at `offset` (one frame without the segment).
    fn read_range(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>>;
    /// Appends, creating the file when absent. Not durable until `sync`.
    fn append(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    /// Creates or replaces a file's contents. Not durable until `sync`.
    fn write_new(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()>;
    fn remove(&self, path: &Path) -> io::Result<()>;
    /// Atomic replace. The new name is durable only after `sync_dir`.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn sync(&self, path: &Path) -> io::Result<()>;
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
    /// Renames a directory; durable after `sync_dir` of its parent.
    fn rename_dir(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn lock(&self, path: &Path) -> io::Result<Box<dyn LockGuard>>;
}

/// Production filesystem. With `full_sync` (the default), `sync` is `sync_all`,
/// i.e. F_FULLFSYNC on Apple targets. `full_sync: false` uses plain fsync(2) and
/// exists for the "fullfsync off" measurement only (ADR 0006 §10).
#[derive(Clone, Debug)]
pub struct StdDisk {
    pub full_sync: bool,
}
impl Default for StdDisk {
    fn default() -> Self {
        Self { full_sync: true }
    }
}
impl StdDisk {
    fn sync_file(&self, file: &std::fs::File) -> io::Result<()> {
        if self.full_sync {
            file.sync_all()
        } else {
            nix::unistd::fsync(file).map_err(io::Error::from)
        }
    }
}
struct StdLock(#[allow(dead_code)] std::fs::File);
impl LockGuard for StdLock {}
impl Durability for StdDisk {
    fn instance(&self) -> String {
        format!("std-{}", std::process::id())
    }
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            if let Ok(name) = entry?.file_name().into_string() {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
    fn read_range(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let mut out = vec![0; len];
        std::fs::File::open(path)?.read_exact_at(&mut out, offset)?;
        Ok(out)
    }
    fn append(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(data)
    }
    fn write_new(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        std::fs::write(path, data)
    }
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)?
            .set_len(len)
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        self.sync_file(&std::fs::File::open(path)?)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.sync_file(&std::fs::File::open(dir)?)
    }
    fn rename_dir(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn lock(&self, path: &Path) -> io::Result<Box<dyn LockGuard>> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        file.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => {
                io::Error::new(io::ErrorKind::WouldBlock, "store directory is locked")
            }
            std::fs::TryLockError::Error(e) => e,
        })?;
        Ok(Box::new(StdLock(file)))
    }
}

/// What a crash leaves in a file's unsynced suffix (ADR 0006 §9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TailFate {
    /// A prefix of the unsynced bytes survives.
    Torn,
    /// The file is extended, but the unsynced pages read as zeros.
    ZeroFill,
    /// The unsynced pages hold arbitrary bytes.
    Garbage,
    /// Each 4 KiB unsynced page independently persisted or zeroed.
    OutOfOrder,
}
#[derive(Clone, Debug, Default)]
struct Inode {
    data: Vec<u8>,
    synced: Vec<u8>,
}
#[derive(Debug, Default)]
struct SimState {
    inodes: Vec<Inode>,
    /// Live namespace: path -> inode.
    live: BTreeMap<PathBuf, usize>,
    /// Durable namespace per directory, as of its last `sync_dir`.
    durable: BTreeMap<PathBuf, BTreeMap<String, usize>>,
    dirs: BTreeSet<PathBuf>,
    locks: BTreeSet<PathBuf>,
    /// Test fault: every `sync` and `sync_dir` becomes a no-op.
    syncs_disabled: bool,
    /// Test fault: the next mutating call fails with an I/O error.
    fail_next: Option<&'static str>,
}
static SIM_INSTANCES: AtomicU64 = AtomicU64::new(0);

/// Deterministic simulated disk. Clones share state; `crash` returns a fresh
/// instance holding only durable state plus a chosen fate for unsynced tails.
#[derive(Clone, Debug)]
pub struct SimDisk {
    id: u64,
    state: Arc<Mutex<SimState>>,
}
struct SimLock {
    state: Arc<Mutex<SimState>>,
    path: PathBuf,
}
impl LockGuard for SimLock {}
impl Drop for SimLock {
    fn drop(&mut self) {
        if let Ok(mut s) = self.state.lock() {
            s.locks.remove(&self.path);
        }
    }
}
impl Default for SimDisk {
    fn default() -> Self {
        Self::new()
    }
}
fn parent(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}
fn name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}
fn not_found(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, path.display().to_string())
}
impl SimDisk {
    pub fn new() -> Self {
        Self {
            id: SIM_INSTANCES.fetch_add(1, Ordering::Relaxed),
            state: Arc::default(),
        }
    }
    fn state(&self) -> std::sync::MutexGuard<'_, SimState> {
        self.state.lock().expect("sim disk lock")
    }
    /// Fault injection: makes every sync a no-op (the G4 harness self-test).
    pub fn disable_syncs(&self) {
        self.state().syncs_disabled = true;
    }
    /// Fault injection: the next mutating operation named `op` fails.
    pub fn fail_next(&self, op: &'static str) {
        self.state().fail_next = Some(op);
    }
    fn check_fault(s: &mut SimState, op: &'static str) -> io::Result<()> {
        if s.fail_next == Some(op) {
            s.fail_next = None;
            return Err(io::Error::other(format!("injected {op} failure")));
        }
        Ok(())
    }
    /// Durable state after a crash. `seed` drives the fate of each unsynced
    /// region deterministically.
    pub fn crash(&self, fate: TailFate, seed: u64) -> SimDisk {
        let s = self.state();
        let mut rng = seed ^ 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut out = SimState {
            dirs: s.dirs.clone(),
            durable: s.durable.clone(),
            ..SimState::default()
        };
        let mut remap = BTreeMap::new();
        for (dir, entries) in &s.durable {
            for (file, inode) in entries {
                let new = *remap.entry(*inode).or_insert_with(|| {
                    let node = &s.inodes[*inode];
                    let mut data = node.synced.clone();
                    // Unsynced suffix of an append-only extension.
                    if node.data.len() > data.len() && node.data.starts_with(&data) {
                        let tail = &node.data[data.len()..];
                        match fate {
                            TailFate::Torn => {
                                let keep = (next() as usize) % (tail.len() + 1);
                                data.extend_from_slice(&tail[..keep]);
                            }
                            TailFate::ZeroFill => data.extend(std::iter::repeat_n(0, tail.len())),
                            TailFate::Garbage => {
                                data.extend((0..tail.len()).map(|_| next() as u8));
                            }
                            TailFate::OutOfOrder => {
                                for page in tail.chunks(4096) {
                                    if next() % 2 == 0 {
                                        data.extend_from_slice(page);
                                    } else {
                                        data.extend(std::iter::repeat_n(0, page.len()));
                                    }
                                }
                            }
                        }
                    }
                    out.inodes.push(Inode {
                        data: data.clone(),
                        synced: data,
                    });
                    out.inodes.len() - 1
                });
                out.live.insert(dir.join(file), new);
            }
        }
        for entries in out.durable.values_mut() {
            for inode in entries.values_mut() {
                *inode = remap[inode];
            }
        }
        SimDisk {
            id: SIM_INSTANCES.fetch_add(1, Ordering::Relaxed),
            state: Arc::new(Mutex::new(out)),
        }
    }
    /// Live bytes of a file (test inspection).
    pub fn peek(&self, path: &Path) -> Option<Vec<u8>> {
        let s = self.state();
        s.live.get(path).map(|i| s.inodes[*i].data.clone())
    }
    /// Overwrites live and synced bytes in place (media-corruption tests).
    pub fn corrupt(&self, path: &Path, offset: usize, byte: u8) {
        let mut s = self.state();
        if let Some(i) = s.live.get(path).copied() {
            let node = &mut s.inodes[i];
            if offset < node.data.len() {
                node.data[offset] = byte;
            }
            if offset < node.synced.len() {
                node.synced[offset] = byte;
            }
        }
    }
}
impl Durability for SimDisk {
    fn instance(&self) -> String {
        format!("sim-{}", self.id)
    }
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        let mut s = self.state();
        let mut d = dir.to_path_buf();
        loop {
            s.dirs.insert(d.clone());
            match d.parent() {
                Some(p) if !p.as_os_str().is_empty() => d = p.to_path_buf(),
                _ => break,
            }
        }
        Ok(())
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let s = self.state();
        if !s.dirs.contains(dir) {
            return Err(not_found(dir));
        }
        // Files and subdirectories, as `read_dir` lists them.
        let mut names: Vec<_> = s
            .live
            .keys()
            .chain(s.dirs.iter())
            .filter(|p| parent(p) == dir && *p != dir)
            .map(|p| name(p))
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }
    fn exists(&self, path: &Path) -> bool {
        let s = self.state();
        s.live.contains_key(path) || s.dirs.contains(path)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let s = self.state();
        let i = s.live.get(path).ok_or_else(|| not_found(path))?;
        Ok(s.inodes[*i].data.clone())
    }
    fn read_range(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let s = self.state();
        let i = s.live.get(path).ok_or_else(|| not_found(path))?;
        let data = &s.inodes[*i].data;
        usize::try_from(offset)
            .ok()
            .and_then(|start| data.get(start..start.checked_add(len)?))
            .map(<[u8]>::to_vec)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "short read"))
    }
    fn append(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let mut s = self.state();
        Self::check_fault(&mut s, "append")?;
        if !s.dirs.contains(&parent(path)) {
            return Err(not_found(path));
        }
        let i = match s.live.get(path) {
            Some(i) => *i,
            None => {
                s.inodes.push(Inode::default());
                let i = s.inodes.len() - 1;
                s.live.insert(path.to_path_buf(), i);
                i
            }
        };
        s.inodes[i].data.extend_from_slice(data);
        Ok(())
    }
    fn write_new(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let mut s = self.state();
        Self::check_fault(&mut s, "write")?;
        if !s.dirs.contains(&parent(path)) {
            return Err(not_found(path));
        }
        s.inodes.push(Inode {
            data: data.to_vec(),
            synced: Vec::new(),
        });
        let i = s.inodes.len() - 1;
        s.live.insert(path.to_path_buf(), i);
        Ok(())
    }
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        let mut s = self.state();
        Self::check_fault(&mut s, "truncate")?;
        let i = *s.live.get(path).ok_or_else(|| not_found(path))?;
        s.inodes[i].data.truncate(len as usize);
        Ok(())
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        let mut s = self.state();
        s.live.remove(path).ok_or_else(|| not_found(path))?;
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut s = self.state();
        Self::check_fault(&mut s, "rename")?;
        let i = s.live.remove(from).ok_or_else(|| not_found(from))?;
        s.live.insert(to.to_path_buf(), i);
        Ok(())
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        let mut s = self.state();
        Self::check_fault(&mut s, "sync")?;
        if s.syncs_disabled {
            return Ok(());
        }
        let i = *s.live.get(path).ok_or_else(|| not_found(path))?;
        // The file's bytes become durable; its name needs `sync_dir`.
        let node = &mut s.inodes[i];
        node.synced = node.data.clone();
        Ok(())
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        let mut s = self.state();
        Self::check_fault(&mut s, "sync_dir")?;
        if s.syncs_disabled {
            return Ok(());
        }
        let entries = s
            .live
            .iter()
            .filter(|(p, _)| parent(p) == dir)
            .map(|(p, i)| (name(p), *i))
            .collect();
        s.durable.insert(dir.to_path_buf(), entries);
        Ok(())
    }
    /// Simplification: the simulated directory rename is immediately durable
    /// (crash tests cover log/blob/snapshot files, not migrations).
    fn rename_dir(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut s = self.state();
        Self::check_fault(&mut s, "rename")?;
        let moved: Vec<_> = s
            .live
            .keys()
            .filter(|p| p.starts_with(from))
            .cloned()
            .collect();
        for p in moved {
            let i = s.live.remove(&p).expect("listed");
            s.live
                .insert(to.join(p.strip_prefix(from).expect("prefix")), i);
        }
        if let Some(entries) = s.durable.remove(from) {
            s.durable.insert(to.to_path_buf(), entries);
        }
        if s.dirs.remove(from) {
            s.dirs.insert(to.to_path_buf());
        }
        Ok(())
    }
    fn lock(&self, path: &Path) -> io::Result<Box<dyn LockGuard>> {
        let mut s = self.state();
        if !s.locks.insert(path.to_path_buf()) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "store directory is locked",
            ));
        }
        Ok(Box::new(SimLock {
            state: self.state.clone(),
            path: path.to_path_buf(),
        }))
    }
}
