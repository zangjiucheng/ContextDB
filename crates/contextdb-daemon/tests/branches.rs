//! P6 sessions on branches (ADR 0013 §7, D1; protocol 4): a session binds
//! to the named branch, else to the branch named like its git branch, else
//! `main`; reads, records, checks and certificates use that branch; the
//! binding survives a restart; the observer reads and commits on the branch
//! of its worktree.
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
fn code(r: &Response) -> &str {
    match r {
        Response::Error { code, .. } => code,
        _ => "ok",
    }
}
fn start(s: &Service<SimDisk>, branch: Option<&str>, git_branch: Option<&str>) -> Response {
    call(
        s,
        AGENT,
        Request::SessionStart {
            harness: "test".into(),
            repo: "r".into(),
            head_sha: "h".into(),
            parent: None,
            branch: branch.map(str::to_owned),
            git_branch: git_branch.map(str::to_owned),
        },
    )
}
fn session(r: Response) -> SessionId {
    match r {
        Response::Session { session } => session,
        other => panic!("{other:?}"),
    }
}
fn record(s: &Service<SimDisk>, session: SessionId, key: &str) -> Response {
    call(
        s,
        AGENT,
        Request::Record {
            binding: Binding::root(session),
            kind: RecordKind::Decision,
            body: key.as_bytes().to_vec(),
            target: WriteTarget::Create {
                key: key.into(),
                scope_lane: "repo".into(),
            },
            deps: Vec::new(),
            adopt_as_base: true,
            client_txn_id: None,
        },
    )
}
fn read(s: &Service<SimDisk>, session: SessionId, key: &str) -> Response {
    call(
        s,
        AGENT,
        Request::Read {
            binding: Binding::root(session),
            target: Target::Key(key.into()),
            mode: Mode::Advisory,
        },
    )
}
/// An admin batch on `branch`.
fn admin(s: &Service<SimDisk>, branch: BranchId, ops: Vec<Op>) -> Response {
    let mut batch = Batch::new(ops);
    batch.branch = branch;
    call(
        s,
        ADMIN,
        Request::Raw {
            blobs: Vec::new(),
            batch,
        },
    )
}

#[test]
fn sessions_bind_to_branches_and_keep_them_across_restart() {
    let disk = SimDisk::new();
    let (on_feature, on_main) = {
        let s = service(&disk);
        let main = session(start(&s, None, None));
        assert_eq!(code(&record(&s, main, "shared")), "ok");
        let created = admin(
            &s,
            MAIN,
            vec![Op::CreateBranch {
                name: "feature/x".into(),
            }],
        );
        assert_eq!(code(&created), "ok");
        // The git branch names the session's branch.
        let feature = session(start(&s, None, Some("feature/x")));
        assert_eq!(code(&record(&s, feature, "branch-only")), "ok");
        assert_eq!(code(&read(&s, feature, "branch-only")), "ok");
        assert_eq!(
            code(&read(&s, feature, "shared")),
            "ok",
            "inherited at the fork"
        );
        // Main does not see the branch's record.
        assert_eq!(code(&read(&s, main, "branch-only")), "UnknownRef");
        // An unknown git branch falls back to main; an unknown explicit
        // branch is an error.
        let fallback = session(start(&s, None, Some("no-such-branch")));
        assert_eq!(code(&read(&s, fallback, "branch-only")), "UnknownRef");
        assert_eq!(
            code(&start(&s, Some("no-such-branch"), None)),
            "UnknownBranch"
        );
        call(&s, ADMIN, Request::Shutdown);
        (feature, main)
    };
    // The binding is durable: after a restart the session still reads and
    // writes its branch.
    let s = service(&disk);
    assert_eq!(code(&read(&s, on_feature, "branch-only")), "ok");
    assert_eq!(code(&record(&s, on_feature, "after-restart")), "ok");
    assert_eq!(code(&read(&s, on_main, "after-restart")), "UnknownRef");
    // Archived branches: an explicit name is refused, a git name falls back.
    let archived = admin(
        &s,
        BranchId(1),
        vec![Op::ArchiveBranch {
            name: "feature/x".into(),
        }],
    );
    assert_eq!(code(&archived), "ok");
    assert_eq!(code(&start(&s, Some("feature/x"), None)), "BranchArchived");
    let fallback = session(start(&s, None, Some("feature/x")));
    assert_eq!(code(&read(&s, fallback, "branch-only")), "UnknownRef");
}

