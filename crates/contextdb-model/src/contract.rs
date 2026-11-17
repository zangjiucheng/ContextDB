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
/// Reserved principal ids (ADR 0017 §3.1). 0–15 and `0x5641_0000..=0x5641_FFFF`
/// are never assigned by an access document, whose ids start at 16.
impl PrincipalId {
    /// The store owner: the uid the daemon runs as.
    pub const STORE_OWNER: Self = Self(0);
    /// The store owner's agent and observer channel principals.
    pub const OWNER_AGENT: Self = Self(1);
    pub const OWNER_OBSERVER: Self = Self(2);
    /// The first id an access document assigns.
    pub const FIRST_MAPPED: Self = Self(16);
    /// The validator workers' range (they act for the store owner).
    pub const VALIDATORS: std::ops::RangeInclusive<u32> = 0x5641_0000..=0x5641_FFFF;
    /// True for ids no access document may assign.
    pub fn is_reserved(self) -> bool {
        self.0 < Self::FIRST_MAPPED.0 || Self::VALIDATORS.contains(&self.0)
    }
    /// True for the store owner and every principal acting for it (its
    /// channels, the reserved ids, the validator workers).
    pub fn is_store_owner(self) -> bool {
        self.is_reserved()
    }
}
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
    /// P10 (SPEC SD21.7): the daemon's replica channel, the only writer of
    /// replica-origin logicals. No client socket maps to it.
    Replica,
}
/// A team's 128-bit id (SPEC SD21.1).
pub type TeamId = [u8; 16];
/// Where a logical comes from (SPEC SD21.7): set once at creation,
/// immutable. `Local` is encoded as absent, so existing objects, ops and
/// digests are unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    #[default]
    Local,
    /// A mirror of team `team`'s record, written by the replica channel.
    Replica { team: TeamId },
}
impl Origin {
    pub fn team(self) -> Option<TeamId> {
        match self {
            Self::Local => None,
            Self::Replica { team } => Some(team),
        }
    }
    /// The observation scope of a replica's watermark, `team:<hex id>`
    /// (ADR 0018 §5.2); `None` for a local record.
    pub fn scope(self) -> Option<String> {
        self.team().map(|t| team_scope(&t))
    }
}
/// The watermark of a team the store left (ADR 0018 §8.5): every mirror's
/// base is retracted, so a read over them never lags.
pub const TEAM_LEFT: &[u8] = b"left";
/// `team:<lowercase hex id>`, the watermark scope of team `team`.
pub fn team_scope(team: &TeamId) -> String {
    let mut s = String::with_capacity(5 + 32);
    s.push_str("team:");
    for b in team {
        s.push_str(&format!("{b:02x}"));
    }
    s
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

/// One lane's roles (ADR 0017 §4.1, §4.3). Role lists hold principal names,
/// `@group`, `*members`, `*owner` and (curators only) `*writers`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneEntry {
    pub members: BTreeSet<String>,
    pub writers: BTreeSet<String>,
    pub curators: BTreeSet<String>,
    pub egress: bool,
    /// Dir narrowing: a principal (or `@group`) → the dirs it may read.
    pub grants: BTreeMap<String, BTreeSet<String>>,
    /// Key-prefix denies: `(principal or @group, prefix)`.
    pub deny: BTreeSet<(String, String)>,
}
impl LaneEntry {
    /// Only the store owner (`default_lane`'s default: fail closed).
    pub fn owner_only() -> Self {
        let owner: BTreeSet<String> = ["*owner".to_owned()].into();
        Self {
            members: owner.clone(),
            writers: owner.clone(),
            curators: owner,
            egress: true,
            grants: BTreeMap::new(),
            deny: BTreeSet::new(),
        }
    }
    /// `code_lanes`' default: every member reads, only the store owner's
    /// observer writes anchors (ADR 0017 §4.1, N5).
    pub fn code_default() -> Self {
        Self {
            members: ["*members".to_owned()].into(),
            ..Self::owner_only()
        }
    }
}
/// The committed access document (ADR 0017 §4): global, set by `SetAccess`.
/// `owner` names the store owner's entry, whose uid is the daemon's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessDoc {
    pub version: u16,
    pub owner: String,
    /// Principal name → OS uid.
    pub principals: BTreeMap<String, u32>,
    pub groups: BTreeMap<String, BTreeSet<String>>,
    pub lanes: BTreeMap<String, LaneEntry>,
    pub code_lanes: LaneEntry,
    /// `[personal] egress` (F6: default `false`).
    pub personal_egress: bool,
    pub default_lane: LaneEntry,
    /// `[team_lanes]` (P10, ADR 0018 §5.4): the entry of every `team:` lane
    /// the document does not list. Absent, they fall to `default_lane`, and
    /// the canonical form and hash are those of a P9 document (m11).
    pub team_lanes: Option<Box<LaneEntry>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    CreateLogical {
        key: String,
        kind: RecordKind,
        scope_lane: String,
        content: ContentHash,
        adopt_as_base: bool,
        /// P10: `Replica{team}` only from the replica channel (A8).
        origin: Origin,
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
        /// Metadata blob (ADR 0015 §3): stored, never read by the kernel.
        meta: Option<ContentHash>,
        /// Fail with `ValidationExists` if `(rule, curvec)` already has a
        /// record on the branch (workers; ADR 0015 §3.1).
        if_absent: bool,
    },
    SetPolicy {
        policy: PolicyDoc,
    },
    ObserverWatermark {
        scope: String,
        expected: Option<Vec<u8>>,
        position: Vec<u8>,
        /// P10: a `team:` catch-up whose start is not in the replica's
        /// per-entry log (ADR 0018 §5.2); strict history treats it as a
        /// change to every mirror of the team.
        gap: bool,
    },
    /// Replaces the access document (ADR 0017 §4.1): admin only, sole Op of
    /// its batch, committed on `main`, global; never in a WHAT_IF or a merge.
    SetAccess {
        doc: Box<AccessDoc>,
    },
    /// Shreds a payload (ADR 0017 §8.3): admin only, global, batches of
    /// `ShredContent` alone, committed on `main`, never in a WHAT_IF. No
    /// effect on derivation; later ops naming the hash are refused.
    ShredContent {
        content: ContentHash,
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
        /// Resolution note blob (ADR 0015 §6).
        note: Option<ContentHash>,
    },
    /// Forks the batch's branch at this commit (SD19, P6). Sole Op of its batch.
    CreateBranch {
        name: String,
    },
    /// Makes the batch's branch read-only (SD19, P6). Sole Op of its batch.
    ArchiveBranch {
        name: String,
    },
    /// Read-set check against the pre-batch heads (SPEC §6.1 contract 12).
    ExpectHeads {
        entries: Vec<(LogicalRef, RevisionId)>,
        source: ReadSource,
    },
    /// P10 (SPEC §6.1 contract 16): makes an existing replica-origin
    /// logical, created on another branch, visible on the batch's branch
    /// with its head revision, and sets head and base. Replica channel only.
    MirrorAttach {
        key: String,
        head: ContentHash,
        base: bool,
    },
    /// Merges `source` into the batch's branch (SD19 supplement, P6).
    Merge {
        source: BranchId,
        require_certified: bool,
        certify: Vec<LogicalRef>,
    },
}
/// Who wrote an `ExpectHeads`: the client, or the daemon actor from a
/// certificate's read vector (only the actor may write the latter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadSource {
    Client,
    Certificate(ContentHash),
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
            Self::SetAccess { .. } => "SetAccess",
            Self::ShredContent { .. } => "ShredContent",
            Self::SetMetadata { .. } => "SetMetadata",
            Self::OpenArtifact { .. } => "OpenArtifact",
            Self::ResolveArtifact { .. } => "ResolveArtifact",
            Self::CreateBranch { .. } => "CreateBranch",
            Self::ArchiveBranch { .. } => "ArchiveBranch",
            Self::ExpectHeads { .. } => "ExpectHeads",
            Self::MirrorAttach { .. } => "MirrorAttach",
            Self::Merge { .. } => "Merge",
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
    /// The branch id space (u16, P6).
    Branch,
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
    /// `if_absent` RecordValidation found a record (ADR 0015 §3.1).
    ValidationExists {
        rule: RuleId,
        curvec: HeadVector,
        verdict: Verdict,
    },
    WatermarkConflict {
        scope: String,
        expected: Option<Vec<u8>>,
        actual: Option<Vec<u8>>,
    },
    LogicalExists {
        key: String,
    },
    /// P10: a local write to a `team/` key or `team:` lane (SPEC §6.1
    /// contract 6).
    ReservedNamespace,
    /// P10 A8: a non-replica write to a replica-origin logical, or a local
    /// rule, Rebind or artifact naming one.
    ReplicaReadOnly,
    /// P10: a merge would carry a rule or validation naming a mirror
    /// revision the target cannot see (SD19 team supplement).
    MirrorLag,
    /// P10 (ADR 0018 §6.1): a batch writing both team and local records,
    /// or records of two teams.
    MixedTeamBatch,
    /// P10 (§4.1): a team proposal relying on a record that is not a
    /// record of the same team.
    TeamDepOutside,
    /// P10 (O3): a local batch relying on a record that exists only in a
    /// queued proposal.
    Unpublished,
    /// P10: a team proposal refused by the team layer or the queue; `code`
    /// is the SPEC §6.1 name (`accept`'s reason, `EgressDenied`,
    /// `ProposalTooLarge`, `conflicted`, `ReplicaStalled`).
    Team {
        code: String,
        detail: String,
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
    BranchExists {
        name: String,
    },
    InvalidBranchName {
        name: String,
    },
    UnknownBranch,
    BranchArchived {
        name: String,
    },
    SoleOpRequired {
        op: String,
    },
    InvalidMerge,
    MergeConflict {
        kind: MergeConflictKind,
    },
    InvalidReadSet {
        logical: LogicalId,
    },
    /// A refused access document (ADR 0017 §4.2).
    InvalidAccess {
        reason: String,
    },
    /// An op names a shredded payload (ADR 0017 §8.3).
    ContentShredded {
        content: ContentHash,
    },
    /// The batch's principal lacks `need` on what op `op_index` names
    /// (ADR 0017 §6.1). `reference` is the ref as the op wrote it, never a
    /// resolved id of anything the principal cannot read.
    Unauthorized {
        op_index: usize,
        reference: String,
        need: AccessNeed,
    },
    /// `personal:<name>` for a name the access document does not list
    /// (ADR 0017 §6.2 rule 1).
    InvalidLane {
        lane: String,
    },
    /// A merge into `main` or a store-owner branch carries changes the
    /// merging principal could not make directly (ADR 0017 §6.2 rule 7);
    /// `count` failing changes, nothing named.
    MergeUnauthorized {
        count: u32,
    },
    /// A commit on, or archive of, a branch another principal created
    /// (ADR 0017 §6.2 rule 6).
    BranchOfAnotherPrincipal,
}
/// What a write needs on a logical (ADR 0017 §6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessNeed {
    Read,
    Write,
    Curate,
}
/// What a merge cannot combine (SD19 supplement).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeConflictKind {
    Policy,
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
    /// P10 (SPEC SD21.7).
    pub origin: Origin,
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
    /// Metadata blob (ADR 0015 §3); not part of `sem` or merge classification.
    pub meta: Option<ContentHash>,
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
/// What a merge conflicts on (ADR 0013 §6.2), in canonical opening order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictKind {
    Head,
    Base,
    Enabled,
    ArtifactOpen,
    Validation,
}
/// One `Conflict` artifact a merge opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeConflictEntry {
    pub kind: ConflictKind,
    pub logical: Option<LogicalId>,
    pub rule: Option<RuleId>,
    /// The artifact's target revision.
    pub target: RevisionId,
    pub artifact: ArtifactId,
    /// The canonical payload; the artifact's `payload_hash` is its BLAKE3.
    pub payload: Vec<u8>,
}
/// A metadata field both sides changed differently: listed, never gating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataField {
    Tags,
    Dirs,
}
/// A merge's typed result (ADR 0013 §6.3 step 8).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub source: BranchId,
    /// Heads the merge moved on existing logicals, `(logical, old, new)`.
    pub moved_heads: Vec<(LogicalId, RevisionId, RevisionId)>,
    pub conflicts: Vec<MergeConflictEntry>,
    pub metadata_conflicts: Vec<(LogicalId, MetadataField)>,
    /// Newly visible logicals, revisions, rules and artifacts.
    pub newly_visible: [usize; 4],
    /// Values both sides changed the same way.
    pub convergent: usize,
}
/// The versioned trailer of a format-3 durable result (ADR 0013 §8): the
/// batch's branch and, for a merge, its report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultTrailer {
    pub branch: BranchId,
    pub merge: Option<MergeReport>,
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
