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
pub use history::{EpochDiff, Impact, NodeChange};
pub use memory::MemoryFootprint;
pub mod engine;
mod layers;
mod query;
pub use query::Proof;
use std::{cell::RefCell, sync::OnceLock};
mod pk;
mod stats;
mod topo;
pub use engine::{Counting, FullRecompute, Maintainer};
pub use pk::PearceKelly;
pub use stats::{CommitStats, NoStats, StatsSink};
mod txn;
mod view;
pub use arena::{Adjacency, Indexes};
use arena::{RevisionArena, RuleArena};
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
    revisions: RevisionArena,
    rules: RuleArena,
    epoch: Epoch,
    derived: OnceLock<DerivedSnapshot>,
    memo: RefCell<layers::LayerMemo>,
    indexes: Indexes,
    topo: NaiveTopo,
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
            revisions: RevisionArena::default(),
            rules: RuleArena::default(),
            epoch: Epoch(0),
            derived: OnceLock::new(),
            memo: RefCell::default(),
            indexes: Indexes::default(),
            topo: NaiveTopo::default(),
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
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
    pub fn inputs(&self) -> Inputs {
        let mut i = self.tables.clone();
        i.revisions = self.revisions.export();
        i.rules = self.rules.export();
        i
    }
    pub fn indexes(&self) -> &Indexes {
        &self.indexes
    }
    pub fn topo(&self) -> &NaiveTopo {
        &self.topo
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
        #[cfg(feature = "timing")]
        let start = std::time::Instant::now();
        let result = self.commit_inner(b);
        #[cfg(feature = "timing")]
        if result.is_ok() {
            self.timing = Some(CommitTiming {
                total_nanos: start.elapsed().as_nanos(),
            });
        }
        result
    }
    fn commit_inner(&mut self, b: &Batch) -> Result<CommitResult, TxError> {
        if b.branch != MAIN {
            return Err(TxError::UnknownRef);
        }
        if fast::eligible(b) {
            return self.commit_mutable(b);
        }
        let mut pk = self.pk.clone();
        let mut stats = S::default();
        let before = self.inputs();
        let before_snapshot = self.derived().clone();
        let rev_mark = self.revisions.len();
        let rule_mark = self.rules.len();
        // Mutable-column undo; other input tables stay staged until publication.
        let enabled_undo = self.rules.enabled.clone();
        let mut working = before.clone();
        let mut allocations = Allocations::default();
        let mut context = txn::Context::new(&working);
        let next = Epoch(self.epoch.0.saturating_add(1));
        let result =
            (|| {
                for (n, op) in b.ops.iter().enumerate() {
                    txn::apply(
                        &mut working,
                        &mut allocations,
                        b,
                        op,
                        n,
                        next,
                        &mut pk,
                        &mut stats,
                        &mut context,
                    )?;
                }
                txn::final_policy(&working, rev_mark)?;
                pk.sync(&working, b.ops.len().saturating_sub(1), &mut stats)?;
                let topo = pk.diagnostic_order();
                let epoch = Epoch(self.epoch.0.checked_add(1).ok_or(
                    TxError::CapacityExceeded {
                        kind: CapacityKind::Epoch,
                    },
                )?);
                Ok((topo, epoch))
            })();
        let (topo, epoch) = match result {
            Ok(v) => v,
            Err(error) => {
                self.revisions.truncate(rev_mark);
                self.rules.truncate(rule_mark);
                self.rules.enabled = enabled_undo;
                return Err(error);
            }
        };
        for r in &working.revisions[rev_mark..] {
            self.revisions.append(r);
        }
        for j in &working.rules[rule_mark..] {
            self.rules.append(j);
        }
        for (n, j) in working.rules.iter().enumerate() {
            self.rules.enabled[n] = j.enabled;
        }
        let net_delta = NetDelta {
            base_flips: working
                .base
                .iter()
                .enumerate()
                .filter(|(n, v)| **v != before.base.get(*n).copied().unwrap_or(false))
                .map(|(n, _)| RevisionId(n as u32))
                .collect(),
            head_moves: working
                .heads
                .iter()
                .enumerate()
                .filter(|(n, v)| before.heads.get(*n) != Some(*v))
                .map(|(n, v)| (LogicalId(n as u32), before.heads.get(n).copied(), *v))
                .collect(),
            enable_flips: working
                .rules
                .iter()
                .enumerate()
                .take(before.rules.len())
                .filter(|(n, j)| j.enabled != before.rules[*n].enabled)
                .map(|(n, _)| RuleId(n as u32))
                .collect(),
            new_rules: (rule_mark..working.rules.len())
                .map(|n| RuleId(n as u32))
                .collect(),
        };
        let indexes = Indexes::build(&working);
        let l0_flips = self
            .maintainer
            .update(&working, &indexes, &pk, &net_delta, &mut stats)
            .flips;
        let mut memo = layers::LayerMemo::default();
        let derived = memo.snapshot(&working, self.maintainer.state(), &indexes, epoch);
        #[cfg(feature = "paranoid")]
        assert_eq!(derived, contextdb_oracle::jacobi(&working));
        let status_diff: Vec<_> = derived
            .nodes
            .iter()
            .enumerate()
            .filter(|(n, v)| before_snapshot.nodes.get(*n) != Some(*v))
            .map(|(n, _)| RevisionId(n as u32))
            .collect();
        self.history.push(history::Transition {
            epoch,
            changes: status_diff
                .iter()
                .map(|r| NodeChange {
                    revision: *r,
                    before: before_snapshot.nodes.get(r.index()).cloned(),
                    after: Some(derived.nodes[r.index()].clone()),
                })
                .collect(),
        });
        self.stats = stats;
        self.pk = pk;
        self.indexes = indexes;
        self.topo = topo;
        self.derived = OnceLock::from(derived);
        self.memo = RefCell::new(memo);
        self.epoch = epoch;
        working.revisions.clear();
        working.rules.clear();
        self.tables = working;
        Ok(CommitResult {
            epoch,
            allocations,
            net_delta,
            l0_flips,
            status_diff,
        })
    }
    fn commit_mutable(&mut self, b: &Batch) -> Result<CommitResult, TxError> {
        let undo = fast::Undo::stage(
            &mut self.tables,
            &self.revisions,
            &mut self.rules,
            b,
            self.epoch,
        )?;
        let delta = undo.delta(&self.tables, &self.rules);
        let mut seeds = fast::status_seeds(&self.view(), &self.indexes, &delta);
        let epoch = Epoch(self.epoch.0 + 1); // checked by stage
        let mut stats = S::default();
        let view = view::StoredView {
            tables: &self.tables,
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
            let old = update
                .before
                .get(&r)
                .copied()
                .unwrap_or_else(|| self.maintainer.state().row(r));
            let before = memo.cached_node(&before_view, r, old);
            memo.invalidate(&[r]);
            let after = memo
                .node(&view, self.maintainer.state(), &self.indexes, r, epoch)
                .expect("status revision");
            let propagates = before.l0 != after.l0
                || before.l1 != after.l1
                || before.l2 != after.l2
                || before.dimensions.stale_reason != after.dimensions.stale_reason;
            if propagates {
                for id in &self.indexes.rules_by_premise[r.index()] {
                    let c = view.conclusion(self.indexes.inc_rule[id.index()]);
                    if seeds.insert(c) {
                        heap.push(Reverse((self.pk.rank(&CycleNode::Revision(c)), c)));
                    }
                }
                let logical = view.revision_logical(r);
                if view.head(logical) == r {
                    for id in &self.indexes.sem_by_logical[logical.index()] {
                        let c = view.conclusion(self.indexes.inc_rule[id.index()]);
                        if seeds.insert(c) {
                            heap.push(Reverse((self.pk.rank(&CycleNode::Revision(c)), c)));
                        }
                    }
                }
            }
            if before != after {
                changes.push(NodeChange {
                    revision: r,
                    before: Some(before),
                    after: Some(after),
                });
            }
        }
        drop(memo);
        // Preserve an already materialized public snapshot by patching changed
        // rows. Dropping its N nodes / per-rule sat allocations here is O(N).
        if let Some(snapshot) = self.derived.get_mut() {
            for change in &changes {
                snapshot.nodes[change.revision.index()] =
                    change.after.as_ref().expect("existing revision").clone();
            }
            let mut memo = self.memo.borrow_mut();
            for r in &seeds {
                for j in &self.indexes.rules_by_concl[r.index()] {
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
        changes.sort_by_key(|c| c.revision);
        let l0_flips = update.flips;
        let status_diff = changes.iter().map(|c| c.revision).collect();
        self.history.push(history::Transition { epoch, changes });
        self.epoch = epoch;
        self.stats = stats;
        #[cfg(feature = "paranoid")]
        assert_eq!(self.derived(), &contextdb_oracle::jacobi(&self.inputs()));
        Ok(CommitResult {
            epoch,
            allocations: Allocations::default(),
            net_delta: delta,
            l0_flips,
            status_diff,
        })
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
