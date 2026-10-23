//! Raw worktree capture. No mtime invalidation and no clean/filter execution.
use crate::{Descriptor, ObjectFormat, Observation, descriptor::unhex, validate_legacy_path};
use contextdb_model::Wire;
use nix::{
    errno::Errno,
    fcntl::{OFlag, openat},
    sys::stat::{Mode, SFlag, fstat},
};
use sha2::Digest;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub const MAX_ANCHORS: usize = 1000;
pub const MAX_FILE: usize = 16 << 20;
pub const MAX_SCAN: usize = 256 << 20;
pub const OBSERVER_TIMEOUT: Duration = Duration::from_secs(5);
pub const LIVE_TIMEOUT: Duration = Duration::from_millis(40);
/// Capture worker pool: at most this many threads, each given at least this
/// many paths, so small scopes stay on one thread.
const MAX_WORKERS: usize = 8;
const PATHS_PER_WORKER: usize = 32;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unavailable(pub String);
impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unavailable {}
fn error(e: impl std::fmt::Display) -> Unavailable {
    Unavailable(e.to_string())
}
fn remaining(deadline: Instant) -> Result<Duration, Unavailable> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| error("capture deadline"))
}

/// Bounded output and deadline; stderr is suppressed and never interpreted as
/// an absent file. Reader/writer threads are joined after kill + reap.
pub fn git(
    root: &Path,
    args: &[&str],
    input: &[u8],
    cap: usize,
    deadline: Instant,
) -> Result<(bool, Vec<u8>), Unavailable> {
    remaining(deadline)?;
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for k in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        cmd.env_remove(k);
    }
    let mut child = cmd.spawn().map_err(error)?;
    let stdin = child.stdin.take().expect("pipe");
    let stdout = child.stdout.take().expect("pipe");
    let input = input.to_vec();
    let write = std::thread::spawn(move || {
        let mut s = stdin;
        s.write_all(&input)
    });
    let read = std::thread::spawn(move || {
        let mut out = Vec::new();
        stdout
            .take(cap as u64 + 1)
            .read_to_end(&mut out)
            .map(|_| out)
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_micros(100)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error("git deadline"));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error(e));
            }
        }
    };
    let wrote = write.join().map_err(|_| error("git stdin thread"))?;
    let out = read
        .join()
        .map_err(|_| error("git stdout thread"))?
        .map_err(error)?;
    let status = status?;
    if out.len() > cap {
        return Err(error("git output limit"));
    }
    wrote.map_err(error)?;
    remaining(deadline)?;
    Ok((status.success(), out))
}
fn line(bytes: &[u8]) -> Result<&str, Unavailable> {
    std::str::from_utf8(bytes).map(str::trim_end).map_err(error)
}

