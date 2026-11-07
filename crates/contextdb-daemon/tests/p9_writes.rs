//! P9 milestone 4 (ADR 0017 §6): writes, through the daemon.
//!
//! - T102: every rule of §6.2 (create, change, bases, rules, artifacts);
//!   `ctx_what_if` and `ImpactBatch` refuse exactly what the commit
//!   refuses; `Unauthorized` never depends on whether a revision exists;
//!   deps and `carry_rules` naming hidden logicals.
//! - T107: proof of possession for every content hash, in dry runs as in
//!   commits.
use contextdb_daemon::{
    actor,
    principals::PrincipalMap,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_proto::fsguard;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const OWNER_AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
const BOB: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(16),
};
const CAROL: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(17),
};

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}
fn h(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}
fn body(k: &str) -> Vec<u8> {
    format!("body of {k}").into_bytes()
}
/// alice owns the store; bob and carol write `repo`, only alice curates
/// it; bob (group `billing`) reads and writes `billing`; `secret` falls to
/// the owner-only default.
fn doc() -> AccessDoc {
    AccessDoc {
        version: 1,
        owner: "alice".into(),
        principals: [
            ("alice".to_owned(), fsguard::current_uid()),
            ("bob".to_owned(), 4242),
            ("carol".to_owned(), 4243),
        ]
        .into(),
        groups: [("billing".to_owned(), set(&["bob"]))].into(),
        lanes: [
            (
                "repo".to_owned(),
                LaneEntry {
                    members: set(&["*members"]),
                    writers: set(&["*members"]),
                    curators: set(&["alice"]),
                    ..LaneEntry::owner_only()
                },
            ),
            (
                "billing".to_owned(),
                LaneEntry {
                    members: set(&["@billing"]),
                    writers: set(&["@billing"]),
                    ..LaneEntry::owner_only()
                },
            ),
        ]
        .into(),
        code_lanes: LaneEntry::code_default(),
        personal_egress: false,
        default_lane: LaneEntry::owner_only(),
    }
}
type Svc = Service<SimDisk>;
struct Fixture {
    s: Svc,
    rev: BTreeMap<&'static str, RevisionId>,
    logical: BTreeMap<&'static str, LogicalId>,
}
/// `pub/a` (repo, not base), `pub/b` (repo, base), `bill/x` (billing),
/// `sec/s` (secret, base), all the store owner's.
fn fixture() -> Fixture {
    let disk = SimDisk::new();
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk, dir, false, 1 << 20).unwrap();
    let s = Service::new(actor::spawn(store).unwrap(), log, events).unwrap();
    s.set_principals(
        PrincipalMap::new(
            Some("alice"),
            fsguard::current_uid(),
            [
                ("bob".to_owned(), 4242, PrincipalId(16)),
                ("carol".to_owned(), 4243, PrincipalId(17)),
            ],
        )
        .unwrap(),
    );
    let r = s
        .handle(
            ADMIN,
            Request::SetAccess {
                doc: doc(),
                client_txn_id: None,
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    let recs = [
        ("pub/a", "repo", false),
        ("pub/b", "repo", true),
        ("bill/x", "billing", false),
        ("sec/s", "secret", true),
    ];
    let result = match s
        .handle(
            ADMIN,
            Request::Raw {
                blobs: recs.iter().map(|(k, ..)| body(k)).collect(),
                batch: Batch::new(
                    recs.iter()
                        .map(|(k, lane, base)| Op::CreateLogical {
                            key: (*k).into(),
                            kind: RecordKind::Fact,
                            scope_lane: (*lane).into(),
                            content: h(&body(k)),
                            adopt_as_base: *base,
                        })
                        .collect(),
                ),
            },
        )
        .response
    {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    };
    let names = recs.map(|(k, ..)| k);
    Fixture {
        s,
        rev: names
            .iter()
            .copied()
            .zip(result.allocations.new_revisions.iter().copied())
            .collect(),
        logical: names
            .iter()
            .copied()
            .zip(result.allocations.new_logicals.iter().copied())
            .collect(),
    }
}
fn start(s: &Svc, id: Identity) -> SessionId {
    match s
        .handle(
            id,
            Request::SessionStart {
                harness: "test".into(),
                repo: "r".into(),
                head_sha: "h".into(),
                parent: None,
                branch: None,
                git_branch: None,
            },
        )
        .response
    {
        Response::Session { session } => session,
        other => panic!("{other:?}"),
    }
}
fn commit(
    s: &Svc,
    id: Identity,
    session: SessionId,
    ops: Vec<Op>,
    blobs: Vec<Vec<u8>>,
) -> Response {
    s.handle(
        id,
        Request::Commit {
            binding: Binding::root(session),
            ops,
            blobs,
            expect_cert_valid: false,
            client_txn_id: None,
        },
    )
    .response
}
fn what_if(
    s: &Svc,
    id: Identity,
    session: SessionId,
    ops: Vec<Op>,
    blobs: Vec<Vec<u8>>,
) -> Response {
    s.handle(
        id,
        Request::WhatIf {
            binding: Binding::root(session),
            ops,
            blobs,
        },
    )
    .response
}
fn impact(s: &Svc, id: Identity, ops: Vec<Op>, blobs: Vec<Vec<u8>>) -> Response {
    s.handle(
        id,
        Request::ImpactBatch {
            batch: Batch::new(ops),
            blobs,
        },
    )
    .response
}
fn code(r: &Response) -> String {
    match r {
        Response::Error { code, .. } => code.clone(),
        _ => "ok".into(),
    }
}
fn detail(r: &Response) -> String {
    match r {
        Response::Error { detail, .. } => detail.clone(),
        other => format!("{other:?}"),
    }
}
/// `Unauthorized` with this need and reference.
fn unauthorized(r: &Response, need: &str, reference: &str) {
    assert_eq!(code(r), "Unauthorized", "{r:?}");
    let d = detail(r);
    assert!(d.contains(&format!("need: {need}")), "{d}");
    assert!(d.contains(&format!("reference: \"{reference}\"")), "{d}");
}
fn create(key: &str, lane: &str, base: bool) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: lane.into(),
        content: h(&body(key)),
        adopt_as_base: base,
    }
}
fn rule(conclusion: RevisionId, premises: &[RevisionId]) -> Op {
    Op::AddRule {
        conclusion: RevRef::Existing(conclusion),
        incidences: premises
            .iter()
            .map(|p| (RevRef::Existing(*p), EdgeKind::Cur))
            .collect(),
        enabled: true,
    }
}

