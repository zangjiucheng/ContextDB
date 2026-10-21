//! IPC protocol (ADR 0008 §5): length-prefixed, versioned frames whose body
//! is the Wire encoding of a `Request` or an `Envelope`. Requests carry no
//! channel or principal: identity comes from the socket and peer credentials.
use contextdb_core::{EpochDiff, Impact, Proof};
use contextdb_model::*;
use std::io::{self, Read, Write};

pub const PROTOCOL_VERSION: u16 = 1;
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
        session: SessionId,
        query: String,
        scope: String,
        mode: Mode,
        limit: u32,
    },
    Read {
        session: SessionId,
        target: Target,
        mode: Mode,
    },
    Record {
        session: SessionId,
        kind: RecordKind,
        body: Vec<u8>,
        target: WriteTarget,
        deps: Vec<Dep>,
        adopt_as_base: bool,
        client_txn_id: Option<[u8; 16]>,
    },
    Supersede {
        session: SessionId,
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
    Check {
        session: SessionId,
        cert_id: Option<ContentHash>,
        live_pos: Vec<(String, Vec<u8>)>,
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
    /// Observer and admin only; kernel channel rules still apply.
    Raw {
        batch: Batch,
    },
    /// Admin only.
    Shutdown,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Warn,
    Deny,
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
impl Wire for Decision {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Allow),
            1 => Ok(Self::Warn),
            2 => Ok(Self::Deny),
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
                session,
                query,
                scope,
                mode,
                limit,
            } => {
                2u8.encode(o);
                session.encode(o);
                query.encode(o);
                scope.encode(o);
                mode.encode(o);
                limit.encode(o);
            }
            Self::Read {
                session,
                target,
                mode,
            } => {
                3u8.encode(o);
                session.encode(o);
                target.encode(o);
                mode.encode(o);
            }
            Self::Record {
                session,
                kind,
                body,
                target,
                deps,
                adopt_as_base,
                client_txn_id,
            } => {
                4u8.encode(o);
                session.encode(o);
                kind.encode(o);
                body.encode(o);
                target.encode(o);
                deps.encode(o);
                adopt_as_base.encode(o);
                client_txn_id.encode(o);
            }
            Self::Supersede {
                session,
                logical,
                body,
                expected_head,
                adopt_as_base,
                carry_rules,
                reason,
                client_txn_id,
            } => {
                5u8.encode(o);
                session.encode(o);
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
                session,
                cert_id,
                live_pos,
            } => {
                10u8.encode(o);
                session.encode(o);
                cert_id.encode(o);
                live_pos.encode(o);
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
            Self::Raw { batch } => {
                15u8.encode(o);
                batch.encode(o);
            }
            Self::Shutdown => 16u8.encode(o),
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
                session: Wire::decode(i)?,
                query: Wire::decode(i)?,
                scope: Wire::decode(i)?,
                mode: Wire::decode(i)?,
                limit: Wire::decode(i)?,
            },
            3 => Self::Read {
                session: Wire::decode(i)?,
                target: Wire::decode(i)?,
                mode: Wire::decode(i)?,
            },
            4 => Self::Record {
                session: Wire::decode(i)?,
                kind: Wire::decode(i)?,
                body: Wire::decode(i)?,
                target: Wire::decode(i)?,
                deps: Wire::decode(i)?,
                adopt_as_base: Wire::decode(i)?,
                client_txn_id: Wire::decode(i)?,
            },
            5 => Self::Supersede {
                session: Wire::decode(i)?,
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
                session: Wire::decode(i)?,
                cert_id: Wire::decode(i)?,
                live_pos: Wire::decode(i)?,
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
                batch: Wire::decode(i)?,
            },
            16 => Self::Shutdown,
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
            } => {
                10u8.encode(o);
                decision.encode(o);
                validation.encode(o);
                cert.as_deref().cloned().encode(o);
                superseded.encode(o);
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
                session: s,
                query: "db".into(),
                scope: "repo".into(),
                mode: Mode::Certified,
                limit: 8,
            },
            Request::Read {
                session: s,
                target: Target::Handle("0123456789ab.2".into()),
                mode: Mode::Advisory,
            },
            Request::Record {
                session: s,
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
                session: s,
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
                session: s,
                cert_id: Some([5; 32]),
                live_pos: vec![("repo".into(), vec![1])],
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
                batch: Batch::new(Vec::new()),
            },
            Request::Shutdown,
        ]
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
