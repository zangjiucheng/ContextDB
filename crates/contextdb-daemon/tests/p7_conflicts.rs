//! P7 milestone 5 (ADR 0015 §2, §6; SD20): `ctx_conflicts` and
//! `ctx_resolve`. T78: a Conflict pair, one side superseded, per branch, and
//! the refusals of `open`.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_proto::docs::{
    ConflictDoc, ConflictSource, Label, ResolutionDoc, ReviewDoc, Suggestion, ValidationMeta,
    ValidatorKind,
};
use contextdb_store::{OpenOptions, Payload, SimDisk, Store, session::SessionLog};
use std::path::Path;

const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
const OBSERVER: Identity = Identity {
    channel: Channel::Observer,
    principal: PrincipalId(2),
};
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
type Svc = Service<SimDisk>;
fn service(disk: &SimDisk) -> Svc {
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    let actor = actor::spawn(store).unwrap();
    Service::new(actor, log, events).unwrap()
}
fn call(s: &Svc, id: Identity, req: Request) -> Response {
    s.handle(id, req).response
}
fn code(r: &Response) -> &str {
    match r {
        Response::Error { code, .. } => code,
        Response::Refused { .. } => "Refused",
        _ => "ok",
    }
}
fn h(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}
#[allow(clippy::result_large_err)]
fn raw_as(
    s: &Svc,
    id: Identity,
    branch: BranchId,
    blobs: Vec<Vec<u8>>,
    ops: Vec<Op>,
) -> Result<CommitResult, Response> {
    let mut batch = Batch::new(ops);
    batch.branch = branch;
    match call(s, id, Request::Raw { blobs, batch }) {
        Response::Committed { result } => Ok(result),
        other => Err(other),
    }
}
fn raw(s: &Svc, blobs: Vec<Vec<u8>>, ops: Vec<Op>) -> CommitResult {
    raw_as(s, ADMIN, MAIN, blobs, ops).unwrap()
}
fn create(key: &str, kind: RecordKind, body: &[u8]) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind,
        scope_lane: "repo".into(),
        content: h(body),
        adopt_as_base: true,
    }
}
/// a = r0 (Fact), b = r1 (Decision), both base, CERTIFIED.
fn setup(s: &Svc) {
    raw(
        s,
        vec![
            b"orders use postgres".to_vec(),
            b"orders on mongodb".to_vec(),
        ],
        vec![
            create("a", RecordKind::Fact, b"orders use postgres"),
            create("b", RecordKind::Decision, b"orders on mongodb"),
        ],
    );
}
fn start(s: &Svc) -> SessionId {
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
fn key_rev(key: &str, rev: u32) -> Target {
    Target::KeyRev {
        key: key.into(),
        rev: RevisionId(rev),
    }
}
fn open(binding: Option<Binding>, branch: Option<&str>, a: Target, b: Option<Target>) -> Request {
    Request::ConflictOpen {
        binding,
        branch: branch.map(str::to_owned),
        a,
        b,
        reason: "a and b cannot both hold".into(),
        evidence: vec!["a@0".into(), "b@1".into()],
        from: None,
        expect_cert_valid: false,
        client_txn_id: None,
    }
}
fn list(
    s: &Svc,
    id: Identity,
    binding: Option<Binding>,
    branch: Option<&str>,
    state: ConflictState,
) -> Vec<ConflictItem> {
    match call(
        s,
        id,
        Request::Conflicts {
            binding,
            branch: branch.map(str::to_owned),
            scope: None,
            state,
            limit: 0,
        },
    ) {
        Response::Conflicts { items, .. } => items,
        other => panic!("{other:?}"),
    }
}
fn resolve(binding: Option<Binding>, artifacts: Vec<ArtifactId>, note: &str) -> Request {
    Request::Resolve {
        binding,
        branch: None,
        artifacts,
        note: note.into(),
        expect_cert_valid: false,
        client_txn_id: None,
    }
}
fn status(s: &Svc, branch: BranchId, r: RevisionId) -> Summary {
    s.actor.snapshot().view(branch).status(r).unwrap()
}
fn committed(r: Response) -> CommitResult {
    match r {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}

/// T78: an agent opens the pair; both sides read CONFLICTED. Superseding
/// `a` frees the new head while `b` stays CONFLICTED and the list marks the
/// pair `side superseded`; resolving both, with a note, frees `b`.
#[test]
fn t78_pair_one_side_superseded_then_resolved() {
    let disk = SimDisk::new();
    let s = service(&disk);
    setup(&s);
    let session = start(&s);
    let bind = || Some(Binding::root(session));
    let r = committed(call(
        &s,
        AGENT,
        open(bind(), None, key_rev("a", 0), Some(key_rev("b", 1))),
    ));
    let pair = r.allocations.new_artifacts.clone();
    assert_eq!(pair.len(), 2);
    assert_eq!(status(&s, MAIN, RevisionId(0)), Summary::Conflicted);
    assert_eq!(status(&s, MAIN, RevisionId(1)), Summary::Conflicted);
    // One shared ConflictDoc, from the agent.
    let snap = s.actor.snapshot();
    let view = snap.view(MAIN);
    let (x, y) = (
        view.artifact(pair[0]).unwrap(),
        view.artifact(pair[1]).unwrap(),
    );
    assert_eq!(x.payload_hash, y.payload_hash);
    assert_eq!((x.target, y.target), (RevisionId(0), RevisionId(1)));
    let Payload::Available(bytes) = snap.log.read_blob(&x.payload_hash) else {
        panic!("conflict doc")
    };
    let doc = ConflictDoc::from_bytes(&bytes).unwrap();
    assert_eq!(doc.sides, vec![RevisionId(0), RevisionId(1)]);
    assert_eq!(doc.source, ConflictSource::Agent);
    drop(snap);
    let items = list(&s, AGENT, bind(), None, ConflictState::Open);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].artifacts, pair);
    assert_eq!(items[0].origin, "agent");
    assert!(items[0].marks.is_empty());
    assert_eq!(
        (
            items[0].sides[0].key.as_str(),
            items[0].sides[1].key.as_str()
        ),
        ("a", "b")
    );

    // Supersede a: a' is not gated, b stays CONFLICTED.
    let r = raw(
        &s,
        vec![b"orders use postgres 17".to_vec()],
        vec![
            Op::AddRevision {
                logical: LogicalId(0).into(),
                content: h(b"orders use postgres 17"),
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: LogicalId(0).into(),
                expected: RevisionId(0).into(),
                new: RevRef::New(0),
            },
        ],
    );
    let a2 = r.allocations.new_revisions[0];
    assert_eq!(status(&s, MAIN, a2), Summary::Certified);
    assert_eq!(status(&s, MAIN, RevisionId(1)), Summary::Conflicted);
    let items = list(&s, AGENT, bind(), None, ConflictState::Open);
    assert_eq!(items[0].marks, vec!["side superseded".to_owned()]);
    assert!(!items[0].sides[0].current && items[0].sides[1].current);

    // Resolve both with a note.
    committed(call(
        &s,
        AGENT,
        resolve(bind(), pair.clone(), "postgres stays"),
    ));
    assert_eq!(status(&s, MAIN, RevisionId(1)), Summary::Certified);
    assert!(list(&s, AGENT, bind(), None, ConflictState::All).is_empty());
    let snap = s.actor.snapshot();
    let batch = snap.log.read_row(snap.epoch).unwrap().batch().unwrap();
    let Op::ResolveArtifact {
        note: Some(note), ..
    } = &batch.ops[0]
    else {
        panic!("{:?}", batch.ops)
    };
    let Payload::Available(bytes) = snap.log.read_blob(note) else {
        panic!("note blob")
    };
    assert_eq!(
        ResolutionDoc::from_bytes(&bytes).unwrap().note,
        "postgres stays"
    );
    assert_eq!(batch.channel, Channel::Agent);
}

