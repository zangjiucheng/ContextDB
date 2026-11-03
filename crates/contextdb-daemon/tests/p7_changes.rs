//! P7 milestone 8 (ADR 0015 §8): `ctx_changes` from the durable `diff_log`.
//! T76: the concatenated pages equal the replay's per-epoch `status_diff`s
//! for the branch, with each post-state's summary; `Truncated` only when
//! the epochs cannot be served. Also the long-poll and its bounds.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use std::path::Path;
use std::sync::Arc;
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
fn service() -> Svc {
    let disk = SimDisk::new();
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk, dir, false, 1 << 20).unwrap();
    Service::new(actor::spawn(store).unwrap(), log, events).unwrap()
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
fn create(s: &Svc, branch: BranchId, key: &str, scope: &str) -> RevisionId {
    raw(
        s,
        branch,
        vec![key.as_bytes().to_vec()],
        vec![Op::CreateLogical {
            key: key.into(),
            kind: RecordKind::Fact,
            scope_lane: scope.into(),
            content: h(key.as_bytes()),
            adopt_as_base: true,
        }],
    )
    .allocations
    .new_revisions[0]
}
fn revise(s: &Svc, branch: BranchId, key: &str, n: u32) {
    let snap = s.actor.snapshot();
    let view = snap.view(branch);
    let l = view.logical_by_key(key).unwrap();
    let old = view.head(l).unwrap();
    drop(snap);
    let body = format!("{key} v{n}");
    raw(
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
}
fn page(
    s: &Svc,
    id: Identity,
    branch: Option<&str>,
    since: Epoch,
    scope: Option<&str>,
    max: u32,
    wait_ms: u32,
) -> Response {
    s.handle(
        id,
        Request::Changes {
            binding: None,
            branch: branch.map(str::to_owned),
            since_epoch: since,
            scope: scope.map(str::to_owned),
            max_items: max,
            wait_ms,
        },
    )
    .response
}
/// Every page from epoch 0, in order, checking that pages end on epoch
/// boundaries.
fn all_pages(s: &Svc, branch: Option<&str>, scope: Option<&str>, max: u32) -> Vec<ChangeItem> {
    let mut since = Epoch(0);
    let mut out: Vec<ChangeItem> = Vec::new();
    for _ in 0..10_000 {
        match page(s, ADMIN, branch, since, scope, max, 0) {
            Response::Changes {
                items,
                reached,
                more,
            } => {
                assert!(items.iter().all(|i| i.epoch > since && i.epoch <= reached));
                out.extend(items);
                since = reached;
                if !more {
                    return out;
                }
            }
            other => panic!("{other:?}"),
        }
    }
    panic!("no end")
}
/// The workload: main and dev, with rules whose premises move, so statuses
/// change across several revisions per epoch.
fn workload(s: &Svc) -> BranchId {
    for i in 0..4 {
        create(s, MAIN, &format!("p{i}"), "repo");
    }
    let c = create(s, MAIN, "c", "ops");
    raw(
        s,
        MAIN,
        Vec::new(),
        vec![Op::AddRule {
            conclusion: c.into(),
            incidences: vec![
                (RevisionId(0).into(), EdgeKind::Sem),
                (RevisionId(1).into(), EdgeKind::Cur),
            ],
            enabled: true,
        }],
    );
    let created = s
        .handle(
            ADMIN,
            Request::BranchCreate {
                name: "dev".into(),
                from: None,
                client_txn_id: None,
            },
        )
        .response;
    assert!(!matches!(created, Response::Error { .. }), "{created:?}");
    let dev = s.actor.snapshot().kernel.branch_by_name("dev").unwrap();
    for n in 1..4 {
        revise(s, MAIN, "p0", n);
        revise(s, dev, "p1", n);
        revise(s, MAIN, "p1", n + 10);
        create(s, dev, &format!("d{n}"), "repo");
    }
    dev
}
/// What a replay says: per epoch of `branch`, its status_diff with each
/// post-state's summary.
fn replayed(s: &Svc, branch: BranchId) -> Vec<(Epoch, RevisionId, Summary)> {
    let snap = s.actor.snapshot();
    let mut out = Vec::new();
    for e in 1..=snap.epoch.0 {
        let row = snap.log.read_row(Epoch(e)).unwrap();
        if row.batch().unwrap().branch != branch {
            continue;
        }
        let result = row.result().unwrap();
        let at = snap.log.replay_kernel_to(Epoch(e)).unwrap();
        let view = at.branch(branch).unwrap();
        let mut revs = result.status_diff.clone();
        revs.sort();
        for r in revs {
            out.push((Epoch(e), r, view.status(r).unwrap()));
        }
    }
    out
}

/// T76: for both branches and several page sizes, the concatenated pages
/// equal the replay's per-epoch status diffs and summaries.
#[test]
fn t76_pages_equal_the_replayed_status_diffs() {
    let s = service();
    let dev = workload(&s);
    for (name, b) in [(None, MAIN), (Some("dev"), dev)] {
        let want = replayed(&s, b);
        assert!(want.len() >= 8, "{name:?}: {}", want.len());
        assert!(
            want.windows(2).any(|w| w[0].0 == w[1].0),
            "some epoch changes several revisions"
        );
        for max in [1, 2, 3, 7, 1000] {
            let got: Vec<_> = all_pages(&s, name, None, max)
                .into_iter()
                .map(|i| (i.epoch, i.rev, i.summary))
                .collect();
            assert_eq!(got, want, "{name:?} max {max}");
        }
    }
    // A scope filter drops items, never epochs.
    let ops = all_pages(&s, None, Some("ops"), 2);
    assert!(!ops.is_empty());
    assert!(ops.iter().all(|i| i.key == "c"));
    // Keys come from the branch.
    assert!(
        all_pages(&s, Some("dev"), None, 50)
            .iter()
            .any(|i| i.key.starts_with('d'))
    );
    assert!(
        !all_pages(&s, None, None, 50)
            .iter()
            .any(|i| i.key.starts_with('d'))
    );
}

/// `Truncated` only when the epochs after `since` cannot be served: a
/// reader ahead of the log. Agents need a binding.
#[test]
fn t76_truncated_and_refusals() {
    let s = service();
    create(&s, MAIN, "a", "repo");
    let tail = s.actor.snapshot().epoch;
    assert_eq!(
        page(&s, ADMIN, None, Epoch(tail.0 + 5), None, 10, 0),
        Response::Truncated { earliest: tail }
    );
    // At the tail: nothing, not truncated.
    assert_eq!(
        page(&s, ADMIN, None, tail, None, 10, 0),
        Response::Changes {
            items: Vec::new(),
            reached: tail,
            more: false
        }
    );
    match page(&s, AGENT, None, Epoch(0), None, 10, 0) {
        Response::Error { code, .. } => assert_eq!(code, "InvalidArgument"),
        other => panic!("{other:?}"),
    }
    match page(&s, ADMIN, Some("nope"), Epoch(0), None, 10, 0) {
        Response::Error { .. } => {}
        other => panic!("{other:?}"),
    }
}

/// The long-poll wakes on the next epoch with an item, times out with an
/// empty page, and at most 64 requests wait at once (`Busy` beyond).
#[test]
fn long_poll_wakes_times_out_and_is_bounded() {
    let s = Arc::new(service());
    create(&s, MAIN, "a", "repo");
    let tail = s.actor.snapshot().epoch;
    // Times out empty.
    let t = Instant::now();
    match page(&s, ADMIN, None, tail, None, 10, 150) {
        Response::Changes { items, reached, .. } => {
            assert!(items.is_empty());
            assert_eq!(reached, tail);
        }
        other => panic!("{other:?}"),
    }
    assert!(t.elapsed() >= Duration::from_millis(140));
    // Wakes on a commit.
    let waiter = {
        let s = s.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            (page(&s, ADMIN, None, tail, None, 10, 10_000), t.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    create(&s, MAIN, "b", "repo");
    let (r, waited) = waiter.join().unwrap();
    match r {
        Response::Changes { items, .. } => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].key, "b");
        }
        other => panic!("{other:?}"),
    }
    assert!(waited < Duration::from_secs(5), "{waited:?}");
    // At most 64 waiters.
    let tail = s.actor.snapshot().epoch;
    let waiters: Vec<_> = (0..64)
        .map(|_| {
            let s = s.clone();
            std::thread::spawn(move || page(&s, ADMIN, None, tail, None, 10, 10_000))
        })
        .collect();
    // Let the 64 start waiting, then one more asks.
    std::thread::sleep(Duration::from_millis(500));
    let r = page(&s, ADMIN, None, tail, None, 10, 50);
    assert!(
        matches!(&r, Response::Error { code, .. } if code == "Busy"),
        "the 65th waiter is refused: {r:?}"
    );
    create(&s, MAIN, "c", "repo");
    for w in waiters {
        assert!(matches!(w.join().unwrap(), Response::Changes { .. }));
    }
    // Slots are released: a new wait is accepted.
    assert!(matches!(
        page(&s, ADMIN, None, s.actor.snapshot().epoch, None, 10, 20),
        Response::Changes { .. }
    ));
}

/// The actor's epoch signal: `wait_epoch` blocks for the timeout when
/// nothing is published, and returns at once for a past epoch or soon
/// after a publication.
#[test]
fn wait_epoch_blocks_until_a_publication() {
    let s = Arc::new(service());
    create(&s, MAIN, "a", "repo");
    let tail = s.actor.snapshot().epoch;
    let t = Instant::now();
    assert_eq!(s.actor.wait_epoch(tail, Duration::from_millis(150)), tail);
    assert!(t.elapsed() >= Duration::from_millis(140));
    let t = Instant::now();
    assert_eq!(s.actor.wait_epoch(Epoch(0), Duration::from_secs(5)), tail);
    assert!(t.elapsed() < Duration::from_millis(50));
    let committer = {
        let s = s.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            create(&s, MAIN, "b", "repo");
        })
    };
    let got = s.actor.wait_epoch(tail, Duration::from_secs(5));
    assert_eq!(got, Epoch(tail.0 + 1));
    committer.join().unwrap();
}
