//! IPC protocol (ADR 0008 §5, ADR 0010 §4): length-prefixed, versioned frames
//! whose body is the Wire encoding of a `Request` or an `Envelope`. Requests
//! carry no channel or principal: identity comes from the socket and peer
//! credentials. Version 2 (P4) binds session-scoped requests through
//! [`Binding`] and adds the hook requests.
use contextdb_core::{EpochDiff, Impact, Proof};
use contextdb_model::*;
use std::io::{self, Read, Write};

pub const PROTOCOL_VERSION: u16 = 2;
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
    SessionStart {
        harness: String,
        repo: String,
        head_sha: String,
        parent: Option<SessionId>,
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
    /// Admin only (ADR 0008 §0.4).
    Validate {
        rule: RuleId,
        verdict: Verdict,
        curvec_seen: HeadVector,
        validator: String,
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
}
/// Every response names the snapshot epoch it was computed on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub epoch: Epoch,
    pub response: Response,
}

wire_struct!(Dep { target, kind });
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
            } => {
                0u8.encode(o);
                harness.encode(o);
                repo.encode(o);
                head_sha.encode(o);
                parent.encode(o);
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
            } => {
                2u8.encode(o);
                binding.encode(o);
                query.encode(o);
                scope.encode(o);
                mode.encode(o);
                limit.encode(o);
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
            } => {
                14u8.encode(o);
                rule.encode(o);
                verdict.encode(o);
                curvec_seen.encode(o);
                validator.encode(o);
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
            } => {
                17u8.encode(o);
                harness.encode(o);
                harness_session.encode(o);
                repo.encode(o);
                head_sha.encode(o);
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
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::SessionStart {
                harness: Wire::decode(i)?,
                repo: Wire::decode(i)?,
                head_sha: Wire::decode(i)?,
                parent: Wire::decode(i)?,
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
            } => {
                3u8.encode(o);
                items.encode(o);
                withheld.encode(o);
                cert_id.encode(o);
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
            } => {
                12u8.encode(o);
                revisions.encode(o);
                logicals.encode(o);
                sessions.encode(o);
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
            Request::SessionStart {
                harness: "claude".into(),
                repo: "r".into(),
                head_sha: "abc".into(),
                parent: Some([1; 16]),
            },
            Request::SessionEnd { session: s },
            Request::Recall {
                binding: Binding::root(s),
                query: "db".into(),
                scope: "repo".into(),
                mode: Mode::Certified,
                limit: 8,
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
            },
            Request::Raw {
                blobs: vec![b"x".to_vec()],
                batch: Batch::new(Vec::new()),
            },
            Request::Shutdown,
            Request::SessionAttach {
                harness: "claude".into(),
                harness_session: "abc".into(),
                repo: "r".into(),
                head_sha: "h".into(),
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
        ]
    }
    fn responses() -> Vec<Response> {
        vec![
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
        ]
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
