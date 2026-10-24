//! ADR 0008 §3: group commit through the actor, two-replica publication,
//! snapshot immutability (I4) and the clone fallback.
use contextdb_daemon::actor::{self, SubmitError};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store};
use std::path::Path;

fn store() -> Store<SimDisk> {
    Store::open(SimDisk::new(), Path::new("/db"), &OpenOptions::default())
        .unwrap()
        .0
}
fn create(key: &str, body: &[u8]) -> (Vec<Vec<u8>>, Batch) {
    let content = *blake3::hash(body).as_bytes();
    (
        vec![body.to_vec()],
        Batch::new(vec![Op::CreateLogical {
            key: key.into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content,
            adopt_as_base: true,
        }]),
    )
}
#[test]
fn writes_publish_epochs_and_old_snapshots_never_change() {
    let h = actor::spawn(store()).unwrap();
    assert_eq!(h.snapshot().epoch, Epoch(0));
    let (blobs, b) = create("a", b"a");
    let r = h.write(blobs, b).unwrap().unwrap();
    assert_eq!(r.epoch, Epoch(1));
    let held = h.snapshot();
    let held_digest = held.db().digest();
    assert_eq!(held.epoch, Epoch(1));
    for n in 0..20 {
        let (blobs, b) = create(&format!("k{n}"), format!("body {n}").as_bytes());
        h.write(blobs, b).unwrap().unwrap();
    }
    assert_eq!(h.snapshot().epoch, Epoch(21));
    // The held snapshot is immutable although 20 groups were published.
    assert_eq!(held.db().digest(), held_digest);
    assert_eq!(held.db().epoch(), Epoch(1));
    let stats = &h.stats;
    assert!(
        stats
            .clone_fallbacks
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1,
        "the held snapshot forced a clone"
    );
}
#[test]
fn concurrent_writers_group_and_lose_nothing() {
    let h = actor::spawn(store()).unwrap();
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let h = h.clone();
            std::thread::spawn(move || {
                (0..25)
                    .map(|n| {
                        let (blobs, b) =
                            create(&format!("t{t}-{n}"), format!("{t}/{n}").as_bytes());
                        h.write(blobs, b).unwrap().unwrap().epoch
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut epochs: Vec<_> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    epochs.sort();
    assert_eq!(
        epochs,
        (1..=200).map(Epoch).collect::<Vec<_>>(),
        "200 distinct epochs"
    );
    let snap = h.snapshot();
    assert_eq!(snap.epoch, Epoch(200));
    assert_eq!(snap.db().revision_count(), 200);
    let groups = h.stats.groups.load(std::sync::atomic::Ordering::Relaxed);
    assert!(groups <= 200, "{groups} groups");
    // The log reader in the snapshot is bounded by the same epoch.
    assert_eq!(snap.log.tail(), Epoch(200));
    assert_eq!(
        snap.log.replay_to(snap.epoch).unwrap().digest(),
        snap.db().digest()
    );
}
#[test]
fn rejected_batches_publish_nothing_and_shutdown_stops_the_actor() {
    let h = actor::spawn(store()).unwrap();
    let r = h
        .write(
            Vec::new(),
            Batch::new(vec![Op::RetractBase {
                rev: RevisionId(9).into(),
            }]),
        )
        .unwrap();
    assert!(r.is_err());
    assert_eq!(h.snapshot().epoch, Epoch(0));
    h.request(|reply| actor::Message::Shutdown { reply })
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let (blobs, b) = create("late", b"late");
    assert_eq!(h.write(blobs, b).err(), Some(SubmitError::Stopped));
}
#[test]
fn sequential_writes_reclaim_replicas_without_clones() {
    let h = actor::spawn(store()).unwrap();
    for n in 0..30 {
        let (blobs, b) = create(&format!("s{n}"), format!("s {n}").as_bytes());
        h.write(blobs, b).unwrap().unwrap();
    }
    let clones = h
        .stats
        .clone_fallbacks
        .load(std::sync::atomic::Ordering::Relaxed);
    // Only the first group (no spare yet) may clone.
    assert!(
        clones <= 1,
        "{clones} clone fallbacks for 30 sequential writes"
    );
}
/// ADR 0008 §5: a request past its timeout gets `Timeout`; a timed-out write
/// may still commit, and its outcome is resolved through `TxnStatus`.
#[test]
fn timed_out_write_is_resolved_through_txn_status() {
    let mut h = actor::spawn(store()).unwrap();
    assert_eq!(h.timeout, actor::REQUEST_TIMEOUT);
    h.timeout = std::time::Duration::ZERO;
    let (blobs, mut b) = create("slow", b"slow");
    let id = [9; 16];
    b.client_txn_id = Some(id);
    assert_eq!(h.write(blobs, b), Err(SubmitError::Timeout));
    h.timeout = actor::REQUEST_TIMEOUT;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match h
            .request(|reply| actor::Message::TxnStatus { id, reply })
            .unwrap()
            .unwrap()
        {
            contextdb_store::TxnStatus::Committed { epoch, .. } => {
                assert_eq!(epoch, Epoch(1));
                break;
            }
            contextdb_store::TxnStatus::NotCommitted => {
                assert!(std::time::Instant::now() < deadline, "write never landed");
                std::thread::yield_now();
            }
        }
    }
}
/// Review finding 8: exact impacts are rate-limited to one per group
/// interval, yet every queued impact and write is answered.
#[test]
fn queued_exact_impacts_are_paced_and_all_answered() {
    let h = actor::spawn(store()).unwrap();
    let (blobs, b) = create("a", b"a");
    h.write(blobs, b).unwrap().unwrap();
    let threads: Vec<_> = (0..6)
        .map(|n| {
            let h = h.clone();
            std::thread::spawn(move || {
                if n % 2 == 0 {
                    let (_, batch) = create(&format!("imp{n}"), b"i");
                    h.request(|reply| actor::Message::ImpactExact { batch, reply })
                        .unwrap()
                        .is_ok()
                } else {
                    let (blobs, b) = create(&format!("w{n}"), format!("w{n}").as_bytes());
                    h.write(blobs, b).unwrap().is_ok()
                }
            })
        })
        .collect();
    for t in threads {
        assert!(t.join().unwrap());
    }
    assert_eq!(h.snapshot().epoch, Epoch(4), "impacts never commit");
}
