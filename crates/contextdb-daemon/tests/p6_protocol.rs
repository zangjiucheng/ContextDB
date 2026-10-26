//! P6 end to end at the service layer (protocol 4): the T19 variants of
//! ADR 0013 §11 through `ctx_commit(expect_cert_valid)` (a base retraction,
//! a rule disable or a ValidationRecord alone in the window; two plans in
//! one actor group; plans on different branches) and S13 `CONFLICTED`
//! after a merge.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
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
    let (store, _) = Store::open(
        disk.clone(),
        dir,
        &OpenOptions {
            create: true,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    Service::new(actor::spawn(store).unwrap(), log, events).unwrap()
}
fn call(s: &Service<SimDisk>, id: Identity, req: Request) -> Response {
    s.handle(id, req).response
}
fn start(s: &Service<SimDisk>, branch: Option<&str>) -> SessionId {
    match call(
        s,
        AGENT,
        Request::SessionStart {
            harness: "t".into(),
            repo: "r".into(),
            head_sha: "h".into(),
            parent: None,
            branch: branch.map(str::to_owned),
            git_branch: None,
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
    base: bool,
    deps: Vec<Dep>,
) -> CommitResult {
    match call(
        s,
        AGENT,
        Request::Record {
            binding: Binding::root(session),
            kind: RecordKind::Fact,
            body: key.as_bytes().to_vec(),
            target: WriteTarget::Create {
                key: key.into(),
                scope_lane: "repo".into(),
            },
            deps,
            adopt_as_base: base,
            client_txn_id: None,
        },
    ) {
        Response::Committed { result } => result,
        other => panic!("{key}: {other:?}"),
    }
}
fn read(s: &Service<SimDisk>, session: SessionId, key: &str, mode: Mode) -> Item {
    match call(
        s,
        AGENT,
        Request::Read {
            binding: Binding::root(session),
            target: Target::Key(key.into()),
            mode,
        },
    ) {
        Response::Read { item, .. } => item,
        other => panic!("{key}: {other:?}"),
    }
}
fn check(s: &Service<SimDisk>, session: SessionId) -> Validation {
    match call(
        s,
        AGENT,
        Request::Check {
            binding: Binding::root(session),
            cert_id: None,
            live_pos: Vec::new(),
            action: None,
            mode: None,
            harness_version: String::new(),
            strict: false,
        },
    ) {
        Response::Check { validation, .. } => validation,
        other => panic!("{other:?}"),
    }
}
fn commit(s: &Service<SimDisk>, session: SessionId, ops: Vec<Op>, expect: bool) -> Response {
    call(
        s,
        AGENT,
        Request::Commit {
            binding: Binding::root(session),
            ops,
            blobs: Vec::new(),
            expect_cert_valid: expect,
            client_txn_id: None,
        },
    )
}
fn supersede(s: &Service<SimDisk>, session: SessionId, item: &Item, body: &[u8]) -> Response {
    call(
        s,
        AGENT,
        Request::Supersede {
            binding: Binding::root(session),
            logical: item.logical,
            body: body.to_vec(),
            expected_head: item.rev,
            adopt_as_base: true,
            carry_rules: false,
            reason: "t".into(),
            client_txn_id: None,
        },
    )
}
/// A plan session that certified reads of `keys` and holds a certificate.
fn plan(s: &Service<SimDisk>, keys: &[&str], branch: Option<&str>) -> SessionId {
    let p = start(s, branch);
    for k in keys {
        let item = read(s, p, k, Mode::Certified);
        assert_eq!(item.summary, Summary::Certified, "{k}");
    }
    assert!(
        matches!(
            check(s, p),
            Validation::Valid | Validation::ValidRecertified { .. }
        ),
        "the plan's certificate"
    );
    p
}
/// A harmless write for a plan's commit: base on its own fact.
fn own_write(s: &Service<SimDisk>, writer: SessionId, key: &str) -> Vec<Op> {
    let rev = record(s, writer, key, false, Vec::new())
        .allocations
        .new_revisions[0];
    vec![Op::AssertBase { rev: rev.into() }]
}
fn refused(r: &Response) -> bool {
    matches!(r, Response::Refused { .. })
}

/// T19 variants: one change other than a head move, alone in the window,
/// refuses a plan whose certificate it invalidates.
#[test]
fn t19_window_changes_other_than_head_moves() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let seed = start(&s, None);
    // A base retraction.
    let a = record(&s, seed, "a", true, Vec::new());
    let p = plan(&s, &["a"], None);
    let ops = own_write(&s, seed, "w1");
    let retract = commit(
        &s,
        seed,
        vec![Op::RetractBase {
            rev: a.allocations.new_revisions[0].into(),
        }],
        false,
    );
    assert!(matches!(retract, Response::Committed { .. }), "{retract:?}");
    assert!(refused(&commit(&s, p, ops, true)), "base retraction");
    // A rule disable: d holds through rule j ← p0.
    let p0 = record(&s, seed, "p0", true, Vec::new())
        .allocations
        .new_revisions[0];
    let d = record(
        &s,
        seed,
        "d",
        false,
        vec![Dep {
            target: Target::KeyRev {
                key: "p0".into(),
                rev: p0,
            },
            kind: EdgeKind::Pin,
        }],
    );
    let j = d.allocations.new_rules[0];
    let p = plan(&s, &["d"], None);
    let ops = own_write(&s, seed, "w2");
    let disable = commit(
        &s,
        seed,
        vec![Op::SetRuleEnabled {
            rule: j.into(),
            enabled: false,
        }],
        false,
    );
    assert!(matches!(disable, Response::Committed { .. }), "{disable:?}");
    assert!(refused(&commit(&s, p, ops, true)), "rule disable");
    // A ValidationRecord: e depends Sem on q, q moved, and e holds by a
    // Verified record; a Rejected record for the same vector in the window.
    let q0 = record(&s, seed, "q", true, Vec::new())
        .allocations
        .new_revisions[0];
    let e = record(
        &s,
        seed,
        "e",
        false,
        vec![Dep {
            target: Target::KeyRev {
                key: "q".into(),
                rev: q0,
            },
            kind: EdgeKind::Sem,
        }],
    );
    let sem_rule = e.allocations.new_rules[0];
    let q = read(&s, seed, "q", Mode::Advisory);
    let Response::Committed { result } = supersede(&s, seed, &q, b"q2") else {
        panic!()
    };
    let q2 = result.allocations.new_revisions[0];
    let validate = |verdict| {
        call(
            &s,
            ADMIN,
            Request::Validate {
                rule: sem_rule,
                verdict,
                curvec_seen: vec![(q.logical, q2)],
                validator: "t".into(),
            },
        )
    };
    assert!(matches!(
        validate(Verdict::Verified),
        Response::Committed { .. }
    ));
    let p = plan(&s, &["e"], None);
    let ops = own_write(&s, seed, "w3");
    assert!(matches!(
        validate(Verdict::Rejected),
        Response::Committed { .. }
    ));
    assert!(refused(&commit(&s, p, ops, true)), "validation record");
}

/// T19 cross-branch: a plan on b is unaffected by head moves on main.
#[test]
fn t19_plans_on_other_branches_do_not_conflict() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let seed = start(&s, None);
    record(&s, seed, "a", true, Vec::new());
    call(
        &s,
        AGENT,
        Request::BranchCreate {
            name: "b".into(),
            from: None,
            client_txn_id: None,
        },
    );
    let on_b = start(&s, Some("b"));
    let p = plan(&s, &["a"], Some("b"));
    let a = read(&s, seed, "a", Mode::Advisory);
    assert!(matches!(
        supersede(&s, seed, &a, b"a2"),
        Response::Committed { .. }
    ));
    let ops = own_write(&s, on_b, "w");
    let r = commit(&s, p, ops, true);
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
}

/// T19 with both plans in flight at once (often in one actor group): each
/// read A and B and moves one of them; exactly one commits.
#[test]
fn t19_concurrent_plans_one_commits() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let seed = start(&s, None);
    for round in 0..20 {
        let (ka, kb) = (format!("a{round}"), format!("b{round}"));
        record(&s, seed, &ka, true, Vec::new());
        record(&s, seed, &kb, true, Vec::new());
        let (p1, p2) = (plan(&s, &[&ka, &kb], None), plan(&s, &[&ka, &kb], None));
        let moves = |key: &str, body: &[u8]| {
            let item = read(&s, seed, key, Mode::Advisory);
            vec![
                Op::AddRevision {
                    logical: item.logical.into(),
                    content: content_hash(body),
                    adopt_as_base: true,
                },
                Op::SetHead {
                    logical: item.logical.into(),
                    expected: item.rev.into(),
                    new: RevRef::New(0),
                },
            ]
        };
        let (m1, m2) = (moves(&ka, b"x"), moves(&kb, b"y"));
        let send = |p, ops, body: &[u8]| {
            call(
                &s,
                AGENT,
                Request::Commit {
                    binding: Binding::root(p),
                    ops,
                    blobs: vec![body.to_vec()],
                    expect_cert_valid: true,
                    client_txn_id: None,
                },
            )
        };
        let (r1, r2) = std::thread::scope(|t| {
            let h1 = t.spawn(|| send(p1, m1, b"x"));
            let h2 = t.spawn(|| send(p2, m2, b"y"));
            (h1.join().unwrap(), h2.join().unwrap())
        });
        let committed = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, Response::Committed { .. }))
            .count();
        assert_eq!(committed, 1, "round {round}: {r1:?} / {r2:?}");
        assert!(refused(&r1) || refused(&r2), "round {round}");
    }
}

