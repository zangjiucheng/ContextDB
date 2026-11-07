//! The single redaction point (ADR 0017 §5.1): every response to a
//! principal other than the store owner passes through [`Viewer::redact`]
//! on its way to the encoder. A response names only logicals the principal
//! is authorised for, with their revisions and rules; everything else is
//! dropped, or counted where §2.3 allows a count. The store owner's
//! responses are never filtered (§5.10).
//!
//! Reads that leave a trace (ledger entries, certificates, recall slots)
//! are also filtered where they are computed (§5.2, §5.3); this pass is the
//! last line, and it is total: a response type a non-owner can never
//! receive becomes `ChannelRestricted`.
use crate::proto::*;
use contextdb_core::{BranchView, Impact, NodeChange, Proof};
use contextdb_model::*;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
};

thread_local! {
    /// The branch the request being handled on this thread reads: the
    /// bound session's, or the one `OnBranch` named. Directory grants are
    /// judged against that branch's `dirs` (ADR 0017 §4.4).
    static BRANCH: Cell<Option<BranchId>> = const { Cell::new(None) };
}
/// Notes the branch the current request reads (see [`BRANCH`]).
pub(crate) fn note_branch(b: BranchId) {
    BRANCH.with(|c| c.set(Some(b)));
}
/// Takes (and clears) the branch noted for the current request.
pub(crate) fn take_branch() -> Option<BranchId> {
    BRANCH.with(Cell::take)
}

/// A stand-in for a hidden logical where a response's shape needs one
/// (`Validation::Ambiguous`): never a real id (ids are dense from 0).
pub const REDACTED_LOGICAL: LogicalId = LogicalId(u32::MAX);

/// Error codes whose detail names nothing but the caller's own input or
/// fixed text; any other code keeps its code and loses its detail (§5.7).
const PLAIN_ERRORS: &[&str] = &[
    "AccessRevoked",
    "ChannelRestricted",
    "InvalidRequest",
    "SessionOfAnotherPrincipal",
    "Timeout",
    "Unavailable",
    "Unclaimed",
    "UnknownSession",
];

/// What one principal may see on one branch, decided once per logical.
pub struct Viewer<'a, 'v> {
    p: PrincipalId,
    db: &'a BranchView<'v>,
    seen: RefCell<BTreeMap<LogicalId, bool>>,
}

impl<'a, 'v> Viewer<'a, 'v> {
    /// `p` reading `db`, under the access document of `db`'s kernel.
    pub fn new(p: PrincipalId, db: &'a BranchView<'v>) -> Self {
        Self {
            p,
            db,
            seen: RefCell::new(BTreeMap::new()),
        }
    }
    /// The store owner sees everything (§5.10).
    pub fn is_owner(&self) -> bool {
        self.p.is_store_owner()
    }
    /// `Authorized(p, l, b)` (§4.4). A logical the branch cannot name is
    /// hidden.
    pub fn sees(&self, l: LogicalId) -> bool {
        if self.is_owner() {
            return true;
        }
        if let Some(&s) = self.seen.borrow().get(&l) {
            return s;
        }
        let s = self.db.authorizes(self.p, l);
        self.seen.borrow_mut().insert(l, s);
        s
    }
    pub fn sees_key(&self, key: &str) -> bool {
        self.db.logical_by_key(key).is_some_and(|l| self.sees(l))
    }
    pub fn sees_rev(&self, r: RevisionId) -> bool {
        self.is_owner() || self.db.revision_logical(r).is_some_and(|l| self.sees(l))
    }
    /// A rule is shown whole only if its conclusion, premises and
    /// references are all visible.
    pub fn sees_rule(&self, j: RuleId) -> bool {
        if self.is_owner() {
            return true;
        }
        self.db.rule_parts(j).is_some_and(|(c, premises, refs)| {
            self.sees_rev(c)
                && premises.iter().all(|i| self.sees_rev(i.premise))
                && refs.iter().all(|r| self.sees_rev(*r))
        })
    }
    /// (visible, total) premises of rule `j`.
    fn premise_counts(&self, j: RuleId) -> (usize, usize) {
        match self.db.rule_parts(j) {
            Some((_, premises, _)) => (
                premises.iter().filter(|i| self.sees_rev(i.premise)).count(),
                premises.len(),
            ),
            None => (0, 0),
        }
    }

