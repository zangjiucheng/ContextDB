//! The proposal queue (ADR 0018 §6, SPEC SD21.11).
//!
//! A team proposal is local data of its author until a push publishes it:
//! `<data>/teams/<id>/proposals` is an append-only log of checksummed frames
//! (enqueue, state transitions, confirm listings) and `proposal-blobs/`
//! holds the bytes its revisions need. It is never part of any branch.
//!
//! Entering the queue takes two checks, and both must pass:
//!
//! 1. **The local layer**, as the requesting local principal under the
//!    local access document (P9): `Read` on everything the proposal names,
//!    `Write` on the lanes it changes, `Curate` for base changes, egress on
//!    every lane it writes. A name the principal cannot read and a name that
//!    does not exist get the same reply, `Unauthorized(Read)`; the one
//!    exception is SPEC's global key namespace (`LogicalExists`).
//! 2. **The team layer**, a scratch `accept` as the bound member on a clone
//!    of the replica with the earlier queued proposals applied. A rejection
//!    that only a proposal hidden from the author causes is reported as a
//!    bare queue conflict, naming nothing.

use crate::commit::hex;
use crate::enc::{Reader, Writer};
use crate::ops::{Hash, MAX_BATCH_BYTES, MAX_BLOB_BYTES, MAX_OPS, TeamBatch, TeamOp};
use crate::replica::{Reason, Replica};
use contextdb_core::access::{self, AccessState, LogicalFacts, Role};
use contextdb_model::{AccessNeed, PrincipalId, TxError};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const FRAME: &[u8; 4] = b"CXTQ";

/// A proposal's life (§6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Queued,
    /// Handed to `git push` (the publish linearization point): may be on
    /// the remote.
    Published {
        commit: String,
    },
    Accepted,
    Rejected {
        code: String,
        detail: String,
    },
    /// Left out of a publish; the author re-proposes or drops it.
    Conflicted {
        reason: String,
    },
    Dropped,
}