/// S13 `CONFLICTED` end to end: a merge conflict on A shows in reads and
/// in a plan's check.
#[test]
fn s13_conflicted_after_a_merge() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let seed = start(&s, None);
    record(&s, seed, "a", true, Vec::new());
    call(
        &s,
        AGENT,
        Request::BranchCreate {
            name: "b".into(),
            from: None,
            client_txn_id: None,
        },
    );
    let on_b = start(&s, Some("b"));
    let a = read(&s, seed, "a", Mode::Advisory);
    assert!(matches!(
        supersede(&s, on_b, &a, b"on b"),
        Response::Committed { .. }
    ));
    assert!(matches!(
        supersede(&s, seed, &a, b"on main"),
        Response::Committed { .. }
    ));
    let p = plan(&s, &["a"], None);
    let merged = call(
        &s,
        AGENT,
        Request::Merge {
            binding: None,
            source: "b".into(),
            into: None,
            require_certified: false,
            certify: Vec::new(),
            client_txn_id: None,
        },
    );
    let Response::Merged { report, .. } = merged else {
        panic!("{merged:?}")
    };
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(
        read(&s, seed, "a", Mode::Advisory).summary,
        Summary::Conflicted
    );
    assert!(matches!(check(&s, p), Validation::Conflicted { .. }));
    // The default merge refuses a conflict (D2: require_certified).
    let refused_merge = call(
        &s,
        AGENT,
        Request::Merge {
            binding: Some(Binding::root(on_b)),
            source: "main".into(),
            into: None,
            require_certified: true,
            certify: Vec::new(),
            client_txn_id: None,
        },
    );
    assert!(
        matches!(&refused_merge, Response::Error { code, .. } if code == "MergeNotCertified"),
        "{refused_merge:?}"
    );
}

