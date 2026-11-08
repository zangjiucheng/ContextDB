//! P9 milestone 3 (ADR 0017 §5): reads.
//!
//! - T17: a hidden logical read by key is UNAVAILABLE (the key echoed, no
//!   payload, no ledger entry); recall skips hidden matches before they take
//!   a slot, so it returns `min(limit, |authorised eligible|)` items.
//! - T17b: WHY redaction (`Redacted{count}`, `redacted_rules`, a redacted
//!   witness), WHY_NOT and IMPACT counts.
//! - T100: no hidden name in diff-shaped responses, transactions, errors or
//!   certificates (`vector_size` only).
//! - T101: revocation makes earlier certificates and ledger reads `Denied`.
//! - S13-rows-e2e: the UNAVAILABLE row end to end.
//! - PROP15 (read half): random histories and access documents; the
//!   redaction point against an independent reference.
use contextdb_core::{BranchView, Kernel, Proof};
use contextdb_daemon::{
    actor,
    principals::PrincipalMap,
    proto::*,
    redact::Viewer,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_oracle::AccessModel;
use contextdb_proto::fsguard;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
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
const WITHHELD: &str = "details withheld (ADR 0017 §5.7)";

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}
fn h(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}
/// alice owns the store; bob and carol share `repo`; only bob (group
/// `billing`) reads `billing`; `secret` falls to the owner-only default.
fn doc(repo_members: &[&str]) -> AccessDoc {
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
                    members: set(repo_members),
                    writers: set(repo_members),
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
fn service() -> Svc {
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
    set_access(&s, doc(&["*members"]));
    s
}
fn set_access(s: &Svc, d: AccessDoc) {
    let r = s
        .handle(
            ADMIN,
            Request::SetAccess {
                doc: d,
                client_txn_id: None,
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
}
fn raw(s: &Svc, blobs: Vec<Vec<u8>>, ops: Vec<Op>, txn: [u8; 16]) -> CommitResult {
    let mut batch = Batch::new(ops);
    batch.client_txn_id = Some(txn);
    match s.handle(ADMIN, Request::Raw { blobs, batch }).response {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
/// The fixture: `pub/a` (repo, not base) is supported by rule j0 over
/// `sec/s` and `pub/b`, and by rule j1 over `sec/s` alone; `bill/x`
/// (billing) by j2 over `pub/b`. Revisions: pub/a 0, pub/b 1, bill/x 2,
/// sec/s 3 (after the access document's epoch).
struct Fixture {
    s: Svc,
    rev: BTreeMap<&'static str, RevisionId>,
}
fn fixture() -> Fixture {
    let s = service();
    let recs = [
        ("pub/a", "repo", false),
        ("pub/b", "repo", true),
        ("bill/x", "billing", false),
        ("sec/s", "secret", true),
    ];
    let result = raw(
        &s,
        recs.iter()
            .map(|(k, ..)| format!("body of {k}").into_bytes())
            .collect(),
        recs.iter()
            .map(|(k, lane, base)| Op::CreateLogical {
                key: (*k).into(),
                kind: RecordKind::Fact,
                scope_lane: (*lane).into(),
                content: h(format!("body of {k}").as_bytes()),
                adopt_as_base: *base,
            })
            .collect(),
        [1; 16],
    );
    let rev: BTreeMap<_, _> = recs
        .iter()
        .map(|(k, ..)| *k)
        .zip(result.allocations.new_revisions.iter().copied())
        .collect();
    let rule = |c: &str, ps: &[&str]| Op::AddRule {
        conclusion: RevRef::Existing(rev[c]),
        incidences: ps
            .iter()
            .map(|p| (RevRef::Existing(rev[p]), EdgeKind::Cur))
            .collect(),
        enabled: true,
    };
    raw(
        &s,
        Vec::new(),
        vec![
            rule("pub/a", &["sec/s", "pub/b"]),
            rule("pub/a", &["sec/s"]),
            rule("bill/x", &["pub/b"]),
        ],
        [2; 16],
    );
    Fixture { s, rev }
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
fn read(s: &Svc, id: Identity, session: SessionId, target: Target, mode: Mode) -> Envelope {
    s.handle(
        id,
        Request::Read {
            binding: Binding::root(session),
            target,
            mode,
            level: None,
        },
    )
}
fn key(k: &str) -> Target {
    Target::Key(k.into())
}
fn recall(s: &Svc, id: Identity, session: SessionId, scope: &str, limit: u32) -> Vec<String> {
    match s
        .handle(
            id,
            Request::Recall {
                binding: Binding::root(session),
                query: "/".into(),
                scope: scope.into(),
                mode: Mode::Advisory,
                limit,
                index_wait_ms: 0,
                opts: RecallOpts::default(),
            },
        )
        .response
    {
        Response::Recall {
            items, withheld, ..
        } => {
            assert!(withheld.is_empty());
            items.into_iter().map(|i| i.key).collect()
        }
        other => panic!("{other:?}"),
    }
}
fn check(s: &Svc, id: Identity, session: SessionId, cert_id: Option<ContentHash>) -> Envelope {
    s.handle(
        id,
        Request::Check {
            binding: Binding::root(session),
            cert_id,
            live_pos: Vec::new(),
            action: None,
            mode: None,
            harness_version: String::new(),
            strict: false,
        },
    )
}

/// T17 and S13-rows-e2e (UNAVAILABLE): a hidden logical by key is
/// UNAVAILABLE with the key echoed and nothing else; it leaves no ledger
/// entry; the store owner reads it unfiltered.
#[test]
fn t17_hidden_reads_are_unavailable_and_leave_no_trace() {
    let f = fixture();
    let s = &f.s;
    let bob = start(s, BOB);
    for (target, echoed) in [
        (key("sec/s"), "sec/s".to_owned()),
        (
            Target::KeyRev {
                key: "sec/s".into(),
                rev: f.rev["sec/s"],
            },
            format!("sec/s@{}", f.rev["sec/s"].0),
        ),
    ] {
        for mode in [Mode::Advisory, Mode::Certified] {
            let env = read(s, BOB, bob, target.clone(), mode);
            assert_eq!(
                env.response,
                Response::Unavailable {
                    target: echoed.clone()
                }
            );
            assert_eq!(env.redaction, Some(Redaction::default()));
        }
    }
    // Carol is not in `billing`; bob is.
    let carol = start(s, CAROL);
    assert!(matches!(
        read(s, CAROL, carol, key("bill/x"), Mode::Advisory).response,
        Response::Unavailable { .. }
    ));
    assert!(matches!(
        read(s, BOB, bob, key("bill/x"), Mode::Advisory).response,
        Response::Read { .. }
    ));
    // Nothing hidden reached the ledger: the session certificate covers only
    // what bob may read.
    assert!(matches!(
        read(s, BOB, bob, key("pub/b"), Mode::Certified).response,
        Response::Read { .. }
    ));
    let env = check(s, BOB, bob, None);
    let Response::Check {
        validation, cert, ..
    } = env.response
    else {
        panic!("{:?}", env.response)
    };
    assert_eq!(validation, Validation::Valid);
    let cert = cert.unwrap();
    assert!(
        cert.entries
            .iter()
            .all(|e| e.logical != LogicalId(3) && e.rev != f.rev["sec/s"])
    );
    // T100 (certificates): the vector is withheld, its size reported.
    assert!(cert.vector.is_empty());
    let sizes = env.redaction.unwrap().vector_size;
    assert_eq!(sizes.len(), 1);
    assert_eq!(sizes[0].0, cert.cert_id);
    assert!(sizes[0].1 >= 1);
    // The store owner is never filtered.
    let alice = start(s, OWNER_AGENT);
    let env = read(s, OWNER_AGENT, alice, key("sec/s"), Mode::Advisory);
    assert!(matches!(
        env.response,
        Response::Read {
            payload: Some(_),
            ..
        }
    ));
    assert_eq!(env.redaction, None);
}

/// T17 (recall): hidden matches are skipped before they take a slot; a
/// scope naming a lane the principal is not a member of returns nothing,
/// without an error.
#[test]
fn t17_recall_returns_min_limit_authorised() {
    let f = fixture();
    let s = &f.s;
    let (bob, carol, alice) = (start(s, BOB), start(s, CAROL), start(s, OWNER_AGENT));
    let mut b = recall(s, BOB, bob, "", 10);
    b.sort();
    assert_eq!(b, ["bill/x", "pub/a", "pub/b"]);
    let mut c = recall(s, CAROL, carol, "", 10);
    c.sort();
    assert_eq!(c, ["pub/a", "pub/b"]);
    assert_eq!(recall(s, OWNER_AGENT, alice, "", 10).len(), 4);
    for limit in 1..=3 {
        assert_eq!(recall(s, BOB, bob, "", limit).len(), limit.min(3) as usize);
        assert_eq!(
            recall(s, CAROL, carol, "", limit).len(),
            limit.min(2) as usize
        );
    }
    assert!(recall(s, CAROL, carol, "secret", 10).is_empty());
    assert!(recall(s, CAROL, carol, "billing", 10).is_empty());
    assert_eq!(recall(s, BOB, bob, "billing", 10), ["bill/x"]);
}

/// T17b: WHY shows authorised premises plus counts; the witness rule has a
/// hidden premise, so it is redacted. WHY_NOT, IMPACT and items too.
#[test]
fn t17b_why_why_not_and_impact_are_redacted() {
    let f = fixture();
    let s = &f.s;
    let on = |id: Identity, request: Request| s.handle(id, request);
    let env = on(
        BOB,
        Request::Why {
            target: key("pub/a"),
            depth: 4,
        },
    );
    let Response::Why { proof } = env.response else {
        panic!("{:?}", env.response)
    };
    let e = &proof.explanation;
    assert!(!e.base);
    assert_eq!(e.witness, Some(Witness::Redacted));
    assert_eq!(
        e.active_rules,
        vec![RuleId(0)],
        "j1's premises are all hidden"
    );
    assert_eq!(proof.premises.len(), 1, "pub/b only");
    assert_eq!(proof.premises[0].explanation.revision, f.rev["pub/b"]);
    assert_eq!(
        env.redaction.unwrap().nodes,
        vec![NodeRedaction {
            rev: f.rev["pub/a"],
            rule: None,
            premises: 1,
            rules: 1,
        }]
    );
    // The store owner sees the whole proof.
    let env = on(
        OWNER_AGENT,
        Request::Why {
            target: key("pub/a"),
            depth: 4,
        },
    );
    let Response::Why { proof } = env.response else {
        panic!()
    };
    assert_eq!(proof.explanation.witness, Some(Witness::Rule(RuleId(0))));
    assert_eq!(proof.premises.len(), 2);
    // A hidden target: UNAVAILABLE, the target echoed.
    for request in [
        Request::Why {
            target: key("sec/s"),
            depth: 2,
        },
        Request::WhyNot {
            target: key("sec/s"),
            depth: 2,
        },
        Request::Impact {
            target: key("sec/s"),
        },
    ] {
        assert_eq!(
            on(CAROL, request).response,
            Response::Unavailable {
                target: "sec/s".into()
            }
        );
    }
    // IMPACT of pub/b: pub/a and bill/x depend on it; carol cannot read
    // bill/x.
    let env = on(
        CAROL,
        Request::Impact {
            target: key("pub/b"),
        },
    );
    let Response::Impact { impact } = env.response else {
        panic!()
    };
    assert_eq!(impact.supported, vec![f.rev["pub/a"]]);
    assert_eq!(env.redaction.unwrap().redacted, 1);
    // WHY_NOT of sec/s's dependant once sec/s loses its base: the failing
    // incidence on sec/s is counted, never named.
    raw(
        s,
        Vec::new(),
        vec![Op::RetractBase {
            rev: RevRef::Existing(f.rev["sec/s"]),
        }],
        [3; 16],
    );
    let env = on(
        CAROL,
        Request::WhyNot {
            target: key("pub/a"),
            depth: 3,
        },
    );
    let Response::WhyNot { why_not } = env.response else {
        panic!("{:?}", env.response)
    };
    let shown: Vec<RuleId> = why_not.rules.iter().map(|r| r.rule).collect();
    assert_eq!(shown, vec![RuleId(0)]);
    for f in &why_not.rules {
        assert!(f.upstream.is_empty());
        assert!(f.incidences.iter().all(|i| !matches!(
            i,
            IncidenceFailure::Unsupported(r) if *r == RevisionId(3)
        )));
    }
    let nodes = env.redaction.unwrap().nodes;
    assert!(
        nodes.contains(&NodeRedaction {
            rev: f.rev["pub/a"],
            rule: None,
            premises: 0,
            rules: 1
        }),
        "{nodes:?}"
    );
    assert!(
        nodes
            .iter()
            .any(|n| n.rule == Some(RuleId(0)) && n.premises >= 1),
        "{nodes:?}"
    );
    // The item: a witness naming a rule with a hidden premise is redacted.
    let bob = start(s, BOB);
    let env = read(s, BOB, bob, key("bill/x"), Mode::Advisory);
    let Response::Read { item, .. } = env.response else {
        panic!()
    };
    assert!(
        (item.state.witness.iter().flatten()).all(|w| *w != Witness::Redacted),
        "j2's premise pub/b is visible to bob"
    );
}

/// T100: the same epoch touches visible and hidden logicals; every
/// diff-shaped response names only the visible ones and counts the rest.
/// A non-owner sees only its own transactions; error details are withheld.
#[test]
fn t100_no_hidden_names_in_diffs_transactions_and_errors() {
    let f = fixture();
    let s = &f.s;
    let before = s.handle(ADMIN, Request::Status).epoch;
    // One owner batch moving both sides: retract pub/b's and sec/s's bases.
    raw(
        s,
        Vec::new(),
        vec![
            Op::RetractBase {
                rev: RevRef::Existing(f.rev["pub/b"]),
            },
            Op::RetractBase {
                rev: RevRef::Existing(f.rev["sec/s"]),
            },
        ],
        [9; 16],
    );
    let after = s.handle(ADMIN, Request::Status).epoch;
    // Diff.
    let owner = match s
        .handle(
            OWNER_AGENT,
            Request::Diff {
                from: before,
                to: after,
            },
        )
        .response
    {
        Response::Diff { diff } => diff,
        other => panic!("{other:?}"),
    };
    let env = s.handle(
        CAROL,
        Request::Diff {
            from: before,
            to: after,
        },
    );
    let Response::Diff { diff } = env.response else {
        panic!()
    };
    let visible = |r: RevisionId| r == f.rev["pub/a"] || r == f.rev["pub/b"];
    assert!(diff.changes.iter().all(|c| visible(c.revision)));
    let hidden = owner
        .changes
        .iter()
        .filter(|c| !visible(c.revision))
        .count();
    assert!(hidden >= 2, "sec/s and bill/x changed");
    assert_eq!(env.redaction.unwrap().redacted as usize, hidden);
    // Changes (bound).
    let carol = start(s, CAROL);
    let env = s.handle(
        CAROL,
        Request::Changes {
            binding: Some(Binding::root(carol)),
            branch: None,
            since_epoch: before,
            scope: None,
            max_items: 100,
            wait_ms: 0,
        },
    );
    let Response::Changes { items, .. } = env.response else {
        panic!("{:?}", env.response)
    };
    assert!(items.iter().all(|i| i.key.starts_with("pub/")));
    assert!(env.redaction.unwrap().redacted >= 2);
    // TxnStatus: a transaction exists only for its own principal (§5.5).
    let alice = start(s, OWNER_AGENT);
    let record = |id: Identity, session: SessionId, k: &str, txn: [u8; 16]| {
        let r = s
            .handle(
                id,
                Request::Record {
                    binding: Binding::root(session),
                    kind: RecordKind::Fact,
                    body: k.as_bytes().to_vec(),
                    target: WriteTarget::Create {
                        key: k.into(),
                        scope_lane: "repo".into(),
                    },
                    deps: Vec::new(),
                    // A base in `repo` needs a curator (milestone 4).
                    adopt_as_base: id.principal.is_store_owner(),
                    client_txn_id: Some(txn),
                },
            )
            .response;
        assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    };
    record(OWNER_AGENT, alice, "pub/by-alice", [9; 16]);
    record(CAROL, carol, "pub/by-carol", [10; 16]);
    let status =
        |id: Identity, txn: [u8; 16]| match s.handle(id, Request::TxnStatus { id: txn }).response {
            Response::TxnStatus { committed } => committed.is_some(),
            other => panic!("{other:?}"),
        };
    assert!(status(OWNER_AGENT, [9; 16]));
    assert!(!status(CAROL, [9; 16]), "as for an unknown id");
    assert!(status(CAROL, [10; 16]));
    assert!(!status(BOB, [10; 16]));
    assert!(
        status(ADMIN, [9; 16]),
        "the store owner's channels are one principal"
    );
    // Errors keep their code and lose their detail.
    let env = read(s, CAROL, carol, key("nope"), Mode::Advisory);
    assert_eq!(
        env.response,
        Response::Error {
            code: "UnknownRef".into(),
            detail: WITHHELD.into()
        }
    );
    // A store-owner request is refused, not redacted.
    assert!(matches!(
        s.handle(CAROL, Request::Status).response,
        Response::Error { code, .. } if code == "ChannelRestricted"
    ));
}

/// T101: a revocation makes earlier certificates and ledger reads `Denied`
/// (UNAVAILABLE → Denied); re-issuance refuses the hidden entry.
#[test]
fn t101_revocation_denies_earlier_certificates() {
    let f = fixture();
    let s = &f.s;
    let bob = start(s, BOB);
    assert!(matches!(
        read(s, BOB, bob, key("pub/b"), Mode::Certified).response,
        Response::Read { .. }
    ));
    let Response::Check {
        validation, cert, ..
    } = check(s, BOB, bob, None).response
    else {
        panic!()
    };
    assert_eq!(validation, Validation::Valid);
    let cert_id = cert.unwrap().cert_id;
    // The owner drops bob from `repo`.
    set_access(s, doc(&["alice", "carol"]));
    let Response::Check { validation, .. } = check(s, BOB, bob, Some(cert_id)).response else {
        panic!()
    };
    assert_eq!(validation, Validation::Denied);
    let Response::Check {
        validation, cert, ..
    } = check(s, BOB, bob, None).response
    else {
        panic!()
    };
    assert_eq!(validation, Validation::Denied);
    assert!(cert.is_none_or(|c| c.entries.is_empty()));
    // Reads now: UNAVAILABLE.
    assert!(matches!(
        read(s, BOB, bob, key("pub/b"), Mode::Advisory).response,
        Response::Unavailable { .. }
    ));
    // Carol is unaffected.
    let carol = start(s, CAROL);
    assert!(matches!(
        read(s, CAROL, carol, key("pub/b"), Mode::Certified).response,
        Response::Read { .. }
    ));
}

/// Handoff (ADR 0017 §3.4): the acceptor gets only the reads it may read;
/// the others are counted in `dropped_unauthorized`.
#[test]
fn handoff_drops_what_the_acceptor_may_not_read() {
    let f = fixture();
    let s = &f.s;
    let alice = start(s, OWNER_AGENT);
    for k in ["pub/b", "sec/s"] {
        assert!(matches!(
            read(s, OWNER_AGENT, alice, key(k), Mode::Certified).response,
            Response::Read { .. }
        ));
    }
    let id = match s
        .handle(
            OWNER_AGENT,
            Request::HandoffBegin {
                binding: Binding::root(alice),
                summary: "half".into(),
                to_agent: None,
                to: Some("carol".into()),
            },
        )
        .response
    {
        Response::HandoffBegun { id, .. } => id,
        other => panic!("{other:?}"),
    };
    let carol = start(s, CAROL);
    match s
        .handle(
            CAROL,
            Request::HandoffAccept {
                binding: Binding::root(carol),
                id: Some(id),
            },
        )
        .response
    {
        Response::HandoffAccepted {
            copied,
            dropped_unauthorized,
            validation,
            ..
        } => {
            assert_eq!((copied, dropped_unauthorized), (1, 1));
            assert!(
                matches!(
                    validation,
                    Validation::Valid | Validation::ValidRecertified { .. }
                ),
                "{validation:?}"
            );
        }
        other => panic!("{other:?}"),
    }
}

/// T100 over every list-bearing response type, on a pure kernel: `pub/v`
/// and `pub/w` (repo) are visible to bob, `sec/h` (secret) is not. Hidden
/// entries are dropped and counted (§5.5); feeds drop without a count
/// (§5.8); a certificate's vector is withheld (§5.6); store-owner responses
/// are refused; the store owner's own responses are untouched.
#[test]
fn t100_every_response_type_names_only_visible_ids() {
    let mut k = Kernel::default();
    k.commit(&admin(vec![Op::SetAccess {
        doc: Box::new(doc(&["*members"])),
    }]))
    .unwrap();
    let mk = |key: &str, lane: &str| Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: lane.into(),
        content: h(key.as_bytes()),
        adopt_as_base: true,
    };
    k.commit(&Batch::new(vec![
        mk("pub/v", "repo"),
        mk("sec/h", "secret"),
        mk("pub/w", "repo"),
    ]))
    .unwrap();
    let (vis, hid, w) = (RevisionId(0), RevisionId(1), RevisionId(2));
    let (lv, lh) = (LogicalId(0), LogicalId(1));
    let rule = |p: RevisionId| Op::AddRule {
        conclusion: RevRef::Existing(vis),
        incidences: vec![(RevRef::Existing(p), EdgeKind::Cur)],
        enabled: true,
    };
    k.commit(&Batch::new(vec![rule(hid), rule(w)])).unwrap();
    let (j_hid, j_vis) = (RuleId(0), RuleId(1));
    let v = k.branch(MAIN).unwrap();
    let bob = Viewer::new(PrincipalId(16), &v);
    let run = |r: Response| {
        let mut red = Redaction::default();
        let out = bob.redact(r, &mut red);
        (out, red)
    };
    let cert = Certificate {
        cert_id: [5; 32],
        session_id: [0; 16],
        principal: PrincipalId(16),
        branch: MAIN,
        issued_epoch: Epoch(1),
        log_checksum_at_issue: [0; 32],
        code_commit: String::new(),
        watermarks: Vec::new(),
        entries: [lv, lh]
            .iter()
            .zip([vis, hid])
            .map(|(l, r)| CertItem {
                logical: *l,
                rev: r,
                required: true,
                via: Via::Head,
                since_epoch: Epoch(1),
            })
            .collect(),
        vector: [(lv, vis), (lh, hid), (LogicalId(2), w)]
            .iter()
            .map(|(l, r)| VectorItem {
                logical: *l,
                rev: *r,
                since_epoch: Epoch(1),
            })
            .collect(),
        strict_no_aba: false,
    };
    let diff = |l: LogicalId, r: RevisionId| CertDiff {
        logical: l,
        cert_rev: Some(r),
        current: Some(r),
        summary: Some(Summary::Stale(StaleReason::SupersededPinnedPremise(hid))),
    };
    // Check: verdict diffs, the certificate, superseded reads, named reads.
    let (out, red) = run(Response::Check {
        decision: Decision::Warn,
        validation: Validation::Invalid {
            reason: InvalidReason::Changed,
            diffs: vec![diff(lv, vis), diff(lh, hid)],
        },
        cert: Some(Box::new(cert.clone())),
        superseded: vec![diff(lv, vis), diff(lh, hid)],
        gate: None,
        updates_pending: 0,
        named: [vis, hid]
            .iter()
            .map(|r| NamedRead {
                key: "k".into(),
                agent: None,
                read: *r,
                current: None,
                summary: Summary::Certified,
            })
            .collect(),
    });
    let Response::Check {
        validation: Validation::Invalid { diffs, .. },
        cert: Some(c),
        superseded,
        named,
        ..
    } = out
    else {
        panic!()
    };
    assert_eq!(red.redacted, 4);
    assert_eq!(red.vector_size, vec![([5; 32], 3)]);
    assert!(c.vector.is_empty() && c.entries.len() == 1 && c.entries[0].logical == lv);
    assert_eq!((diffs.len(), superseded.len(), named.len()), (1, 1, 1));
    assert_eq!(
        diffs[0].summary,
        Some(Summary::Stale(StaleReason::SupersededPinnedPremiseRedacted))
    );
    // A recertified certificate and the other verdicts.
    let (out, red) = run(Response::Refused {
        validation: Validation::ValidRecertified {
            new_cert: Box::new(cert.clone()),
            diffs: vec![diff(lh, hid)],
        },
    });
    let Response::Refused {
        validation: Validation::ValidRecertified { new_cert, diffs },
    } = out
    else {
        panic!()
    };
    assert!(new_cert.vector.is_empty() && diffs.is_empty());
    assert_eq!((red.redacted, red.vector_size.len()), (2, 1));
    let (out, _) = run(Response::Refused {
        validation: Validation::Ambiguous { logical: lh },
    });
    assert_eq!(
        out,
        Response::Refused {
            validation: Validation::Ambiguous {
                logical: contextdb_daemon::redact::REDACTED_LOGICAL
            }
        }
    );
    // Commit results and merge reports.
    let result = CommitResult {
        epoch: Epoch(9),
        allocations: Allocations::default(),
        net_delta: NetDelta {
            base_flips: vec![vis, hid],
            head_moves: vec![(lv, None, vis), (lh, None, hid)],
            enable_flips: vec![j_hid, j_vis],
            new_rules: vec![j_hid, j_vis],
        },
        l0_flips: vec![vis, hid],
        status_diff: vec![vis, hid],
    };
    let report = MergeReport {
        source: BranchId(1),
        moved_heads: vec![(lv, vis, vis), (lh, hid, hid)],
        conflicts: vec![MergeConflictEntry {
            kind: ConflictKind::Head,
            logical: Some(lh),
            rule: None,
            target: hid,
            artifact: ArtifactId(0),
            payload: Vec::new(),
        }],
        metadata_conflicts: vec![(lh, MetadataField::Dirs), (lv, MetadataField::Tags)],
        newly_visible: [1, 2, 3, 4],
        convergent: 0,
    };
    let (out, red) = run(Response::Merged {
        result: result.clone(),
        report: report.clone(),
    });
    let Response::Merged {
        result: r,
        report: m,
    } = out
    else {
        panic!()
    };
    assert_eq!(red.redacted, 6 + 3);
    assert_eq!(r.net_delta.new_rules, vec![j_vis]);
    assert_eq!(
        (
            m.moved_heads.len(),
            m.conflicts.len(),
            m.metadata_conflicts.len()
        ),
        (1, 0, 1)
    );
    let (_, red) = run(Response::WhatIf {
        result: result.clone(),
        merge: Some(report),
    });
    assert_eq!(red.redacted, 9);
    for r in [
        Response::Committed {
            result: result.clone(),
        },
        Response::ImpactExact {
            result: result.clone(),
        },
        Response::TxnStatus {
            committed: Some((Epoch(9), result.clone())),
        },
    ] {
        assert_eq!(run(r).1.redacted, 6);
    }
    // Feeds with a count (§5.5) and without (§5.8).
    let (out, red) = run(Response::Updates {
        token: 1,
        items: [(lv, vis), (lh, hid)]
            .iter()
            .map(|(l, r)| UpdateItem {
                logical: *l,
                key: "k".into(),
                cert_rev: *r,
                current: None,
                summary: Summary::Certified,
            })
            .collect(),
        more: false,
    });
    assert!(matches!(out, Response::Updates { items, .. } if items.len() == 1));
    assert_eq!(red.redacted, 1);
    let (out, red) = run(Response::Changes {
        items: [(lv, vis), (lh, hid)]
            .iter()
            .map(|(l, r)| ChangeItem {
                epoch: Epoch(2),
                logical: *l,
                rev: *r,
                key: "k".into(),
                summary: Summary::Certified,
            })
            .collect(),
        reached: Epoch(3),
        more: false,
    });
    assert!(matches!(out, Response::Changes { items, .. } if items.len() == 1));
    assert_eq!(red.redacted, 1);
    let item = |sides: &[RevisionId], rule: Option<RuleId>| ConflictItem {
        kind: "conflict".into(),
        artifacts: Vec::new(),
        sides: sides
            .iter()
            .map(|r| ConflictSide {
                rev: *r,
                key: "k".into(),
                current: true,
            })
            .collect(),
        reason: String::new(),
        origin: "agent".into(),
        rule,
        marks: Vec::new(),
        evidence: Vec::new(),
        candidate: None,
    };
    let (out, red) = run(Response::Conflicts {
        items: vec![
            item(&[vis, w], None),
            item(&[vis, hid], None),
            item(&[vis, w], Some(j_hid)),
            item(&[vis, w], Some(j_vis)),
        ],
        truncated: false,
    });
    assert!(matches!(out, Response::Conflicts { items, .. } if items.len() == 2));
    assert_eq!(
        red,
        Redaction::default(),
        "listings are filtered without a count"
    );
    // Items: a witness over a hidden premise, a hidden pinned premise.
    let mut state = v.node(vis).unwrap();
    state.witness = [Some(Witness::Rule(j_hid)), Some(Witness::Rule(j_vis)), None];
    state.dimensions.stale_reason = StaleReason::SupersededPinnedPremise(hid);
    let it = |l: LogicalId, r: RevisionId| Item {
        logical: l,
        key: "k".into(),
        kind: RecordKind::Fact,
        rev: r,
        state: state.clone(),
        summary: Summary::Stale(StaleReason::SupersededPinnedPremise(hid)),
        handle: None,
        premises: None,
        body: None,
    };
    let (out, _) = run(Response::Read {
        item: it(lv, vis),
        payload: None,
        unavailable: None,
    });
    let Response::Read { item, .. } = out else {
        panic!()
    };
    assert_eq!(
        item.state.witness,
        [Some(Witness::Redacted), Some(Witness::Rule(j_vis)), None]
    );
    assert_eq!(
        item.state.dimensions.stale_reason,
        StaleReason::SupersededPinnedPremiseRedacted
    );
    assert_eq!(
        item.summary,
        Summary::Stale(StaleReason::SupersededPinnedPremiseRedacted)
    );
    assert_eq!(
        run(Response::Read {
            item: it(lh, hid),
            payload: Some(b"secret".to_vec()),
            unavailable: None,
        })
        .0,
        Response::Unavailable {
            target: String::new()
        }
    );
    let (out, _) = run(Response::Recall {
        items: vec![it(lv, vis), it(lh, hid)],
        withheld: vec![it(lh, hid)],
        cert_id: None,
        index: IndexReport {
            fts: IndexState::Disabled,
            vectors: IndexState::Disabled,
        },
        limit_clamped: false,
        omitted: 0,
        estimate: None,
    });
    assert!(
        matches!(out, Response::Recall { items, withheld, .. } if items.len() == 1 && withheld.is_empty())
    );
    // Errors and store-owner responses.
    // A cycle's path may run through hidden nodes: withheld. A head
    // conflict comes only after the read check (milestone 4): shown.
    assert_eq!(
        run(Response::Error {
            code: "CycleDetected".into(),
            detail: "path [Revision(1)]".into()
        })
        .0,
        Response::Error {
            code: "CycleDetected".into(),
            detail: WITHHELD.into()
        }
    );
    let conflict = Response::Error {
        code: "HeadConflict".into(),
        detail: "HeadConflict { logical: 0 }".into(),
    };
    assert_eq!(run(conflict.clone()).0, conflict);
    assert!(matches!(
        run(Response::Status {
            revisions: 3,
            logicals: 3,
            sessions: 0,
            index: IndexReport {
                fts: IndexState::Disabled,
                vectors: IndexState::Disabled,
            },
            purge_pending: vec![],
        })
        .0,
        Response::Error { code, .. } if code == "ChannelRestricted"
    ));
    // The store owner: untouched.
    let owner = Viewer::new(PrincipalId(0), &v);
    let r = Response::Changes {
        items: vec![ChangeItem {
            epoch: Epoch(2),
            logical: lh,
            rev: hid,
            key: "sec/h".into(),
            summary: Summary::Certified,
        }],
        reached: Epoch(3),
        more: false,
    };
    let mut red = Redaction::default();
    assert_eq!(owner.redact(r.clone(), &mut red), r);
    assert_eq!(red, Redaction::default());
}

// ------------------------------------------------------- PROP15 read half

struct Gen(ChaCha8Rng);
impl Gen {
    fn pick(&mut self, n: usize) -> usize {
        self.0.next_u32() as usize % n.max(1)
    }
    fn coin(&mut self, percent: u32) -> bool {
        self.0.next_u32() % 100 < percent
    }
    const NAMES: [&'static str; 5] = ["alice", "bob", "carol", "dave", "erin"];
    const LANES: [&'static str; 7] = [
        "repo",
        "billing",
        "notes",
        "code:r1",
        "personal:bob",
        "personal:carol",
        "x",
    ];
    const DIRS: [&'static str; 4] = ["src", "src/billing", "src/billing/inv", "docs"];
    const KEYS: [&'static str; 3] = ["n", "s.", "db/"];
    /// A valid document: 2–5 principals, groups, shared and personal lanes,
    /// grants, denies, `*writers` curators, `code_lanes`.
    fn doc(&mut self) -> AccessDoc {
        let n = 2 + self.pick(4);
        let names: Vec<&str> = Self::NAMES[..n].to_vec();
        let principals = (names.iter().enumerate())
            .map(|(i, m)| ((*m).to_owned(), 501 + i as u32))
            .collect();
        let mut groups = BTreeMap::new();
        let mut tokens: Vec<String> = names.iter().map(|m| (*m).to_owned()).collect();
        tokens.push("*members".into());
        if self.coin(60) {
            let m: BTreeSet<String> = (0..1 + self.pick(3))
                .map(|_| names[self.pick(n)].to_owned())
                .collect();
            groups.insert("g".to_owned(), m);
            tokens.push("@g".into());
        }
        let entry = |g: &mut Self, code: bool| {
            let members: BTreeSet<String> = (0..1 + g.pick(3))
                .map(|_| tokens[g.pick(tokens.len())].clone())
                .collect();
            let mut e = LaneEntry {
                members: members.clone(),
                writers: if code {
                    set(&["*owner"])
                } else {
                    members.clone()
                },
                curators: set(&["*writers"]),
                egress: g.coin(50),
                grants: BTreeMap::new(),
                deny: BTreeSet::new(),
            };
            // Grants and denies only for principals the entry admits.
            let admitted: Vec<&str> = names
                .iter()
                .copied()
                .filter(|m| {
                    members.contains("*members")
                        || members.contains(*m)
                        || (members.contains("@g")
                            && groups
                                .get("g")
                                .is_some_and(|x: &BTreeSet<String>| x.contains(*m)))
                })
                .collect();
            if !admitted.is_empty() {
                for _ in 0..g.pick(3) {
                    let who = admitted[g.pick(admitted.len())];
                    e.grants
                        .entry(who.to_owned())
                        .or_default()
                        .insert(Self::DIRS[g.pick(4)].to_owned());
                }
                for _ in 0..g.pick(3) {
                    let who = admitted[g.pick(admitted.len())];
                    e.deny
                        .insert((who.to_owned(), Self::KEYS[g.pick(3)].to_owned()));
                }
            }
            e
        };
        let mut lanes = BTreeMap::new();
        for l in ["repo", "billing", "notes", "code:r1", "x"] {
            if self.coin(70) {
                let e = entry(self, l.starts_with("code:"));
                lanes.insert(l.to_owned(), e);
            }
        }
        let code_lanes = if self.coin(50) {
            LaneEntry::code_default()
        } else {
            entry(self, true)
        };
        let default_lane = if self.coin(50) {
            LaneEntry::owner_only()
        } else {
            entry(self, false)
        };
        AccessDoc {
            version: 1,
            owner: names[0].to_owned(),
            principals,
            groups,
            lanes,
            code_lanes,
            personal_egress: self.coin(50),
            default_lane,
        }
    }
}
fn admin(ops: Vec<Op>) -> Batch {
    let mut b = Batch::new(ops);
    b.channel = Channel::Admin;
    b
}

/// What the reference says principal `p` may see on `v`.
struct Reference<'a, 'v> {
    v: &'a BranchView<'v>,
    sees: BTreeMap<LogicalId, bool>,
}
impl Reference<'_, '_> {
    fn rev(&self, r: RevisionId) -> bool {
        self.v
            .revision_logical(r)
            .is_some_and(|l| self.sees.get(&l) == Some(&true))
    }
    fn rule(&self, j: RuleId) -> bool {
        self.v.rule_parts(j).is_some_and(|(c, ps, refs)| {
            self.rev(c)
                && ps.iter().all(|i| self.rev(i.premise))
                && refs.iter().all(|r| self.rev(*r))
        })
    }
    fn some_premise(&self, j: RuleId) -> bool {
        let (_, ps, _) = self.v.rule_parts(j).unwrap();
        ps.is_empty() || ps.iter().any(|i| self.rev(i.premise))
    }
    /// The node counts WHY should report for an unredacted proof whose
    /// root is visible (ADR 0017 §5.4), in any order.
    fn why_nodes(&self, p: &Proof, out: &mut Vec<NodeRedaction>) {
        let e = &p.explanation;
        let dropped: Vec<RuleId> = (e.active_rules.iter())
            .copied()
            .filter(|j| !self.some_premise(*j))
            .collect();
        let refs = (e.references.iter())
            .filter(|(j, r)| !dropped.contains(j) && !self.rev(*r))
            .count();
        let premises = p
            .premises
            .iter()
            .filter(|q| !self.rev(q.explanation.revision))
            .count();
        if refs + premises > 0 || !dropped.is_empty() {
            out.push(NodeRedaction {
                rev: e.revision,
                rule: None,
                premises: (refs + premises) as u32,
                rules: dropped.len() as u32,
            });
        }
        for q in p
            .premises
            .iter()
            .filter(|q| self.rev(q.explanation.revision))
        {
            self.why_nodes(q, out);
        }
    }
    /// Every id a redacted proof names is visible, and a witness rule only
    /// when the whole rule is.
    fn proof_clean(&self, p: &Proof) {
        let e = &p.explanation;
        assert!(self.rev(e.revision));
        for j in &e.active_rules {
            assert!(self.some_premise(*j), "rule {j:?} has no visible premise");
        }
        for (_, r) in &e.references {
            assert!(self.rev(*r));
        }
        if let Some(Witness::Rule(j)) = e.witness {
            assert!(self.rule(j));
        }
        for q in &p.premises {
            self.proof_clean(q);
        }
    }
    fn why_not_clean(&self, w: &WhyNot) {
        assert!(self.rev(w.revision));
        for f in &w.rules {
            assert!(self.some_premise(f.rule));
            for (l, _) in f.asserted.iter().chain(&f.current) {
                assert_eq!(self.sees.get(l), Some(&true));
            }
            for i in &f.incidences {
                match i {
                    IncidenceFailure::Unsupported(r)
                    | IncidenceFailure::SemanticHeadUnsupported(r) => {
                        assert!(self.rev(*r))
                    }
                    IncidenceFailure::NotCurrent { premise, head }
                    | IncidenceFailure::SupersededPinned { premise, head } => {
                        assert!(self.rev(*premise) && self.rev(*head))
                    }
                }
            }
            for u in &f.upstream {
                self.why_not_clean(u);
            }
        }
    }
    fn node_clean(&self, n: &NodeState) {
        for w in n.witness.iter().flatten() {
            if let Witness::Rule(j) = w {
                assert!(self.rule(*j));
            }
        }
        if let StaleReason::SupersededPinnedPremise(r) = n.dimensions.stale_reason {
            assert!(self.rev(r));
        }
    }
}

/// PROP15 (read half, ADR 0017 §11 G3): random `generate_p7` histories
/// whose logicals spread over shared, personal, code and unlisted lanes,
/// with several creators, `SetMetadata` dirs, and a second access document
/// (revocations) in the middle. For every principal, the redaction point
/// agrees with `contextdb-oracle::AccessModel` on what is visible, leaves no
/// hidden id in WHY, WHY_NOT, IMPACT, diffs, commit results or verdicts,
/// and reports the reference's counts; the snapshot and digests do not
/// depend on the reader; principal ids avoid the reserved ranges.
/// `CONTEXTDB_P9_READ_SEEDS` (default 300).
#[test]
fn prop15_redaction_matches_the_reference() {
    let seeds: u64 = std::env::var("CONTEXTDB_P9_READ_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let (mut hidden_seen, mut shown_seen) = (0u64, 0u64);
    // Coverage: redactions that actually removed something.
    let (mut why_cut, mut impact_cut, mut diff_cut, mut result_cut) = (0u64, 0u64, 0u64, 0u64);
    for seed in 0..seeds {
        let mut g = Gen(ChaCha8Rng::seed_from_u64(seed ^ 0x5039_0003));
        let docs = [g.doc(), g.doc()];
        let mut k = Kernel::default();
        let mut model = None::<AccessModel>;
        let mut results = Vec::new();
        let history = contextdb_testkit::generate_p7(seed).batches;
        let middle = history.len() / 2;
        for (n, mut b) in history.into_iter().enumerate() {
            if n == 0 || n == middle {
                let d = docs[usize::from(n != 0)].clone();
                k.commit(&admin(vec![Op::SetAccess {
                    doc: Box::new(d.clone()),
                }]))
                .unwrap_or_else(|e| panic!("seed {seed}: {e:?}"));
                model = Some(AccessModel::apply(model.as_ref(), &d).unwrap());
            }
            for op in &mut b.ops {
                if let Op::CreateLogical { scope_lane, .. } = op {
                    *scope_lane = Gen::LANES[g.pick(Gen::LANES.len())].into();
                }
            }
            b.principal = [
                PrincipalId(0),
                PrincipalId(16),
                PrincipalId(17),
                PrincipalId(18),
            ][g.pick(4)];
            if let Ok(r) = k.commit(&b) {
                results.push(r);
            }
            if g.coin(30) {
                let view = k.branch(MAIN).unwrap();
                let count = view.logical_count();
                if count > 0 {
                    let l = view.logicals().nth(g.pick(count)).unwrap();
                    let dirs = (0..g.pick(3))
                        .map(|_| Gen::DIRS[g.pick(4)].to_owned())
                        .collect();
                    let _ = k.commit(&admin(vec![Op::SetMetadata {
                        logical: LogicalRef::Existing(l),
                        tags: BTreeSet::new(),
                        dirs,
                    }]));
                }
            }
        }
        let model = model.unwrap();
        let v = k.branch(MAIN).unwrap();
        let digest = v.digest();
        for id in model.ids.values() {
            assert!(
                *id == 0 || (*id >= 16 && !(0x5641_0000..=0x5641_FFFF).contains(id)),
                "seed {seed}: reserved id {id}"
            );
        }
        let mut readers: Vec<PrincipalId> = model.ids.values().map(|i| PrincipalId(*i)).collect();
        readers.extend([PrincipalId(1), PrincipalId(99)]);
        let epoch = v.epoch();
        for p in readers {
            let sees: BTreeMap<LogicalId, bool> = v
                .logicals()
                .map(|l| {
                    let (key, _, lane) = v.logical_info(l).unwrap();
                    let owner = v.logical_owner(l).unwrap();
                    let dirs = v.metadata(l).unwrap_or_default().dirs;
                    (l, model.authorized(p, &key, &lane, owner, &dirs).0)
                })
                .collect();
            let reference = Reference { v: &v, sees };
            let viewer = Viewer::new(p, &v);
            for (l, s) in &reference.sees {
                assert_eq!(viewer.sees(*l), *s, "seed {seed} p {p:?} l {l:?}");
                if *s {
                    shown_seen += 1;
                } else {
                    hidden_seen += 1;
                }
            }
            let redact = |r: Response| {
                let mut red = Redaction::default();
                let r = viewer.redact(r, &mut red);
                (r, red)
            };
            let owner = p.is_store_owner();
            let revisions: Vec<RevisionId> = (v.logicals())
                .flat_map(|l| v.head(l))
                .filter(|r| reference.rev(*r))
                .collect();
            for &r in &revisions {
                // WHY.
                let full = v.why_depth(r, 4).unwrap();
                let (resp, red) = redact(Response::Why {
                    proof: full.clone(),
                });
                let Response::Why { proof } = resp else {
                    panic!()
                };
                reference.proof_clean(&proof);
                let mut want = Vec::new();
                if !owner {
                    reference.why_nodes(&full, &mut want);
                }
                why_cut += u64::from(!red.nodes.is_empty());
                let mut got = red.nodes;
                got.sort_by_key(|n| (n.rev, n.rule));
                want.sort_by_key(|n| (n.rev, n.rule));
                assert_eq!(got, want, "seed {seed} p {p:?} r {r:?}");
                // WHY_NOT.
                let (resp, _) = redact(Response::WhyNot {
                    why_not: v.why_not(r, 3).unwrap(),
                });
                let Response::WhyNot { why_not } = resp else {
                    panic!()
                };
                reference.why_not_clean(&why_not);
                // IMPACT.
                let full = v.impact_reachable(&[r]).unwrap();
                let (resp, red) = redact(Response::Impact {
                    impact: full.clone(),
                });
                let Response::Impact { impact } = resp else {
                    panic!()
                };
                let keep = |x: &Vec<RevisionId>| -> Vec<RevisionId> {
                    x.iter().copied().filter(|r| reference.rev(*r)).collect()
                };
                assert_eq!(impact.supported, keep(&full.supported));
                assert_eq!(impact.reference_only, keep(&full.reference_only));
                impact_cut += u64::from(red.redacted + red.redacted_references > 0);
                assert_eq!(
                    red.redacted as usize,
                    full.supported.len() - impact.supported.len()
                );
                assert_eq!(
                    red.redacted_references as usize,
                    full.reference_only.len() - impact.reference_only.len()
                );
            }
            // The diff over the whole history.
            let full = v.diff(Epoch(0), epoch).unwrap();
            let (resp, red) = redact(Response::Diff { diff: full.clone() });
            let Response::Diff { diff } = resp else {
                panic!()
            };
            for c in &diff.changes {
                assert!(reference.rev(c.revision));
                for s in [&c.before, &c.after].into_iter().flatten() {
                    reference.node_clean(s);
                }
            }
            let hidden = full.changes.len() - diff.changes.len();
            diff_cut += u64::from(hidden > 0);
            assert_eq!(red.redacted as usize, hidden);
            assert_eq!(
                hidden,
                (full.changes.iter())
                    .filter(|c| !reference.rev(c.revision))
                    .count()
            );
            // Every commit result of the history.
            for result in &results {
                let (resp, red) = redact(Response::Committed {
                    result: result.clone(),
                });
                let Response::Committed { result: r } = resp else {
                    panic!()
                };
                let d = &result.net_delta;
                let want = result
                    .l0_flips
                    .iter()
                    .filter(|x| !reference.rev(**x))
                    .count()
                    + result
                        .status_diff
                        .iter()
                        .filter(|x| !reference.rev(**x))
                        .count()
                    + d.base_flips.iter().filter(|x| !reference.rev(**x)).count()
                    + (d.head_moves.iter())
                        .filter(|(l, _, _)| reference.sees.get(l) != Some(&true))
                        .count()
                    + d.enable_flips
                        .iter()
                        .filter(|j| !reference.rule(**j))
                        .count()
                    + d.new_rules.iter().filter(|j| !reference.rule(**j)).count();
                assert_eq!(red.redacted as usize, want, "seed {seed}");
                result_cut += u64::from(want > 0);
                assert!(
                    r.l0_flips
                        .iter()
                        .chain(&r.status_diff)
                        .all(|x| reference.rev(*x))
                );
                assert!(r.net_delta.new_rules.iter().all(|j| reference.rule(*j)));
            }
            // Verdicts naming rules and logicals.
            let rules: Vec<RuleId> = (0..)
                .map(RuleId)
                .take_while(|j| v.rule_parts(*j).is_some())
                .collect();
            let (resp, red) = redact(Response::Refused {
                validation: Validation::Pending {
                    rules: rules.clone(),
                },
            });
            let Response::Refused {
                validation: Validation::Pending { rules: kept },
            } = resp
            else {
                panic!()
            };
            assert!(kept.iter().all(|j| reference.rule(*j)));
            assert_eq!(red.redacted as usize, rules.len() - kept.len());
            let all: Vec<LogicalId> = v.logicals().collect();
            let (resp, red) = redact(Response::Refused {
                validation: Validation::Conflicted {
                    entries: all.clone(),
                },
            });
            let Response::Refused {
                validation: Validation::Conflicted { entries },
            } = resp
            else {
                panic!()
            };
            assert!(entries.iter().all(|l| reference.sees[l]));
            assert_eq!(red.redacted as usize, all.len() - entries.len());
        }
        // The derived snapshot and digests do not depend on the reader.
        assert_eq!(k.branch(MAIN).unwrap().digest(), digest, "seed {seed}");
    }
    eprintln!(
        "PROP15 read half: {seeds} seeds; {hidden_seen} hidden and {shown_seen} shown (principal, logical) pairs; \
         redactions: WHY {why_cut}, IMPACT {impact_cut}, diff {diff_cut}, commit results {result_cut}"
    );
    assert!(hidden_seen > 0 && shown_seen > 0);
    assert!(why_cut > 0 && impact_cut > 0 && diff_cut > 0 && result_cut > 0);
}

/// Security review M2 (ADR 0017 §5.7), not reproduced: a certificate's
/// vector holds the session's reads, so the read set the daemon appends
/// for `expect_cert_valid` never contains a hidden premise. A commit after
/// reading a record whose support rests on one neither fails nor names it.
#[test]
fn a_certificate_read_set_never_names_a_hidden_premise() {
    let f = fixture();
    let s = &f.s;
    // pub/c (base) rests on sec/s, which bob cannot read.
    let r = raw(
        s,
        vec![b"body of pub/c".to_vec()],
        vec![Op::CreateLogical {
            key: "pub/c".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: h(b"body of pub/c"),
            adopt_as_base: true,
        }],
        [3; 16],
    );
    let c = r.allocations.new_revisions[0];
    let rule = raw(
        s,
        Vec::new(),
        vec![Op::AddRule {
            conclusion: RevRef::Existing(c),
            incidences: vec![(RevRef::Existing(f.rev["sec/s"]), EdgeKind::Sem)],
            enabled: true,
        }],
        [4; 16],
    )
    .allocations
    .new_rules[0];
    let sec = s
        .actor
        .snapshot()
        .view(MAIN)
        .logical_by_key("sec/s")
        .unwrap();
    raw(
        s,
        Vec::new(),
        vec![Op::RecordValidation {
            rule: rule.into(),
            curvec_seen: vec![(sec, f.rev["sec/s"])],
            verdict: Verdict::Verified,
            validator: "admin".into(),
            meta: None,
            if_absent: false,
        }],
        [5; 16],
    );
    let session = start(s, BOB);
    let env = read(s, BOB, session, key("pub/c"), Mode::Certified);
    assert!(matches!(env.response, Response::Read { .. }), "{env:?}");
    // The check issues the session's certificate.
    let checked = s
        .handle(
            BOB,
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
        .response;
    assert!(matches!(checked, Response::Check { .. }), "{checked:?}");
    let r = s
        .handle(
            BOB,
            Request::Commit {
                binding: Binding::root(session),
                ops: vec![Op::CreateLogical {
                    key: "pub/new".into(),
                    kind: RecordKind::Fact,
                    scope_lane: "repo".into(),
                    content: h(b"a new body"),
                    adopt_as_base: false,
                }],
                blobs: vec![b"a new body".to_vec()],
                expect_cert_valid: true,
                client_txn_id: None,
            },
        )
        .response;
    let hidden = f.rev["sec/s"];
    let text = format!("{r:?}");
    assert!(
        !text.contains("Unauthorized") && !text.contains(&format!("logical {}", hidden.0)),
        "{text}"
    );
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    // The appended read set holds what bob read, never the hidden premise.
    let snap = s.actor.snapshot();
    let row = snap.log.read_row(snap.epoch).unwrap().batch().unwrap();
    let appended: Vec<LogicalId> = row
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::ExpectHeads {
                entries,
                source: ReadSource::Certificate(_),
            } => Some(entries.iter().filter_map(|(l, _)| match l {
                LogicalRef::Existing(l) => Some(*l),
                _ => None,
            })),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(
        !appended.is_empty() && !appended.contains(&sec),
        "{appended:?}"
    );
}

/// Security review M1 (ADR 0017 §5.7, §6.2 rule 7): a merge dry run
/// (`ctx_what_if` with `ExpectHeads` and `Merge`) never reveals a hidden
/// logical's head, and a merge's `certify` list never reveals whether a
/// hidden record is CERTIFIED.
#[test]
fn a_merge_never_reveals_a_hidden_head_or_status() {
    let f = fixture();
    let s = &f.s;
    let r = s
        .handle(
            BOB,
            Request::BranchCreate {
                name: "probe".into(),
                from: None,
                client_txn_id: None,
            },
        )
        .response;
    assert!(!matches!(r, Response::Error { .. }), "{r:?}");
    let session = start(s, BOB);
    let hidden_head = f.rev["sec/s"];
    let probe = s.actor.snapshot().kernel.branch_by_name("probe").unwrap();
    for entry in [
        LogicalRef::Key("sec/s".into()),
        LogicalRef::Existing(
            s.actor
                .snapshot()
                .view(MAIN)
                .logical_by_key("sec/s")
                .unwrap(),
        ),
    ] {
        let r = s
            .handle(
                BOB,
                Request::WhatIf {
                    binding: Binding::root(session),
                    ops: vec![
                        Op::ExpectHeads {
                            entries: vec![(entry.clone(), f.rev["pub/b"])],
                            source: ReadSource::Client,
                        },
                        Op::Merge {
                            source: probe,
                            require_certified: false,
                            certify: Vec::new(),
                        },
                    ],
                    blobs: Vec::new(),
                },
            )
            .response;
        let text = format!("{r:?}");
        assert!(
            !text.contains("ReadSetConflict")
                && !text.contains(&format!("RevisionId({})", hidden_head.0)),
            "{entry:?}: {text}"
        );
    }
    // `certify` naming a hidden record is refused for reading it (key
    // existence is the §2.3 exception), never MergeNotCertified or Merged,
    // whatever its status.
    let merge = |certify: &str| {
        let r = s
            .handle(
                BOB,
                Request::Merge {
                    binding: None,
                    source: "main".into(),
                    into: Some("probe".into()),
                    require_certified: true,
                    certify: vec![certify.into()],
                    client_txn_id: None,
                },
            )
            .response;
        match r {
            Response::Error { code, .. } => code,
            other => format!("{other:?}")
                .split([' ', '{'])
                .next()
                .unwrap()
                .to_owned(),
        }
    };
    assert_eq!(merge("sec/s"), "Unauthorized");
    assert_eq!(merge("no/such-key"), "UnknownRef");
}
