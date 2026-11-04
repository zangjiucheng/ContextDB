//! P8 milestone 2 (ADR 0016 §3): eligibility before ranking, paging
//! rankers, structured scope, RRF fusion and compatibility. T54, T85, T95.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, MAX_RECALL, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use std::path::Path;
use std::time::Duration;

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
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    Service::new(actor::spawn(store).unwrap(), log, events).unwrap()
}
fn shutdown(s: Service<SimDisk>) {
    assert_eq!(s.handle(ADMIN, Request::Shutdown).response, Response::Done);
}
fn attach(s: &Service<SimDisk>, key: &str) -> Binding {
    match (s.handle(
        AGENT,
        Request::SessionAttach {
            harness: "claude".into(),
            harness_session: key.into(),
            repo: "/repo".into(),
            head_sha: "h".into(),
            branch: None,
            git_branch: None,
        },
    ))
    .response
    {
        Response::Attached { session, .. } => Binding::root(session),
        other => panic!("{other:?}"),
    }
}
/// Creates `key`; `certified` adopts it as base (CERTIFIED), otherwise it
/// stays PENDING.
fn create(
    s: &Service<SimDisk>,
    b: &Binding,
    key: &str,
    kind: RecordKind,
    lane: &str,
    body: &str,
    certified: bool,
) -> LogicalId {
    match (s.handle(
        AGENT,
        Request::Record {
            binding: b.clone(),
            kind,
            body: body.as_bytes().to_vec(),
            target: WriteTarget::Create {
                key: key.into(),
                scope_lane: lane.into(),
            },
            deps: Vec::new(),
            adopt_as_base: certified,
            client_txn_id: None,
        },
    ))
    .response
    {
        Response::Committed { result } => result.allocations.new_logicals[0],
        other => panic!("{key}: {other:?}"),
    }
}
struct Got {
    items: Vec<Item>,
    withheld: Vec<Item>,
    limit_clamped: bool,
}
fn recall(
    s: &Service<SimDisk>,
    b: &Binding,
    query: &str,
    scope: &str,
    opts: RecallOpts,
    mode: Mode,
    limit: u32,
) -> Got {
    // Wait for the text index first, so text hits are deterministic.
    for _ in 0..400 {
        match (s.handle(
            AGENT,
            Request::Recall {
                binding: b.clone(),
                query: query.into(),
                scope: scope.into(),
                mode,
                limit,
                index_wait_ms: 2_000,
                opts: opts.clone(),
            },
        ))
        .response
        {
            Response::Recall {
                items,
                withheld,
                index,
                limit_clamped,
                ..
            } => {
                if matches!(index.fts, IndexState::Ready { .. }) {
                    return Got {
                        items,
                        withheld,
                        limit_clamped,
                    };
                }
            }
            other => panic!("{other:?}"),
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the index never became ready")
}
fn keys(items: &[Item]) -> Vec<&str> {
    items.iter().map(|i| i.key.as_str()).collect()
}
fn plain() -> RecallOpts {
    RecallOpts::default()
}

/// T54: the r0.1 review's counterexample. 60 PENDING `auth/*` keys and one
/// CERTIFIED: certified recall returns the certified one (filtering after
/// the cut returned nothing), withheld entries are capped at `limit` and
/// never take an item slot, and a short page means the rankers ran out.
#[test]
fn t54_filtering_happens_before_the_cut() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    for n in 0..60 {
        create(
            &s,
            &a,
            &format!("auth/p{n:02}"),
            RecordKind::Decision,
            "repo",
            "pending auth note",
            false,
        );
    }
    create(
        &s,
        &a,
        "auth/zz-good",
        RecordKind::Decision,
        "repo",
        "certified",
        true,
    );
    let got = recall(&s, &a, "auth", "", plain(), Mode::Certified, 10);
    assert_eq!(keys(&got.items), ["auth/zz-good"]);
    assert_eq!(got.withheld.len(), 10, "withheld capped at limit");
    assert!((got.withheld.iter()).all(|i| i.summary != Summary::Certified));
    // With more certified records than `limit`, the page is full and holds
    // only certified items.
    for n in 0..15 {
        create(
            &s,
            &a,
            &format!("auth/c{n:02}"),
            RecordKind::Decision,
            "repo",
            "certified",
            true,
        );
    }
    let got = recall(&s, &a, "auth", "", plain(), Mode::Certified, 10);
    assert_eq!(got.items.len(), 10);
    assert!(got.items.iter().all(|i| i.summary == Summary::Certified));
    // Advisory: every item carries its own label, PENDING included.
    let got = recall(&s, &a, "auth", "", plain(), Mode::Advisory, 100);
    assert_eq!(got.items.len(), 76);
    assert!(got.withheld.is_empty());
    for i in &got.items {
        let read = s
            .handle(
                AGENT,
                Request::Read {
                    binding: a.clone(),
                    target: Target::Key(i.key.clone()),
                    mode: Mode::Advisory,
                },
            )
            .response;
        let Response::Read { item, .. } = read else {
            panic!("{read:?}")
        };
        assert_eq!((item.rev, &item.summary), (i.rev, &i.summary), "{}", i.key);
    }
    shutdown(s);
}

/// T54 through the text ranker alone: certified hits ranked below more
/// than a page of PENDING ones are still found by paging.
#[test]
fn t54_the_text_ranker_pages_past_ineligible_hits() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    for n in 0..80 {
        create(
            &s,
            &a,
            &format!("p{n:02}"),
            RecordKind::Fact,
            "repo",
            "otter otter otter",
            false,
        );
    }
    for n in 0..3 {
        create(
            &s,
            &a,
            &format!("c{n}"),
            RecordKind::Fact,
            "repo",
            &format!("a long body that mentions the otter once among many other words {n}"),
            true,
        );
    }
    // limit 2: a page is 24 hits; the certified ones rank after all 80.
    let got = recall(&s, &a, "otter", "", plain(), Mode::Certified, 2);
    assert_eq!(got.items.len(), 2, "{:?}", keys(&got.items));
    assert!(got.items.iter().all(|i| i.key.starts_with('c')));
    // limit 5: only 3 eligible exist, so the short page means exhaustion.
    let got = recall(&s, &a, "otter", "", plain(), Mode::Certified, 5);
    assert_eq!(keys(&got.items).len(), 3);
    shutdown(s);
}

