//! A joined team on disk and its import (ADR 0018 §4.2, §8.1–8.2, §8.6–8.7).
//!
//! `<dir>/anchor` names the team, the remote and the genesis (or checkpoint)
//! entry given out of band. `<dir>/log` is an append-only log of every
//! decided entry with the bytes `accept` read: on open the replica is the
//! log replayed through `accept`, which is deterministic, so a restart
//! reaches the same state and decisions; a torn tail is cut and re-imported
//! from git. Import walks the tracking ref's first-parent path from the
//! trusted anchor of the current generation and decides each entry once.

use crate::catchup::{self, History};
use crate::commit::{ObjectFormat, hex};
use crate::enc::{Reader, Writer};
use crate::git::{GitError, Repo};
use crate::replica::{EntryInput, JoinError, Outcome, Reason, Replica};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const FRAME: &[u8; 4] = b"CXTL";
/// Entries read from git per batch on import.
const IMPORT_BATCH: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    UpToDate,
    Imported(usize),
    /// An accepted `Compact` whose successor ref is not there yet.
    CompactPending,
    /// The tracking ref is gone (the remote deleted it, then a prune).
    RootMissing,
    /// The ref's history is not a continuation of the trusted chain.
    Diverged(String),
    /// Objects to fetch by id (a partial clone).
    Incomplete(Vec<String>),
    /// An entry this build cannot decide (unsupported `accept_version`).
    Stalled(String),
    InvalidSuccessor(Reason),
}

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Corrupt(String),
    Join(JoinError),
    Git(GitError),
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Entry = 0,
    Successor = 1,
    Join = 2,
}

struct Frame {
    kind: Kind,
    oid: String,
    seq: u64,
    first: bool,
    commit: Vec<u8>,
    tree: Vec<(String, Vec<u8>)>,
}

impl Frame {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(self.kind as u8)
            .str(&self.oid)
            .u64(self.seq)
            .bool(self.first)
            .bytes(&self.commit);
        w.count(self.tree.len());
        for (p, b) in &self.tree {
            w.str(p).bytes(b);
        }
        let payload = w.0;
        let mut out = FRAME.to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
        out.extend_from_slice(blake3::hash(&payload).as_bytes());
        out
    }

    /// One frame from the front of `buf`; `None` for a torn or damaged tail.
    fn decode(buf: &[u8]) -> Option<(Self, usize)> {
        if buf.len() < 8 || &buf[..4] != FRAME {
            return None;
        }
        let n = u32::from_be_bytes(buf[4..8].try_into().ok()?) as usize;
        let end = 8usize.checked_add(n)?.checked_add(32)?;
        if buf.len() < end {
            return None;
        }
        let payload = &buf[8..8 + n];
        if blake3::hash(payload).as_bytes() != &buf[8 + n..end] {
            return None;
        }
        let mut r = Reader(payload);
        let kind = match r.u8().ok()? {
            0 => Kind::Entry,
            1 => Kind::Successor,
            2 => Kind::Join,
            _ => return None,
        };
        let oid = r.str().ok()?.to_owned();
        let seq = r.u64().ok()?;
        let first = r.bool().ok()?;
        let commit = r.bytes().ok()?.to_vec();
        let k = r.count(8).ok()?;
        let mut tree = Vec::with_capacity(k);
        for _ in 0..k {
            tree.push((r.str().ok()?.to_owned(), r.bytes().ok()?.to_vec()));
        }
        r.end().ok()?;
        Some((
            Self {
                kind,
                oid,
                seq,
                first,
                commit,
                tree,
            },
            end,
        ))
    }
}

/// A joined team: its anchor, durable log and replica.
pub struct TeamStore {
    dir: PathBuf,
    team: [u8; 16],
    remote: String,
    format: ObjectFormat,
    replica: Replica,
    /// The trusted first entry of the current generation.
    anchor: String,
    /// The last decided entry of the current generation.
    last: Option<String>,
    log: File,
    /// Every decided proposal: `(member, proposal id)` → its outcome, from
    /// the entries imported (the publish side reads its outcomes here).
    decisions: BTreeMap<(String, [u8; 16]), Result<(), Reason>>,
    /// The projection after every entry this edge decided (§5.2): what
    /// catch-up splits by. Rebuilt by the log replay at open.
    history: History,
}

