//! P11 milestone 3 (ADR 0019 §3.5, §3.7, §3.9, §4): ctxd acknowledges a
//! cascading commit within its budget and settles it when idle or on a
//! pull; the spare replays the store's steps exactly; commit responses and
//! `TxnStatus` carry `settling` over protocol 9; `ctxd.toml` turns lazy
//! maintenance off.
use contextdb_daemon::{
    actor::{Acked, ActorHandle, Message, WriteRequest},
    proto::*,
    server::{Client, Daemon, Options, StartError},
};
use contextdb_model::*;
use contextdb_store::StdDisk;
use contextdb_testkit::{Shape, shape};
use std::{
    path::PathBuf,
    sync::{atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

const N: usize = 300;
/// A budget the hub's cascade exceeds many times over.
const SMALL: &str = "[maintenance]\nlazy_budget = 64\n";
const PAYLOAD: &[u8] = b"payload";

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ctxd-p11-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
fn start(name: &str, ctxd_toml: &str) -> Daemon {
    let dir = tmp(name);
    std::fs::write(dir.join("ctxd.toml"), ctxd_toml).unwrap();
    let options = Options {
        full_sync: false,
        ..Options::default()
    };
    Daemon::start(&dir, &options).unwrap()
}
fn actor(d: &Daemon) -> &ActorHandle<StdDisk> {
    &d.service.actor
}
fn admin(b: Batch) -> Batch {
    Batch {
        channel: Channel::Admin,
        ..b
    }
}
/// A hub of `N` revisions: 0, 1 and 2 are bases and every other revision
/// concludes a rule on all three, so toggling revision 0 reaches them all.
fn hub(a: &ActorHandle<StdDisk>) {
    let content = *blake3::hash(PAYLOAD).as_bytes();
    for mut b in shape(Shape::Hub, N, 42).batches {
        for op in &mut b.ops {
            if let Op::CreateLogical { content: c, .. } = op {
                *c = content;
            }
        }
        a.write(vec![PAYLOAD.to_vec()], admin(b)).unwrap().unwrap();
    }
    settled(a);
}
fn toggle(on: bool) -> Batch {
    let rev = RevisionId(0).into();
    admin(Batch::new(vec![if on {
        Op::AssertBase { rev }
    } else {
        Op::RetractBase { rev }
    }]))
}
fn create(key: &str) -> (Vec<u8>, Batch) {
    let content = *blake3::hash(key.as_bytes()).as_bytes();
    let b = Batch::new(vec![Op::CreateLogical {
        origin: Default::default(),
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content,
        adopt_as_base: true,
    }]);
    (key.as_bytes().to_vec(), admin(b))
}
/// Submits a write without waiting for it.
fn submit(
    a: &ActorHandle<StdDisk>,
    blobs: Vec<Vec<u8>>,
    batch: Batch,
) -> mpsc::Receiver<Result<Acked, contextdb_store::StoreError>> {
    let (reply, rx) = mpsc::channel();
    a.submit(Message::Write(WriteRequest {
        blobs,
        batch,
        client_hash: None,
        expect: None,
        reply,
    }))
    .unwrap();
    rx
}
fn pending(a: &ActorHandle<StdDisk>) -> Option<Epoch> {
    a.snapshot().db().frontier_epoch()
}
/// Waits on publications until the published snapshot has no frontier;
/// returns the generation that showed it.
fn settled(a: &ActorHandle<StdDisk>) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let snap = a.snapshot();
        if !snap.db().has_frontier() {
            return snap.generation;
        }
        let g = snap.generation;
        drop(snap);
        assert!(Instant::now() < deadline, "never settled");
        a.wait_generation(g, Duration::from_secs(1));
    }
}
/// Every message queued before this one has been handled.
fn barrier(a: &ActorHandle<StdDisk>) {
    a.request(|reply| Message::Checksum {
        epoch: Epoch(0),
        reply,
    })
    .unwrap()
    .unwrap();
}

