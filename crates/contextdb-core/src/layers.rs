use crate::{Indexes, InputView, engine::CountingState};
use contextdb_model::*;

#[derive(Clone, Debug)]
struct Entry {
    epoch: Option<Epoch>,
    truth: [bool; 2],
    witness: [Option<Witness>; 2],
    reason: StaleReason,
}
impl Default for Entry {
    fn default() -> Self {
        Self {
            epoch: None,
            truth: [false; 2],
            witness: [None; 2],
            reason: StaleReason::Unsupported,
        }
    }
}
/// Query-time memoization. Commits invalidate the status dependency closure;
/// untouched entries remain reusable across epochs without whole-vector clears.
#[derive(Clone, Debug, Default)]
pub(crate) struct LayerMemo {
    entries: Vec<Entry>,
}

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
impl LayerMemo {
    pub(crate) fn retained_bytes(&self) -> usize {
        crate::memory::vec_bytes(&self.entries)
    }
    pub fn invalidate(&mut self, affected: &[RevisionId]) {
        for r in affected {
            if let Some(entry) = self.entries.get_mut(r.index()) {
                entry.epoch = None;
            }
        }
    }
    fn literal(&self, i: &impl InputView, p: Incidence, level: usize) -> bool {
        let logical = i.revision_logical(p.premise);
        let head = i.head(logical);
        self.entries[p.premise.index()].truth[level]
            && (p.kind != EdgeKind::Cur || head == p.premise)
            && (p.kind != EdgeKind::Pin
                || head == p.premise
                || i.logical_kind(logical) == RecordKind::Evidence)
            && (p.kind != EdgeKind::Sem || self.entries[head.index()].truth[level])
    }
    fn ensure(
        &mut self,
        i: &impl InputView,
        c: &CountingState,
        index: &Indexes,
        r: RevisionId,
        epoch: Epoch,
    ) {
        self.entries.resize_with(i.revision_count(), Entry::default);
        let mut stack = vec![(r, false)];
        while let Some((r, expanded)) = stack.pop() {
            if self.entries[r.index()].epoch.is_some() {
                continue;
            }
            if !expanded && !i.base(r) {
                stack.push((r, true));
                for j in &index.rules_by_concl[r.index()] {
                    if !i.enabled(*j) {
                        continue;
                    }
                    for p in i.premises(*j) {
                        if self.entries[p.premise.index()].epoch.is_none() {
                            stack.push((p.premise, false));
                        }
                        if p.kind == EdgeKind::Sem {
                            let head = i.head(i.revision_logical(p.premise));
                            if self.entries[head.index()].epoch.is_none() {
                                stack.push((head, false));
                            }
                        }
                    }
                }
                continue;
            }
            let mut truth = [i.base(r); 2];
            let mut witness = if i.base(r) {
                [Some(Witness::Base); 2]
            } else {
                [None; 2]
            };
            if !i.base(r) {
                for j in &index.rules_by_concl[r.index()] {
                    if !i.enabled(*j) {
                        continue;
                    }
                    let sem = semantic(i, *j);
                    for level in 0..2 {
                        let gate = if level == 0 {
                            sem != SemState::Rejected
                        } else {
                            matches!(sem, SemState::Verified | SemState::NotRequired)
                        };
                        if gate && i.premises(*j).all(|p| self.literal(i, p, level)) {
                            truth[level] = true;
                            if witness[level].is_none() {
                                witness[level] = Some(Witness::Rule(*j));
                            }
                        }
                    }
                }
            }
            let mut reason = StaleReason::Unsupported;
            if i.head(i.revision_logical(r)) != r {
                reason = StaleReason::HeadSuperseded;
            } else if c.s[r.index()]
                && let Some(Witness::Rule(j)) = c.witness[r.index()]
            {
                if semantic(i, j) == SemState::Rejected {
                    reason = StaleReason::Rejected;
                } else if let Some(p) = i.premises(j).find(|p| !self.literal(i, *p, 0)) {
                    let logical = i.revision_logical(p.premise);
                    let head = i.head(logical);
                    reason = if p.kind == EdgeKind::Pin
                        && head != p.premise
                        && i.logical_kind(logical) != RecordKind::Evidence
                    {
                        StaleReason::SupersededPinnedPremise(p.premise)
                    } else if p.kind == EdgeKind::Sem
                        && head != p.premise
                        && !self.entries[head.index()].truth[0]
                    {
                        self.entries[head.index()].reason.clone()
                    } else {
                        self.entries[p.premise.index()].reason.clone()
                    };
                }
            }
            self.entries[r.index()] = Entry {
                epoch: Some(epoch),
                truth,
                witness,
                reason,
            };
        }
    }
    pub fn node(
        &mut self,
        i: &impl InputView,
        c: &CountingState,
        index: &Indexes,
        r: RevisionId,
        epoch: Epoch,
    ) -> Result<NodeState, TxError> {
        if r.index() >= i.revision_count() {
            return Err(TxError::UnknownRef);
        }
        self.ensure(i, c, index, r, epoch);
        Ok(self.cached_node(i, r, c.row(r)))
    }
    /// Used by the transaction diff walker before invalidating this node. All
    /// entries are valid at publication; DAG order preserves old entries until pop.
    pub fn cached_node(
        &self,
        i: &impl InputView,
        r: RevisionId,
        c: crate::engine::CountingRow,
    ) -> NodeState {
        let entry = &self.entries[r.index()];
        crate::invariant_assert!(entry.epoch.is_some());

        let witness = [c.witness, entry.witness[0], entry.witness[1]];
        let sem = if !c.s {
            SemState::NotApplicable
        } else {
            match witness.iter().rev().flatten().next() {
                Some(Witness::Base) => SemState::NotRequired,
                Some(Witness::Rule(j)) => semantic(i, *j),
                None => SemState::NotApplicable,
            }
        };
        NodeState {
            l0: c.s,
            l1: entry.truth[0],
            l2: entry.truth[1],
            support: c.support,
            witness,
            dimensions: Dimensions {
                authorized: true,
                conflicted: false,
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
                l1: entry.truth[0],
                l2: entry.truth[1],
                semantic: sem,
                stale_reason: entry.reason.clone(),
                observation: Observation::NotApplicable,
            },
        }
    }
    pub fn refresh_rule(
        &mut self,
        i: &impl InputView,
        c: &CountingState,
        index: &Indexes,
        j: RuleId,
        epoch: Epoch,
        d: &mut DerivedSnapshot,
    ) {
        let n = j.index();
        d.sat[n].copy_from_slice(&c.sat[index.rule_incidences(j)]);
        d.unsat[n] = c.unsat[n];
        d.active[n] = c.counted[n];
        let sem = semantic(i, j);
        d.sem[n] = sem;
        for p in i.premises(j) {
            self.ensure(i, c, index, p.premise, epoch);
            if p.kind == EdgeKind::Sem {
                self.ensure(i, c, index, i.head(i.revision_logical(p.premise)), epoch);
            }
        }
        for level in 0..2 {
            let gate = if level == 0 {
                sem != SemState::Rejected
            } else {
                matches!(sem, SemState::Verified | SemState::NotRequired)
            };
            d.layer_active[n][level] =
                i.enabled(j) && gate && i.premises(j).all(|p| self.literal(i, p, level));
        }
    }
    pub fn snapshot(
        &mut self,
        i: &impl InputView,
        c: &CountingState,
        index: &Indexes,
        epoch: Epoch,
    ) -> DerivedSnapshot {
        let nodes = (0..i.revision_count())
            .map(|n| {
                self.node(i, c, index, RevisionId(n as u32), epoch)
                    .expect("valid revision")
            })
            .collect();
        let sem: Vec<_> = (0..i.rule_count())
            .map(|n| semantic(i, RuleId(n as u32)))
            .collect();
        let mut layer_active = Vec::new();
        for (n, rule_sem) in sem.iter().enumerate() {
            let j = RuleId(n as u32);
            let mut active = [false; 2];
            if i.enabled(j) {
                for p in i.premises(j) {
                    self.ensure(i, c, index, p.premise, epoch);
                    if p.kind == EdgeKind::Sem {
                        self.ensure(i, c, index, i.head(i.revision_logical(p.premise)), epoch);
                    }
                }
                for (level, result) in active.iter_mut().enumerate() {
                    let gate = if level == 0 {
                        *rule_sem != SemState::Rejected
                    } else {
                        matches!(rule_sem, SemState::Verified | SemState::NotRequired)
                    };
                    *result = gate && i.premises(j).all(|p| self.literal(i, p, level));
                }
            }
            layer_active.push(active);
        }
        DerivedSnapshot {
            nodes,
            sem,
            layer_active,
            sat: index
                .rule_start
                .windows(2)
                .map(|w| c.sat[w[0] as usize..w[1] as usize].to_vec())
                .collect(),
            unsat: c.unsat.clone(),
            active: c.counted.clone(),
        }
    }
}