/// `(author, proposal ids)` of an entry, from its trailers.
fn proposers(format: ObjectFormat, commit: &[u8]) -> Option<(String, Vec<[u8; 16]>)> {
    let c = crate::commit::parse(commit, format).ok()?;
    Some((c.trailers.author?, c.trailers.proposals))
}

fn write_anchor_file(
    dir: &Path,
    team: [u8; 16],
    format: ObjectFormat,
    remote: &str,
    anchor: &str,
) -> std::io::Result<()> {
    let fmt = match format {
        ObjectFormat::Sha1 => "sha1",
        ObjectFormat::Sha256 => "sha256",
    };
    write_atomic(
        &dir.join("anchor"),
        format!(
            "team {}\nformat {fmt}\nremote {remote}\nanchor {anchor}\n",
            hex(&team)
        )
        .as_bytes(),
    )
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(d) = path.parent() {
        File::open(d)?.sync_all()?;
    }
    Ok(())
}

impl TeamStore {
    /// Joins at genesis: records the out-of-band anchor; import does the rest.
    pub fn create(
        dir: &Path,
        team: [u8; 16],
        format: ObjectFormat,
        remote: &str,
        genesis: &str,
    ) -> Result<Self, StoreError> {
        std::fs::create_dir_all(dir)?;
        write_anchor_file(dir, team, format, remote, genesis)?;
        Self::open_with(dir, false)
    }

    /// Joins from a checkpoint: `first` is a later generation's first entry,
    /// given out of band, read from the repository and verified.
    pub fn create_from_checkpoint(
        dir: &Path,
        team: [u8; 16],
        repo: &Repo,
        remote: &str,
        first: &str,
    ) -> Result<Self, StoreError> {
        let objs = repo.read_entry(first).map_err(StoreError::Git)?;
        let r = Replica::join_checkpoint(team, repo.format(), &objs.commit, &objs.tree)
            .map_err(StoreError::Join)?;
        // The Join frame is durable before the anchor names the checkpoint:
        // an anchor without it would open as a join at genesis (review
        // finding 12). A crash before the anchor leaves no joined team.
        std::fs::create_dir_all(dir)?;
        let frame = Frame {
            kind: Kind::Join,
            oid: first.to_owned(),
            seq: r.seq().expect("joined"),
            first: true,
            commit: objs.commit,
            tree: objs.tree,
        };
        write_atomic(&dir.join("log"), &frame.encode())?;
        write_anchor_file(dir, team, repo.format(), remote, first)?;
        Self::open_with(dir, false)
    }

