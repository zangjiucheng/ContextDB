//! Cascade-bounded lazy maintenance (ADR 0019 §3, SPEC SD14a).
//!
//! A commit's counting pass and its L1/L2 status walk run as one pass,
//! interleaved by rank: the smaller heap top goes first, and on equal rank
//! the counting pop. Processing a node reads only nodes of lower rank, and
//! its own counting row only after its counting pop (P-1), so the pass can
//! stop at any chunk or node boundary and resume later with the same
//! result, as long as no input changes in between. What it leaves is the
//! *frontier*. `lazy: None` runs the same code to the end.
use crate::{
    Db, Maintainer, StatsSink, engine::Pass, fast, history::NodeChange, stats::Work,
    view::StoredView,
};
use contextdb_model::*;
use std::{
    cmp::Reverse,
    collections::{BTreeSet, BinaryHeap},
};

/// `C`: the most units one chunk of a node's fan-in or fan-out runs
/// (ADR 0019 §3.2).
pub const CHUNK: usize = 4096;
/// `S`: the most nodes a settledness search visits (ADR 0019 §3.4).
pub const SEARCH: usize = 4096;

/// The default `E` (ADR 0019 O1), fixed in milestone 1 from the unit rate
/// measured on the local baseline machine: a 1M-revision hub or chain
/// cascade acknowledged after 1,000,000 units took 43–48 ms in the core
/// (target ≈ 50 ms; docs/gates/P11.md).
pub const LAZY_BUDGET: u64 = 1_000_000;
/// A commit's maintenance budget (ADR 0019 §3.2): a slice stops at the first
/// chunk or node boundary after `units` work units; `chunk` is `C`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub units: u64,
    pub chunk: usize,
}
impl Default for Budget {
    fn default() -> Self {
        Self::new(LAZY_BUDGET)
    }
}
impl Budget {
    pub fn new(units: u64) -> Self {
        Self {
            units,
            chunk: CHUNK,
        }
    }
}
/// How far [`Db::drain`] runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drain {
    /// One slice of at most this many units (plus one chunk).
    Units(u64),
    /// Until every node of rank ≤ this is settled.
    Rank(usize),
    /// To the end.
    Full,
}
/// The deferred epoch's diffs, reported by the drain that empties the
/// frontier: what an eager commit would have returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settled {
    pub epoch: Epoch,
    pub l0_flips: Vec<RevisionId>,
    pub status_diff: Vec<RevisionId>,
}

/// The rest of one commit's maintenance (ADR 0019 §3.3). Never persisted;
/// it lives in the branch's `Db` and is copied with it.
#[derive(Clone, Debug)]
pub(crate) struct Frontier<S> {
    epoch: Epoch,
    pass: Pass,
    walk: BinaryHeap<Reverse<(usize, RevisionId)>>,
    walk_previous: Option<usize>,
    /// Every node the walk has queued (popped ones included).
    seeds: BTreeSet<RevisionId>,
    visited: Vec<RevisionId>,
    changes: Vec<NodeChange>,
    paused: Option<Paused>,
    /// The commit's pre-values, for pre-states (`fast::Before`).
    undo: fast::Undo,
    new_rules: Vec<RuleId>,
    old_revisions: usize,
    stats: S,
    units: u64,
    chunk: usize,
}
/// A node whose work a chunk boundary interrupted, or one just popped.
#[derive(Clone, Debug)]
struct Paused {
    r: RevisionId,
    rank: usize,
    step: Step,
}
#[derive(Clone, Debug)]
enum Step {
    /// Counting fan-in: pending incidences, then pending rules.
    CountInc(Vec<IncId>, usize),
    CountRule(Vec<RuleId>, usize),
    /// Counting fan-out of a node whose S flipped.
    CountOut(usize),
    /// Walk fan-in.
    WalkInc(Option<NodeState>, Vec<IncId>, usize),
    WalkRule(Option<NodeState>, Vec<RuleId>, usize),
    /// Walk fan-out: 0 marks premise incidences, 1 marks Sem incidences of
    /// a head, 2 queues every conclusion.
    WalkOut {
        before: Option<NodeState>,
        after: NodeState,
        stage: u8,
        pos: usize,
    },
}
impl Step {
    /// The step's kind (a `WalkOut` stage counts as its own) and position.
    fn position(&self) -> (u8, usize) {
        match self {
            Step::CountInc(_, pos) => (0, *pos),
            Step::CountRule(_, pos) => (1, *pos),
            Step::CountOut(pos) => (2, *pos),
            Step::WalkInc(_, _, pos) => (3, *pos),
            Step::WalkRule(_, _, pos) => (4, *pos),
            Step::WalkOut { stage, pos, .. } => (5 + stage, *pos),
        }
    }
}
impl<S> Frontier<S> {
    /// `min(rank of both heap tops, rank of the paused node)`.
    fn cursor(&self) -> Option<usize> {
        [
            self.pass.top(),
            self.walk.peek().map(|Reverse((rank, _))| *rank),
            self.paused.as_ref().map(|p| p.rank),
        ]
        .into_iter()
        .flatten()
        .min()
    }
    fn is_empty(&self) -> bool {
        self.cursor().is_none()
    }
}
/// What a completed pass hands back to `finish` or `drain`.
pub(crate) struct Done {
    pub settled: Settled,
    pub undo: fast::Undo,
}