    // Values inside authorised items (§5.2).
    fn reason(&self, r: &mut StaleReason) {
        if let StaleReason::SupersededPinnedPremise(p) = r
            && !self.sees_rev(*p)
        {
            *r = StaleReason::SupersededPinnedPremiseRedacted;
        }
    }
    pub fn summary(&self, s: &mut Summary) {
        if let Summary::Stale(r) = s {
            self.reason(r);
        }
    }
    pub fn node(&self, n: &mut NodeState) {
        for w in n.witness.iter_mut().flatten() {
            if let Witness::Rule(j) = *w
                && !self.sees_rule(j)
            {
                *w = Witness::Redacted;
            }
        }
        self.reason(&mut n.dimensions.stale_reason);
    }
    pub fn item(&self, i: &mut Item) {
        self.node(&mut i.state);
        self.summary(&mut i.summary);
    }
    fn items(&self, items: &mut Vec<Item>) -> u32 {
        let n = drop_hidden(items, |i| self.sees(i.logical));
        for i in items.iter_mut() {
            self.item(i);
        }
        n
    }

    // Diff-shaped values (§5.5): hidden entries dropped and counted.
    fn result(&self, r: &mut CommitResult) -> u32 {
        let mut n = drop_hidden(&mut r.l0_flips, |x| self.sees_rev(*x));
        n += drop_hidden(&mut r.status_diff, |x| self.sees_rev(*x));
        let d = &mut r.net_delta;
        n += drop_hidden(&mut d.base_flips, |x| self.sees_rev(*x));
        n += drop_hidden(&mut d.head_moves, |(l, _, _)| self.sees(*l));
        n += drop_hidden(&mut d.enable_flips, |j| self.sees_rule(*j));
        n += drop_hidden(&mut d.new_rules, |j| self.sees_rule(*j));
        n
    }
    fn merge(&self, m: &mut MergeReport) -> u32 {
        let mut n = drop_hidden(&mut m.moved_heads, |(l, _, _)| self.sees(*l));
        n += drop_hidden(&mut m.conflicts, |c| {
            c.logical.is_none_or(|l| self.sees(l))
                && c.rule.is_none_or(|j| self.sees_rule(j))
                && self.sees_rev(c.target)
        });
        n += drop_hidden(&mut m.metadata_conflicts, |(l, _)| self.sees(*l));
        n
    }
    fn changes(&self, c: &mut Vec<NodeChange>) -> u32 {
        let n = drop_hidden(c, |x| self.sees_rev(x.revision));
        for x in c.iter_mut() {
            for s in [&mut x.before, &mut x.after].into_iter().flatten() {
                self.node(s);
            }
        }
        n
    }
    fn diffs(&self, d: &mut Vec<CertDiff>) -> u32 {
        let n = drop_hidden(d, |x| self.sees(x.logical));
        for x in d.iter_mut() {
            if let Some(s) = &mut x.summary {
                self.summary(s);
            }
        }
        n
    }
    /// A certificate keeps its id and the entries the principal may read;
    /// its vector is withheld and only its size reported (§5.6).
    fn cert(&self, c: &mut Certificate, red: &mut Redaction) -> u32 {
        red.vector_size.push((c.cert_id, c.vector.len() as u32));
        c.vector.clear();
        drop_hidden(&mut c.entries, |e| self.sees(e.logical))
    }
    fn validation(&self, v: &mut Validation, red: &mut Redaction) -> u32 {
        match v {
            Validation::Invalid { diffs, .. } => self.diffs(diffs),
            Validation::ValidRecertified { new_cert, diffs } => {
                self.cert(new_cert, red) + self.diffs(diffs)
            }
            Validation::Conflicted { entries } => drop_hidden(entries, |l| self.sees(*l)),
            Validation::Ambiguous { logical } => {
                if !self.sees(*logical) {
                    *logical = REDACTED_LOGICAL;
                }
                0
            }
            Validation::Pending { rules } => drop_hidden(rules, |j| self.sees_rule(*j)),
            Validation::Denied
            | Validation::Unknown { .. }
            | Validation::Empty
            | Validation::Valid => 0,
        }
    }