/// The ack of a cascade over protocol 9 is `settling` with empty diffs;
/// the idle drain then settles it, publishing newer generations, and the
/// epoch's status changes reach `ctx_changes`.
#[test]
fn a_cascade_is_acked_settling_then_settled_when_idle() {
    let d = start("ack", SMALL);
    let a = actor(&d);
    hub(a);
    let mut c = Client::connect(d.socket(Channel::Admin)).unwrap();
    let env = c
        .call(&Request::Raw {
            blobs: Vec::new(),
            batch: toggle(false),
        })
        .unwrap();
    let Response::Committed { result, settling } = env.response else {
        panic!("{:?}", env.response)
    };
    assert!(settling, "the cascade exceeds the budget");
    assert!(result.l0_flips.is_empty() && result.status_diff.is_empty());
    let e = result.epoch;
    let acked_at = a.snapshot().generation;
    let generation = settled(a);
    assert!(generation > acked_at, "settling publishes");
    let snap = a.snapshot();
    assert_eq!(snap.epoch, e);
    assert!(!snap.kernel.branch(MAIN).unwrap().db().has_frontier());
    drop(snap);
    let Response::Changes { items, reached, .. } = c
        .call(&Request::Changes {
            binding: None,
            branch: None,
            since_epoch: Epoch(e.0 - 1),
            scope: None,
            max_items: 10_000,
            wait_ms: 0,
        })
        .unwrap()
        .response
    else {
        panic!()
    };
    assert_eq!(reached, e);
    // Revision 0 and every conclusion change; bases 1 and 2 do not.
    assert_eq!(items.iter().filter(|i| i.epoch == e).count(), N - 2);
    assert_eq!(a.stats.replay_mismatches.load(Ordering::Relaxed), 0);
}

/// A `ctx_changes` long poll from before a settling epoch wakes when the
/// epoch settles, though no new epoch is published.
#[test]
fn a_changes_long_poll_wakes_when_the_epoch_settles() {
    let d = start("poll", SMALL);
    let a = actor(&d);
    hub(a);
    let acked = a.write(Vec::new(), toggle(false)).unwrap().unwrap();
    assert!(acked.settling);
    let e = acked.result.epoch;
    let mut c = Client::connect(d.socket(Channel::Admin)).unwrap();
    let asked = Instant::now();
    let Response::Changes { items, reached, .. } = c
        .call(&Request::Changes {
            binding: None,
            branch: None,
            since_epoch: Epoch(e.0 - 1),
            scope: None,
            max_items: 10_000,
            wait_ms: 20_000,
        })
        .unwrap()
        .response
    else {
        panic!()
    };
    assert!(
        asked.elapsed() < Duration::from_secs(15),
        "woke on settling"
    );
    assert_eq!(reached, e);
    assert_eq!(items.iter().filter(|i| i.epoch == e).count(), N - 2);
}

/// `pull` sends `Settle`: the actor drains at least through the asked rank
/// and publishes, which wakes the waiter.
#[test]
fn a_pull_settles_through_its_rank_and_wakes_the_waiter() {
    let d = start("pull", SMALL);
    let a = actor(&d);
    hub(a);
    let acked = a.write(Vec::new(), toggle(false)).unwrap().unwrap();
    assert!(acked.settling);
    let snap = a.snapshot();
    let (generation, cursor) = (snap.generation, snap.db().cursor());
    drop(snap);
    let Some(cursor) = cursor else {
        panic!("settled before the pull")
    };
    let through = cursor + N / 2;
    let e = acked.result.epoch;
    assert!(a.pull(e, Some(through), generation, Duration::from_secs(10)));
    assert!(a.snapshot().generation > generation);
    // The `Settle` is handled once a later request is answered.
    barrier(a);
    let left = a.snapshot().db().cursor();
    assert!(left.is_none_or(|c| c > through), "{left:?} ≤ {through}");
    // A full pull settles; a pull for an epoch no longer pending does not
    // wait for a publication that never comes.
    let generation = a.snapshot().generation;
    if pending(a) == Some(e) {
        assert!(a.pull(e, None, generation, Duration::from_secs(10)));
        barrier(a);
    }
    assert_eq!(pending(a), None);
    let generation = a.snapshot().generation;
    assert!(!a.pull(e, None, generation, Duration::from_millis(50)));
}

/// Writes queued behind a settling commit commit after it, in order: the
/// store ends the group at the settling commit and the actor puts the rest
/// back at the head of its queue.
#[test]
fn writes_behind_a_settling_commit_commit_in_fifo_order() {
    let d = start("fifo", SMALL);
    let a = actor(&d);
    hub(a);
    // While the first toggle commits, the rest queue up: the second toggle
    // settles the first at the top of its group and ends that group.
    for round in 0..10 {
        let off = submit(a, Vec::new(), toggle(false));
        let on = submit(a, Vec::new(), toggle(true));
        let (x, bx) = create(&format!("fifo/{round}/x"));
        let x = submit(a, vec![x], bx);
        let (y, by) = create(&format!("fifo/{round}/y"));
        let y = submit(a, vec![y], by);
        let acks = [off, on, x, y].map(|rx| rx.recv().unwrap().unwrap());
        let settling = acks.each_ref().map(|a| a.settling);
        assert_eq!(settling, [true, true, false, false], "round {round}");
        let e = acks[0].result.epoch.0;
        let epochs = acks.each_ref().map(|a| a.result.epoch.0);
        assert_eq!(epochs, [e, e + 1, e + 2, e + 3], "round {round}");
        if round >= 2 && a.stats.requeued.load(Ordering::Relaxed) > 0 {
            break;
        }
    }
    assert!(
        a.stats.requeued.load(Ordering::Relaxed) > 0,
        "no group ended"
    );
    settled(a);
    assert_eq!(a.stats.replay_mismatches.load(Ordering::Relaxed), 0);
}