/// T102, rule 1: creating needs a writer of the lane, a base a curator;
/// `personal:<unlisted>` is `InvalidLane` for everyone.
#[test]
fn t102_creates_need_the_lane_s_writers_and_curators() {
    let f = fixture();
    let s = &f.s;
    let (bob, carol, alice) = (start(s, BOB), start(s, CAROL), start(s, OWNER_AGENT));
    let c = |id, session, key: &str, lane: &str, base| {
        commit(
            s,
            id,
            session,
            vec![create(key, lane, base)],
            vec![body(key)],
        )
    };
    assert_eq!(code(&c(BOB, bob, "pub/new", "repo", false)), "ok");
    unauthorized(
        &c(BOB, bob, "pub/based", "repo", true),
        "Curate",
        "pub/based",
    );
    unauthorized(
        &c(CAROL, carol, "bill/c", "billing", false),
        "Write",
        "bill/c",
    );
    unauthorized(&c(CAROL, carol, "sec/c", "secret", false), "Write", "sec/c");
    assert_eq!(code(&c(BOB, bob, "bill/b", "billing", false)), "ok");
    // A personal lane: its principal writes and curates it, nobody else.
    assert_eq!(
        code(&c(CAROL, carol, "mine/1", "personal:carol", true)),
        "ok"
    );
    unauthorized(
        &c(BOB, bob, "mine/2", "personal:carol", false),
        "Write",
        "mine/2",
    );
    for (id, session) in [(CAROL, carol), (OWNER_AGENT, alice)] {
        assert_eq!(
            code(&c(id, session, "z/1", "personal:zed", false)),
            "InvalidLane"
        );
    }
    // The store owner passes every check.
    assert_eq!(
        code(&c(OWNER_AGENT, alice, "sec/new", "secret", true)),
        "ok"
    );
}

