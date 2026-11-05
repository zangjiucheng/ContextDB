//! P8 milestone 5b (ADR 0016 §7): the vector projection, the vector ranker
//! and embedding contradiction candidates. T89, and the vector half of
//! T54/T94.
use contextdb_daemon::{
    actor,
    candidates::{CandState, EMBEDDING, LEXICAL},
    embedding::Settings,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use contextdb_validate::ValidatorError;
use contextdb_validate::config::CandidateLimits;
use contextdb_validate::embed::{Embedder, Mock, VectorLimits, normalise};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
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
    Service::new(actor::spawn(store).unwrap(), log, events).unwrap()
}
fn settings(e: Arc<dyn Embedder>, timeout: Duration) -> Settings {
    Settings {
        embedder: e,
        max_batch: 4,
        timeout,
        limits: VectorLimits::default(),
    }
}
fn call(s: &Svc, id: Identity, req: Request) -> Response {
    s.handle(id, req).response
}
fn h(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}
fn create_on(
    s: &Svc,
    branch: BranchId,
    key: &str,
    kind: RecordKind,
    body: &str,
    base: bool,
) -> RevisionId {
    let mut batch = Batch::new(vec![Op::CreateLogical {
        key: key.into(),
        kind,
        scope_lane: "repo".into(),
        content: h(body.as_bytes()),
        adopt_as_base: base,
    }]);
    batch.branch = branch;
    match call(
        s,
        ADMIN,
        Request::Raw {
            blobs: vec![body.as_bytes().to_vec()],
            batch,
        },
    ) {
        Response::Committed { result } => result.allocations.new_revisions[0],
        other => panic!("{other:?}"),
    }
}
fn create(s: &Svc, key: &str, kind: RecordKind, body: &str) -> RevisionId {
    create_on(s, MAIN, key, kind, body, true)
}
fn attach(s: &Svc) -> Binding {
    match call(
        s,
        AGENT,
        Request::SessionAttach {
            harness: "claude".into(),
            harness_session: "A".into(),
            repo: "/repo".into(),
            head_sha: "h".into(),
            branch: None,
            git_branch: None,
        },
    ) {
        Response::Attached { session, .. } => Binding::root(session),
        other => panic!("{other:?}"),
    }
}
/// Waits until vectors are Ready at the current epoch and the embedding
/// candidates have caught up.
fn settle(s: &Svc) {
    let start = Instant::now();
    loop {
        let epoch = s.actor.snapshot().epoch;
        let v = s.vectors.get().unwrap();
        if matches!(v.state(epoch), IndexState::Ready { .. })
            && s.candidates.embed_watermark().unwrap() == Some(epoch)
            && s.candidates.watermark().unwrap() == Some(epoch)
        {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "vectors never caught up"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
struct Got {
    items: Vec<Item>,
    withheld: Vec<Item>,
    index: IndexReport,
}
fn recall(s: &Svc, b: &Binding, query: &str, mode: Mode) -> Got {
    match call(
        s,
        AGENT,
        Request::Recall {
            binding: b.clone(),
            query: query.into(),
            scope: String::new(),
            mode,
            limit: 10,
            index_wait_ms: 2_000,
            opts: RecallOpts::default(),
        },
    ) {
        Response::Recall {
            items,
            withheld,
            index,
            ..
        } => Got {
            items,
            withheld,
            index,
        },
        other => panic!("{other:?}"),
    }
}

/// Words in one group share a dimension: a stand-in for a model that knows
/// synonyms, which neither keys nor bm25 can match.
struct Synonyms;
impl Embedder for Synonyms {
    fn model(&self) -> &str {
        "synonyms"
    }
    fn dims(&self) -> usize {
        8
    }
    fn embed(&self, texts: &[String], _: Instant) -> Result<Vec<Vec<f32>>, ValidatorError> {
        let group = |w: &str| match w {
            "car" | "automobile" | "vehicle" => Some(0),
            "ledger" | "bookkeeping" => Some(1),
            "rocket" | "spacecraft" => Some(2),
            _ => None,
        };
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; 8];
                for w in t.split(|c: char| !c.is_alphanumeric()) {
                    if let Some(i) = group(&w.to_lowercase()) {
                        v[i] += 1.0;
                    }
                }
                normalise(&mut v);
                v
            })
            .collect())
    }
}

