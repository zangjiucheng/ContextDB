//! Independent oracle for team acceptance `accept_version 1` and team
//! support v1 (ADR 0018 §4.2–4.3, §13 PROP17; SPEC SD21.4–5).
//!
//! It shares no code with `contextdb-team` or the core kernel: its own op
//! and document types, its own staging, the oracle's `InputModel` for the
//! kernel's transaction rules and `jacobi` for status. Entry-level checks
//! (commit structure, trailers, signatures) are parsing; the caller tells
//! the oracle which entries are structurally rejected. The state limit is
//! not modelled (callers keep states far below it).

use crate::{InputModel, jacobi, summarize};
use contextdb_model::{
    Batch, BlobPresence, Channel, ContentHash, EdgeKind, LogicalRef, MAIN, Op, PolicyDoc,
    PrincipalId, RecordKind, RevRef, RevisionId, RuleId, RuleRef, Summary,
};
use std::collections::{BTreeMap, BTreeSet};

pub type H = [u8; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OEdge {
    Pin(H),
    Cur(H),
    Ref(H),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ORule {
    pub conclusion: (String, H),
    pub incidences: Vec<(String, OEdge)>,
}

/// Roles already expanded to member sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ODoc {
    pub admins: BTreeSet<String>,
    pub members: BTreeSet<String>,
    pub writers: BTreeMap<String, BTreeSet<String>>,
    pub curators: BTreeMap<String, BTreeSet<String>>,
    pub require_deps: BTreeSet<RecordKind>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OOp {
    New {
        path: String,
        lane: String,
        kind: RecordKind,
        content: H,
        base: bool,
    },
    Revise {
        path: String,
        content: H,
        base: bool,
    },
    SetHead {
        path: String,
        expected: H,
        new: H,
    },
    SetBase {
        path: String,
        content: H,
        expected: bool,
        base: bool,
    },
    AddRule {
        rule: ORule,
        enabled: bool,
    },
    SetRuleEnabled {
        rule: ORule,
        expected: bool,
        enabled: bool,
    },
    SetTags {
        path: String,
        expected: BTreeSet<String>,
        tags: BTreeSet<String>,
    },
    Expect {
        path: String,
        content: H,
    },
    SetTeamAccess {
        doc: ODoc,
    },
    TeamShred {
        content: H,
    },
}

/// One record as the oracle sees it, in the order the comparison needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ORecord {
    pub lane: String,
    pub kind: RecordKind,
    pub tags: BTreeSet<String>,
    pub certified: bool,
    pub head: H,
    /// `(content, base, shredded)` in creation order.
    pub revisions: Vec<(H, bool, bool)>,
}

#[derive(Clone, Debug, Default)]
struct Rec {
    lane: String,
    kind: Option<RecordKind>,
    revs: Vec<H>,
    head: H,
}

#[derive(Clone, Debug)]
pub struct TeamOracle {
    team_hex: String,
    model: InputModel,
    doc: Option<ODoc>,
    recs: BTreeMap<String, Rec>,
    tags: BTreeMap<String, BTreeSet<String>>,
    base: BTreeMap<(String, H), bool>,
    ids: BTreeMap<(String, H), RevisionId>,
    rules: BTreeMap<ORule, (RuleId, bool)>,
    shred: BTreeSet<H>,
    blobs: BTreeSet<H>,
    accepted: BTreeSet<(String, [u8; 16])>,
    seq: Option<u64>,
}

struct Blobs<'a>(&'a BTreeSet<H>);
impl BlobPresence for Blobs<'_> {
    fn contains(&self, c: &ContentHash) -> bool {
        self.0.contains(c)
    }
}

impl TeamOracle {
    pub fn new(team_hex: &str) -> Self {
        Self {
            team_hex: team_hex.to_owned(),
            model: InputModel::default(),
            doc: None,
            recs: BTreeMap::new(),
            tags: BTreeMap::new(),
            base: BTreeMap::new(),
            ids: BTreeMap::new(),
            rules: BTreeMap::new(),
            shred: BTreeSet::new(),
            blobs: BTreeSet::new(),
            accepted: BTreeSet::new(),
            seq: None,
        }
    }

    /// A structurally rejected entry: decides nothing, takes its position.
    pub fn reject_entry(&mut self, seq: u64) {
        self.seq = Some(seq);
    }