fn create_op(key: &str) -> (Vec<Op>, Vec<Vec<u8>>) {
    let body = format!("body-{key}").into_bytes();
    let content = *blake3::hash(&body).as_bytes();
    let op = Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content,
        adopt_as_base: true,
    };
    (vec![op], vec![body])
}
fn commit_as(
    s: &Service<SimDisk>,
    id: Identity,
    session: SessionId,
    key: &str,
    expect: bool,
    txn: [u8; 16],
) -> Response {
    let (ops, blobs) = create_op(key);
    call(
        s,
        id,
        Request::Commit {
            binding: Binding::root(session),
            ops,
            blobs,
            expect_cert_valid: expect,
            client_txn_id: Some(txn),
        },
    )
}

/// Review (P6): a `ctx_commit` retry is the same request only on the same
/// branch, channel and principal; another session's identical ops under
/// the same id get `TxnIdReuse`, never the other branch's result.
#[test]
fn commit_txn_ids_are_bound_to_branch_and_principal() {
    let disk = SimDisk::new();
    let s = service(&disk);
    call(
        &s,
        ADMIN,
        Request::BranchCreate {
            name: "b".into(),
            from: None,
            client_txn_id: None,
        },
    );
    let (on_b, on_main) = (start(&s, Some("b")), start(&s, None));
    let first = commit_as(&s, AGENT, on_b, "x", false, [7; 16]);
    assert!(matches!(first, Response::Committed { .. }), "{first:?}");
    let other = Identity {
        channel: Channel::Agent,
        principal: PrincipalId(9),
    };
    for (id, session) in [(other, on_main), (AGENT, on_main), (other, on_b)] {
        let r = commit_as(&s, id, session, "x", false, [7; 16]);
        assert!(
            matches!(&r, Response::Error { code, .. } if code == "TxnIdReuse"),
            "{r:?}"
        );
    }
    assert_eq!(commit_as(&s, AGENT, on_b, "x", false, [7; 16]), first);
}

