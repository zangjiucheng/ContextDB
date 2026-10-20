use std::collections::{BTreeMap, BTreeSet};

macro_rules! ids {
    ($($name:ident:$ty:ty),* $(,)?) => {$(
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub $ty);
        impl $name { pub fn index(self) -> usize { self.0 as usize } }
    )*};
}
ids! { LogicalId:u32, RevisionId:u32, RuleId:u32, ArtifactId:u32, IncId:u32,
BranchId:u16, PrincipalId:u32, Epoch:u64 }
pub const MAIN: BranchId = BranchId(0);
pub type ContentHash = [u8; 32];
pub type HeadVector = Vec<(LogicalId, RevisionId)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    Ref,
    Pin,
    Sem,
    Cur,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecordKind {
    Fact,
    Assumption,
    Decision,
    Plan,
    Convention,
    Procedure,
    Artifact,
    Evidence,
    CodeAnchor,
    Summary,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Channel {
    Agent,
    Observer,
    Admin,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogicalRef {
    Existing(LogicalId),
    Key(String),
    New(u16),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevRef {
    Existing(RevisionId),
    New(u16),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleRef {
    Existing(RuleId),
    New(u16),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactRef {
    Existing(ArtifactId),
    New(u16),
}
impl From<LogicalId> for LogicalRef {
    fn from(id: LogicalId) -> Self {
        Self::Existing(id)
    }
}
impl From<RevisionId> for RevRef {
    fn from(id: RevisionId) -> Self {
        Self::Existing(id)
    }
}
impl From<RuleId> for RuleRef {
    fn from(id: RuleId) -> Self {
        Self::Existing(id)
    }
}
impl From<ArtifactId> for ArtifactRef {
    fn from(id: ArtifactId) -> Self {
        Self::Existing(id)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Verified,
    Rejected,
    Pending,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArtifactType {
    Conflict,
    PendingReview,
    ImportBroken,
    AnchorBroken,
    DriftReport,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyDoc {
    pub version: u16,
    pub require_deps: BTreeSet<RecordKind>,
}
impl Default for PolicyDoc {
    fn default() -> Self {
        Self {
            version: 1,
            require_deps: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    CreateLogical {
        key: String,
        kind: RecordKind,
        scope_lane: String,
        content: ContentHash,
        adopt_as_base: bool,
    },
    AddRevision {
        logical: LogicalRef,
        content: ContentHash,
        adopt_as_base: bool,
    },
    SetHead {
        logical: LogicalRef,
        expected: RevRef,
        new: RevRef,
    },
    AssertBase {
        rev: RevRef,
    },
    RetractBase {
        rev: RevRef,
    },
    AddRule {
        conclusion: RevRef,
        incidences: Vec<(RevRef, EdgeKind)>,
        enabled: bool,
    },
    SetRuleEnabled {
        rule: RuleRef,
        enabled: bool,
    },
    Rebind {
        rule: RuleRef,
        from: RevRef,
        to: RevRef,
    },
    RecordValidation {
        rule: RuleRef,
        curvec_seen: HeadVector,
        verdict: Verdict,
        validator: String,
    },
    SetPolicy {
        policy: PolicyDoc,
    },
    ObserverWatermark {
        scope: String,
        expected: Option<Vec<u8>>,
        position: Vec<u8>,
    },
    SetMetadata {
        logical: LogicalRef,
        tags: BTreeSet<String>,
        dirs: BTreeSet<String>,
    },
    OpenArtifact {
        ty: ArtifactType,
        target: RevRef,
        payload_hash: ContentHash,
    },
    ResolveArtifact {
        id: ArtifactRef,
    },
}
impl Op {
    pub fn name(&self) -> &'static str {
        match self {
            Self::CreateLogical { .. } => "CreateLogical",
            Self::AddRevision { .. } => "AddRevision",
            Self::SetHead { .. } => "SetHead",
            Self::AssertBase { .. } => "AssertBase",
            Self::RetractBase { .. } => "RetractBase",
            Self::AddRule { .. } => "AddRule",
            Self::SetRuleEnabled { .. } => "SetRuleEnabled",
            Self::Rebind { .. } => "Rebind",
            Self::RecordValidation { .. } => "RecordValidation",
            Self::SetPolicy { .. } => "SetPolicy",
            Self::ObserverWatermark { .. } => "ObserverWatermark",
            Self::SetMetadata { .. } => "SetMetadata",
            Self::OpenArtifact { .. } => "OpenArtifact",
            Self::ResolveArtifact { .. } => "ResolveArtifact",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    pub branch: BranchId,
    pub principal: PrincipalId,
    pub channel: Channel,
    pub client_txn_id: Option<[u8; 16]>,
    pub ops: Vec<Op>,
}
impl Batch {
    pub fn new(ops: Vec<Op>) -> Self {
        Self {
            branch: MAIN,
            principal: PrincipalId(0),
            channel: Channel::Agent,
            client_txn_id: None,
            ops,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CycleNode {
    Revision(RevisionId),
    Logical(LogicalId),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvalidRule {
    EmptyPremises,
    SelfPremise,
    ConflictingVersions(LogicalId),
    TooManyPremises,
    ForbiddenConclusion,
    ForbiddenPremise,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebindReason {
    NotPremise,
    SameRevision,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyReason {
    NoEnabledRule,
    BaseNotAllowed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityKind {
    Logical,
    Revision,
    Rule,
    Artifact,
    Incidence,
    Epoch,
    WatermarkScope,
    WatermarkPosition,
    /// A batch whose canonical encoding exceeds a u32 length field (P2 store).
    Encoding,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxError {
    UnknownRef,
    ChannelRestricted {
        op: String,
        channel: Channel,
    },
    KindRestricted,
    ImmutableKind,
    InvalidKey {
        key: String,
    },
    InvalidPolicy,
    MissingBlob {
        content: ContentHash,
    },
    InvalidMetadata,
    LogicalMismatch,
    InvalidRebind {
        reason: RebindReason,
    },
    InvalidRule(InvalidRule),
    DuplicateRule {
        existing: RuleId,
        enabled: bool,
    },
    NoSemIncidence {
        rule: RuleId,
    },
    HeadConflict {
        logical: LogicalId,
        expected: RevisionId,
        actual: RevisionId,
    },
    CurvecMoved {
        rule: RuleId,
        seen: HeadVector,
        current: HeadVector,
    },
    WatermarkConflict {
        scope: String,
        expected: Option<Vec<u8>>,
        actual: Option<Vec<u8>>,
    },
    LogicalExists {
        key: String,
    },
    ArtifactNotOpen {
        id: ArtifactId,
    },
    CycleDetected {
        op_index: usize,
        path: Vec<CycleNode>,
    },
    CapacityExceeded {
        kind: CapacityKind,
    },
    PolicyViolation {
        kind: RecordKind,
        logical: LogicalId,
        reason: PolicyReason,
    },
    InvalidDep,
    TxnIdReuse {
        id: [u8; 16],
    },
    ReadSetConflict {
        logical: LogicalId,
        read: RevisionId,
        now: RevisionId,
    },
    MergeNotCertified {
        logical: LogicalId,
        status: crate::Summary,
    },
}
impl std::fmt::Display for TxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for TxError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Logical {
    pub key: String,
    pub kind: RecordKind,
    pub scope_lane: String,
    pub owner: PrincipalId,
    pub origin_branch: BranchId,
    pub origin_epoch: Epoch,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    pub logical: LogicalId,
    pub content: ContentHash,
    pub author: PrincipalId,
    pub origin_branch: BranchId,
    pub origin_epoch: Epoch,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Incidence {
    pub premise: RevisionId,
    pub kind: EdgeKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub conclusion: RevisionId,
    pub premises: Vec<Incidence>,
    pub references: Vec<RevisionId>,
    pub enabled: bool,
    pub origin_branch: BranchId,
    pub origin_epoch: Epoch,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationRecord {
    pub verdict: Verdict,
    pub validator: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub tags: BTreeSet<String>,
    pub dirs: BTreeSet<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub ty: ArtifactType,
    pub target: RevisionId,
    pub payload_hash: ContentHash,
    pub open: bool,
    pub author: PrincipalId,
    pub origin_branch: BranchId,
    pub origin_epoch: Epoch,
}
/// Neutral snapshot with no validation or propagation logic.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inputs {
    pub logicals: Vec<Logical>,
    pub revisions: Vec<Revision>,
    pub heads: Vec<RevisionId>,
    pub base: Vec<bool>,
    pub rules: Vec<Rule>,
    pub validations: BTreeMap<(RuleId, HeadVector), ValidationRecord>,
    pub artifacts: Vec<Artifact>,
    pub metadata: Vec<Metadata>,
    pub watermarks: BTreeMap<String, Vec<u8>>,
    pub policy: PolicyDoc,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allocations {
    pub new_logicals: Vec<LogicalId>,
    pub new_revisions: Vec<RevisionId>,
    pub new_rules: Vec<RuleId>,
    pub new_artifacts: Vec<ArtifactId>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyResult {
    pub epoch: Epoch,
    pub allocations: Allocations,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetDelta {
    pub base_flips: Vec<RevisionId>,
    pub head_moves: Vec<(LogicalId, Option<RevisionId>, RevisionId)>,
    pub enable_flips: Vec<RuleId>,
    pub new_rules: Vec<RuleId>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitResult {
    pub epoch: Epoch,
    pub allocations: Allocations,
    pub net_delta: NetDelta,
    pub l0_flips: Vec<RevisionId>,
    pub status_diff: Vec<RevisionId>,
}

/// Payload availability consulted by `MissingBlob` (SPEC §6.1 contract 4, ADR 0006 §3).
/// Checked for `CreateLogical.content` and every `AddRevision.content`, dedup hits included.
pub trait BlobPresence {
    fn contains(&self, content: &ContentHash) -> bool;
}
/// P0/P1 callers and log replay: payloads are never consulted.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllPresent;
impl BlobPresence for AllPresent {
    fn contains(&self, _: &ContentHash) -> bool {
        true
    }
}
impl BlobPresence for std::collections::BTreeSet<ContentHash> {
    fn contains(&self, content: &ContentHash) -> bool {
        std::collections::BTreeSet::contains(self, content)
    }
}
