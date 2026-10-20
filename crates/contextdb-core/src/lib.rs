//! Sequential transactions with exact incremental L0 and memoized L1/L2 queries.
//! Inputs are exported by value; callers cannot mutate stored inputs or derived state.
//! ```compile_fail
//! let mut db = contextdb_core::Db::default();
//! db.inputs_mut().heads.clear();
//! ```
// Keep diagnostic assertions active in optimized paranoid builds as well.
macro_rules! invariant_assert { ($($arg:tt)*) => { if cfg!(any(debug_assertions, feature="paranoid")) { assert!($($arg)*); } }; }
pub(crate) use invariant_assert;
mod arena;
mod fast;
mod history;
mod memory;
pub use history::{EpochDiff, Impact, NodeChange, diff_rows};
pub use memory::MemoryFootprint;
pub mod engine;
mod layers;
mod query;
pub use query::Proof;
use std::{cell::RefCell, sync::OnceLock};
mod pk;
mod stage;
mod stats;
mod topo;
pub use engine::{Counting, FullRecompute, Maintainer};
pub use pk::PearceKelly;
pub use stats::{CommitStats, NoStats, StatsSink};
mod txn;
mod view;
pub use arena::{Adjacency, Indexes};
use arena::{LogicalArena, RevisionArena, RuleArena};
use contextdb_model::*;
pub use topo::NaiveTopo;
pub use view::InputView;

