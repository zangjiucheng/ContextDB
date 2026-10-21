use crate::{Db, Maintainer, StatsSink};
use contextdb_model::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeChange {
    pub revision: RevisionId,
    pub before: Option<NodeState>,
    pub after: Option<NodeState>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpochDiff {
    pub from: Epoch,
    pub to: Epoch,
    pub changes: Vec<NodeChange>,
}
/// Only post-states are retained: a change's pre-state is the revision's
/// previous post-state, or absent for a revision created in that epoch.
#[derive(Clone, Debug)]
pub(crate) struct Transition {
    pub epoch: Epoch,
    pub changes: Vec<(RevisionId, NodeState)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Impact {
    pub supported: Vec<RevisionId>,
    pub reference_only: Vec<RevisionId>,
}
/// Exact endpoint comparison over retained post-states in epoch order
/// (shared by in-memory history and the store's diff_log, ADR 0006 §8).
/// Changes that cancel between the endpoints disappear.
pub fn diff_rows<'a>(
    from: Epoch,
    to: Epoch,
    rows: impl Iterator<Item = (Epoch, RevisionId, &'a NodeState)> + Clone,
) -> EpochDiff {
    let (low, high) = if from <= to { (from, to) } else { (to, from) };
    let mut changes = BTreeMap::<RevisionId, NodeChange>::new();
    for (_, r, after) in rows.clone().filter(|(e, _, _)| *e > low && *e <= high) {
        changes
            .entry(r)
            .and_modify(|old| old.after = Some(after.clone()))
            .or_insert_with(|| NodeChange {
                revision: r,
                before: None,
                after: Some(after.clone()),
            });
    }
    for (_, r, state) in rows.take_while(|(e, _, _)| *e <= low) {
        if let Some(c) = changes.get_mut(&r) {
            c.before = Some(state.clone());
        }
    }
    let changes = changes
        .into_values()
        .filter(|c| c.before != c.after)
        .map(|mut c| {
            if from > to {
                std::mem::swap(&mut c.before, &mut c.after);
            }
            c
        })
        .collect();
    EpochDiff { from, to, changes }
}
impl<M: Maintainer, S: StatsSink> Db<M, S> {
    pub(crate) fn record(&mut self, epoch: Epoch, changes: Vec<NodeChange>) {
        crate::invariant_assert!(changes.iter().all(|c| {
            let previous = self
                .history
                .iter()
                .rev()
                .find_map(|t| t.changes.iter().find(|(r, _)| *r == c.revision))
                .map(|(_, s)| s);
            c.before.as_ref() == previous && c.after.is_some()
        }));
        // A fresh exact-size buffer: in-place collect would keep the larger
        // NodeChange allocation alive.
        let mut retained = Vec::with_capacity(changes.len());
        retained.extend(
            changes
                .into_iter()
                .map(|c| (c.revision, c.after.expect("post-state"))),
        );
        self.history.push(Transition {
            epoch,
            changes: retained,
        });
    }
    /// First epoch whose post-states this database retains (SPEC §6.1 read modes;
    /// earlier epochs are served by the store's log replay, ADR 0006 §8).
    pub fn initial_epoch(&self) -> Epoch {
        self.initial_epoch
    }
    /// Post-state of `r` as of `e`, or `None` when `r` did not exist yet.
    fn state_at(&self, r: RevisionId, e: Epoch) -> Option<&NodeState> {
        self.history
            .iter()
            .rev()
            .filter(|t| t.epoch <= e)
            .find_map(|t| t.changes.iter().find(|(x, _)| *x == r).map(|(_, s)| s))
    }
    fn retained(&self, e: Epoch) -> Result<(), TxError> {
        if e < self.initial_epoch || e > self.epoch {
            return Err(TxError::UnknownRef);
        }
        Ok(())
    }
    /// `ReadMode::Historical(e)`: status of a revision as of `e`, head or not.
    pub fn read_historical(&self, r: RevisionId, e: Epoch) -> Result<NodeState, TxError> {
        self.retained(e)?;
        self.state_at(r, e).cloned().ok_or(TxError::UnknownRef)
    }
    /// `ReadMode::Snapshot(e)`: the logical's head as of `e` and its status then.
    /// The head is the logical's revision whose state at `e` is `Current`.
    pub fn read_snapshot(
        &self,
        l: LogicalId,
        e: Epoch,
    ) -> Result<(RevisionId, NodeState), TxError> {
        self.retained(e)?;
        (0..self.revisions.len() as u32)
            .map(RevisionId)
            .filter(|r| self.revisions.logical(*r) == l)
            .find_map(|r| {
                self.state_at(r, e)
                    .filter(|s| s.dimensions.currency == Currency::Current)
                    .map(|s| (r, s.clone()))
            })
            .ok_or(TxError::UnknownRef)
    }
    /// Exact endpoint comparison: changes which cancel between epochs disappear.
    pub fn diff(&self, from: Epoch, to: Epoch) -> Result<EpochDiff, TxError> {
        if from < self.initial_epoch
            || to < self.initial_epoch
            || from > self.epoch
            || to > self.epoch
        {
            return Err(TxError::UnknownRef);
        }
        Ok(diff_rows(
            from,
            to,
            self.history
                .iter()
                .flat_map(|t| t.changes.iter().map(move |(r, s)| (t.epoch, *r, s))),
        ))
    }
    /// Conservative support reachability. Ref sources are reported separately.
    pub fn impact_reachable(&self, roots: &[RevisionId]) -> Result<Impact, TxError> {
        use crate::InputView;
        let view = self.view();
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::new();
        for r in roots {
            if r.index() >= view.revision_count() {
                return Err(TxError::UnknownRef);
            }
            if seen.insert(*r) {
                queue.push_back(*r);
            }
        }
        let mut refs = BTreeSet::new();
        while let Some(r) = queue.pop_front() {
            for j in self.indexes.refs_by_target.row(r.index()) {
                refs.insert(view.conclusion(*j));
            }
            for id in self.indexes.rules_by_premise.row(r.index()).iter().chain(
                self.indexes
                    .sem_by_logical
                    .row(view.revision_logical(r).index()),
            ) {
                let c = view.conclusion(self.indexes.inc_rule[id.index()]);
                if seen.insert(c) {
                    queue.push_back(c);
                }
            }
        }
        let root_set: BTreeSet<_> = roots.iter().copied().collect();
        Ok(Impact {
            reference_only: refs.difference(&seen).copied().collect(),
            supported: seen.difference(&root_set).copied().collect(),
        })
    }
    /// Staged commit followed by unconditional undo. Arena suffixes are truncated;
    /// derived columns and mutable tables are restored, including caches and stats.
    pub fn impact_exact(&mut self, batch: &Batch) -> Result<CommitResult, TxError> {
        let revisions = self.revisions.len();
        let rules = self.rules.len();
        let enabled = self.rules.enabled.clone();
        let tables = self.tables.clone();
        let logicals = self.logicals.clone();
        let maintainer = self.maintainer.clone();
        let stats = self.stats.clone();
        let pk = self.pk.clone();
        let indexes = self.indexes.clone();
        let epoch = self.epoch;
        let derived = self.derived.clone();
        let layers = self.layers.clone();
        let changes = self.changes.clone();
        let history_len = self.history.len();
        #[cfg(feature = "timing")]
        let timing = self.timing.clone();
        let result = self.commit(batch);
        self.revisions.truncate(revisions);
        self.rules.truncate(rules);
        self.rules.enabled = enabled;
        self.tables = tables;
        self.logicals = logicals;
        self.maintainer = maintainer;
        self.stats = stats;
        self.pk = pk;
        self.indexes = indexes;
        self.epoch = epoch;
        self.derived = derived;
        self.layers = layers;
        self.changes = changes;
        self.history.truncate(history_len);
        #[cfg(feature = "timing")]
        {
            self.timing = timing;
        }
        result
    }
}