/// T78: Conflicts are per branch: one opened on `dev` neither gates nor
/// lists on `main`.
#[test]
fn t78_conflicts_are_per_branch() {
    let disk = SimDisk::new();
    let s = service(&disk);
    setup(&s);
    assert_eq!(
        code(&call(
            &s,
            ADMIN,
            Request::BranchCreate {
                name: "dev".into(),
                from: None,
                client_txn_id: None,
            },
        )),
        "ok"
    );
    let dev = s.actor.snapshot().kernel.branch_by_name("dev").unwrap();
    committed(call(
        &s,
        ADMIN,
        open(None, Some("dev"), key_rev("a", 0), Some(key_rev("b", 1))),
    ));
    assert_eq!(status(&s, dev, RevisionId(1)), Summary::Conflicted);
    assert_eq!(status(&s, MAIN, RevisionId(1)), Summary::Certified);
    let on_dev = list(&s, ADMIN, None, Some("dev"), ConflictState::Open);
    assert_eq!(on_dev.len(), 1);
    assert_eq!(on_dev[0].origin, "admin");
    assert!(list(&s, ADMIN, None, None, ConflictState::Open).is_empty());
    // Resolving from main does not see dev's artifacts.
    let r = call(
        &s,
        ADMIN,
        resolve(None, on_dev[0].artifacts.clone(), "not here"),
    );
    assert_eq!(code(&r), "UnknownRef", "{r:?}");
}