/// The vector ranker finds a record by meaning alone; in certified mode
/// an uncertified match is withheld, never an item (T54); vectors report
/// Ready (T94).
#[test]
fn the_vector_ranker_finds_what_keys_and_text_cannot() {
    let disk = SimDisk::new();
    let s = service(&disk);
    s.start_vectors(settings(Arc::new(Synonyms), Duration::from_secs(5)), None)
        .unwrap();
    let car = create(
        &s,
        "fleet/policy",
        RecordKind::Decision,
        "Every car is leased.",
    );
    create(&s, "books", RecordKind::Fact, "The ledger closes monthly.");
    create_on(
        &s,
        MAIN,
        "fleet/draft",
        RecordKind::Plan,
        "Buy a vehicle next year.",
        false,
    );
    settle(&s);
    let b = attach(&s);
    let got = recall(&s, &b, "automobile", Mode::Advisory);
    assert!(
        matches!(got.index.vectors, IndexState::Ready { .. }),
        "{:?}",
        got.index
    );
    let keys: Vec<&str> = got.items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(keys.contains(&"fleet/policy") && keys.contains(&"fleet/draft"));
    assert!(!keys.contains(&"books"), "unrelated: cosine 0");
    let got = recall(&s, &b, "automobile", Mode::Certified);
    assert_eq!(got.items.len(), 1);
    assert_eq!(
        (got.items[0].key.as_str(), got.items[0].rev),
        ("fleet/policy", car)
    );
    assert_eq!(got.items[0].summary, Summary::Certified);
    assert!(got.withheld.iter().any(|w| w.key == "fleet/draft"));
    // Without an embedder the same query finds nothing.
    let plain = service(&SimDisk::new());
    create(
        &plain,
        "fleet/policy",
        RecordKind::Decision,
        "Every car is leased.",
    );
    let b2 = attach(&plain);
    let got = recall(&plain, &b2, "automobile", Mode::Advisory);
    assert!(got.items.is_empty());
    assert_eq!(got.index.vectors, IndexState::Disabled);
}

/// T89: embedding candidates are per branch and never pair a revision
/// invisible on the branch; a pair both sources find is one row, `both`.
#[test]
fn t89_embedding_candidates_are_per_branch() {
    let disk = SimDisk::new();
    let s = service(&disk);
    // Lexical candidates off: every pair below comes from embeddings.
    s.candidates.set_limits(CandidateLimits {
        min_score: 1.0,
        per_head: 0,
    });
    s.start_vectors(
        settings(Arc::new(Mock::new(256)), Duration::from_secs(5)),
        None,
    )
    .unwrap();
    let words = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    ];
    for (i, w) in words.iter().enumerate() {
        create(
            &s,
            &format!("filler-{i}"),
            RecordKind::Fact,
            &format!("{w} {w} filler note"),
        );
    }
    let a = create(
        &s,
        "sessions",
        RecordKind::Decision,
        "user sessions are cached in the redis cluster",
    );
    // Not a candidate kind.
    let ev = create(
        &s,
        "sessions-evidence",
        RecordKind::Evidence,
        "user sessions are cached in the redis cluster",
    );
    settle(&s);
    let made = call(
        &s,
        ADMIN,
        Request::BranchCreate {
            name: "dev".into(),
            from: None,
            client_txn_id: None,
        },
    );
    assert!(!matches!(made, Response::Error { .. }), "{made:?}");
    let dev = s.actor.snapshot().kernel.branch_by_name("dev").unwrap();
    let c = create(
        &s,
        "sessions-main",
        RecordKind::Fact,
        "user sessions are cached in the redis cluster hourly",
    );
    settle(&s);
    let b = create_on(
        &s,
        dev,
        "sessions-dev",
        RecordKind::Fact,
        "user sessions are never cached in the redis cluster",
        true,
    );
    settle(&s);
    let all = s.candidates.all().unwrap();
    assert!(all.iter().all(|x| x.source == EMBEDDING), "{all:?}");
    let has = |branch: BranchId, x: RevisionId, y: RevisionId| {
        all.iter().any(|p| {
            p.branch == branch
                && p.state == CandState::New
                && (p.a.min(p.b), p.a.max(p.b)) == (x.min(y), x.max(y))
        })
    };
    assert!(has(MAIN, a, c), "{all:?}");
    assert!(has(dev, a, b), "{all:?}");
    // c is invisible on dev, b on main; Evidence never pairs.
    let snap = s.actor.snapshot();
    for p in &all {
        let view = snap.view(p.branch);
        assert!(
            view.is_visible_revision(p.a) && view.is_visible_revision(p.b),
            "{p:?}"
        );
        assert!(p.a != ev && p.b != ev);
        assert!(p.cosine.unwrap() >= 0.82);
    }
    assert!(
        !all.iter()
            .any(|p| p.branch == dev && (p.a == c || p.b == c))
    );
    assert!(
        !all.iter()
            .any(|p| p.branch == MAIN && (p.a == b || p.b == b))
    );
    drop(snap);
    // Listed with the embedding reason.
    let items = match call(
        &s,
        ADMIN,
        Request::Conflicts {
            binding: None,
            branch: None,
            scope: None,
            state: ConflictState::Candidates,
            limit: 0,
        },
    ) {
        Response::Conflicts { items, .. } => items,
        other => panic!("{other:?}"),
    };
    assert!(
        items
            .iter()
            .any(|i| i.reason.starts_with("embedding similarity, cosine")),
        "{items:?}"
    );
    // Lexical candidates on: a new head found by both sources is one row.
    s.candidates.set_limits(CandidateLimits::default());
    let d = create(
        &s,
        "sessions-ttl",
        RecordKind::Plan,
        "user sessions are cached in the redis cluster hourly with ttl",
    );
    settle(&s);
    let all = s.candidates.all().unwrap();
    let with_d: Vec<_> = all.iter().filter(|p| p.a == d || p.b == d).collect();
    assert!(!with_d.is_empty());
    assert!(
        with_d.iter().any(|p| p.source == LEXICAL | EMBEDDING),
        "{with_d:?}"
    );
    let mut seen = std::collections::BTreeSet::new();
    for p in &all {
        assert!(
            seen.insert((p.branch, p.a.min(p.b), p.a.max(p.b))),
            "one row per pair"
        );
    }
}