    // WHY, WHY_NOT and IMPACT (§5.4); the target itself is checked where
    // it is resolved.
    fn proof(&self, p: &mut Proof, red: &mut Redaction) {
        let e = &mut p.explanation;
        let (mut premises, mut rules) = (0u32, 0u32);
        let mut dropped = Vec::new();
        e.active_rules.retain(|j| {
            let (seen, total) = self.premise_counts(*j);
            let keep = seen > 0 || total == 0;
            if !keep {
                rules += 1;
                dropped.push(*j);
            }
            keep
        });
        e.references.retain(|(j, r)| {
            if dropped.contains(j) {
                return false;
            }
            let keep = self.sees_rev(*r);
            premises += u32::from(!keep);
            keep
        });
        if let Some(Witness::Rule(j)) = e.witness
            && !self.sees_rule(j)
        {
            e.witness = Some(Witness::Redacted);
        }
        premises += drop_hidden(&mut p.premises, |q| self.sees_rev(q.explanation.revision));
        if premises > 0 || rules > 0 {
            red.nodes.push(NodeRedaction {
                rev: p.explanation.revision,
                rule: None,
                premises,
                rules,
            });
        }
        for q in &mut p.premises {
            self.proof(q, red);
        }
    }
    fn why_not(&self, w: &mut WhyNot, red: &mut Redaction) {
        let mut rules = 0u32;
        w.rules.retain(|f| {
            let (seen, total) = self.premise_counts(f.rule);
            let keep = seen > 0 || total == 0;
            rules += u32::from(!keep);
            keep
        });
        for f in &mut w.rules {
            let mut n = drop_hidden(&mut f.asserted, |(l, _)| self.sees(*l));
            n += drop_hidden(&mut f.current, |(l, _)| self.sees(*l));
            n += drop_hidden(&mut f.incidences, |i| match i {
                IncidenceFailure::Unsupported(r) | IncidenceFailure::SemanticHeadUnsupported(r) => {
                    self.sees_rev(*r)
                }
                IncidenceFailure::NotCurrent { premise, head }
                | IncidenceFailure::SupersededPinned { premise, head } => {
                    self.sees_rev(*premise) && self.sees_rev(*head)
                }
            });
            // The recursion stops at redacted nodes; their failing
            // incidences are already counted.
            f.upstream.retain(|u| self.sees_rev(u.revision));
            if n > 0 {
                red.nodes.push(NodeRedaction {
                    rev: w.revision,
                    rule: Some(f.rule),
                    premises: n,
                    rules: 0,
                });
            }
            for u in &mut f.upstream {
                self.why_not(u, red);
            }
        }
        if rules > 0 {
            red.nodes.push(NodeRedaction {
                rev: w.revision,
                rule: None,
                premises: 0,
                rules,
            });
        }
    }
    fn impact(&self, i: &mut Impact, red: &mut Redaction) {
        red.redacted += drop_hidden(&mut i.supported, |r| self.sees_rev(*r));
        red.redacted_references += drop_hidden(&mut i.reference_only, |r| self.sees_rev(*r));
    }

