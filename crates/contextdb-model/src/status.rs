use crate::{HeadVector, LogicalId, RevisionId, RuleId};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemState {
    NotRequired,
    Verified,
    Pending,
    Rejected,
    NotApplicable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Witness {
    Base,
    Rule(RuleId),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StaleReason {
    HeadSuperseded,
    Unsupported,
    Rejected,
    SupersededPinnedPremise(RevisionId),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingReason {
    Semantic,
    StructuralUnknown,
    ObservationLag,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Summary {
    Unavailable,
    Conflicted,
    Stale(StaleReason),
    Pending(PendingReason),
    Certified,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Structural {
    Supported,
    Unsupported,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Currency {
    Current,
    Superseded,
    BranchOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    NotApplicable,
    Fresh,
    Lagging,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dimensions {
    pub authorized: bool,
    pub conflicted: bool,
    pub currency: Currency,
    pub structural: Structural,
    pub l1: bool,
    pub l2: bool,
    pub semantic: SemState,
    pub stale_reason: StaleReason,
    pub observation: Observation,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeState {
    pub l0: bool,
    pub l1: bool,
    pub l2: bool,
    pub support: u32,
    pub witness: [Option<Witness>; 3],
    pub dimensions: Dimensions,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DerivedSnapshot {
    pub nodes: Vec<NodeState>,
    pub sat: Vec<Vec<bool>>,
    pub unsat: Vec<u32>,
    pub active: Vec<bool>,
    pub layer_active: Vec<[bool; 2]>,
    pub sem: Vec<SemState>,
}
pub trait DerivedView {
    fn derived(&self) -> &DerivedSnapshot;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Explanation {
    pub revision: RevisionId,
    pub base: bool,
    pub active_rules: Vec<RuleId>,
    pub witness: Option<Witness>,
    pub references: Vec<(RuleId, RevisionId)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IncidenceFailure {
    Unsupported(RevisionId),
    NotCurrent {
        premise: RevisionId,
        head: RevisionId,
    },
    SupersededPinned {
        premise: RevisionId,
        head: RevisionId,
    },
    SemanticHeadUnsupported(RevisionId),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleFailure {
    pub rule: RuleId,
    pub disabled: bool,
    pub gate: SemState,
    pub asserted: HeadVector,
    pub current: HeadVector,
    pub incidences: Vec<IncidenceFailure>,
    pub upstream: Vec<WhyNot>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhyNot {
    pub revision: RevisionId,
    pub rules: Vec<RuleFailure>,
}
/// P0 pure certificate oracle types; ledger/certificate issuance is implemented in P3.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertEntry {
    pub logical: LogicalId,
    pub revision: RevisionId,
    pub required: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CertVerdict {
    Denied,
    Invalid,
    Conflicted,
    Ambiguous,
    Unknown,
    Pending,
    ValidRecertified,
    Valid,
}