/// T85: an exact key match is first; fusion is deterministic; a record
/// both rankers find ranks above one only a single ranker finds at the
/// same rank; `kinds` and `dir` (via `dirs` metadata) filter before the
/// cut.
#[test]
fn t85_fusion_order_and_structured_scope() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    let mk = |key: &str, kind, body: &str| create(&s, &a, key, kind, "repo", body, true);
    mk("heron", RecordKind::Decision, "nothing relevant");
    let both = mk("heron-cache", RecordKind::Decision, "heron cache policy");
    mk("notes", RecordKind::Fact, "heron heron heron");
    mk("plan-x", RecordKind::Plan, "the heron plan");
    let net = mk("net-rule", RecordKind::Decision, "heron over the network");
    let first = recall(&s, &a, "heron", "", plain(), Mode::Advisory, 10);
    assert_eq!(first.items[0].key, "heron", "exact key first");
    for _ in 0..3 {
        let again = recall(&s, &a, "heron", "", plain(), Mode::Advisory, 10);
        assert_eq!(keys(&again.items), keys(&first.items), "deterministic");
    }
    let pos = |k: &str| keys(&first.items).iter().position(|x| *x == k).unwrap();
    assert!(
        pos("heron-cache") < pos("plan-x") && pos("heron-cache") < pos("net-rule"),
        "{:?}",
        keys(&first.items)
    );
    let _ = both;
    // kinds.
    let only = |kinds: Vec<RecordKind>| RecallOpts { kinds, dir: None };
    let plans = recall(
        &s,
        &a,
        "heron",
        "",
        only(vec![RecordKind::Plan]),
        Mode::Advisory,
        10,
    );
    assert_eq!(keys(&plans.items), ["plan-x"]);
    let two = recall(
        &s,
        &a,
        "heron",
        "",
        only(vec![RecordKind::Fact, RecordKind::Plan]),
        Mode::Advisory,
        10,
    );
    let mut k = keys(&two.items);
    k.sort();
    assert_eq!(k, ["notes", "plan-x"]);
    // dir through `dirs` metadata.
    let set = s.handle(
        ADMIN,
        Request::Raw {
            blobs: Vec::new(),
            batch: Batch::new(vec![Op::SetMetadata {
                logical: LogicalRef::Existing(net),
                tags: Default::default(),
                dirs: ["src/net".to_string()].into(),
            }]),
        },
    );
    assert!(
        matches!(set.response, Response::Committed { .. }),
        "{set:?}"
    );
    let dir = |d: &str| RecallOpts {
        kinds: Vec::new(),
        dir: Some(d.into()),
    };
    for (d, want) in [
        ("src", vec!["net-rule"]),
        ("src/net/", vec!["net-rule"]),
        ("src/ne", vec![]),
        ("lib", vec![]),
    ] {
        let got = recall(&s, &a, "heron", "", dir(d), Mode::Advisory, 10);
        assert_eq!(keys(&got.items), want, "dir {d}");
    }
    shutdown(s);
}

/// T95: a plain scope string is a lane; `limit` above 100 is clamped and
/// reported; without `budget_tokens` and `level` the response is headers
/// only (an `Item` carries no body).
#[test]
fn t95_compatibility() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    for n in 0..120 {
        let lane = if n % 2 == 0 { "repo" } else { "other" };
        create(
            &s,
            &a,
            &format!("k{n:03}"),
            RecordKind::Fact,
            lane,
            "body",
            true,
        );
    }
    let got = recall(&s, &a, "k", "other", plain(), Mode::Advisory, 10);
    assert_eq!(got.items.len(), 10);
    assert!(!got.limit_clamped);
    let lanes = |items: &[Item]| {
        items
            .iter()
            .all(|i| i.key[1..].parse::<u32>().unwrap() % 2 == 1)
    };
    assert!(lanes(&got.items), "{:?}", keys(&got.items));
    let got = recall(&s, &a, "k", "", plain(), Mode::Advisory, 5_000);
    assert!(got.limit_clamped);
    assert_eq!(got.items.len(), MAX_RECALL);
    assert_eq!(MAX_RECALL, 100);
    let got = recall(&s, &a, "k", "", plain(), Mode::Advisory, 100);
    assert!(!got.limit_clamped);
    shutdown(s);
}