/// T102, rules 2–5: changes, bases, rules and artifacts. A ref to a hidden
/// logical is `Unauthorized(Read)` whether or not the revisions the op
/// also names exist.
#[test]
fn t102_changes_bases_rules_and_artifacts() {
    let f = fixture();
    let s = &f.s;
    let (bob, carol, alice) = (start(s, BOB), start(s, CAROL), start(s, OWNER_AGENT));
    let r = |k| f.rev[k];
    // Rule 2: a revision or head of a hidden logical.
    let add = |logical: LogicalRef| Op::AddRevision {
        logical,
        content: h(b"new"),
        adopt_as_base: false,
    };
    let rr = commit(
        s,
        CAROL,
        carol,
        vec![add(LogicalRef::Key("sec/s".into()))],
        vec![b"new".to_vec()],
    );
    unauthorized(&rr, "Read", "sec/s");
    let id = LogicalRef::Existing(f.logical["sec/s"]);
    let rr = commit(
        s,
        CAROL,
        carol,
        vec![add(id.clone())],
        vec![b"new".to_vec()],
    );
    unauthorized(&rr, "Read", &format!("logical {}", f.logical["sec/s"].0));
    for missing in [RevisionId(999), r("sec/s")] {
        let head = Op::SetHead {
            logical: LogicalRef::Key("sec/s".into()),
            expected: missing.into(),
            new: missing.into(),
        };
        unauthorized(
            &commit(s, CAROL, carol, vec![head.clone()], vec![]),
            "Read",
            "sec/s",
        );
        if missing == RevisionId(999) {
            // The store owner learns that the revision does not exist.
            assert_eq!(
                code(&commit(s, OWNER_AGENT, alice, vec![head], vec![])),
                "UnknownRef"
            );
        }
    }
    // Rule 3: bases need a curator.
    for (op, rev) in [
        (
            Op::AssertBase {
                rev: r("pub/a").into(),
            },
            r("pub/a"),
        ),
        (
            Op::RetractBase {
                rev: r("pub/b").into(),
            },
            r("pub/b"),
        ),
    ] {
        let rr = commit(s, BOB, bob, vec![op], vec![]);
        unauthorized(&rr, "Curate", &format!("rev {}", rev.0));
    }
    let rr = commit(
        s,
        BOB,
        bob,
        vec![add(LogicalRef::Key("pub/a".into()))],
        vec![b"new".to_vec()],
    );
    assert_eq!(code(&rr), "ok", "a writer revises");
    // Rule 4: every premise read, the conclusion's lane written.
    unauthorized(
        &commit(
            s,
            CAROL,
            carol,
            vec![rule(r("pub/a"), &[r("sec/s")])],
            vec![],
        ),
        "Read",
        &format!("rev {}", r("sec/s").0),
    );
    unauthorized(
        &commit(
            s,
            CAROL,
            carol,
            vec![rule(r("bill/x"), &[r("pub/b")])],
            vec![],
        ),
        "Read",
        &format!("rev {}", r("bill/x").0),
    );
    assert_eq!(
        code(&commit(
            s,
            BOB,
            bob,
            vec![rule(r("pub/a"), &[r("bill/x")])],
            vec![]
        )),
        "ok",
        "bob reads billing and writes repo"
    );
    // Rule 5: an artifact on a hidden target.
    let open = Op::OpenArtifact {
        ty: ArtifactType::Conflict,
        target: r("sec/s").into(),
        payload_hash: h(b"why"),
    };
    unauthorized(
        &commit(s, CAROL, carol, vec![open], vec![b"why".to_vec()]),
        "Read",
        &format!("rev {}", r("sec/s").0),
    );
    // Rule 2: metadata on a writable logical.
    let meta = Op::SetMetadata {
        logical: LogicalRef::Key("pub/a".into()),
        tags: BTreeSet::new(),
        dirs: set(&["src"]),
    };
    assert_eq!(code(&commit(s, CAROL, carol, vec![meta], vec![])), "ok");
}