    /// `redact(p, b, response)` (§5.1).
    pub fn redact(&self, response: Response, red: &mut Redaction) -> Response {
        if self.is_owner() {
            return response;
        }
        let mut response = response;
        let mut replace = None;
        let n = match &mut response {
            Response::Error { code, detail } => {
                if !PLAIN_ERRORS.contains(&code.as_str()) {
                    *detail = "details withheld (ADR 0017 §5.7)".into();
                }
                0
            }
            Response::Session { .. }
            | Response::Done
            | Response::Attached { .. }
            | Response::Branches { .. }
            | Response::Truncated { .. }
            | Response::HandoffBegun { .. }
            | Response::Handoffs { .. }
            | Response::Unavailable { .. } => 0,
            Response::Recall {
                items, withheld, ..
            } => {
                // Recall skips hidden matches before they take a slot
                // (§5.3); nothing is counted.
                self.items(items);
                self.items(withheld);
                0
            }
            Response::Read { item, .. } => {
                if self.sees(item.logical) {
                    self.item(item);
                } else {
                    // Resolved before redaction for every target; kept
                    // total all the same.
                    replace = Some(Response::Unavailable {
                        target: String::new(),
                    });
                }
                0
            }
            Response::Committed { result } | Response::ImpactExact { result } => {
                self.result(result)
            }
            Response::TxnStatus { committed } => {
                committed.as_mut().map_or(0, |(_, r)| self.result(r))
            }
            Response::WhatIf { result, merge } => {
                self.result(result) + merge.as_mut().map_or(0, |m| self.merge(m))
            }
            Response::Merged { result, report } => self.result(result) + self.merge(report),
            Response::Why { proof } => {
                self.proof(proof, red);
                0
            }
            Response::WhyNot { why_not } => {
                self.why_not(why_not, red);
                0
            }
            Response::Impact { impact } => {
                self.impact(impact, red);
                0
            }
            Response::Check {
                validation,
                cert,
                superseded,
                named,
                ..
            } => {
                let mut n = self.validation(validation, red);
                if let Some(c) = cert {
                    n += self.cert(c, red);
                }
                n += self.diffs(superseded);
                n += drop_hidden(named, |x| self.sees_rev(x.read));
                for x in named.iter_mut() {
                    self.summary(&mut x.summary);
                }
                n
            }
            Response::Refused { validation } | Response::HandoffAccepted { validation, .. } => {
                self.validation(validation, red)
            }
            Response::Diff { diff } => self.changes(&mut diff.changes),
            Response::Updates { items, .. } => {
                let n = drop_hidden(items, |x| self.sees(x.logical));
                for x in items.iter_mut() {
                    self.summary(&mut x.summary);
                }
                n
            }
            Response::Changes { items, .. } => {
                let n = drop_hidden(items, |x| self.sees(x.logical));
                for x in items.iter_mut() {
                    self.summary(&mut x.summary);
                }
                n
            }
            Response::Conflicts { items, .. } => {
                // Listed only when every side is authorised; not counted
                // (§5.8).
                items.retain(|c| {
                    c.sides.iter().all(|s| self.sees_rev(s.rev))
                        && c.rule.is_none_or(|j| self.sees_rule(j))
                });
                0
            }
            Response::AlreadyOpen { artifacts } => {
                artifacts.retain(|a| {
                    self.db
                        .artifact(*a)
                        .is_some_and(|x| self.sees_rev(x.target))
                });
                0
            }
            // Store-owner responses: unreachable through the allowlist
            // (§3.3), refused all the same.
            _ => {
                replace = Some(Response::Error {
                    code: "ChannelRestricted".into(),
                    detail: "this request is for the store owner only".into(),
                });
                0
            }
        };
        red.redacted += n;
        replace.unwrap_or(response)
    }
}

/// Keeps the entries `keep` accepts; returns how many were dropped.
fn drop_hidden<T>(v: &mut Vec<T>, mut keep: impl FnMut(&T) -> bool) -> u32 {
    let before = v.len();
    v.retain(|x| keep(x));
    (before - v.len()) as u32
}
