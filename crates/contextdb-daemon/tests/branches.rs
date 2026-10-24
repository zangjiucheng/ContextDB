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
