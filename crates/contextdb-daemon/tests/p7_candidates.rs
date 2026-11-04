//! P7 milestone 6 (ADR 0015 §5): lexical contradiction candidates over a
//! per-branch recall index. The candidate half of T84: candidates never pair
//! a revision invisible on the branch.
use contextdb_daemon::{
    actor,
    candidates::{CandState, Candidate},
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_proto::docs::{ConflictDoc, ConflictSource};
use contextdb_store::{OpenOptions, Payload, SimDisk, Store, session::SessionLog};
use std::path::Path;
use std::time::{Duration, Instant};

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
fn raw_on(s: &Svc, branch: BranchId, blobs: Vec<Vec<u8>>, ops: Vec<Op>) -> CommitResult {
    let mut batch = Batch::new(ops);
    batch.branch = branch;
    match call(s, ADMIN, Request::Raw { blobs, batch }) {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
/// Creates `key` with `body` on `branch`; returns its revision.
fn create(s: &Svc, branch: BranchId, key: &str, kind: RecordKind, body: &str) -> RevisionId {
    let r = raw_on(
        s,
        branch,
        vec![body.as_bytes().to_vec()],
        vec![Op::CreateLogical {
            key: key.into(),
            kind,
            scope_lane: "repo".into(),
            content: h(body.as_bytes()),
            adopt_as_base: true,
        }],
    );
    r.allocations.new_revisions[0]
}
/// A new head for `key` on `branch`.
fn revise(s: &Svc, branch: BranchId, key: &str, body: &str) -> RevisionId {
    let snap = s.actor.snapshot();
    let view = snap.view(branch);
    let l = view.logical_by_key(key).unwrap();
    let old = view.head(l).unwrap();
    drop(snap);
    let r = raw_on(
        s,
        branch,
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
    r.allocations.new_revisions[0]
}
/// Waits until the indexer has generated candidates for the current epoch.
fn settle(s: &Svc) {
    let start = Instant::now();
    loop {
        let epoch = s.actor.snapshot().epoch;
        if s.candidates.watermark().unwrap() == Some(epoch) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "candidates never caught up"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
/// Unrelated records, so shared words are rare enough to score.
fn filler(s: &Svc) {
    let words = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    ];
    for (i, w) in words.iter().enumerate() {
        create(
            s,
            MAIN,
            &format!("filler-{i}"),
            RecordKind::Fact,
            &format!("{w} {w} is a filler note about topic {w}"),
        );
    }
}
fn pairs(s: &Svc, branch: BranchId) -> Vec<(RevisionId, RevisionId)> {
    let mut v: Vec<_> = (s.candidates.listed(Some(branch)).unwrap().into_iter())
        .map(|c| (c.a.min(c.b), c.a.max(c.b)))
        .collect();
    v.sort();
    v
}
fn listed(s: &Svc, branch: Option<&str>) -> Vec<ConflictItem> {
    match call(
        s,
        ADMIN,
        Request::Conflicts {
            binding: None,
            branch: branch.map(str::to_owned),
            scope: None,
            state: ConflictState::Candidates,
            limit: 0,
        },
    ) {
        Response::Conflicts { items, .. } => items,
        other => panic!("{other:?}"),
    }
}
fn pair(a: RevisionId, b: RevisionId) -> (RevisionId, RevisionId) {
    (a.min(b), a.max(b))
}

/// A Decision and a Fact sharing rare terms are paired; a rule's premise
/// and conclusion are not (direct neighbours), nor is an Evidence record.
#[test]
fn similar_heads_pair_neighbours_and_other_kinds_do_not() {
    let disk = SimDisk::new();
    let s = service(&disk);
    filler(&s);
    let d = create(
        &s,
        MAIN,
        "orders-db",
        RecordKind::Decision,
        "orders are stored in postgres with nightly backups",
    );
    let f = create(
        &s,
        MAIN,
        "orders-store",
        RecordKind::Fact,
        "orders are stored in mongodb, never in postgres",
    );
    // Premise and conclusion of a rule, with text close to each other.
    let p = create(
        &s,
        MAIN,
        "deploy-k8s",
        RecordKind::Fact,
        "the kubernetes cluster runs the ingress gateway",
    );
    let c = create(
        &s,
        MAIN,
        "deploy-plan",
        RecordKind::Plan,
        "move the ingress gateway to the kubernetes cluster",
    );
    raw_on(
        &s,
        MAIN,
        Vec::new(),
        vec![Op::AddRule {
            conclusion: c.into(),
            incidences: vec![(p.into(), EdgeKind::Sem)],
            enabled: true,
        }],
    );
    let e = create(
        &s,
        MAIN,
        "orders-log",
        RecordKind::Evidence,
        "log: orders stored in postgres nightly backups ok",
    );
    settle(&s);
    let got = pairs(&s, MAIN);
    assert!(got.contains(&pair(d, f)), "{got:?}");
    assert!(!got.contains(&pair(p, c)), "{got:?}");
    assert!(got.iter().all(|(a, b)| *a != e && *b != e), "{got:?}");
    // Every candidate is listed with both sides current, and a score.
    let items = listed(&s, None);
    assert_eq!(items.len(), got.len());
    for i in &items {
        assert_eq!(i.kind, "candidate");
        assert!(i.sides.iter().all(|x| x.current));
        assert!(i.candidate.is_some());
        assert!(i.reason.contains("bm25"));
    }
    // A candidate never gates.
    let snap = s.actor.snapshot();
    assert_eq!(snap.view(MAIN).status(d).unwrap(), Summary::Certified);
}

/// T84 (candidates): pairs are per branch and never name a revision the
/// branch cannot see; a side that stops being the head turns the pair
/// `gone`, on that branch only.
#[test]
fn t84_candidates_never_pair_a_revision_invisible_on_the_branch() {
    let disk = SimDisk::new();
    let s = service(&disk);
    filler(&s);
    let a = create(
        &s,
        MAIN,
        "sessions",
        RecordKind::Decision,
        "user sessions are cached in the redis cluster",
    );
    settle(&s);
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
    // c arrives on main after the fork, before b on dev: b's query reads
    // main's rows, c's among them, and must drop it.
    let c = create(
        &s,
        MAIN,
        "sessions-main",
        RecordKind::Fact,
        "user sessions cached in redis cluster expire hourly",
    );
    settle(&s);
    let b = create(
        &s,
        dev,
        "sessions-dev",
        RecordKind::Fact,
        "user sessions are never cached in the redis cluster",
    );
    settle(&s);
    let on_dev = pairs(&s, dev);
    let on_main = pairs(&s, MAIN);
    assert!(on_dev.contains(&pair(a, b)), "{on_dev:?}");
    assert!(on_main.contains(&pair(a, c)), "{on_main:?}");
    // c is invisible on dev and b on main.
    assert!(on_dev.iter().all(|(x, y)| *x != c && *y != c), "{on_dev:?}");
    assert!(
        on_main.iter().all(|(x, y)| *x != b && *y != b),
        "{on_main:?}"
    );
    let snap = s.actor.snapshot();
    for (branch, list) in [(dev, &on_dev), (MAIN, &on_main)] {
        let view = snap.view(branch);
        for (x, y) in list {
            assert!(view.is_visible_revision(*x) && view.is_visible_revision(*y));
        }
    }
    drop(snap);
    // A new head for a on dev: dev's pairs with the old a are gone, main's
    // stay; the new head pairs on dev with b again.
    let a2 = revise(
        &s,
        dev,
        "sessions",
        "user sessions are cached in the redis cluster for one day",
    );
    settle(&s);
    let all: Vec<Candidate> = s.candidates.all().unwrap();
    let old_dev = all
        .iter()
        .find(|x| x.branch == dev && pair(x.a, x.b) == pair(a, b))
        .unwrap();
    assert_eq!(old_dev.state, CandState::Gone);
    assert!(pairs(&s, MAIN).contains(&pair(a, c)));
    assert!(
        pairs(&s, dev).contains(&pair(a2, b)),
        "{:?}",
        pairs(&s, dev)
    );
    // A later dev head reads main's row of a, which dev shadows with a2:
    // it pairs with a2, never with a.
    let d = create(
        &s,
        dev,
        "sessions-ttl",
        RecordKind::Plan,
        "user sessions cached in the redis cluster are evicted",
    );
    settle(&s);
    let with_d: Vec<_> = (pairs(&s, dev).into_iter())
        .filter(|(x, y)| *x == d || *y == d)
        .collect();
    assert!(with_d.contains(&pair(a2, d)), "{with_d:?}");
    assert!(!with_d.contains(&pair(a, d)), "{with_d:?}");
    // Listing on dev shows only pairs of dev's current heads.
    for i in listed(&s, Some("dev")) {
        assert!(i.sides.iter().all(|x| x.current), "{i:?}");
        assert!(i.sides.iter().all(|x| x.rev != a && x.rev != c), "{i:?}");
    }
}

/// `open{from: candidate}`: the pair (b filled in from the candidate) with
/// a `Candidate` source; the candidate is then no longer listed, and a
/// second open, a wrong side or another branch's id is refused.
#[test]
fn open_from_a_candidate() {
    let disk = SimDisk::new();
    let s = service(&disk);
    filler(&s);
    let a = create(
        &s,
        MAIN,
        "queue",
        RecordKind::Decision,
        "background jobs run on the rabbitmq broker",
    );
    let b = create(
        &s,
        MAIN,
        "queue-fact",
        RecordKind::Fact,
        "background jobs never touch the rabbitmq broker",
    );
    settle(&s);
    let item = listed(&s, None)
        .into_iter()
        .find(|i| i.sides.iter().any(|x| x.rev == a) && i.sides.iter().any(|x| x.rev == b))
        .expect("candidate a-b");
    let id = item.candidate.unwrap();
    let open = |a: Target, b: Option<Target>, id: u64| Request::ConflictOpen {
        binding: None,
        branch: None,
        a,
        b,
        reason: "they disagree on the broker".into(),
        evidence: Vec::new(),
        from: Some(ConflictFrom::Candidate(id)),
        expect_cert_valid: false,
        client_txn_id: None,
    };
    let rev = |key: &str, r: RevisionId| Target::KeyRev {
        key: key.into(),
        rev: r,
    };
    // A revision that is not a side.
    let wrong = call(&s, ADMIN, open(rev("filler-0", RevisionId(0)), None, id));
    assert_eq!(code(&wrong), "InvalidArgument", "{wrong:?}");
    let wrong_b = call(
        &s,
        ADMIN,
        open(rev("queue", a), Some(rev("filler-0", RevisionId(0))), id),
    );
    assert_eq!(code(&wrong_b), "InvalidArgument", "{wrong_b:?}");
    let unknown = call(&s, ADMIN, open(rev("queue", a), None, 9_999));
    assert_eq!(code(&unknown), "UnknownRef", "{unknown:?}");
    // From b's side, a filled in.
    let r = match call(&s, ADMIN, open(rev("queue-fact", b), None, id)) {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    };
    assert_eq!(r.allocations.new_artifacts.len(), 2);
    let snap = s.actor.snapshot();
    let view = snap.view(MAIN);
    assert_eq!(view.status(a).unwrap(), Summary::Conflicted);
    assert_eq!(view.status(b).unwrap(), Summary::Conflicted);
    let x = view.artifact(r.allocations.new_artifacts[0]).unwrap();
    let Payload::Available(bytes) = snap.log.read_blob(&x.payload_hash) else {
        panic!("doc")
    };
    let doc = ConflictDoc::from_bytes(&bytes).unwrap();
    assert_eq!(doc.source, ConflictSource::Candidate(id));
    assert_eq!(doc.sides, vec![b, a]);
    drop(snap);
    assert_eq!(
        s.candidates.get(id).unwrap().unwrap().state,
        CandState::Opened
    );
    assert!(listed(&s, None).iter().all(|i| i.candidate != Some(id)));
    let again = call(&s, ADMIN, open(rev("queue", a), None, id));
    assert_eq!(code(&again), "InvalidArgument", "{again:?}");
    // Another branch does not know main's candidate ids.
    call(
        &s,
        ADMIN,
        Request::BranchCreate {
            name: "dev".into(),
            from: None,
            client_txn_id: None,
        },
    );
    let mut on_dev = open(rev("queue", a), None, id);
    if let Request::ConflictOpen { branch, .. } = &mut on_dev {
        *branch = Some("dev".into());
    }
    assert_eq!(code(&call(&s, ADMIN, on_dev)), "UnknownRef");
}

/// The recall regression with per-branch rows: a session on `dev` finds
/// text only dev's head has, and main does not.
#[test]
fn recall_reads_the_branch_rows() {
    let disk = SimDisk::new();
    let s = service(&disk);
    create(
        &s,
        MAIN,
        "db",
        RecordKind::Decision,
        "the service uses postgres",
    );
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
    let new = revise(&s, dev, "db", "the service moves to cockroachdb");
    settle(&s);
    let start = |branch: Option<&str>| match call(
        &s,
        AGENT,
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
    };
    let recall = |session, q: &str| match call(
        &s,
        AGENT,
        Request::Recall {
            binding: Binding::root(session),
            query: q.into(),
            scope: String::new(),
            mode: Mode::Advisory,
            limit: 10,
            index_wait_ms: 0,
            opts: Default::default(),
        },
    ) {
        Response::Recall { items, .. } => items,
        other => panic!("{other:?}"),
    };
    let on_dev = start(Some("dev"));
    let on_main = start(None);
    let hits = recall(on_dev, "cockroachdb");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].rev, new);
    assert!(recall(on_main, "cockroachdb").is_empty());
    // Main's old text is shadowed on dev.
    assert!(recall(on_dev, "postgres").is_empty());
    assert_eq!(recall(on_main, "postgres").len(), 1);
}