/// An embedder that hangs (until its deadline), fails, or works, as set.
struct Flaky {
    mode: AtomicU8,
    calls: AtomicUsize,
}
const WORK: u8 = 0;
const HANG: u8 = 1;
const FAIL: u8 = 2;
impl Embedder for Flaky {
    fn model(&self) -> &str {
        "flaky"
    }
    fn dims(&self) -> usize {
        64
    }
    fn embed(&self, texts: &[String], deadline: Instant) -> Result<Vec<Vec<f32>>, ValidatorError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        match self.mode.load(Ordering::Acquire) {
            HANG => {
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                Err(ValidatorError::Timeout)
            }
            FAIL => Err(ValidatorError::Malformed("wrong dims".into())),
            _ => Mock::new(64).embed(texts, deadline),
        }
    }
}

/// T89: a hung or failing embedder never blocks the writer, the text
/// indexer or shutdown. Vectors report the failure and recover.
#[test]
fn t89_a_hung_or_failing_embedder_never_blocks() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let e = Arc::new(Flaky {
        mode: AtomicU8::new(HANG),
        calls: AtomicUsize::new(0),
    });
    s.start_vectors(settings(e.clone(), Duration::from_secs(3)), None)
        .unwrap();
    let t = Instant::now();
    for i in 0..20 {
        create(
            &s,
            &format!("otter-{i}"),
            RecordKind::Fact,
            &format!("otter note {i}"),
        );
    }
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "writes held up: {:?}",
        t.elapsed()
    );
    let b = attach(&s);
    // Text recall answers; vectors are not Ready. The first recall may wait
    // for its query vector (at most 2 s); the failure is then held.
    let t = Instant::now();
    let got = recall(&s, &b, "otter", Mode::Advisory);
    assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
    assert!(matches!(got.index.fts, IndexState::Ready { .. }));
    assert!(
        !matches!(got.index.vectors, IndexState::Ready { .. }),
        "{:?}",
        got.index
    );
    assert_eq!(got.items.len(), 10);
    let t = Instant::now();
    recall(&s, &b, "otter", Mode::Advisory);
    assert!(
        t.elapsed() < Duration::from_millis(1_500),
        "{:?}",
        t.elapsed()
    );
    // Failing batches report Failed, retried with back-off.
    e.mode.store(FAIL, Ordering::Release);
    let start = Instant::now();
    while !matches!(
        s.index_report(s.actor.snapshot().epoch).vectors,
        IndexState::Failed { .. }
    ) {
        assert!(start.elapsed() < Duration::from_secs(15), "never Failed");
        std::thread::sleep(Duration::from_millis(10));
    }
    // A working embedder again: every head gets its vector.
    e.mode.store(WORK, Ordering::Release);
    settle(&s);
    assert_eq!(
        s.vectors
            .get()
            .unwrap()
            .stats
            .embedded
            .load(Ordering::Relaxed),
        20
    );
    // Shutdown is not held up by a hung call.
    e.mode.store(HANG, Ordering::Release);
    create(&s, "late", RecordKind::Fact, "a late note");
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    s.stop_indexer();
    assert!(
        t.elapsed() < Duration::from_millis(1_000),
        "{:?}",
        t.elapsed()
    );
}