/// Many groups, idle slices, pulls and dry runs in between: the spare
/// replays every step with no mismatch, and the settled state equals an
/// eager replay of the log.
#[test]
fn the_spare_replays_commits_and_drains_exactly() {
    let d = start("replay", SMALL);
    let a = actor(&d);
    hub(a);
    let start = a.snapshot().generation;
    for round in 0..12u32 {
        let acked = a
            .write(Vec::new(), toggle(round % 2 == 1))
            .unwrap()
            .unwrap();
        assert!(acked.settling, "round {round}");
        let e = acked.result.epoch;
        match round % 4 {
            // Let some idle slices run.
            0 => std::thread::sleep(Duration::from_millis(5)),
            1 => {
                let snap = a.snapshot();
                let (g, c) = (snap.generation, snap.db().cursor().unwrap_or(0));
                drop(snap);
                a.pull(e, Some(c + 3), g, Duration::from_secs(5));
            }
            // A dry run settles on the actor first.
            2 => {
                a.request(|reply| Message::WhatIf {
                    batch: toggle(round % 2 == 0),
                    reply,
                })
                .unwrap()
                .unwrap();
                assert_eq!(pending(a), None, "the settle was published");
            }
            // The next group settles at its top.
            _ => {}
        }
        let (x, bx) = create(&format!("replay/{round}"));
        let x = a.write(vec![x], bx).unwrap().unwrap();
        assert!(x.result.epoch > e);
    }
    settled(a);
    let stats = &a.stats;
    assert_eq!(stats.replay_mismatches.load(Ordering::Relaxed), 0);
    let snap = a.snapshot();
    assert!(snap.generation > start + 24, "drains are published too");
    let eager = snap.log.replay_kernel_to(snap.epoch).unwrap();
    assert_eq!(
        snap.kernel.branch(MAIN).unwrap().digest(),
        eager.branch(MAIN).unwrap().digest()
    );
}

/// An idempotent retry and `TxnStatus` report what the frame recorded.
#[test]
fn a_retry_and_txn_status_report_settling() {
    let d = start("retry", SMALL);
    let a = actor(&d);
    hub(a);
    let id = [7; 16];
    let batch = Batch {
        client_txn_id: Some(id),
        ..toggle(false)
    };
    let first = a.write(Vec::new(), batch.clone()).unwrap().unwrap();
    assert!(first.settling);
    let retry = a.write(Vec::new(), batch.clone()).unwrap().unwrap();
    assert_eq!(retry, first);
    settled(a);
    let again = a.write(Vec::new(), batch).unwrap().unwrap();
    assert_eq!(again, first, "the frame keeps what was acknowledged");
    let mut c = Client::connect(d.socket(Channel::Admin)).unwrap();
    let status = c.call(&Request::TxnStatus { id }).unwrap().response;
    assert_eq!(
        status,
        Response::TxnStatus {
            committed: Some((first.result.epoch, first.result.clone())),
            settling: true,
        }
    );
}

/// `lazy = false` in `ctxd.toml`: every commit is eager.
#[test]
fn lazy_off_in_ctxd_toml_never_leaves_a_frontier() {
    let d = start("eager", "[maintenance]\nlazy = false\nlazy_budget = 64\n");
    let a = actor(&d);
    hub(a);
    for round in 0..4 {
        let acked = a
            .write(Vec::new(), toggle(round % 2 == 1))
            .unwrap()
            .unwrap();
        assert!(!acked.settling);
        assert_eq!(acked.result.status_diff.len(), N - 2);
        assert_eq!(pending(a), None);
    }
}

/// An invalid `ctxd.toml` refuses to start, with the reason.
#[test]
fn an_invalid_ctxd_toml_refuses_to_start() {
    let dir = tmp("invalid");
    std::fs::write(dir.join("ctxd.toml"), "[maintenance]\nlazy = 3\n").unwrap();
    match Daemon::start(&dir, &Options::default()) {
        Err(StartError::Config(reason)) => {
            assert!(reason.contains("maintenance.lazy"), "{reason}")
        }
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("started"),
    }
}
