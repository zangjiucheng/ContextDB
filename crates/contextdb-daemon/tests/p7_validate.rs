//! P7 milestone 2 at the service layer (ADR 0015 §4.2, §4.3): an agent can
//! never write a ValidationRecord (T80); admin `Validate` stores a
//! `ValidationMeta` blob and can convert an open PendingReview in the same
//! batch.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_proto::docs::{Label, ValidationMeta, ValidatorKind};
use contextdb_store::{OpenOptions, Payload, SimDisk, Store, session::SessionLog};
use std::path::Path;

const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
fn service(disk: &SimDisk) -> Service<SimDisk> {
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    let actor = actor::spawn(store).unwrap();
    Service::new(actor, log, events).unwrap()
}
fn call(s: &Service<SimDisk>, id: Identity, req: Request) -> Response {
    s.handle(id, req).response
}
fn code(r: &Response) -> &str {
    match r {
        Response::Error { code, .. } => code,
        _ => "ok",
    }
}
fn raw(s: &Service<SimDisk>, blobs: Vec<Vec<u8>>, ops: Vec<Op>) -> CommitResult {
    match call(
        s,
        ADMIN,
        Request::Raw {
            blobs,
            batch: Batch::new(ops),
        },
    ) {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
fn create(key: &str, body: &[u8]) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: *blake3::hash(body).as_bytes(),
        adopt_as_base: true,
    }
}
/// a = r0, b = r1 ← Sem a@r0; a moves to r2: rule 0 is Pending at
/// [(a, r2)].
fn setup(s: &Service<SimDisk>) -> HeadVector {
    raw(
        s,
        vec![b"a0".to_vec(), b"b".to_vec()],
        vec![create("a", b"a0"), create("b", b"b")],
    );
    raw(
        s,
        Vec::new(),
        vec![Op::AddRule {
            conclusion: RevisionId(1).into(),
            incidences: vec![(RevisionId(0).into(), EdgeKind::Sem)],
            enabled: true,
        }],
    );
    raw(
        s,
        vec![b"a1".to_vec()],
        vec![
            Op::AddRevision {
                logical: LogicalId(0).into(),
                content: *blake3::hash(b"a1").as_bytes(),
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: LogicalId(0).into(),
                expected: RevisionId(0).into(),
                new: RevRef::New(0),
            },
        ],
    );
    vec![(LogicalId(0), RevisionId(2))]
}
fn start(s: &Service<SimDisk>) -> SessionId {
    match call(
        s,
        AGENT,
        Request::SessionStart {
            harness: "test".into(),
            repo: "r".into(),
            head_sha: "h".into(),
            parent: None,
            branch: None,
            git_branch: None,
        },
    ) {
        Response::Session { session } => session,
        other => panic!("{other:?}"),
    }
}
fn validate(curvec: &HeadVector, from_review: Option<ArtifactId>) -> Request {
    Request::Validate {
        rule: RuleId(0),
        verdict: Verdict::Verified,
        curvec_seen: curvec.clone(),
        validator: "maint-agent".into(),
        label: Some(Label::Reinforcement),
        rationale: "re-read both revisions".into(),
        evidence: vec!["a@2".into()],
        from_review,
    }
}