/// T78: `open` refuses a == b, a side not visible on the branch, a pair
/// already open, an empty reason, an unknown candidate, and an agent
/// without a session.
#[test]
fn t78_open_refusals() {
    let disk = SimDisk::new();
    let s = service(&disk);
    setup(&s);
    let session = start(&s);
    let bind = || Some(Binding::root(session));
    let same = call(
        &s,
        AGENT,
        open(bind(), None, key_rev("a", 0), Some(key_rev("a", 0))),
    );
    assert_eq!(code(&same), "InvalidArgument");
    // A logical only on dev is not visible on main.
    call(
        &s,
        ADMIN,
        Request::BranchCreate {
            name: "dev".into(),
            from: None,
            client_txn_id: None,
        },
    );
    let dev = s.actor.snapshot().kernel.branch_by_name("dev").unwrap();
    raw_as(
        &s,
        ADMIN,
        dev,
        vec![b"dev only".to_vec()],
        vec![create("c", RecordKind::Fact, b"dev only")],
    )
    .unwrap();
    let invisible = call(
        &s,
        AGENT,
        open(bind(), None, key_rev("a", 0), Some(Target::Key("c".into()))),
    );
    assert_eq!(code(&invisible), "UnknownRef", "{invisible:?}");
    // Already open.
    committed(call(
        &s,
        AGENT,
        open(bind(), None, key_rev("a", 0), Some(key_rev("b", 1))),
    ));
    let again = call(
        &s,
        AGENT,
        open(bind(), None, key_rev("a", 0), Some(key_rev("b", 1))),
    );
    assert_eq!(code(&again), "InvalidArgument", "{again:?}");
    // Empty reason, candidate source.
    let mut r = open(bind(), None, key_rev("b", 1), None);
    if let Request::ConflictOpen { reason, .. } = &mut r {
        *reason = "  ".into();
    }
    assert_eq!(code(&call(&s, AGENT, r)), "InvalidArgument");
    let mut r = open(bind(), None, key_rev("b", 1), None);
    if let Request::ConflictOpen { from, .. } = &mut r {
        *from = Some(ConflictFrom::Candidate(3));
    }
    assert_eq!(code(&call(&s, AGENT, r)), "UnknownRef");
    // Agents act through a session.
    let unbound = call(&s, AGENT, open(None, None, key_rev("b", 1), None));
    assert_eq!(code(&unbound), "InvalidArgument");
    // A one-sided Conflict works.
    let one = committed(call(&s, ADMIN, open(None, None, key_rev("b", 1), None)));
    assert_eq!(one.allocations.new_artifacts.len(), 1);
    // Nothing above opened anything else: the pair and the single side.
    assert_eq!(list(&s, ADMIN, None, None, ConflictState::Open).len(), 2);
}

/// `ctx_resolve` closes Conflicts and PendingReviews only; AnchorBroken
/// stays observer-only, and closed or unknown artifacts are refused.
#[test]
fn resolve_accepts_conflicts_and_reviews_only() {
    let disk = SimDisk::new();
    let s = service(&disk);
    setup(&s);
    let anchor = raw_as(
        &s,
        OBSERVER,
        MAIN,
        vec![b"anchor".to_vec()],
        vec![Op::CreateLogical {
            key: "code:src/lib.rs".into(),
            kind: RecordKind::CodeAnchor,
            scope_lane: "repo".into(),
            content: h(b"anchor"),
            adopt_as_base: true,
        }],
    )
    .unwrap()
    .allocations
    .new_revisions[0];
    let broken = raw_as(
        &s,
        OBSERVER,
        MAIN,
        Vec::new(),
        vec![Op::OpenArtifact {
            ty: ArtifactType::AnchorBroken,
            target: anchor.into(),
            payload_hash: [7; 32],
        }],
    );
    let id = broken.unwrap().allocations.new_artifacts[0];
    assert_eq!(
        code(&call(&s, ADMIN, resolve(None, vec![id], ""))),
        "InvalidArgument"
    );
    assert_eq!(
        code(&call(&s, ADMIN, resolve(None, vec![ArtifactId(99)], ""))),
        "UnknownRef"
    );
    assert_eq!(
        code(&call(&s, ADMIN, resolve(None, Vec::new(), ""))),
        "InvalidArgument"
    );
    let r = committed(call(&s, ADMIN, open(None, None, key_rev("b", 1), None)));
    let id = r.allocations.new_artifacts[0];
    committed(call(&s, ADMIN, resolve(None, vec![id], "")));
    assert_eq!(
        code(&call(&s, ADMIN, resolve(None, vec![id], ""))),
        "InvalidArgument",
        "already resolved"
    );
}