impl State {
    /// Still on its way to the root: part of the overlay new proposals see.
    pub fn pending(&self) -> bool {
        matches!(self, Self::Queued | Self::Published { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub id: [u8; 16],
    /// The local principal that proposed it.
    pub author: PrincipalId,
    pub ops: Vec<TeamOp>,
    /// Earlier proposals it reads or names.
    pub deps: Vec<[u8; 16]>,
    pub state: State,
}

#[derive(Debug)]
pub enum ProposeError {
    /// The local layer: `Unauthorized`, `LogicalExists`.
    Local(TxError),
    /// A lane the local document keeps on this machine.
    EgressDenied {
        lane: String,
    },
    /// The team layer's rejection.
    Team(Reason),
    /// A rejection caused by a queued proposal the author cannot see.
    QueueConflict,
    TooLarge(&'static str),
    Io(std::io::Error),
}

impl From<std::io::Error> for ProposeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// The local access document in force, and an opaque version of it (the
/// publish linearization point compares versions).
#[derive(Clone, Copy)]
pub struct LocalPolicy<'a> {
    pub access: Option<&'a AccessState>,
    pub version: &'a [u8],
}

pub struct Queue {
    dir: PathBuf,
    log: File,
    pub proposals: Vec<Proposal>,
    /// Confirm listings (§7.2 step 5): token → the proposal ids it names.
    pub listed: BTreeMap<Hash, Vec<[u8; 16]>>,
}

enum Frame {
    Enqueue {
        id: [u8; 16],
        author: u32,
        ops: Vec<TeamOp>,
        deps: Vec<[u8; 16]>,
    },
    State {
        id: [u8; 16],
        state: State,
    },
    Listed {
        token: Hash,
        ids: Vec<[u8; 16]>,
    },
}

fn encode_state(w: &mut Writer, s: &State) {
    match s {
        State::Queued => {
            w.u8(0);
        }
        State::Published { commit } => {
            w.u8(1).str(commit);
        }
        State::Accepted => {
            w.u8(2);
        }
        State::Rejected { code, detail } => {
            w.u8(3).str(code).str(detail);
        }
        State::Conflicted { reason } => {
            w.u8(4).str(reason);
        }
        State::Dropped => {
            w.u8(5);
        }
    }
}

fn decode_state(r: &mut Reader) -> Option<State> {
    Some(match r.u8().ok()? {
        0 => State::Queued,
        1 => State::Published {
            commit: r.str().ok()?.to_owned(),
        },
        2 => State::Accepted,
        3 => State::Rejected {
            code: r.str().ok()?.to_owned(),
            detail: r.str().ok()?.to_owned(),
        },
        4 => State::Conflicted {
            reason: r.str().ok()?.to_owned(),
        },
        5 => State::Dropped,
        _ => return None,
    })
}

impl Frame {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            Self::Enqueue {
                id,
                author,
                ops,
                deps,
            } => {
                w.u8(0).raw(id).u32(*author);
                w.bytes(
                    &TeamBatch {
                        proposals: vec![ops.clone()],
                    }
                    .encode(),
                );
                w.count(deps.len());
                for d in deps {
                    w.raw(d);
                }
            }
            Self::State { id, state } => {
                w.u8(1).raw(id);
                encode_state(&mut w, state);
            }
            Self::Listed { token, ids } => {
                w.u8(2).raw(token).count(ids.len());
                for i in ids {
                    w.raw(i);
                }
            }
        }
        let payload = w.0;
        let mut out = FRAME.to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
        out.extend_from_slice(blake3::hash(&payload).as_bytes());
        out
    }

    fn decode(buf: &[u8]) -> Option<(Self, usize)> {
        if buf.len() < 8 || &buf[..4] != FRAME {
            return None;
        }
        let n = u32::from_be_bytes(buf[4..8].try_into().ok()?) as usize;
        let end = 8usize.checked_add(n)?.checked_add(32)?;
        if buf.len() < end || blake3::hash(&buf[8..8 + n]).as_bytes() != &buf[8 + n..end] {
            return None;
        }
        let mut r = Reader(&buf[8..8 + n]);
        let f = match r.u8().ok()? {
            0 => {
                let id = r.array().ok()?;
                let author = r.u32().ok()?;
                let mut ops = TeamBatch::decode(r.bytes().ok()?).ok()?.proposals;
                if ops.len() != 1 {
                    return None;
                }
                let k = r.count(16).ok()?;
                let deps = (0..k).map(|_| r.array().ok()).collect::<Option<_>>()?;
                Self::Enqueue {
                    id,
                    author,
                    ops: ops.pop()?,
                    deps,
                }
            }
            1 => Self::State {
                id: r.array().ok()?,
                state: decode_state(&mut r)?,
            },
            2 => {
                let token = r.array().ok()?;
                let k = r.count(16).ok()?;
                Self::Listed {
                    token,
                    ids: (0..k).map(|_| r.array().ok()).collect::<Option<_>>()?,
                }
            }
            _ => return None,
        };
        r.end().ok()?;
        Some((f, end))
    }
}

/// The paths an op names, and the one it writes (if any).
fn names(op: &TeamOp) -> (Vec<&str>, Option<&str>) {
    match op {
        TeamOp::New { path, .. } => (vec![], Some(path)),
        TeamOp::Revise { path, .. }
        | TeamOp::SetHead { path, .. }
        | TeamOp::SetBase { path, .. }
        | TeamOp::SetTags { path, .. } => (vec![path], Some(path)),
        TeamOp::Expect { path, .. } => (vec![path], None),
        TeamOp::AddRule { rule, .. } | TeamOp::SetRuleEnabled { rule, .. } => {
            let mut v: Vec<&str> = vec![&rule.conclusion.0];
            v.extend(rule.incidences.iter().map(|(p, _)| p.as_str()));
            (v, Some(&rule.conclusion.0))
        }
        TeamOp::SetTeamAccess { .. } | TeamOp::TeamShred { .. } | TeamOp::Compact { .. } => {
            (vec![], None)
        }
    }
}

/// A random 128-bit proposal id (§7.4: two machines of one member never
/// collide).
pub fn fresh_id() -> [u8; 16] {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("operating system randomness");
    id
}

impl Queue {
    /// Opens (or creates) the queue in `dir`, replaying its log; a torn tail
    /// is cut.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir.join("proposal-blobs"))?;
        let path = dir.join("proposals");
        let mut buf = Vec::new();
        if path.exists() {
            File::open(&path)?.read_to_end(&mut buf)?;
        }
        let log = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut q = Self {
            dir: dir.to_owned(),
            log,
            proposals: Vec::new(),
            listed: BTreeMap::new(),
        };
        let mut at = 0;
        while let Some((f, n)) = Frame::decode(&buf[at..]) {
            q.apply(f);
            at += n;
        }
        if at < buf.len() {
            q.log.set_len(at as u64)?;
            q.log.sync_all()?;
        }
        Ok(q)
    }

