//! L1/L2 as maintained columns (SPEC §3.4 Phase 3b, ADR 0008 §7).
//!
//! Counting mode updates the columns incrementally inside the status walker:
//! incidences and rules are marked dirty by net-delta seeds and by upstream
//! node changes, and are re-evaluated when their conclusion is popped (rank
//! order, so premises and Sem heads are final). FullRecompute mode rebuilds
//! every column in topological order with direct literal evaluation.
use crate::{Indexes, InputView, PearceKelly, engine::CountingRow, engine::CountingState};
use contextdb_model::*;
use std::collections::BTreeMap;

pub(crate) fn vectors(i: &impl InputView, j: RuleId) -> (HeadVector, HeadVector) {
    let mut asserted: Vec<_> = i
        .premises(j)
        .filter(|p| p.kind == EdgeKind::Sem)
        .map(|p| (i.revision_logical(p.premise), p.premise))
        .collect();
    asserted.sort_unstable();
    let current = asserted.iter().map(|(l, _)| (*l, i.head(*l))).collect();
    (asserted, current)
}
pub(crate) fn semantic(i: &impl InputView, j: RuleId) -> SemState {
    let (asserted, current) = vectors(i, j);
    if asserted.is_empty() {
        SemState::NotRequired
    } else if asserted == current {
        SemState::Verified
    } else {
        match i.validation(j, &current).map(|r| r.verdict) {
            Some(Verdict::Verified) => SemState::Verified,
            Some(Verdict::Rejected) => SemState::Rejected,
            _ => SemState::Pending,
        }
    }
}
/// Gate of layer `level` (0 = L1, 1 = L2) for a rule with semantic state `sem`.
fn gate(sem: SemState, level: usize) -> bool {
    if level == 0 {
        sem != SemState::Rejected
    } else {
        matches!(sem, SemState::Verified | SemState::NotRequired)
    }
}

