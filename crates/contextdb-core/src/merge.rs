//! The database half of `Merge` (SPEC SD19, ADR 0013 §6.3 steps 2–5): the
//! kernel classifies and plans in global ids; the target branch's database
//! publishes the plan, under its local ids, like one structural commit.
use crate::{CommitContext, Db, Maintainer, StatsSink, stage};
use contextdb_model::*;
use std::collections::BTreeMap;

/// A merge's effect on the target database, in its local ids. New objects
/// take the next local ids in order; their records reference local ids.
#[derive(Clone, Debug, Default)]
pub(crate) struct MergePlan {
    /// New logicals with their head and metadata.
    pub logicals: Vec<(Logical, RevisionId, Metadata)>,
    /// New revisions with their base bit.
    pub revisions: Vec<(Revision, bool)>,
    /// New rules; `enabled` is their state on the target.
    pub rules: Vec<Rule>,
    /// New artifacts: imported ones, then the Conflicts the merge opens.
    pub artifacts: Vec<Artifact>,
    pub conflicts: usize,
    pub heads: BTreeMap<LogicalId, RevisionId>,
    pub base: BTreeMap<RevisionId, bool>,
    pub enabled: BTreeMap<RuleId, bool>,
    pub artifact_open: BTreeMap<ArtifactId, bool>,
    pub metadata: BTreeMap<LogicalId, Metadata>,
    pub validations: BTreeMap<(RuleId, HeadVector), ValidationRecord>,
    pub policy: Option<PolicyDoc>,
    /// Newly visible code anchors, unobserved on the target until its
    /// observer's next watermark for their scope.
    pub unobserved: Vec<LogicalId>,
}

impl<M: Maintainer, S: StatsSink> Db<M, S> {
    /// Publishes a merge plan. Newly visible rules enter the cycle graph in
    /// ascending id (step 3); a cycle refuses the merge with
    /// `CycleDetected` at `op_index` and changes nothing. `op_errors` then
    /// reports the merge's remaining Op-level errors, before A5 checks the
    /// merged state (newly visible revisions count as new; §9 order).
    pub(crate) fn commit_merge(
        &mut self,
        plan: &MergePlan,
        op_index: usize,
        ctx: &CommitContext<'_>,
        op_errors: impl FnOnce() -> Result<(), TxError>,
    ) -> Result<CommitResult, TxError> {
        let epoch = ctx.epoch.expect("the kernel supplies merge epochs");
        crate::invariant_assert!(epoch > self.epoch, "commit epochs increase");
        let pk_mark = self.pk.begin();
        let mut stats = S::default();
        let mut staged = stage::Stage::new(
            &self.tables,
            &self.logicals,
            &self.revisions,
            &self.rules,
            &self.indexes,
        );
        for (logical, head, metadata) in &plan.logicals {
            let l = staged.push_logical(logical.clone(), *head);
            if *metadata != Metadata::default() {
                staged.metadata.insert(l, metadata.clone());
            }
        }
        for (revision, base) in &plan.revisions {
            staged.push_revision(revision.clone(), *base);
        }
        for rule in &plan.rules {
            staged.push_rule(rule.clone());
        }
        staged.new_artifacts.extend(plan.artifacts.iter().cloned());
        staged.heads.extend(&plan.heads);
        staged.base.extend(&plan.base);
        staged.enabled.extend(&plan.enabled);
        staged.artifact_open.extend(&plan.artifact_open);
        staged
            .metadata
            .extend(plan.metadata.iter().map(|(l, m)| (*l, m.clone())));
        staged.validations.extend(plan.validations.clone());
        staged.policy = plan.policy.clone();
        let checked = (self.pk.sync(&staged, op_index, &mut stats))
            .and_then(|()| op_errors())
            .and_then(|()| crate::txn::final_policy(&staged));
        if let Err(error) = checked {
            drop(staged);
            self.pk.rollback(pk_mark);
            return Err(error);
        }
        let parts = staged.into_parts();
        let first = self.tables.artifacts.len() + plan.artifacts.len() - plan.conflicts;
        let allocations = Allocations {
            new_artifacts: (first..first + plan.conflicts)
                .map(|n| ArtifactId(n as u32))
                .collect(),
            ..Allocations::default()
        };
        let result = self.publish(parts, pk_mark, allocations, epoch, stats);
        self.anchor_unobserved.extend(&plan.unobserved);
        Ok(result)
    }
    pub(crate) fn logical_record(&self, l: LogicalId) -> Logical {
        self.logicals.record(l)
    }
    pub(crate) fn revision_record(&self, r: RevisionId) -> Revision {
        self.revisions.record(r)
    }
    pub(crate) fn rule_record(&self, j: RuleId) -> Rule {
        self.rules.record(j)
    }
    pub(crate) fn artifact_record(&self, a: ArtifactId) -> &Artifact {
        &self.tables.artifacts[a.index()]
    }
    pub(crate) fn metadata(&self, l: LogicalId) -> Metadata {
        self.logicals.metadata(l)
    }
    pub(crate) fn validations(&self) -> &BTreeMap<(RuleId, HeadVector), ValidationRecord> {
        &self.tables.validations
    }
    pub(crate) fn policy(&self) -> &PolicyDoc {
        &self.tables.policy
    }
    pub(crate) fn kind_of(&self, l: LogicalId) -> RecordKind {
        self.logicals.kind(l)
    }
}
