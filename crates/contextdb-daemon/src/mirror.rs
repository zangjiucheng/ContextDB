//! The git mirror (ADR 0016 §8): a read-only rendering of a branch's live
//! heads as a commit chain under `refs/contextdb/b<branch id>` in the
//! project repository. Nothing is ever read back into the op_log.
//!
//! - **Render.** `<lane>/<kind>/<key path>.md` per live head (front matter,
//!   then the full body) plus a root `README.md`. Every path segment is
//!   percent-encoded outside `[A-Za-z0-9_-]` (so no segment contains a
//!   `.`), an empty segment is a bare `%`, and only the leaf gets `.md`:
//!   the encoding is injective, and paths equal after case-folding are all
//!   renamed `<leaf>~<logical id>.md`. The tree is a function of the
//!   branch's committed state only.
//! - **Write.** Blobs through one `git fast-import` stream (marks only, no
//!   commit, no ref), trees through `git mktree --batch` (one call per
//!   depth), then `git commit-tree` and a compare-and-swap `git update-ref`
//!   (the zero OID when creating). Every git call has hooks disabled and a
//!   deadline. No body is written to a temporary file.
//! - **Tip check.** A tip equal to the cached commit in `mirror.sqlite` is
//!   accepted. Any other tip is validated by its trailers, the op_log
//!   checksum at its last epoch, and its tree against a replayed render at
//!   that epoch: valid tips are accepted and cached, a different log is
//!   `MirrorStale`, anything else `MirrorModified`.
use crate::actor::Snapshot;
use contextdb_anchors::capture;
use contextdb_core::BranchView;
use contextdb_model::*;
use contextdb_proto::proto::{MirrorProposal, MirrorReport};
use contextdb_store::{Durability, LogReader, Payload};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const FILE: &str = "mirror.sqlite";
pub const REF_PREFIX: &str = "refs/contextdb/b";
pub const BACKUP_PREFIX: &str = "refs/contextdb-backup/b";
/// The render format; part of every tree (`README.md`).
pub const VERSION: u32 = 1;
const UNAVAILABLE: &str = "(payload unavailable)\n";
/// Longest file-name segment most file systems check out.
const NAME_MAX: usize = 255;
/// Per git call.
const GIT_DEADLINE: Duration = Duration::from_secs(120);
const GIT_CAP: usize = 512 << 20;
/// How far back `--force` and `proposals` look for the last valid commit.
const ANCESTORS: usize = 64;

/// Why export refused (ADR 0016 §8.3, §8.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MirrorError {
    /// The tip is not a commit ContextDB would have written.
    Modified { reference: String, found: String },
    /// The tip names a log that no longer exists (`LogRewritten`).
    Stale { reference: String, found: String },
    /// Another export moved the ref in between.
    Raced { reference: String },
    /// Git or the cache failed.
    Failed(String),
}
impl MirrorError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Modified { .. } => "MirrorModified",
            Self::Stale { .. } => "MirrorStale",
            Self::Raced { .. } => "MirrorRaced",
            Self::Failed(_) => "MirrorFailed",
        }
    }
    pub fn detail(&self) -> String {
        match self {
            Self::Modified { reference, found } => format!(
                "{reference} points at {found}, which is not a commit ContextDB wrote for this branch; \
                 `ctx mirror proposals` lists its edits, `--force` moves the ref after a backup"
            ),
            Self::Stale { reference, found } => format!(
                "{reference} points at {found}, exported from a log that no longer exists \
                 (LogRewritten); `--force` moves the ref after a backup"
            ),
            Self::Raced { reference } => {
                format!("{reference} was moved by another export; nothing was changed")
            }
            Self::Failed(m) => m.clone(),
        }
    }
}
type Result<T> = std::result::Result<T, MirrorError>;
fn failed(e: impl std::fmt::Display) -> MirrorError {
    MirrorError::Failed(e.to_string())
}

// ---------------------------------------------------------------- encoding

/// One path segment: bytes outside `[A-Za-z0-9_-]` as `%XX` (uppercase),
/// the empty segment as a bare `%`.
pub fn encode_segment(s: &str) -> String {
    if s.is_empty() {
        return "%".into();
    }
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}
/// The inverse of [`encode_segment`]; `None` for anything it never makes.
pub fn decode_segment(s: &str) -> Option<String> {
    if s == "%" {
        return Some(String::new());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                if hex.bytes().any(|c| c.is_ascii_lowercase()) {
                    return None;
                }
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' => {
                out.push(c);
                i += 1;
            }
            _ => return None,
        }
    }
    let text = String::from_utf8(out).ok()?;
    (encode_segment(&text) == s).then_some(text)
}
/// A record's path segments: lane, kind, then each `/`-separated segment of
/// the key, the last with `.md`.
pub fn record_path(lane: &str, kind: RecordKind, key: &str) -> Vec<String> {
    let mut out = vec![encode_segment(lane), encode_segment(&format!("{kind:?}"))];
    out.extend(key.split('/').map(encode_segment));
    if let Some(last) = out.last_mut() {
        last.push_str(".md");
    }
    out
}
/// The key a path names, when it is one [`record_path`] makes (a `~<id>`
/// rename suffix is dropped).
pub fn key_of_path(path: &str) -> Option<String> {
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() < 3 {
        return None;
    }
    let leaf = segs.last()?.strip_suffix(".md")?;
    let leaf = leaf.split('~').next()?;
    let mut key = Vec::new();
    for s in &segs[2..segs.len() - 1] {
        key.push(decode_segment(s)?);
    }
    key.push(decode_segment(leaf)?);
    Some(key.join("/"))
}

