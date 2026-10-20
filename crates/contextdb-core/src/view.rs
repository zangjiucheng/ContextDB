use crate::arena::{RevisionArena, RuleArena};
use contextdb_model::*;

/// Read-only columns consumed by maintainers and queries, never validation logic.
pub trait InputView {
    fn revision_count(&self) -> usize;
    fn rule_count(&self) -> usize;
    fn logical(&self, id: LogicalId) -> &Logical;
    fn revision_logical(&self, id: RevisionId) -> LogicalId;
    fn head(&self, id: LogicalId) -> RevisionId;
    fn base(&self, id: RevisionId) -> bool;
    fn enabled(&self, id: RuleId) -> bool;
    fn conclusion(&self, id: RuleId) -> RevisionId;
    fn premises(&self, id: RuleId) -> impl Iterator<Item = Incidence>;
    fn references(&self, id: RuleId) -> &[RevisionId];
    fn validation(&self, id: RuleId, vector: &HeadVector) -> Option<&ValidationRecord>;
}
impl InputView for Inputs {
    fn revision_count(&self) -> usize {
        self.revisions.len()
    }
    fn rule_count(&self) -> usize {
        self.rules.len()
    }
    fn logical(&self, id: LogicalId) -> &Logical {
        &self.logicals[id.index()]
    }
    fn revision_logical(&self, id: RevisionId) -> LogicalId {
        self.revisions[id.index()].logical
    }
    fn head(&self, id: LogicalId) -> RevisionId {
        self.heads[id.index()]
    }
    fn base(&self, id: RevisionId) -> bool {
        self.base[id.index()]
    }
    fn enabled(&self, id: RuleId) -> bool {
        self.rules[id.index()].enabled
    }
    fn conclusion(&self, id: RuleId) -> RevisionId {
        self.rules[id.index()].conclusion
    }
    fn premises(&self, id: RuleId) -> impl Iterator<Item = Incidence> {
        self.rules[id.index()].premises.iter().copied()
    }
    fn references(&self, id: RuleId) -> &[RevisionId] {
        &self.rules[id.index()].references
    }
    fn validation(&self, id: RuleId, vector: &HeadVector) -> Option<&ValidationRecord> {
        self.validations.get(&(id, vector.clone()))
    }
}
pub(crate) struct StoredView<'a> {
    pub tables: &'a Inputs,
    pub revisions: &'a RevisionArena,
    pub rules: &'a RuleArena,
}
impl InputView for StoredView<'_> {
    fn revision_count(&self) -> usize {
        self.revisions.len()
    }
    fn rule_count(&self) -> usize {
        self.rules.len()
    }
    fn logical(&self, id: LogicalId) -> &Logical {
        &self.tables.logicals[id.index()]
    }
    fn revision_logical(&self, id: RevisionId) -> LogicalId {
        self.revisions.logical(id)
    }
    fn head(&self, id: LogicalId) -> RevisionId {
        self.tables.heads[id.index()]
    }
    fn base(&self, id: RevisionId) -> bool {
        self.tables.base[id.index()]
    }
    fn enabled(&self, id: RuleId) -> bool {
        self.rules.enabled[id.index()]
    }
    fn conclusion(&self, id: RuleId) -> RevisionId {
        self.rules.conclusion(id)
    }
    fn premises(&self, id: RuleId) -> impl Iterator<Item = Incidence> {
        self.rules.premises(id)
    }
    fn references(&self, id: RuleId) -> &[RevisionId] {
        self.rules.references(id)
    }
    fn validation(&self, id: RuleId, vector: &HeadVector) -> Option<&ValidationRecord> {
        self.tables.validations.get(&(id, vector.clone()))
    }
}