/// `ctx reindex --vectors` reports Rebuilding or Ready and re-embeds only
/// what is missing; a reverted revision reuses its vector.
#[test]
fn reindex_and_reuse_by_content() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let e = Arc::new(Flaky {
        mode: AtomicU8::new(WORK),
        calls: AtomicUsize::new(0),
    });
    s.start_vectors(settings(e.clone(), Duration::from_secs(5)), None)
        .unwrap();
    for i in 0..6 {
        create(&s, &format!("k{i}"), RecordKind::Fact, &format!("body {i}"));
    }
    settle(&s);
    let embedded = |s: &Svc| {
        s.vectors
            .get()
            .unwrap()
            .stats
            .embedded
            .load(Ordering::Relaxed)
    };
    assert_eq!(embedded(&s), 6);
    match call(
        &s,
        ADMIN,
        Request::Reindex {
            target: ReindexTarget::Vectors,
        },
    ) {
        Response::Reindexed { index } => assert!(
            matches!(
                index.vectors,
                IndexState::Rebuilding { .. } | IndexState::Ready { .. }
            ),
            "{index:?}"
        ),
        other => panic!("{other:?}"),
    }
    std::thread::sleep(Duration::from_millis(100));
    settle(&s);
    assert_eq!(embedded(&s), 6, "nothing missing: nothing re-embedded");
    // The same content under another key shares the vector.
    create(&s, "copy", RecordKind::Fact, "body 3");
    settle(&s);
    assert_eq!(embedded(&s), 6);
    match call(&s, ADMIN, Request::Status) {
        Response::Status { index, .. } => {
            assert!(
                matches!(index.vectors, IndexState::Ready { .. }),
                "{index:?}"
            )
        }
        other => panic!("{other:?}"),
    }
}

/// End to end: `ctxd` reads `embeddings.toml`, `ctx status` reports the
/// vectors, they survive a restart in `vectors.sqlite` (nothing is embedded
/// again), and an unusable file leaves vectors `Failed` while recall works.
#[test]
fn ctxd_reads_embeddings_toml_and_keeps_vectors_across_restart() {
    use contextdb_daemon::server::{Client, Daemon, Options};
    let dir = std::env::temp_dir().join(format!("ctxd-p8-vec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let status = |c: &mut Client| match c.call(&Request::Status).unwrap().response {
        Response::Status { index, .. } => index,
        other => panic!("{other:?}"),
    };
    let ready = |d: &Daemon| {
        let start = Instant::now();
        while !matches!(
            d.service
                .index_report(d.service.actor.snapshot().epoch)
                .vectors,
            IndexState::Ready { .. }
        ) {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "vectors never Ready"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    // No file: Disabled.
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    assert_eq!(status(&mut admin).vectors, IndexState::Disabled);
    for i in 0..5 {
        let body = format!("note {i} about otters");
        let r = admin
            .call(&Request::Raw {
                blobs: vec![body.as_bytes().to_vec()],
                batch: Batch::new(vec![Op::CreateLogical {
                    key: format!("k{i}"),
                    kind: RecordKind::Fact,
                    scope_lane: "repo".into(),
                    content: h(body.as_bytes()),
                    adopt_as_base: true,
                }]),
            })
            .unwrap();
        assert!(matches!(r.response, Response::Committed { .. }), "{r:?}");
    }
    d.stop();
    // A mock embedder: every head gets a vector.
    std::fs::write(
        dir.join("embeddings.toml"),
        "[embedder]\ntype = \"mock\"\ndims = 32\n",
    )
    .unwrap();
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    ready(&d);
    let v = d.service.vectors.get().unwrap();
    assert_eq!(v.stats.embedded.load(Ordering::Relaxed), 5);
    assert_eq!(v.model(), "mock-bow-32");
    d.stop();
    assert!(dir.join("vectors.sqlite").exists());
    // Restart: the vectors are kept.
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    ready(&d);
    assert_eq!(
        d.service
            .vectors
            .get()
            .unwrap()
            .stats
            .embedded
            .load(Ordering::Relaxed),
        0
    );
    d.stop();
    // An unusable file: Failed with the reason; the daemon still serves.
    std::fs::write(dir.join("embeddings.toml"), "[embedder]\ndims = 8\n").unwrap();
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    match status(&mut admin).vectors {
        IndexState::Failed { error } => assert!(error.contains("embedder.command"), "{error}"),
        other => panic!("{other:?}"),
    }
    d.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
