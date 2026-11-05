//! P7 milestone 7 (ADR 0015 §7): handoff in the session log. T52 (accepted
//! once, under concurrent accepts and after a restart), T79 (the read set
//! is copied, skipping logicals already read; the acceptor turns Invalid
//! exactly when a handed-over read changed; `to_agent` and branch
//! refusals) and T83 (a crash between the claim and its flush never lets
//! two accepts succeed).
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, TailFate, session::SessionLog};
use std::path::Path;
use std::sync::Arc;

const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
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
    Service::new(actor::spawn(store).unwrap(), log, events).unwrap()
}
fn call(s: &Svc, req: Request) -> Response {
    s.handle(AGENT, req).response
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
fn raw(s: &Svc, branch: BranchId, blobs: Vec<Vec<u8>>, ops: Vec<Op>) -> CommitResult {
    let mut batch = Batch::new(ops);
    batch.branch = branch;
    match s.handle(ADMIN, Request::Raw { blobs, batch }).response {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
/// Logicals `k0..k{n}` (Facts, base, CERTIFIED) on main.
fn records(s: &Svc, n: u32) {
    let ops = (0..n)
        .map(|i| Op::CreateLogical {
            key: format!("k{i}"),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: h(format!("body {i}").as_bytes()),
            adopt_as_base: true,
        })
        .collect();
    let blobs = (0..n).map(|i| format!("body {i}").into_bytes()).collect();
    raw(s, MAIN, blobs, ops);
}
fn revise(s: &Svc, key: &str, body: &str) {
    let snap = s.actor.snapshot();
    let view = snap.view(MAIN);
    let l = view.logical_by_key(key).unwrap();
    let old = view.head(l).unwrap();
    drop(snap);
    raw(
        s,
        MAIN,
        vec![body.as_bytes().to_vec()],
        vec![
            Op::AddRevision {
                logical: l.into(),
                content: h(body.as_bytes()),
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: l.into(),
                expected: old.into(),
                new: RevRef::New(0),
            },
        ],
    );
}
fn start(s: &Svc, branch: Option<&str>) -> SessionId {
    match call(
        s,
        Request::SessionStart {
            harness: "test".into(),
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
fn bind(session: SessionId, agent: Option<&str>) -> Binding {
    Binding::Session {
        session,
        agent: agent.map(str::to_owned),
    }
}
fn read(s: &Svc, b: Binding, key: &str) {
    match call(
        s,
        Request::Read {
            binding: b,
            target: Target::Key(key.into()),
            mode: Mode::Certified,
            level: None,
        },
    ) {
        Response::Read { .. } => {}
        other => panic!("{other:?}"),
    }
}
fn check(s: &Svc, b: Binding) -> Validation {
    match call(
        s,
        Request::Check {
            binding: b,
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
fn begin(s: &Svc, b: Binding, to_agent: Option<&str>) -> Response {
    call(
        s,
        Request::HandoffBegin {
            binding: b,
            summary: "schema done; migrations left".into(),
            to_agent: to_agent.map(str::to_owned),
        },
    )
}
fn begun(r: Response) -> [u8; 16] {
    match r {
        Response::HandoffBegun { id, .. } => id,
        other => panic!("{other:?}"),
    }
}
fn accept(s: &Svc, b: Binding, id: [u8; 16]) -> Response {
    call(
        s,
        Request::HandoffAccept {
            binding: b,
            id: Some(id),
        },
    )
}
fn invalid(v: &Validation) -> bool {
    matches!(v, Validation::Invalid { .. })
}

/// T79: the acceptor gets the summary and a Valid verdict; the owner's reads
/// are copied except a logical the acceptor already read. Its next check is
/// Valid; after a handed-over read changes it is Invalid; a change to a
/// logical outside the handoff does not matter.
#[test]
fn t79_reads_are_copied_and_the_acceptor_turns_invalid_when_one_changes() {
    let disk = SimDisk::new();
    let s = service(&disk);
    records(&s, 4);
    let owner = start(&s, None);
    for k in ["k0", "k1", "k2"] {
        read(&s, bind(owner, None), k);
    }
    let id = begun(begin(&s, bind(owner, None), None));
    let acceptor = start(&s, None);
    read(&s, bind(acceptor, None), "k2");
    match accept(&s, bind(acceptor, None), id) {
        Response::HandoffAccepted {
            owner: o,
            summary,
            validation,
            copied,
            skipped,
            ..
        } => {
            assert_eq!(o, owner);
            assert_eq!(summary, "schema done; migrations left");
            assert_eq!(validation, Validation::Valid);
            assert_eq!((copied, skipped), (2, 1));
        }
        other => panic!("{other:?}"),
    }
    assert!(!invalid(&check(&s, bind(acceptor, None))));
    // k3 is not part of the handoff.
    revise(&s, "k3", "k3 v2");
    assert!(!invalid(&check(&s, bind(acceptor, None))));
    // k0 was handed over.
    revise(&s, "k0", "k0 v2");
    let v = check(&s, bind(acceptor, None));
    assert!(invalid(&v), "{v:?}");
    // The owner's view agrees.
    assert!(invalid(&check(&s, bind(owner, None))));
}

/// T79: `to_agent` must match the acceptor's agent, the acceptor must be on
/// the handoff's branch, and an unknown id is refused. Begin refuses a
/// certificate that is not Valid, an oversized summary, and nothing to
/// hand over (Empty).
#[test]
fn t79_refusals() {
    let disk = SimDisk::new();
    let s = service(&disk);
    records(&s, 2);
    assert_eq!(
        code(
            &s.handle(
                ADMIN,
                Request::BranchCreate {
                    name: "dev".into(),
                    from: None,
                    client_txn_id: None,
                },
            )
            .response
        ),
        "ok"
    );
    let owner = start(&s, None);
    // Nothing read: an Empty certificate is not handed over.
    assert_eq!(code(&begin(&s, bind(owner, None), None)), "Refused");
    read(&s, bind(owner, None), "k0");
    let big = call(
        &s,
        Request::HandoffBegin {
            binding: bind(owner, None),
            summary: "x".repeat(8 * 1024 + 1),
            to_agent: None,
        },
    );
    assert_eq!(code(&big), "InvalidArgument");
    let id = begun(begin(&s, bind(owner, None), Some("reviewer")));
    let other = start(&s, None);
    assert_eq!(
        code(&accept(&s, bind(other, Some("writer")), id)),
        "HandoffNotAddressed"
    );
    assert_eq!(
        code(&accept(&s, bind(other, None), id)),
        "HandoffNotAddressed"
    );
    let on_dev = start(&s, Some("dev"));
    assert_eq!(
        code(&accept(&s, bind(on_dev, Some("reviewer")), id)),
        "BranchMismatch"
    );
    assert_eq!(
        code(&accept(&s, bind(other, Some("reviewer")), [0xAB; 16])),
        "UnknownRef"
    );
    // Listing: the reviewer on main sees it; others do not.
    let list = |b| match call(
        &s,
        Request::HandoffAccept {
            binding: b,
            id: None,
        },
    ) {
        Response::Handoffs { items } => items,
        other => panic!("{other:?}"),
    };
    assert_eq!(list(bind(other, Some("reviewer"))).len(), 1);
    assert!(list(bind(other, Some("writer"))).is_empty());
    assert!(list(bind(on_dev, Some("reviewer"))).is_empty());
    // The addressed agent accepts; the owner's later change makes its
    // certificate Invalid, and a new begin is refused with that verdict.
    assert!(matches!(
        accept(&s, bind(other, Some("reviewer")), id),
        Response::HandoffAccepted { .. }
    ));
    assert!(list(bind(other, Some("reviewer"))).is_empty());
    revise(&s, "k0", "k0 v2");
    match begin(&s, bind(owner, None), None) {
        Response::Refused { validation } => assert!(invalid(&validation), "{validation:?}"),
        other => panic!("{other:?}"),
    }
}

/// T52: of 8 concurrent accepts exactly one succeeds and the others get
/// `HandoffTaken`; the winner's retry gets its own claim back; after a
/// restart the durable accept still decides.
#[test]
fn t52_accepted_once_under_concurrency_and_after_restart() {
    let disk = SimDisk::new();
    let s = Arc::new(service(&disk));
    records(&s, 3);
    let owner = start(&s, None);
    read(&s, bind(owner, None), "k0");
    read(&s, bind(owner, None), "k1");
    let id = begun(begin(&s, bind(owner, None), None));
    let sessions: Vec<SessionId> = (0..8).map(|_| start(&s, None)).collect();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let results: Vec<(SessionId, String)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (sessions.iter())
            .map(|&sid| {
                let (s, barrier) = (s.clone(), barrier.clone());
                scope.spawn(move || {
                    barrier.wait();
                    (sid, code(&accept(&s, bind(sid, None), id)).to_owned())
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let winners: Vec<_> = results.iter().filter(|(_, c)| c == "ok").collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    assert!(
        (results.iter())
            .filter(|(_, c)| c != "ok")
            .all(|(_, c)| c == "HandoffTaken"),
        "{results:?}"
    );
    let winner = winners[0].0;
    assert_eq!(code(&accept(&s, bind(winner, None), id)), "ok");
    // Restart: the same answers from the durable log.
    let s = Arc::try_unwrap(s).ok().expect("one owner");
    s.stop_writer();
    drop(s);
    let s = service(&disk.crash(TailFate::Torn, 1));
    for sid in &sessions {
        let r = accept(&s, bind(*sid, None), id);
        if *sid == winner {
            assert!(matches!(r, Response::HandoffAccepted { .. }), "{r:?}");
        } else {
            assert_eq!(code(&r), "HandoffTaken", "{r:?}");
        }
    }
    // The winner's copied reads survived the restart.
    revise(&s, "k1", "k1 v2");
    assert!(invalid(&check(&s, bind(winner, None))));
}

/// T83: the claim's flush fails, so the event may or may not reach media.
/// That accept fails and every later handoff operation is refused until
/// restart. After a crash the durable log decides: exactly one of the two
/// acceptors holds the handoff, and a retry by the first succeeds only when
/// its own claim was durable. Over the seeds both outcomes occur.
#[test]
fn t83_a_crash_between_claim_and_flush_never_lets_two_accepts_succeed() {
    let mut outcomes = std::collections::BTreeSet::new();
    for seed in 0..32u64 {
        let disk = SimDisk::new();
        let s = service(&disk);
        records(&s, 2);
        let owner = start(&s, None);
        let (a, b) = (start(&s, None), start(&s, None));
        read(&s, bind(owner, None), "k0");
        // Begin's flush drains everything queued: the next sync is the
        // claim's own.
        let id = begun(begin(&s, bind(owner, None), None));
        disk.fail_next("sync");
        let first = accept(&s, bind(a, None), id);
        assert!(
            matches!(first, Response::Error { .. }),
            "seed {seed}: {first:?}"
        );
        // Refused until restart, for anyone.
        assert_eq!(code(&accept(&s, bind(b, None), id)), "Unavailable");
        assert_eq!(code(&accept(&s, bind(a, None), id)), "Unavailable");
        assert_eq!(code(&begin(&s, bind(owner, None), None)), "Unavailable");
        drop(s);
        // Unsynced pages independently persist or read as zeros, or a prefix
        // survives: the claim's group is durable on some seeds only.
        let fate = if seed < 24 {
            TailFate::OutOfOrder
        } else {
            TailFate::Torn
        };
        let s = service(&disk.crash(fate, seed));
        let rb = accept(&s, bind(b, None), id);
        let ra = accept(&s, bind(a, None), id);
        let (a_ok, b_ok) = (
            matches!(ra, Response::HandoffAccepted { .. }),
            matches!(rb, Response::HandoffAccepted { .. }),
        );
        assert!(a_ok != b_ok, "seed {seed}: {ra:?} {rb:?}");
        let loser = if a_ok { &rb } else { &ra };
        assert_eq!(code(loser), "HandoffTaken", "seed {seed}");
        outcomes.insert(a_ok);
    }
    assert_eq!(outcomes.len(), 2, "both crash outcomes exercised");
}
