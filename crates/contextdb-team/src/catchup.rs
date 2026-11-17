//! Mirrors in the local kernel and their catch-up (ADR 0018 §5.1–5.2, SPEC
//! SD21.7–8).
//!
//! The replica's **projection** is what every mirror must hold: the team
//! record's revisions in creation order, its head, its base — on the head
//! only, true exactly while the record is certified at the root — and its
//! tags. The **history** keeps the projection after every decided entry as
//! a delta, so a branch can be brought from any position it has recorded to
//! the tip entry by entry.
//!
//! Catch-up brings one open branch to the tip: it diffs the branch's actual
//! mirror state (not its previous watermark) against the projection, so
//! whatever a fork or an old snapshot left is corrected, in batches cut so
//! that no mirror's `(head, base)` returns within a batch to a value it
//! already had there; each batch ends with the branch's `team:` watermark,
//! a CAS on its previous position. A position the history does not hold
//! (a rebuild from a later checkpoint, a rejoin) gives one net batch with
//! `gap = true`.

use crate::commit::hex;
use crate::ops::Hash;
use crate::replica::Replica;
use contextdb_core::Kernel;
use contextdb_model::{
    Batch, BranchId, Channel, CommitResult, LogicalId, LogicalRef, MAIN, Op, Origin, RecordKind,
    RevRef, RevisionId, TeamId, TxError,
};
use std::collections::{BTreeMap, BTreeSet};

/// What one mirror must hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorState {
    pub kind: RecordKind,
    pub lane: String,
    /// Every revision's content, in creation order.
    pub revisions: Vec<Hash>,
    pub head: Hash,
    /// The head's base bit: certified at the root.
    pub certified: bool,
    pub tags: BTreeSet<String>,
}

/// Path → mirror state.
pub type Projection = BTreeMap<String, MirrorState>;

/// The projection of a replica now.
pub fn projection(r: &Replica) -> Projection {
    let Some(s) = r.keyed_state() else {
        return Projection::new();
    };
    s.records
        .into_iter()
        .map(|x| {
            (
                x.path,
                MirrorState {
                    kind: x.kind,
                    lane: x.lane,
                    revisions: x.revisions.iter().map(|v| v.content).collect(),
                    head: x.head,
                    certified: x.certified,
                    tags: x.tags,
                },
            )
        })
        .collect()
}

/// The projection after every decided entry, as deltas.
#[derive(Clone, Debug, Default)]
pub struct History {
    /// The entry the history starts after; `None` is before genesis.
    pub start_oid: Option<String>,
    pub start: Projection,
    /// `(entry oid, records whose state changed)`, in chain order.
    pub entries: Vec<(String, Projection)>,
    current: Projection,
}

impl History {
    /// A history starting after `oid` at `start` (a checkpoint join).
    pub fn starting_at(oid: Option<String>, start: Projection) -> Self {
        Self {
            start_oid: oid,
            current: start.clone(),
            start,
            entries: Vec::new(),
        }
    }
    /// Records the projection after entry `oid`.
    pub fn record(&mut self, oid: &str, after: Projection) {
        let delta: Projection = (after.iter())
            .filter(|(p, s)| self.current.get(*p) != Some(*s))
            .map(|(p, s)| (p.clone(), s.clone()))
            .collect();
        self.entries.push((oid.to_owned(), delta));
        self.current = after;
    }
    /// The entry catch-up brings branches to.
    pub fn tip(&self) -> Option<&str> {
        self.entries
            .last()
            .map(|(o, _)| o.as_str())
            .or(self.start_oid.as_deref())
    }
    /// How many entries are applied at position `oid` (`None`: the start
    /// of a genesis history); `None` if the history does not hold it.
    pub fn position(&self, oid: Option<&str>) -> Option<usize> {
        match oid {
            None => self.start_oid.is_none().then_some(0),
            Some(o) if self.start_oid.as_deref() == Some(o) => Some(0),
            Some(o) => self.entries.iter().position(|(e, _)| e == o).map(|i| i + 1),
        }
    }
    /// The full projection after the first `n` entries.
    pub fn projection_at(&self, n: usize) -> Projection {
        let mut p = self.start.clone();
        for (_, d) in &self.entries[..n] {
            for (k, v) in d {
                p.insert(k.clone(), v.clone());
            }
        }
        p
    }
    pub fn current(&self) -> &Projection {
        &self.current
    }
}