/// T102: `ctx_what_if` and `ImpactBatch` refuse exactly what the commit
/// refuses (one predicate, §6.1), and accept what it accepts.
#[test]
fn t102_dry_runs_refuse_exactly_what_the_commit_refuses() {
    let f = fixture();
    let s = &f.s;
    let carol = start(s, CAROL);
    let r = |k| f.rev[k];
    let cases: Vec<(Vec<Op>, Vec<Vec<u8>>)> = vec![
        (vec![create("pub/c", "repo", true)], vec![body("pub/c")]),
        (
            vec![create("bill/c", "billing", false)],
            vec![body("bill/c")],
        ),
        (
            vec![create("z/c", "personal:zed", false)],
            vec![body("z/c")],
        ),
        (vec![rule(r("pub/a"), &[r("sec/s")])], vec![]),
        (
            vec![Op::SetHead {
                logical: LogicalRef::Key("sec/s".into()),
                expected: RevisionId(999).into(),
                new: RevisionId(999).into(),
            }],
            vec![],
        ),
        (
            vec![Op::RetractBase {
                rev: r("pub/b").into(),
            }],
            vec![],
        ),
        (
            vec![Op::CreateLogical {
                key: "pub/stolen".into(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: h(&body("sec/s")),
                adopt_as_base: false,
            }],
            vec![],
        ),
        (
            vec![create("pub/fine", "repo", false)],
            vec![body("pub/fine")],
        ),
    ];
    for (ops, blobs) in cases {
        let dry = what_if(s, CAROL, carol, ops.clone(), blobs.clone());
        let batch = impact(s, CAROL, ops.clone(), blobs.clone());
        let real = commit(s, CAROL, carol, ops.clone(), blobs);
        assert_eq!(code(&dry), code(&real), "{ops:?}");
        assert_eq!(code(&batch), code(&real), "{ops:?}");
        if code(&real) != "ok" {
            assert_eq!(detail(&dry), detail(&real));
            assert_eq!(detail(&batch), detail(&real));
        }
    }
}

/// T107: a content hash without its bytes is `MissingBlob` for a
/// non-owner, whether someone holds the bytes or nobody does, in every
/// content-hash field and in dry runs; the bytes, or a hash already on a
/// revision the principal reads, are enough.
#[test]
fn t107_proof_of_possession() {
    let f = fixture();
    let s = &f.s;
    let (carol, alice) = (start(s, CAROL), start(s, OWNER_AGENT));
    let known = h(&body("sec/s"));
    let unknown = h(b"nobody has these bytes");
    let mk = |key: &str, content| Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content,
        adopt_as_base: false,
    };
    for content in [known, unknown] {
        let want = format!("{:?}", TxError::MissingBlob { content });
        for r in [
            commit(s, CAROL, carol, vec![mk("pub/p", content)], vec![]),
            what_if(s, CAROL, carol, vec![mk("pub/p", content)], vec![]),
            impact(s, CAROL, vec![mk("pub/p", content)], vec![]),
        ] {
            assert_eq!(code(&r), "MissingBlob");
            assert_eq!(detail(&r), want, "the same answer for both hashes");
        }
    }
    // Every content-hash field: a revision, an artifact payload.
    let add = Op::AddRevision {
        logical: LogicalRef::Key("pub/a".into()),
        content: known,
        adopt_as_base: false,
    };
    assert_eq!(
        code(&commit(s, CAROL, carol, vec![add], vec![])),
        "MissingBlob"
    );
    let open = Op::OpenArtifact {
        ty: ArtifactType::Conflict,
        target: f.rev["pub/a"].into(),
        payload_hash: known,
    };
    assert_eq!(
        code(&commit(s, CAROL, carol, vec![open], vec![])),
        "MissingBlob"
    );
    // The bytes prove possession.
    let r = commit(
        s,
        CAROL,
        carol,
        vec![mk("pub/p", known)],
        vec![body("sec/s")],
    );
    assert_eq!(code(&r), "ok", "{r:?}");
    // A hash on a revision carol reads needs no bytes.
    let r = commit(
        s,
        CAROL,
        carol,
        vec![mk("pub/q", h(&body("pub/b")))],
        vec![],
    );
    assert_eq!(code(&r), "ok", "{r:?}");
    // The store owner is not asked (the store holds the bytes).
    let r = commit(s, OWNER_AGENT, alice, vec![mk("pub/o", known)], vec![]);
    assert_eq!(code(&r), "ok", "{r:?}");
}

