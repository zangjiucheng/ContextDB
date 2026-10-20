//! P2 durability: T18, T18b, T33, T34, T35 and the G4 crash-injection harness
//! (ADR 0006 §9) on the simulated disk.
use contextdb_core::Db;
use contextdb_model::*;
use contextdb_store::*;
use std::path::Path;

const DIR: &str = "/db";
fn open(disk: &SimDisk) -> Result<(Store<SimDisk>, OpenReport), StoreError> {
    Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default())
}
/// Generated batches use synthetic content hashes; store each as a real blob
/// and rewrite the batch to reference blake3(blob).
fn with_blobs(store: &mut Store<SimDisk>, batch: &Batch) -> Batch {
    let mut b = batch.clone();
    for op in &mut b.ops {
        if let Op::CreateLogical { content, .. } | Op::AddRevision { content, .. } = op {
            *content = store.put_blob(content).unwrap();
        }
    }
    b
}
/// Reference: the same batches through a plain in-memory Db.
fn reference(batches: &[Batch]) -> Vec<ContentHash> {
    let mut db = Db::default();
    let mut digests = vec![db.digest()];
    for b in batches {
        if db.commit(b).is_ok() {
            digests.push(db.digest());
        }
    }
    digests
}
fn txn(n: u64) -> [u8; 16] {
    let mut id = [0; 16];
    id[..8].copy_from_slice(&n.to_le_bytes());
    id
}