/// Validator reviews are listed with their suggestion, rationale and
/// validator, marked `stale curvec` once the rule's vector moves; `open
/// from = review` converts one into a Conflict and resolves it in the same
/// batch.
#[test]
fn reviews_listed_stale_curvec_and_converted() {
    let disk = SimDisk::new();
    let s = service(&disk);
    setup(&s);
    // c ← Sem a; a moves: the rule is Pending at [(a, a1)].
    raw(
        &s,
        vec![b"use joins".to_vec()],
        vec![create("c", RecordKind::Plan, b"use joins")],
    );
    raw(
        &s,
        Vec::new(),
        vec![Op::AddRule {
            conclusion: RevisionId(2).into(),
            incidences: vec![(RevisionId(0).into(), EdgeKind::Sem)],
            enabled: true,
        }],
    );
    let move_a = |body: &[u8], expected: RevisionId| {
        raw(
            &s,
            vec![body.to_vec()],
            vec![
                Op::AddRevision {
                    logical: LogicalId(0).into(),
                    content: h(body),
                    adopt_as_base: true,
                },
                Op::SetHead {
                    logical: LogicalId(0).into(),
                    expected: expected.into(),
                    new: RevRef::New(0),
                },
            ],
        )
        .allocations
        .new_revisions[0]
    };
    let a1 = move_a(b"orders use mongodb", RevisionId(0));
    let meta = ValidationMeta {
        v: 1,
        kind: ValidatorKind::Llm,
        validator: "claude".into(),
        version: "2".into(),
        model: Some("claude-haiku-5-5".into()),
        prompt_sha256: None,
        label: Label::Conflict,
        rationale: "joins need sql".into(),
        evidence: vec![format!("a@{}", a1.0)],
        tokens_in: None,
        tokens_out: None,
        job: None,
        based_on: None,
        at: 0,
    };
    let (meta_hash, meta_bytes) = contextdb_proto::docs::blob(&meta).unwrap();
    let review = ReviewDoc {
        v: 1,
        suggestion: Suggestion::Reject,
        branch: MAIN,
        rule: Some(RuleId(0)),
        curvec: vec![(LogicalId(0), a1)],
        sides: Vec::new(),
        meta: meta_hash,
    };
    let (review_hash, review_bytes) = contextdb_proto::docs::blob(&review).unwrap();
    let opened = raw(
        &s,
        vec![meta_bytes, review_bytes],
        vec![Op::OpenArtifact {
            ty: ArtifactType::PendingReview,
            target: RevisionId(2).into(),
            payload_hash: review_hash,
        }],
    );
    let review_id = opened.allocations.new_artifacts[0];
    let items = list(&s, ADMIN, None, None, ConflictState::Review);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, "review");
    assert_eq!(items[0].reason, "reject: joins need sql");
    assert_eq!(items[0].origin, "claude (claude-haiku-5-5)");
    assert_eq!(items[0].rule, Some(RuleId(0)));
    assert!(items[0].marks.is_empty());
    assert!(list(&s, ADMIN, None, None, ConflictState::Open).is_empty());
    // The vector moves on: stale curvec.
    move_a(b"orders use cassandra", a1);
    let items = list(&s, ADMIN, None, None, ConflictState::All);
    assert_eq!(items[0].marks, vec!["stale curvec".to_owned()]);
    // Convert: the Conflict opens and the review closes in one batch.
    let mut r = open(
        None,
        None,
        Target::Key("c".into()),
        Some(key_rev("a", a1.0)),
    );
    if let Request::ConflictOpen { from, .. } = &mut r {
        *from = Some(ConflictFrom::Review(review_id));
    }
    let result = committed(call(&s, ADMIN, r));
    assert_eq!(result.allocations.new_artifacts.len(), 2);
    let items = list(&s, ADMIN, None, None, ConflictState::All);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].origin, format!("review {}", review_id.0));
    assert!(
        !s.actor
            .snapshot()
            .view(MAIN)
            .artifact(review_id)
            .unwrap()
            .open,
        "the review is resolved"
    );
}

/// Bound writes are ordinary certified writes: `expect_cert_valid` without
/// a session certificate is refused, and unbound callers cannot ask for it.
#[test]
fn expect_cert_valid_goes_through_the_session() {
    let disk = SimDisk::new();
    let s = service(&disk);
    setup(&s);
    let session = start(&s);
    let mut r = open(Some(Binding::root(session)), None, key_rev("b", 1), None);
    if let Request::ConflictOpen {
        expect_cert_valid, ..
    } = &mut r
    {
        *expect_cert_valid = true;
    }
    assert_eq!(code(&call(&s, AGENT, r)), "Refused");
    let mut r = open(None, None, key_rev("b", 1), None);
    if let Request::ConflictOpen {
        expect_cert_valid, ..
    } = &mut r
    {
        *expect_cert_valid = true;
    }
    assert_eq!(code(&call(&s, ADMIN, r)), "InvalidArgument");
    assert!(list(&s, ADMIN, None, None, ConflictState::All).is_empty());
}
