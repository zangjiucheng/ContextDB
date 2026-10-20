//! Sequential transaction front-end with oracle-backed P0 queries.
//! Inputs are exported by value; callers cannot mutate stored inputs or derived state.
//! ```compile_fail
//! let mut db = contextdb_core::Db::default();
//! db.inputs_mut().heads.clear();
//! ```
mod arena;
mod topo;
mod txn;
pub use arena::Indexes;
use arena::{RevisionArena, RuleArena};
use contextdb_model::*;
pub use topo::NaiveTopo;

#[derive(Clone, Debug, Default)]
pub struct Db {
    tables: Inputs,
    revisions: RevisionArena,
    rules: RuleArena,
    epoch: Epoch,
    derived: DerivedSnapshot,
    indexes: Indexes,
    topo: NaiveTopo,
}
impl DerivedView for Db {
    fn derived(&self) -> &DerivedSnapshot {
        &self.derived
    }
}
impl Db {
    pub fn at_epoch(epoch: Epoch) -> Self {
        Self {
            epoch,
            ..Self::default()
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
        h.update(&self.derived.canonical_bytes());
        *h.finalize().as_bytes()
    }
    pub fn derived_digest(&self) -> ContentHash {
        *blake3::hash(&self.derived.canonical_bytes()).as_bytes()
    }
    pub fn status(&self, r: RevisionId) -> Result<Summary, TxError> {
        self.derived
            .nodes
            .get(r.index())
            .map(|n| contextdb_oracle::summarize(&n.dimensions))
            .ok_or(TxError::UnknownRef)
    }
    pub fn why(&self, r: RevisionId) -> Result<Explanation, TxError> {
        contextdb_oracle::why(&self.inputs(), &self.derived, r)
    }
    pub fn why_not(&self, r: RevisionId, depth: usize) -> Result<WhyNot, TxError> {
        contextdb_oracle::why_not(&self.inputs(), &self.derived, r, depth)
    }
    pub fn commit(&mut self, b: &Batch) -> Result<CommitResult, TxError> {
        if b.branch != MAIN {
            return Err(TxError::UnknownRef);
        }
        let before = self.inputs();
        let rev_mark = self.revisions.len();
        let rule_mark = self.rules.len();
        // Mutable-column undo; other input tables stay staged until publication.
        let enabled_undo = self.rules.enabled.clone();
        let mut working = before.clone();
        let mut allocations = Allocations::default();
        let next = Epoch(self.epoch.0.saturating_add(1));
        let result =
            (|| {
                for (n, op) in b.ops.iter().enumerate() {
                    txn::apply(&mut working, &mut allocations, b, op, n, next)?;
                    for r in &working.revisions[self.revisions.len()..] {
                        self.revisions.append(r);
                    }
                    for j in &working.rules[self.rules.len()..] {
                        self.rules.append(j);
                    }
                    for (n, j) in working.rules.iter().enumerate() {
                        self.rules.enabled[n] = j.enabled;
                    }
                }
                txn::final_policy(&working, rev_mark)?;
                let topo = NaiveTopo::build(&working, b.ops.len().saturating_sub(1))?;
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
        let derived = contextdb_oracle::jacobi(&working);
        let l0_flips = derived
            .nodes
            .iter()
            .enumerate()
            .filter(|(n, v)| v.l0 != self.derived.nodes.get(*n).is_some_and(|old| old.l0))
            .map(|(n, _)| RevisionId(n as u32))
            .collect();
        let status_diff = derived
            .nodes
            .iter()
            .enumerate()
            .filter(|(n, v)| self.derived.nodes.get(*n) != Some(*v))
            .map(|(n, _)| RevisionId(n as u32))
            .collect();
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
        self.indexes = Indexes::build(&working);
        self.topo = topo;
        self.derived = derived;
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