    /// Opens a joined team and replays its log; a torn tail is truncated.
    /// A `log.rebuild` left by an interrupted [`Self::rebuild`] is put
    /// back first (review finding 12): the rebuild runs again on request.
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        Self::open_with(dir, true)
    }

    fn open_with(dir: &Path, restore: bool) -> Result<Self, StoreError> {
        let aside = dir.join("log.rebuild");
        if restore && aside.exists() {
            std::fs::rename(&aside, dir.join("log"))?;
            File::open(dir)?.sync_all()?;
        }
        let text = std::fs::read_to_string(dir.join("anchor"))?;
        let field = |k: &str| -> Result<String, StoreError> {
            text.lines()
                .find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix(' ')))
                .map(str::to_owned)
                .ok_or_else(|| StoreError::Corrupt(format!("anchor: no {k}")))
        };
        let team = crate::commit::parse_id128(&field("team")?)
            .ok_or(StoreError::Corrupt("team".into()))?;
        let format = match field("format")?.as_str() {
            "sha1" => ObjectFormat::Sha1,
            "sha256" => ObjectFormat::Sha256,
            _ => return Err(StoreError::Corrupt("format".into())),
        };
        let remote = field("remote")?;
        let anchor = field("anchor")?;
        let path = dir.join("log");
        let mut buf = Vec::new();
        if path.exists() {
            File::open(&path)?.read_to_end(&mut buf)?;
        }
        let mut frames = Vec::new();
        let mut at = 0;
        while let Some((f, n)) = Frame::decode(&buf[at..]) {
            frames.push(f);
            at += n;
        }
        let log = OpenOptions::new().create(true).append(true).open(&path)?;
        if at < buf.len() {
            log.set_len(at as u64)?;
            log.sync_all()?;
        }
        let mut s = Self {
            dir: dir.to_owned(),
            team,
            remote,
            format,
            replica: Replica::new(team, format),
            anchor,
            last: None,
            log,
            decisions: BTreeMap::new(),
            history: History::default(),
        };
        for f in frames {
            s.replay(&f)?;
        }
        Ok(s)
    }

    fn replay(&mut self, f: &Frame) -> Result<(), StoreError> {
        if f.kind == Kind::Join {
            self.replica = Replica::join_checkpoint(self.team, self.format, &f.commit, &f.tree)
                .map_err(StoreError::Join)?;
            self.anchor = f.oid.clone();
            self.last = Some(f.oid.clone());
            self.history =
                History::starting_at(self.last.clone(), catchup::projection(&self.replica));
            return Ok(());
        }
        let out = self.replica.accept(&EntryInput {
            oid: &f.oid,
            seq: f.seq,
            first_in_generation: f.first,
            commit: &f.commit,
            tree: &f.tree,
        });
        match (f.kind, out) {
            (Kind::Successor, Outcome::Successor) => self.anchor = f.oid.clone(),
            (Kind::Entry, o @ (Outcome::Decided(_) | Outcome::EntryRejected(_))) => {
                self.record_decisions(&f.commit, &o)
            }
            (_, o) => {
                return Err(StoreError::Corrupt(format!(
                    "log replay of {}: {o:?}",
                    f.oid
                )));
            }
        }
        self.advanced(&f.oid);
        Ok(())
    }

    fn write_anchor(&self) -> Result<(), StoreError> {
        write_anchor_file(
            &self.dir,
            self.team,
            self.format,
            &self.remote,
            &self.anchor,
        )?;
        Ok(())
    }

    /// `ctx team rebuild` (§8.5): discards the replica and replays it from
    /// the trusted anchor — the genesis, or the last verified checkpoint
    /// (Q13) — through import. The old log is kept until the new one is
    /// complete; on failure it is put back.
    pub fn rebuild(dir: &Path, repo: &Repo) -> Result<(Self, Status), StoreError> {
        let old = Self::open(dir)?;
        let (team, remote, anchor) = (old.team, old.remote.clone(), old.anchor.clone());
        drop(old);
        let log = dir.join("log");
        let aside = dir.join("log.rebuild");
        if log.exists() {
            std::fs::rename(&log, &aside)?;
        }
        let built = (|| -> Result<(Self, Status), StoreError> {
            let objs = repo.read_entry(&anchor).map_err(StoreError::Git)?;
            let checkpoint = crate::commit::parse(&objs.commit, repo.format())
                .map_err(|e| StoreError::Corrupt(format!("anchor: {e:?}")))?
                .trailers
                .compacts
                .is_some();
            let mut s = if checkpoint {
                Self::create_from_checkpoint(dir, team, repo, &remote, &anchor)?
            } else {
                Self::create(dir, team, repo.format(), &remote, &anchor)?
            };
            let status = s.import(repo)?;
            Ok((s, status))
        })();
        match built {
            Ok(v) => {
                let _ = std::fs::remove_file(&aside);
                Ok(v)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&log);
                if aside.exists() {
                    std::fs::rename(&aside, &log)?;
                }
                Err(e)
            }
        }
    }

    /// `oid` is decided (or followed as a successor): it is the tip.
    fn advanced(&mut self, oid: &str) {
        self.last = Some(oid.to_owned());
        // Only what the entry may have changed (G5); a replica that cannot
        // say gives the whole projection.
        match self.replica.touched() {
            Some(paths) => {
                let changed = catchup::projection_of(&self.replica, paths);
                self.history.record_partial(oid, changed);
            }
            None => self.history.record(oid, catchup::projection(&self.replica)),
        }
        #[cfg(debug_assertions)]
        assert_eq!(
            self.history.current(),
            &catchup::projection(&self.replica),
            "the catch-up history drifted"
        );
    }

    /// The projection after every entry decided here, for catch-up.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// The team id.
    pub fn team(&self) -> [u8; 16] {
        self.team
    }

    fn append(&mut self, frames: &[Frame]) -> Result<(), StoreError> {
        self.append_unsynced(frames)?;
        self.log.sync_data()?;
        Ok(())
    }

    /// Import's appends: synced once when `import` returns (G5). A crash
    /// before that loses a tail import redoes from git; nothing outside the
    /// replica (no catch-up) has used it yet, since catch-up runs after
    /// `import` returns.
    fn append_unsynced(&mut self, frames: &[Frame]) -> Result<(), StoreError> {
        let mut bytes = Vec::new();
        for f in frames {
            bytes.extend_from_slice(&f.encode());
        }
        self.log.write_all(&bytes)?;
        Ok(())
    }

    fn record_decisions(&mut self, commit: &[u8], o: &Outcome) {
        let Some((author, ids)) = proposers(self.format, commit) else {
            return;
        };
        // A rejected entry whose author did not sign it decides nothing: its
        // trailers are anyone's words.
        if matches!(o, Outcome::EntryRejected(_)) && !self.replica.signed_by_author(commit) {
            return;
        }
        for (i, id) in ids.iter().enumerate() {
            let d = match o {
                Outcome::Decided(v) => v.get(i).cloned().unwrap_or(Ok(())),
                Outcome::EntryRejected(r) => Err(r.clone()),
                _ => continue,
            };
            // The first decision stands: a later replay of the same id is a
            // DuplicateProposal of an accepted one.
            self.decisions.entry((author.clone(), *id)).or_insert(d);
        }
    }
    /// The outcome of `(member, proposal id)`, if an imported entry decided
    /// it.
    pub fn decision(&self, member: &str, id: &[u8; 16]) -> Option<&Result<(), Reason>> {
        self.decisions.get(&(member.to_owned(), *id))
    }
    /// The last decided entry of the current generation (the parent of the
    /// next publish).
    pub fn tip(&self) -> Option<&str> {
        self.last.as_deref()
    }
    pub fn replica(&self) -> &Replica {
        &self.replica
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The tracking ref of generation `g`.
    pub fn tracking_ref(&self, g: u32) -> String {
        format!(
            "refs/contextdb/remotes/{}/team/{}/g{g}",
            self.remote,
            hex(&self.team)
        )
    }

    /// Imports what the tracking refs hold (§8.2): decides every new entry
    /// once, follows a verified successor, and stops on anything it cannot
    /// prove continues the trusted chain.
    pub fn import(&mut self, repo: &Repo) -> Result<Status, StoreError> {
        let r = self.import_unsynced(repo);
        self.log.sync_data()?;
        r
    }

    fn import_unsynced(&mut self, repo: &Repo) -> Result<Status, StoreError> {
        let mut imported = 0usize;
        loop {
            if let Some(p) = self.replica.pending().cloned() {
                let Some(tip) = repo
                    .resolve(&self.tracking_ref(p.generation))
                    .map_err(StoreError::Git)?
                else {
                    return Ok(if imported > 0 {
                        Status::Imported(imported)
                    } else {
                        Status::CompactPending
                    });
                };
                let path = match repo.first_parent_path(&tip) {
                    Ok(p) => p,
                    Err(GitError::Missing(m)) => return Ok(Status::Incomplete(m)),
                    Err(e) => return Err(StoreError::Git(e)),
                };
                let root = path[0].clone();
                let objs = match repo.read_entry(&root) {
                    Ok(o) => o,
                    Err(GitError::Missing(m)) => return Ok(Status::Incomplete(m)),
                    Err(e) => return Err(StoreError::Git(e)),
                };
                let seq = p.compact_seq + 1;
                let out = self.replica.accept(&EntryInput {
                    oid: &root,
                    seq,
                    first_in_generation: true,
                    commit: &objs.commit,
                    tree: &objs.tree,
                });
                match out {
                    Outcome::Successor => {
                        self.append(&[Frame {
                            kind: Kind::Successor,
                            oid: root.clone(),
                            seq,
                            first: true,
                            commit: objs.commit,
                            tree: objs.tree,
                        }])?;
                        self.anchor = root.clone();
                        // Q13: the verified successor is the trust anchor a
                        // rebuild replays from after a prune.
                        self.write_anchor()?;
                        self.advanced(&root);
                        imported += 1;
                        continue;
                    }
                    Outcome::InvalidSuccessor(r) => return Ok(Status::InvalidSuccessor(r)),
                    other => return Err(StoreError::Corrupt(format!("successor: {other:?}"))),
                }
            }
            let g = self.replica.generation();
            let Some(tip) = repo
                .resolve(&self.tracking_ref(g))
                .map_err(StoreError::Git)?
            else {
                return Ok(if imported > 0 {
                    Status::Imported(imported)
                } else if self.last.is_none() {
                    Status::UpToDate
                } else {
                    Status::RootMissing
                });
            };
            if self.last.as_deref() == Some(tip.as_str()) {
                return Ok(if imported > 0 {
                    Status::Imported(imported)
                } else {
                    Status::UpToDate
                });
            }
            // G5: an exact continuation of the last decided entry is walked
            // from there; anything else walks the whole path and decides.
            let tail = match &self.last {
                Some(l) => repo.first_parent_since(&tip, l).map_err(StoreError::Git)?,
                None => None,
            };
            let (path, start) = if let (Some(tail), Some(l)) = (tail, &self.last) {
                let mut p = Vec::with_capacity(tail.len() + 1);
                p.push(l.clone());
                p.extend(tail);
                (p, 1)
            } else {
                let path = match repo.first_parent_path(&tip) {
                    Ok(p) => p,
                    Err(GitError::Missing(m)) => return Ok(Status::Incomplete(m)),
                    Err(e) => return Err(StoreError::Git(e)),
                };
                if path.first() != Some(&self.anchor) {
                    return Ok(Status::Diverged(format!(
                        "g{g} does not start at the trusted entry"
                    )));
                }
                let start = match &self.last {
                    None => 0,
                    Some(l) => match path.iter().position(|o| o == l) {
                        Some(i) => i + 1,
                        None if repo.is_ancestor(&tip, l).map_err(StoreError::Git)? => {
                            return Ok(if imported > 0 {
                                Status::Imported(imported)
                            } else {
                                Status::UpToDate
                            });
                        }
                        None => {
                            return Ok(Status::Diverged(format!("g{g} no longer contains {l}")));
                        }
                    },
                };
                (path, start)
            };
            // Entries are read in batches: a few git processes per batch,
            // not one per entry (G5).
            let mut batch: std::collections::VecDeque<Result<crate::git::EntryObjects, GitError>> =
                Default::default();
            for (i, oid) in path.iter().enumerate().skip(start) {
                if batch.is_empty() {
                    let end = (i + IMPORT_BATCH).min(path.len());
                    batch = repo
                        .read_entries(&path[i..end])
                        .map_err(StoreError::Git)?
                        .into();
                }
                let objs = match batch.pop_front().expect("read for this entry") {
                    Ok(o) => o,
                    Err(GitError::Missing(m)) => return Ok(Status::Incomplete(m)),
                    Err(e) => return Err(StoreError::Git(e)),
                };
                let seq = self.replica.seq().map_or(0, |s| s + 1);
                let first = i == 0;
                let out = self.replica.accept(&EntryInput {
                    oid,
                    seq,
                    first_in_generation: first,
                    commit: &objs.commit,
                    tree: &objs.tree,
                });
                match out {
                    o @ (Outcome::Decided(_) | Outcome::EntryRejected(_)) => {
                        self.record_decisions(&objs.commit, &o)
                    }
                    Outcome::Stall(s) => return Ok(Status::Stalled(format!("{oid}: {s:?}"))),
                    other => return Err(StoreError::Corrupt(format!("{oid}: {other:?}"))),
                }
                self.append_unsynced(&[Frame {
                    kind: Kind::Entry,
                    oid: oid.clone(),
                    seq,
                    first,
                    commit: objs.commit,
                    tree: objs.tree,
                }])?;
                self.advanced(oid);
                imported += 1;
                if self.replica.pending().is_some() {
                    break;
                }
            }
            if self.replica.pending().is_none() {
                return Ok(if imported > 0 {
                    Status::Imported(imported)
                } else {
                    Status::UpToDate
                });
            }
        }
    }
}