/// A string in double quotes: `\\`, `\"`, `\n`, `\r`, `\t`, and other
/// control characters as `\u{XX}`.
fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                o.push_str(&format!("\\u{{{:02X}}}", c as u32))
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}
fn unquote(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut o = String::new();
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            o.push(c);
            continue;
        }
        match it.next()? {
            '\\' => o.push('\\'),
            '"' => o.push('"'),
            'n' => o.push('\n'),
            'r' => o.push('\r'),
            't' => o.push('\t'),
            'u' => {
                let rest: String = it.by_ref().take_while(|c| *c != '}').collect();
                let hex = rest.strip_prefix('{')?;
                o.push(char::from_u32(u32::from_str_radix(hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(o)
}

// ------------------------------------------------------------------ render

/// A branch's mirror tree: path → file bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    pub files: BTreeMap<String, Vec<u8>>,
    /// Paths with a segment longer than most file systems allow.
    pub long: Vec<String>,
}
/// What the front matter of a rendered file says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Front {
    pub key: String,
    pub logical: LogicalId,
    pub rev: RevisionId,
    pub kind: String,
}
/// Parses the front matter [`render`] writes; returns it and the body.
pub fn parse_file(text: &str) -> Option<(Front, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    let (head, body) = (&rest[..end], &rest[end + 5..]);
    let field = |name: &str| {
        head.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(": "))
    };
    let key = unquote(field("key")?)?;
    let logical = LogicalId(field("logical")?.parse().ok()?);
    let rev = RevisionId(field("rev")?.parse().ok()?);
    let kind = field("kind")?.to_owned();
    Some((
        Front {
            key,
            logical,
            rev,
            kind,
        },
        body,
    ))
}
/// The body of a mirror file: what follows the front matter, or the whole
/// text when it has none (a hand-made file).
pub fn body_of(text: &str) -> &str {
    match text
        .strip_prefix("---\n")
        .and_then(|r| r.find("\n---\n").map(|e| &r[e + 5..]))
    {
        Some(b) => b,
        None => text,
    }
}
fn readme(info: &contextdb_core::BranchInfo) -> Vec<u8> {
    format!(
        "# ContextDB mirror: branch {} (b{})\n\n\
         Generated by ContextDB (mirror format {VERSION}). This ref is generated and \
         **edits are not read back**: `ctx mirror proposals` lists hand edits as supersede \
         proposals, and nothing is applied.\n\n\
         It holds the full body of every live record on the branch. ContextDB never pushes \
         it, but `git push --mirror`, custom refspecs and repository backup tools do.\n",
        info.name, info.id.0
    )
    .into_bytes()
}
/// Renders `view` (ADR 0016 §8.1). `rev_epoch` gives the epoch each
/// revision was created at.
pub fn render<D: Durability>(
    view: &BranchView<'_>,
    log: &LogReader<D>,
    rev_epoch: &dyn Fn(RevisionId) -> Option<Epoch>,
) -> Rendered {
    let mut out = Rendered::default();
    out.files.insert("README.md".into(), readme(view.info()));
    // (path segments, logical, bytes)
    let mut items: Vec<(Vec<String>, LogicalId, Vec<u8>)> = Vec::new();
    for l in view.logicals() {
        let (Some(r), Some((key, kind, lane))) = (view.head(l), view.logical_info(l)) else {
            continue;
        };
        let label = view
            .status(r)
            .map(|s| format!("{s:?}"))
            .unwrap_or_else(|_| "Unavailable".into());
        let mut premises = BTreeSet::new();
        for (_, enabled, incid, refs) in view.rules_concluding(r) {
            if !enabled {
                continue;
            }
            let name = |p: RevisionId| {
                let k = (view.revision_logical(p))
                    .and_then(|pl| view.logical_info(pl))
                    .map_or_else(|| "?".to_owned(), |(k, ..)| k);
                format!("{k}@{}", p.0)
            };
            for i in incid {
                premises.insert(format!("{} ({:?})", name(i.premise), i.kind));
            }
            for p in refs {
                premises.insert(format!("{} (Ref)", name(p)));
            }
        }
        let mut text = String::from("---\n");
        text.push_str(&format!("key: {}\n", quote(&key)));
        text.push_str(&format!("logical: {}\n", l.0));
        text.push_str(&format!("rev: {}\n", r.0));
        text.push_str(&format!("kind: {kind:?}\n"));
        text.push_str(&format!("label: {label}\n"));
        match rev_epoch(r) {
            Some(e) => text.push_str(&format!("rev_epoch: {}\n", e.0)),
            None => text.push_str("rev_epoch: unknown\n"),
        }
        if premises.is_empty() {
            text.push_str("premises: []\n");
        } else {
            text.push_str("premises:\n");
            for p in &premises {
                text.push_str(&format!("  - {}\n", quote(p)));
            }
        }
        text.push_str("---\n");
        let mut bytes = text.into_bytes();
        match view.revision_content(r).map(|c| log.read_blob(&c)) {
            Ok(Payload::Available(b)) => bytes.extend(b),
            _ => bytes.extend(UNAVAILABLE.as_bytes()),
        }
        items.push((record_path(&lane, kind, &key), l, bytes));
    }
    // Case-insensitive file systems: every member of a folded collision is
    // renamed with its logical id.
    let mut folded: BTreeMap<String, usize> = BTreeMap::new();
    for (segs, ..) in &items {
        *folded.entry(segs.join("/").to_lowercase()).or_default() += 1;
    }
    for (mut segs, l, bytes) in items {
        if folded[&segs.join("/").to_lowercase()] > 1 {
            let last = segs.last_mut().expect("leaf");
            let stem = last.strip_suffix(".md").expect("leaf").to_owned();
            *last = format!("{stem}~{}.md", l.0);
        }
        let path = segs.join("/");
        if segs.iter().any(|s| s.len() > NAME_MAX) {
            out.long.push(path.clone());
        }
        out.files.insert(path, bytes);
    }
    out
}

// ------------------------------------------------------------------- cache

/// `mirror.sqlite`: the last commit export wrote per (repository, branch),
/// and the epoch each revision was created at. A cache only.
pub struct Cache {
    conn: Connection,
}
impl Cache {
    pub fn open(path: Option<&Path>) -> Result<Self> {
        let conn = match path {
            Some(p) => Connection::open(p),
            None => Connection::open_in_memory(),
        }
        .map_err(failed)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS tips(
                 repo TEXT NOT NULL, branch INTEGER NOT NULL, commit_id TEXT NOT NULL,
                 epoch INTEGER NOT NULL, PRIMARY KEY (repo, branch));
             CREATE TABLE IF NOT EXISTS rev_epochs(rev INTEGER PRIMARY KEY, epoch INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS rev_watermark(
                 id INTEGER PRIMARY KEY CHECK (id = 0), epoch INTEGER NOT NULL, checksum BLOB NOT NULL);",
        )
        .map_err(failed)?;
        Ok(Self { conn })
    }
    pub fn tip(&self, repo: &str, branch: BranchId) -> Result<Option<(String, Epoch)>> {
        self.conn
            .query_row(
                "SELECT commit_id, epoch FROM tips WHERE repo = ?1 AND branch = ?2",
                params![repo, branch.0],
                |r| Ok((r.get::<_, String>(0)?, Epoch(r.get::<_, i64>(1)? as u64))),
            )
            .optional()
            .map_err(failed)
    }
    pub fn set_tip(&self, repo: &str, branch: BranchId, commit: &str, epoch: Epoch) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO tips(repo, branch, commit_id, epoch) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(repo, branch) DO UPDATE SET commit_id = excluded.commit_id, epoch = excluded.epoch",
                params![repo, branch.0, commit, epoch.0 as i64],
            )
            .map(|_| ())
            .map_err(failed)
    }
    pub fn tips(&self, repo: &str) -> Result<Vec<(BranchId, String, Epoch)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT branch, commit_id, epoch FROM tips WHERE repo = ?1 ORDER BY branch")
            .map_err(failed)?;
        let rows = stmt
            .query_map(params![repo], |r| {
                Ok((
                    BranchId(r.get(0)?),
                    r.get::<_, String>(1)?,
                    Epoch(r.get::<_, i64>(2)? as u64),
                ))
            })
            .map_err(failed)?;
        rows.collect::<rusqlite::Result<_>>().map_err(failed)
    }
    /// Brings the revision epochs up to the log's tail; a log that changed
    /// under the watermark (a truncation) starts over.
    pub fn rev_epochs<D: Durability>(
        &mut self,
        log: &LogReader<D>,
    ) -> Result<BTreeMap<RevisionId, Epoch>> {
        let tail = log.tail();
        let mark: Option<(u64, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT epoch, checksum FROM rev_watermark WHERE id = 0",
                [],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
            )
            .optional()
            .map_err(failed)?;
        let from = match mark {
            Some((e, sum))
                if e <= tail.0
                    && log.checksum_at(Epoch(e)).ok().map(|c| c.to_vec()).as_ref()
                        == Some(&sum) =>
            {
                e
            }
            _ => {
                self.conn
                    .execute_batch("DELETE FROM rev_epochs; DELETE FROM rev_watermark;")
                    .map_err(failed)?;
                0
            }
        };
        let tx = self.conn.transaction().map_err(failed)?;
        for e in from + 1..=tail.0 {
            let result = (log.read_row(Epoch(e)))
                .map_err(|e| failed(format!("op_log: {e:?}")))?
                .result()
                .map_err(failed)?;
            for r in result.allocations.new_revisions {
                tx.execute(
                    "INSERT OR IGNORE INTO rev_epochs(rev, epoch) VALUES (?1, ?2)",
                    params![r.0, e as i64],
                )
                .map_err(failed)?;
            }
        }
        if tail.0 > from {
            let sum = log
                .checksum_at(tail)
                .map_err(|e| failed(format!("{e:?}")))?;
            tx.execute(
                "INSERT INTO rev_watermark(id, epoch, checksum) VALUES (0, ?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET epoch = excluded.epoch, checksum = excluded.checksum",
                params![tail.0 as i64, &sum[..]],
            )
            .map_err(failed)?;
        }
        tx.commit().map_err(failed)?;
        let mut stmt = self
            .conn
            .prepare("SELECT rev, epoch FROM rev_epochs")
            .map_err(failed)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((RevisionId(r.get(0)?), Epoch(r.get::<_, i64>(1)? as u64)))
            })
            .map_err(failed)?;
        rows.collect::<rusqlite::Result<_>>().map_err(failed)
    }
}

