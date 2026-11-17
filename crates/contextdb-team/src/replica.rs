//! The replica and the pure `accept` (ADR 0018 §4.2, SPEC SD21.4).
//!
//! A replica is a single-branch kernel `Db` plus the team side state (the
//! document, shred set, accepted proposal ids, blob set, tags and the
//! portable names of revisions and rules). Its only input is the chain:
//! `accept(replica at parent, entry)` decides an entry from the replica and
//! the entry's bytes alone. Every deterministic failure rejects; only an
//! `accept_version` this build does not implement stalls (I/O and missing
//! objects stall in the caller, before `accept` runs).

use crate::commit::{self, ObjectFormat, hex};
use crate::doc::TeamDoc;
use crate::ops::{
    Edge, Hash, MAX_BLOB_BYTES, MAX_ENTRY_BYTES, RuleSpec, TeamBatch, TeamOp, tags_hash,
};
use crate::sshsig;
use crate::state::{KeyedState, RecordItem, RevisionItem};
use contextdb_core::Db;
use contextdb_model::{
    Batch, BlobPresence, Channel, ContentHash, EdgeKind, LogicalId, LogicalRef, MAIN, Op,
    PolicyDoc, PrincipalId, RecordKind, RevRef, RevisionId, RuleId, RuleRef, Summary, TxError,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// The `accept_version`s this build implements.
pub const SUPPORTED_ACCEPT_VERSIONS: &[u32] = &[1];

/// One entry as the caller read it from git: its position-derived sequence
/// number, whether it is its generation's first, the raw commit object and
/// the flattened tree (`batch`, `blobs/<hex>`).
#[derive(Clone, Debug)]
pub struct EntryInput<'a> {
    /// The entry's commit id (hex), named by a successor's `Ctx-Compacts`.
    pub oid: &'a str,
    pub seq: u64,
    pub first_in_generation: bool,
    pub commit: &'a [u8],
    pub tree: &'a [(String, Vec<u8>)],
}

/// A deterministic rejection: a stable code and a detail that names keys
/// and contents, never replica ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reason {
    pub code: &'static str,
    pub detail: String,
}