/// Nondeterministic wall time, excluded from CommitStats, digest and history.
#[cfg(feature = "timing")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitTiming {
    pub total_nanos: u128,
}
#[derive(Clone, Debug)]
pub struct Db<M: Maintainer = Counting, S: StatsSink = NoStats> {
    maintainer: M,
    stats: S,
    pk: PearceKelly,
    tables: Inputs,
    logicals: LogicalArena,
    revisions: RevisionArena,
    rules: RuleArena,
    epoch: Epoch,
    derived: OnceLock<DerivedSnapshot>,
    memo: RefCell<layers::LayerMemo>,
    indexes: Indexes,
    history: Vec<history::Transition>,
    initial_epoch: Epoch,
    #[cfg(feature = "timing")]
    timing: Option<CommitTiming>,
}
impl<M: Maintainer, S: StatsSink> DerivedView for Db<M, S> {
    fn derived(&self) -> &DerivedSnapshot {
        self.derived.get_or_init(|| {
            self.memo.borrow_mut().snapshot(
                &self.view(),
                self.maintainer.state(),
                &self.indexes,
                self.epoch,
            )
        })
    }
}
impl Default for Db<Counting, NoStats> {
    fn default() -> Self {
        Self::new()
    }
}
impl<M: Maintainer, S: StatsSink> Db<M, S> {
    pub fn new() -> Self {
        Self {
            tables: Inputs::default(),
            logicals: LogicalArena::default(),
            revisions: RevisionArena::default(),
            rules: RuleArena::default(),
            epoch: Epoch(0),
            derived: OnceLock::new(),
            memo: RefCell::default(),
            indexes: Indexes::default(),
            pk: PearceKelly::default(),
            maintainer: M::default(),
            stats: S::default(),
            history: Vec::new(),
            initial_epoch: Epoch(0),
            #[cfg(feature = "timing")]
            timing: None,
        }
    }
    pub fn stats(&self) -> &S {
        &self.stats
    }
    pub fn counting_state(&self) -> &engine::CountingState {
        self.maintainer.state()
    }
    pub fn dynamic_topo(&self) -> &PearceKelly {
        &self.pk
    }
    pub fn at_epoch(epoch: Epoch) -> Self {
        Self {
            epoch,
            initial_epoch: epoch,
            ..Self::new()
        }
    }
    /// Rebuilds a database from a durable input snapshot (ADR 0006 §6): the
    /// same maintenance as one structural commit adding every input at once.
    /// History starts empty at `epoch`; derived state must equal replay (PROP10).
    pub fn from_inputs(mut inputs: Inputs, epoch: Epoch) -> Result<Self, TxError> {
        let mut db = Self::at_epoch(epoch);
        let mut stats = S::default();
        let mut pk = PearceKelly::default();
        pk.sync(&inputs, 0, &mut stats)?;
        let delta = NetDelta {
            base_flips: (0..inputs.base.len())
                .filter(|n| inputs.base[*n])
                .map(|n| RevisionId(n as u32))
                .collect(),
            head_moves: inputs
                .heads
                .iter()
                .enumerate()
                .map(|(n, h)| (LogicalId(n as u32), None, *h))
                .collect(),
            enable_flips: Vec::new(),
            new_rules: (0..inputs.rules.len()).map(|n| RuleId(n as u32)).collect(),
        };
        let indexes = Indexes::build(&inputs);
        db.maintainer
            .update(&inputs, &indexes, &pk, &delta, &mut stats);
        db.revisions.reserve(inputs.revisions.len());
        for r in &inputs.revisions {
            db.revisions.append(r);
        }
        db.rules.reserve(&inputs.rules);
        for j in &inputs.rules {
            db.rules.append(j);
        }
        db.logicals.reserve(&inputs.logicals);
        for l in &inputs.logicals {
            db.logicals.append(l);
        }
        db.logicals.set_metadata(&inputs.metadata);
        inputs.logicals = Vec::new();
        inputs.metadata = Vec::new();
        inputs.revisions = Vec::new();
        inputs.rules = Vec::new();
        db.tables = inputs;
        db.pk = pk;
        db.indexes = indexes;
        // Baseline post-states at `epoch`: pre-states for later diffs. DIFF
        // windows start after `epoch`, so the baseline is never reported itself.
        let baseline = db
            .derived()
            .nodes
            .iter()
            .enumerate()
            .map(|(n, state)| NodeChange {
                revision: RevisionId(n as u32),
                before: None,
                after: Some(state.clone()),
            })
            .collect();
        db.record(epoch, baseline);
        #[cfg(feature = "paranoid")]
        assert_eq!(db.derived(), &contextdb_oracle::jacobi(&db.inputs()));
        Ok(db)
    }
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
    pub fn inputs(&self) -> Inputs {
        let mut i = self.tables.clone();
        (i.logicals, i.metadata) = self.logicals.export();
        i.revisions = self.revisions.export();
        i.rules = self.rules.export();
        i
    }
    pub fn indexes(&self) -> &Indexes {
        &self.indexes
    }
    /// Reference order derived from the dynamic topology; not retained separately.
    pub fn topo(&self) -> NaiveTopo {
        self.pk.diagnostic_order()
    }
    pub fn digest(&self) -> ContentHash {
        let mut h = blake3::Hasher::new();
        h.update(&self.inputs().canonical_bytes());
        h.update(&self.derived().canonical_bytes());
        *h.finalize().as_bytes()
    }
    pub fn derived_digest(&self) -> ContentHash {
        *blake3::hash(&self.derived().canonical_bytes()).as_bytes()
    }
    fn view(&self) -> view::StoredView<'_> {
        view::StoredView {
            tables: &self.tables,
            logicals: &self.logicals,
            revisions: &self.revisions,
            rules: &self.rules,
        }
    }
    pub fn node(&self, r: RevisionId) -> Result<NodeState, TxError> {
        self.memo.borrow_mut().node(
            &self.view(),
            self.maintainer.state(),
            &self.indexes,
            r,
            self.epoch,
        )
    }
    pub fn current(&self, l: LogicalId) -> Result<(RevisionId, NodeState), TxError> {
        let r = *self
            .tables
            .heads
            .get(l.index())
            .ok_or(TxError::UnknownRef)?;
        Ok((r, self.node(r)?))
    }
    /// Recomputes the oracle's per-node L0 definition for `sample` from the
    /// inputs and the stored premise bits, and compares it with the counting
    /// state (store open self-check, ADR 0006 §4 step 7). Returns the first
    /// mismatching revision. The graph is acyclic, so each node is checked
    /// against its own premises.
    pub fn verify_counting_sample(&self, sample: &[RevisionId]) -> Result<(), RevisionId> {
        let view = self.view();
        let c = self.maintainer.state();
        for r in sample {
            let n = r.index();
            if n >= view.revision_count() {
                return Err(*r);
            }
            let holds = |j: RuleId| {
                view.enabled(j)
                    && view.premises(j).all(|p| {
                        c.s[p.premise.index()]
                            && (p.kind != EdgeKind::Cur
                                || view.head(view.revision_logical(p.premise)) == p.premise)
                    })
            };
            let rules = self.indexes.rules_by_concl.row(n);
            let support = rules.iter().filter(|j| holds(**j)).count() as u32;
            let base = view.base(*r);
            let witness = if base {
                Some(Witness::Base)
            } else {
                rules.iter().find(|j| holds(**j)).map(|j| Witness::Rule(*j))
            };
            if c.support[n] != support || c.s[n] != (base || support > 0) || c.witness[n] != witness
            {
                return Err(*r);
            }
        }
        Ok(())
    }
    pub fn revision_count(&self) -> usize {
        self.revisions.len()
    }
    /// Current base bit of a revision (store projections, base_history).
    pub fn is_base(&self, r: RevisionId) -> Result<bool, TxError> {
        self.tables
            .base
            .get(r.index())
            .copied()
            .ok_or(TxError::UnknownRef)
    }
    /// Payload hash of a revision (payload bodies live in the store).
    pub fn revision_content(&self, r: RevisionId) -> Result<ContentHash, TxError> {
        (r.index() < self.revisions.len())
            .then(|| self.revisions.content(r))
            .ok_or(TxError::UnknownRef)
    }
    pub fn current_l0(&self, l: LogicalId) -> Result<(RevisionId, bool), TxError> {
        let r = *self
            .tables
            .heads
            .get(l.index())
            .ok_or(TxError::UnknownRef)?;
        Ok((r, self.maintainer.state().s[r.index()]))
    }
    pub fn status(&self, r: RevisionId) -> Result<Summary, TxError> {
        Ok(contextdb_oracle::summarize(&self.node(r)?.dimensions))
    }
    pub fn why(&self, r: RevisionId) -> Result<Explanation, TxError> {
        query::why(&self.view(), self.maintainer.state(), &self.indexes, r)
    }
    pub fn why_depth(&self, r: RevisionId, depth: usize) -> Result<Proof, TxError> {
        query::proof(
            &self.view(),
            self.maintainer.state(),
            &self.indexes,
            r,
            depth,
        )
    }
    pub fn why_not(&self, r: RevisionId, depth: usize) -> Result<WhyNot, TxError> {
        query::why_not(
            &self.view(),
            self.maintainer.state(),
            &self.indexes,
            &mut self.memo.borrow_mut(),
            r,
            depth,
            self.epoch,
        )
    }
    #[cfg(feature = "timing")]
    pub fn timing(&self) -> Option<&CommitTiming> {
        self.timing.as_ref()
    }
    pub fn commit(&mut self, b: &Batch) -> Result<CommitResult, TxError> {
        self.commit_with(b, &AllPresent)
    }
    /// Commit with payload availability for `MissingBlob` (P2 store; replay uses AllPresent).
    pub fn commit_with(
        &mut self,
        b: &Batch,
        blobs: &dyn BlobPresence,
    ) -> Result<CommitResult, TxError> {
        #[cfg(feature = "timing")]
        let start = std::time::Instant::now();
        let result = self.commit_inner(b, blobs);
        #[cfg(feature = "timing")]
        if result.is_ok() {
            self.timing = Some(CommitTiming {
                total_nanos: start.elapsed().as_nanos(),
            });
        }
        result
    }
    fn commit_inner(
        &mut self,
        b: &Batch,
        blobs: &dyn BlobPresence,
    ) -> Result<CommitResult, TxError> {
        if b.branch != MAIN {
            return Err(TxError::UnknownRef);
        }
        if fast::eligible(b) {
            return self.commit_mutable(b);
        }
        self.commit_structural(b, blobs)
    }
    /// Structural batches (ADR 0007): validate against an overlay, publish
    /// incrementally, then walk statuses from the net-delta seeds.
    fn commit_structural(
        &mut self,
        b: &Batch,
        blobs: &dyn BlobPresence,
    ) -> Result<CommitResult, TxError> {
        #[cfg(any(debug_assertions, feature = "paranoid"))]
        let pk_before = self.pk.clone();
        let pk_mark = self.pk.begin();
        let mut stats = S::default();
        let mut allocations = Allocations::default();
        let now = self.epoch;
        let next = Epoch(now.0.saturating_add(1));
        let mut staged = stage::Stage::new(
            &self.tables,
            &self.logicals,
            &self.revisions,
            &self.rules,
            &self.indexes,
        );
        let pk = &mut self.pk;
        let result = (|| {
            for (n, op) in b.ops.iter().enumerate() {
                txn::apply(
                    &mut staged,
                    &mut allocations,
                    b,
                    op,
                    n,
                    next,
                    pk,
                    &mut stats,
                    blobs,
                )?;
            }
            txn::final_policy(&staged)?;
            pk.sync(&staged, b.ops.len().saturating_sub(1), &mut stats)?;
            now.0
                .checked_add(1)
                .map(Epoch)
                .ok_or(TxError::CapacityExceeded {
                    kind: CapacityKind::Epoch,
                })
        })();
        let stage::Stage {
            new_logicals,
            new_heads,
            new_revisions,
            new_base,
            new_rules,
            new_artifacts,
            heads,
            base,
            enabled,
            artifact_open,
            metadata,
            validations,
            watermarks,
            policy,
            ..
        } = staged;
        let epoch = match result {
            Ok(epoch) => epoch,
            Err(error) => {
                self.pk.rollback(pk_mark);
                #[cfg(any(debug_assertions, feature = "paranoid"))]
                assert_eq!(
                    self.pk, pk_before,
                    "PK rollback restores the published graph"
                );
                return Err(error);
            }
        };
        self.pk.commit();
        // Publication: append, then apply overrides while recording pre-states.
        let (old_logicals, old_revisions, old_rules) =
            (self.logicals.len(), self.revisions.len(), self.rules.len());
        let mut undo = fast::Undo::default();
        self.logicals.reserve(&new_logicals);
        memory::grow(&mut self.tables.heads, new_heads.len());
        for (l, h) in new_logicals.iter().zip(&new_heads) {
            self.logicals.append(l);
            self.tables.heads.push(*h);
        }
        for (l, h) in heads {
            let old = std::mem::replace(&mut self.tables.heads[l.index()], h);
            if old != h {
                undo.heads.insert(l, old);
            }
        }
        self.revisions.reserve(new_revisions.len());
        memory::grow(&mut self.tables.base, new_base.len());
        for (r, b) in new_revisions.iter().zip(&new_base) {
            self.revisions.append(r);
            self.tables.base.push(*b);
        }
        for (r, b) in base {
            let old = std::mem::replace(&mut self.tables.base[r.index()], b);
            if old != b {
                undo.base.insert(r, old);
            }
        }
        self.rules.reserve(&new_rules);
        for j in &new_rules {
            self.rules.append(j);
        }
        for (j, e) in enabled {
            let old = std::mem::replace(&mut self.rules.enabled[j.index()], e);
            if old != e {
                undo.enabled.insert(j, old);
            }
        }
        for (key, record) in validations {
            let old = self.tables.validations.insert(key.clone(), record);
            undo.validations.entry(key).or_insert(old);
        }
        if let Some(p) = policy {
            self.tables.policy = p;
        }
        self.tables.watermarks.extend(watermarks);
        for (l, m) in metadata {
            self.logicals.put_metadata(l, m);
        }
        for (a, open) in artifact_open {
            self.tables.artifacts[a.index()].open = open;
        }
        self.tables.artifacts.extend(new_artifacts);
        let view = view::StoredView {
            tables: &self.tables,
            logicals: &self.logicals,
            revisions: &self.revisions,
            rules: &self.rules,
        };
        self.indexes.append(&view, old_revisions, old_rules);
        #[cfg(feature = "paranoid")]
        assert!(self.indexes == Indexes::build(&self.inputs()));
        let mut base_flips: Vec<RevisionId> = undo.base.keys().copied().collect();
        base_flips.extend(
            (old_revisions..self.revisions.len())
                .filter(|n| self.tables.base[*n])
                .map(|n| RevisionId(n as u32)),
        );
        let mut head_moves: Vec<_> = undo
            .heads
            .iter()
            .map(|(l, old)| (*l, Some(*old), self.tables.heads[l.index()]))
            .collect();
        head_moves.extend(
            (old_logicals..self.logicals.len())
                .map(|n| (LogicalId(n as u32), None, self.tables.heads[n])),
        );
        let delta = NetDelta {
            base_flips,
            head_moves,
            enable_flips: undo.enabled.keys().copied().collect(),
            new_rules: (old_rules..self.rules.len())
                .map(|n| RuleId(n as u32))
                .collect(),
        };
        // New revisions, new rules' conclusions and revalidated rules' conclusions.
        let mut extra: std::collections::BTreeSet<RevisionId> = (old_revisions
            ..self.revisions.len())
            .map(|n| RevisionId(n as u32))
            .collect();
        extra.extend(delta.new_rules.iter().map(|j| self.rules.conclusion(*j)));
        extra.extend(
            undo.validations
                .keys()
                .map(|(j, _)| self.rules.conclusion(*j)),
        );
        Ok(self.finish(undo, delta, allocations, extra, old_revisions, epoch, stats))
    }
    fn commit_mutable(&mut self, b: &Batch) -> Result<CommitResult, TxError> {
        let undo = fast::Undo::stage(
            &mut self.tables,
            &self.logicals,
            &self.revisions,
            &mut self.rules,
            b,
            self.epoch,
        )?;
        let delta = undo.delta(&self.tables, &self.rules);
        let epoch = Epoch(self.epoch.0 + 1); // checked by stage
        let old_revisions = self.revisions.len();
        Ok(self.finish(
            undo,
            delta,
            Allocations::default(),
            Default::default(),
            old_revisions,
            epoch,
            S::default(),
        ))
    }
    /// Shared publication tail: counting maintenance, the status walker from
    /// net-delta seeds (revisions at or above `old_revisions` are new and have
    /// no pre-state), snapshot patching, history and epoch.
    #[allow(clippy::too_many_arguments)]
    fn finish(
        &mut self,
        undo: fast::Undo,
        delta: NetDelta,
        allocations: Allocations,
        extra: std::collections::BTreeSet<RevisionId>,
        old_revisions: usize,
        epoch: Epoch,
        mut stats: S,
    ) -> CommitResult {
        let mut seeds = fast::status_seeds(&self.view(), &self.indexes, &delta);
        seeds.extend(extra);
        let view = view::StoredView {
            tables: &self.tables,
            logicals: &self.logicals,
            revisions: &self.revisions,
            rules: &self.rules,
        };
        let update = self
            .maintainer
            .update(&view, &self.indexes, &self.pk, &delta, &mut stats);
        // Counter/witness-only changes must be reported, even without S flips.
        seeds.extend(update.before.keys().copied());
        let before_view = fast::Before {
            view: view::StoredView {
                tables: &self.tables,
                logicals: &self.logicals,
                revisions: &self.revisions,
                rules: &self.rules,
            },
            undo: &undo,
        };
        use std::{cmp::Reverse, collections::BinaryHeap};
        let mut heap = BinaryHeap::new();
        for r in &seeds {
            heap.push(Reverse((self.pk.rank(&CycleNode::Revision(*r)), *r)));
        }
        let mut changes = Vec::new();
        let mut memo = self.memo.borrow_mut();
        while let Some(Reverse((_, r))) = heap.pop() {
            stats.record(stats::Work::Status(r));
            let is_new = r.index() >= old_revisions;
            let before = (!is_new).then(|| {
                let old = update
                    .before
                    .get(&r)
                    .copied()
                    .unwrap_or_else(|| self.maintainer.state().row(r));
                memo.cached_node(&before_view, r, old)
            });
            memo.invalidate(&[r]);
            let after = memo
                .node(&view, self.maintainer.state(), &self.indexes, r, epoch)
                .expect("status revision");
            let propagates = before.as_ref().is_none_or(|before| {
                before.l0 != after.l0
                    || before.l1 != after.l1
                    || before.l2 != after.l2
                    || before.dimensions.stale_reason != after.dimensions.stale_reason
            });
            if propagates {
                for id in self.indexes.rules_by_premise.row(r.index()) {
                    let c = view.conclusion(self.indexes.inc_rule[id.index()]);
                    if seeds.insert(c) {
                        heap.push(Reverse((self.pk.rank(&CycleNode::Revision(c)), c)));
                    }
                }
                let logical = view.revision_logical(r);
                if view.head(logical) == r {
                    for id in self.indexes.sem_by_logical.row(logical.index()) {
                        let c = view.conclusion(self.indexes.inc_rule[id.index()]);
                        if seeds.insert(c) {
                            heap.push(Reverse((self.pk.rank(&CycleNode::Revision(c)), c)));
                        }
                    }
                }
            }
            if before.as_ref() != Some(&after) {
                changes.push(NodeChange {
                    revision: r,
                    before,
                    after: Some(after),
                });
            }
        }
        drop(memo);
        changes.sort_by_key(|c| c.revision);
        // Preserve an already materialized public snapshot: extend it for new
        // revisions/rules and patch changed rows. Rebuilding would be O(N).
        if let Some(snapshot) = self.derived.get_mut() {
            for change in &changes {
                let after = change.after.as_ref().expect("post-state").clone();
                let n = change.revision.index();
                if n < snapshot.nodes.len() {
                    snapshot.nodes[n] = after;
                } else {
                    crate::invariant_assert!(n == snapshot.nodes.len());
                    snapshot.nodes.push(after);
                }
            }
            let added = delta.new_rules.len();
            memory::grow(&mut snapshot.sat, added);
            memory::grow(&mut snapshot.unsat, added);
            memory::grow(&mut snapshot.active, added);
            memory::grow(&mut snapshot.layer_active, added);
            memory::grow(&mut snapshot.sem, added);
            let missing = self.revisions.len().saturating_sub(snapshot.nodes.len());
            memory::grow(&mut snapshot.nodes, missing);
            for j in delta.new_rules.iter() {
                crate::invariant_assert!(j.index() == snapshot.sat.len());
                snapshot
                    .sat
                    .push(vec![false; self.indexes.rule_incidences(*j).len()]);
                snapshot.unsat.push(0);
                snapshot.active.push(false);
                snapshot.layer_active.push([false; 2]);
                snapshot.sem.push(SemState::NotRequired);
            }
            let mut memo = self.memo.borrow_mut();
            for r in &seeds {
                for j in self.indexes.rules_by_concl.row(r.index()) {
                    memo.refresh_rule(
                        &view,
                        self.maintainer.state(),
                        &self.indexes,
                        *j,
                        epoch,
                        snapshot,
                    );
                }
            }
        }
        let l0_flips = update.flips;
        let status_diff = changes.iter().map(|c| c.revision).collect();
        self.record(epoch, changes);
        self.epoch = epoch;
        self.stats = stats;
        #[cfg(feature = "paranoid")]
        assert_eq!(self.derived(), &contextdb_oracle::jacobi(&self.inputs()));
        CommitResult {
            epoch,
            allocations,
            net_delta: delta,
            l0_flips,
            status_diff,
        }
    }
}