/// Certificates name the session's branch, and a check on the branch
/// validates against the branch's state.
#[test]
fn checks_and_certificates_use_the_session_branch() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let main = session(start(&s, None, None));
    assert_eq!(code(&record(&s, main, "a")), "ok");
    admin(&s, MAIN, vec![Op::CreateBranch { name: "b".into() }]);
    let on_b = session(start(&s, Some("b"), None));
    let certified = call(
        &s,
        AGENT,
        Request::Read {
            binding: Binding::root(on_b),
            target: Target::Key("a".into()),
            mode: Mode::Certified,
        },
    );
    assert_eq!(code(&certified), "ok");
    // Main moves `a`'s head away; the branch session's check stays valid.
    let a = match read(&s, main, "a") {
        Response::Read { item, .. } => item,
        other => panic!("{other:?}"),
    };
    let moved = call(
        &s,
        AGENT,
        Request::Supersede {
            binding: Binding::root(main),
            logical: a.logical,
            body: b"a2".to_vec(),
            expected_head: a.rev,
            adopt_as_base: true,
            carry_rules: false,
            reason: "test".into(),
            client_txn_id: None,
        },
    );
    assert_eq!(code(&moved), "ok");
    let check = |session| {
        call(
            &s,
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
        )
    };
    match check(on_b) {
        Response::Check { validation, .. } => assert_eq!(validation, Validation::Valid),
        other => panic!("{other:?}"),
    }
    match check(main) {
        Response::Check { validation, .. } => {
            assert!(
                !matches!(validation, Validation::Invalid { .. }),
                "{validation:?}"
            )
        }
        other => panic!("{other:?}"),
    }
}

/// The observer reads (and so commits) a scope on its worktree's branch.
#[test]
fn anchor_state_follows_the_observer_branch() {
    let disk = SimDisk::new();
    let s = service(&disk);
    admin(
        &s,
        MAIN,
        vec![Op::CreateBranch {
            name: "topic".into(),
        }],
    );
    let state = |branch: Option<&str>| {
        call(
            &s,
            ADMIN,
            Request::AnchorState {
                scope: "code:repo".into(),
                branch: branch.map(str::to_owned),
            },
        )
    };
    match state(Some("topic")) {
        Response::AnchorState { branch, .. } => assert_eq!(branch, BranchId(1)),
        other => panic!("{other:?}"),
    }
    for name in [None, Some("unknown")] {
        match state(name) {
            Response::AnchorState { branch, .. } => assert_eq!(branch, MAIN),
            other => panic!("{other:?}"),
        }
    }
}

fn certified_read(s: &Service<SimDisk>, session: SessionId, key: &str) -> Item {
    match call(
        s,
        AGENT,
        Request::Read {
            binding: Binding::root(session),
            target: Target::Key(key.into()),
            mode: Mode::Certified,
        },
    ) {
        Response::Read { item, .. } => item,
        other => panic!("{other:?}"),
    }
}
fn check(s: &Service<SimDisk>, session: SessionId) -> Response {
    call(
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
    )
}
/// Ops that move `item`'s logical to a new revision with `body`.
fn supersede_ops(item: &Item, body: &[u8]) -> Vec<Op> {
    vec![
        Op::AddRevision {
            logical: item.logical.into(),
            content: *blake3::hash(body).as_bytes(),
            adopt_as_base: true,
        },
        Op::SetHead {
            logical: item.logical.into(),
            expected: item.rev.into(),
            new: RevRef::New(0),
        },
    ]
}
fn commit(
    s: &Service<SimDisk>,
    session: SessionId,
    ops: Vec<Op>,
    blobs: Vec<Vec<u8>>,
    txn: Option<[u8; 16]>,
) -> Response {
    call(
        s,
        AGENT,
        Request::Commit {
            binding: Binding::root(session),
            ops,
            blobs,
            expect_cert_valid: true,
            client_txn_id: txn,
        },
    )
}