    /// Decides an entry's proposals; returns `Ok` or the rejection code of
    /// each. `blobs` are the contents the entry carries.
    pub fn entry(
        &mut self,
        seq: u64,
        author: &str,
        proposals: &[([u8; 16], Vec<OOp>)],
        blobs: &BTreeSet<H>,
    ) -> Vec<Result<(), String>> {
        let out = proposals
            .iter()
            .map(|(id, ops)| {
                if self.accepted.contains(&(author.to_owned(), *id)) {
                    return Err("DuplicateProposal".to_owned());
                }
                let mut next = self.clone();
                next.proposal(author, ops, blobs)?;
                next.accepted.insert((author.to_owned(), *id));
                *self = next;
                Ok(())
            })
            .collect();
        self.seq = Some(seq);
        out
    }

    fn key(&self, path: &str) -> String {
        format!("team/{}/{path}", self.team_hex)
    }

    /// Applies one proposal to `self` (a clone the caller discards on error).
    fn proposal(
        &mut self,
        author: &str,
        ops: &[OOp],
        entry_blobs: &BTreeSet<H>,
    ) -> Result<(), String> {
        let mut kernel: Vec<Op> = Vec::new();
        let (mut new_logicals, mut new_revs, mut new_rules) = (
            Vec::<String>::new(),
            Vec::<(String, H)>::new(),
            Vec::<ORule>::new(),
        );
        let pre_heads: BTreeMap<String, H> =
            self.recs.iter().map(|(p, r)| (p.clone(), r.head)).collect();
        let present: BTreeSet<H> = self.blobs.union(entry_blobs).copied().collect();
        let can = |doc: &ODoc, set: &BTreeMap<String, BTreeSet<String>>, lane: &str| {
            set.get(lane).is_some_and(|m| m.contains(author)) && doc.members.contains(author)
        };
        for op in ops {
            let doc = match (&self.doc, op) {
                (Some(d), _) => d.clone(),
                (None, OOp::SetTeamAccess { doc }) => doc.clone(),
                (None, _) => return Err("Genesis".into()),
            };
            let need = |curate: bool, lane: &str| -> Result<(), String> {
                let ok = if curate {
                    can(&doc, &doc.curators, lane)
                } else {
                    can(&doc, &doc.writers, lane)
                };
                if ok {
                    Ok(())
                } else if curate {
                    Err("Unauthorized(Curate)".into())
                } else {
                    Err("Unauthorized(Write)".into())
                }
            };
            let rev_ref =
                |s: &Self, new_revs: &Vec<(String, H)>, p: &str, h: &H| -> Result<RevRef, String> {
                    if let Some(r) = s.ids.get(&(p.to_owned(), *h)) {
                        return Ok(RevRef::Existing(*r));
                    }
                    new_revs
                        .iter()
                        .position(|(q, c)| q == p && c == h)
                        .map(|k| RevRef::New(k as u16))
                        .ok_or_else(|| "UnknownRevision".to_owned())
                };
            let usable = |s: &Self, h: &H| -> Result<(), String> {
                if s.shred.contains(h) {
                    Err("ContentShredded".into())
                } else if !present.contains(h) {
                    Err("MissingBlob".into())
                } else {
                    Ok(())
                }
            };
            let lref = |s: &Self, new_logicals: &Vec<String>, p: &str| -> LogicalRef {
                match new_logicals.iter().position(|q| q == p) {
                    Some(k) => LogicalRef::New(k as u16),
                    None => LogicalRef::Key(s.key(p)),
                }
            };
            match op {
                OOp::New {
                    path,
                    lane,
                    kind,
                    content,
                    base,
                } => {
                    if !doc.writers.contains_key(lane) {
                        return Err("UnknownLane".into());
                    }
                    need(false, lane)?;
                    if *base {
                        need(true, lane)?;
                    }
                    if self.recs.contains_key(path) {
                        return Err("LogicalExists".into());
                    }
                    usable(self, content)?;
                    kernel.push(Op::CreateLogical {
                        origin: Default::default(),
                        key: self.key(path),
                        kind: *kind,
                        scope_lane: format!("team:{}/{lane}", self.team_hex),
                        content: *content,
                        adopt_as_base: *base,
                    });
                    new_logicals.push(path.clone());
                    new_revs.push((path.clone(), *content));
                    self.recs.insert(
                        path.clone(),
                        Rec {
                            lane: lane.clone(),
                            kind: Some(*kind),
                            revs: vec![*content],
                            head: *content,
                        },
                    );
                    self.base.insert((path.clone(), *content), *base);
                }
                OOp::Revise {
                    path,
                    content,
                    base,
                } => {
                    let lane = self.recs.get(path).ok_or("UnknownKey")?.lane.clone();
                    need(false, &lane)?;
                    if *base {
                        need(true, &lane)?;
                    }
                    usable(self, content)?;
                    kernel.push(Op::AddRevision {
                        logical: lref(self, &new_logicals, path),
                        content: *content,
                        adopt_as_base: *base,
                    });
                    new_revs.push((path.clone(), *content));
                    let rec = self.recs.get_mut(path).expect("checked");
                    if !rec.revs.contains(content) {
                        rec.revs.push(*content);
                    }
                    if *base {
                        self.base.insert((path.clone(), *content), true);
                    }
                }
                OOp::SetHead {
                    path,
                    expected,
                    new,
                } => {
                    let rec = self.recs.get(path).ok_or("UnknownKey")?;
                    need(false, &rec.lane.clone())?;
                    if rec.head != *expected {
                        return Err("HeadConflict".into());
                    }
                    let e = rev_ref(self, &new_revs, path, expected)?;
                    let n = rev_ref(self, &new_revs, path, new)?;
                    kernel.push(Op::SetHead {
                        logical: lref(self, &new_logicals, path),
                        expected: e,
                        new: n,
                    });
                    self.recs.get_mut(path).expect("checked").head = *new;
                }
                OOp::SetBase {
                    path,
                    content,
                    expected,
                    base,
                } => {
                    let lane = self.recs.get(path).ok_or("UnknownKey")?.lane.clone();
                    need(true, &lane)?;
                    let r = rev_ref(self, &new_revs, path, content)?;
                    let now = self
                        .base
                        .get(&(path.clone(), *content))
                        .copied()
                        .unwrap_or(false);
                    if now != *expected {
                        return Err("BaseConflict".into());
                    }
                    if base != expected {
                        kernel.push(if *base {
                            Op::AssertBase { rev: r }
                        } else {
                            Op::RetractBase { rev: r }
                        });
                        self.base.insert((path.clone(), *content), *base);
                    }
                }
                OOp::AddRule { rule, enabled } => {
                    let (cp, ch) = &rule.conclusion;
                    let lane = self.recs.get(cp).ok_or("UnknownKey")?.lane.clone();
                    need(false, &lane)?;
                    let c = rev_ref(self, &new_revs, cp, ch)?;
                    let mut inc = Vec::new();
                    for (p, e) in &rule.incidences {
                        let (k, h) = match e {
                            OEdge::Pin(h) => (EdgeKind::Pin, h),
                            OEdge::Ref(h) => (EdgeKind::Ref, h),
                            OEdge::Cur(h) => {
                                if self.recs.get(p).ok_or("UnknownKey")?.head != *h {
                                    return Err("CurMoved".into());
                                }
                                (EdgeKind::Cur, h)
                            }
                        };
                        inc.push((rev_ref(self, &new_revs, p, h)?, k));
                    }
                    kernel.push(Op::AddRule {
                        conclusion: c,
                        incidences: inc,
                        enabled: *enabled,
                    });
                    new_rules.push(rule.clone());
                    self.rules
                        .insert(rule.clone(), (RuleId(u32::MAX), *enabled));
                }
                OOp::SetRuleEnabled {
                    rule,
                    expected,
                    enabled,
                } => {
                    let lane = self
                        .recs
                        .get(&rule.conclusion.0)
                        .ok_or("UnknownKey")?
                        .lane
                        .clone();
                    need(false, &lane)?;
                    let (id, now) = *self.rules.get(rule).ok_or("UnknownRule")?;
                    let r = match new_rules.iter().position(|x| x == rule) {
                        Some(k) => RuleRef::New(k as u16),
                        None => RuleRef::Existing(id),
                    };
                    if now != *expected {
                        return Err("EnabledConflict".into());
                    }
                    if enabled != expected {
                        kernel.push(Op::SetRuleEnabled {
                            rule: r,
                            enabled: *enabled,
                        });
                        self.rules.get_mut(rule).expect("checked").1 = *enabled;
                    }
                }
                OOp::SetTags {
                    path,
                    expected,
                    tags,
                } => {
                    let lane = self.recs.get(path).ok_or("UnknownKey")?.lane.clone();
                    need(false, &lane)?;
                    if self.tags.get(path).cloned().unwrap_or_default() != *expected {
                        return Err("TagsConflict".into());
                    }
                    self.tags.insert(path.clone(), tags.clone());
                }
                OOp::Expect { path, content } => {
                    if pre_heads.get(path).ok_or("UnknownKey")? != content {
                        return Err("ReadSetConflict".into());
                    }
                }
                OOp::SetTeamAccess { doc: new } => {
                    if !doc.admins.contains(author) {
                        return Err("AdminOnly".into());
                    }
                    if self
                        .recs
                        .values()
                        .any(|r| !new.writers.contains_key(&r.lane))
                    {
                        return Err("LaneInUse".into());
                    }
                    if self.doc.as_ref().map(|d| &d.require_deps) != Some(&new.require_deps) {
                        kernel.push(Op::SetPolicy {
                            policy: PolicyDoc {
                                version: 1,
                                require_deps: new.require_deps.clone(),
                            },
                        });
                    }
                    self.doc = Some(new.clone());
                }
                OOp::TeamShred { content } => {
                    if !doc.admins.contains(author) {
                        return Err("AdminOnly".into());
                    }
                    self.shred.insert(*content);
                }
            }
        }
        if !kernel.is_empty() {
            let batch = Batch {
                branch: MAIN,
                principal: PrincipalId::STORE_OWNER,
                channel: Channel::Admin,
                client_txn_id: None,
                ops: kernel,
            };
            let r = self
                .model
                .apply_with(&batch, &Blobs(&present))
                .map_err(|e| {
                    format!(
                        "Kernel:{}",
                        format!("{e:?}")
                            .split(|c: char| !c.is_ascii_alphanumeric())
                            .next()
                            .unwrap_or("")
                    )
                })?;
            for ((p, h), id) in new_revs.iter().zip(&r.allocations.new_revisions) {
                self.ids.entry((p.clone(), *h)).or_insert(*id);
            }
            for (rule, id) in new_rules.iter().zip(&r.allocations.new_rules) {
                self.rules.get_mut(rule).expect("staged").0 = *id;
            }
        }
        for (_, h) in &new_revs {
            if !self.shred.contains(h) {
                self.blobs.insert(*h);
            }
        }
        for h in &self.shred.clone() {
            self.blobs.remove(h);
        }
        Ok(())
    }