    fn apply(&mut self, f: Frame) {
        match f {
            Frame::Enqueue {
                id,
                author,
                ops,
                deps,
            } => self.proposals.push(Proposal {
                id,
                author: PrincipalId(author),
                ops,
                deps,
                state: State::Queued,
            }),
            Frame::State { id, state } => {
                if let Some(p) = self.proposals.iter_mut().find(|p| p.id == id) {
                    p.state = state;
                }
            }
            Frame::Listed { token, ids } => {
                self.listed.insert(token, ids);
            }
        }
    }

    fn append(&mut self, frames: Vec<Frame>) -> std::io::Result<()> {
        let mut bytes = Vec::new();
        for f in &frames {
            bytes.extend_from_slice(&f.encode());
        }
        self.log.write_all(&bytes)?;
        self.log.sync_data()?;
        for f in frames {
            self.apply(f);
        }
        Ok(())
    }

    /// Records state transitions durably, in order.
    pub fn transition(&mut self, changes: Vec<([u8; 16], State)>) -> std::io::Result<()> {
        self.append(
            changes
                .into_iter()
                .map(|(id, state)| Frame::State { id, state })
                .collect(),
        )
    }

    /// Records a confirm listing and returns its token (§7.2 step 5).
    pub fn list(&mut self, ids: Vec<[u8; 16]>) -> std::io::Result<Hash> {
        let mut sorted = ids.clone();
        sorted.sort();
        let mut h = blake3::Hasher::new();
        for i in &sorted {
            h.update(i);
        }
        let token = *h.finalize().as_bytes();
        self.append(vec![Frame::Listed { token, ids }])?;
        Ok(token)
    }

    pub fn get(&self, id: &[u8; 16]) -> Option<&Proposal> {
        self.proposals.iter().find(|p| p.id == *id)
    }

    /// The bytes a proposal's revision needs.
    pub fn blob(&self, h: &Hash) -> Option<Vec<u8>> {
        std::fs::read(self.dir.join("proposal-blobs").join(hex(h))).ok()
    }

    pub fn blobs_of(&self, p: &Proposal) -> BTreeMap<Hash, Vec<u8>> {
        let mut out = BTreeMap::new();
        for op in &p.ops {
            if let TeamOp::New { content, .. } | TeamOp::Revise { content, .. } = op
                && let Some(b) = self.blob(content)
            {
                out.insert(*content, b);
            }
        }
        out
    }

    /// Records each path a pending proposal creates, with its lane and
    /// author: queued records, as the local layer sees them.
    fn queued_records(&self, upto: usize) -> BTreeMap<String, (String, PrincipalId)> {
        let mut out = BTreeMap::new();
        for p in self.proposals[..upto].iter().filter(|p| p.state.pending()) {
            for op in &p.ops {
                if let TeamOp::New { path, lane, .. } = op {
                    out.entry(path.clone()).or_insert((lane.clone(), p.author));
                }
            }
        }
        out
    }