/// T60 / §6.2 rule 4 through `ctx_record` and `ctx_supersede`: a dep on a
/// hidden logical is `Unauthorized(Read)` before anything else is said
/// (no head hint, no "does not exist"); `carry_rules` over a hidden
/// premise is refused without naming it.
#[test]
fn deps_and_carried_rules_on_hidden_logicals() {
    let f = fixture();
    let s = &f.s;
    let (carol, alice) = (start(s, CAROL), start(s, OWNER_AGENT));
    let record = |id, session, deps: Vec<Dep>| {
        s.handle(
            id,
            Request::Record {
                binding: Binding::root(session),
                kind: RecordKind::Fact,
                body: b"claim".to_vec(),
                target: WriteTarget::Create {
                    key: "pub/claim".into(),
                    scope_lane: "repo".into(),
                },
                deps,
                adopt_as_base: false,
                client_txn_id: None,
            },
        )
        .response
    };
    for target in [
        Target::KeyRev {
            key: "sec/s".into(),
            rev: RevisionId(999),
        },
        Target::KeyRev {
            key: "sec/s".into(),
            rev: f.rev["sec/s"],
        },
        Target::Key("sec/s".into()),
    ] {
        let r = record(
            CAROL,
            carol,
            vec![Dep {
                target,
                kind: EdgeKind::Cur,
            }],
        );
        assert_eq!(code(&r), "Unauthorized", "{r:?}");
        assert!(!detail(&r).contains("head"), "{r:?}");
    }
    // The owner justifies pub/a by sec/s; carol may revise pub/a, but not
    // carry that justification.
    let r = commit(
        s,
        OWNER_AGENT,
        alice,
        vec![rule(f.rev["pub/a"], &[f.rev["sec/s"]])],
        vec![],
    );
    assert_eq!(code(&r), "ok", "{r:?}");
    let supersede = |carry| {
        s.handle(
            CAROL,
            Request::Supersede {
                binding: Binding::root(carol),
                logical: f.logical["pub/a"],
                body: b"revised".to_vec(),
                expected_head: f.rev["pub/a"],
                adopt_as_base: false,
                carry_rules: carry,
                reason: "edit".into(),
                client_txn_id: None,
            },
        )
        .response
    };
    let r = supersede(true);
    unauthorized(&r, "Read", "carry_rules");
    assert!(!detail(&r).contains(&format!("rev {}", f.rev["sec/s"].0)));
    assert_eq!(code(&supersede(false)), "ok");
}

fn branch(s: &Svc, id: Identity, name: &str) -> Response {
    s.handle(
        id,
        Request::BranchCreate {
            name: name.into(),
            from: None,
            client_txn_id: None,
        },
    )
    .response
}
fn merge(s: &Svc, id: Identity, source: &str, into: Option<&str>) -> Response {
    s.handle(
        id,
        Request::Merge {
            binding: None,
            source: source.into(),
            into: into.map(str::to_owned),
            require_certified: false,
            certify: Vec::new(),
            client_txn_id: None,
        },
    )
    .response
}
fn session_on(s: &Svc, id: Identity, branch: Option<&str>, git: Option<&str>) -> SessionId {
    match s
        .handle(
            id,
            Request::SessionStart {
                harness: "test".into(),
                repo: "r".into(),
                head_sha: "h".into(),
                parent: None,
                branch: branch.map(str::to_owned),
                git_branch: git.map(str::to_owned),
            },
        )
        .response
    {
        Response::Session { session } => session,
        other => panic!("{other:?}"),
    }
}