/// A mirror as one branch holds it.
#[derive(Clone, Debug)]
struct Held {
    head: Hash,
    head_id: RevisionId,
    /// content → (global revision id, base).
    revisions: BTreeMap<Hash, (RevisionId, bool)>,
    tags: BTreeSet<String>,
}

impl Held {
    fn value(&self) -> (Hash, bool) {
        (self.head, self.revisions[&self.head].1)
    }
}

pub fn scope(team: TeamId) -> String {
    contextdb_model::team_scope(&team)
}

fn key(team: TeamId, path: &str) -> String {
    format!("team/{}/{path}", hex(&team))
}

/// The branch's mirrors of `team`, by path, in global ids.
fn held(k: &Kernel, b: BranchId, team: TeamId) -> BTreeMap<String, Held> {
    let v = k.branch(b).expect("open branch");
    let db = v.db();
    let prefix = format!("team/{}/", hex(&team));
    let mut out = BTreeMap::new();
    let mut by_logical: BTreeMap<LogicalId, String> = BTreeMap::new();
    for n in 0..db.logical_count() as u32 {
        let l = LogicalId(n);
        if db.logical_origin(l) != (Origin::Replica { team }) {
            continue;
        }
        let (key, ..) = db.logical_info(l).expect("visible");
        let Some(path) = key.strip_prefix(&prefix) else {
            continue;
        };
        by_logical.insert(l, path.to_owned());
        let head = db.head(l).expect("head");
        out.insert(
            path.to_owned(),
            Held {
                head: db.revision_content(head).expect("revision"),
                head_id: v.global_revision(head),
                revisions: BTreeMap::new(),
                tags: v
                    .metadata(v.global_logical(l))
                    .map(|m| m.tags)
                    .unwrap_or_default(),
            },
        );
    }
    for n in 0..db.revision_count() as u32 {
        let r = RevisionId(n);
        let Some(path) = db.revision_logical(r).and_then(|l| by_logical.get(&l)) else {
            continue;
        };
        let content = db.revision_content(r).expect("revision");
        let base = db.is_base(r).expect("revision");
        out.get_mut(path)
            .expect("indexed")
            .revisions
            .insert(content, (v.global_revision(r), base));
    }
    out
}