/// Hashes the SPEC's canonical policy JSON, independent of enum discriminants.
pub fn policy_hash(p: &PolicyDoc) -> ContentHash {
    let mut names: Vec<_> = p
        .require_deps
        .iter()
        .map(|k| format!("\"{k:?}\""))
        .collect();
    names.sort();
    let json = format!(
        "{{\"require_deps\":[{}],\"version\":{}}}",
        names.join(","),
        p.version
    );
    *blake3::hash(json.as_bytes()).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maintenance_journal_has_old_rows_without_inventing_new_prestate() {
        let create = |key: &str, content, base| Op::CreateLogical {
            key: key.into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: [content; 32],
            adopt_as_base: base,
        };
        let mut db = Db::default();
        db.commit(&Batch::new(vec![create("source", 0, true)]))
            .unwrap();
        let mut previous = db.maintainer.clone();
        let result = db
            .commit(&Batch::new(vec![
                create("conclusion", 1, false),
                Op::AddRule {
                    conclusion: RevRef::New(0),
                    incidences: vec![(RevisionId(0).into(), EdgeKind::Pin)],
                    enabled: true,
                },
                Op::RetractBase {
                    rev: RevisionId(0).into(),
                },
            ]))
            .unwrap();
        let journal = previous.update(
            &db.inputs(),
            &db.indexes,
            &db.pk,
            &result.net_delta,
            &mut NoStats,
        );
        assert_eq!(journal.flips, vec![RevisionId(0)]);
        assert_eq!(journal.before.len(), 1);
        let source = journal.before.get(&RevisionId(0)).unwrap();
        assert!(source.s);
        assert_eq!(source.support, 0);
        assert_eq!(source.witness, Some(Witness::Base));
        assert!(!journal.before.contains_key(&RevisionId(1)));
        assert_eq!(previous.state(), db.maintainer.state());
    }
    fn logical(key: &str, lane: &str, content: u8) -> Op {
        Op::CreateLogical {
            key: key.into(),
            kind: RecordKind::Fact,
            scope_lane: lane.into(),
            content: [content; 32],
            adopt_as_base: true,
        }
    }
    #[test]
    fn fast_path_resolves_logical_keys_exactly() {
        let mut db = Db::default();
        db.commit(&Batch::new(vec![
            logical("a", "x", 0),
            logical("b", "y", 1),
            logical("c", "x", 2),
        ]))
        .unwrap();
        db.commit(&Batch::new(vec![Op::AddRevision {
            logical: LogicalRef::Existing(LogicalId(1)),
            content: [9; 32],
            adopt_as_base: false,
        }]))
        .unwrap();
        // SetHead is fast-path eligible; the key must select logical 1, not 0 or 2.
        db.commit(&Batch::new(vec![Op::SetHead {
            logical: LogicalRef::Key("b".into()),
            expected: RevisionId(1).into(),
            new: RevisionId(3).into(),
        }]))
        .unwrap();
        assert_eq!(db.current_l0(LogicalId(1)).unwrap().0, RevisionId(3));
        assert_eq!(db.current_l0(LogicalId(0)).unwrap().0, RevisionId(0));
        assert_eq!(
            db.commit(&Batch::new(vec![Op::SetHead {
                logical: LogicalRef::Key("missing".into()),
                expected: RevisionId(0).into(),
                new: RevisionId(0).into(),
            }])),
            Err(TxError::UnknownRef)
        );
        // Lanes are interned, but each logical exports its own lane.
        let lanes: Vec<_> = db
            .inputs()
            .logicals
            .into_iter()
            .map(|l| l.scope_lane)
            .collect();
        assert_eq!(lanes, ["x", "y", "x"]);
    }
    #[test]
    fn exact_impact_restores_structural_state_completely() {
        let mut db = Db::default();
        db.commit(&Batch::new(vec![logical("a", "x", 0)])).unwrap();
        let (inputs, digest) = (db.inputs(), db.digest());
        let (pk, topo, indexes) = (db.pk.clone(), db.topo(), db.indexes.clone());
        let preview = db
            .impact_exact(&Batch::new(vec![
                logical("b", "z", 1),
                Op::AddRule {
                    conclusion: RevRef::New(0),
                    incidences: vec![(RevisionId(0).into(), EdgeKind::Pin)],
                    enabled: true,
                },
                Op::SetMetadata {
                    logical: LogicalRef::Existing(LogicalId(0)),
                    tags: ["t".to_string()].into(),
                    dirs: Default::default(),
                },
            ]))
            .unwrap();
        assert_eq!(preview.allocations.new_revisions, vec![RevisionId(1)]);
        assert_eq!(db.inputs(), inputs);
        assert_eq!(db.digest(), digest);
        assert_eq!(
            (db.pk.clone(), db.topo(), db.indexes.clone()),
            (pk, topo, indexes)
        );
        assert_eq!(db.revisions.len(), 1);
        assert_eq!(db.rules.len(), 0);
    }
    #[test]
    fn policy_json_is_canonical_by_kind_name() {
        let policy = PolicyDoc {
            version: 1,
            require_deps: [
                RecordKind::Plan,
                RecordKind::CodeAnchor,
                RecordKind::Artifact,
            ]
            .into_iter()
            .collect(),
        };
        assert_eq!(
            policy_hash(&policy),
            *blake3::hash(br#"{"require_deps":["Artifact","CodeAnchor","Plan"],"version":1}"#)
                .as_bytes()
        );
    }
}