impl<M: Maintainer, S: StatsSink> Db<M, S> {
    /// Starts the interleaved pass for a published commit: installs every
    /// counting and layer seed (DA1, not cut: R5), opens the epoch's history
    /// transition, then runs up to `budget`. `Done` when the pass emptied;
    /// otherwise the rest stays as the frontier.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_pass(
        &mut self,
        undo: fast::Undo,
        delta: &NetDelta,
        mut seeds: BTreeSet<RevisionId>,
        revalidated: &[RuleId],
        old_revisions: usize,
        epoch: Epoch,
        mut stats: S,
        budget: Option<Budget>,
    ) -> Option<Done> {
        let mut units = 0;
        let view = StoredView {
            tables: &self.tables,
            logicals: &self.logicals,
            revisions: &self.revisions,
            rules: &self.rules,
        };
        let counting = self.maintainer.counting().expect("incremental maintainer");
        let pass = counting.begin(
            &view,
            &self.indexes,
            &self.pk,
            delta,
            &mut stats,
            &mut units,
        );
        self.layers.grow(&view, &self.indexes);
        let marked = self.layers.seed(&view, &self.indexes, delta, revalidated);
        units += marked.len() as u64;
        seeds.extend(marked);
        let mut walk = BinaryHeap::new();
        for r in &seeds {
            walk.push(Reverse((self.pk.rank(&CycleNode::Revision(*r)), *r)));
            units += 1;
        }
        self.open_transition(epoch);
        let mut f = Frontier {
            epoch,
            pass,
            walk,
            walk_previous: None,
            seeds,
            visited: Vec::new(),
            changes: Vec::new(),
            paused: None,
            undo,
            new_rules: delta.new_rules.clone(),
            old_revisions,
            stats,
            units,
            chunk: budget.map_or(CHUNK, |b| b.chunk),
        };
        if self.advance(&mut f, budget.map(|b| b.units), None) {
            Some(self.complete(f))
        } else {
            self.frontier = Some(Box::new(f));
            None
        }
    }
    /// Whether a commit's maintenance is still pending.
    pub fn has_frontier(&self) -> bool {
        self.frontier.is_some()
    }
    /// The epoch whose maintenance is pending.
    pub fn frontier_epoch(&self) -> Option<Epoch> {
        self.frontier.as_ref().map(|f| f.epoch)
    }
    /// Every node below this rank is settled (ADR 0019 §3.3).
    pub fn cursor(&self) -> Option<usize> {
        self.frontier.as_ref().and_then(|f| f.cursor())
    }
    /// Work units the pending commit has run so far.
    pub fn frontier_units(&self) -> Option<u64> {
        self.frontier.as_ref().map(|f| f.units)
    }
    /// A hash of the pending maintenance, for comparing two replicas (ADR
    /// 0019 §3.5 item 5): epoch, cursor, both heaps' entries sorted by
    /// `(rank, r)` (a `BinaryHeap`'s layout depends on its push history),
    /// the paused node's revision, step and position, and the units run.
    pub fn frontier_signature(&self) -> Option<ContentHash> {
        let f = self.frontier.as_ref()?;
        let mut h = blake3::Hasher::new();
        h.update(&f.epoch.0.to_le_bytes());
        h.update(&f.cursor().map_or(u64::MAX, |c| c as u64).to_le_bytes());
        for heap in [&f.pass.heap, &f.walk] {
            let mut entries: Vec<(usize, RevisionId)> = heap.iter().map(|e| e.0).collect();
            entries.sort_unstable();
            h.update(&(entries.len() as u64).to_le_bytes());
            for (rank, r) in entries {
                h.update(&(rank as u64).to_le_bytes());
                h.update(&r.0.to_le_bytes());
            }
        }
        match &f.paused {
            None => h.update(&[0]),
            Some(p) => {
                let (kind, pos) = p.step.position();
                h.update(&[1, kind]);
                h.update(&p.r.0.to_le_bytes());
                h.update(&(pos as u64).to_le_bytes())
            }
        };
        h.update(&f.units.to_le_bytes());
        Some(*h.finalize().as_bytes())
    }
    /// Runs the pending maintenance (ADR 0019 §3.7). Returns the deferred
    /// epoch's diffs when this drain empties the frontier.
    pub fn drain(&mut self, how: Drain) -> Option<Settled> {
        let mut f = self.frontier.take()?;
        let (budget, through) = match how {
            Drain::Units(e) => (Some(e), None),
            Drain::Rank(k) => (None, Some(k)),
            Drain::Full => (None, None),
        };
        if self.advance(&mut f, budget, through) {
            Some(self.complete(*f).settled)
        } else {
            self.frontier = Some(f);
            None
        }
    }
    /// `r`'s rank in the topological order, which a pull settles through
    /// (ADR 0019 §3.7); `None` for an unknown revision.
    pub fn rank(&self, r: RevisionId) -> Option<usize> {
        (r.index() < self.revisions.len()).then(|| self.pk.rank(&CycleNode::Revision(r)))
    }
    /// `settled(r)` (ADR 0019 §3.4): nothing the frontier can still change
    /// reaches `r`. True below the cursor; otherwise a backward search over
    /// the topology's in-edges, through nodes at or above the cursor, must
    /// find no frontier member within `S` visited nodes.
    pub fn settled(&self, r: RevisionId) -> bool {
        let Some(f) = &self.frontier else {
            return true;
        };
        let Some(cursor) = f.cursor() else {
            return true;
        };
        if r.index() >= self.revisions.len() {
            return true;
        }
        if self.pk.rank(&CycleNode::Revision(r)) < cursor {
            return true;
        }
        let member = |x: RevisionId| self.maintainer.is_queued(x) || f.seeds.contains(&x);
        self.pk.reaches_back(r, cursor, SEARCH, member) == Some(false)
    }
    /// What an unsettled node reads as (ADR 0019 §3.6): structural
    /// `Unknown`; L0/L1/L2, support, witnesses, `sem` and the stale reason
    /// withheld; currency and `conflicted` exact (from inputs, R3).
    pub(crate) fn withheld(&self, r: RevisionId) -> NodeState {
        let current = self.tables.heads[self.revisions.logical(r).index()] == r;
        NodeState {
            l0: false,
            l1: false,
            l2: false,
            support: 0,
            witness: [None; 3],
            dimensions: Dimensions {
                authorized: true,
                conflicted: self.indexes.conflicted(r),
                currency: if current {
                    Currency::Current
                } else {
                    Currency::Superseded
                },
                structural: Structural::Unknown,
                l1: false,
                l2: false,
                semantic: SemState::NotApplicable,
                stale_reason: if current {
                    StaleReason::Unsupported
                } else {
                    StaleReason::HeadSuperseded
                },
                observation: Observation::NotApplicable,
            },
        }
    }
    /// Runs the pass until it empties (true), the slice's `budget` is spent
    /// at a chunk or node boundary, or the next node ranks above `through`.
    fn advance(
        &mut self,
        f: &mut Frontier<S>,
        budget: Option<u64>,
        through: Option<usize>,
    ) -> bool {
        let start = f.units;
        let spent = |f: &Frontier<S>| budget.is_some_and(|e| f.units - start >= e);
        loop {
            if let Some(paused) = f.paused.take() {
                f.paused = self.chunk(f, paused);
                if spent(f) {
                    return f.is_empty();
                }
                continue;
            }
            let count = f.pass.top();
            let walk = f.walk.peek().map(|Reverse((rank, _))| *rank);
            let (rank, counting) = match (count, walk) {
                (None, None) => return true,
                (Some(c), Some(w)) if w < c => (w, false),
                (Some(c), _) => (c, true),
                (None, Some(w)) => (w, false),
            };
            if through.is_some_and(|k| rank > k) {
                return false;
            }
            f.paused = Some(if counting {
                self.count_pop(f)
            } else {
                self.walk_pop(f)
            });
        }
    }
    fn count_pop(&mut self, f: &mut Frontier<S>) -> Paused {
        let view = StoredView {
            tables: &self.tables,
            logicals: &self.logicals,
            revisions: &self.revisions,
            rules: &self.rules,
        };
        let counting = self.maintainer.counting().expect("incremental maintainer");
        let r = counting
            .pop(
                &mut f.pass,
                &view,
                &self.indexes,
                &self.pk,
                &mut f.stats,
                &mut f.units,
            )
            .expect("a queued node");
        Paused {
            r,
            rank: self.pk.rank(&CycleNode::Revision(r)),
            step: Step::CountInc(counting.take_pending_inc(r), 0),
        }
    }
    fn walk_pop(&mut self, f: &mut Frontier<S>) -> Paused {
        let Reverse((rank, r)) = f.walk.pop().expect("a queued node");
        f.units += 1;
        // DA2 (restated, ADR 0019 §3.1): the walk pops in strictly
        // increasing rank, so it visits each node at most once.
        crate::invariant_assert!(f.walk_previous.is_none_or(|old| old < rank));
        f.walk_previous = Some(rank);
        f.stats.record(Work::Status(r));
        f.visited.push(r);
        let state = self.maintainer.state();
        let before = (r.index() < f.old_revisions).then(|| {
            let old = f
                .pass
                .before
                .get(&r)
                .copied()
                .unwrap_or_else(|| state.row(r));
            let before = fast::Before {
                view: StoredView {
                    tables: &self.tables,
                    logicals: &self.logicals,
                    revisions: &self.revisions,
                    rules: &self.rules,
                },
                undo: &f.undo,
            };
            self.layers.node_state(&before, r, old)
        });
        if let Some(journal) = &mut self.journal
            && let Some(rows) = self.layers.footprint(&self.indexes, r, journal.layer_lens)
        {
            journal.layer_rows.push(rows);
        }
        Paused {
            r,
            rank,
            step: Step::WalkInc(before, self.layers.take_pending_inc(r), 0),
        }
    }
    /// Runs one chunk of a node's work. `None` when the node is done.
    fn chunk(&mut self, f: &mut Frontier<S>, mut p: Paused) -> Option<Paused> {
        let start = f.units;
        let full = |f: &Frontier<S>| f.units - start >= f.chunk as u64;
        let r = p.r;
        let view = StoredView {
            tables: &self.tables,
            logicals: &self.logicals,
            revisions: &self.revisions,
            rules: &self.rules,
        };
        let index = &self.indexes;
        loop {
            p.step = match p.step {
                Step::CountInc(items, mut pos) => {
                    let c = self.maintainer.counting().expect("incremental maintainer");
                    while pos < items.len() {
                        c.drain_incidence(items[pos], &view, index, &mut f.stats, &mut f.units);
                        pos += 1;
                        if full(f) && pos < items.len() {
                            p.step = Step::CountInc(items, pos);
                            return Some(p);
                        }
                    }
                    Step::CountRule(c.take_pending_rule(r), 0)
                }
                Step::CountRule(items, mut pos) => {
                    let c = self.maintainer.counting().expect("incremental maintainer");
                    while pos < items.len() {
                        c.drain_rule(r, items[pos], &view, &mut f.stats, &mut f.units);
                        pos += 1;
                        if full(f) && pos < items.len() {
                            p.step = Step::CountRule(items, pos);
                            return Some(p);
                        }
                    }
                    if !c.conclude(r, &mut f.pass, &view, index, &mut f.stats) {
                        self.counted(f, r);
                        return None;
                    }
                    Step::CountOut(0)
                }
                Step::CountOut(mut pos) => {
                    let c = self.maintainer.counting().expect("incremental maintainer");
                    let row = index.rules_by_premise.row(r.index());
                    while pos < row.len() {
                        c.fan_out(
                            row.get(pos).copied().expect("in the row"),
                            &mut f.pass,
                            &view,
                            index,
                            &self.pk,
                            &mut f.stats,
                            &mut f.units,
                        );
                        pos += 1;
                        if full(f) && pos < row.len() {
                            p.step = Step::CountOut(pos);
                            return Some(p);
                        }
                    }
                    self.counted(f, r);
                    return None;
                }
                Step::WalkInc(before, items, mut pos) => {
                    while pos < items.len() {
                        self.layers
                            .drain_incidence(&view, index, items[pos], &mut f.units);
                        pos += 1;
                        if full(f) && pos < items.len() {
                            p.step = Step::WalkInc(before, items, pos);
                            return Some(p);
                        }
                    }
                    Step::WalkRule(before, self.layers.take_pending_rule(r), 0)
                }
                Step::WalkRule(before, items, mut pos) => {
                    while pos < items.len() {
                        self.layers.drain_rule(&view, items[pos], &mut f.units);
                        pos += 1;
                        if full(f) && pos < items.len() {
                            p.step = Step::WalkRule(before, items, pos);
                            return Some(p);
                        }
                    }
                    let state = self.maintainer.state();
                    self.layers.node(&view, state, index, r);
                    let after = self.layers.node_state(&view, r, state.row(r));
                    let propagates = before.as_ref().is_none_or(|before| {
                        before.l0 != after.l0
                            || before.l1 != after.l1
                            || before.l2 != after.l2
                            || before.dimensions.stale_reason != after.dimensions.stale_reason
                    });
                    if !propagates {
                        self.walked(f, r, before, after);
                        return None;
                    }
                    Step::WalkOut {
                        before,
                        after,
                        stage: 0,
                        pos: 0,
                    }
                }
                Step::WalkOut {
                    before,
                    after,
                    stage,
                    mut pos,
                } => {
                    let logical = self.revisions.logical(r);
                    let row = match stage {
                        0 | 2 => Some(index.rules_by_premise.row(r.index())),
                        _ if self.tables.heads[logical.index()] == r => {
                            Some(index.sem_by_logical.row(logical.index()))
                        }
                        _ => None,
                    };
                    let len = row.as_ref().map_or(0, |row| row.len());
                    while pos < len {
                        let id = *row
                            .as_ref()
                            .and_then(|row| row.get(pos))
                            .expect("in the row");
                        f.units += 1;
                        let c = if stage == 2 {
                            Some(self.rules.conclusion(index.inc_rule[id.index()]))
                        } else {
                            self.layers.mark_incidence(&view, index, id)
                        };
                        if let Some(c) = c
                            && f.seeds.insert(c)
                        {
                            f.walk
                                .push(Reverse((self.pk.rank(&CycleNode::Revision(c)), c)));
                            f.units += 1;
                        }
                        pos += 1;
                        if full(f) && pos < len {
                            p.step = Step::WalkOut {
                                before,
                                after,
                                stage,
                                pos,
                            };
                            return Some(p);
                        }
                    }
                    if stage == 2 {
                        self.walked(f, r, before, after);
                        return None;
                    }
                    Step::WalkOut {
                        before,
                        after,
                        stage: stage + 1,
                        pos: 0,
                    }
                }
            };
        }
    }
    /// A counting pop is done: an existing revision it popped now needs its
    /// status walked (eager seeds with every popped row).
    fn counted(&mut self, f: &mut Frontier<S>, r: RevisionId) {
        if r.index() < f.old_revisions && f.seeds.insert(r) {
            f.walk
                .push(Reverse((self.pk.rank(&CycleNode::Revision(r)), r)));
            f.units += 1;
        }
    }
    /// A walk pop is done: its change joins the epoch's open transition.
    fn walked(
        &mut self,
        f: &mut Frontier<S>,
        r: RevisionId,
        before: Option<NodeState>,
        after: NodeState,
    ) {
        if before.as_ref() != Some(&after) {
            let change = NodeChange {
                revision: r,
                before,
                after: Some(after),
            };
            self.append_change(&change);
            f.changes.push(change);
        }
    }
    /// Full drain (ADR 0019 §3.3): the moved assertions, history in eager
    /// order, the derived snapshot patch, and the epoch's statistics.
    fn complete(&mut self, mut f: Frontier<S>) -> Done {
        let counting = self.maintainer.counting().expect("incremental maintainer");
        let maintenance = counting.end(f.pass);
        #[cfg(any(debug_assertions, feature = "paranoid"))]
        self.layers.assert_matches_rebuild(
            &self.view(),
            self.maintainer.state(),
            &self.indexes,
            &self.pk,
        );
        self.close_transition(f.epoch);
        f.changes.sort_by_key(|c| c.revision);
        self.patch_snapshot(&f.changes, &f.visited, &f.new_rules);
        f.stats.record(Work::Units(f.units));
        self.stats = f.stats;
        #[cfg(feature = "paranoid")]
        assert_eq!(self.derived(), &contextdb_oracle::jacobi(&self.inputs()));
        Done {
            settled: Settled {
                epoch: f.epoch,
                l0_flips: maintenance.flips,
                status_diff: f.changes.iter().map(|c| c.revision).collect(),
            },
            undo: f.undo,
        }
    }
    /// Extends a materialized snapshot for new revisions and rules, and
    /// patches changed rows (rebuilding is O(N)).
    fn patch_snapshot(
        &mut self,
        changes: &[NodeChange],
        visited: &[RevisionId],
        new_rules: &[RuleId],
    ) {
        let Some(snapshot) = self.derived.get_mut() else {
            return;
        };
        for change in changes {
            let after = change.after.as_ref().expect("post-state").clone();
            let n = change.revision.index();
            if n < snapshot.nodes.len() {
                snapshot.nodes[n] = after;
            } else {
                crate::invariant_assert!(n == snapshot.nodes.len());
                snapshot.nodes.push(after);
            }
        }
        let added = new_rules.len();
        crate::memory::grow(&mut snapshot.sat, added);
        crate::memory::grow(&mut snapshot.unsat, added);
        crate::memory::grow(&mut snapshot.active, added);
        crate::memory::grow(&mut snapshot.layer_active, added);
        crate::memory::grow(&mut snapshot.sem, added);
        for j in new_rules {
            crate::invariant_assert!(j.index() == snapshot.sat.len());
            snapshot
                .sat
                .push(vec![false; self.indexes.rule_incidences(*j).len()]);
            snapshot.unsat.push(0);
            snapshot.active.push(false);
            snapshot.layer_active.push([false; 2]);
            snapshot.sem.push(SemState::NotRequired);
        }
        let c = self.maintainer.state();
        for r in visited {
            for j in self.indexes.rules_by_concl.row(r.index()) {
                let n = j.index();
                snapshot.sat[n].copy_from_slice(&c.sat[self.indexes.rule_incidences(*j)]);
                snapshot.unsat[n] = c.unsat[n];
                snapshot.active[n] = c.counted[n];
                snapshot.layer_active[n] = self.layers.layer_active(*j);
                snapshot.sem[n] = self.layers.sem(*j);
            }
        }
    }
}
