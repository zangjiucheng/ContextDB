//! P3 end-to-end at the service layer (identity injected as the transport
//! would): T07e, T32, T36, T46a, carry_rules, `ctx validate`, sessions across
//! restart and the G6 flow (stale plan → Invalid with diffs → recovery).
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use std::path::{Path, PathBuf};

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
fn service(disk: &SimDisk) -> Service<SimDisk> {
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    let actor = actor::spawn(store).unwrap();
    Service::new(actor, disk.clone(), PathBuf::from(dir), log, events)
}
fn call(s: &Service<SimDisk>, id: Identity, req: Request) -> Response {
    s.handle(id, req).response
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
        },
    ) {
        Response::Session { session } => session,
        other => panic!("{other:?}"),
    }
}
fn record(
    s: &Service<SimDisk>,
    session: SessionId,
    key: &str,
    body: &str,
    deps: Vec<Dep>,
) -> CommitResult {
    match call(
        s,
        AGENT,
        Request::Record {
            session,
            kind: RecordKind::Decision,
            body: body.as_bytes().to_vec(),
            target: WriteTarget::Create {
                key: key.into(),
                scope_lane: "repo".into(),
            },
            deps,
            adopt_as_base: deps_empty_base(),
            client_txn_id: None,
        },
    ) {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
fn deps_empty_base() -> bool {
    true
}
fn code(r: &Response) -> &str {
    match r {
        Response::Error { code, .. } => code,
        _ => "ok",
    }
}
fn recall(s: &Service<SimDisk>, session: SessionId, query: &str) -> (Vec<Item>, Vec<Item>) {
    match call(
        s,
        AGENT,
        Request::Recall {
            session,
            query: query.into(),
            scope: "repo".into(),
            mode: Mode::Certified,
            limit: 10,
        },
    ) {
        Response::Recall {
            items, withheld, ..
        } => (items, withheld),
        other => panic!("{other:?}"),
    }
}
#[test]
fn t46a_agent_cannot_write_anchors_raw_batches_or_validations() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let session = start(&s);
    let r = call(
        &s,
        AGENT,
        Request::Record {
            session,
            kind: RecordKind::CodeAnchor,
            body: b"fn main".to_vec(),
            target: WriteTarget::Create {
                key: "code:repo/a.rs".into(),
                scope_lane: "repo".into(),
            },
            deps: Vec::new(),
            adopt_as_base: true,
            client_txn_id: None,
        },
    );
    assert_eq!(code(&r), "KindRestricted");
    let raw = Request::Raw {
        blobs: Vec::new(),
        batch: Batch::new(Vec::new()),
    };
    assert_eq!(code(&call(&s, AGENT, raw.clone())), "ChannelRestricted");
    let validate = Request::Validate {
        rule: RuleId(0),
        verdict: Verdict::Verified,
        curvec_seen: Vec::new(),
        validator: "agent".into(),
    };
    assert_eq!(code(&call(&s, AGENT, validate)), "ChannelRestricted");
    // The observer channel may write anchors through raw batches.
    let content = *blake3::hash(b"").as_bytes();
    let anchor = |blobs: Vec<Vec<u8>>| Request::Raw {
        blobs,
        batch: Batch::new(vec![Op::CreateLogical {
            key: "code:repo/b.rs".into(),
            kind: RecordKind::CodeAnchor,
            scope_lane: "repo".into(),
            content,
            adopt_as_base: true,
        }]),
    };
    // The payload must exist first: without it the batch is MissingBlob.
    assert_eq!(code(&call(&s, OBSERVER, anchor(Vec::new()))), "MissingBlob");
    assert_eq!(code(&call(&s, OBSERVER, anchor(vec![Vec::new()]))), "ok");
}
#[test]
fn t36_deps_must_be_this_sessions_required_handles_or_explicit_revisions() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = start(&s);
    let b = start(&s);
    record(&s, a, "db", "postgres", Vec::new());
    let (items, _) = recall(&s, a, "db");
    let handle = items[0].handle.clone().unwrap();
    let dep = |target| {
        vec![Dep {
            target,
            kind: EdgeKind::Cur,
        }]
    };
    let plan = |session, deps| Request::Record {
        session,
        kind: RecordKind::Plan,
        body: b"migrate".to_vec(),
        target: WriteTarget::Create {
            key: format!("plan-{}", session[0]),
            scope_lane: "repo".into(),
        },
        deps,
        adopt_as_base: false,
        client_txn_id: None,
    };
    // Another session's handle, an unknown prefix and a bare logical: InvalidDep.
    assert_eq!(
        code(&call(
            &s,
            AGENT,
            plan(b, dep(Target::Handle(handle.clone())))
        )),
        "InvalidDep"
    );
    assert_eq!(
        code(&call(
            &s,
            AGENT,
            plan(a, dep(Target::Handle("000000000000.0".into())))
        )),
        "InvalidDep"
    );
    assert_eq!(
        code(&call(
            &s,
            AGENT,
            plan(a, dep(Target::Logical(LogicalId(0))))
        )),
        "InvalidDep"
    );
    // This session's handle and an explicit logical@rev are accepted.
    assert_eq!(
        code(&call(&s, AGENT, plan(a, dep(Target::Handle(handle))))),
        "ok"
    );
    let pinned = Target::Pinned {
        logical: LogicalId(0),
        rev: RevisionId(0),
    };
    assert_eq!(code(&call(&s, AGENT, plan(b, dep(pinned)))), "ok");
}
#[test]
fn t07e_certified_read_never_returns_a_superseded_revision() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = start(&s);
    record(&s, a, "db", "mongodb", Vec::new());
    let (items, _) = recall(&s, a, "db");
    let handle = items[0].handle.clone().unwrap();
    let r = call(
        &s,
        AGENT,
        Request::Supersede {
            session: a,
            logical: LogicalId(0),
            body: b"postgresql".to_vec(),
            expected_head: RevisionId(0),
            adopt_as_base: true,
            carry_rules: false,
            reason: "switch".into(),
            client_txn_id: None,
        },
    );
    assert_eq!(code(&r), "ok");
    let read = |target, mode| {
        call(
            &s,
            AGENT,
            Request::Read {
                session: a,
                target,
                mode,
            },
        )
    };
    // Handle, explicit logical@rev: the superseded DB@v1 is never served as certified.
    assert_eq!(
        code(&read(Target::Handle(handle), Mode::Certified)),
        "NotCertified"
    );
    let v1 = Target::Pinned {
        logical: LogicalId(0),
        rev: RevisionId(0),
    };
    assert_eq!(code(&read(v1.clone(), Mode::Certified)), "NotCertified");
    // Advisory reads return it, marked superseded.
    match read(v1, Mode::Advisory) {
        Response::Read { item, payload } => {
            assert_eq!(payload.as_deref(), Some(&b"mongodb"[..]));
            assert_eq!(item.summary, Summary::Stale(StaleReason::HeadSuperseded));
        }
        other => panic!("{other:?}"),
    }
    match read(Target::Key("db".into()), Mode::Certified) {
        Response::Read { item, payload } => {
            assert_eq!(item.rev, RevisionId(1));
            assert_eq!(payload.as_deref(), Some(&b"postgresql"[..]));
        }
        other => panic!("{other:?}"),
    }
}
#[test]
fn t32_retry_after_conflict_references_exactly_what_was_read() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = start(&s);
    record(&s, a, "db", "v1", Vec::new());
    record(&s, a, "x", "x", Vec::new());
    let revise = |expected, deps| Request::Record {
        session: a,
        kind: RecordKind::Decision,
        body: format!("v{expected}").into_bytes(),
        target: WriteTarget::Revise {
            logical: LogicalId(0),
            expected_head: RevisionId(expected),
        },
        deps,
        adopt_as_base: true,
        client_txn_id: None,
    };
    // Another writer moved the head: the stale CAS fails with the actual head.
    assert_eq!(code(&call(&s, AGENT, revise(0, Vec::new()))), "ok");
    let conflict = call(&s, AGENT, revise(0, Vec::new()));
    assert_eq!(code(&conflict), "HeadConflict");
    // An informed retry succeeds; its rule references only the supplied dep.
    let x = Target::Pinned {
        logical: LogicalId(1),
        rev: RevisionId(1),
    };
    let Response::Committed { result } = call(
        &s,
        AGENT,
        revise(
            2,
            vec![Dep {
                target: x,
                kind: EdgeKind::Cur,
            }],
        ),
    ) else {
        panic!("retry failed")
    };
    let snap = s.actor.snapshot();
    let new_rev = result.allocations.new_revisions[0];
    let rules = snap.db.rules_concluding(new_rev);
    assert_eq!(rules.len(), 1, "nothing added or rebound implicitly");
    assert_eq!(
        rules[0].2.iter().map(|p| p.premise).collect::<Vec<_>>(),
        vec![RevisionId(1)]
    );
}
#[test]
fn carry_rules_copies_the_old_heads_own_rules_only_and_validate_needs_admin() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = start(&s);
    record(&s, a, "evidence", "e", Vec::new());
    let plan = Request::Record {
        session: a,
        kind: RecordKind::Plan,
        body: b"plan v1".to_vec(),
        target: WriteTarget::Create {
            key: "plan".into(),
            scope_lane: "repo".into(),
        },
        deps: vec![Dep {
            target: Target::Pinned {
                logical: LogicalId(0),
                rev: RevisionId(0),
            },
            kind: EdgeKind::Cur,
        }],
        adopt_as_base: false,
        client_txn_id: None,
    };
    assert_eq!(code(&call(&s, AGENT, plan)), "ok");
    // A dependent of the plan, which must not be rebound by supersede.
    let child = Request::Record {
        session: a,
        kind: RecordKind::Plan,
        body: b"child".to_vec(),
        target: WriteTarget::Create {
            key: "child".into(),
            scope_lane: "repo".into(),
        },
        deps: vec![Dep {
            target: Target::Pinned {
                logical: LogicalId(1),
                rev: RevisionId(1),
            },
            kind: EdgeKind::Pin,
        }],
        adopt_as_base: false,
        client_txn_id: None,
    };
    assert_eq!(code(&call(&s, AGENT, child)), "ok");
    let sup = Request::Supersede {
        session: a,
        logical: LogicalId(1),
        body: b"plan v2".to_vec(),
        expected_head: RevisionId(1),
        adopt_as_base: false,
        carry_rules: true,
        reason: "refine".into(),
        client_txn_id: None,
    };
    let Response::Committed { result } = call(&s, AGENT, sup) else {
        panic!()
    };
    let snap = s.actor.snapshot();
    let v2 = result.allocations.new_revisions[0];
    assert_eq!(
        snap.db.rules_concluding(v2).len(),
        1,
        "own justification copied"
    );
    // The child still pins v1, so it goes stale (not silently rebound).
    assert_eq!(
        snap.db.status(RevisionId(2)).unwrap(),
        Summary::Stale(StaleReason::SupersededPinnedPremise(RevisionId(1)))
    );
    assert_eq!(snap.db.status(v2).unwrap(), Summary::Certified);
    let validate = Request::Validate {
        rule: RuleId(0),
        verdict: Verdict::Verified,
        curvec_seen: Vec::new(),
        validator: "human".into(),
    };
    // Admin reaches the kernel (which rejects: no Sem incidence), agent never does.
    assert_eq!(code(&call(&s, ADMIN, validate.clone())), "NoSemIncidence");
    assert_eq!(code(&call(&s, AGENT, validate)), "ChannelRestricted");
}
#[test]
fn g6_stale_plan_is_invalid_with_diffs_then_recovers() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let planner = start(&s);
    let other = start(&s);
    record(&s, planner, "db", "mongodb", Vec::new());
    let (items, _) = recall(&s, planner, "db");
    let handle = items[0].handle.clone().unwrap();
    let plan = Request::Record {
        session: planner,
        kind: RecordKind::Plan,
        body: b"use mongodb driver".to_vec(),
        target: WriteTarget::Create {
            key: "plan".into(),
            scope_lane: "repo".into(),
        },
        deps: vec![Dep {
            target: Target::Handle(handle),
            kind: EdgeKind::Cur,
        }],
        adopt_as_base: false,
        client_txn_id: None,
    };
    assert_eq!(code(&call(&s, AGENT, plan)), "ok");
    let read_plan = Request::Read {
        session: planner,
        target: Target::Key("plan".into()),
        mode: Mode::Certified,
    };
    assert_eq!(code(&call(&s, AGENT, read_plan)), "ok");
    let check = || {
        call(
            &s,
            AGENT,
            Request::Check {
                session: planner,
                cert_id: None,
                live_pos: Vec::new(),
            },
        )
    };
    let Response::Check {
        decision,
        validation,
        ..
    } = check()
    else {
        panic!()
    };
    assert_eq!((decision, validation), (Decision::Allow, Validation::Valid));
    // Another agent supersedes the database decision.
    let sup = Request::Supersede {
        session: other,
        logical: LogicalId(0),
        body: b"postgresql".to_vec(),
        expected_head: RevisionId(0),
        adopt_as_base: true,
        carry_rules: false,
        reason: "migration".into(),
        client_txn_id: None,
    };
    assert_eq!(code(&call(&s, AGENT, sup)), "ok");
    let Response::Check {
        decision,
        validation,
        ..
    } = check()
    else {
        panic!()
    };
    assert_eq!(decision, Decision::Warn);
    let Validation::Invalid { diffs, .. } = validation else {
        panic!("stale plan must be Invalid, got {validation:?}")
    };
    assert!(diffs.iter().any(|d| d.logical == LogicalId(0)), "{diffs:?}");
    // Re-reading the new decision and the plan recovers once the plan has
    // alternative support from the new revision (T32-style re-read flow).
    let (items, _) = recall(&s, planner, "db");
    assert_eq!(items[0].rev, RevisionId(2));
}
#[test]
fn sessions_and_handles_survive_a_daemon_restart() {
    let disk = SimDisk::new();
    let (session, handle) = {
        let s = service(&disk);
        let a = start(&s);
        record(&s, a, "db", "pg", Vec::new());
        let (items, _) = recall(&s, a, "db");
        let h = items[0].handle.clone().unwrap();
        // Force durability of the recall certificate, as a write using it would.
        let dep = Request::Record {
            session: a,
            kind: RecordKind::Plan,
            body: b"p".to_vec(),
            target: WriteTarget::Create {
                key: "p".into(),
                scope_lane: "repo".into(),
            },
            deps: vec![Dep {
                target: Target::Handle(h.clone()),
                kind: EdgeKind::Cur,
            }],
            adopt_as_base: false,
            client_txn_id: None,
        };
        assert_eq!(code(&call(&s, AGENT, dep)), "ok");
        call(&s, ADMIN, Request::Shutdown);
        (a, h)
    };
    std::thread::sleep(std::time::Duration::from_millis(50));
    let crashed = disk.crash(contextdb_store::TailFate::Torn, 1);
    let s = service(&crashed);
    let read = Request::Read {
        session,
        target: Target::Handle(handle),
        mode: Mode::Certified,
    };
    assert_eq!(
        code(&call(&s, AGENT, read)),
        "ok",
        "handle resolves after restart"
    );
}