fn reject<T>(code: &'static str, detail: impl Into<String>) -> Result<T, Reason> {
    Err(Reason {
        code,
        detail: detail.into(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stall {
    UnsupportedAcceptVersion(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every proposal is rejected for one entry-level reason.
    EntryRejected(Reason),
    /// One decision per proposal, in `Ctx-Proposal` order.
    Decided(Vec<Result<(), Reason>>),
    Stall(Stall),
    /// After an accepted `Compact`, entries appended to the terminal
    /// generation are ignored: no decision, no sequence number (§8.6).
    Ignored,
    /// The next generation's first entry verified against the `Compact`.
    Successor,
    /// A first entry that is not the committed successor: the edge does not
    /// follow it, and nothing changes.
    InvalidSuccessor(Reason),
}

/// An accepted `Compact` awaiting its successor (`CompactPending`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub generation: u32,
    pub tree: Vec<u8>,
    pub digest: Hash,
    pub compact_oid: String,
    pub compact_seq: u64,
}

/// Why a checkpoint join failed: an object to fetch, or a verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinError {
    Missing(String),
    Invalid(Reason),
}

enum Fail {
    Reject(Reason),
    Stall(Stall),
}

fn rej(code: &'static str, detail: impl Into<String>) -> Fail {
    Fail::Reject(Reason {
        code,
        detail: detail.into(),
    })
}

impl From<Reason> for Fail {
    fn from(r: Reason) -> Self {
        Self::Reject(r)
    }
}

/// The variant name of a kernel error: stable across edges (no ids).
fn tx_code(e: &TxError) -> String {
    let s = format!("{e:?}");
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or("TxError")
        .to_owned()
}

#[derive(Clone, Debug)]
pub struct Replica {
    team: [u8; 16],
    format: ObjectFormat,
    db: Db,
    doc: Option<TeamDoc>,
    seq: Option<u64>,
    shred: BTreeSet<Hash>,
    accepted: BTreeSet<(String, [u8; 16])>,
    /// The blob set (§3.1): the bytes of every non-shredded revision.
    blobs: BTreeMap<Hash, Arc<Vec<u8>>>,
    generation: u32,
    pending: Option<Pending>,
    tags: BTreeMap<String, BTreeSet<String>>,
    revs: BTreeMap<(String, Hash), RevisionId>,
    rules: BTreeMap<RuleSpec, RuleId>,
}

struct Present<'a> {
    replica: &'a BTreeMap<Hash, Arc<Vec<u8>>>,
    entry: &'a BTreeMap<Hash, Vec<u8>>,
}

impl BlobPresence for Present<'_> {
    fn contains(&self, c: &ContentHash) -> bool {
        self.replica.contains_key(c) || self.entry.contains_key(c)
    }
}

/// What a proposal changes, before the kernel commits it.
#[derive(Default)]
struct Stage {
    new_logicals: Vec<String>,
    new_records: BTreeMap<String, (String, RecordKind)>,
    /// Every revision slot the batch allocates, in order (dedup hits too).
    new_revs: Vec<(String, Hash)>,
    new_rules: Vec<RuleSpec>,
    heads: BTreeMap<String, Hash>,
    base: BTreeMap<(String, Hash), bool>,
    enabled: BTreeMap<RuleSpec, bool>,
    tags: BTreeMap<String, BTreeSet<String>>,
    shred: BTreeSet<Hash>,
    doc: Option<TeamDoc>,
    ops: Vec<Op>,
}

impl Replica {
    pub fn new(team: [u8; 16], format: ObjectFormat) -> Self {
        Self {
            team,
            format,
            db: Db::new(),
            doc: None,
            seq: None,
            shred: BTreeSet::new(),
            accepted: BTreeSet::new(),
            blobs: BTreeMap::new(),
            generation: 0,
            pending: None,
            tags: BTreeMap::new(),
            revs: BTreeMap::new(),
            rules: BTreeMap::new(),
        }
    }

    pub fn doc(&self) -> Option<&TeamDoc> {
        self.doc.as_ref()
    }
    pub fn seq(&self) -> Option<u64> {
        self.seq
    }
    pub fn db(&self) -> &Db {
        &self.db
    }
    pub fn generation(&self) -> u32 {
        self.generation
    }
    /// `Some` while an accepted `Compact` awaits its successor.
    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }
    pub fn blob(&self, h: &Hash) -> Option<&[u8]> {
        self.blobs.get(h).map(|b| b.as_slice())
    }

    pub fn key(&self, path: &str) -> String {
        format!("team/{}/{path}", hex(&self.team))
    }
    pub fn lane(&self, lane: &str) -> String {
        format!("team:{}/{lane}", hex(&self.team))
    }
    fn lane_name<'l>(&self, lane: &'l str) -> &'l str {
        &lane["team:".len() + 32 + 1..]
    }

    /// `accept(replica at parent, entry)`: decides the entry and, unless it
    /// stalls, applies the accepted proposals and advances the sequence.
    pub fn accept(&mut self, e: &EntryInput) -> Outcome {
        if self.pending.is_some() {
            return if e.first_in_generation {
                match self.check_successor(e) {
                    Ok(()) => {
                        let p = self.pending.take().expect("pending");
                        self.generation = p.generation;
                        self.seq = Some(e.seq);
                        Outcome::Successor
                    }
                    Err(r) => Outcome::InvalidSuccessor(r),
                }
            } else {
                Outcome::Ignored
            };
        }
        if e.first_in_generation && self.doc.is_some() {
            return Outcome::InvalidSuccessor(Reason {
                code: "NoCompact",
                detail: "no accepted Compact".into(),
            });
        }
        let parsed = match self.check_entry(e) {
            Ok(p) => p,
            Err(Fail::Reject(r)) => {
                self.seq = Some(e.seq);
                return Outcome::EntryRejected(r);
            }
            Err(Fail::Stall(s)) => return Outcome::Stall(s),
        };
        let (author, ids, batch, blobs) = parsed;
        if let [ops] = &batch.proposals[..]
            && let [
                TeamOp::Compact {
                    generation,
                    tree,
                    digest,
                },
            ] = &ops[..]
        {
            let d = if self.accepted.contains(&(author.clone(), ids[0])) {
                reject("DuplicateProposal", hex(&ids[0]))
            } else {
                self.decide_compact(e, &author, ids[0], *generation, tree, digest)
            };
            self.seq = Some(e.seq);
            return Outcome::Decided(vec![d]);
        }
        let mut decisions = Vec::with_capacity(ids.len());
        for (id, ops) in ids.iter().zip(&batch.proposals) {
            let d = if self.accepted.contains(&(author.clone(), *id)) {
                reject("DuplicateProposal", hex(id))
            } else {
                match self.apply(&author, *id, ops, &blobs) {
                    Ok(()) => Ok(()),
                    Err(Fail::Reject(r)) => Err(r),
                    Err(Fail::Stall(s)) => return Outcome::Stall(s),
                }
            };
            decisions.push(d);
        }
        self.seq = Some(e.seq);
        Outcome::Decided(decisions)
    }

    /// The entry-level checks of §3.1 and §4.2, in order: structure and
    /// sizes, trailers, the signature, the author, decoding.
    #[allow(clippy::type_complexity)]
    fn check_entry(
        &self,
        e: &EntryInput,
    ) -> Result<(String, Vec<[u8; 16]>, TeamBatch, BTreeMap<Hash, Vec<u8>>), Fail> {
        let c = commit::parse(e.commit, self.format)
            .or_else(|err| reject("Commit", format!("{err:?}")))?;
        let want_parents = usize::from(!e.first_in_generation);
        if c.parents.len() != want_parents {
            return Err(rej("Parents", format!("{} parents", c.parents.len())));
        }
        // Sizes and names before decoding anything.
        let mut batch_bytes = None;
        let mut blobs = BTreeMap::new();
        let mut total = 0usize;
        for (name, bytes) in e.tree {
            total = total.saturating_add(bytes.len());
            if name == "batch" {
                if batch_bytes.replace(bytes).is_some() {
                    return Err(rej("Tree", "two batch files"));
                }
            } else if let Some(h) = name.strip_prefix("blobs/") {
                if bytes.len() > MAX_BLOB_BYTES {
                    return Err(rej("BlobTooLarge", h));
                }
                let actual = *blake3::hash(bytes).as_bytes();
                if hex(&actual) != h {
                    return Err(rej("BlobName", h));
                }
                if blobs.insert(actual, bytes.clone()).is_some() {
                    return Err(rej("Tree", "duplicate blob"));
                }
            } else {
                return Err(rej("Tree", name.clone()));
            }
        }
        if total > MAX_ENTRY_BYTES {
            return Err(rej("EntryTooLarge", total.to_string()));
        }
        let batch_bytes = batch_bytes.ok_or_else(|| rej("Tree", "no batch"))?;
        let t = &c.trailers;
        let (Some(team), Some(seq), Some(author), Some(bh), Some(av)) =
            (&t.team, t.seq, &t.author, &t.batch_hash, t.accept_version)
        else {
            return Err(rej("Trailers", "missing trailer"));
        };
        if t.compacts.is_some() {
            return Err(rej("Trailers", "Ctx-Compacts on an ordinary entry"));
        }
        if *team != hex(&self.team) {
            return Err(rej("Team", team.clone()));
        }
        if seq != e.seq {
            return Err(rej("Seq", format!("{seq} at position {}", e.seq)));
        }
        if *bh != TeamBatch::hash_hex(batch_bytes) {
            return Err(rej("BatchHash", bh.clone()));
        }
        let batch = TeamBatch::decode(batch_bytes).map_err(|err| rej("Decode", err.0))?;
        if t.proposals.is_empty() || t.proposals.len() != batch.proposals.len() {
            return Err(rej("Proposals", "count"));
        }
        let sole = batch.proposals.iter().flatten().any(TeamOp::is_sole);
        if sole && (batch.proposals.len() != 1 || batch.proposals[0].len() != 1) {
            return Err(rej("SoleOpRequired", ""));
        }
        // The document that governs the entry: the parent's, or for genesis
        // the one it carries.
        let genesis_doc;
        let doc = match (&self.doc, e.seq) {
            (None, 0) => match &batch.proposals[0][..] {
                [TeamOp::SetTeamAccess { doc }] if batch.proposals.len() == 1 => {
                    genesis_doc = doc.as_ref().clone();
                    &genesis_doc
                }
                _ => return Err(rej("Genesis", "must hold the team document only")),
            },
            (Some(d), _) => d,
            (None, _) => return Err(rej("Genesis", "no team document")),
        };
        if doc.id != self.team {
            return Err(rej("Team", "document id"));
        }
        if !SUPPORTED_ACCEPT_VERSIONS.contains(&doc.accept_version) {
            return Err(Fail::Stall(Stall::UnsupportedAcceptVersion(
                doc.accept_version,
            )));
        }
        if av != doc.accept_version {
            return Err(rej("AcceptVersion", av.to_string()));
        }
        let keys = doc.keys_of(author);
        if keys.is_empty() {
            return Err(rej("NotTeamMember", author.clone()));
        }
        let Some(sig) = &c.signature else {
            return Err(rej("Unsigned", ""));
        };
        if let Err(err) = sshsig::verify(sig, "git", &c.payload, &keys) {
            return Err(rej("Signature", format!("{err:?}")));
        }
        if e.seq == 0 && self.doc.is_none() && !doc.is_admin(author) {
            return Err(rej("Genesis", "author is not an admin"));
        }
        Ok((author.clone(), t.proposals.clone(), batch, blobs))
    }

    fn logical(&self, path: &str) -> Option<LogicalId> {
        self.db.logical_by_key(&self.key(path))
    }

    /// Decides one proposal; on success commits it to the kernel and the
    /// side state, on failure leaves both unchanged.
    fn apply(
        &mut self,
        author: &str,
        id: [u8; 16],
        ops: &[TeamOp],
        entry_blobs: &BTreeMap<Hash, Vec<u8>>,
    ) -> Result<(), Fail> {
        let mut st = Stage::default();
        for op in ops {
            self.stage_op(author, op, &mut st, entry_blobs)?;
        }
        // Capacity (SD21.6): the state after this proposal, its own id
        // included, plus the reserve for one future Compact.
        let doc_after = st
            .doc
            .clone()
            .or_else(|| self.doc.clone())
            .expect("a document");
        let prospective = self.prospective_state(&st, author, id, &doc_after);
        let size = prospective.ckpt_size_reserved(|h| self.blob_size(h, entry_blobs));
        if size > doc_after.state_bytes {
            return Err(rej(
                "TeamStateLimit",
                format!("{size} > {}", doc_after.state_bytes),
            ));
        }
        let mut result = None;
        if !st.ops.is_empty() {
            let batch = Batch {
                branch: MAIN,
                principal: PrincipalId::STORE_OWNER,
                channel: Channel::Admin,
                client_txn_id: None,
                ops: std::mem::take(&mut st.ops),
            };
            let present = Present {
                replica: &self.blobs,
                entry: entry_blobs,
            };
            match self.db.commit_with(&batch, &present) {
                Ok(r) => result = Some(r),
                Err(e) => return Err(rej("Kernel", tx_code(&e))),
            }
        }
        // Commit the side state.
        if let Some(r) = result {
            for ((path, h), rid) in st.new_revs.iter().zip(&r.allocations.new_revisions) {
                self.revs.entry((path.clone(), *h)).or_insert(*rid);
            }
            for (spec, j) in st.new_rules.iter().zip(&r.allocations.new_rules) {
                self.rules.insert(spec.clone(), *j);
            }
        }
        for (path, tags) in st.tags {
            self.tags.insert(path, tags);
        }
        for (_, h) in &st.new_revs {
            if !self.shred.contains(h)
                && !st.shred.contains(h)
                && !self.blobs.contains_key(h)
                && let Some(b) = entry_blobs.get(h)
            {
                self.blobs.insert(*h, Arc::new(b.clone()));
            }
        }
        for h in st.shred {
            self.shred.insert(h);
            self.blobs.remove(&h);
        }
        if let Some(d) = st.doc {
            self.doc = Some(d);
        }
        self.accepted.insert((author.to_owned(), id));
        Ok(())
    }

    fn blob_size(&self, h: &Hash, entry: &BTreeMap<Hash, Vec<u8>>) -> u64 {
        self.blobs
            .get(h)
            .map(|b| b.len() as u64)
            .or_else(|| entry.get(h).map(|b| b.len() as u64))
            .unwrap_or(0)
    }

    // ----- staging: resolution against the replica plus earlier ops -----

    fn exists(&self, st: &Stage, path: &str) -> bool {
        st.new_records.contains_key(path) || self.logical(path).is_some()
    }

    fn lane_kind(&self, st: &Stage, path: &str) -> Result<(String, RecordKind), Reason> {
        if let Some(lk) = st.new_records.get(path) {
            return Ok(lk.clone());
        }
        let l = self.logical(path).ok_or_else(|| Reason {
            code: "UnknownKey",
            detail: path.to_owned(),
        })?;
        let (_, kind, lane) = self.db.logical_info(l).expect("logical exists");
        Ok((self.lane_name(&lane).to_owned(), kind))
    }

    fn logical_ref(&self, st: &Stage, path: &str) -> Result<LogicalRef, Reason> {
        if let Some(l) = self.logical(path) {
            return Ok(LogicalRef::Existing(l));
        }
        match st.new_logicals.iter().position(|p| p == path) {
            Some(k) => Ok(LogicalRef::New(k as u16)),
            None => reject("UnknownKey", path),
        }
    }

    fn rev_ref(&self, st: &Stage, path: &str, h: &Hash) -> Result<RevRef, Reason> {
        if let Some(r) = self.revs.get(&(path.to_owned(), *h)) {
            return Ok(RevRef::Existing(*r));
        }
        match st.new_revs.iter().position(|(p, c)| p == path && c == h) {
            Some(k) => Ok(RevRef::New(k as u16)),
            None => reject("UnknownRevision", format!("{path}@{}", hex(h))),
        }
    }

    fn head(&self, st: &Stage, path: &str) -> Result<Hash, Reason> {
        if let Some(h) = st.heads.get(path) {
            return Ok(*h);
        }
        let l = self.logical(path).ok_or_else(|| Reason {
            code: "UnknownKey",
            detail: path.to_owned(),
        })?;
        Ok(self
            .db
            .revision_content(self.db.head(l).expect("head"))
            .expect("revision"))
    }

    fn base(&self, st: &Stage, path: &str, h: &Hash) -> bool {
        if let Some(b) = st.base.get(&(path.to_owned(), *h)) {
            return *b;
        }
        self.revs
            .get(&(path.to_owned(), *h))
            .is_some_and(|r| self.db.is_base(*r).unwrap_or(false))
    }

    fn tags_of(&self, st: &Stage, path: &str) -> BTreeSet<String> {
        st.tags
            .get(path)
            .or_else(|| self.tags.get(path))
            .cloned()
            .unwrap_or_default()
    }

    fn rule_enabled(&self, st: &Stage, rule: &RuleSpec) -> Result<bool, Reason> {
        if let Some(e) = st.enabled.get(rule) {
            return Ok(*e);
        }
        match self.rules.get(rule) {
            Some(j) => Ok(self.db.rule_semantic(*j).expect("rule").enabled),
            None => reject("UnknownRule", rule.conclusion.0.clone()),
        }
    }

    fn rule_ref(&self, st: &Stage, rule: &RuleSpec) -> Result<RuleRef, Reason> {
        if let Some(j) = self.rules.get(rule) {
            return Ok(RuleRef::Existing(*j));
        }
        match st.new_rules.iter().position(|r| r == rule) {
            Some(k) => Ok(RuleRef::New(k as u16)),
            None => reject("UnknownRule", rule.conclusion.0.clone()),
        }
    }

    fn content_ok(
        &self,
        st: &Stage,
        h: &Hash,
        entry: &BTreeMap<Hash, Vec<u8>>,
    ) -> Result<(), Reason> {
        if self.shred.contains(h) || st.shred.contains(h) {
            return reject("ContentShredded", hex(h));
        }
        if !self.blobs.contains_key(h) && !entry.contains_key(h) {
            return reject("MissingBlob", hex(h));
        }
        Ok(())
    }

    fn stage_op(
        &self,
        author: &str,
        op: &TeamOp,
        st: &mut Stage,
        entry: &BTreeMap<Hash, Vec<u8>>,
    ) -> Result<(), Fail> {
        let doc = st.doc.as_ref().or(self.doc.as_ref());
        // Genesis: the document being installed governs its own entry.
        let genesis;
        let doc = match (doc, op) {
            (Some(d), _) => d,
            (None, TeamOp::SetTeamAccess { doc }) => {
                genesis = doc.as_ref().clone();
                &genesis
            }
            (None, _) => return Err(rej("Genesis", "")),
        };
        let role = |lane: &str, curate: bool| -> Result<(), Reason> {
            let ok = if curate {
                doc.can_curate(author, lane)
            } else {
                doc.can_write(author, lane)
            };
            if ok {
                Ok(())
            } else {
                reject(
                    if curate {
                        "Unauthorized(Curate)"
                    } else {
                        "Unauthorized(Write)"
                    },
                    lane,
                )
            }
        };
        match op {
            TeamOp::New {
                path,
                lane,
                kind,
                content,
                adopt_as_base,
            } => {
                if !doc.lanes.contains_key(lane) {
                    return Err(rej("UnknownLane", lane.clone()));
                }
                role(lane, false)?;
                if *adopt_as_base {
                    role(lane, true)?;
                }
                if self.exists(st, path) {
                    return Err(rej("LogicalExists", path.clone()));
                }
                self.content_ok(st, content, entry)?;
                st.ops.push(Op::CreateLogical {
                    origin: Default::default(),
                    key: self.key(path),
                    kind: *kind,
                    scope_lane: self.lane(lane),
                    content: *content,
                    adopt_as_base: *adopt_as_base,
                });
                st.new_logicals.push(path.clone());
                st.new_records.insert(path.clone(), (lane.clone(), *kind));
                st.new_revs.push((path.clone(), *content));
                st.heads.insert(path.clone(), *content);
                st.base.insert((path.clone(), *content), *adopt_as_base);
            }
            TeamOp::Revise {
                path,
                content,
                adopt_as_base,
            } => {
                let (lane, _) = self.lane_kind(st, path)?;
                role(&lane, false)?;
                if *adopt_as_base {
                    role(&lane, true)?;
                }
                self.content_ok(st, content, entry)?;
                st.ops.push(Op::AddRevision {
                    logical: self.logical_ref(st, path)?,
                    content: *content,
                    adopt_as_base: *adopt_as_base,
                });
                st.new_revs.push((path.clone(), *content));
                if *adopt_as_base {
                    st.base.insert((path.clone(), *content), true);
                }
            }
            TeamOp::SetHead {
                path,
                expected,
                new,
            } => {
                let (lane, _) = self.lane_kind(st, path)?;
                role(&lane, false)?;
                let head = self.head(st, path)?;
                if head != *expected {
                    return Err(rej("HeadConflict", path.clone()));
                }
                st.ops.push(Op::SetHead {
                    logical: self.logical_ref(st, path)?,
                    expected: self.rev_ref(st, path, expected)?,
                    new: self.rev_ref(st, path, new)?,
                });
                st.heads.insert(path.clone(), *new);
            }
            TeamOp::SetBase {
                path,
                content,
                expected,
                base,
            } => {
                let (lane, _) = self.lane_kind(st, path)?;
                role(&lane, true)?;
                let rev = self.rev_ref(st, path, content)?;
                if self.base(st, path, content) != *expected {
                    return Err(rej("BaseConflict", path.clone()));
                }
                if base != expected {
                    st.ops.push(if *base {
                        Op::AssertBase { rev }
                    } else {
                        Op::RetractBase { rev }
                    });
                    st.base.insert((path.clone(), *content), *base);
                }
            }
            TeamOp::AddRule { rule, enabled } => {
                let (cpath, ch) = &rule.conclusion;
                let (lane, _) = self.lane_kind(st, cpath)?;
                role(&lane, false)?;
                let conclusion = self.rev_ref(st, cpath, ch)?;
                let mut incidences = Vec::with_capacity(rule.incidences.len());
                for (p, e) in &rule.incidences {
                    let kind = match e {
                        Edge::Pin(_) => EdgeKind::Pin,
                        Edge::Ref(_) => EdgeKind::Ref,
                        Edge::Cur(seen) => {
                            if self.head(st, p)? != *seen {
                                return Err(rej("CurMoved", p.clone()));
                            }
                            EdgeKind::Cur
                        }
                    };
                    incidences.push((self.rev_ref(st, p, &e.content())?, kind));
                }
                st.ops.push(Op::AddRule {
                    conclusion,
                    incidences,
                    enabled: *enabled,
                });
                st.new_rules.push(rule.clone());
                st.enabled.insert(rule.clone(), *enabled);
            }
            TeamOp::SetRuleEnabled {
                rule,
                expected,
                enabled,
            } => {
                let (lane, _) = self.lane_kind(st, &rule.conclusion.0)?;
                role(&lane, false)?;
                let r = self.rule_ref(st, rule)?;
                if self.rule_enabled(st, rule)? != *expected {
                    return Err(rej("EnabledConflict", rule.conclusion.0.clone()));
                }
                if enabled != expected {
                    st.ops.push(Op::SetRuleEnabled {
                        rule: r,
                        enabled: *enabled,
                    });
                    st.enabled.insert(rule.clone(), *enabled);
                }
            }
            TeamOp::SetTags {
                path,
                expected,
                tags,
            } => {
                let (lane, _) = self.lane_kind(st, path)?;
                role(&lane, false)?;
                if tags_hash(&self.tags_of(st, path)) != *expected {
                    return Err(rej("TagsConflict", path.clone()));
                }
                st.tags.insert(path.clone(), tags.clone());
            }
            TeamOp::Expect { path, content } => {
                // Against the state before the proposal (SPEC §6.1 contract 12).
                let l = self.logical(path).ok_or_else(|| Reason {
                    code: "UnknownKey",
                    detail: path.clone(),
                })?;
                let head = self
                    .db
                    .revision_content(self.db.head(l).expect("head"))
                    .expect("rev");
                if head != *content {
                    return Err(rej("ReadSetConflict", path.clone()));
                }
            }
            TeamOp::SetTeamAccess { doc: new } => {
                if !doc.is_admin(author) {
                    return Err(rej("AdminOnly", "SetTeamAccess"));
                }
                if new.id != self.team {
                    return Err(rej("Team", "document id"));
                }
                for (path, _) in self.revs.keys() {
                    let (lane, _) = self.lane_kind(st, path)?;
                    if !new.lanes.contains_key(&lane) {
                        return Err(rej("LaneInUse", lane));
                    }
                }
                let old = self.doc.as_ref().map(|d| &d.require_deps);
                if old != Some(&new.require_deps) {
                    st.ops.push(Op::SetPolicy {
                        policy: PolicyDoc {
                            version: 1,
                            require_deps: new.require_deps.clone(),
                        },
                    });
                }
                st.doc = Some(new.as_ref().clone());
            }
            TeamOp::TeamShred { content } => {
                if !doc.is_admin(author) {
                    return Err(rej("AdminOnly", "TeamShred"));
                }
                st.shred.insert(*content);
            }
            TeamOp::Compact { .. } => return Err(rej("SoleOpRequired", "Compact")),
        }
        Ok(())
    }

    // ----- scratch decisions (§6.2, §7.2) -----

    /// Decides one proposal as `accept` would for an entry by `member`,
    /// without an entry around it: the scratch check at enqueue and at
    /// publish (ADR 0018 §4.2 "scratch accept"). Run it on a clone, never on
    /// the replica itself; nothing here checks a signature or a sequence.
    pub fn scratch_apply(
        &mut self,
        member: &str,
        id: [u8; 16],
        ops: &[TeamOp],
        blobs: &BTreeMap<Hash, Vec<u8>>,
    ) -> Result<(), Reason> {
        if self.doc.is_none() {
            return reject("Genesis", "no team document");
        }
        if ops.iter().any(TeamOp::is_sole) && ops.len() != 1 {
            return reject("SoleOpRequired", "");
        }
        if self
            .doc
            .as_ref()
            .is_some_and(|d| d.keys_of(member).is_empty())
        {
            return reject("NotTeamMember", member);
        }
        if self.accepted.contains(&(member.to_owned(), id)) {
            return reject("DuplicateProposal", hex(&id));
        }
        match self.apply(member, id, ops, blobs) {
            Ok(()) => Ok(()),
            Err(Fail::Reject(r)) => Err(r),
            Err(Fail::Stall(s)) => reject("Stall", format!("{s:?}")),
        }
    }
    /// Whether `(member, id)` is an accepted proposal.
    pub fn is_accepted(&self, member: &str, id: &[u8; 16]) -> bool {
        self.accepted.contains(&(member.to_owned(), *id))
    }
    pub fn team(&self) -> [u8; 16] {
        self.team
    }
    pub fn format(&self) -> ObjectFormat {
        self.format
    }

    // ----- compaction (§8.6) -----

    /// Σc: the state after an accepted `Compact` by `author` with proposal
    /// id `id` at `seq` — the replica's state with that sequence number and
    /// the id accepted, nothing else.
    pub fn sigma_c(&self, author: &str, id: [u8; 16], seq: u64) -> Option<KeyedState> {
        let mut s = self.keyed_state()?;
        s.seq = seq;
        s.accepted.insert((author.to_owned(), id));
        Some(s)
    }

    /// `ckpt(Σ, g)` with this replica's blob bytes.
    pub fn checkpoint_files(&self, state: &KeyedState, generation: u32) -> Vec<(String, Vec<u8>)> {
        crate::checkpoint::encode(state, self.team, generation, |h| {
            self.blobs
                .get(h)
                .map(|b| b.as_ref().clone())
                .unwrap_or_default()
        })
    }

    fn decide_compact(
        &mut self,
        e: &EntryInput,
        author: &str,
        id: [u8; 16],
        generation: u32,
        tree: &[u8],
        digest: &Hash,
    ) -> Result<(), Reason> {
        let doc = self.doc.clone().expect("checked by check_entry");
        if !doc.is_admin(author) {
            return reject("AdminOnly", "Compact");
        }
        if generation != self.generation + 1 {
            return reject("CompactGeneration", generation.to_string());
        }
        let sc = self.sigma_c(author, id, e.seq).expect("document");
        if sc.digest() != *digest {
            return reject("CompactDigest", hex(digest));
        }
        let size = sc.ckpt_size(|h| self.blobs.get(h).map_or(0, |b| b.len() as u64));
        if size > doc.state_bytes {
            return reject("TeamStateLimit", format!("{size} > {}", doc.state_bytes));
        }
        let files = self.checkpoint_files(&sc, generation);
        match crate::gitobj::tree_id(self.format, &files) {
            Ok(t) if t == tree => {}
            Ok(_) => return reject("CompactTree", hex(tree)),
            Err(_) => return reject("Sha1Collision", "checkpoint tree"),
        }
        self.accepted.insert((author.to_owned(), id));
        self.pending = Some(Pending {
            generation,
            tree: tree.to_vec(),
            digest: *digest,
            compact_oid: e.oid.to_owned(),
            compact_seq: e.seq,
        });
        Ok(())
    }

    /// The successor's own checks: a root commit with the committed tree,
    /// naming the `Compact`, signed by an admin of Σc's document.
    fn check_successor(&self, e: &EntryInput) -> Result<(), Reason> {
        let p = self.pending.as_ref().expect("pending");
        let doc = self.doc.as_ref().expect("document");
        let c = commit::parse(e.commit, self.format)
            .or_else(|err| reject("Commit", format!("{err:?}")))?;
        let t = &c.trailers;
        if !c.parents.is_empty() {
            return reject("Parents", "a successor has no parent");
        }
        let (Some(team), Some(seq), Some(author), Some(av), Some(compacts)) =
            (&t.team, t.seq, &t.author, t.accept_version, &t.compacts)
        else {
            return reject("Trailers", "missing trailer");
        };
        if t.batch_hash.is_some() || !t.proposals.is_empty() {
            return reject("Trailers", "a successor carries no batch");
        }
        if *team != hex(&self.team) || av != doc.accept_version {
            return reject("Trailers", "team or accept version");
        }
        if seq != p.compact_seq + 1 || e.seq != seq {
            return reject("Seq", seq.to_string());
        }
        if *compacts != p.compact_oid {
            return reject("Compacts", compacts.clone());
        }
        if c.tree != hex(&p.tree) {
            return reject("CompactTree", c.tree.clone());
        }
        match crate::gitobj::tree_id(self.format, e.tree) {
            Ok(t) if t == p.tree => {}
            _ => return reject("CompactTree", "files"),
        }
        if !doc.is_admin(author) {
            return reject("AdminOnly", "successor");
        }
        let sig = c.signature.as_ref().ok_or(Reason {
            code: "Unsigned",
            detail: String::new(),
        })?;
        sshsig::verify(sig, "git", &c.payload, &doc.keys_of(author))
            .or_else(|err| reject("Signature", format!("{err:?}")))?;
        Ok(())
    }

    /// A checkpoint joiner (§8.6, §8.7): given generation `g`'s first entry
    /// out of band, verifies it and its checkpoint, rebuilds Σc and applies
    /// the entry. The result equals a follower's replica after the same
    /// entry, digest included.
    pub fn join_checkpoint(
        team: [u8; 16],
        format: ObjectFormat,
        commit_raw: &[u8],
        files: &[(String, Vec<u8>)],
    ) -> Result<Self, JoinError> {
        let inv = |code: &'static str, d: String| JoinError::Invalid(Reason { code, detail: d });
        let map: BTreeMap<String, Vec<u8>> = files.iter().cloned().collect();
        let dec = crate::checkpoint::decode(&map).map_err(|e| match e {
            crate::checkpoint::CkptError::Missing(p) => JoinError::Missing(p),
            crate::checkpoint::CkptError::Invalid(m) => inv("Checkpoint", m),
        })?;
        if dec.header.team != team {
            return Err(inv("Team", "checkpoint team".into()));
        }
        let c = commit::parse(commit_raw, format).map_err(|e| inv("Commit", format!("{e:?}")))?;
        let t = &c.trailers;
        let (Some(tm), Some(seq), Some(author), Some(av), Some(_)) =
            (&t.team, t.seq, &t.author, t.accept_version, &t.compacts)
        else {
            return Err(inv("Trailers", "missing trailer".into()));
        };
        let doc = &dec.state.doc;
        if !c.parents.is_empty()
            || t.batch_hash.is_some()
            || !t.proposals.is_empty()
            || *tm != hex(&team)
            || av != doc.accept_version
            || seq != dec.header.seq + 1
        {
            return Err(inv("Successor", "structure".into()));
        }
        if !SUPPORTED_ACCEPT_VERSIONS.contains(&doc.accept_version) {
            return Err(inv("AcceptVersion", doc.accept_version.to_string()));
        }
        match crate::gitobj::tree_id(format, files) {
            Ok(id) if hex(&id) == c.tree => {}
            _ => return Err(inv("CompactTree", "files are not the commit's tree".into())),
        }
        if !doc.is_admin(author) {
            return Err(inv("AdminOnly", "successor".into()));
        }
        let sig = c.signature.as_ref().ok_or(inv("Unsigned", String::new()))?;
        sshsig::verify(sig, "git", &c.payload, &doc.keys_of(author))
            .map_err(|e| inv("Signature", format!("{e:?}")))?;
        let mut r = Self::from_state(team, format, &dec.state, &dec.blobs)
            .map_err(|e| inv("Rebuild", e))?;
        if r.digest() != Some(dec.header.digest) {
            return Err(inv("CompactDigest", "rebuilt state".into()));
        }
        r.generation = dec.header.generation;
        r.seq = Some(seq);
        Ok(r)
    }

    /// Rebuilds a replica from a keyed state: records through the kernel's
    /// input snapshot (no transaction checks, so a state legal when it was
    /// built is never refused by a later scan), rules through ordinary
    /// commits on top.
    fn from_state(
        team: [u8; 16],
        format: ObjectFormat,
        st: &KeyedState,
        blobs: &BTreeMap<Hash, Vec<u8>>,
    ) -> Result<Self, String> {
        use contextdb_model::{Epoch, Inputs, Logical, Metadata, PolicyDoc as P, Revision};
        let mut r = Self::new(team, format);
        let mut inputs = Inputs {
            policy: P {
                version: 1,
                require_deps: st.doc.require_deps.clone(),
            },
            ..Inputs::default()
        };
        for rec in &st.records {
            let l = LogicalId(inputs.logicals.len() as u32);
            inputs.logicals.push(Logical {
                origin: Default::default(),
                key: r.key(&rec.path),
                kind: rec.kind,
                scope_lane: r.lane(&rec.lane),
                owner: PrincipalId::STORE_OWNER,
                origin_branch: MAIN,
                origin_epoch: Epoch(1),
            });
            inputs.metadata.push(Metadata::default());
            let mut head = None;
            for v in &rec.revisions {
                let id = RevisionId(inputs.revisions.len() as u32);
                inputs.revisions.push(Revision {
                    logical: l,
                    content: v.content,
                    author: PrincipalId::STORE_OWNER,
                    origin_branch: MAIN,
                    origin_epoch: Epoch(1),
                });
                inputs.base.push(v.base);
                r.revs.insert((rec.path.clone(), v.content), id);
                if v.content == rec.head {
                    head = Some(id);
                }
            }
            inputs.heads.push(head.ok_or("record without its head")?);
            if !rec.tags.is_empty() {
                r.tags.insert(rec.path.clone(), rec.tags.clone());
            }
        }
        r.db = Db::from_inputs(inputs, Epoch(1)).map_err(|e| format!("{e:?}"))?;
        if !st.rules.is_empty() {
            let mut ops = Vec::with_capacity(st.rules.len());
            for (spec, enabled) in &st.rules {
                let rev = |p: &str, h: &Hash| -> Result<RevRef, String> {
                    r.revs
                        .get(&(p.to_owned(), *h))
                        .map(|id| RevRef::Existing(*id))
                        .ok_or_else(|| format!("rule names an unknown revision of {p}"))
                };
                let mut incidences = Vec::with_capacity(spec.incidences.len());
                for (p, e) in &spec.incidences {
                    let k = match e {
                        Edge::Pin(_) => EdgeKind::Pin,
                        Edge::Cur(_) => EdgeKind::Cur,
                        Edge::Ref(_) => EdgeKind::Ref,
                    };
                    incidences.push((rev(p, &e.content())?, k));
                }
                ops.push(Op::AddRule {
                    conclusion: rev(&spec.conclusion.0, &spec.conclusion.1)?,
                    incidences,
                    enabled: *enabled,
                });
            }
            let batch = Batch {
                branch: MAIN,
                principal: PrincipalId::STORE_OWNER,
                channel: Channel::Admin,
                client_txn_id: None,
                ops,
            };
            let res = r.db.commit(&batch).map_err(|e| format!("rules: {e:?}"))?;
            for (spec, j) in st.rules.keys().zip(&res.allocations.new_rules) {
                r.rules.insert(spec.clone(), *j);
            }
        }
        r.doc = Some(st.doc.clone());
        r.shred = st.shred.clone();
        r.accepted = st.accepted.clone();
        r.seq = Some(st.seq);
        for (h, b) in blobs {
            r.blobs.insert(*h, Arc::new(b.clone()));
        }
        Ok(r)
    }

    // ----- the keyed state -----

    /// The keyed state now (§4.3).
    pub fn keyed_state(&self) -> Option<KeyedState> {
        let doc = self.doc.clone()?;
        let mut paths: BTreeMap<String, Vec<(Hash, RevisionId)>> = BTreeMap::new();
        for ((p, h), r) in &self.revs {
            paths.entry(p.clone()).or_default().push((*h, *r));
        }
        let mut records = Vec::with_capacity(paths.len());
        for (path, mut revs) in paths {
            revs.sort_by_key(|(_, r)| *r);
            let l = self.logical(&path).expect("logical");
            let (_, kind, lane) = self.db.logical_info(l).expect("info");
            let head = self.db.head(l).expect("head");
            records.push(RecordItem {
                lane: self.lane_name(&lane).to_owned(),
                kind,
                tags: self.tags.get(&path).cloned().unwrap_or_default(),
                certified: self.db.status(head).is_ok_and(|s| s == Summary::Certified),
                head: self.db.revision_content(head).expect("rev"),
                revisions: revs
                    .iter()
                    .map(|(h, r)| RevisionItem {
                        content: *h,
                        base: self.db.is_base(*r).unwrap_or(false),
                        shredded: self.shred.contains(h),
                    })
                    .collect(),
                path,
            });
        }
        let rules = self
            .rules
            .iter()
            .map(|(spec, j)| {
                (
                    spec.clone(),
                    self.db.rule_semantic(*j).expect("rule").enabled,
                )
            })
            .collect();
        Some(KeyedState {
            doc,
            records,
            rules,
            shred: self.shred.clone(),
            accepted: self.accepted.clone(),
            seq: self.seq.unwrap_or(0),
        })
    }

    /// The team digest (§4.3); `None` before genesis.
    pub fn digest(&self) -> Option<Hash> {
        self.keyed_state().map(|s| s.digest())
    }

    /// The team status of a record: `certified` iff its head is CERTIFIED in
    /// the replica (no Sem, validators or Conflicts: L1 = L2, §4.3).
    pub fn certified(&self, path: &str) -> Option<bool> {
        let l = self.logical(path)?;
        let head = self.db.head(l)?;
        Some(self.db.status(head).is_ok_and(|s| s == Summary::Certified))
    }

    /// The state the proposal would leave, for the size check. Statuses and
    /// base bits are fixed-size fields, so placeholders do not change the
    /// size.
    fn prospective_state(
        &self,
        st: &Stage,
        author: &str,
        id: [u8; 16],
        doc: &TeamDoc,
    ) -> KeyedState {
        let mut paths: BTreeMap<String, Vec<Hash>> = BTreeMap::new();
        let mut known: BTreeSet<(String, Hash)> = BTreeSet::new();
        let mut ordered: Vec<((String, Hash), RevisionId)> =
            self.revs.iter().map(|(k, r)| (k.clone(), *r)).collect();
        ordered.sort_by_key(|(_, r)| *r);
        for ((p, h), _) in ordered {
            known.insert((p.clone(), h));
            paths.entry(p).or_default().push(h);
        }
        for (p, h) in &st.new_revs {
            if known.insert((p.clone(), *h)) {
                paths.entry(p.clone()).or_default().push(*h);
            }
        }
        let shred: BTreeSet<Hash> = self.shred.union(&st.shred).copied().collect();
        let records = paths
            .into_iter()
            .map(|(path, hs)| {
                let (lane, kind) = self.lane_kind(st, &path).expect("staged or known");
                RecordItem {
                    lane,
                    kind,
                    tags: self.tags_of(st, &path),
                    certified: false,
                    head: [0; 32],
                    revisions: hs
                        .iter()
                        .map(|h| RevisionItem {
                            content: *h,
                            base: false,
                            shredded: shred.contains(h),
                        })
                        .collect(),
                    path,
                }
            })
            .collect();
        let mut rules: BTreeMap<RuleSpec, bool> =
            self.rules.keys().map(|r| (r.clone(), false)).collect();
        for r in &st.new_rules {
            rules.insert(r.clone(), false);
        }
        let mut accepted = self.accepted.clone();
        accepted.insert((author.to_owned(), id));
        KeyedState {
            doc: doc.clone(),
            records,
            rules,
            shred,
            accepted,
            seq: 0,
        }
    }
}