#[test]
fn reopen_replays_to_identical_state_t18() {
    let disk = SimDisk::new();
    let log = contextdb_testkit::generate_p1(7);
    let (mut store, _) = open(&disk).unwrap();
    let mut applied = Vec::new();
    for b in &log.batches {
        let b = with_blobs(&mut store, b);
        if store.commit(&b).is_ok() {
            applied.push(b);
        }
    }
    let (epoch, digest) = (store.epoch(), store.db().digest());
    assert!(epoch.0 >= 4);
    drop(store);
    let (store, report) = open(&disk).unwrap();
    assert_eq!(report.replayed, epoch.0);
    assert_eq!((store.epoch(), store.db().digest()), (epoch, digest));
    assert_eq!(*reference(&applied).last().unwrap(), digest);
    // Derived state after replay equals the oracle.
    assert_eq!(
        DerivedView::derived(store.db()),
        &contextdb_oracle::jacobi(&store.db().inputs())
    );
}
#[test]
fn idempotent_retry_and_txn_status_t18b() {
    let disk = SimDisk::new();
    let (mut store, _) = open(&disk).unwrap();
    let blob = store.put_blob(b"payload").unwrap();
    let mut b = Batch::new(vec![Op::CreateLogical {
        key: "a".into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: blob,
        adopt_as_base: true,
    }]);
    b.client_txn_id = Some(txn(1));
    let first = store.commit(&b).unwrap();
    // Crash after sync, before the client saw the ack.
    let crashed = disk.crash(TailFate::Garbage, 1);
    drop(store);
    let (mut store, _) = open(&crashed).unwrap();
    assert_eq!(
        store.txn_status(&txn(1)).unwrap(),
        TxnStatus::Committed {
            epoch: first.epoch,
            result: Box::new(first.clone())
        }
    );
    assert_eq!(
        store.commit(&b).unwrap(),
        first,
        "retry returns the original result"
    );
    assert_eq!(store.epoch(), first.epoch, "retry does not apply twice");
    let mut other = b.clone();
    other.ops.clear();
    assert_eq!(
        store.commit(&other),
        Err(StoreError::Tx(TxError::TxnIdReuse { id: txn(1) }))
    );
    assert_eq!(store.txn_status(&txn(2)).unwrap(), TxnStatus::NotCommitted);
    // A rejected batch records nothing; its id stays NotCommitted.
    let mut bad = Batch::new(vec![Op::AssertBase {
        rev: RevisionId(99).into(),
    }]);
    bad.client_txn_id = Some(txn(3));
    assert!(store.commit(&bad).is_err());
    assert_eq!(store.txn_status(&txn(3)).unwrap(), TxnStatus::NotCommitted);
}
#[test]
fn blobs_are_idempotent_verified_and_required_t35() {
    let disk = SimDisk::new();
    let (mut store, _) = open(&disk).unwrap();
    let h = store.put_blob(b"same").unwrap();
    assert_eq!(store.put_blob(b"same").unwrap(), h);
    assert_eq!(store.get_blob(&h), Payload::Available(b"same".to_vec()));
    let missing = [7; 32];
    assert_eq!(
        store.get_blob(&missing),
        Payload::Unavailable(Unavailable::Missing)
    );
    let create = Op::CreateLogical {
        key: "a".into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: missing,
        adopt_as_base: true,
    };
    assert_eq!(
        store.commit(&Batch::new(vec![create])),
        Err(StoreError::Tx(TxError::MissingBlob { content: missing }))
    );
    // A corrupt blob is reported and replaced by the next put.
    let path = Path::new(DIR)
        .join("blobs")
        .join(h.iter().map(|b| format!("{b:02x}")).collect::<String>());
    disk.corrupt(&path, 0, b'X');
    assert_eq!(
        store.get_blob(&h),
        Payload::Unavailable(Unavailable::Corrupt)
    );
    store.put_blob(b"same").unwrap();
    assert_eq!(store.get_blob(&h), Payload::Available(b"same".to_vec()));
}
#[test]
fn lock_poison_and_restart() {
    let disk = SimDisk::new();
    let (mut store, _) = open(&disk).unwrap();
    assert!(matches!(open(&disk), Err(StoreError::Locked)));
    let b = with_blobs(&mut store, &contextdb_testkit::generate(1).batches[0]);
    disk.fail_next("sync");
    assert_eq!(store.commit(&b), Err(StoreError::Poisoned));
    assert_eq!(store.commit(&b), Err(StoreError::Poisoned));
    drop(store);
    // Same disk instance (same process): restart required.
    assert!(matches!(open(&disk), Err(StoreError::RestartRequired)));
    // A fresh process sees only durable state: the unsynced frame is gone.
    let (store, _) = open(&disk.crash(TailFate::ZeroFill, 3)).unwrap();
    assert_eq!(store.epoch(), Epoch(0));
}
fn populated(disk: &SimDisk, n: usize) -> Store<SimDisk> {
    let (mut store, _) = open(disk).unwrap();
    for b in contextdb_testkit::generate_p1(11).batches.iter().take(n) {
        let b = with_blobs(&mut store, b);
        let _ = store.commit(&b);
    }
    store
}
fn segment(disk: &SimDisk) -> std::path::PathBuf {
    let dir = Path::new(DIR).join("log");
    dir.join(disk.list(&dir).unwrap().pop().unwrap())
}
#[test]
fn corrupt_earlier_frame_fails_closed_t33() {
    let disk = SimDisk::new();
    let store = populated(&disk, 8);
    assert!(store.epoch().0 >= 3);
    drop(store);
    // Flip a byte inside the first frame's body.
    disk.corrupt(&segment(&disk), log::HEADER_LEN + 40, 0xAA);
    assert!(matches!(
        open(&disk),
        Err(StoreError::ChainBroken {
            epoch: Epoch(1),
            ..
        })
    ));
}
#[test]
fn corrupt_final_frame_truncates_and_reports_t34() {
    let disk = SimDisk::new();
    let store = populated(&disk, 8);
    let epoch = store.epoch();
    drop(store);
    let seg = segment(&disk);
    let len = disk.peek(&seg).unwrap().len();
    disk.corrupt(&seg, len - 1, 0x55);
    let strict = Store::open(
        disk.clone(),
        Path::new(DIR),
        &OpenOptions {
            strict_tail: true,
            ..OpenOptions::default()
        },
    );
    assert!(matches!(strict, Err(StoreError::TailUnverifiable { .. })));
    let (store, report) = open(&disk).unwrap();
    let tail = report.truncated_tail.unwrap();
    assert_eq!(tail.after_epoch, Epoch(epoch.0 - 1));
    assert_eq!(store.epoch(), Epoch(epoch.0 - 1));
}
#[derive(Clone, Copy, Debug)]
enum Point {
    PreWal,
    PostWalPreSync,
    PostSyncPrePublish,
}
/// One G4 run: commit a prefix, inject at batch `k`, crash, recover, check.
/// Returns whether the injection actually fired, or Err on any invariant violation.
fn g4_run(seed: u64, point: Point, fate: TailFate, broken_sync: bool) -> Result<bool, String> {
    let disk = SimDisk::new();
    if broken_sync {
        disk.disable_syncs();
    }
    let (mut store, _) = open(&disk).map_err(|e| format!("{e:?}"))?;
    let log = contextdb_testkit::generate_p1(seed);
    let k = (seed as usize) % log.batches.len();
    let mut acked: Vec<(Batch, CommitResult)> = Vec::new();
    let mut in_flight = None;
    let mut fired = false;
    for (n, b) in log.batches.iter().enumerate() {
        let mut b = with_blobs(&mut store, b);
        b.client_txn_id = Some(txn(n as u64));
        if n == k {
            match point {
                Point::PreWal => disk.fail_next("append"),
                Point::PostWalPreSync => disk.fail_next("sync"),
                Point::PostSyncPrePublish => {}
            }
            // PostSyncPrePublish: durable, but the ack is lost with the crash.
            let r = store.commit(&b);
            fired = match point {
                Point::PostSyncPrePublish => r.is_ok(),
                _ => r == Err(StoreError::Poisoned),
            };
            in_flight = Some(b.clone());
            break;
        }
        if let Ok(r) = store.commit(&b) {
            acked.push((b, r));
        }
    }
    drop(store);
    let crashed = disk.crash(fate, seed);
    let (mut store, _) = open(&crashed).map_err(|e| format!("open: {e:?}"))?;
    let recovered = store.epoch().0;
    let last_acked = acked.last().map_or(0, |(_, r)| r.epoch.0);
    if recovered < last_acked || recovered > last_acked + 1 {
        return Err(format!("I5(c): recovered {recovered}, acked {last_acked}"));
    }
    // A synced frame is durable and must survive (I5(c) upper bound reached).
    if fired && matches!(point, Point::PostSyncPrePublish) && recovered != last_acked + 1 {
        return Err("synced frame lost".into());
    }
    // A frame that never reached the log cannot appear.
    if fired && matches!(point, Point::PreWal) && recovered != last_acked {
        return Err("unwritten frame recovered".into());
    }
    // No partial transaction: recovered state equals the reference prefix.
    let mut reference = Db::default();
    for (b, _) in &acked {
        reference.commit(b).map_err(|e| format!("{e:?}"))?;
    }
    if recovered == last_acked + 1 {
        reference
            .commit(in_flight.as_ref().unwrap())
            .map_err(|e| format!("{e:?}"))?;
    }
    if store.db().digest() != reference.digest() {
        return Err("recovered state is not a committed prefix".into());
    }
    if DerivedView::derived(store.db()) != &contextdb_oracle::jacobi(&store.db().inputs()) {
        return Err("replayed derived state differs from oracle".into());
    }
    // Every acked id is Committed with its original result.
    for (b, r) in &acked {
        match store.txn_status(&b.client_txn_id.unwrap()) {
            Ok(TxnStatus::Committed { result, .. }) if *result == *r => {}
            other => return Err(format!("acked txn lost: {other:?}")),
        }
    }
    // The in-flight id is Committed or NotCommitted; retrying applies at most once.
    if let Some(b) = in_flight {
        let before = store.epoch();
        let status = store
            .txn_status(&b.client_txn_id.unwrap())
            .map_err(|e| format!("{e:?}"))?;
        let retry = store.commit(&b);
        match status {
            TxnStatus::Committed { result, .. } => {
                if retry != Ok(*result) || store.epoch() != before {
                    return Err("retry of committed txn applied again".into());
                }
            }
            TxnStatus::NotCommitted => {
                if retry.is_ok() && store.epoch().0 != before.0 + 1 {
                    return Err("retry of uncommitted txn not applied exactly once".into());
                }
            }
        }
    }
    Ok(fired)
}
const FATES: [TailFate; 4] = [
    TailFate::Torn,
    TailFate::ZeroFill,
    TailFate::Garbage,
    TailFate::OutOfOrder,
];
/// Runs seeds until `runs` injections actually fired (G4 counts effective runs).
fn g4(point: Point, runs: u64) {
    let (mut fired, mut seed) = (0, 0);
    while fired < runs {
        let fate = FATES[(seed % 4) as usize];
        match g4_run(seed, point, fate, false) {
            Ok(true) => fired += 1,
            Ok(false) => {}
            Err(e) => panic!("{point:?} seed {seed} {fate:?}: {e}"),
        }
        seed += 1;
        assert!(seed < runs * 20, "{point:?}: injections rarely fire");
    }
}
#[test]
fn g4_pre_wal() {
    g4(Point::PreWal, runs());
}
#[test]
fn g4_post_wal_pre_sync() {
    g4(Point::PostWalPreSync, runs());
}
#[test]
fn g4_post_sync_pre_publish() {
    g4(Point::PostSyncPrePublish, runs());
}
/// Gate count via CONTEXTDB_G4_RUNS (G4 requires >= 1000 per point).
fn runs() -> u64 {
    std::env::var("CONTEXTDB_G4_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
}
#[test]
fn g4_harness_detects_missing_fsync() {
    // With syncs disabled, crashes must expose lost acked commits.
    let detected = (0..200)
        .any(|seed| g4_run(seed, Point::PostSyncPrePublish, TailFate::ZeroFill, true).is_err());
    assert!(detected, "G4 harness cannot detect a no-op sync");
}
#[test]
fn snapshot_accelerates_open_and_matches_full_replay() {
    let disk = SimDisk::new();
    let mut store = populated(&disk, 6);
    let snap = store.snapshot().unwrap();
    let mut more = Vec::new();
    for b in contextdb_testkit::generate_p1(12).batches.iter().take(6) {
        let b = with_blobs(&mut store, b);
        if store.commit(&b).is_ok() {
            more.push(b);
        }
    }
    let (epoch, digest) = (store.epoch(), store.db().digest());
    drop(store);
    let (store, report) = open(&disk).unwrap();
    assert_eq!(report.snapshot_epoch, Some(snap));
    assert_eq!(report.replayed, epoch.0 - snap.0);
    assert_eq!((store.epoch(), store.db().digest()), (epoch, digest));
    // A corrupt snapshot is ignored and full replay is used.
    let dir = Path::new(DIR).join("snap");
    let file = dir.join(disk.list(&dir).unwrap().pop().unwrap());
    drop(store);
    disk.corrupt(&file, 70, 0xEE);
    let (store, report) = open(&disk).unwrap();
    assert_eq!(report.snapshot_epoch, None);
    assert_eq!(report.rejected_snapshots.len(), 1);
    assert_eq!(report.replayed, epoch.0);
    assert_eq!(store.db().digest(), digest);
}
#[test]
fn g4_mid_snapshot() {
    for (n, step) in ["write", "sync", "rename", "sync_dir"]
        .into_iter()
        .enumerate()
    {
        for seed in 0..(runs() / 4 + 1) {
            let disk = SimDisk::new();
            let mut store = populated(&disk, 3 + (seed as usize % 6));
            let (epoch, digest) = (store.epoch(), store.db().digest());
            disk.fail_next(step);
            assert!(store.snapshot().is_err(), "{step}");
            // The log is untouched; the store stays usable.
            assert_eq!(store.epoch(), epoch);
            drop(store);
            let crashed = disk.crash(FATES[(seed as usize + n) % 4], seed);
            let (store, report) = open(&crashed).unwrap();
            assert_eq!(
                (store.epoch(), store.db().digest()),
                (epoch, digest),
                "{step} {seed}"
            );
            assert_eq!(report.snapshot_epoch, None);
        }
    }
}
#[test]
fn historical_reads_survive_restart_t07b_t13() {
    let disk = SimDisk::new();
    let (mut store, _) = open(&disk).unwrap();
    let v1 = store.put_blob(b"db: mongodb").unwrap();
    let v2 = store.put_blob(b"db: postgresql").unwrap();
    let create = Op::CreateLogical {
        key: "db".into(),
        kind: RecordKind::Decision,
        scope_lane: "repo".into(),
        content: v1,
        adopt_as_base: true,
    };
    let e1 = store.commit(&Batch::new(vec![create])).unwrap().epoch;
    let add = Op::AddRevision {
        logical: LogicalId(0).into(),
        content: v2,
        adopt_as_base: true,
    };
    store.commit(&Batch::new(vec![add])).unwrap();
    let head = Op::SetHead {
        logical: LogicalId(0).into(),
        expected: RevisionId(0).into(),
        new: RevisionId(1).into(),
    };
    let e3 = store.commit(&Batch::new(vec![head])).unwrap().epoch;
    store.snapshot().unwrap();
    let read = |s: &Store<SimDisk>| {
        let (old, payload) = s.read_historical(RevisionId(0), e3).unwrap();
        let (then, _) = s.read_historical(RevisionId(0), e1).unwrap();
        let (h1, _) = s.read_snapshot(LogicalId(0), e1).unwrap();
        let (h3, _) = s.read_snapshot(LogicalId(0), e3).unwrap();
        (
            old.canonical_bytes(),
            payload,
            then.canonical_bytes(),
            h1,
            h3,
            old.dimensions.currency,
        )
    };
    let before = read(&store);
    assert_eq!(before, read(&store), "repeated reads are identical");
    assert_eq!(before.1, Payload::Available(b"db: mongodb".to_vec()));
    assert_eq!((before.3, before.4), (RevisionId(0), RevisionId(1)));
    assert_eq!(
        before.5,
        Currency::Superseded,
        "T07b: superseded yet readable"
    );
    drop(store);
    // After restart the Db starts at the snapshot; earlier epochs come from replay.
    let (store, report) = open(&disk).unwrap();
    assert_eq!(report.snapshot_epoch, Some(e3));
    assert!(store.db().initial_epoch() == e3);
    assert_eq!(read(&store), before, "T13: identical across restart");
    assert!(
        store.read_historical(RevisionId(1), e1).is_err(),
        "not yet created at e1"
    );
}
/// PROP10 (store): reopening from op_log (with or without a snapshot) equals the
/// incrementally maintained database, and DIFF agrees for every epoch pair.
#[test]
fn prop10_rebuild_from_op_log_equals_incremental() {
    let seeds: u64 = std::env::var("CONTEXTDB_PROP10_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    for seed in 0..seeds {
        let disk = SimDisk::new();
        let (mut store, _) = open(&disk).unwrap();
        let log = contextdb_testkit::generate_p1(seed);
        let snap_at = (seed as usize) % (log.batches.len() + 1);
        for (n, b) in log.batches.iter().enumerate() {
            if n == snap_at && seed % 2 == 0 {
                store.snapshot().unwrap();
            }
            let b = with_blobs(&mut store, b);
            let _ = store.commit(&b);
        }
        let end = store.epoch();
        let digest = store.db().digest();
        let diffs: Vec<_> = (0..=end.0)
            .map(|a| store.diff(Epoch(a), end).unwrap())
            .collect();
        drop(store);
        let (store, _) = open(&disk).unwrap();
        assert_eq!(store.db().digest(), digest, "seed {seed}");
        for (a, d) in diffs.iter().enumerate() {
            assert_eq!(
                &store.diff(Epoch(a as u64), end).unwrap(),
                d,
                "seed {seed} diff {a}"
            );
        }
    }
}
fn open_with(disk: &SimDisk, proj: &std::path::Path) -> (Store<SimDisk>, OpenReport) {
    let options = OpenOptions {
        projections: ProjectionFile::Path(proj.to_path_buf()),
        ..OpenOptions::default()
    };
    Store::open(disk.clone(), Path::new(DIR), &options).unwrap()
}
fn commit_some(store: &mut Store<SimDisk>, seed: u64, n: usize) {
    for b in contextdb_testkit::generate_p1(seed).batches.iter().take(n) {
        let b = with_blobs(store, b);
        let _ = store.commit(&b);
    }
}
/// Reads that must agree whichever source answers them (ADR 0006 §8).
fn every_read(store: &Store<SimDisk>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let end = store.epoch();
    let inputs = store.db().inputs();
    for e in 0..=end.0 {
        let e = Epoch(e);
        for r in 0..inputs.revisions.len() as u32 {
            if let Ok((s, _)) = store.read_historical(RevisionId(r), e) {
                out.push(s.canonical_bytes());
            } else {
                out.push(vec![0xFF]);
            }
        }
        for l in 0..inputs.logicals.len() as u32 {
            match store.read_snapshot(LogicalId(l), e) {
                Ok((r, s)) => out.push([r.0.to_le_bytes().to_vec(), s.canonical_bytes()].concat()),
                Err(_) => out.push(vec![0xFE]),
            }
        }
        out.push(format!("{:?}", store.diff(e, end).unwrap()).into_bytes());
    }
    out
}
#[test]
fn projections_roll_forward_rebuild_and_agree_with_replay() {
    let tmp = std::env::temp_dir().join(format!("contextdb-proj-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let proj = tmp.join("projections.sqlite");
    let _ = std::fs::remove_file(&proj);
    let disk = SimDisk::new();
    let (mut store, report) = open_with(&disk, &proj);
    assert_eq!(report.projection_watermark, Epoch(0));
    commit_some(&mut store, 21, 8);
    let k = store.epoch();
    drop(store);
    // Current projection: watermark equals the tail, nothing to roll forward.
    let (store, report) = open_with(&disk, &proj);
    assert_eq!(report.projection_watermark, k);
    drop(store);
    // The projection file misses later epochs: roll forward at the next open.
    let (mut store, _) = open(&disk).unwrap();
    commit_some(&mut store, 22, 8);
    store.snapshot().unwrap();
    commit_some(&mut store, 23, 4);
    let reference = every_read(&store); // memory projection, rebuilt at open
    drop(store);
    let (store, report) = open_with(&disk, &proj);
    assert_eq!(report.projection_watermark, k, "lagging watermark detected");
    assert!(
        store.db().initial_epoch() > Epoch(0),
        "reads below it use projections"
    );
    assert_eq!(
        every_read(&store),
        reference,
        "projection reads == replay/memory reads"
    );
    drop(store);
    // A projection naming a different log is rebuilt from scratch.
    let other = SimDisk::new();
    let (mut s2, _) = open_with(&other, &proj);
    let blob = s2.put_blob(b"foreign").unwrap();
    s2.commit(&Batch::new(vec![Op::CreateLogical {
        key: "foreign".into(),
        kind: RecordKind::Fact,
        scope_lane: "x".into(),
        content: blob,
        adopt_as_base: true,
    }]))
    .unwrap();
    commit_some(&mut s2, 99, 3);
    drop(s2);
    let (store, report) = open_with(&disk, &proj);
    assert_eq!(
        report.projection_watermark,
        Epoch(0),
        "foreign projection discarded"
    );
    assert_eq!(every_read(&store), reference);
    let _ = std::fs::remove_dir_all(&tmp);
}
