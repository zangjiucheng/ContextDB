use super::{CountingRow, CountingState, Maintainer, Maintenance};
use crate::InputView;
use crate::pending::{Cursor, Pending};
use crate::{Indexes, PearceKelly, StatsSink, stats::Work};
use contextdb_model::*;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap},
};

/// Incremental set counting. Pending incidence/rule bits prevent duplicate work.
#[derive(Clone, Debug, Default)]
pub struct Counting {
    state: CountingState,
    dirty_inc: Vec<bool>,
    dirty_rule: Vec<bool>,
    queued: Vec<bool>,
    pending_inc: Pending<IncId>,
    pending_rule: Pending<RuleId>,
    journal: Option<Journal>,
}
/// Pre-values of everything one `update` wrote. A popped revision's
/// footprint is its row, the rules concluding it and their incidences; the
/// pass writes nothing else in pre-existing rows.
#[derive(Clone, Debug)]
struct Journal {
    /// Revision, rule and incidence counts at `begin_journal`.
    lens: [usize; 3],
    rows: Vec<Footprint>,
}
/// One popped revision's row, its rules' `(unsat, counted)` and their
/// incidences' `sat`.
type Footprint = (
    RevisionId,
    CountingRow,
    Vec<(RuleId, u32, bool)>,
    Vec<(usize, bool)>,
);
impl Counting {
    fn save(&mut self, r: RevisionId, index: &Indexes) {
        let Some(journal) = &mut self.journal else {
            return;
        };
        let [revisions, rules, _] = journal.lens;
        if r.index() >= revisions {
            return;
        }
        let state = &self.state;
        let mut by_rule = Vec::new();
        let mut by_inc = Vec::new();
        for j in index.rules_by_concl.row(r.index()) {
            if j.index() < rules {
                by_rule.push((*j, state.unsat[j.index()], state.counted[j.index()]));
                by_inc.extend(index.rule_incidences(*j).map(|n| (n, state.sat[n])));
            }
        }
        journal.rows.push((r, state.row(r), by_rule, by_inc));
    }
    fn queue<S: StatsSink>(
        &mut self,
        r: RevisionId,
        topo: &PearceKelly,
        heap: &mut BinaryHeap<Reverse<(usize, RevisionId)>>,
        stats: &mut S,
        seed: bool,
        units: &mut u64,
    ) {
        if !self.queued[r.index()] {
            self.queued[r.index()] = true;
            heap.push(Reverse((topo.rank(&CycleNode::Revision(r)), r)));
            *units += 1;
            stats.record(Work::Push);
            if seed {
                stats.record(Work::Seed(r));
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn incidence<S: StatsSink, V: InputView>(
        &mut self,
        id: IncId,
        index: &Indexes,
        i: &V,
        topo: &PearceKelly,
        heap: &mut BinaryHeap<Reverse<(usize, RevisionId)>>,
        stats: &mut S,
        seed: bool,
        units: &mut u64,
    ) {
        if !self.dirty_inc[id.index()] {
            self.dirty_inc[id.index()] = true;
            *units += 1;
            let j = index.inc_rule[id.index()];
            let c = i.conclusion(j);
            self.pending_inc.push(c, id);
            self.queue(c, topo, heap, stats, seed, units);
        }
    }
    fn rule<V: InputView>(&mut self, j: RuleId, i: &V, units: &mut u64) {
        if !self.dirty_rule[j.index()] {
            self.dirty_rule[j.index()] = true;
            *units += 1;
            self.pending_rule.push(i.conclusion(j), j);
        }
    }
}

/// One counting pass between steps (ADR 0019 §3.3): the heap and what
/// `update` keeps beside it. The pending lists and dirty bits stay in
/// [`Counting`] itself.
#[derive(Clone, Debug)]
pub(crate) struct Pass {
    pub(crate) heap: BinaryHeap<Reverse<(usize, RevisionId)>>,
    previous: Option<usize>,
    popped: Vec<RevisionId>,
    flips: Vec<RevisionId>,
    /// Rows at their first pop, for pre-existing revisions.
    pub(crate) before: BTreeMap<RevisionId, CountingRow>,
    old_len: usize,
    #[cfg(any(debug_assertions, feature = "paranoid"))]
    old: CountingState,
}
impl Pass {
    /// Rank of the next node to pop.
    pub(crate) fn top(&self) -> Option<usize> {
        self.heap.peek().map(|Reverse((rank, _))| *rank)
    }
}
/// The resumable steps of [`Counting::update`] (ADR 0019 §3.1–§3.2). Every
/// step adds its work units to `units`: one per heap pop or push, per
/// fan-out entry examined, per pending mark set while draining and per
/// pending item drained.
impl Counting {
    /// Grows the columns and installs every seed (DA1).
    pub(crate) fn begin<S: StatsSink, V: InputView>(
        &mut self,
        i: &V,
        index: &Indexes,
        topo: &PearceKelly,
        delta: &NetDelta,
        stats: &mut S,
        units: &mut u64,
    ) -> Pass {
        #[cfg(any(debug_assertions, feature = "paranoid"))]
        let old = self.state.clone();
        let old_len = self.state.s.len();
        let n = i.revision_count();
        use crate::memory::resize;
        resize(&mut self.state.s, n, false);
        resize(&mut self.state.support, n, 0);
        resize(&mut self.state.witness, n, None);
        let old_rules = self.state.unsat.len();
        resize(&mut self.state.unsat, i.rule_count(), 0);
        resize(&mut self.state.counted, i.rule_count(), false);
        resize(&mut self.state.sat, index.inc_rule.len(), false);
        resize(&mut self.dirty_inc, index.inc_rule.len(), false);
        resize(&mut self.dirty_rule, i.rule_count(), false);
        resize(&mut self.queued, n, false);
        let mut heap = BinaryHeap::new();
        // DA1: no pop occurs until all final net-delta seeds have been installed.
        for r in &delta.base_flips {
            self.queue(*r, topo, &mut heap, stats, true, units);
        }
        for (_, old, new) in &delta.head_moves {
            for r in old.iter().chain(std::iter::once(new)) {
                for id in index.exact_by_rev.row(r.index()) {
                    self.incidence(*id, index, i, topo, &mut heap, stats, true, units);
                }
            }
        }
        for j in &delta.enable_flips {
            self.rule(*j, i, units);
            self.queue(i.conclusion(*j), topo, &mut heap, stats, true, units);
        }
        for j in &delta.new_rules {
            crate::invariant_assert!(j.index() >= old_rules);
            self.state.unsat[j.index()] =
                u32::try_from(index.rule_incidences(*j).len()).expect("canonical premise bound");
            self.rule(*j, i, units);
            if index.rule_incidences(*j).is_empty() {
                // A reference-only rule has no incidence to queue its conclusion.
                self.queue(i.conclusion(*j), topo, &mut heap, stats, true, units);
            }
            for id in index.rule_incidences(*j) {
                self.incidence(
                    IncId(id as u32),
                    index,
                    i,
                    topo,
                    &mut heap,
                    stats,
                    true,
                    units,
                );
            }
        }
        Pass {
            heap,
            previous: None,
            popped: Vec::new(),
            flips: Vec::new(),
            before: BTreeMap::new(),
            old_len,
            #[cfg(any(debug_assertions, feature = "paranoid"))]
            old,
        }
    }
    /// Pops the next node: its pre-row is saved and its pending lists are
    /// ready to drain.
    pub(crate) fn pop<S: StatsSink, V: InputView>(
        &mut self,
        pass: &mut Pass,
        i: &V,
        index: &Indexes,
        topo: &PearceKelly,
        stats: &mut S,
        units: &mut u64,
    ) -> Option<RevisionId> {
        let Reverse((rank, r)) = pass.heap.pop()?;
        *units += 1;
        // DA2: strict rank order implies each revision is popped at most once.
        crate::invariant_assert!(pass.previous.is_none_or(|old| old < rank));
        pass.previous = Some(rank);
        stats.record(Work::Pop(r));
        pass.popped.push(r);
        if r.index() < pass.old_len {
            pass.before.insert(r, self.state.row(r));
        }
        self.save(r, index);
        // DA3 / DA6: all supporting premises precede their conclusion.
        crate::invariant_assert!(index.rules_by_concl.row(r.index()).iter().all(|j| {
            i.premises(*j)
                .all(|p| topo.rank(&CycleNode::Revision(p.premise)) < rank)
        }));
        Some(r)
    }
    pub(crate) fn take_pending_inc(&mut self, r: RevisionId) -> Cursor {
        self.pending_inc.take(r)
    }
    pub(crate) fn take_pending_rule(&mut self, r: RevisionId) -> Cursor {
        self.pending_rule.take(r)
    }
    pub(crate) fn next_pending_inc(&self, at: &mut Cursor) -> Option<IncId> {
        self.pending_inc.next(at)
    }
    pub(crate) fn next_pending_rule(&self, at: &mut Cursor) -> Option<RuleId> {
        self.pending_rule.next(at)
    }
    /// Re-evaluates one pending incidence of the popped node.
    pub(crate) fn drain_incidence<S: StatsSink, V: InputView>(
        &mut self,
        id: IncId,
        i: &V,
        index: &Indexes,
        stats: &mut S,
        units: &mut u64,
    ) {
        *units += 1;
        self.dirty_inc[id.index()] = false;
        stats.record(Work::Incidence);
        let j = index.inc_rule[id.index()];
        let Incidence { premise: p, kind } = index.incidence(i, id.index());
        let now = self.state.s[p.index()]
            && (kind != EdgeKind::Cur || i.head(i.revision_logical(p)) == p);
        let stored = self.state.sat[id.index()];
        // DA4: adjust counters only on a stored-bit transition.
        if now != stored {
            self.state.sat[id.index()] = now;
            self.state.unsat[j.index()] = if now {
                self.state.unsat[j.index()]
                    .checked_sub(1)
                    .expect("unsat underflow")
            } else {
                self.state.unsat[j.index()]
                    .checked_add(1)
                    .expect("unsat overflow")
            };
            self.rule(j, i, units);
        }
    }
    /// Re-evaluates one pending rule concluding the popped node `r`.
    pub(crate) fn drain_rule<S: StatsSink, V: InputView>(
        &mut self,
        r: RevisionId,
        j: RuleId,
        i: &V,
        stats: &mut S,
        units: &mut u64,
    ) {
        *units += 1;
        self.dirty_rule[j.index()] = false;
        stats.record(Work::Rule);
        let now = i.fires(j) && self.state.unsat[j.index()] == 0;
        if now != self.state.counted[j.index()] {
            self.state.counted[j.index()] = now;
            self.state.support[r.index()] = if now {
                self.state.support[r.index()]
                    .checked_add(1)
                    .expect("support overflow")
            } else {
                self.state.support[r.index()]
                    .checked_sub(1)
                    .expect("support underflow")
            };
            stats.record(Work::RuleFlip);
        }
    }
    /// The popped node's witness and S once its pending lists are drained;
    /// true when S flipped, so its fan-out must be marked.
    pub(crate) fn conclude<S: StatsSink, V: InputView>(
        &mut self,
        r: RevisionId,
        pass: &mut Pass,
        i: &V,
        index: &Indexes,
        stats: &mut S,
    ) -> bool {
        self.state.witness[r.index()] = if i.base(r) {
            Some(Witness::Base)
        } else {
            index
                .rules_by_concl
                .row(r.index())
                .iter()
                .find(|j| self.state.counted[j.index()])
                .map(|j| Witness::Rule(*j))
        };
        let now = i.base(r) || self.state.support[r.index()] > 0;
        if now == self.state.s[r.index()] {
            return false;
        }
        self.state.s[r.index()] = now;
        pass.flips.push(r);
        stats.record(Work::NodeFlip);
        true
    }
    /// Marks one downstream incidence of a flipped node (`id` from
    /// `rules_by_premise.row(r)`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fan_out<S: StatsSink, V: InputView>(
        &mut self,
        id: IncId,
        pass: &mut Pass,
        i: &V,
        index: &Indexes,
        topo: &PearceKelly,
        stats: &mut S,
        units: &mut u64,
    ) {
        *units += 1;
        crate::invariant_assert!(
            topo.rank(&CycleNode::Revision(
                i.conclusion(index.inc_rule[id.index()])
            )) > pass.previous.expect("a popped node")
        );
        // The examination is the unit; a fresh mark is not counted again.
        let mut marks = 0;
        self.incidence(id, index, i, topo, &mut pass.heap, stats, false, &mut marks);
        *units += marks.saturating_sub(1);
    }
    /// Closes an emptied pass (DA5 for the counting rows).
    pub(crate) fn end(&mut self, pass: Pass) -> Maintenance {
        crate::invariant_assert!(pass.heap.is_empty());
        self.pending_inc.reset();
        self.pending_rule.reset();
        for r in &pass.popped {
            self.queued[r.index()] = false;
        }
        #[cfg(any(debug_assertions, feature = "paranoid"))]
        {
            let visited: std::collections::BTreeSet<_> = pass.popped.iter().copied().collect();
            for n in 0..pass.old.s.len() {
                if !visited.contains(&RevisionId(n as u32)) {
                    assert_eq!(pass.old.s[n], self.state.s[n]);
                    assert_eq!(pass.old.support[n], self.state.support[n]);
                    assert_eq!(pass.old.witness[n], self.state.witness[n]);
                }
            }
        }
        let mut flips = pass.flips;
        flips.sort_unstable();
        Maintenance {
            flips,
            before: pass.before,
        }
    }
}
impl Maintainer for Counting {
    const INCREMENTAL_LAYERS: bool = true;
    fn retained_bytes(&self) -> usize {
        self.state.retained_bytes()
            + crate::memory::bool_bytes(&self.dirty_inc)
            + crate::memory::bool_bytes(&self.dirty_rule)
            + crate::memory::bool_bytes(&self.queued)
    }
    fn state(&self) -> &CountingState {
        &self.state
    }
    fn counting(&mut self) -> Option<&mut Counting> {
        Some(self)
    }
    fn is_queued(&self, r: RevisionId) -> bool {
        self.queued.get(r.index()).copied().unwrap_or(false)
    }
    fn begin_journal(&mut self) {
        let s = &self.state;
        self.journal = Some(Journal {
            lens: [s.s.len(), s.unsat.len(), s.sat.len()],
            rows: Vec::new(),
        });
    }
    fn rollback_journal(&mut self) {
        let Some(Journal { lens, rows }) = self.journal.take() else {
            return;
        };
        let s = &mut self.state;
        for (r, row, by_rule, by_inc) in rows {
            (s.s[r.index()], s.support[r.index()], s.witness[r.index()]) =
                (row.s, row.support, row.witness);
            for (j, unsat, counted) in by_rule {
                (s.unsat[j.index()], s.counted[j.index()]) = (unsat, counted);
            }
            for (n, sat) in by_inc {
                s.sat[n] = sat;
            }
        }
        let [revisions, rules, incs] = lens;
        for v in [&mut s.s, &mut self.queued] {
            v.truncate(revisions);
        }
        s.support.truncate(revisions);
        s.witness.truncate(revisions);
        s.unsat.truncate(rules);
        s.counted.truncate(rules);
        self.dirty_rule.truncate(rules);
        s.sat.truncate(incs);
        self.dirty_inc.truncate(incs);
    }
    fn update<S: StatsSink, V: InputView>(
        &mut self,
        i: &V,
        index: &Indexes,
        topo: &PearceKelly,
        delta: &NetDelta,
        stats: &mut S,
    ) -> Maintenance {
        let units = &mut 0;
        let mut pass = self.begin(i, index, topo, delta, stats, units);
        while let Some(r) = self.pop(&mut pass, i, index, topo, stats, units) {
            let mut items = self.take_pending_inc(r);
            while let Some(id) = self.next_pending_inc(&mut items) {
                self.drain_incidence(id, i, index, stats, units);
            }
            let mut items = self.take_pending_rule(r);
            while let Some(j) = self.next_pending_rule(&mut items) {
                self.drain_rule(r, j, i, stats, units);
            }
            if self.conclude(r, &mut pass, i, index, stats) {
                for id in index.rules_by_premise.row(r.index()) {
                    self.fan_out(*id, &mut pass, i, index, topo, stats, units);
                }
            }
        }
        self.end(pass)
    }
}