    /// Whether `who` may see proposal `p` (every lane it writes readable, or
    /// its own).
    pub fn visible_to(
        &self,
        p: &Proposal,
        who: PrincipalId,
        replica: &Replica,
        policy: &LocalPolicy,
    ) -> bool {
        if p.author == who || who.is_store_owner() {
            return true;
        }
        let records = records_of(replica, &self.queued_records(self.proposals.len()));
        p.ops.iter().all(|op| match names(op).1 {
            Some(path) => readable(replica, &records, policy, who, path),
            None => false,
        })
    }

    /// The queue as `who` may see it (§6.2: listings, `proposed` markers and
    /// WHAT_IF replies show nothing else, not even a count).
    pub fn listing_for(
        &self,
        who: PrincipalId,
        replica: &Replica,
        policy: &LocalPolicy,
    ) -> Vec<&Proposal> {
        self.proposals
            .iter()
            .filter(|p| self.visible_to(p, who, replica, policy))
            .collect()
    }

    /// Enters a proposal: both layers, then a durable enqueue frame.
    #[allow(clippy::too_many_arguments)]
    pub fn propose(
        &mut self,
        replica: &Replica,
        member: &str,
        author: PrincipalId,
        ops: Vec<TeamOp>,
        blobs: BTreeMap<Hash, Vec<u8>>,
        policy: &LocalPolicy,
    ) -> Result<[u8; 16], ProposeError> {
        if ops.is_empty() || ops.len() > MAX_OPS {
            return Err(ProposeError::TooLarge("ops"));
        }
        if (TeamBatch {
            proposals: vec![ops.clone()],
        })
        .encode()
        .len()
            > MAX_BATCH_BYTES
        {
            return Err(ProposeError::TooLarge("batch"));
        }
        if blobs.values().any(|b| b.len() > MAX_BLOB_BYTES) {
            return Err(ProposeError::TooLarge("blob"));
        }
        let queued = self.queued_records(self.proposals.len());
        local_layer(replica, &queued, policy, author, &ops)?;
        // Dependencies: earlier pending proposals that create or change a
        // path this one names.
        let named: BTreeSet<&str> = ops.iter().flat_map(|op| names(op).0).collect();
        let deps: Vec<[u8; 16]> = (self.proposals.iter())
            .filter(|p| p.state.pending())
            .filter(|p| {
                p.ops
                    .iter()
                    .any(|op| names(op).1.is_some_and(|w| named.contains(w)))
            })
            .map(|p| p.id)
            .collect();
        // The team layer: on the full overlay, and — to keep hidden
        // proposals hidden — again on the visible part when it fails.
        let id = fresh_id();
        let attempt = |visible_only: bool| -> Result<(), Reason> {
            let mut scratch = replica.clone();
            for p in self.proposals.iter().filter(|p| p.state.pending()) {
                if visible_only && !self.visible_to(p, author, replica, policy) {
                    continue;
                }
                let _ = scratch.scratch_apply(member, p.id, &p.ops, &self.blobs_of(p));
            }
            scratch.scratch_apply(member, id, &ops, &blobs)
        };
        if let Err(r) = attempt(false) {
            return Err(if attempt(true).is_ok() {
                ProposeError::QueueConflict
            } else {
                ProposeError::Team(r)
            });
        }
        for (h, b) in &blobs {
            let path = self.dir.join("proposal-blobs").join(hex(h));
            if !path.exists() {
                let tmp = path.with_extension("tmp");
                let mut f = File::create(&tmp)?;
                f.write_all(b)?;
                f.sync_all()?;
                std::fs::rename(&tmp, &path)?;
            }
        }
        self.append(vec![Frame::Enqueue {
            id,
            author: author.0,
            ops,
            deps,
        }])?;
        Ok(id)
    }
}

