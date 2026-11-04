//! IPC protocol (ADR 0008 §5, ADR 0010 §4): length-prefixed, versioned frames
//! whose body is the Wire encoding of a `Request` or an `Envelope`. Requests
//! carry no channel or principal: identity comes from the socket and peer
//! credentials. Version 2 (P4) binds session-scoped requests through
//! [`Binding`] and adds the hook requests.
use contextdb_core::{EpochDiff, Impact, Proof};
use contextdb_model::*;
use std::io::{self, Read, Write};

pub const PROTOCOL_VERSION: u16 = 6;
pub const MAX_FRAME: u32 = 16 << 20;
const KIND_REQUEST: u16 = 1;
const KIND_RESPONSE: u16 = 2;

pub type SessionId = [u8; 16];

macro_rules! wire_struct {
    ($name:ident { $($field:ident),* $(,)? }) => {
        impl Wire for $name {
            fn encode(&self, o: &mut Vec<u8>) {
                $(self.$field.encode(o);)*
            }
            fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
                Ok(Self { $($field: Wire::decode(i)?,)* })
            }
        }
    };
}

/// Which session and agent a session-scoped request acts for (ADR 0010 §2).
/// `agent` is None for the root agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// An explicit session (P3 clients, hooks after `SessionAttach`).
    Session {
        session: SessionId,
        agent: Option<String>,
    },
    /// A harness session key, attached on first use (Codex MCP `_meta`).
    Key {
        harness: String,
        harness_session: String,
        agent: Option<String>,
    },
    /// A tool-call id claimed by the PreToolUse hook (Claude Code MCP).
    Call { id: String },
}
impl Binding {
    pub fn root(session: SessionId) -> Self {
        Self::Session {
            session,
            agent: None,
        }
    }
}
/// One `context_update` line (ADR 0010 §4.4): a read whose head or status
/// changed since it was last reported to this agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateItem {
    pub logical: LogicalId,
    pub key: String,
    pub cert_rev: RevisionId,
    pub current: Option<RevisionId>,
    pub summary: Summary,
}
/// A read in the checked agent's view that is no longer certified at its
/// head, with the agent whose read put it there (ADR 0010 §6.1 reasons).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedRead {
    pub key: String,
    pub agent: Option<String>,
    pub read: RevisionId,
    pub current: Option<RevisionId>,
    pub summary: Summary,
}
/// One report input (ADR 0010 §10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TelemetryEntry {
    Gate {
        session: SessionId,
        agent: Option<String>,
        action: Action,
        mode: GateMode,
        verdict: String,
        decision: Decision,
        available: bool,
        harness_version: String,
        epoch: Epoch,
        /// Wall-clock seconds (the dogfood period, ADR 0010 §10).
        at: u64,
    },
    Write {
        session: SessionId,
        agent: Option<String>,
        with_deps: bool,
        epoch: Epoch,
    },
    Drift {
        session: SessionId,
        agent: Option<String>,
        report: DriftReport,
    },
    Skip {
        stream: [u8; 16],
        from: u64,
        to: u64,
    },
    Unclaimed {
        epoch: Epoch,
    },
}