/// The ops that bring branch state `have` to `want`, in the order of §5.2:
/// attach/create, revisions, base retractions, head moves, base
/// assertions, tags.
fn diff(k: &Kernel, team: TeamId, have: &BTreeMap<String, Held>, want: &Projection) -> Vec<Op> {
    let mut first = Vec::new();
    let mut revs = Vec::new();
    let mut retract = Vec::new();
    let mut heads = Vec::new();
    let mut assert = Vec::new();
    let mut tags = Vec::new();
    // Revision slots: (path, content) → New(k).
    let mut slots: BTreeMap<(String, Hash), u16> = BTreeMap::new();
    let mut next_slot = 0u16;
    let mut later: Vec<(&String, &MirrorState, Option<&Held>)> = Vec::new();
    let mut attached: BTreeSet<&String> = BTreeSet::new();
    for (path, st) in want {
        let key = key(team, path);
        match have.get(path) {
            Some(h) => later.push((path, st, Some(h))),
            None if k.replica_logical(&key).is_some() => {
                // Attach at the head; the other revisions follow by dedup.
                first.push(Op::MirrorAttach {
                    key,
                    head: st.head,
                    base: st.certified,
                });
                attached.insert(path);
                later.push((path, st, None));
            }
            None => {
                let base = st.revisions[0] == st.head && st.certified;
                first.push(Op::CreateLogical {
                    key,
                    kind: st.kind,
                    scope_lane: format!("team:{}/{}", hex(&team), st.lane),
                    content: st.revisions[0],
                    adopt_as_base: base,
                    origin: Origin::Replica { team },
                });
                slots.insert((path.clone(), st.revisions[0]), next_slot);
                next_slot += 1;
                later.push((path, st, None));
            }
        }
    }
    for (path, st, h) in &later {
        let key = key(team, path);
        let is_attached = attached.contains(path);
        let present = |c: &Hash, slots: &BTreeMap<(String, Hash), u16>| {
            h.is_some_and(|h| h.revisions.contains_key(c))
                || slots.contains_key(&((*path).clone(), *c))
                || (is_attached && *c == st.head)
        };
        for c in &st.revisions {
            if !present(c, &slots) {
                revs.push(Op::AddRevision {
                    logical: LogicalRef::Key(key.clone()),
                    content: *c,
                    adopt_as_base: false,
                });
                slots.insert(((*path).clone(), *c), next_slot);
                next_slot += 1;
            }
        }
        let rev = |c: &Hash| -> RevRef {
            match slots.get(&((*path).clone(), *c)) {
                Some(s) => RevRef::New(*s),
                None => h.expect("visible").revisions[c].0.into(),
            }
        };
        match h {
            Some(h) => {
                for (c, (id, base)) in &h.revisions {
                    if *base && !(*c == st.head && st.certified) {
                        retract.push(Op::RetractBase { rev: (*id).into() });
                    }
                }
                if h.head != st.head {
                    heads.push(Op::SetHead {
                        logical: LogicalRef::Key(key.clone()),
                        expected: h.head_id.into(),
                        new: rev(&st.head),
                    });
                }
                let head_base = h.revisions.get(&st.head).is_some_and(|x| x.1);
                if st.certified && !head_base {
                    assert.push(Op::AssertBase { rev: rev(&st.head) });
                }
                if h.tags != st.tags {
                    tags.push(Op::SetMetadata {
                        logical: LogicalRef::Key(key.clone()),
                        tags: st.tags.clone(),
                        dirs: BTreeSet::new(),
                    });
                }
            }
            None => {
                // Created here: the head may be a later revision.
                // (An attached mirror got its head and base from the attach.)
                if let Some(first_slot) = slots.get(&((*path).clone(), st.revisions[0]))
                    && !is_attached
                    && st.revisions[0] != st.head
                {
                    heads.push(Op::SetHead {
                        logical: LogicalRef::Key(key.clone()),
                        expected: RevRef::New(*first_slot),
                        new: rev(&st.head),
                    });
                    if st.certified {
                        assert.push(Op::AssertBase { rev: rev(&st.head) });
                    }
                }
                if !st.tags.is_empty() {
                    tags.push(Op::SetMetadata {
                        logical: LogicalRef::Key(key.clone()),
                        tags: st.tags.clone(),
                        dirs: BTreeSet::new(),
                    });
                }
            }
        }
    }
    [first, revs, retract, heads, assert, tags].concat()
}

/// Where catch-up batches go: the kernel itself, or a store that logs them
/// first (the daemon's single writer).
pub trait Writer {
    type Error: From<TxError>;
    fn kernel(&self) -> &Kernel;
    fn commit(&mut self, b: &Batch) -> Result<CommitResult, Self::Error>;
}
impl Writer for Kernel {
    type Error = TxError;
    fn kernel(&self) -> &Kernel {
        self
    }
    fn commit(&mut self, b: &Batch) -> Result<CommitResult, TxError> {
        Kernel::commit(self, b)
    }
}

/// Brings branch `b` to the history's tip. Returns every committed batch's
/// result, in order.
pub fn catch_up<W: Writer + ?Sized>(
    w: &mut W,
    b: BranchId,
    team: TeamId,
    h: &History,
) -> Result<Vec<CommitResult>, W::Error> {
    let mut done = Vec::new();
    let Some(tip) = h.tip().map(str::to_owned) else {
        return Ok(done);
    };
    let scope = scope(team);
    let watermark = |k: &Kernel| {
        k.branch(b)
            .expect("open branch")
            .db()
            .watermark(&scope)
            .map(<[u8]>::to_vec)
    };
    let at = watermark(w.kernel()).map(|w| String::from_utf8_lossy(&w).into_owned());
    if at.as_deref() == Some(tip.as_str()) {
        return Ok(done);
    }
    let commit = |w: &mut W, want: &Projection, oid: &str, gap: bool| {
        let k = w.kernel();
        let have = held(k, b, team);
        let mut ops = diff(k, team, &have, want);
        ops.push(Op::ObserverWatermark {
            scope: scope.clone(),
            expected: watermark(k),
            position: oid.as_bytes().to_vec(),
            gap,
        });
        let mut batch = Batch::new(ops);
        batch.branch = b;
        batch.channel = Channel::Replica;
        w.commit(&batch)
    };
    let Some(from) = h.position(at.as_deref()) else {
        // A position the history does not hold: one net batch, gap.
        done.push(commit(w, h.current(), &tip, true)?);
        return Ok(done);
    };
    // Walk the entries, cutting before any entry that would bring a mirror
    // back to a value it already had within the current batch.
    let mut proj = h.projection_at(from);
    let start = held(w.kernel(), b, team);
    let mut seen: BTreeMap<String, BTreeSet<(Hash, bool)>> = BTreeMap::new();
    let mut value: BTreeMap<String, (Hash, bool)> = BTreeMap::new();
    for (p, x) in &start {
        seen.entry(p.clone()).or_default().insert(x.value());
        value.insert(p.clone(), x.value());
    }
    for (i, (_, delta)) in h.entries.iter().enumerate().skip(from) {
        let returns = delta.iter().any(|(p, st)| {
            let v = (st.head, st.certified);
            value.get(p) != Some(&v) && seen.get(p).is_some_and(|s| s.contains(&v))
        });
        if returns {
            let prev = &h.entries[i - 1].0;
            done.push(commit(w, &proj, prev, false)?);
            seen.clear();
            for (p, v) in &value {
                seen.entry(p.clone()).or_default().insert(*v);
            }
        }
        for (p, st) in delta {
            let v = (st.head, st.certified);
            seen.entry(p.clone()).or_default().insert(v);
            value.insert(p.clone(), v);
            proj.insert(p.clone(), st.clone());
        }
    }
    done.push(commit(w, &proj, &tip, false)?);
    Ok(done)
}