/// Maintained L1/L2 state. Indexed like the counting state: per incidence
/// (`lit`), per rule (`unsat`, `active`, `sem`) and per revision (the rest).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LayerColumns {
    /// Literal truth per incidence: bit 0 = L1, bit 1 = L2.
    lit: Vec<u8>,
    unsat: Vec<[u32; 2]>,
    active: Vec<[bool; 2]>,
    sem: Vec<SemState>,
    truth: Vec<[bool; 2]>,
    witness: Vec<[Option<Witness>; 2]>,
    reason: Vec<StaleReason>,
    /// SD11 `Conflicted(r)` as last evaluated; gates L2.
    conflicted: Vec<bool>,
    dirty_inc: Vec<bool>,
    dirty_rule: Vec<bool>,
    pending_inc: BTreeMap<RevisionId, Vec<IncId>>,
    pending_rule: BTreeMap<RevisionId, Vec<RuleId>>,
}
/// Pre-values of one popped revision's footprint in the layer columns: its
/// node columns, the rules concluding it and their literals (`refresh`
/// writes nothing else in pre-existing rows).
#[derive(Clone, Debug)]
pub(crate) struct LayerRows {
    r: RevisionId,
    node: ([bool; 2], [Option<Witness>; 2], StaleReason, bool),
    rules: Vec<(RuleId, [u32; 2], [bool; 2], SemState)>,
    lits: Vec<(usize, u8)>,
}
impl LayerColumns {
    /// Revision, rule and incidence counts.
    pub(crate) fn lens(&self) -> [usize; 3] {
        [self.truth.len(), self.unsat.len(), self.lit.len()]
    }
    /// `r`'s footprint, limited to rows below `lens`.
    pub(crate) fn footprint(
        &self,
        index: &Indexes,
        r: RevisionId,
        lens: [usize; 3],
    ) -> Option<LayerRows> {
        let n = r.index();
        if n >= lens[0] {
            return None;
        }
        let mut rules = Vec::new();
        let mut lits = Vec::new();
        for j in index.rules_by_concl.row(n) {
            if j.index() < lens[1] {
                let k = j.index();
                rules.push((*j, self.unsat[k], self.active[k], self.sem[k]));
                lits.extend(index.rule_incidences(*j).map(|id| (id, self.lit[id])));
            }
        }
        Some(LayerRows {
            r,
            node: (
                self.truth[n],
                self.witness[n],
                self.reason[n].clone(),
                self.conflicted[n],
            ),
            rules,
            lits,
        })
    }
    /// Restores saved footprints, then drops rows at or above `lens`.
    pub(crate) fn restore(&mut self, saved: Vec<LayerRows>, lens: [usize; 3]) {
        for LayerRows {
            r,
            node,
            rules,
            lits,
        } in saved
        {
            let n = r.index();
            (
                self.truth[n],
                self.witness[n],
                self.reason[n],
                self.conflicted[n],
            ) = node;
            for (j, unsat, active, sem) in rules {
                let k = j.index();
                (self.unsat[k], self.active[k], self.sem[k]) = (unsat, active, sem);
            }
            for (id, lit) in lits {
                self.lit[id] = lit;
            }
        }
        let [revisions, rules, incs] = lens;
        self.truth.truncate(revisions);
        self.witness.truncate(revisions);
        self.reason.truncate(revisions);
        self.conflicted.truncate(revisions);
        self.unsat.truncate(rules);
        self.active.truncate(rules);
        self.sem.truncate(rules);
        self.dirty_rule.truncate(rules);
        self.lit.truncate(incs);
        self.dirty_inc.truncate(incs);
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        use crate::memory::{bool_bytes, vec_bytes as b};
        b(&self.lit)
            + b(&self.unsat)
            + b(&self.active)
            + b(&self.sem)
            + b(&self.truth)
            + b(&self.witness)
            + b(&self.reason)
            + bool_bytes(&self.conflicted)
            + bool_bytes(&self.dirty_inc)
            + bool_bytes(&self.dirty_rule)
    }
    /// Literal of incidence `p` at `level` from the current columns.
    fn literal(&self, i: &impl InputView, p: Incidence, level: usize) -> bool {
        let logical = i.revision_logical(p.premise);
        let head = i.head(logical);
        self.truth[p.premise.index()][level]
            && (p.kind != EdgeKind::Cur || head == p.premise)
            && (p.kind != EdgeKind::Pin
                || head == p.premise
                || i.logical_kind(logical) == RecordKind::Evidence)
            && (p.kind != EdgeKind::Sem || self.truth[head.index()][level])
    }
    /// Extends every column for appended revisions, rules and incidences.
    /// New rules start with every literal false (unsat = premise count).
    pub(crate) fn grow(&mut self, i: &impl InputView, index: &Indexes) {
        use crate::memory::resize;
        let (n, rules, incs) = (i.revision_count(), i.rule_count(), index.inc_rule.len());
        let old_rules = self.unsat.len();
        resize(&mut self.lit, incs, 0);
        resize(&mut self.dirty_inc, incs, false);
        resize(&mut self.active, rules, [false; 2]);
        resize(&mut self.sem, rules, SemState::NotRequired);
        resize(&mut self.dirty_rule, rules, false);
        resize(&mut self.unsat, rules, [0; 2]);
        for j in old_rules..rules {
            let k = index.rule_incidences(RuleId(j as u32)).len() as u32;
            self.unsat[j] = [k; 2];
        }
        resize(&mut self.truth, n, [false; 2]);
        resize(&mut self.witness, n, [None; 2]);
        resize(&mut self.reason, n, StaleReason::Unsupported);
        resize(&mut self.conflicted, n, false);
    }
    /// Marks an incidence for re-evaluation; returns its conclusion when newly marked.
    fn mark_incidence(
        &mut self,
        i: &impl InputView,
        index: &Indexes,
        id: IncId,
    ) -> Option<RevisionId> {
        if self.dirty_inc[id.index()] {
            return None;
        }
        self.dirty_inc[id.index()] = true;
        let c = i.conclusion(index.inc_rule[id.index()]);
        self.pending_inc.entry(c).or_default().push(id);
        Some(c)
    }
    fn mark_rule(&mut self, i: &impl InputView, j: RuleId) -> Option<RevisionId> {
        if self.dirty_rule[j.index()] {
            return None;
        }
        self.dirty_rule[j.index()] = true;
        let c = i.conclusion(j);
        self.pending_rule.entry(c).or_default().push(j);
        Some(c)
    }
    /// Phase 3b seeds (SPEC §3.4). Returns the conclusions to push.
    pub(crate) fn seed(
        &mut self,
        i: &impl InputView,
        index: &Indexes,
        delta: &NetDelta,
        revalidated: &[RuleId],
    ) -> Vec<RevisionId> {
        let mut out = Vec::new();
        for (l, old, new) in &delta.head_moves {
            for r in old.iter().chain(std::iter::once(new)) {
                for id in index
                    .exact_by_rev
                    .row(r.index())
                    .iter()
                    .chain(index.pin_by_rev.row(r.index()))
                {
                    out.extend(self.mark_incidence(i, index, *id));
                }
            }
            for id in index.sem_by_logical.row(l.index()) {
                out.extend(self.mark_incidence(i, index, *id));
                out.extend(self.mark_rule(i, index.inc_rule[id.index()]));
            }
        }
        for j in delta.enable_flips.iter().chain(revalidated) {
            out.extend(self.mark_rule(i, *j));
        }
        for j in &delta.new_rules {
            out.extend(self.mark_rule(i, *j));
            for id in index.rule_incidences(*j) {
                out.extend(self.mark_incidence(i, index, IncId(id as u32)));
            }
        }
        out
    }
    /// Re-evaluates the dirty incidences and rules concluding `r`, then `r`'s
    /// node columns. Premises and Sem heads rank below `r`, so they are final.
    pub(crate) fn refresh(
        &mut self,
        i: &impl InputView,
        c: &CountingState,
        index: &Indexes,
        r: RevisionId,
    ) {
        for id in self.pending_inc.remove(&r).unwrap_or_default() {
            self.dirty_inc[id.index()] = false;
            let j = index.inc_rule[id.index()];
            let p = index.incidence(i, id.index());
            let old = self.lit[id.index()];
            let mut now = 0;
            for level in 0..2 {
                if self.literal(i, p, level) {
                    now |= 1 << level;
                }
            }
            if now != old {
                self.lit[id.index()] = now;
                for level in 0..2 {
                    let (was, is) = (old >> level & 1 == 1, now >> level & 1 == 1);
                    if was != is {
                        let u = &mut self.unsat[j.index()][level];
                        *u = if is {
                            u.checked_sub(1).expect("layer unsat underflow")
                        } else {
                            u.checked_add(1).expect("layer unsat overflow")
                        };
                    }
                }
                self.mark_rule(i, j);
            }
        }
        for j in self.pending_rule.remove(&r).unwrap_or_default() {
            self.dirty_rule[j.index()] = false;
            let sem = semantic(i, j);
            self.sem[j.index()] = sem;
            for level in 0..2 {
                self.active[j.index()][level] =
                    i.enabled(j) && gate(sem, level) && self.unsat[j.index()][level] == 0;
            }
        }
        self.node(i, c, index, r);
    }
    /// Node columns of `r` from its rules' columns.
    fn node(&mut self, i: &impl InputView, c: &CountingState, index: &Indexes, r: RevisionId) {
        let n = r.index();
        let base = i.base(r);
        let conflicted = index.conflicted(r);
        self.conflicted[n] = conflicted;
        for level in 0..2 {
            let first = index
                .rules_by_concl
                .row(n)
                .iter()
                .find(|j| self.active[j.index()][level])
                .copied();
            // L2 = ¬Conflicted(r) ∧ (base ∨ an active rule) (SD11).
            let gated = level == 1 && conflicted;
            self.truth[n][level] = !gated && (base || first.is_some());
            self.witness[n][level] = if gated {
                None
            } else if base {
                Some(Witness::Base)
            } else {
                first.map(Witness::Rule)
            };
        }
        self.reason[n] = self.stale_reason(i, c, index, r);
    }
    /// SD13 STALE reason rule; inherited reasons are read from final premises.
    fn stale_reason(
        &self,
        i: &impl InputView,
        c: &CountingState,
        index: &Indexes,
        r: RevisionId,
    ) -> StaleReason {
        if i.head(i.revision_logical(r)) != r {
            return StaleReason::HeadSuperseded;
        }
        let (true, Some(Witness::Rule(j))) = (c.s[r.index()], c.witness[r.index()]) else {
            return StaleReason::Unsupported;
        };
        if self.sem[j.index()] == SemState::Rejected {
            return StaleReason::Rejected;
        }
        let failing = index
            .rule_incidences(j)
            .find(|id| self.lit[*id] & 1 == 0)
            .map(|id| index.incidence(i, id));
        let Some(p) = failing else {
            return StaleReason::Unsupported;
        };
        let logical = i.revision_logical(p.premise);
        let head = i.head(logical);
        if p.kind == EdgeKind::Pin
            && head != p.premise
            && i.logical_kind(logical) != RecordKind::Evidence
        {
            StaleReason::SupersededPinnedPremise(p.premise)
        } else if p.kind == EdgeKind::Sem && head != p.premise && !self.truth[head.index()][0] {
            self.reason[head.index()].clone()
        } else {
            self.reason[p.premise.index()].clone()
        }
    }
    /// After `r` is popped: marks downstream incidences when `r`'s truth or
    /// stale reason changed (reason-only changes still propagate, ADR 0008 §7).
    /// Returns the conclusions to push.
    pub(crate) fn propagate(
        &mut self,
        i: &impl InputView,
        index: &Indexes,
        r: RevisionId,
    ) -> Vec<RevisionId> {
        let mut out = Vec::new();
        for id in index.rules_by_premise.row(r.index()) {
            out.extend(self.mark_incidence(i, index, *id));
        }
        let logical = i.revision_logical(r);
        if i.head(logical) == r {
            for id in index.sem_by_logical.row(logical.index()) {
                out.extend(self.mark_incidence(i, index, *id));
            }
        }
        out
    }
    /// The node's full published state.
    pub(crate) fn node_state(
        &self,
        i: &impl InputView,
        r: RevisionId,
        c: CountingRow,
    ) -> NodeState {
        let n = r.index();
        let witness = [c.witness, self.witness[n][0], self.witness[n][1]];
        let sem = if !c.s {
            SemState::NotApplicable
        } else {
            match witness.iter().rev().flatten().next() {
                Some(Witness::Base) => SemState::NotRequired,
                Some(Witness::Rule(j)) => self.sem[j.index()],
                None => SemState::NotApplicable,
            }
        };
        let [l1, l2] = self.truth[n];
        NodeState {
            l0: c.s,
            l1,
            l2,
            support: c.support,
            witness,
            dimensions: Dimensions {
                authorized: true,
                conflicted: self.conflicted[n],
                currency: if i.head(i.revision_logical(r)) == r {
                    Currency::Current
                } else {
                    Currency::Superseded
                },
                structural: if c.s {
                    Structural::Supported
                } else {
                    Structural::Unsupported
                },
                l1,
                l2,
                semantic: sem,
                stale_reason: self.reason[n].clone(),
                observation: Observation::NotApplicable,
            },
        }
    }
    pub(crate) fn sem(&self, j: RuleId) -> SemState {
        self.sem[j.index()]
    }
    pub(crate) fn layer_active(&self, j: RuleId) -> [bool; 2] {
        self.active[j.index()]
    }
    /// Recomputes every column from scratch in topological order with direct
    /// literal evaluation (FullRecompute mode; snapshot rebuilds).
    pub(crate) fn rebuild(
        i: &impl InputView,
        c: &CountingState,
        index: &Indexes,
        topo: &PearceKelly,
    ) -> Self {
        let mut out = Self::default();
        out.grow(i, index);
        for node in topo.order() {
            let CycleNode::Revision(r) = node else {
                continue;
            };
            for j in index.rules_by_concl.row(r.index()) {
                let sem = semantic(i, *j);
                out.sem[j.index()] = sem;
                let mut unsat = [0u32; 2];
                for id in index.rule_incidences(*j) {
                    let p = index.incidence(i, id);
                    let mut bits = 0;
                    for (level, u) in unsat.iter_mut().enumerate() {
                        if out.literal(i, p, level) {
                            bits |= 1 << level;
                        } else {
                            *u += 1;
                        }
                    }
                    out.lit[id] = bits;
                }
                out.unsat[j.index()] = unsat;
                for (level, u) in unsat.iter().enumerate() {
                    out.active[j.index()][level] = i.enabled(*j) && gate(sem, level) && *u == 0;
                }
            }
            out.node(i, c, index, *r);
        }
        out
    }
    /// Debug/paranoid: the incremental columns equal a full rebuild (DA5 for L1/L2).
    #[cfg(any(debug_assertions, feature = "paranoid", test))]
    pub(crate) fn assert_matches_rebuild(
        &self,
        i: &impl InputView,
        c: &CountingState,
        index: &Indexes,
        topo: &PearceKelly,
    ) {
        let full = Self::rebuild(i, c, index, topo);
        assert!(self.pending_inc.is_empty() && self.pending_rule.is_empty());
        assert_eq!(
            (&self.lit, &self.unsat, &self.active, &self.sem),
            (&full.lit, &full.unsat, &full.active, &full.sem),
            "L1/L2 rule columns"
        );
        assert_eq!(
            (&self.truth, &self.witness, &self.reason),
            (&full.truth, &full.witness, &full.reason),
            "L1/L2 node columns"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, Maintainer};
    #[test]
    #[should_panic(expected = "L1/L2")]
    fn rebuild_cross_check_detects_a_corrupted_column() {
        let mut db = Db::default();
        db.commit(&Batch::new(vec![Op::CreateLogical {
            key: "x".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: [1; 32],
            adopt_as_base: true,
        }]))
        .unwrap();
        db.layers.truth[0] = [false; 2];
        db.layers
            .assert_matches_rebuild(&db.view(), db.maintainer.state(), &db.indexes, &db.pk);
    }
}