/// Every record the local layer can name: the replica's (owned by the store
/// owner, like every mirror) and the queued ones (owned by their authors).
fn records_of(
    replica: &Replica,
    queued: &BTreeMap<String, (String, PrincipalId)>,
) -> BTreeMap<String, (String, PrincipalId)> {
    let mut out = queued.clone();
    if let Some(s) = replica.keyed_state() {
        for r in s.records {
            out.insert(r.path, (r.lane, PrincipalId::STORE_OWNER));
        }
    }
    out
}

fn full_lane(replica: &Replica, lane: &str) -> String {
    format!("team:{}/{lane}", hex(&replica.team()))
}

fn readable(
    replica: &Replica,
    records: &BTreeMap<String, (String, PrincipalId)>,
    policy: &LocalPolicy,
    who: PrincipalId,
    path: &str,
) -> bool {
    let Some((lane, owner)) = records.get(path) else {
        return false;
    };
    let key = replica.key(path);
    let lane = full_lane(replica, lane);
    let dirs = BTreeSet::new();
    let facts = LogicalFacts {
        key: &key,
        lane: &lane,
        owner: *owner,
        dirs: &dirs,
    };
    access::authorized(policy.access, who, &facts).allowed
}

/// The local layer (§6.1 layer 1).
fn local_layer(
    replica: &Replica,
    queued: &BTreeMap<String, (String, PrincipalId)>,
    policy: &LocalPolicy,
    who: PrincipalId,
    ops: &[TeamOp],
) -> Result<(), ProposeError> {
    let mut records = records_of(replica, queued);
    let refuse = |i: usize, reference: &str, need: AccessNeed| {
        ProposeError::Local(TxError::Unauthorized {
            op_index: i,
            reference: reference.to_owned(),
            need,
        })
    };
    let lane_of = |records: &BTreeMap<String, (String, PrincipalId)>, path: &str| {
        full_lane(replica, &records[path].0)
    };
    for (i, op) in ops.iter().enumerate() {
        if matches!(
            op,
            TeamOp::SetTeamAccess { .. } | TeamOp::TeamShred { .. } | TeamOp::Compact { .. }
        ) {
            if !who.is_store_owner() {
                return Err(refuse(i, "team administration", AccessNeed::Curate));
            }
            continue;
        }
        let (read, write) = names(op);
        for path in &read {
            if !readable(replica, &records, policy, who, path) {
                return Err(refuse(i, path, AccessNeed::Read));
            }
        }
        let curate = matches!(op, TeamOp::SetBase { .. })
            || matches!(
                op,
                TeamOp::New {
                    adopt_as_base: true,
                    ..
                } | TeamOp::Revise {
                    adopt_as_base: true,
                    ..
                }
            );
        let lane = match op {
            TeamOp::New { path, lane, .. } => {
                if records.contains_key(path) {
                    return Err(ProposeError::Local(TxError::LogicalExists {
                        key: replica.key(path),
                    }));
                }
                full_lane(replica, lane)
            }
            _ => match write {
                Some(path) => lane_of(&records, path),
                None => continue,
            },
        };
        if !access::has_role(policy.access, who, &lane, Role::Writer) {
            return Err(refuse(i, write.unwrap_or_default(), AccessNeed::Write));
        }
        if curate && !access::has_role(policy.access, who, &lane, Role::Curator) {
            return Err(refuse(i, write.unwrap_or_default(), AccessNeed::Curate));
        }
        if !access::egress(policy.access, &lane) {
            return Err(ProposeError::EgressDenied { lane });
        }
        if let TeamOp::New { path, lane, .. } = op {
            records.insert(path.clone(), (lane.clone(), who));
        }
    }
    Ok(())
}

/// The local layer again, at publish, under the policy then in force
/// (§7.2 step 5, R5-M1): a proposal that no longer passes is not sent.
pub fn recheck(
    replica: &Replica,
    queue: &Queue,
    upto: usize,
    p: &Proposal,
    policy: &LocalPolicy,
) -> Result<(), ProposeError> {
    local_layer(
        replica,
        &queue.queued_records(upto),
        policy,
        p.author,
        &p.ops,
    )
}