/// Brings every open branch to the tip, `main` first, then by id
/// (ADR 0018 §5.2, Q7). Archived branches are not written. Stops at the
/// first branch whose catch-up fails, naming it.
pub fn catch_up_all<W: Writer + ?Sized>(
    w: &mut W,
    team: TeamId,
    h: &History,
) -> Result<usize, (BranchId, W::Error)> {
    let open: Vec<BranchId> = (w.kernel().branches())
        .filter(|b| !b.archived)
        .map(|b| b.id)
        .collect();
    let mut total = 0;
    for b in std::iter::once(MAIN).chain(open.into_iter().filter(|b| *b != MAIN)) {
        total += catch_up(w, b, team, h).map_err(|e| (b, e))?.len();
    }
    Ok(total)
}

/// Leaving a team on branch `b` (ADR 0018 §8.5): one replica-channel batch
/// that retracts every base of the team's mirrors and sets the watermark to
/// [`TEAM_LEFT`]. `None` when the branch has already left.
pub fn leave<W: Writer + ?Sized>(
    w: &mut W,
    b: BranchId,
    team: TeamId,
) -> Result<Option<CommitResult>, W::Error> {
    let scope = scope(team);
    let (ops, watermark) = {
        let k = w.kernel();
        let v = k.branch(b).expect("open branch");
        let db = v.db();
        let watermark = db.watermark(&scope).map(<[u8]>::to_vec);
        let mut ops: Vec<Op> = (0..db.revision_count() as u32)
            .map(RevisionId)
            .filter(|r| {
                db.revision_logical(*r)
                    .is_some_and(|l| db.logical_origin(l) == (Origin::Replica { team }))
                    && db.is_base(*r).unwrap_or(false)
            })
            .map(|r| Op::RetractBase {
                rev: RevRef::Existing(v.global_revision(r)),
            })
            .collect();
        if ops.is_empty() && watermark.as_deref() == Some(contextdb_model::TEAM_LEFT) {
            return Ok(None);
        }
        ops.push(Op::ObserverWatermark {
            scope: scope.clone(),
            expected: watermark.clone(),
            position: contextdb_model::TEAM_LEFT.to_vec(),
            gap: false,
        });
        (ops, watermark)
    };
    let _ = watermark;
    let mut batch = Batch::new(ops);
    batch.branch = b;
    batch.channel = Channel::Replica;
    w.commit(&batch).map(Some)
}

/// [`leave`] on every open branch, `main` first. Archived branches are
/// not written.
pub fn leave_all<W: Writer + ?Sized>(
    w: &mut W,
    team: TeamId,
) -> Result<usize, (BranchId, W::Error)> {
    let open: Vec<BranchId> = (w.kernel().branches())
        .filter(|b| !b.archived)
        .map(|b| b.id)
        .collect();
    let mut n = 0;
    for b in std::iter::once(MAIN).chain(open.into_iter().filter(|b| *b != MAIN)) {
        n += usize::from(leave(w, b, team).map_err(|e| (b, e))?.is_some());
    }
    Ok(n)
}