/// T19 through `ctx_commit(expect_cert_valid)`: two plans read A and B and
/// write different keys; the second to commit is refused (write skew).
/// An idempotent retry returns the first result even after the state
/// moved; a different batch under the same id is `TxnIdReuse`.
#[test]
fn t19_expect_cert_valid_first_committer_wins() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let seed = session(start(&s, None, None));
    for key in ["a", "b", "c"] {
        assert_eq!(code(&record(&s, seed, key)), "ok");
    }
    let (p1, p2) = (
        session(start(&s, None, None)),
        session(start(&s, None, None)),
    );
    let mut items = Vec::new();
    for p in [p1, p2] {
        items.push(certified_read(&s, p, "a"));
        certified_read(&s, p, "b");
        assert!(matches!(check(&s, p), Response::Check { .. }));
    }
    let c = certified_read(&s, seed, "c");
    let txn = Some([7; 16]);
    let first = commit(
        &s,
        p1,
        supersede_ops(&items[0], b"a2"),
        vec![b"a2".to_vec()],
        txn,
    );
    let Response::Committed { result } = first else {
        panic!("{first:?}")
    };
    // Plan 2 read the same A: refused with the verdict, nothing committed.
    let second = commit(&s, p2, supersede_ops(&c, b"c2"), vec![b"c2".to_vec()], None);
    assert!(
        matches!(
            second,
            Response::Refused {
                validation: Validation::Invalid { .. }
            }
        ),
        "{second:?}"
    );
    // The retry matches the client hash, not the extended batch.
    let retry = commit(
        &s,
        p1,
        supersede_ops(&items[0], b"a2"),
        vec![b"a2".to_vec()],
        txn,
    );
    assert_eq!(retry, Response::Committed { result });
    let reuse = commit(&s, p1, supersede_ops(&c, b"c3"), vec![b"c3".to_vec()], txn);
    assert_eq!(code(&reuse), "TxnIdReuse");
    // Clients never send certificate read sets themselves.
    let forged = commit(
        &s,
        p1,
        vec![Op::ExpectHeads {
            entries: Vec::new(),
            source: ReadSource::Certificate([0; 32]),
        }],
        Vec::new(),
        None,
    );
    assert_eq!(code(&forged), "ChannelRestricted");
}

/// Branch requests: create, list, archive, merge with its report, reads
/// on a named branch and dry runs on the session's branch.
#[test]
fn branch_requests() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let main = session(start(&s, None, None));
    assert_eq!(code(&record(&s, main, "a")), "ok");
    let created = call(
        &s,
        AGENT,
        Request::BranchCreate {
            name: "feature".into(),
            from: None,
            client_txn_id: None,
        },
    );
    assert_eq!(code(&created), "ok");
    match call(&s, AGENT, Request::BranchList) {
        Response::Branches { branches } => {
            let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
            assert_eq!(names, ["main", "feature"]);
            assert_eq!(branches[1].parent, Some(MAIN));
        }
        other => panic!("{other:?}"),
    }
    let on_b = session(start(&s, Some("feature"), None));
    assert_eq!(code(&record(&s, on_b, "x")), "ok");
    // A dry run on the session's branch changes nothing.
    let x = match read(&s, on_b, "x") {
        Response::Read { item, .. } => item,
        other => panic!("{other:?}"),
    };
    let dry = call(
        &s,
        AGENT,
        Request::WhatIf {
            binding: Binding::root(on_b),
            ops: vec![Op::RetractBase { rev: x.rev.into() }],
        },
    );
    assert!(matches!(dry, Response::WhatIf { .. }), "{dry:?}");
    // Unbound reads on a named branch.
    let why = |branch: Option<&str>| {
        let inner = Request::Why {
            target: Target::Key("x".into()),
            depth: 1,
        };
        let req = match branch {
            Some(b) => Request::OnBranch {
                branch: BranchSel::Named(b.into()),
                request: Box::new(inner),
            },
            None => inner,
        };
        code(&call(&s, AGENT, req)).to_owned()
    };
    assert_eq!(why(Some("feature")), "ok");
    assert_eq!(why(None), "UnknownRef");
    assert_eq!(why(Some("nope")), "UnknownBranch");
    // Merge into main: x arrives; the report says so.
    match call(
        &s,
        AGENT,
        Request::Merge {
            binding: None,
            source: "feature".into(),
            into: None,
            require_certified: false,
            certify: Vec::new(),
            client_txn_id: None,
        },
    ) {
        Response::Merged { report, .. } => assert_eq!(report.newly_visible[0], 1),
        other => panic!("{other:?}"),
    }
    assert_eq!(code(&read(&s, main, "x")), "ok");
    let unknown = call(
        &s,
        AGENT,
        Request::Merge {
            binding: None,
            source: "nope".into(),
            into: None,
            require_certified: true,
            certify: Vec::new(),
            client_txn_id: None,
        },
    );
    assert_eq!(code(&unknown), "InvalidMerge");
    assert_eq!(
        code(&call(
            &s,
            AGENT,
            Request::BranchArchive {
                name: "feature".into(),
                client_txn_id: None
            }
        )),
        "ok"
    );
    assert_eq!(code(&record(&s, on_b, "y")), "BranchArchived");
}