/// T108 (ADR 0017 §6.2 rules 6–7): branches belong to their creator;
/// merges into `main` or an owner branch apply only what the merger could
/// do directly (`MergeUnauthorized{count}`, naming nothing); verdicts are
/// transported with their exact `(rule, curvec)` and validator; a merge
/// into one's own branch catches up without checks; the store owner
/// merges anything.
#[test]
fn t108_branches_and_merges() {
    let f = fixture();
    let s = &f.s;
    let r = |k| f.rev[k];
    // Rule 6: bob's branch takes commits and its archive from bob only.
    assert_eq!(code(&branch(s, BOB, "feat-bob")), "ok");
    let carol_there = session_on(s, CAROL, Some("feat-bob"), None);
    let rr = commit(
        s,
        CAROL,
        carol_there,
        vec![create("pub/c1", "repo", false)],
        vec![body("pub/c1")],
    );
    assert_eq!(code(&rr), "BranchOfAnotherPrincipal");
    let bob_there = session_on(s, BOB, Some("feat-bob"), None);
    let rr = commit(
        s,
        BOB,
        bob_there,
        vec![create("pub/b1", "repo", false)],
        vec![body("pub/b1")],
    );
    assert_eq!(code(&rr), "ok", "{rr:?}");
    let archive = |id, name: &str| {
        s.handle(
            id,
            Request::BranchArchive {
                name: name.into(),
                client_txn_id: None,
            },
        )
        .response
    };
    assert_eq!(
        code(&archive(CAROL, "feat-bob")),
        "BranchOfAnotherPrincipal"
    );
    // Auto-binding skips a name another principal created.
    let auto = |id| {
        let session = session_on(s, id, None, Some("feat-bob"));
        let rr = commit(
            s,
            id,
            session,
            vec![create(
                &format!("pub/auto{}", id.principal.0),
                "repo",
                false,
            )],
            vec![body(&format!("pub/auto{}", id.principal.0))],
        );
        assert_eq!(code(&rr), "ok", "{rr:?}");
        // Where did it land? main sees it only if the session bound main.
        let alice = start(s, OWNER_AGENT);
        matches!(
            s.handle(
                OWNER_AGENT,
                Request::Read {
                    binding: Binding::root(alice),
                    target: Target::Key(format!("pub/auto{}", id.principal.0)),
                    mode: Mode::Advisory,
                    level: None,
                },
            )
            .response,
            Response::Read { .. }
        )
    };
    assert!(auto(CAROL), "carol's session bound main, not bob's branch");
    assert!(!auto(BOB), "bob's session bound his own branch");
    // Bob merges his branch into main: every change is one he could make.
    assert_eq!(code(&merge(s, BOB, "feat-bob", Some("main"))), "ok");

    // An owner branch carrying changes bob could not make directly.
    assert_eq!(code(&branch(s, OWNER_AGENT, "owner-work")), "ok");
    let alice_there = session_on(s, OWNER_AGENT, Some("owner-work"), None);
    let rr = commit(
        s,
        OWNER_AGENT,
        alice_there,
        vec![
            create("sec/new", "secret", true),
            Op::AssertBase {
                rev: r("pub/a").into(),
            },
        ],
        vec![body("sec/new")],
    );
    assert_eq!(code(&rr), "ok", "{rr:?}");
    // New logical and revision in `secret` (2: no Write, no Read), and the
    // base bit on pub/a (no Curate); the new revision arrives as base.
    let rr = merge(s, BOB, "owner-work", Some("main"));
    assert_eq!(code(&rr), "MergeUnauthorized", "{rr:?}");
    assert!(!detail(&rr).contains("sec/new") && !detail(&rr).contains("secret"));
    assert!(detail(&rr).contains("count"), "{rr:?}");
    // Catching up: bob merges main and the owner branch into his own branch.
    assert_eq!(code(&branch(s, BOB, "bob-2")), "ok");
    let rr = merge(s, BOB, "owner-work", Some("bob-2"));
    assert_eq!(
        code(&rr),
        "ok",
        "a merge into one's own branch is not checked: {rr:?}"
    );
    // The store owner merges anything.
    assert_eq!(
        code(&merge(s, OWNER_AGENT, "owner-work", Some("main"))),
        "ok"
    );

    // A transported verdict keeps its rule, curvec and validator.
    assert_eq!(code(&branch(s, OWNER_AGENT, "verdicts")), "ok");
    let on_verdicts = session_on(s, OWNER_AGENT, Some("verdicts"), None);
    let rr = commit(
        s,
        OWNER_AGENT,
        on_verdicts,
        vec![Op::AddRule {
            conclusion: r("pub/a").into(),
            incidences: vec![(r("pub/b").into(), EdgeKind::Sem)],
            enabled: true,
        }],
        vec![],
    );
    let Response::Committed { result } = rr else {
        panic!("{rr:?}")
    };
    let rule = result.allocations.new_rules[0];
    let curvec = vec![(f.logical["pub/b"], r("pub/b"))];
    let rr = s
        .handle(
            ADMIN,
            Request::OnBranch {
                branch: BranchSel::Named("verdicts".into()),
                request: Box::new(Request::Validate {
                    rule,
                    verdict: Verdict::Verified,
                    curvec_seen: curvec.clone(),
                    validator: "human:alice".into(),
                    label: None,
                    rationale: "checked".into(),
                    evidence: Vec::new(),
                    from_review: None,
                }),
            },
        )
        .response;
    assert_eq!(code(&rr), "ok", "{rr:?}");
    // Bob may add that rule directly (he reads both, writes repo); the
    // verdict is transported, not his.
    let rr = merge(s, BOB, "verdicts", Some("main"));
    assert_eq!(code(&rr), "ok", "{rr:?}");
    let snap = s.actor.snapshot();
    let main = snap.view(MAIN);
    let records = main.rule_validations(rule);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].0, curvec);
    assert_eq!(records[0].1.verdict, Verdict::Verified);
    assert_eq!(records[0].1.validator, "human:alice");
}