// --------------------------------------------------------------------- git

/// The project repository, through the capped, deadline-bounded helper,
/// with hooks disabled.
pub struct Repo {
    pub root: PathBuf,
    /// Where the fast-import marks file goes (the data directory).
    scratch: PathBuf,
}
impl Repo {
    pub fn open(root: &Path, scratch: &Path) -> Result<Self> {
        let r = Self {
            root: root.to_path_buf(),
            scratch: scratch.to_path_buf(),
        };
        let (ok, _) = r.raw(&["rev-parse", "--git-dir"], &[])?;
        if !ok {
            return Err(failed(format!(
                "{} is not a git repository",
                root.display()
            )));
        }
        Ok(r)
    }
    fn raw(&self, args: &[&str], input: &[u8]) -> Result<(bool, Vec<u8>)> {
        let mut all = vec!["-c", "core.hooksPath=/dev/null"];
        all.extend_from_slice(args);
        capture::git(
            &self.root,
            &all,
            input,
            GIT_CAP,
            Instant::now() + GIT_DEADLINE,
        )
        .map_err(|e| failed(format!("git {}: {e}", args.first().unwrap_or(&""))))
    }
    fn run(&self, args: &[&str], input: &[u8]) -> Result<String> {
        match self.raw(args, input)? {
            (true, out) => String::from_utf8(out).map_err(failed),
            (false, _) => Err(failed(format!("git {} failed", args.join(" ")))),
        }
    }
    /// The commit a ref points at, if any.
    pub fn resolve(&self, reference: &str) -> Result<Option<String>> {
        let spec = format!("{reference}^{{commit}}");
        match self.raw(&["rev-parse", "-q", "--verify", &spec], &[])? {
            (true, out) => Ok(Some(
                String::from_utf8(out).map_err(failed)?.trim().to_owned(),
            )),
            (false, _) => {
                // A ref to something other than a commit is still a tip.
                match self.raw(&["rev-parse", "-q", "--verify", reference], &[])? {
                    (true, out) => Ok(Some(
                        String::from_utf8(out).map_err(failed)?.trim().to_owned(),
                    )),
                    (false, _) => Ok(None),
                }
            }
        }
    }
    pub fn has_commit(&self, id: &str) -> Result<bool> {
        Ok(self
            .raw(&["cat-file", "-e", &format!("{id}^{{commit}}")], &[])?
            .0)
    }
    /// `(tree, parents, message)` of a commit; `None` if it is not one.
    pub fn commit(&self, id: &str) -> Result<Option<(String, Vec<String>, String)>> {
        let (ok, out) = self.raw(&["cat-file", "commit", id], &[])?;
        if !ok {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&out).into_owned();
        let Some((head, message)) = text.split_once("\n\n") else {
            return Ok(None);
        };
        let mut tree = None;
        let mut parents = Vec::new();
        for l in head.lines() {
            if let Some(t) = l.strip_prefix("tree ") {
                tree = Some(t.to_owned());
            } else if let Some(p) = l.strip_prefix("parent ") {
                parents.push(p.to_owned());
            }
        }
        Ok(tree.map(|t| (t, parents, message.to_owned())))
    }
    /// Writes the files as blobs and trees; returns the root tree id.
    pub fn write_tree(&self, files: &BTreeMap<String, Vec<u8>>) -> Result<String> {
        // Blobs: one fast-import stream from memory, marks only.
        let mut stream = Vec::new();
        for (n, bytes) in files.values().enumerate() {
            stream.extend(format!("blob\nmark :{}\ndata {}\n", n + 1, bytes.len()).as_bytes());
            stream.extend(bytes);
            stream.push(b'\n');
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let marks = self
            .scratch
            .join(format!("mirror-marks.{}.{n}", std::process::id()));
        let marks_arg = format!("--export-marks={}", marks.display());
        let imported = self.run(&["fast-import", "--quiet", &marks_arg], &stream);
        let read = std::fs::read_to_string(&marks);
        let _ = std::fs::remove_file(&marks);
        imported?;
        let read = read.map_err(|e| failed(format!("fast-import marks: {e}")))?;
        let mut ids: BTreeMap<usize, String> = BTreeMap::new();
        for l in read.lines() {
            if let Some((m, id)) = l.strip_prefix(':').and_then(|l| l.split_once(' ')) {
                ids.insert(m.parse().map_err(failed)?, id.to_owned());
            }
        }
        // Directory → entries `(mode, type, id, name)`.
        let mut dirs: BTreeMap<String, Vec<(String, String, String, String)>> = BTreeMap::new();
        dirs.insert(String::new(), Vec::new());
        for (n, path) in files.keys().enumerate() {
            let id = ids
                .get(&(n + 1))
                .ok_or_else(|| failed("fast-import: a mark is missing"))?;
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
            dirs.entry(dir.to_owned()).or_default().push((
                "100644".into(),
                "blob".into(),
                id.clone(),
                name.to_owned(),
            ));
            // Every ancestor exists.
            let mut d = dir;
            while !d.is_empty() {
                let parent = d.rsplit_once('/').map_or("", |(p, _)| p);
                dirs.entry(parent.to_owned()).or_default();
                d = parent;
            }
        }
        let depth = |d: &str| {
            if d.is_empty() {
                0
            } else {
                d.matches('/').count() + 1
            }
        };
        let deepest = dirs.keys().map(|d| depth(d)).max().unwrap_or(0);
        let mut tree_ids: BTreeMap<String, String> = BTreeMap::new();
        for level in (0..=deepest).rev() {
            let batch: Vec<String> = (dirs.keys())
                .filter(|d| depth(d) == level)
                .cloned()
                .collect();
            let mut input = String::new();
            for d in &batch {
                let mut entries = dirs[d].clone();
                // Subdirectories written at the level below.
                for (child, id) in &tree_ids {
                    let parent = child.rsplit_once('/').map_or("", |(p, _)| p);
                    if parent == d && depth(child) == level + 1 {
                        let name = child.rsplit_once('/').map_or(child.as_str(), |(_, n)| n);
                        entries.push(("040000".into(), "tree".into(), id.clone(), name.to_owned()));
                    }
                }
                for (mode, ty, id, name) in entries {
                    input.push_str(&format!("{mode} {ty} {id}\t{name}\n"));
                }
                input.push('\n');
            }
            let out = self.run(&["mktree", "--batch"], input.as_bytes())?;
            let got: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
            if got.len() != batch.len() {
                return Err(failed("mktree: wrong number of trees"));
            }
            for (d, id) in batch.into_iter().zip(got) {
                tree_ids.insert(d, id.to_owned());
            }
        }
        tree_ids
            .remove("")
            .ok_or_else(|| failed("mktree: no root tree"))
    }
    pub fn commit_tree(&self, tree: &str, parent: Option<&str>, message: &str) -> Result<String> {
        let mut args = vec![
            "-c",
            "user.name=ContextDB",
            "-c",
            "user.email=contextdb@localhost",
            "commit-tree",
            "--no-gpg-sign",
            tree,
        ];
        if let Some(p) = parent {
            args.extend(["-p", p]);
        }
        Ok(self.run(&args, message.as_bytes())?.trim().to_owned())
    }
    /// `update-ref <ref> <new> <old>`, a compare-and-swap; `old = None`
    /// creates (the zero OID). `false` when the ref was not at `old`.
    pub fn cas(&self, reference: &str, new: &str, old: Option<&str>) -> Result<bool> {
        let zero = "0".repeat(new.len());
        let old = old.unwrap_or(&zero);
        Ok(self.raw(&["update-ref", reference, new, old], &[])?.0)
    }
    pub fn delete_ref(&self, reference: &str, old: &str) -> Result<bool> {
        Ok(self.raw(&["update-ref", "-d", reference, old], &[])?.0)
    }
    /// `(ref, id)` of every ref under `prefix`.
    pub fn refs(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let out = self.run(
            &["for-each-ref", "--format=%(refname) %(objectname)", prefix],
            &[],
        )?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(r, id)| (r.to_owned(), id.to_owned()))
            .collect())
    }
    /// `(status, path)` of every file that differs between two trees.
    pub fn diff(&self, a: &str, b: &str) -> Result<Vec<(char, String)>> {
        let out = self.run(
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-renames",
                "--name-status",
                a,
                b,
            ],
            &[],
        )?;
        let mut parts = out.split('\0').filter(|p| !p.is_empty());
        let mut v = Vec::new();
        while let (Some(st), Some(path)) = (parts.next(), parts.next()) {
            v.push((st.chars().next().unwrap_or('?'), path.to_owned()));
        }
        Ok(v)
    }
    /// A file of a tree-ish, as text.
    pub fn show(&self, treeish: &str, path: &str) -> Result<String> {
        let (ok, out) = self.raw(&["cat-file", "blob", &format!("{treeish}:{path}")], &[])?;
        if !ok {
            return Err(failed(format!("no {path} in {treeish}")));
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

// ----------------------------------------------------------------- trailers

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trailers {
    pub branch: BranchId,
    pub range: (Epoch, Epoch),
    pub checksum: String,
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
pub fn message(
    info: &contextdb_core::BranchInfo,
    a: Epoch,
    b: Epoch,
    checksum: &ContentHash,
) -> String {
    format!(
        "contextdb mirror: {} epochs {}..{}\n\n\
         Code-Commit: none\n\
         Ctx-Branch: {} (b{})\n\
         Ctx-Epoch-Range: {}..{}\n\
         Ctx-Log-Checksum: {}\n\
         Ctx-Mirror-Version: {VERSION}\n",
        info.name,
        a.0,
        b.0,
        info.name,
        info.id.0,
        a.0,
        b.0,
        hex(checksum)
    )
}
pub fn parse_trailers(message: &str) -> Option<Trailers> {
    let field = |name: &str| {
        message
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(": "))
    };
    if field("Ctx-Mirror-Version")? != VERSION.to_string() {
        return None;
    }
    let branch = field("Ctx-Branch")?;
    let id = branch.rsplit_once(" (b")?.1.strip_suffix(')')?;
    let (a, b) = field("Ctx-Epoch-Range")?.split_once("..")?;
    Some(Trailers {
        branch: BranchId(id.parse().ok()?),
        range: (Epoch(a.parse().ok()?), Epoch(b.parse().ok()?)),
        checksum: field("Ctx-Log-Checksum")?.to_owned(),
    })
}

// ------------------------------------------------------------------ export

/// What validating a tip found.
enum Tip {
    /// A commit ContextDB wrote for this branch, at its last epoch.
    Valid(Epoch),
    Stale,
    Modified,
}

pub struct Mirror<'a, D: Durability> {
    pub snap: &'a Snapshot<D>,
    pub repo: Repo,
    pub cache: Cache,
    rev_epochs: Option<BTreeMap<RevisionId, Epoch>>,
}
impl<'a, D: Durability> Mirror<'a, D> {
    pub fn new(snap: &'a Snapshot<D>, project: &Path, data: Option<&Path>) -> Result<Self> {
        let scratch = data.map_or_else(std::env::temp_dir, Path::to_path_buf);
        Ok(Self {
            snap,
            repo: Repo::open(project, &scratch)?,
            cache: Cache::open(data.map(|d| d.join(FILE)).as_deref())?,
            rev_epochs: None,
        })
    }
    fn key(&self) -> String {
        self.repo.root.display().to_string()
    }
    fn epochs(&mut self) -> Result<&BTreeMap<RevisionId, Epoch>> {
        if self.rev_epochs.is_none() {
            self.rev_epochs = Some(self.cache.rev_epochs(&self.snap.log)?);
        }
        Ok(self.rev_epochs.as_ref().expect("set"))
    }
    /// The branch rendered at the snapshot.
    pub fn render_now(&mut self, branch: BranchId) -> Result<Rendered> {
        let snap = self.snap;
        let epochs = self.epochs()?.clone();
        let view = snap
            .kernel
            .branch(branch)
            .ok_or_else(|| failed("unknown branch"))?;
        Ok(render(&view, &snap.log, &|r| epochs.get(&r).copied()))
    }
    /// The branch rendered at epoch `e`, by replaying the log (cache miss).
    fn render_at(&mut self, branch: BranchId, e: Epoch) -> Result<Option<Rendered>> {
        let epochs = self.epochs()?.clone();
        let kernel = (self.snap.log)
            .replay_kernel_to(e)
            .map_err(|e| failed(format!("replay: {e:?}")))?;
        let Some(view) = kernel.branch(branch) else {
            return Ok(None);
        };
        Ok(Some(render(&view, &self.snap.log, &|r| {
            epochs.get(&r).copied()
        })))
    }
    /// Validates a tip by its trailers, the log checksum and a replayed
    /// render (ADR 0016 §8.4).
    fn validate(&mut self, branch: BranchId, id: &str) -> Result<Tip> {
        let Some((tree, _, msg)) = self.repo.commit(id)? else {
            return Ok(Tip::Modified);
        };
        let Some(t) = parse_trailers(&msg) else {
            return Ok(Tip::Modified);
        };
        if t.branch != branch || t.range.0 > t.range.1 {
            return Ok(Tip::Modified);
        }
        let b = t.range.1;
        if b > self.snap.log.tail() {
            return Ok(Tip::Stale);
        }
        match self.snap.log.checksum_at(b) {
            Ok(sum) if hex(&sum) == t.checksum => {}
            _ => return Ok(Tip::Stale),
        }
        let Some(r) = self.render_at(branch, b)? else {
            return Ok(Tip::Modified);
        };
        Ok(if self.repo.write_tree(&r.files)? == tree {
            Tip::Valid(b)
        } else {
            Tip::Modified
        })
    }
    /// The first commit at or below `id` (first parents) that is the cached
    /// tip or validates, with its last epoch.
    fn last_valid(&mut self, branch: BranchId, id: &str) -> Result<Option<(String, Epoch)>> {
        let cached = self.cache.tip(&self.key(), branch)?;
        let mut at = Some(id.to_owned());
        for _ in 0..ANCESTORS {
            let Some(c) = at.take() else { break };
            if let Some((cid, e)) = &cached
                && *cid == c
            {
                return Ok(Some((c, *e)));
            }
            if let Tip::Valid(e) = self.validate(branch, &c)? {
                return Ok(Some((c, e)));
            }
            at = self
                .repo
                .commit(&c)?
                .and_then(|(_, p, _)| p.into_iter().next());
        }
        Ok(None)
    }
    /// The tip's last epoch from its trailers (a commit export wrote).
    fn trailer_epoch(&self, id: &str) -> Result<Option<Epoch>> {
        Ok(self
            .repo
            .commit(id)?
            .and_then(|(_, _, m)| parse_trailers(&m))
            .map(|t| t.range.1))
    }
    fn backup(&self, branch: BranchId, id: &str) -> Result<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        for n in 0..100u32 {
            let name = if n == 0 {
                format!("{BACKUP_PREFIX}{}-{now}", branch.0)
            } else {
                format!("{BACKUP_PREFIX}{}-{now}-{n}", branch.0)
            };
            if self.repo.cas(&name, id, None)? {
                return Ok(name);
            }
        }
        Err(failed("could not name a backup ref"))
    }
    /// `ctx mirror export` (ADR 0016 §8.1–§8.4).
    pub fn export(&mut self, branch: BranchId, force: bool, _adopt: bool) -> Result<MirrorReport> {
        let snap = self.snap;
        let info = snap
            .kernel
            .branch(branch)
            .ok_or_else(|| failed("unknown branch"))?
            .info()
            .clone();
        let reference = format!("{REF_PREFIX}{}", branch.0);
        let mut report = MirrorReport::default();
        let found = self.repo.resolve(&reference)?;
        let cached = self.cache.tip(&self.key(), branch)?;
        // The parent: `(commit, its last epoch)`.
        let parent: Option<(String, Epoch)> = match &found {
            None => match cached {
                // A cached commit from a log that was since truncated is not
                // a parent: new commits would follow history the log no
                // longer has. With no ref to overwrite, start fresh.
                Some((c, _)) if self.repo.has_commit(&c)? && !self.log_matches(&c)? => {
                    report.lines.push(format!(
                        "{reference} was deleted and its cached commit {c} names a log that no longer matches: created fresh"
                    ));
                    None
                }
                Some((c, e)) if self.repo.has_commit(&c)? => {
                    report.lines.push(format!(
                        "{reference} was deleted: recreated on the cached commit {c}"
                    ));
                    let e = self.trailer_epoch_or(&c, e)?;
                    Some((c, e))
                }
                Some(_) => {
                    report.lines.push(format!(
                        "{reference} was deleted and its commit is gone: created fresh"
                    ));
                    None
                }
                None => None,
            },
            // The cached tip: accepted if the log it names still exists
            // (one frame checksum; a truncated and regrown log is
            // `MirrorStale`, G4).
            Some(t) if cached.as_ref().is_some_and(|(c, _)| c == t) && !self.log_matches(t)? => {
                let err = MirrorError::Stale {
                    reference: reference.clone(),
                    found: t.clone(),
                };
                if !force {
                    return Err(err);
                }
                let saved = self.backup(branch, t)?;
                report
                    .lines
                    .push(format!("{}: saved as {saved}", err.code()));
                None
            }
            Some(t) if cached.as_ref().is_some_and(|(c, _)| c == t) => {
                let e = cached.as_ref().map(|(_, e)| *e).expect("cached");
                Some((t.clone(), self.trailer_epoch_or(t, e)?))
            }
            Some(t) => match self.validate(branch, t)? {
                Tip::Valid(e) => {
                    self.cache.set_tip(&self.key(), branch, t, e)?;
                    report
                        .lines
                        .push(format!("accepted {t}: not cached, and it validates"));
                    Some((t.clone(), e))
                }
                bad => {
                    let err = match bad {
                        Tip::Stale => MirrorError::Stale {
                            reference: reference.clone(),
                            found: t.clone(),
                        },
                        _ => MirrorError::Modified {
                            reference: reference.clone(),
                            found: t.clone(),
                        },
                    };
                    if !force {
                        return Err(err);
                    }
                    let saved = self.backup(branch, t)?;
                    report
                        .lines
                        .push(format!("{}: saved as {saved}", err.code()));
                    match bad {
                        Tip::Stale => None,
                        _ => self.last_valid(branch, t)?,
                    }
                }
            },
        };
        let rendered = self.render_now(branch)?;
        for p in &rendered.long {
            report.lines.push(format!(
                "warning: a segment of {p} is over {NAME_MAX} bytes; checking it out may fail"
            ));
        }
        let tree = self.repo.write_tree(&rendered.files)?;
        let b = snap.epoch;
        if let Some((p, pe)) = &parent {
            let ptree = self.repo.commit(p)?.map(|(t, ..)| t);
            if ptree.as_deref() == Some(tree.as_str()) && found.as_deref() == Some(p.as_str()) {
                self.cache.set_tip(&self.key(), branch, p, *pe)?;
                report.lines.push(format!(
                    "{reference} unchanged at {p} (epochs ..{}): no commit",
                    pe.0
                ));
                return Ok(report);
            }
        }
        let a = parent.as_ref().map_or(Epoch(1), |(_, e)| Epoch(e.0 + 1));
        let a = Epoch(a.0.min(b.0));
        let sum = snap
            .log
            .checksum_at(b)
            .map_err(|e| failed(format!("{e:?}")))?;
        let commit = self.repo.commit_tree(
            &tree,
            parent.as_ref().map(|(p, _)| p.as_str()),
            &message(&info, a, b, &sum),
        )?;
        crate::p8_crash_point("mirror-commit-tree");
        if !self.repo.cas(&reference, &commit, found.as_deref())? {
            // Raced only if the ref really moved; otherwise git refused
            // (a held lock such as `<ref>.lock`, or a full disk).
            if self.repo.resolve(&reference)? != found {
                return Err(MirrorError::Raced { reference });
            }
            return Err(failed(format!(
                "git update-ref {reference} failed with the ref unchanged (a held lock, or the disk?)"
            )));
        }
        crate::p8_crash_point("mirror-update-ref");
        self.cache.set_tip(&self.key(), branch, &commit, b)?;
        report.lines.push(format!(
            "{reference} -> {commit} (branch {}, epochs {}..{})",
            info.name, a.0, b.0
        ));
        report.lines.push(
            "the mirror holds every live record's full body; `git push --mirror` and backup tools send it"
                .into(),
        );
        Ok(report)
    }
    /// Whether the log a commit's trailers name is still this log: its last
    /// epoch exists and has the recorded frame checksum.
    fn log_matches(&self, id: &str) -> Result<bool> {
        let Some(t) = self
            .repo
            .commit(id)?
            .and_then(|(_, _, m)| parse_trailers(&m))
        else {
            return Ok(false);
        };
        Ok(t.range.1 <= self.snap.log.tail()
            && self
                .snap
                .log
                .checksum_at(t.range.1)
                .is_ok_and(|sum| hex(&sum) == t.checksum))
    }
    fn trailer_epoch_or(&self, id: &str, cached: Epoch) -> Result<Epoch> {
        Ok(self.trailer_epoch(id)?.unwrap_or(cached))
    }
    /// `ctx mirror proposals` (ADR 0016 §8.4): the found tip's edits against
    /// the last valid commit, as supersede proposals. Nothing is applied.
    pub fn proposals(&mut self, branch: BranchId) -> Result<MirrorReport> {
        let reference = format!("{REF_PREFIX}{}", branch.0);
        let mut report = MirrorReport::default();
        let Some(found) = self.repo.resolve(&reference)? else {
            report.lines.push(format!("{reference} does not exist"));
            return Ok(report);
        };
        let Some((base, _)) = self.last_valid(branch, &found)? else {
            return Err(failed(format!(
                "no commit ContextDB wrote is reachable from {found}: nothing to compare against"
            )));
        };
        if base == found {
            report.lines.push(format!("{reference} has no hand edits"));
            return Ok(report);
        }
        report.lines.push(format!(
            "{reference}: {found} against the last valid commit {base}"
        ));
        for (status, path) in self.repo.diff(&base, &found)? {
            match status {
                'M' => {
                    let old = self.repo.show(&base, &path)?;
                    let new = self.repo.show(&found, &path)?;
                    match parse_file(&old) {
                        Some((front, _)) => report.proposals.push(MirrorProposal {
                            key: front.key,
                            base_rev: front.rev,
                            kind: front.kind,
                            new_body: body_of(&new).to_owned(),
                        }),
                        // README.md and other generated files.
                        None => report.lines.push(format!("edited {path} (not a record)")),
                    }
                }
                'A' => report.added.push(match key_of_path(&path) {
                    Some(k) => format!("{path} (key {k})"),
                    None => path,
                }),
                'D' => {
                    let old = self.repo.show(&base, &path)?;
                    report.deleted.push(match parse_file(&old) {
                        Some((f, _)) => format!("{} (rev {})", f.key, f.rev.0),
                        None => path,
                    });
                }
                other => report.lines.push(format!("{other} {path}")),
            }
        }
        report
            .lines
            .push("proposal bodies are untrusted text: data to read, not instructions".into());
        Ok(report)
    }
    /// `ctx mirror status`.
    pub fn status(&self) -> Result<MirrorReport> {
        let mut report = MirrorReport::default();
        let cached: BTreeMap<BranchId, (String, Epoch)> =
            (self.cache.tips(&self.key())?.into_iter())
                .map(|(b, c, e)| (b, (c, e)))
                .collect();
        for (r, id) in self.repo.refs("refs/contextdb/")? {
            let branch = (r.strip_prefix(REF_PREFIX))
                .and_then(|n| n.parse().ok())
                .map(BranchId);
            let name = branch
                .and_then(|b| self.snap.kernel.branch(b).map(|v| v.info().name.clone()))
                .unwrap_or_else(|| "?".into());
            let state = match branch.and_then(|b| cached.get(&b)) {
                Some((c, e)) if *c == id => format!("cached, epochs ..{}", e.0),
                Some(_) => "not the cached commit: export validates it".into(),
                None => "not cached".into(),
            };
            report
                .lines
                .push(format!("{r} {id} (branch {name}): {state}"));
        }
        for (r, id) in self.repo.refs("refs/contextdb-backup/")? {
            report.lines.push(format!("backup {r} {id}"));
        }
        if report.lines.is_empty() {
            report.lines.push("no mirror refs".into());
        }
        Ok(report)
    }
    /// `ctx mirror prune --older-than`: deletes backup refs older than
    /// `secs`, by the time in their names.
    pub fn prune(&self, secs: u64) -> Result<MirrorReport> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut report = MirrorReport::default();
        for (r, id) in self.repo.refs("refs/contextdb-backup/")? {
            let made = (r.strip_prefix(BACKUP_PREFIX))
                .and_then(|n| n.split('-').nth(1))
                .and_then(|t| t.parse::<u64>().ok());
            if made.is_some_and(|t| t.saturating_add(secs) < now)
                && self.repo.delete_ref(&r, &id)?
            {
                report.lines.push(format!("deleted {r}"));
            }
        }
        if report.lines.is_empty() {
            report.lines.push("nothing to prune".into());
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_injective_and_never_dot_names() {
        let keys = [
            "x", "x.md/y", "a//b", "a/./b", ".", "..", ".git", "A", "a", "", "%", "%25", "é/ü",
            "a b", "a/b/", "/a",
        ];
        let mut seen = BTreeSet::new();
        for k in keys {
            let p = record_path("repo", RecordKind::Fact, k).join("/");
            assert!(seen.insert(p.clone()), "{k} collides at {p}");
            for seg in p.split('/') {
                assert!(!matches!(seg, "." | ".." | ".git"), "{p}");
                let stem = seg.strip_suffix(".md").unwrap_or(seg);
                assert!(!stem.contains('.'), "{p}");
            }
            assert_eq!(key_of_path(&p).as_deref(), Some(k), "{p}");
        }
        assert_eq!(
            record_path("repo", RecordKind::Fact, "x").join("/"),
            "repo/Fact/x.md"
        );
        assert_eq!(
            record_path("repo", RecordKind::Fact, "x.md/y").join("/"),
            "repo/Fact/x%2Emd/y.md"
        );
        assert_eq!(
            record_path("repo", RecordKind::Fact, "a//b").join("/"),
            "repo/Fact/a/%/b.md"
        );
        assert_eq!(
            record_path("repo", RecordKind::Fact, "a/./b").join("/"),
            "repo/Fact/a/%2E/b.md"
        );
        assert_eq!(decode_segment("%2e"), None, "lowercase hex is never made");
        assert_eq!(decode_segment("a.b"), None);
    }

    /// T97: a property over random keys: the encoding is injective and
    /// decodes back.
    #[test]
    fn t97_encoding_is_injective_over_random_keys() {
        let alphabet: Vec<char> = "aA./%_- é~\\".chars().collect();
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 7) as usize;
            let key: String = (0..len)
                .map(|i| alphabet[((x >> (i * 5)) % alphabet.len() as u64) as usize])
                .collect();
            let p = record_path("l", RecordKind::Plan, &key).join("/");
            if let Some(other) = seen.insert(p.clone(), key.clone()) {
                assert_eq!(other, key, "{p}");
            }
            assert_eq!(key_of_path(&p).as_deref(), Some(key.as_str()));
        }
    }

    #[test]
    fn quoting_round_trips() {
        for s in [
            "plain",
            "a \"q\" \\ b",
            "line\nbreak\ttab\r",
            "\u{1}\u{7f}",
            "é",
        ] {
            assert_eq!(unquote(&quote(s)).as_deref(), Some(s));
        }
    }

    #[test]
    fn trailers_round_trip() {
        let info = contextdb_core::BranchInfo {
            id: BranchId(3),
            name: "feat (x)".into(),
            parent: None,
            fork_epoch: Epoch(0),
            archived: false,
            creator: PrincipalId::STORE_OWNER,
        };
        let m = message(&info, Epoch(4), Epoch(9), &[0xab; 32]);
        assert_eq!(
            parse_trailers(&m),
            Some(Trailers {
                branch: BranchId(3),
                range: (Epoch(4), Epoch(9)),
                checksum: "ab".repeat(32),
            })
        );
        assert_eq!(parse_trailers("hand-made commit\n"), None);
    }
}