/// What a read or a dep refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// `<12 hex of cert_id>.<index>` from a certificate of this session.
    Handle(String),
    /// Explicit `logical@rev`.
    Pinned { logical: LogicalId, rev: RevisionId },
    /// A logical's current head.
    Logical(LogicalId),
    /// A logical's current head, by key.
    Key(String),
    /// Explicit `key@rev` (the MCP spelling of `logical@rev`).
    KeyRev { key: String, rev: RevisionId },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Certified,
    Advisory,
}
/// The health of one retrieval projection (ADR 0016 §2.2). Lag only ever
/// misses records: status always comes from the snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexState {
    Ready {
        watermark: Epoch,
    },
    Lagging {
        watermark: Epoch,
        head_epoch: Epoch,
    },
    Rebuilding {
        done: u64,
        total: u64,
    },
    /// The last update failed; it is retried.
    Failed {
        error: String,
    },
    /// No embedder configured (vectors).
    Disabled,
}
impl Wire for IndexState {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Ready { watermark } => {
                0u8.encode(o);
                watermark.encode(o);
            }
            Self::Lagging {
                watermark,
                head_epoch,
            } => {
                1u8.encode(o);
                watermark.encode(o);
                head_epoch.encode(o);
            }
            Self::Rebuilding { done, total } => {
                2u8.encode(o);
                done.encode(o);
                total.encode(o);
            }
            Self::Failed { error } => {
                3u8.encode(o);
                error.encode(o);
            }
            Self::Disabled => 4u8.encode(o),
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Ready {
                watermark: Wire::decode(i)?,
            },
            1 => Self::Lagging {
                watermark: Wire::decode(i)?,
                head_epoch: Wire::decode(i)?,
            },
            2 => Self::Rebuilding {
                done: Wire::decode(i)?,
                total: Wire::decode(i)?,
            },
            3 => Self::Failed {
                error: Wire::decode(i)?,
            },
            4 => Self::Disabled,
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
/// Both retrieval projections' health (ADR 0016 §2.2, T94).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexReport {
    pub fts: IndexState,
    pub vectors: IndexState,
}
wire_struct!(IndexReport { fts, vectors });
/// Which projection `ctx reindex` rebuilds (ADR 0016 §2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReindexTarget {
    Fts,
    Vectors,
    All,
}
impl Wire for ReindexTarget {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Fts,
            1 => Self::Vectors,
            2 => Self::All,
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dep {
    pub target: Target,
    pub kind: EdgeKind,
}
/// `ctx_record` precondition (mandatory CAS, ADR 0008 §8.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteTarget {
    Create {
        key: String,
        scope_lane: String,
    },
    Revise {
        logical: LogicalId,
        expected_head: RevisionId,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// `branch` names the session's branch explicitly; otherwise the
    /// branch named like `git_branch` (the client's checked-out git branch)
    /// if it exists and is open, else `main` (ADR 0013 §7, D1).
    SessionStart {
        harness: String,
        repo: String,
        head_sha: String,
        parent: Option<SessionId>,
        branch: Option<String>,
        git_branch: Option<String>,
    },
    SessionEnd {
        session: SessionId,
    },
    Recall {
        binding: Binding,
        query: String,
        scope: String,
        mode: Mode,
        limit: u32,
        /// How long to wait for the text index to reach the snapshot
        /// (ADR 0016 §2.2; capped at 2 s).
        index_wait_ms: u32,
    },
    Read {
        binding: Binding,
        target: Target,
        mode: Mode,
    },
    Record {
        binding: Binding,
        kind: RecordKind,
        body: Vec<u8>,
        target: WriteTarget,
        deps: Vec<Dep>,
        adopt_as_base: bool,
        client_txn_id: Option<[u8; 16]>,
    },
    Supersede {
        binding: Binding,
        logical: LogicalId,
        body: Vec<u8>,
        expected_head: RevisionId,
        adopt_as_base: bool,
        carry_rules: bool,
        reason: String,
        client_txn_id: Option<[u8; 16]>,
    },
    Why {
        target: Target,
        depth: u32,
    },
    WhyNot {
        target: Target,
        depth: u32,
    },
    Impact {
        target: Target,
    },
    ImpactBatch {
        batch: Batch,
    },
    /// `ctx_check` for the bound agent's view (ADR 0010 §6, §7). With
    /// `mode`, the daemon applies the gate predicate, logs the decision and
    /// returns it; `action` is recorded, never used to filter in P4.
    Check {
        binding: Binding,
        cert_id: Option<ContentHash>,
        live_pos: Vec<(String, Vec<u8>)>,
        action: Option<Action>,
        mode: Option<GateMode>,
        harness_version: String,
        /// Validate and issue with `strict_no_aba` (`ctx check --strict`).
        strict: bool,
    },
    Diff {
        from: Epoch,
        to: Epoch,
    },
    Status,
    TxnStatus {
        id: [u8; 16],
    },
    /// Admin only (ADR 0008 §0.4). Protocol 5 (ADR 0015 §3.2, §4.2): the
    /// daemon writes a `ValidationMeta` blob (kind Admin) with the label,
    /// rationale and evidence; `from_review` also resolves that open
    /// PendingReview in the same batch.
    Validate {
        rule: RuleId,
        verdict: Verdict,
        curvec_seen: HeadVector,
        validator: String,
        label: Option<crate::docs::Label>,
        rationale: String,
        evidence: Vec<String>,
        from_review: Option<ArtifactId>,
    },
    /// Observer and admin only; kernel channel rules still apply. `blobs`
    /// are stored before the batch (e.g. anchor contents).
    Raw {
        blobs: Vec<Vec<u8>>,
        batch: Batch,
    },
    /// Admin only.
    Shutdown,
    /// The live session of a harness session, started if missing (ADR 0010 §2.1).
    SessionAttach {
        harness: String,
        harness_session: String,
        repo: String,
        head_sha: String,
        branch: Option<String>,
        git_branch: Option<String>,
    },
    /// Claude Code PreToolUse on a ContextDB MCP tool: the call with this
    /// tool-use id acts for this session and agent (ADR 0010 §2.3).
    Claim {
        session: SessionId,
        agent: Option<String>,
        call: String,
    },
    /// `context_update`: peek (`ack: None`) or acknowledge token and peek
    /// the next batch (ADR 0010 §4.4).
    Updates {
        binding: Binding,
        ack: Option<u64>,
        max_items: u32,
    },
    /// Outbox delivery for one stream, in seq order; replies once durable
    /// (ADR 0010 §5.2–5.3). `skip` names a range the client cannot resend.
    Facts {
        harness: String,
        harness_session: String,
        stream: [u8; 16],
        packets: Vec<Packet>,
        skip: Option<(u64, u64)>,
    },
    /// Advisory SessionStart briefing; adds nothing to the ledger (ADR 0010
    /// §4.5). No binding: repository-wide counts only (`ctx check --all`).
    Briefing {
        binding: Option<Binding>,
        budget_chars: u32,
    },
    /// Admin only: report inputs from index `since` (ADR 0010 §10).
    Telemetry {
        since: u64,
        limit: u32,
    },
    /// Bounded committed scope state, observer/admin only (ADR 0011).
    /// `branch` is the observer worktree's git branch: the scope is read
    /// (and its next batch committed) on the branch of that name if it
    /// exists and is open, else `main` (ADR 0013 §7, m3).
    AnchorState {
        scope: String,
        branch: Option<String>,
    },
    /// Point-in-time range check of all current roots reachable from anchors.
    CheckAnchors {
        scope: String,
        paths: Vec<String>,
        expected_head: Vec<u8>,
        expected_position: Vec<u8>,
    },
    /// An unbound request (`Why`, `WhyNot`, `Impact`, `ImpactBatch`, `Diff`,
    /// `Status`, `Validate`) on a branch instead of `main` (protocol 4,
    /// ADR 0013 §8). Not nested.
    OnBranch {
        branch: BranchSel,
        request: Box<Request>,
    },
    /// `ctx_commit` (ADR 0013 §4): the client's ops as one batch on the
    /// session's branch. With `expect_cert_valid`, the session's certificate
    /// must validate at commit time and its vector is checked by the kernel
    /// in the same step. Branch ops and certificate read sets are refused.
    Commit {
        binding: Binding,
        ops: Vec<Op>,
        blobs: Vec<Vec<u8>>,
        expect_cert_valid: bool,
        client_txn_id: Option<[u8; 16]>,
    },
    /// `ctx_what_if` (ADR 0013 §5): the result the ops would have on the
    /// session's branch; nothing changes.
    WhatIf {
        binding: Binding,
        ops: Vec<Op>,
    },
    /// Forks `from` (default `main`) as `name`.
    BranchCreate {
        name: String,
        from: Option<String>,
        client_txn_id: Option<[u8; 16]>,
    },
    BranchList,
    BranchArchive {
        name: String,
        client_txn_id: Option<[u8; 16]>,
    },
    /// `ctx_merge` (ADR 0013 §6): `source` into `into`, else the bound
    /// session's branch, else `main`. `certify` names logical keys.
    Merge {
        binding: Option<Binding>,
        source: String,
        into: Option<String>,
        require_certified: bool,
        certify: Vec<String>,
        client_txn_id: Option<[u8; 16]>,
    },
    /// `ctx validators …` (ADR 0015 §4.4). Admin only.
    Validators {
        action: ValidatorsAction,
    },
    /// `ctx_conflicts list` (ADR 0015 §6) on the bound session's branch, or
    /// `branch` (admin), else `main`.
    Conflicts {
        binding: Option<Binding>,
        branch: Option<String>,
        scope: Option<String>,
        state: ConflictState,
        limit: u32,
    },
    /// `ctx_conflicts open`: a Conflict pair (or one side) sharing one
    /// `ConflictDoc` (SD20); `from` names the review it converts (resolved
    /// in the same batch) or the candidate it confirms.
    ConflictOpen {
        binding: Option<Binding>,
        branch: Option<String>,
        a: Target,
        b: Option<Target>,
        reason: String,
        evidence: Vec<String>,
        from: Option<ConflictFrom>,
        expect_cert_valid: bool,
        client_txn_id: Option<[u8; 16]>,
    },
    /// `ctx_resolve`: Conflict and PendingReview artifacts only, with a note.
    Resolve {
        binding: Option<Binding>,
        branch: Option<String>,
        artifacts: Vec<ArtifactId>,
        note: String,
        expect_cert_valid: bool,
        client_txn_id: Option<[u8; 16]>,
    },
    /// `ctx_handoff_begin` (ADR 0015 §7): hands the caller's certified
    /// state to another session; `to_agent` limits who may accept.
    HandoffBegin {
        binding: Binding,
        summary: String,
        to_agent: Option<String>,
    },
    /// `ctx_handoff_accept`: claims a handoff once; without an id, lists
    /// the open handoffs on the caller's branch the caller may accept.
    HandoffAccept {
        binding: Binding,
        id: Option<[u8; 16]>,
    },
    /// `ctx_changes` (ADR 0015 §8): a branch's per-epoch status diffs after
    /// `since_epoch`, from the durable `diff_log`. The branch is the bound
    /// session's unless named (an unbound admin call: named or `main`).
    /// With `wait_ms` (at most 30 s), waits for a later epoch when nothing
    /// is there yet.
    Changes {
        binding: Option<Binding>,
        branch: Option<String>,
        since_epoch: Epoch,
        scope: Option<String>,
        max_items: u32,
        wait_ms: u32,
    },
    /// `ctx reindex` (admin, ADR 0016 §2.3): clear and rebuild a retrieval
    /// projection from the current snapshot, in the background.
    Reindex {
        target: ReindexTarget,
    },
}
/// One `ctx_changes` item: revision `rev` of `key` changed status at
/// `epoch`, to `summary`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeItem {
    pub epoch: Epoch,
    pub logical: LogicalId,
    pub rev: RevisionId,
    pub key: String,
    pub summary: Summary,
}
/// One open handoff, as `ctx_handoff_accept` without an id lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffInfo {
    pub id: [u8; 16],
    pub owner: SessionId,
    pub owner_agent: Option<String>,
    pub to_agent: Option<String>,
    pub summary: String,
    pub epoch: Epoch,
    pub reads: u32,
}
/// Which items `ctx_conflicts list` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictState {
    Open,
    Review,
    Candidates,
    All,
}
impl Wire for ConflictState {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Open,
            1 => Self::Review,
            2 => Self::Candidates,
            3 => Self::All,
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
/// What a `ConflictOpen` converts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictFrom {
    Candidate(u64),
    Review(ArtifactId),
}
impl Wire for ConflictFrom {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Candidate(id) => {
                0u8.encode(o);
                id.encode(o);
            }
            Self::Review(a) => {
                1u8.encode(o);
                a.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Candidate(Wire::decode(i)?),
            1 => Self::Review(Wire::decode(i)?),
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
/// One side of a listed item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictSide {
    pub rev: RevisionId,
    pub key: String,
    /// Whether `rev` is still its logical's head on the branch.
    pub current: bool,
}
/// One `ctx_conflicts list` item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictItem {
    /// `conflict`, `review` or `candidate`.
    pub kind: String,
    pub artifacts: Vec<ArtifactId>,
    pub sides: Vec<ConflictSide>,
    /// A Conflict's reason; a review's suggestion and rationale.
    pub reason: String,
    /// Who opened it: agent, admin, candidate N, review N, merge, or the
    /// validator (and model) of a review.
    pub origin: String,
    pub rule: Option<RuleId>,
    /// `side superseded` (a Conflict side is no longer the head), `stale
    /// curvec` (a review's vector moved on).
    pub marks: Vec<String>,
    pub evidence: Vec<String>,
    /// A candidate pair's id, for `open{from: candidate}`.
    pub candidate: Option<u64>,
}
/// What `ctx validators` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidatorsAction {
    Status,
    /// Re-reads `validators.toml`.
    Reload,
    /// Requeues one job by key, or every `Failed` one, with the generation
    /// bumped.
    Retry {
        key: Option<ContentHash>,
        failed: bool,
    },
    /// Runs each configured validator on a fixed probe task.
    Check,
}
impl Wire for ValidatorsAction {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Status => 0u8.encode(o),
            Self::Reload => 1u8.encode(o),
            Self::Retry { key, failed } => {
                2u8.encode(o);
                key.encode(o);
                failed.encode(o);
            }
            Self::Check => 3u8.encode(o),
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Status,
            1 => Self::Reload,
            2 => Self::Retry {
                key: Wire::decode(i)?,
                failed: Wire::decode(i)?,
            },
            3 => Self::Check,
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
/// One configured validator in a report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatorInfo {
    pub name: String,
    pub kind: crate::docs::ValidatorKind,
    pub version: String,
}
/// One job row in a report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobInfo {
    pub key: ContentHash,
    pub branch: BranchId,
    pub rule: RuleId,
    pub curvec: HeadVector,
    pub generation: u32,
    pub attempts: u32,
    pub last_error: Option<String>,
}
/// A named counter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counter {
    pub name: String,
    pub value: u64,
}
/// `ctx validators` output.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ValidatorsReport {
    pub validators: Vec<ValidatorInfo>,
    /// workers, lease seconds, attempts, LLM calls per hour.
    pub limits: Vec<Counter>,
    /// Job rows per state.
    pub jobs: Vec<Counter>,
    pub failed: Vec<JobInfo>,
    pub stats: Vec<Counter>,
    /// The last `validators.toml` error; the previous configuration stays.
    pub error: Option<String>,
    /// Rows a retry requeued.
    pub retried: u64,
    /// `check`: one line per validator.
    pub check: Vec<String>,
}
/// A payload's content hash (SPEC §6.1 contract 4), for clients that send
/// ops with their bodies (`Commit`, `WhatIf`).
pub fn content_hash(bytes: &[u8]) -> ContentHash {
    *blake3::hash(bytes).as_bytes()
}
/// Which branch an `OnBranch` request reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchSel {
    /// A branch by name.
    Named(String),
    /// The bound session's branch.
    Session(Binding),
}
impl Wire for BranchSel {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Named(name) => {
                0u8.encode(o);
                name.encode(o);
            }
            Self::Session(binding) => {
                1u8.encode(o);
                binding.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Named(Wire::decode(i)?),
            1 => Self::Session(Wire::decode(i)?),
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
/// One branch-table entry (ADR 0013 §2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchEntry {
    pub id: BranchId,
    pub name: String,
    pub parent: Option<BranchId>,
    pub fork_epoch: Epoch,
    pub archived: bool,
}
impl Wire for BranchEntry {
    fn encode(&self, o: &mut Vec<u8>) {
        self.id.encode(o);
        self.name.encode(o);
        self.parent.encode(o);
        self.fork_epoch.encode(o);
        self.archived.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            id: Wire::decode(i)?,
            name: Wire::decode(i)?,
            parent: Wire::decode(i)?,
            fork_epoch: Wire::decode(i)?,
            archived: Wire::decode(i)?,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorEntry {
    pub logical: LogicalId,
    pub key: String,
    pub head: RevisionId,
    pub content: ContentHash,
    pub base: bool,
    pub breaks: Vec<ArtifactId>,
    pub descriptor: Vec<u8>,
}
impl Wire for AnchorEntry {
    fn encode(&self, o: &mut Vec<u8>) {
        self.logical.encode(o);
        self.key.encode(o);
        self.head.encode(o);
        self.content.encode(o);
        self.base.encode(o);
        self.breaks.encode(o);
        self.descriptor.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            logical: Wire::decode(i)?,
            key: Wire::decode(i)?,
            head: Wire::decode(i)?,
            content: Wire::decode(i)?,
            base: Wire::decode(i)?,
            breaks: Wire::decode(i)?,
            descriptor: Wire::decode(i)?,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub logical: LogicalId,
    pub key: String,
    pub kind: RecordKind,
    pub rev: RevisionId,
    pub state: NodeState,
    pub summary: Summary,
    pub handle: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    /// Stable code (SPEC §6.2 name) plus human detail.
    Error {
        code: String,
        detail: String,
    },
    Session {
        session: SessionId,
    },
    Done,
    Recall {
        items: Vec<Item>,
        withheld: Vec<Item>,
        cert_id: Option<ContentHash>,
        index: IndexReport,
    },
    Read {
        item: Item,
        payload: Option<Vec<u8>>,
    },
    Committed {
        result: CommitResult,
    },
    Why {
        proof: Proof,
    },
    WhyNot {
        why_not: WhyNot,
    },
    Impact {
        impact: Impact,
    },
    ImpactExact {
        result: CommitResult,
    },
    Check {
        decision: Decision,
        validation: Validation,
        cert: Option<Box<Certificate>>,
        superseded: Vec<CertDiff>,
        /// The gate decision when the request carried a mode.
        gate: Option<Decision>,
        /// `context_update` items waiting for this agent (not consumed).
        updates_pending: u32,
        /// The view's reads that changed (for warn/deny reasons).
        named: Vec<NamedRead>,
    },
    Diff {
        diff: EpochDiff,
    },
    Status {
        revisions: u64,
        logicals: u64,
        sessions: u64,
        index: IndexReport,
    },
    TxnStatus {
        committed: Option<(Epoch, CommitResult)>,
    },
    Attached {
        session: SessionId,
        created: bool,
    },
    Updates {
        token: u64,
        items: Vec<UpdateItem>,
        more: bool,
    },
    /// `drift`: the reports computed from `Drift` facts of this batch, so
    /// the Stop hook can show them (ADR 0010 §5.4).
    Facts {
        applied_through: u64,
        drift: Vec<DriftReport>,
    },
    /// The batch does not continue the stream: resend from `expected`, or
    /// skip what cannot be resent (ADR 0010 §5.3).
    Gap {
        expected: u64,
    },
    Briefing {
        stale: u32,
        pending: u32,
        conflicted: u32,
        items: Vec<Item>,
    },
    Telemetry {
        entries: Vec<TelemetryEntry>,
        next: u64,
    },
    AnchorState {
        /// The branch the state was read on; the observer commits there.
        branch: BranchId,
        watermark: Option<Vec<u8>>,
        anchors: Vec<AnchorEntry>,
    },
    WhatIf {
        result: CommitResult,
        merge: Option<MergeReport>,
    },
    Branches {
        branches: Vec<BranchEntry>,
    },
    Merged {
        result: CommitResult,
        report: MergeReport,
    },
    /// `expect_cert_valid`: the certificate's verdict at commit time; nothing
    /// was committed.
    Refused {
        validation: Validation,
    },
    Validators {
        report: ValidatorsReport,
    },
    /// `ctx_handoff_begin`: the handoff id and the certificate it carries.
    HandoffBegun {
        id: [u8; 16],
        cert_id: ContentHash,
    },
    /// `ctx_handoff_accept`: the claim, the handoff certificate's verdict
    /// now (with the changed reads), the summary, and how many reads were
    /// copied into the acceptor's ledger (and skipped as already read).
    HandoffAccepted {
        id: [u8; 16],
        owner: SessionId,
        owner_agent: Option<String>,
        summary: String,
        validation: Validation,
        copied: u32,
        skipped: u32,
    },
    /// `ctx_handoff_accept` without an id.
    Handoffs {
        items: Vec<HandoffInfo>,
    },
    /// A `ctx_changes` page: items in epoch order, the epoch reached (pass
    /// it as the next `since_epoch`) and whether more items follow.
    Changes {
        items: Vec<ChangeItem>,
        reached: Epoch,
        more: bool,
    },
    /// `ctx_changes` cannot serve the epochs after `since_epoch` (the
    /// history was cut or the projection rebuilt): re-read with
    /// `ctx_check`, then follow from `earliest`.
    Truncated {
        earliest: Epoch,
    },
    /// `ctx reindex` accepted: the projections' state after marking.
    Reindexed {
        index: IndexReport,
    },
    Conflicts {
        items: Vec<ConflictItem>,
        /// More items than `limit` matched.
        truncated: bool,
    },
}
/// Every response names the snapshot epoch it was computed on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub epoch: Epoch,
    pub response: Response,
}

wire_struct!(Dep { target, kind });
wire_struct!(ValidatorInfo {
    name,
    kind,
    version
});
wire_struct!(JobInfo {
    key,
    branch,
    rule,
    curvec,
    generation,
    attempts,
    last_error
});
wire_struct!(Counter { name, value });
wire_struct!(ConflictSide { rev, key, current });
wire_struct!(ChangeItem {
    epoch,
    logical,
    rev,
    key,
    summary
});
wire_struct!(HandoffInfo {
    id,
    owner,
    owner_agent,
    to_agent,
    summary,
    epoch,
    reads
});
wire_struct!(ConflictItem {
    kind,
    artifacts,
    sides,
    reason,
    origin,
    rule,
    marks,
    evidence,
    candidate
});
wire_struct!(ValidatorsReport {
    validators,
    limits,
    jobs,
    failed,
    stats,
    error,
    retried,
    check
});
wire_struct!(Item {
    logical,
    key,
    kind,
    rev,
    state,
    summary,
    handle
});
wire_struct!(Envelope { epoch, response });
wire_struct!(NamedRead {
    key,
    agent,
    read,
    current,
    summary
});
wire_struct!(UpdateItem {
    logical,
    key,
    cert_rev,
    current,
    summary
});

impl Wire for Binding {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Session { session, agent } => {
                0u8.encode(o);
                session.encode(o);
                agent.encode(o);
            }
            Self::Key {
                harness,
                harness_session,
                agent,
            } => {
                1u8.encode(o);
                harness.encode(o);
                harness_session.encode(o);
                agent.encode(o);
            }
            Self::Call { id } => {
                2u8.encode(o);
                id.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Session {
                session: Wire::decode(i)?,
                agent: Wire::decode(i)?,
            },
            1 => Self::Key {
                harness: Wire::decode(i)?,
                harness_session: Wire::decode(i)?,
                agent: Wire::decode(i)?,
            },
            2 => Self::Call {
                id: Wire::decode(i)?,
            },
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
impl Wire for TelemetryEntry {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Gate {
                session,
                agent,
                action,
                mode,
                verdict,
                decision,
                available,
                harness_version,
                epoch,
                at,
            } => {
                0u8.encode(o);
                session.encode(o);
                agent.encode(o);
                action.encode(o);
                mode.encode(o);
                verdict.encode(o);
                decision.encode(o);
                available.encode(o);
                harness_version.encode(o);
                epoch.encode(o);
                at.encode(o);
            }
            Self::Write {
                session,
                agent,
                with_deps,
                epoch,
            } => {
                1u8.encode(o);
                session.encode(o);
                agent.encode(o);
                with_deps.encode(o);
                epoch.encode(o);
            }
            Self::Drift {
                session,
                agent,
                report,
            } => {
                2u8.encode(o);
                session.encode(o);
                agent.encode(o);
                report.encode(o);
            }
            Self::Skip { stream, from, to } => {
                3u8.encode(o);
                stream.encode(o);
                from.encode(o);
                to.encode(o);
            }
            Self::Unclaimed { epoch } => {
                4u8.encode(o);
                epoch.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Gate {
                session: Wire::decode(i)?,
                agent: Wire::decode(i)?,
                action: Wire::decode(i)?,
                mode: Wire::decode(i)?,
                verdict: Wire::decode(i)?,
                decision: Wire::decode(i)?,
                available: Wire::decode(i)?,
                harness_version: Wire::decode(i)?,
                epoch: Wire::decode(i)?,
                at: Wire::decode(i)?,
            },
            1 => Self::Write {
                session: Wire::decode(i)?,
                agent: Wire::decode(i)?,
                with_deps: Wire::decode(i)?,
                epoch: Wire::decode(i)?,
            },
            2 => Self::Drift {
                session: Wire::decode(i)?,
                agent: Wire::decode(i)?,
                report: Wire::decode(i)?,
            },
            3 => Self::Skip {
                stream: Wire::decode(i)?,
                from: Wire::decode(i)?,
                to: Wire::decode(i)?,
            },
            4 => Self::Unclaimed {
                epoch: Wire::decode(i)?,
            },
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}

impl Wire for Target {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Handle(h) => {
                0u8.encode(o);
                h.encode(o);
            }
            Self::Pinned { logical, rev } => {
                1u8.encode(o);
                logical.encode(o);
                rev.encode(o);
            }
            Self::Logical(l) => {
                2u8.encode(o);
                l.encode(o);
            }
            Self::Key(k) => {
                3u8.encode(o);
                k.encode(o);
            }
            Self::KeyRev { key, rev } => {
                4u8.encode(o);
                key.encode(o);
                rev.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Handle(Wire::decode(i)?),
            1 => Self::Pinned {
                logical: Wire::decode(i)?,
                rev: Wire::decode(i)?,
            },
            2 => Self::Logical(Wire::decode(i)?),
            3 => Self::Key(Wire::decode(i)?),
            4 => Self::KeyRev {
                key: Wire::decode(i)?,
                rev: Wire::decode(i)?,
            },
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
impl Wire for Mode {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Certified),
            1 => Ok(Self::Advisory),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for WriteTarget {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Create { key, scope_lane } => {
                0u8.encode(o);
                key.encode(o);
                scope_lane.encode(o);
            }
            Self::Revise {
                logical,
                expected_head,
            } => {
                1u8.encode(o);
                logical.encode(o);
                expected_head.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Create {
                key: Wire::decode(i)?,
                scope_lane: Wire::decode(i)?,
            },
            1 => Self::Revise {
                logical: Wire::decode(i)?,
                expected_head: Wire::decode(i)?,
            },
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
impl Wire for Request {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::SessionStart {
                harness,
                repo,
                head_sha,
                parent,
                branch,
                git_branch,
            } => {
                0u8.encode(o);
                harness.encode(o);
                repo.encode(o);
                head_sha.encode(o);
                parent.encode(o);
                branch.encode(o);
                git_branch.encode(o);
            }
            Self::SessionEnd { session } => {
                1u8.encode(o);
                session.encode(o);
            }
            Self::Recall {
                binding,
                query,
                scope,
                mode,
                limit,
                index_wait_ms,
            } => {
                2u8.encode(o);
                binding.encode(o);
                query.encode(o);
                scope.encode(o);
                mode.encode(o);
                limit.encode(o);
                index_wait_ms.encode(o);
            }
            Self::Read {
                binding,
                target,
                mode,
            } => {
                3u8.encode(o);
                binding.encode(o);
                target.encode(o);
                mode.encode(o);
            }
            Self::Record {
                binding,
                kind,
                body,
                target,
                deps,
                adopt_as_base,
                client_txn_id,
            } => {
                4u8.encode(o);
                binding.encode(o);
                kind.encode(o);
                body.encode(o);
                target.encode(o);
                deps.encode(o);
                adopt_as_base.encode(o);
                client_txn_id.encode(o);
            }
            Self::Supersede {
                binding,
                logical,
                body,
                expected_head,
                adopt_as_base,
                carry_rules,
                reason,
                client_txn_id,
            } => {
                5u8.encode(o);
                binding.encode(o);
                logical.encode(o);
                body.encode(o);
                expected_head.encode(o);
                adopt_as_base.encode(o);
                carry_rules.encode(o);
                reason.encode(o);
                client_txn_id.encode(o);
            }
            Self::Why { target, depth } => {
                6u8.encode(o);
                target.encode(o);
                depth.encode(o);
            }
            Self::WhyNot { target, depth } => {
                7u8.encode(o);
                target.encode(o);
                depth.encode(o);
            }
            Self::Impact { target } => {
                8u8.encode(o);
                target.encode(o);
            }
            Self::ImpactBatch { batch } => {
                9u8.encode(o);
                batch.encode(o);
            }
            Self::Check {
                binding,
                cert_id,
                live_pos,
                action,
                mode,
                harness_version,
                strict,
            } => {
                10u8.encode(o);
                binding.encode(o);
                cert_id.encode(o);
                live_pos.encode(o);
                action.encode(o);
                mode.encode(o);
                harness_version.encode(o);
                strict.encode(o);
            }
            Self::Diff { from, to } => {
                11u8.encode(o);
                from.encode(o);
                to.encode(o);
            }
            Self::Status => 12u8.encode(o),
            Self::TxnStatus { id } => {
                13u8.encode(o);
                id.encode(o);
            }
            Self::Validate {
                rule,
                verdict,
                curvec_seen,
                validator,
                label,
                rationale,
                evidence,
                from_review,
            } => {
                14u8.encode(o);
                rule.encode(o);
                verdict.encode(o);
                curvec_seen.encode(o);
                validator.encode(o);
                label.encode(o);
                rationale.encode(o);
                evidence.encode(o);
                from_review.encode(o);
            }
            Self::Raw { blobs, batch } => {
                15u8.encode(o);
                blobs.encode(o);
                batch.encode(o);
            }
            Self::Shutdown => 16u8.encode(o),
            Self::SessionAttach {
                harness,
                harness_session,
                repo,
                head_sha,
                branch,
                git_branch,
            } => {
                17u8.encode(o);
                harness.encode(o);
                harness_session.encode(o);
                repo.encode(o);
                head_sha.encode(o);
                branch.encode(o);
                git_branch.encode(o);
            }
            Self::Claim {
                session,
                agent,
                call,
            } => {
                18u8.encode(o);
                session.encode(o);
                agent.encode(o);
                call.encode(o);
            }
            Self::Updates {
                binding,
                ack,
                max_items,
            } => {
                19u8.encode(o);
                binding.encode(o);
                ack.encode(o);
                max_items.encode(o);
            }
            Self::Facts {
                harness,
                harness_session,
                stream,
                packets,
                skip,
            } => {
                20u8.encode(o);
                harness.encode(o);
                harness_session.encode(o);
                stream.encode(o);
                packets.encode(o);
                skip.encode(o);
            }
            Self::Briefing {
                binding,
                budget_chars,
            } => {
                21u8.encode(o);
                binding.encode(o);
                budget_chars.encode(o);
            }
            Self::Telemetry { since, limit } => {
                22u8.encode(o);
                since.encode(o);
                limit.encode(o);
            }
            Self::AnchorState { scope, branch } => {
                23u8.encode(o);
                scope.encode(o);
                branch.encode(o);
            }
            Self::CheckAnchors {
                scope,
                paths,
                expected_head,
                expected_position,
            } => {
                24u8.encode(o);
                scope.encode(o);
                paths.encode(o);
                expected_head.encode(o);
                expected_position.encode(o);
            }
            Self::OnBranch { branch, request } => {
                25u8.encode(o);
                branch.encode(o);
                request.encode(o);
            }
            Self::Commit {
                binding,
                ops,
                blobs,
                expect_cert_valid,
                client_txn_id,
            } => {
                26u8.encode(o);
                binding.encode(o);
                ops.encode(o);
                blobs.encode(o);
                expect_cert_valid.encode(o);
                client_txn_id.encode(o);
            }
            Self::WhatIf { binding, ops } => {
                27u8.encode(o);
                binding.encode(o);
                ops.encode(o);
            }
            Self::BranchCreate {
                name,
                from,
                client_txn_id,
            } => {
                28u8.encode(o);
                name.encode(o);
                from.encode(o);
                client_txn_id.encode(o);
            }
            Self::BranchList => 29u8.encode(o),
            Self::BranchArchive {
                name,
                client_txn_id,
            } => {
                30u8.encode(o);
                name.encode(o);
                client_txn_id.encode(o);
            }
            Self::Merge {
                binding,
                source,
                into,
                require_certified,
                certify,
                client_txn_id,
            } => {
                31u8.encode(o);
                binding.encode(o);
                source.encode(o);
                into.encode(o);
                require_certified.encode(o);
                certify.encode(o);
                client_txn_id.encode(o);
            }
            Self::Validators { action } => {
                32u8.encode(o);
                action.encode(o);
            }
            Self::Conflicts {
                binding,
                branch,
                scope,
                state,
                limit,
            } => {
                33u8.encode(o);
                binding.encode(o);
                branch.encode(o);
                scope.encode(o);
                state.encode(o);
                limit.encode(o);
            }
            Self::ConflictOpen {
                binding,
                branch,
                a,
                b,
                reason,
                evidence,
                from,
                expect_cert_valid,
                client_txn_id,
            } => {
                34u8.encode(o);
                binding.encode(o);
                branch.encode(o);
                a.encode(o);
                b.encode(o);
                reason.encode(o);
                evidence.encode(o);
                from.encode(o);
                expect_cert_valid.encode(o);
                client_txn_id.encode(o);
            }
            Self::Resolve {
                binding,
                branch,
                artifacts,
                note,
                expect_cert_valid,
                client_txn_id,
            } => {
                35u8.encode(o);
                binding.encode(o);
                branch.encode(o);
                artifacts.encode(o);
                note.encode(o);
                expect_cert_valid.encode(o);
                client_txn_id.encode(o);
            }
            Self::HandoffBegin {
                binding,
                summary,
                to_agent,
            } => {
                36u8.encode(o);
                binding.encode(o);
                summary.encode(o);
                to_agent.encode(o);
            }
            Self::HandoffAccept { binding, id } => {
                37u8.encode(o);
                binding.encode(o);
                id.encode(o);
            }
            Self::Changes {
                binding,
                branch,
                since_epoch,
                scope,
                max_items,
                wait_ms,
            } => {
                38u8.encode(o);
                binding.encode(o);
                branch.encode(o);
                since_epoch.encode(o);
                scope.encode(o);
                max_items.encode(o);
                wait_ms.encode(o);
            }
            Self::Reindex { target } => {
                39u8.encode(o);
                target.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::SessionStart {
                harness: Wire::decode(i)?,
                repo: Wire::decode(i)?,
                head_sha: Wire::decode(i)?,
                parent: Wire::decode(i)?,
                branch: Wire::decode(i)?,
                git_branch: Wire::decode(i)?,
            },
            1 => Self::SessionEnd {
                session: Wire::decode(i)?,
            },
            2 => Self::Recall {
                binding: Wire::decode(i)?,
                query: Wire::decode(i)?,
                scope: Wire::decode(i)?,
                mode: Wire::decode(i)?,
                limit: Wire::decode(i)?,
                index_wait_ms: Wire::decode(i)?,
            },
            3 => Self::Read {
                binding: Wire::decode(i)?,
                target: Wire::decode(i)?,
                mode: Wire::decode(i)?,
            },
            4 => Self::Record {
                binding: Wire::decode(i)?,
                kind: Wire::decode(i)?,
                body: Wire::decode(i)?,
                target: Wire::decode(i)?,
                deps: Wire::decode(i)?,
                adopt_as_base: Wire::decode(i)?,
                client_txn_id: Wire::decode(i)?,
            },
            5 => Self::Supersede {
                binding: Wire::decode(i)?,
                logical: Wire::decode(i)?,
                body: Wire::decode(i)?,
                expected_head: Wire::decode(i)?,
                adopt_as_base: Wire::decode(i)?,
                carry_rules: Wire::decode(i)?,
                reason: Wire::decode(i)?,
                client_txn_id: Wire::decode(i)?,
            },
            6 => Self::Why {
                target: Wire::decode(i)?,
                depth: Wire::decode(i)?,
            },
            7 => Self::WhyNot {
                target: Wire::decode(i)?,
                depth: Wire::decode(i)?,
            },
            8 => Self::Impact {
                target: Wire::decode(i)?,
            },
            9 => Self::ImpactBatch {
                batch: Wire::decode(i)?,
            },
            10 => Self::Check {
                binding: Wire::decode(i)?,
                cert_id: Wire::decode(i)?,
                live_pos: Wire::decode(i)?,
                action: Wire::decode(i)?,
                mode: Wire::decode(i)?,
                harness_version: Wire::decode(i)?,
                strict: Wire::decode(i)?,
            },
            11 => Self::Diff {
                from: Wire::decode(i)?,
                to: Wire::decode(i)?,
            },
            12 => Self::Status,
            13 => Self::TxnStatus {
                id: Wire::decode(i)?,
            },
            14 => Self::Validate {
                rule: Wire::decode(i)?,
                verdict: Wire::decode(i)?,
                curvec_seen: Wire::decode(i)?,
                validator: Wire::decode(i)?,
                label: Wire::decode(i)?,
                rationale: Wire::decode(i)?,
                evidence: Wire::decode(i)?,
                from_review: Wire::decode(i)?,
            },
            15 => Self::Raw {
                blobs: Wire::decode(i)?,
                batch: Wire::decode(i)?,
            },
            16 => Self::Shutdown,
            17 => Self::SessionAttach {
                harness: Wire::decode(i)?,
                harness_session: Wire::decode(i)?,
                repo: Wire::decode(i)?,
                head_sha: Wire::decode(i)?,
                branch: Wire::decode(i)?,
                git_branch: Wire::decode(i)?,
            },
            18 => Self::Claim {
                session: Wire::decode(i)?,
                agent: Wire::decode(i)?,
                call: Wire::decode(i)?,
            },
            19 => Self::Updates {
                binding: Wire::decode(i)?,
                ack: Wire::decode(i)?,
                max_items: Wire::decode(i)?,
            },
            20 => Self::Facts {
                harness: Wire::decode(i)?,
                harness_session: Wire::decode(i)?,
                stream: Wire::decode(i)?,
                packets: Wire::decode(i)?,
                skip: Wire::decode(i)?,
            },
            21 => Self::Briefing {
                binding: Wire::decode(i)?,
                budget_chars: Wire::decode(i)?,
            },
            22 => Self::Telemetry {
                since: Wire::decode(i)?,
                limit: Wire::decode(i)?,
            },
            23 => Self::AnchorState {
                scope: Wire::decode(i)?,
                branch: Wire::decode(i)?,
            },
            24 => Self::CheckAnchors {
                scope: Wire::decode(i)?,
                paths: Wire::decode(i)?,
                expected_head: Wire::decode(i)?,
                expected_position: Wire::decode(i)?,
            },
            25 => {
                let branch = Wire::decode(i)?;
                let request = Request::decode(i)?;
                // One level only: no recursion budget to spend.
                if matches!(request, Request::OnBranch { .. }) {
                    return Err(WireError::InvalidEncoding);
                }
                Self::OnBranch {
                    branch,
                    request: Box::new(request),
                }
            }
            26 => Self::Commit {
                binding: Wire::decode(i)?,
                ops: Wire::decode(i)?,
                blobs: Wire::decode(i)?,
                expect_cert_valid: Wire::decode(i)?,
                client_txn_id: Wire::decode(i)?,
            },
            27 => Self::WhatIf {
                binding: Wire::decode(i)?,
                ops: Wire::decode(i)?,
            },
            28 => Self::BranchCreate {
                name: Wire::decode(i)?,
                from: Wire::decode(i)?,
                client_txn_id: Wire::decode(i)?,
            },
            29 => Self::BranchList,
            30 => Self::BranchArchive {
                name: Wire::decode(i)?,
                client_txn_id: Wire::decode(i)?,
            },
            31 => Self::Merge {
                binding: Wire::decode(i)?,
                source: Wire::decode(i)?,
                into: Wire::decode(i)?,
                require_certified: Wire::decode(i)?,
                certify: Wire::decode(i)?,
                client_txn_id: Wire::decode(i)?,
            },
            32 => Self::Validators {
                action: Wire::decode(i)?,
            },
            33 => Self::Conflicts {
                binding: Wire::decode(i)?,
                branch: Wire::decode(i)?,
                scope: Wire::decode(i)?,
                state: Wire::decode(i)?,
                limit: Wire::decode(i)?,
            },
            34 => Self::ConflictOpen {
                binding: Wire::decode(i)?,
                branch: Wire::decode(i)?,
                a: Wire::decode(i)?,
                b: Wire::decode(i)?,
                reason: Wire::decode(i)?,
                evidence: Wire::decode(i)?,
                from: Wire::decode(i)?,
                expect_cert_valid: Wire::decode(i)?,
                client_txn_id: Wire::decode(i)?,
            },
            35 => Self::Resolve {
                binding: Wire::decode(i)?,
                branch: Wire::decode(i)?,
                artifacts: Wire::decode(i)?,
                note: Wire::decode(i)?,
                expect_cert_valid: Wire::decode(i)?,
                client_txn_id: Wire::decode(i)?,
            },
            36 => Self::HandoffBegin {
                binding: Wire::decode(i)?,
                summary: Wire::decode(i)?,
                to_agent: Wire::decode(i)?,
            },
            37 => Self::HandoffAccept {
                binding: Wire::decode(i)?,
                id: Wire::decode(i)?,
            },
            38 => Self::Changes {
                binding: Wire::decode(i)?,
                branch: Wire::decode(i)?,
                since_epoch: Wire::decode(i)?,
                scope: Wire::decode(i)?,
                max_items: Wire::decode(i)?,
                wait_ms: Wire::decode(i)?,
            },
            39 => Self::Reindex {
                target: Wire::decode(i)?,
            },
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
impl Wire for Response {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Error { code, detail } => {
                0u8.encode(o);
                code.encode(o);
                detail.encode(o);
            }
            Self::Session { session } => {
                1u8.encode(o);
                session.encode(o);
            }
            Self::Done => 2u8.encode(o),
            Self::Recall {
                items,
                withheld,
                cert_id,
                index,
            } => {
                3u8.encode(o);
                items.encode(o);
                withheld.encode(o);
                cert_id.encode(o);
                index.encode(o);
            }
            Self::Read { item, payload } => {
                4u8.encode(o);
                item.encode(o);
                payload.encode(o);
            }
            Self::Committed { result } => {
                5u8.encode(o);
                result.encode(o);
            }
            Self::Why { proof } => {
                6u8.encode(o);
                proof.encode(o);
            }
            Self::WhyNot { why_not } => {
                7u8.encode(o);
                why_not.encode(o);
            }
            Self::Impact { impact } => {
                8u8.encode(o);
                impact.encode(o);
            }
            Self::ImpactExact { result } => {
                9u8.encode(o);
                result.encode(o);
            }
            Self::Check {
                decision,
                validation,
                cert,
                superseded,
                gate,
                updates_pending,
                named,
            } => {
                10u8.encode(o);
                decision.encode(o);
                validation.encode(o);
                cert.as_deref().cloned().encode(o);
                superseded.encode(o);
                gate.encode(o);
                updates_pending.encode(o);
                named.encode(o);
            }
            Self::Diff { diff } => {
                11u8.encode(o);
                diff.encode(o);
            }
            Self::Status {
                revisions,
                logicals,
                sessions,
                index,
            } => {
                12u8.encode(o);
                revisions.encode(o);
                logicals.encode(o);
                sessions.encode(o);
                index.encode(o);
            }
            Self::TxnStatus { committed } => {
                13u8.encode(o);
                committed.encode(o);
            }
            Self::Attached { session, created } => {
                14u8.encode(o);
                session.encode(o);
                created.encode(o);
            }
            Self::Updates { token, items, more } => {
                15u8.encode(o);
                token.encode(o);
                items.encode(o);
                more.encode(o);
            }
            Self::Facts {
                applied_through,
                drift,
            } => {
                16u8.encode(o);
                applied_through.encode(o);
                drift.encode(o);
            }
            Self::Gap { expected } => {
                17u8.encode(o);
                expected.encode(o);
            }
            Self::Briefing {
                stale,
                pending,
                conflicted,
                items,
            } => {
                18u8.encode(o);
                stale.encode(o);
                pending.encode(o);
                conflicted.encode(o);
                items.encode(o);
            }
            Self::Telemetry { entries, next } => {
                19u8.encode(o);
                entries.encode(o);
                next.encode(o);
            }
            Self::AnchorState {
                branch,
                watermark,
                anchors,
            } => {
                20u8.encode(o);
                branch.encode(o);
                watermark.encode(o);
                anchors.encode(o);
            }
            Self::WhatIf { result, merge } => {
                21u8.encode(o);
                result.encode(o);
                merge.encode(o);
            }
            Self::Branches { branches } => {
                22u8.encode(o);
                branches.encode(o);
            }
            Self::Merged { result, report } => {
                23u8.encode(o);
                result.encode(o);
                report.encode(o);
            }
            Self::Refused { validation } => {
                24u8.encode(o);
                validation.encode(o);
            }
            Self::Validators { report } => {
                25u8.encode(o);
                report.encode(o);
            }
            Self::Conflicts { items, truncated } => {
                26u8.encode(o);
                items.encode(o);
                truncated.encode(o);
            }
            Self::HandoffBegun { id, cert_id } => {
                27u8.encode(o);
                id.encode(o);
                cert_id.encode(o);
            }
            Self::HandoffAccepted {
                id,
                owner,
                owner_agent,
                summary,
                validation,
                copied,
                skipped,
            } => {
                28u8.encode(o);
                id.encode(o);
                owner.encode(o);
                owner_agent.encode(o);
                summary.encode(o);
                validation.encode(o);
                copied.encode(o);
                skipped.encode(o);
            }
            Self::Handoffs { items } => {
                29u8.encode(o);
                items.encode(o);
            }
            Self::Changes {
                items,
                reached,
                more,
            } => {
                30u8.encode(o);
                items.encode(o);
                reached.encode(o);
                more.encode(o);
            }
            Self::Truncated { earliest } => {
                31u8.encode(o);
                earliest.encode(o);
            }
            Self::Reindexed { index } => {
                32u8.encode(o);
                index.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Error {
                code: Wire::decode(i)?,
                detail: Wire::decode(i)?,
            },
            1 => Self::Session {
                session: Wire::decode(i)?,
            },
            2 => Self::Done,
            3 => Self::Recall {
                items: Wire::decode(i)?,
                withheld: Wire::decode(i)?,
                cert_id: Wire::decode(i)?,
                index: Wire::decode(i)?,
            },
            4 => Self::Read {
                item: Wire::decode(i)?,
                payload: Wire::decode(i)?,
            },
            5 => Self::Committed {
                result: Wire::decode(i)?,
            },
            6 => Self::Why {
                proof: Wire::decode(i)?,
            },
            7 => Self::WhyNot {
                why_not: Wire::decode(i)?,
            },
            8 => Self::Impact {
                impact: Wire::decode(i)?,
            },
            9 => Self::ImpactExact {
                result: Wire::decode(i)?,
            },
            10 => Self::Check {
                decision: Wire::decode(i)?,
                validation: Wire::decode(i)?,
                cert: Option::<Certificate>::decode(i)?.map(Box::new),
                superseded: Wire::decode(i)?,
                gate: Wire::decode(i)?,
                updates_pending: Wire::decode(i)?,
                named: Wire::decode(i)?,
            },
            11 => Self::Diff {
                diff: Wire::decode(i)?,
            },
            12 => Self::Status {
                revisions: Wire::decode(i)?,
                logicals: Wire::decode(i)?,
                sessions: Wire::decode(i)?,
                index: Wire::decode(i)?,
            },
            13 => Self::TxnStatus {
                committed: Wire::decode(i)?,
            },
            14 => Self::Attached {
                session: Wire::decode(i)?,
                created: Wire::decode(i)?,
            },
            15 => Self::Updates {
                token: Wire::decode(i)?,
                items: Wire::decode(i)?,
                more: Wire::decode(i)?,
            },
            16 => Self::Facts {
                applied_through: Wire::decode(i)?,
                drift: Wire::decode(i)?,
            },
            17 => Self::Gap {
                expected: Wire::decode(i)?,
            },
            18 => Self::Briefing {
                stale: Wire::decode(i)?,
                pending: Wire::decode(i)?,
                conflicted: Wire::decode(i)?,
                items: Wire::decode(i)?,
            },
            19 => Self::Telemetry {
                entries: Wire::decode(i)?,
                next: Wire::decode(i)?,
            },
            20 => Self::AnchorState {
                branch: Wire::decode(i)?,
                watermark: Wire::decode(i)?,
                anchors: Wire::decode(i)?,
            },
            21 => Self::WhatIf {
                result: Wire::decode(i)?,
                merge: Wire::decode(i)?,
            },
            22 => Self::Branches {
                branches: Wire::decode(i)?,
            },
            23 => Self::Merged {
                result: Wire::decode(i)?,
                report: Wire::decode(i)?,
            },
            24 => Self::Refused {
                validation: Wire::decode(i)?,
            },
            25 => Self::Validators {
                report: Wire::decode(i)?,
            },
            26 => Self::Conflicts {
                items: Wire::decode(i)?,
                truncated: Wire::decode(i)?,
            },
            27 => Self::HandoffBegun {
                id: Wire::decode(i)?,
                cert_id: Wire::decode(i)?,
            },
            28 => Self::HandoffAccepted {
                id: Wire::decode(i)?,
                owner: Wire::decode(i)?,
                owner_agent: Wire::decode(i)?,
                summary: Wire::decode(i)?,
                validation: Wire::decode(i)?,
                copied: Wire::decode(i)?,
                skipped: Wire::decode(i)?,
            },
            29 => Self::Handoffs {
                items: Wire::decode(i)?,
            },
            30 => Self::Changes {
                items: Wire::decode(i)?,
                reached: Wire::decode(i)?,
                more: Wire::decode(i)?,
            },
            31 => Self::Truncated {
                earliest: Wire::decode(i)?,
            },
            32 => Self::Reindexed {
                index: Wire::decode(i)?,
            },
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}

/// Framing errors: the connection is closed after any of these.
#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    TooLarge(u32),
    Version(u16),
    Kind(u16),
    Body(WireError),
}
impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
fn write_frame(w: &mut impl Write, kind: u16, body: &[u8]) -> io::Result<()> {
    let len = u32::try_from(body.len())
        .ok()
        .filter(|n| *n <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(body);
    w.write_all(&out)?;
    w.flush()
}
fn read_frame(r: &mut impl Read, kind: u16) -> Result<Vec<u8>, FrameError> {
    let mut head = [0u8; 8];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes(head[..4].try_into().expect("four bytes"));
    let version = u16::from_le_bytes(head[4..6].try_into().expect("two bytes"));
    let k = u16::from_le_bytes(head[6..8].try_into().expect("two bytes"));
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    if version != PROTOCOL_VERSION {
        return Err(FrameError::Version(version));
    }
    if k != kind {
        return Err(FrameError::Kind(k));
    }
    let mut body = vec![0; len as usize];
    r.read_exact(&mut body)?;
    Ok(body)
}
/// Pure decoding of a request body (the cargo-fuzz target, ADR 0008 §5).
pub fn decode_request(body: &[u8]) -> Result<Request, WireError> {
    let mut i = Decoder::new(body, DecodeLimits::IPC);
    let r = Request::decode(&mut i)?;
    if i.remaining() != 0 {
        return Err(WireError::TrailingBytes);
    }
    Ok(r)
}
pub fn write_request(w: &mut impl Write, r: &Request) -> io::Result<()> {
    write_frame(w, KIND_REQUEST, &r.canonical_bytes())
}
pub fn read_request(r: &mut impl Read) -> Result<Request, FrameError> {
    let body = read_frame(r, KIND_REQUEST)?;
    decode_request(&body).map_err(FrameError::Body)
}
pub fn write_response(w: &mut impl Write, e: &Envelope) -> io::Result<()> {
    write_frame(w, KIND_RESPONSE, &e.canonical_bytes())
}
pub fn read_response(r: &mut impl Read) -> Result<Envelope, FrameError> {
    let body = read_frame(r, KIND_RESPONSE)?;
    let mut i = Decoder::new(&body, DecodeLimits::IPC);
    let e = Envelope::decode(&mut i).map_err(FrameError::Body)?;
    if i.remaining() != 0 {
        return Err(FrameError::Body(WireError::TrailingBytes));
    }
    Ok(e)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn requests() -> Vec<Request> {
        let s = [3u8; 16];
        vec![
            Request::AnchorState {
                scope: "code:repo".into(),
                branch: Some("feature/x".into()),
            },
            Request::CheckAnchors {
                scope: "code:repo".into(),
                paths: vec!["a.rs".into()],
                expected_head: vec![1; 20],
                expected_position: vec![2; 32],
            },
            Request::SessionStart {
                harness: "claude".into(),
                repo: "r".into(),
                head_sha: "abc".into(),
                parent: Some([1; 16]),
                branch: None,
                git_branch: Some("main".into()),
            },
            Request::SessionEnd { session: s },
            Request::Recall {
                binding: Binding::root(s),
                query: "db".into(),
                scope: "repo".into(),
                mode: Mode::Certified,
                limit: 8,
                index_wait_ms: 200,
            },
            Request::Read {
                binding: Binding::Call {
                    id: "toolu_01".into(),
                },
                target: Target::Handle("0123456789ab.2".into()),
                mode: Mode::Advisory,
            },
            Request::Record {
                binding: Binding::Key {
                    harness: "codex".into(),
                    harness_session: "t1".into(),
                    agent: Some("t2".into()),
                },
                kind: RecordKind::Plan,
                body: b"plan".to_vec(),
                target: WriteTarget::Create {
                    key: "plan".into(),
                    scope_lane: "repo".into(),
                },
                deps: vec![Dep {
                    target: Target::Pinned {
                        logical: LogicalId(1),
                        rev: RevisionId(2),
                    },
                    kind: EdgeKind::Cur,
                }],
                adopt_as_base: false,
                client_txn_id: Some([9; 16]),
            },
            Request::Supersede {
                binding: Binding::Session {
                    session: s,
                    agent: Some("a".into()),
                },
                logical: LogicalId(1),
                body: vec![1, 2],
                expected_head: RevisionId(3),
                adopt_as_base: true,
                carry_rules: true,
                reason: "why".into(),
                client_txn_id: None,
            },
            Request::Why {
                target: Target::Logical(LogicalId(0)),
                depth: 3,
            },
            Request::WhyNot {
                target: Target::Key("db".into()),
                depth: 2,
            },
            Request::Impact {
                target: Target::Logical(LogicalId(4)),
            },
            Request::ImpactBatch {
                batch: Batch::new(vec![Op::RetractBase {
                    rev: RevisionId(0).into(),
                }]),
            },
            Request::Check {
                binding: Binding::root(s),
                cert_id: Some([5; 32]),
                live_pos: vec![("repo".into(), vec![1])],
                action: Some(Action {
                    tool: ToolClass::Patch,
                    paths: vec!["a.rs".into()],
                    command: None,
                }),
                mode: Some(GateMode::Enforce),
                harness_version: "0.163.0".into(),
                strict: false,
            },
            Request::Diff {
                from: Epoch(1),
                to: Epoch(4),
            },
            Request::Status,
            Request::TxnStatus { id: [7; 16] },
            Request::Validate {
                rule: RuleId(2),
                verdict: Verdict::Verified,
                curvec_seen: vec![(LogicalId(0), RevisionId(1))],
                validator: "human".into(),
                label: Some(crate::docs::Label::Reinforcement),
                rationale: "checked".into(),
                evidence: vec!["db@1".into()],
                from_review: Some(ArtifactId(3)),
            },
            Request::Raw {
                blobs: vec![b"x".to_vec()],
                batch: Batch::new(Vec::new()),
            },
            Request::Shutdown,
            Request::Validators {
                action: ValidatorsAction::Retry {
                    key: Some([4; 32]),
                    failed: false,
                },
            },
            Request::Validators {
                action: ValidatorsAction::Check,
            },
            Request::Conflicts {
                binding: None,
                branch: Some("dev".into()),
                scope: Some("repo".into()),
                state: ConflictState::Review,
                limit: 9,
            },
            Request::ConflictOpen {
                binding: Some(Binding::root([1; 16])),
                branch: None,
                a: Target::Key("a".into()),
                b: Some(Target::KeyRev {
                    key: "b".into(),
                    rev: RevisionId(3),
                }),
                reason: "contradicts".into(),
                evidence: vec!["a@1".into()],
                from: Some(ConflictFrom::Review(ArtifactId(2))),
                expect_cert_valid: true,
                client_txn_id: Some([3; 16]),
            },
            Request::Resolve {
                binding: None,
                branch: None,
                artifacts: vec![ArtifactId(1), ArtifactId(4)],
                note: "fixed".into(),
                expect_cert_valid: false,
                client_txn_id: None,
            },
            Request::HandoffBegin {
                binding: Binding::root([1; 16]),
                summary: "half done".into(),
                to_agent: Some("reviewer".into()),
            },
            Request::HandoffAccept {
                binding: Binding::root([2; 16]),
                id: Some([5; 16]),
            },
            Request::HandoffAccept {
                binding: Binding::root([2; 16]),
                id: None,
            },
            Request::Changes {
                binding: None,
                branch: Some("dev".into()),
                since_epoch: Epoch(4),
                scope: Some("repo".into()),
                max_items: 50,
                wait_ms: 30_000,
            },
            Request::Reindex {
                target: ReindexTarget::All,
            },
            Request::SessionAttach {
                harness: "claude".into(),
                harness_session: "abc".into(),
                repo: "r".into(),
                head_sha: "h".into(),
                branch: Some("b".into()),
                git_branch: None,
            },
            Request::Claim {
                session: s,
                agent: None,
                call: "toolu_02".into(),
            },
            Request::Updates {
                binding: Binding::root(s),
                ack: Some(4),
                max_items: 20,
            },
            Request::Facts {
                harness: "codex".into(),
                harness_session: "t1".into(),
                stream: [8; 16],
                packets: vec![Packet {
                    seq: 1,
                    agent: None,
                    fact: Fact::StreamEnd,
                }],
                skip: Some((2, 5)),
            },
            Request::Briefing {
                binding: Some(Binding::root(s)),
                budget_chars: 1500,
            },
            Request::Telemetry {
                since: 3,
                limit: 100,
            },
            Request::OnBranch {
                branch: BranchSel::Named("feature/x".into()),
                request: Box::new(Request::Status),
            },
            Request::OnBranch {
                branch: BranchSel::Session(Binding::root(s)),
                request: Box::new(Request::Diff {
                    from: Epoch(1),
                    to: Epoch(2),
                }),
            },
            Request::Commit {
                binding: Binding::root(s),
                ops: vec![Op::AssertBase {
                    rev: RevisionId(1).into(),
                }],
                blobs: vec![b"x".to_vec()],
                expect_cert_valid: true,
                client_txn_id: Some([3; 16]),
            },
            Request::WhatIf {
                binding: Binding::root(s),
                ops: Vec::new(),
            },
            Request::BranchCreate {
                name: "b".into(),
                from: Some("main".into()),
                client_txn_id: Some([3; 16]),
            },
            Request::BranchList,
            Request::BranchArchive {
                name: "b".into(),
                client_txn_id: None,
            },
            Request::Merge {
                binding: None,
                source: "b".into(),
                into: Some("main".into()),
                require_certified: true,
                certify: vec!["plan".into()],
                client_txn_id: Some([4; 16]),
            },
        ]
    }
    fn responses() -> Vec<Response> {
        vec![
            Response::AnchorState {
                branch: BranchId(2),
                watermark: Some(vec![1; 32]),
                anchors: vec![AnchorEntry {
                    logical: LogicalId(1),
                    key: "code:repo/a.rs".into(),
                    head: RevisionId(2),
                    content: [3; 32],
                    base: true,
                    breaks: vec![ArtifactId(4)],
                    descriptor: vec![5],
                }],
            },
            Response::Check {
                decision: Decision::Warn,
                validation: Validation::Empty,
                cert: None,
                superseded: Vec::new(),
                gate: Some(Decision::Deny),
                updates_pending: 2,
                named: vec![NamedRead {
                    key: "db".into(),
                    agent: Some("c1".into()),
                    read: RevisionId(1),
                    current: Some(RevisionId(2)),
                    summary: Summary::Certified,
                }],
            },
            Response::Attached {
                session: [1; 16],
                created: true,
            },
            Response::Updates {
                token: 7,
                items: vec![UpdateItem {
                    logical: LogicalId(1),
                    key: "db".into(),
                    cert_rev: RevisionId(2),
                    current: Some(RevisionId(3)),
                    summary: Summary::Stale(StaleReason::HeadSuperseded),
                }],
                more: false,
            },
            Response::Facts {
                applied_through: 9,
                drift: vec![DriftReport::default()],
            },
            Response::Gap { expected: 4 },
            Response::Briefing {
                stale: 1,
                pending: 2,
                conflicted: 0,
                items: Vec::new(),
            },
            Response::Telemetry {
                entries: vec![
                    TelemetryEntry::Skip {
                        stream: [0; 16],
                        from: 1,
                        to: 2,
                    },
                    TelemetryEntry::Unclaimed { epoch: Epoch(3) },
                    TelemetryEntry::Write {
                        session: [2; 16],
                        agent: None,
                        with_deps: true,
                        epoch: Epoch(4),
                    },
                ],
                next: 3,
            },
            Response::WhatIf {
                result: CommitResult {
                    epoch: Epoch(2),
                    allocations: Allocations::default(),
                    net_delta: NetDelta::default(),
                    l0_flips: Vec::new(),
                    status_diff: vec![RevisionId(1)],
                },
                merge: Some(MergeReport::default()),
            },
            Response::Branches {
                branches: vec![BranchEntry {
                    id: BranchId(1),
                    name: "b".into(),
                    parent: Some(MAIN),
                    fork_epoch: Epoch(3),
                    archived: false,
                }],
            },
            Response::Merged {
                result: CommitResult {
                    epoch: Epoch(4),
                    allocations: Allocations::default(),
                    net_delta: NetDelta::default(),
                    l0_flips: Vec::new(),
                    status_diff: Vec::new(),
                },
                report: MergeReport {
                    source: BranchId(1),
                    convergent: 2,
                    ..MergeReport::default()
                },
            },
            Response::Refused {
                validation: Validation::Invalid {
                    reason: InvalidReason::BranchMismatch,
                    diffs: Vec::new(),
                },
            },
            Response::Conflicts {
                items: vec![ConflictItem {
                    kind: "conflict".into(),
                    artifacts: vec![ArtifactId(0), ArtifactId(1)],
                    sides: vec![ConflictSide {
                        rev: RevisionId(2),
                        key: "a".into(),
                        current: false,
                    }],
                    reason: "r".into(),
                    origin: "agent".into(),
                    rule: Some(RuleId(1)),
                    marks: vec!["side superseded".into()],
                    evidence: vec!["a@2".into()],
                    candidate: Some(4),
                }],
                truncated: true,
            },
            Response::HandoffBegun {
                id: [5; 16],
                cert_id: [6; 32],
            },
            Response::HandoffAccepted {
                id: [5; 16],
                owner: [1; 16],
                owner_agent: Some("a".into()),
                summary: "s".into(),
                validation: Validation::Valid,
                copied: 3,
                skipped: 1,
            },
            Response::Changes {
                items: vec![ChangeItem {
                    epoch: Epoch(5),
                    logical: LogicalId(1),
                    rev: RevisionId(2),
                    key: "k".into(),
                    summary: Summary::Certified,
                }],
                reached: Epoch(7),
                more: true,
            },
            Response::Truncated { earliest: Epoch(9) },
            Response::Reindexed {
                index: IndexReport {
                    fts: IndexState::Rebuilding { done: 3, total: 9 },
                    vectors: IndexState::Disabled,
                },
            },
            Response::Status {
                revisions: 4,
                logicals: 3,
                sessions: 1,
                index: IndexReport {
                    fts: IndexState::Lagging {
                        watermark: Epoch(2),
                        head_epoch: Epoch(5),
                    },
                    vectors: IndexState::Failed {
                        error: "disk full".into(),
                    },
                },
            },
            Response::Recall {
                items: Vec::new(),
                withheld: Vec::new(),
                cert_id: Some([7; 32]),
                index: IndexReport {
                    fts: IndexState::Ready {
                        watermark: Epoch(5),
                    },
                    vectors: IndexState::Disabled,
                },
            },
            Response::Handoffs {
                items: vec![HandoffInfo {
                    id: [5; 16],
                    owner: [1; 16],
                    owner_agent: None,
                    to_agent: Some("b".into()),
                    summary: "s".into(),
                    epoch: Epoch(4),
                    reads: 2,
                }],
            },
            Response::Validators {
                report: ValidatorsReport {
                    retried: 2,
                    check: vec!["ok".into()],
                    ..ValidatorsReport::default()
                },
            },
        ]
    }
    /// `OnBranch` nests one level only.
    #[test]
    fn on_branch_does_not_nest() {
        let nested = Request::OnBranch {
            branch: BranchSel::Named("a".into()),
            request: Box::new(Request::OnBranch {
                branch: BranchSel::Named("b".into()),
                request: Box::new(Request::Status),
            }),
        };
        assert_eq!(
            Request::from_bytes(&nested.canonical_bytes()),
            Err(WireError::InvalidEncoding)
        );
    }
    #[test]
    fn new_responses_round_trip() {
        for r in responses() {
            let env = Envelope {
                epoch: Epoch(1),
                response: r,
            };
            let mut buf = Vec::new();
            write_response(&mut buf, &env).unwrap();
            assert_eq!(read_response(&mut buf.as_slice()).unwrap(), env);
        }
    }
    #[test]
    fn every_request_round_trips_through_a_frame() {
        for r in requests() {
            let mut buf = Vec::new();
            write_request(&mut buf, &r).unwrap();
            assert_eq!(read_request(&mut buf.as_slice()).unwrap(), r);
        }
    }
    #[test]
    fn framing_rejects_version_kind_size_and_trailing_bytes() {
        let mut buf = Vec::new();
        write_request(&mut buf, &Request::Status).unwrap();
        let mut bad = buf.clone();
        bad[4] = 9;
        assert!(matches!(
            read_request(&mut bad.as_slice()),
            Err(FrameError::Version(9))
        ));
        let mut bad = buf.clone();
        bad[6] = KIND_RESPONSE as u8;
        assert!(matches!(
            read_request(&mut bad.as_slice()),
            Err(FrameError::Kind(_))
        ));
        let mut bad = buf.clone();
        bad[..4].copy_from_slice(&(MAX_FRAME + 1).to_le_bytes());
        assert!(matches!(
            read_request(&mut bad.as_slice()),
            Err(FrameError::TooLarge(_))
        ));
        let mut body = Request::Status.canonical_bytes();
        body.push(0);
        assert_eq!(decode_request(&body), Err(WireError::TrailingBytes));
    }
    #[test]
    fn decoding_garbage_never_panics() {
        // A cheap in-tree stand-in for the cargo-fuzz target: every prefix and
        // every single-byte corruption of valid requests decodes or errors.
        for r in requests() {
            let body = r.canonical_bytes();
            for n in 0..body.len() {
                let _ = decode_request(&body[..n]);
                for v in [0u8, 0x7F, 0xFF] {
                    let mut b = body.clone();
                    b[n] = v;
                    let _ = decode_request(&b);
                }
            }
        }
    }
}