/// Review (P6): a retry of a durable `expect_cert_valid` commit finds its
/// result after a `ctx_check` re-issued the session certificate, also after
/// a restart (T18b, ADR 0013 §4.1).
#[test]
fn commit_retry_survives_a_reissued_certificate() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let seed = start(&s, None);
    record(&s, seed, "a", true, Vec::new());
    record(&s, seed, "b", true, Vec::new());
    let p = plan(&s, &["a"], None);
    let first = commit_as(&s, AGENT, p, "y", true, [8; 16]);
    assert!(matches!(first, Response::Committed { .. }), "{first:?}");
    let cert = |s: &Service<SimDisk>| match call(
        s,
        AGENT,
        Request::Check {
            binding: Binding::root(p),
            cert_id: None,
            live_pos: Vec::new(),
            action: None,
            mode: None,
            harness_version: String::new(),
            strict: false,
        },
    ) {
        Response::Check { cert: Some(c), .. } => c.cert_id,
        other => panic!("{other:?}"),
    };
    let before = cert(&s);
    read(&s, p, "b", Mode::Certified);
    assert_ne!(cert(&s), before, "the certificate is re-issued");
    assert_eq!(commit_as(&s, AGENT, p, "y", true, [8; 16]), first);
    call(&s, ADMIN, Request::Shutdown);
    drop(s);
    let s = service(&disk);
    assert_eq!(commit_as(&s, AGENT, p, "y", true, [8; 16]), first);
}

/// Review (P6): `expect_cert_valid` without a session certificate is
/// refused with the Empty verdict (§4 step 2); a merge into an archived
/// branch from an unknown source is BranchArchived before InvalidMerge
/// (§9); branch and merge requests take a client_txn_id whose retry returns
/// the stored result (SPEC §6.5).
#[test]
fn branch_requests_take_txn_ids_and_errors_follow_the_order() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let none = start(&s, None);
    let (ops, blobs) = create_op("z");
    let r = call(
        &s,
        AGENT,
        Request::Commit {
            binding: Binding::root(none),
            ops,
            blobs,
            expect_cert_valid: true,
            client_txn_id: None,
        },
    );
    assert_eq!(
        r,
        Response::Refused {
            validation: Validation::Empty
        }
    );
    let create = Request::BranchCreate {
        name: "c".into(),
        from: None,
        client_txn_id: Some([5; 16]),
    };
    let created = call(&s, ADMIN, create.clone());
    assert!(matches!(created, Response::Committed { .. }), "{created:?}");
    assert_eq!(
        call(&s, ADMIN, create),
        created,
        "a retry, not BranchExists"
    );
    let merge = Request::Merge {
        binding: None,
        source: "main".into(),
        into: Some("c".into()),
        require_certified: false,
        certify: Vec::new(),
        client_txn_id: Some([6; 16]),
    };
    let merged = call(&s, AGENT, merge.clone());
    assert!(matches!(merged, Response::Merged { .. }), "{merged:?}");
    assert_eq!(call(&s, AGENT, merge), merged);
    let archive = Request::BranchArchive {
        name: "c".into(),
        client_txn_id: Some([9; 16]),
    };
    let archived = call(&s, ADMIN, archive.clone());
    assert!(
        matches!(archived, Response::Committed { .. }),
        "{archived:?}"
    );
    assert_eq!(call(&s, ADMIN, archive), archived);
    let r = call(
        &s,
        AGENT,
        Request::Merge {
            binding: None,
            source: "nosuch".into(),
            into: Some("c".into()),
            require_certified: false,
            certify: Vec::new(),
            client_txn_id: None,
        },
    );
    assert!(
        matches!(&r, Response::Error { code, .. } if code == "BranchArchived"),
        "{r:?}"
    );
}

/// Review (P6): a `ctx_commit` counts as a session write like record and
/// supersede (ADR 0010 §4.3), once per client_txn_id.
#[test]
fn ctx_commit_is_a_session_write() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let session = start(&s, None);
    let writes = |s: &Service<SimDisk>| match call(
        s,
        ADMIN,
        Request::Telemetry {
            since: 0,
            limit: 1000,
        },
    ) {
        Response::Telemetry { entries, .. } => entries
            .into_iter()
            .filter(|e| matches!(e, TelemetryEntry::Write { .. }))
            .count(),
        other => panic!("{other:?}"),
    };
    let r = commit_as(&s, AGENT, session, "w", false, [4; 16]);
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    assert_eq!(writes(&s), 1);
    commit_as(&s, AGENT, session, "w", false, [4; 16]);
    assert_eq!(writes(&s), 1, "a retry is not a second write");
}
