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
#[derive(Clone, Debug)]
pub(crate) struct Transition {
    pub epoch: Epoch,
    pub changes: Vec<NodeChange>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Impact {
    pub supported: Vec<RevisionId>,
    pub reference_only: Vec<RevisionId>,
}
impl<M: Maintainer, S: StatsSink> Db<M, S> {
    /// Exact endpoint comparison: changes which cancel between epochs disappear.
    pub fn diff(&self, from: Epoch, to: Epoch) -> Result<EpochDiff, TxError> {
        if from < self.initial_epoch
            || to < self.initial_epoch
            || from > self.epoch
            || to > self.epoch
        {
            return Err(TxError::UnknownRef);
        }
        let (low, high) = if from <= to { (from, to) } else { (to, from) };
        let mut changes = BTreeMap::<RevisionId, NodeChange>::new();
        for step in self
            .history
            .iter()
            .filter(|s| s.epoch > low && s.epoch <= high)
        {
            for c in &step.changes {
                changes
                    .entry(c.revision)
                    .and_modify(|old| old.after = c.after.clone())
                    .or_insert_with(|| c.clone());
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
        Ok(EpochDiff { from, to, changes })
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
            for j in &self.indexes.refs_by_target[r.index()] {
                refs.insert(view.conclusion(*j));
            }
            for id in self.indexes.rules_by_premise[r.index()]
                .iter()
                .chain(&self.indexes.sem_by_logical[view.revision_logical(r).index()])
            {
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
        let maintainer = self.maintainer.clone();
        let stats = self.stats.clone();
        let pk = self.pk.clone();
        let topo = self.topo.clone();
        let indexes = self.indexes.clone();
        let epoch = self.epoch;
        let derived = self.derived.clone();
        let memo = self.memo.clone();
        let history_len = self.history.len();
        #[cfg(feature = "timing")]
        let timing = self.timing.clone();
        let result = self.commit(batch);
        self.revisions.truncate(revisions);
        self.rules.truncate(rules);
        self.rules.enabled = enabled;
        self.tables = tables;
        self.maintainer = maintainer;
        self.stats = stats;
        self.pk = pk;
        self.topo = topo;
        self.indexes = indexes;
        self.epoch = epoch;
        self.derived = derived;
        self.memo = memo;
        self.history.truncate(history_len);
        #[cfg(feature = "timing")]
        {
            self.timing = timing;
        }
        result
    }
}