    pub fn seq(&self) -> Option<u64> {
        self.seq
    }

    /// Every record: lane, kind, tags, team status (team support v1 by the
    /// oracle's own fixpoint), head and revisions.
    pub fn records(&self) -> BTreeMap<String, ORecord> {
        let inputs = self.model.inputs();
        let derived = jacobi(inputs);
        self.recs
            .iter()
            .map(|(p, r)| {
                let head_id = self.ids[&(p.clone(), r.head)];
                let certified =
                    summarize(&derived.nodes[head_id.index()].dimensions) == Summary::Certified;
                let revisions = r
                    .revs
                    .iter()
                    .map(|h| {
                        let id = self.ids[&(p.clone(), *h)];
                        (*h, inputs.base[id.index()], self.shred.contains(h))
                    })
                    .collect();
                (
                    p.clone(),
                    ORecord {
                        lane: r.lane.clone(),
                        kind: r.kind.expect("kind"),
                        tags: self.tags.get(p).cloned().unwrap_or_default(),
                        certified,
                        head: r.head,
                        revisions,
                    },
                )
            })
            .collect()
    }

    pub fn rules(&self) -> BTreeMap<ORule, bool> {
        let inputs = self.model.inputs();
        self.rules
            .iter()
            .map(|(r, (id, _))| (r.clone(), inputs.rules[id.index()].enabled))
            .collect()
    }

    pub fn shred(&self) -> &BTreeSet<H> {
        &self.shred
    }
    pub fn accepted(&self) -> &BTreeSet<(String, [u8; 16])> {
        &self.accepted
    }
}