/// T80: Commit, WhatIf (and so MCP `ctx_commit`) and Raw from the agent
/// channel never carry a RecordValidation, whatever its verdict.
#[test]
fn t80_agent_batches_with_record_validation_are_refused() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let curvec = setup(&s);
    let session = start(&s);
    for verdict in [Verdict::Verified, Verdict::Rejected, Verdict::Pending] {
        let op = Op::RecordValidation {
            rule: RuleId(0).into(),
            curvec_seen: curvec.clone(),
            verdict,
            validator: "agent".into(),
            meta: None,
            if_absent: false,
        };
        let commit = Request::Commit {
            binding: Binding::root(session),
            ops: vec![op.clone()],
            blobs: Vec::new(),
            expect_cert_valid: false,
            client_txn_id: None,
        };
        assert_eq!(code(&call(&s, AGENT, commit)), "ChannelRestricted");
        let what_if = Request::WhatIf {
            binding: Binding::root(session),
            ops: vec![op.clone()],
        };
        assert_eq!(code(&call(&s, AGENT, what_if)), "ChannelRestricted");
        let raw = Request::Raw {
            blobs: Vec::new(),
            batch: Batch::new(vec![op]),
        };
        assert_eq!(code(&call(&s, AGENT, raw)), "ChannelRestricted");
    }
    let snap = s.actor.snapshot();
    assert!(snap.db().inputs().validations.is_empty());
    // An agent Commit without one still works.
    let ok = Request::Commit {
        binding: Binding::root(session),
        ops: vec![create("c", b"c")],
        blobs: vec![b"c".to_vec()],
        expect_cert_valid: false,
        client_txn_id: None,
    };
    assert_eq!(code(&call(&s, AGENT, ok)), "ok");
}

/// Admin Validate stores a ValidationMeta blob (kind Admin, with the label,
/// rationale and evidence) and names it in the record.
#[test]
fn admin_validate_stores_metadata() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let curvec = setup(&s);
    assert_eq!(code(&call(&s, ADMIN, validate(&curvec, None))), "ok");
    let snap = s.actor.snapshot();
    let record = snap.db().inputs().validations[&(RuleId(0), curvec)].clone();
    assert_eq!(record.verdict, Verdict::Verified);
    let Payload::Available(bytes) = snap.log.read_blob(&record.meta.unwrap()) else {
        panic!("metadata blob missing");
    };
    let meta = ValidationMeta::from_bytes(&bytes).unwrap();
    assert_eq!(meta.kind, ValidatorKind::Admin);
    assert_eq!(meta.validator, "maint-agent");
    assert_eq!(meta.label, Label::Reinforcement);
    assert_eq!(meta.evidence, ["a@2"]);
    assert_eq!(meta.based_on, None);
    assert_eq!(snap.db().status(RevisionId(1)).unwrap(), Summary::Certified);
}

/// `from_review` converts an open PendingReview: the record names it in
/// `based_on` and the review is resolved in the same batch. Anything other
/// than an open PendingReview is refused and nothing is written.
#[test]
fn validate_from_review_converts_and_resolves_it() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let curvec = setup(&s);
    let opened = raw(
        &s,
        Vec::new(),
        vec![
            Op::OpenArtifact {
                ty: ArtifactType::PendingReview,
                target: RevisionId(1).into(),
                payload_hash: [5; 32],
            },
            Op::OpenArtifact {
                ty: ArtifactType::Conflict,
                target: RevisionId(0).into(),
                payload_hash: [6; 32],
            },
        ],
    );
    let (review, conflict) = (
        opened.allocations.new_artifacts[0],
        opened.allocations.new_artifacts[1],
    );
    assert_eq!(
        code(&call(&s, ADMIN, validate(&curvec, Some(conflict)))),
        "InvalidArgument"
    );
    assert_eq!(
        code(&call(&s, ADMIN, validate(&curvec, Some(ArtifactId(99))))),
        "UnknownRef"
    );
    assert!(s.actor.snapshot().db().inputs().validations.is_empty());
    assert_eq!(
        code(&call(&s, ADMIN, validate(&curvec, Some(review)))),
        "ok"
    );
    let snap = s.actor.snapshot();
    assert!(
        !snap.db().inputs().artifacts[review.index()].open,
        "resolved"
    );
    let record = snap.db().inputs().validations[&(RuleId(0), curvec.clone())].clone();
    let Payload::Available(bytes) = snap.log.read_blob(&record.meta.unwrap()) else {
        panic!("metadata blob missing");
    };
    assert_eq!(
        ValidationMeta::from_bytes(&bytes).unwrap().based_on,
        Some(review)
    );
    // The review is no longer open: converting it again is refused.
    assert_eq!(
        code(&call(&s, ADMIN, validate(&curvec, Some(review)))),
        "InvalidArgument"
    );
}