#[derive(Clone, Debug)]
pub struct Repo {
    pub root: PathBuf,
    pub id: String,
    pub format: ObjectFormat,
    git_dir: PathBuf,
    /// Gitlinks decoded by `git ls-files --stage`, keyed by the BLAKE3 of the
    /// exact index bytes they were decoded from. That output depends only on
    /// those bytes (a split index names its shared part by content hash), so
    /// an equal index reuses it and saves one Git process per capture.
    links: Arc<Mutex<Option<LinkCache>>>,
}
/// See [`Repo::identity`].
pub type RepoIdentity = [(u64, u64, u64, i64); 3];
/// Gitlink paths (submodule roots) and the index digest they came from.
type Links = Arc<Vec<Vec<u8>>>;
type LinkCache = ([u8; 32], Links);
impl PartialEq for Repo {
    fn eq(&self, other: &Self) -> bool {
        (&self.root, &self.id, self.format, &self.git_dir)
            == (&other.root, &other.id, other.format, &other.git_dir)
    }
}
impl Eq for Repo {}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    pub head: Vec<u8>,
    pub entries: BTreeMap<String, Observation>,
    pub position: Vec<u8>,
}
impl Repo {
    pub fn open(path: &Path, deadline: Instant) -> Result<Self, Unavailable> {
        let root = path.canonicalize().map_err(error)?;
        let (ok, metadata) = git(
            &root,
            &[
                "rev-parse",
                "--show-toplevel",
                "--show-object-format",
                "--absolute-git-dir",
            ],
            &[],
            48 << 10,
            deadline,
        )?;
        let metadata = line(&metadata)?;
        let fields: Vec<_> = metadata.lines().collect();
        if !ok || fields.len() != 3 || Path::new(fields[0]).canonicalize().map_err(error)? != root {
            return Err(error("expected worktree root"));
        }
        let format = match fields[1] {
            "sha1" => ObjectFormat::Sha1,
            "sha256" => ObjectFormat::Sha256,
            _ => return Err(error("unsupported Git object format")),
        };
        let git_dir = PathBuf::from(fields[2]);
        let name = root.to_str().ok_or_else(|| error("non-UTF8 root"))?;
        let id = blake3::hash(name.as_bytes()).as_bytes()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(Self {
            root,
            id,
            format,
            git_dir,
            links: Arc::default(),
        })
    }
    pub fn scope(&self) -> String {
        format!("code:{}", self.id)
    }
    /// Device, inode, size and mtime of the worktree root, its `.git` entry
    /// (a directory, or the gitfile of a linked worktree, not followed) and
    /// the Git directory. A cached `Repo` is reusable only while all three
    /// are unchanged, so a retargeted gitfile or replaced root reopens it.
    pub fn identity(&self) -> Result<RepoIdentity, Unavailable> {
        let id = |m: std::fs::Metadata| {
            (
                m.dev(),
                m.ino(),
                m.size(),
                m.mtime_nsec() + m.mtime() * 1_000_000_000,
            )
        };
        Ok([
            id(std::fs::metadata(&self.root).map_err(error)?),
            id(std::fs::symlink_metadata(self.root.join(".git")).map_err(error)?),
            id(std::fs::metadata(&self.git_dir).map_err(error)?),
        ])
    }
    fn head(&self, deadline: Instant) -> Result<Vec<u8>, Unavailable> {
        remaining(deadline)?;
        fn text(path: &Path, cap: u64) -> Result<Option<String>, Unavailable> {
            let mut bytes = Vec::new();
            match File::open(path) {
                Ok(f) => {
                    f.take(cap + 1).read_to_end(&mut bytes).map_err(error)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(error(e)),
            }
            if bytes.len() as u64 > cap {
                return Err(error("ref byte limit"));
            }
            String::from_utf8(bytes).map(Some).map_err(error)
        }
        let common = match text(&self.git_dir.join("commondir"), 4096)? {
            Some(p) => self.git_dir.join(p.trim()),
            None => self.git_dir.clone(),
        };
        if common.join("reftable").exists() {
            let (ok, bytes) = git(
                &self.root,
                &["rev-parse", "--verify", "HEAD"],
                &[],
                128,
                deadline,
            )?;
            if !ok {
                return Err(error("HEAD unavailable"));
            }
            return unhex(line(&bytes)?.as_bytes())
                .map_err(error)
                .and_then(|oid| {
                    if oid.len() == self.format.oid_len() {
                        Ok(oid)
                    } else {
                        Err(error("HEAD OID length"))
                    }
                });
        }
        let head = text(&self.git_dir.join("HEAD"), 4096)?.ok_or_else(|| error("missing HEAD"))?;
        let oid = if let Some(branch) = head.trim().strip_prefix("ref: refs/heads/") {
            if branch.is_empty()
                || branch
                    .bytes()
                    .any(|b| b <= b' ' || b == 127 || b"~^:?*[\\".contains(&b))
                || branch.contains("..")
                || branch.contains("@{")
                || branch.ends_with('.')
                || branch
                    .split('/')
                    .any(|p| p.is_empty() || p.starts_with('.') || p.ends_with(".lock"))
            {
                return Err(error("unsupported HEAD ref"));
            }
            let reference = format!("refs/heads/{branch}");
            match text(&common.join(&reference), 128)? {
                Some(value) => Some(value),
                None => {
                    let packed = text(&common.join("packed-refs"), 8 << 20)?.unwrap_or_default();
                    let mut found = None;
                    for line in packed.lines().filter(|l| !l.starts_with('#')) {
                        if let Some(peeled) = line.strip_prefix('^') {
                            if unhex(peeled.as_bytes()).map_err(error)?.len()
                                != self.format.oid_len()
                            {
                                return Err(error("packed peeled OID length"));
                            }
                            continue;
                        }
                        let (oid, name) = line
                            .split_once(' ')
                            .ok_or_else(|| error("malformed packed ref"))?;
                        if unhex(oid.as_bytes()).map_err(error)?.len() != self.format.oid_len()
                            || !name.starts_with("refs/")
                        {
                            return Err(error("malformed packed ref"));
                        }
                        if name == reference {
                            if found.is_some() {
                                return Err(error("duplicate packed HEAD ref"));
                            }
                            found = Some(oid.to_owned());
                        }
                    }
                    found
                }
            }
        } else {
            Some(head)
        };
        let Some(oid) = oid else {
            return Ok(Vec::new());
        };
        let oid = unhex(oid.trim().as_bytes()).map_err(error)?;
        if oid.len() != self.format.oid_len() {
            return Err(error("HEAD OID length"));
        }
        Ok(oid)
    }
    /// Observes one path; returns its bytes too when `keep` (a symbol host
    /// file the observer will parse).
    fn observe(
        &self,
        root: &File,
        path: &str,
        keep: bool,
        total: &AtomicUsize,
        deadline: Instant,
    ) -> Result<(Observation, Option<Vec<u8>>), Unavailable> {
        remaining(deadline)?;
        validate_legacy_path(&self.id, path).map_err(error)?;
        if path.contains('#') {
            // A legacy P5a path; `#` now separates host paths from symbols.
            return Ok((Observation::Broken("reserved-character".into()), None));
        }
        Ok(match read_regular(root, path)? {
            FileRead::Missing => (Observation::Missing, None),
            FileRead::Broken(reason) => (Observation::Broken(reason.into()), None),
            FileRead::Bytes(bytes) => {
                let before = total.fetch_add(bytes.len(), Ordering::Relaxed);
                if before.saturating_add(bytes.len()) > MAX_SCAN {
                    return Err(error("scan byte limit"));
                }
                let d = Descriptor {
                    repo: self.id.clone(),
                    path: path.to_owned(),
                    format: self.format,
                    oid: blob_oid(self.format, &bytes)?,
                };
                d.validate().map_err(error)?;
                (Observation::Present(d), keep.then_some(bytes))
            }
        })
    }
    /// One complete sample of HEAD and every path. Paths are read by a small
    /// bounded worker pool; each worker shares the same open root and deadline
    /// and the first failure fails the whole sample. Gitlinks are applied by
    /// the caller.
    fn once(
        &self,
        paths: &BTreeSet<String>,
        keep: &BTreeSet<String>,
        deadline: Instant,
    ) -> Result<Sample, Unavailable> {
        if paths.len() > MAX_ANCHORS {
            return Err(error("anchor limit"));
        }
        let root = File::open(&self.root).map_err(error)?;
        let before = root.metadata().map_err(error)?;
        let head = self.head(deadline)?;
        let paths: Vec<&String> = paths.iter().collect();
        let total = AtomicUsize::new(0);
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(MAX_WORKERS)
            .min(paths.len().div_ceil(PATHS_PER_WORKER))
            .max(1);
        let chunk = paths.len().div_ceil(workers).max(1);
        type Observed = (Observation, Option<Vec<u8>>);
        let observed: Vec<Result<Vec<Observed>, Unavailable>> = std::thread::scope(|s| {
            let jobs: Vec<_> = paths
                .chunks(chunk)
                .map(|part| {
                    let (root, total) = (&root, &total);
                    s.spawn(move || {
                        part.iter()
                            .map(|p| self.observe(root, p, keep.contains(*p), total, deadline))
                            .collect()
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|j| j.join().unwrap_or_else(|_| Err(error("capture worker"))))
                .collect()
        });
        let mut entries = BTreeMap::new();
        let mut kept = BTreeMap::new();
        let mut names = paths.into_iter();
        for part in observed {
            for (obs, bytes) in part? {
                let name = names.next().expect("one per path");
                if let Some(bytes) = bytes {
                    kept.insert(name.clone(), bytes);
                }
                entries.insert(name.clone(), obs);
            }
        }
        let after = self.root.metadata().map_err(error)?;
        if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
            return Err(error("root replaced"));
        }
        remaining(deadline)?;
        Ok((head, entries, kept))
    }
    /// Two full equal samples. A later external write remains a documented
    /// TOCTOU race, never an atomic guarantee about the next tool action.
    pub fn capture(
        &self,
        paths: &BTreeSet<String>,
        deadline: Instant,
    ) -> Result<Capture, Unavailable> {
        self.capture_keeping(paths, &BTreeSet::new(), deadline)
            .map(|(capture, _)| capture)
    }
    /// As [`Repo::capture`], also returning the bytes of the `keep` paths from
    /// the second sample (equal OIDs to the first): symbol host files the
    /// observer parses (ADR 0012 §5).
    pub fn capture_keeping(
        &self,
        paths: &BTreeSet<String>,
        keep: &BTreeSet<String>,
        deadline: Instant,
    ) -> Result<(Capture, BTreeMap<String, Vec<u8>>), Unavailable> {
        // Read the index bytes around Git's gitlink decode; no mtime cache.
        let index_path = self.git_dir.join("index");
        let index_bytes = || -> Result<Vec<u8>, Unavailable> {
            match File::open(&index_path) {
                Ok(f) => {
                    let mut b = Vec::new();
                    f.take((8 << 20) + 1).read_to_end(&mut b).map_err(error)?;
                    if b.len() > 8 << 20 {
                        return Err(error("index byte limit"));
                    }
                    Ok(b)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(e) => Err(error(e)),
            }
        };
        let before = index_bytes()?;
        let key = *blake3::hash(&before).as_bytes();
        let cached = self
            .links
            .lock()
            .expect("links")
            .as_ref()
            .filter(|(k, _)| *k == key)
            .map(|(_, l)| l.clone());
        // On a miss Git decodes gitlinks while the first sample runs; both
        // samples are classified with the same links once it returns.
        let (links, a) = std::thread::scope(|s| {
            let decode = cached.is_none().then(|| {
                s.spawn(|| {
                    git(
                        &self.root,
                        &["ls-files", "--stage", "-z"],
                        &[],
                        8 << 20,
                        deadline,
                    )
                })
            });
            let a = self.once(paths, &BTreeSet::new(), deadline);
            let links = match (cached, decode) {
                (Some(links), _) => Ok(links),
                (None, Some(job)) => job
                    .join()
                    .unwrap_or_else(|_| Err(error("git thread")))
                    .and_then(|(ok, index)| {
                        if !ok {
                            return Err(error("index unavailable"));
                        }
                        Ok(Arc::new(
                            index
                                .split(|b| *b == 0)
                                .filter(|entry| entry.starts_with(b"160000 "))
                                .filter_map(|entry| {
                                    entry.splitn(2, |b| *b == b'\t').nth(1).map(<[u8]>::to_vec)
                                })
                                .collect(),
                        ))
                    }),
                (None, None) => unreachable!("decode spawned on a miss"),
            };
            (links, a)
        });
        let links = links?;
        let (head_a, mut a, _) = a?;
        let (head, mut b, mut kept) = self.once(paths, keep, deadline)?;
        if before != index_bytes()? {
            return Err(error("index changed during capture"));
        }
        for entries in [&mut a, &mut b] {
            for (path, obs) in entries.iter_mut() {
                if links.iter().any(|link| {
                    path.as_bytes() == link.as_slice()
                        || path
                            .as_bytes()
                            .strip_prefix(link.as_slice())
                            .is_some_and(|suffix| suffix.starts_with(b"/"))
                }) {
                    *obs = Observation::Broken("submodule".into());
                    kept.remove(path);
                }
            }
        }
        if (&head_a, &a) != (&head, &b) {
            return Err(error("unstable worktree"));
        }
        // Only links decoded from an index that stayed byte-identical.
        *self.links.lock().expect("links") = Some((key, links));
        let position = fingerprint(&self.id, &head, &b);
        Ok((
            Capture {
                head,
                entries: b,
                position,
            },
            kept,
        ))
    }
}
/// Whether every component of `path` exists under `root` spelled byte for
/// byte as given. On a case-insensitive volume `src/a.rs` opens `Src/A.rs`;
/// registration rejects that alias instead of silently renaming the file.
pub fn exact_spelling(root: &Path, path: &str) -> Result<bool, Unavailable> {
    let mut dir = root.to_path_buf();
    for part in path.split('/') {
        let found = std::fs::read_dir(&dir)
            .map_err(error)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(error)?
            .iter()
            .any(|name| name.as_encoded_bytes() == part.as_bytes());
        if !found {
            return Ok(false);
        }
        dir.push(part);
    }
    Ok(true)
}
/// HEAD, observations, and the kept host-file bytes of one sample.
type Sample = (
    Vec<u8>,
    BTreeMap<String, Observation>,
    BTreeMap<String, Vec<u8>>,
);
enum FileRead {
    Missing,
    Broken(&'static str),
    Bytes(Vec<u8>),
}
fn read_regular(root: &File, path: &str) -> Result<FileRead, Unavailable> {
    let parts: Vec<_> = path.split('/').collect();
    // The shared root is borrowed for the first component (no dup per file).
    let mut owned: Option<File> = None;
    for (index, part) in parts.iter().enumerate() {
        let dir = owned.as_ref().unwrap_or(root);
        let leaf = index + 1 == parts.len();
        let flags = OFlag::O_RDONLY
            | OFlag::O_CLOEXEC
            | OFlag::O_NOFOLLOW
            | OFlag::O_NONBLOCK
            | if leaf {
                OFlag::empty()
            } else {
                OFlag::O_DIRECTORY
            };
        let fd = match openat(dir, *part, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => return Ok(FileRead::Missing),
            // EOPNOTSUPP: a Unix socket on macOS. All are non-regular
            // replacements: Broken for this anchor, not the whole scope.
            Err(Errno::ELOOP | Errno::ENOTDIR | Errno::ENXIO | Errno::EOPNOTSUPP) => {
                return Ok(FileRead::Broken("unsupported-type"));
            }
            Err(e) => return Err(error(e)),
        };
        let stat = fstat(&fd).map_err(error)?;
        if leaf {
            if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
                return Ok(FileRead::Broken("unsupported-type"));
            }
            if stat.st_size < 0 || stat.st_size as u64 > MAX_FILE as u64 {
                return Err(error("file size limit"));
            }
            let mut bytes = Vec::new();
            File::from(fd)
                .take(MAX_FILE as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(error)?;
            if bytes.len() > MAX_FILE {
                return Err(error("file size limit"));
            }
            return Ok(FileRead::Bytes(bytes));
        }
        let dir = owned.insert(File::from(fd));
        // Nested worktrees and submodules are outside P5a's path domain. Only
        // a definite absence of `.git` continues; any `.git` entry (including
        // a symlink or special file) is Broken; other errors are Unavailable.
        match openat(
            &*dir,
            ".git",
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
            Mode::empty(),
        ) {
            Err(Errno::ENOENT) => {}
            Ok(_) | Err(Errno::ELOOP | Errno::ENXIO | Errno::EOPNOTSUPP) => {
                return Ok(FileRead::Broken("submodule"));
            }
            Err(e) => return Err(error(e)),
        }
    }
    Err(error("empty path"))
}
/// The scope position (ADR 0011 §4, ADR 0012 §6). Without symbol anchors
/// (`versions` empty) it is exactly `CTXPOS01`, so P5a watermarks stay valid.
/// With them it is `CTXPOS02`: the per-language normaliser versions, then one
/// entry per path, where a host-only path (`hosts_only`) carries tag 4 and its
/// raw OID instead of a file descriptor. Symbols themselves never appear:
/// they are a pure function of their host file's bytes.
pub fn position(
    repo: &str,
    head: &[u8],
    entries: &BTreeMap<String, Observation>,
    hosts_only: &BTreeSet<String>,
    versions: &BTreeMap<crate::Language, String>,
) -> Vec<u8> {
    if versions.is_empty() && hosts_only.is_empty() {
        return fingerprint(repo, head, entries);
    }
    let mut bytes = b"CTXPOS02".to_vec();
    1u32.encode(&mut bytes);
    repo.to_owned().encode(&mut bytes);
    head.to_vec().encode(&mut bytes);
    (versions.len() as u32).encode(&mut bytes);
    for version in versions.values() {
        version.encode(&mut bytes);
    }
    (entries.len() as u32).encode(&mut bytes);
    for (path, obs) in entries {
        path.encode(&mut bytes);
        let (tag, value) = match obs {
            Observation::Present(d) if hosts_only.contains(path) => (4u8, d.oid.clone()),
            Observation::Present(d) => (1, d.canonical_bytes()),
            Observation::Missing => (2, Vec::new()),
            Observation::Broken(r) => (3, r.as_bytes().to_vec()),
        };
        tag.encode(&mut bytes);
        value.encode(&mut bytes);
    }
    blake3::hash(&bytes).as_bytes().to_vec()
}
pub fn fingerprint(repo: &str, head: &[u8], entries: &BTreeMap<String, Observation>) -> Vec<u8> {
    let mut bytes = b"CTXPOS01".to_vec();
    1u32.encode(&mut bytes);
    repo.to_owned().encode(&mut bytes);
    head.to_vec().encode(&mut bytes);
    (entries.len() as u32).encode(&mut bytes);
    for (path, obs) in entries {
        path.encode(&mut bytes);
        let (tag, value) = match obs {
            Observation::Present(d) => (1u8, d.canonical_bytes()),
            Observation::Missing => (2, Vec::new()),
            Observation::Broken(r) => (3, r.as_bytes().to_vec()),
        };
        tag.encode(&mut bytes);
        value.encode(&mut bytes);
    }
    blake3::hash(&bytes).as_bytes().to_vec()
}

/// Git-compatible raw blob identity; checked SHA-1 refuses collision attacks.
pub fn blob_oid(format: ObjectFormat, bytes: &[u8]) -> Result<Vec<u8>, Unavailable> {
    let header = format!("blob {}\0", bytes.len());
    match format {
        ObjectFormat::Sha1 => {
            let mut h = sha1_checked::Sha1::new();
            h.update(header.as_bytes());
            h.update(bytes);
            let result = h.try_finalize();
            if result.has_collision() {
                return Err(error("SHA1 collision detected"));
            }
            Ok(result.hash().to_vec())
        }
        ObjectFormat::Sha256 => {
            let mut h = sha2::Sha256::new();
            h.update(header.as_bytes());
            h.update(bytes);
            Ok(h.finalize().to_vec())
        }
    }
}
