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
    let (epoch, digest) = (store.epoch(), store.db().unwrap().digest());
    assert!(epoch.0 >= 4);
    drop(store);
    let (store, report) = open(&disk).unwrap();
    assert_eq!(report.replayed, epoch.0);
    assert_eq!(
        (store.epoch(), store.db().unwrap().digest()),
        (epoch, digest)
    );
    assert_eq!(*reference(&applied).last().unwrap(), digest);
    // Derived state after replay equals the oracle.
    assert_eq!(
        DerivedView::derived(store.db().unwrap()),
        &contextdb_oracle::jacobi(&store.db().unwrap().inputs())
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
    assert_eq!(
        store.get_blob(&h).unwrap(),
        Payload::Available(b"same".to_vec())
    );
    let missing = [7; 32];
    assert_eq!(
        store.get_blob(&missing).unwrap(),
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
    // A corrupt blob is reported and replaced by the next put (its packed
    // bytes start after the 40-byte record header, ADR 0008 §17).
    let path = Path::new(DIR).join("blobs").join("pack-00000001");
    disk.corrupt(&path, 40, b'X');
    assert_eq!(
        store.get_blob(&h).unwrap(),
        Payload::Unavailable(Unavailable::Corrupt)
    );
    store.put_blob(b"same").unwrap();
    assert_eq!(
        store.get_blob(&h).unwrap(),
        Payload::Available(b"same".to_vec())
    );
    // The replacement record wins after a restart too.
    drop(store);
    let (store, _) = open(&disk).unwrap();
    assert_eq!(
        store.get_blob(&h).unwrap(),
        Payload::Available(b"same".to_vec())
    );
    // Per-file blobs written before packs stay readable and present.
    drop(store);
    let legacy = *blake3::hash(b"legacy").as_bytes();
    let name: String = legacy.iter().map(|b| format!("{b:02x}")).collect();
    let blobs = Path::new(DIR).join("blobs");
    disk.write_new(&blobs.join(&name), b"legacy").unwrap();
    disk.sync(&blobs.join(name)).unwrap();
    disk.sync_dir(&blobs).unwrap();
    let (mut store, _) = open(&disk).unwrap();
    assert_eq!(
        store.get_blob(&legacy).unwrap(),
        Payload::Available(b"legacy".to_vec())
    );
    let create = Op::CreateLogical {
        key: "legacy".into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: legacy,
        adopt_as_base: true,
    };
    store.commit(&Batch::new(vec![create])).unwrap();
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
    // Odd seeds use tiny segments, so rollover, header-only segments and the
    // cross-segment chain are crashed into as well (review finding 10).
    let options = OpenOptions {
        segment_target: if seed % 2 == 1 {
            512
        } else {
            log::SEGMENT_TARGET
        },
        ..OpenOptions::default()
    };
    let reopen = |d: &SimDisk| Store::open(d.clone(), Path::new(DIR), &options);
    let (mut store, _) = reopen(&disk).map_err(|e| format!("{e:?}"))?;
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
                // Durable, but the process dies before publishing or acking.
                Point::PostSyncPrePublish => store.crash_after_next_sync(),
            }
            let r = store.commit(&b);
            fired = r == Err(StoreError::Poisoned);
            in_flight = Some(b.clone());
            if fired {
                // A poisoned store never exposes the non-durable epoch.
                let last = acked.last().map_or(0, |(_, r)| r.epoch.0);
                if store.epoch().0 != last
                    || store.db().is_ok()
                    || store.txn_status(&txn(n as u64)) != Err(StoreError::Poisoned)
                {
                    return Err("poisoned store serves non-durable state".into());
                }
            }
            break;
        }
        if let Ok(r) = store.commit(&b) {
            acked.push((b, r));
        }
    }
    drop(store);
    let crashed = disk.crash(fate, seed);
    let (mut store, _) = reopen(&crashed).map_err(|e| format!("open: {e:?}"))?;
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
    if store.db().unwrap().digest() != reference.digest() {
        return Err("recovered state is not a committed prefix".into());
    }
    if DerivedView::derived(store.db().unwrap())
        != &contextdb_oracle::jacobi(&store.db().unwrap().inputs())
    {
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
    let (epoch, digest) = (store.epoch(), store.db().unwrap().digest());
    drop(store);
    let (store, report) = open(&disk).unwrap();
    assert_eq!(report.snapshot_epoch, Some(snap));
    assert_eq!(report.replayed, epoch.0 - snap.0);
    assert_eq!(
        (store.epoch(), store.db().unwrap().digest()),
        (epoch, digest)
    );
    // A corrupt snapshot is ignored and full replay is used.
    let dir = Path::new(DIR).join("snap");
    let file = dir.join(disk.list(&dir).unwrap().pop().unwrap());
    drop(store);
    disk.corrupt(&file, 70, 0xEE);
    let (store, report) = open(&disk).unwrap();
    assert_eq!(report.snapshot_epoch, None);
    assert_eq!(report.rejected_snapshots.len(), 1);
    assert_eq!(report.replayed, epoch.0);
    assert_eq!(store.db().unwrap().digest(), digest);
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
            let (epoch, digest) = (store.epoch(), store.db().unwrap().digest());
            disk.fail_next(step);
            assert!(store.snapshot().is_err(), "{step}");
            // The log is untouched; the store stays usable.
            assert_eq!(store.epoch(), epoch);
            drop(store);
            let crashed = disk.crash(FATES[(seed as usize + n) % 4], seed);
            let (store, report) = open(&crashed).unwrap();
            assert_eq!(
                (store.epoch(), store.db().unwrap().digest()),
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
    assert!(store.db().unwrap().initial_epoch() == e3);
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
        let digest = store.db().unwrap().digest();
        let diffs: Vec<_> = (0..=end.0)
            .map(|a| store.diff(Epoch(a), end).unwrap())
            .collect();
        drop(store);
        let (store, _) = open(&disk).unwrap();
        assert_eq!(store.db().unwrap().digest(), digest, "seed {seed}");
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
    let inputs = store.db().unwrap().inputs();
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
        store.db().unwrap().initial_epoch() > Epoch(0),
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
#[test]
fn doctor_truncate_before_repairs_broken_chain_t33() {
    let disk = SimDisk::new();
    let (mut store, _) = open(&disk).unwrap();
    let mut committed = Vec::new();
    for (n, b) in contextdb_testkit::generate_p1(31)
        .batches
        .iter()
        .enumerate()
    {
        let mut b = with_blobs(&mut store, b);
        b.client_txn_id = Some(txn(n as u64));
        if let Ok(r) = store.commit(&b) {
            committed.push((txn(n as u64), r.epoch));
        }
    }
    let end = store.epoch();
    assert!(end.0 >= 4);
    let keep = Epoch(2);
    let digest_at_keep = {
        let mut db = Db::default();
        for e in 1..=keep.0 {
            db.commit(&store.read_row(Epoch(e)).unwrap().batch().unwrap())
                .unwrap();
        }
        db.digest()
    };
    drop(store);
    // Corrupt the third frame: an earlier frame, so open fails closed.
    let seg = segment(&disk);
    let bytes = disk.peek(&seg).unwrap();
    let mut pos = log::HEADER_LEN;
    for _ in 0..keep.0 {
        let (_, _, end) = log::parse_frame(&bytes, pos).unwrap();
        pos = end;
    }
    disk.corrupt(&seg, pos + 40, 0x77);
    assert!(matches!(
        open(&disk),
        Err(StoreError::ChainBroken {
            epoch: Epoch(3),
            ..
        })
    ));
    // A plan before the corrupt frame lists the acked data that will be lost.
    let plan = doctor::plan_truncate_before(&disk, Path::new(DIR), Epoch(3)).unwrap();
    assert_eq!(plan.keep_through, keep);
    assert!(
        plan.unreadable_tail,
        "the corrupt frame itself is unreadable"
    );
    // Every readable dropped frame is listed, including those after the
    // corrupt one (review finding 2).
    let after: Vec<_> = committed.iter().filter(|(_, e)| e.0 > 3).collect();
    assert_eq!(
        plan.dropped_epochs,
        after.iter().map(|(_, e)| *e).collect::<Vec<_>>()
    );
    assert_eq!(
        plan.dropped_txn_ids,
        after.iter().map(|(id, _)| *id).collect::<Vec<_>>()
    );
    // A plan past the corruption is refused: earlier frames must verify.
    assert!(doctor::plan_truncate_before(&disk, Path::new(DIR), Epoch(4)).is_err());
    doctor::truncate_before(&disk, Path::new(DIR), &plan).unwrap();
    let (store, _) = open(&disk).unwrap();
    assert_eq!(
        (store.epoch(), store.db().unwrap().digest()),
        (keep, digest_at_keep)
    );
    for (id, epoch) in committed {
        let status = store.txn_status(&id).unwrap();
        assert_eq!(
            matches!(status, TxnStatus::Committed { .. }),
            epoch <= keep,
            "{epoch:?}"
        );
    }
}
/// G4 blob step (SPEC P2 scope): a failure at any put_blob step fail-stops;
/// after a crash the blob is absent or intact, never torn, and no temp file
/// survives open.
#[test]
fn g4_blob_write() {
    for (n, step) in ["append", "sync_weak", "sync_dir_weak", "sync"]
        .into_iter()
        .enumerate()
    {
        for seed in 0..(runs() / 4 + 1) {
            let disk = SimDisk::new();
            drop(populated(&disk, 1 + seed as usize % 4));
            // Reopened, so the put starts a new pack segment (every step fires).
            let (mut store, _) = open(&disk).unwrap();
            let (epoch, digest) = (store.epoch(), store.db().unwrap().digest());
            let payload = format!("blob-{seed}").into_bytes();
            disk.fail_next(step);
            assert_eq!(
                store.put_blob(&payload),
                Err(StoreError::Poisoned),
                "{step}"
            );
            drop(store);
            let crashed = disk.crash(FATES[(seed as usize + n) % 4], seed);
            let (mut store, _) = open(&crashed).unwrap();
            assert_eq!(
                (store.epoch(), store.db().unwrap().digest()),
                (epoch, digest)
            );
            let hash = *blake3::hash(&payload).as_bytes();
            match store.get_blob(&hash).unwrap() {
                Payload::Available(bytes) => assert_eq!(bytes, payload),
                Payload::Unavailable(why) => {
                    assert_eq!(why, Unavailable::Missing, "{step} {seed}")
                }
            }
            let blobs = Path::new(DIR).join("blobs");
            assert!(
                crashed
                    .list(&blobs)
                    .unwrap()
                    .iter()
                    .all(|f| !f.ends_with(".tmp")),
                "{step} {seed}: temp blob left after open"
            );
            assert_eq!(store.put_blob(&payload).unwrap(), hash);
            store
                .commit(&Batch::new(vec![Op::CreateLogical {
                    key: format!("after-{seed}"),
                    kind: RecordKind::Fact,
                    scope_lane: "repo".into(),
                    content: hash,
                    adopt_as_base: true,
                }]))
                .unwrap();
        }
    }
}
/// G4 projection step: a projection write failing mid-transaction after a
/// durable commit leaves the commit acked, reads exact (projections skipped),
/// and the next open rolls the projection forward from its watermark.
#[test]
fn g4_projection_write() {
    let tmp = std::env::temp_dir().join(format!("contextdb-g4proj-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    for seed in 0..runs() {
        let proj = tmp.join(format!("p{seed}.sqlite"));
        let disk = SimDisk::new();
        let (mut store, _) = open_with(&disk, &proj);
        commit_some(&mut store, seed, 1 + seed as usize % 5);
        let mark = store.epoch();
        assert_eq!(store.projection_watermark(), mark);
        store.fail_next_projection_write();
        let mut acked = 0;
        for b in contextdb_testkit::generate_p1(seed + 1000)
            .batches
            .iter()
            .take(4)
        {
            let b = with_blobs(&mut store, b);
            if store.commit(&b).is_ok() {
                acked += 1;
            }
        }
        // The first failed write rolled back; later epochs leave a gap.
        let expected_mark = if acked > 0 { mark } else { store.epoch() };
        assert_eq!(store.projection_watermark(), expected_mark);
        let live = every_read(&store);
        drop(store);
        let crashed = disk.crash(FATES[seed as usize % 4], seed);
        let (reference, _) = open(&crashed).unwrap();
        assert_eq!(every_read(&reference), live, "stale projections skipped");
        drop(reference);
        let (mut store, report) = open_with(&crashed, &proj);
        assert_eq!(report.projection_watermark, expected_mark);
        assert_eq!(every_read(&store), live, "lagging projection reads exact");
        assert_eq!(store.catch_up_projections().unwrap(), store.epoch());
        assert_eq!(every_read(&store), live, "caught-up projection reads exact");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

// Regression tests for the independent P2 review (docs/gates/P2-report.md).

fn small_segments() -> OpenOptions {
    OpenOptions {
        segment_target: 400,
        ..OpenOptions::default()
    }
}
/// Commits generated batches with client ids; returns (id, epoch) per ack.
fn commit_ids(store: &mut Store<SimDisk>, seed: u64) -> Vec<([u8; 16], Epoch)> {
    let mut out = Vec::new();
    for (n, b) in contextdb_testkit::generate_p1(seed)
        .batches
        .iter()
        .enumerate()
    {
        let mut b = with_blobs(store, b);
        b.client_txn_id = Some(txn(n as u64));
        if let Ok(r) = store.commit(&b) {
            out.push((txn(n as u64), r.epoch));
        }
    }
    out
}
#[test]
fn review1_corrupt_final_segment_header_with_frames_fails_closed() {
    let disk = SimDisk::new();
    let store = populated(&disk, 5);
    assert!(store.epoch().0 >= 3);
    drop(store);
    disk.corrupt(&segment(&disk), 0, 0x00);
    for strict_tail in [false, true] {
        let r = Store::open(
            disk.clone(),
            Path::new(DIR),
            &OpenOptions {
                strict_tail,
                ..OpenOptions::default()
            },
        );
        assert!(
            matches!(
                r,
                Err(StoreError::ChainBroken {
                    epoch: Epoch(1),
                    fault: log::FrameFault::SegmentHeader
                })
            ),
            "strict_tail={strict_tail}: {:?}",
            r.err()
        );
    }
    assert!(disk.exists(&segment(&disk)), "evidence is kept");
}
#[test]
fn review1_header_only_tail_segment_is_removed_unless_strict() {
    let disk = SimDisk::new();
    let (mut store, _) = Store::open(disk.clone(), Path::new(DIR), &small_segments()).unwrap();
    commit_ids(&mut store, 41);
    let epoch = store.epoch();
    drop(store);
    // An interrupted segment creation: a partial header and nothing else.
    let dir = Path::new(DIR).join("log");
    let partial = dir.join(log::segment_name(Epoch(epoch.0 + 1)));
    disk.write_new(
        &partial,
        &log::segment_header(Epoch(epoch.0 + 1), [0; 32])[..20],
    )
    .unwrap();
    disk.sync(&partial).unwrap();
    disk.sync_dir(&dir).unwrap();
    let strict = Store::open(
        disk.clone(),
        Path::new(DIR),
        &OpenOptions {
            strict_tail: true,
            ..small_segments()
        },
    );
    assert!(matches!(strict, Err(StoreError::TailUnverifiable { .. })));
    let (store, report) = Store::open(disk.clone(), Path::new(DIR), &small_segments()).unwrap();
    assert_eq!(report.removed_segment, Some(partial));
    assert_eq!(store.epoch(), epoch);
}
#[test]
fn review2_truncation_plan_lists_every_dropped_frame_across_segments() {
    let disk = SimDisk::new();
    let (mut store, _) = Store::open(disk.clone(), Path::new(DIR), &small_segments()).unwrap();
    let committed = commit_ids(&mut store, 42);
    let end = store.epoch();
    drop(store);
    let dir = Path::new(DIR).join("log");
    assert!(
        log::segments(&disk, &dir).unwrap().len() >= 3,
        "several segments"
    );
    for cut in 2..=end.0 {
        let plan = doctor::plan_truncate_before(&disk, Path::new(DIR), Epoch(cut)).unwrap();
        let lost: Vec<_> = committed.iter().filter(|(_, e)| e.0 >= cut).collect();
        assert_eq!(
            plan.dropped_epochs,
            lost.iter().map(|(_, e)| *e).collect::<Vec<_>>(),
            "cut {cut}"
        );
        assert_eq!(plan.dropped_txn_ids.len(), lost.len());
        assert!(!plan.unreadable_tail);
    }
    // Executing a plan keeps exactly the prefix, whatever segment it ends in.
    let cut = end.0 / 2 + 1;
    let plan = doctor::plan_truncate_before(&disk, Path::new(DIR), Epoch(cut)).unwrap();
    doctor::truncate_before(&disk, Path::new(DIR), &plan).unwrap();
    let (store, _) = Store::open(disk.clone(), Path::new(DIR), &small_segments()).unwrap();
    assert_eq!(store.epoch(), Epoch(cut - 1));
    for (id, e) in &committed {
        let kept = matches!(store.txn_status(id).unwrap(), TxnStatus::Committed { .. });
        assert_eq!(kept, e.0 < cut);
    }
}
#[test]
fn review3_interrupted_migration_is_refused_then_resumed() {
    let disk = SimDisk::new();
    let batches = contextdb_testkit::generate_p1(43).batches;
    contextdb_store::migrate::write_legacy(&disk, Path::new(DIR), &batches).unwrap();
    let mut expected = Db::default();
    for b in &batches {
        let _ = expected.commit(b);
    }
    // Reproduce a crash between the two renames of `migrate`.
    let probe = disk.crash(TailFate::Torn, 0);
    migrate::migrate(&probe, Path::new(DIR)).unwrap();
    let (root, log_dir) = (Path::new(DIR), Path::new(DIR).join("log"));
    probe
        .rename_dir(&log_dir, &root.join("log.migrating"))
        .unwrap();
    probe.sync_dir(root).unwrap();
    assert!(matches!(open(&probe), Err(StoreError::MigrationIncomplete)));
    let report = migrate::migrate(&probe, root).unwrap();
    assert_eq!(report.to, log::FORMAT_VERSION);
    let (store, _) = open(&probe).unwrap();
    assert_eq!(store.db().unwrap().digest(), expected.digest());
}
#[test]
fn review4_offline_tools_respect_the_store_lock() {
    let disk = SimDisk::new();
    let store = populated(&disk, 6);
    let plan = doctor::plan_truncate_before(&disk, Path::new(DIR), Epoch(2)).unwrap();
    assert_eq!(
        doctor::truncate_before(&disk, Path::new(DIR), &plan),
        Err(StoreError::Locked)
    );
    assert_eq!(
        migrate::migrate(&disk, Path::new(DIR)),
        Err(StoreError::Locked)
    );
    drop(store);
    // A plan made before further commits is refused once the log changed.
    let (mut store, _) = open(&disk).unwrap();
    commit_some(&mut store, 44, 3);
    drop(store);
    assert_eq!(
        doctor::truncate_before(&disk, Path::new(DIR), &plan),
        Err(StoreError::PlanOutdated)
    );
}
#[test]
fn review5_poisoned_store_serves_nothing() {
    let disk = SimDisk::new();
    let mut store = populated(&disk, 4);
    let epoch = store.epoch();
    let content = store.put_blob(b"ghost").unwrap();
    let mut b = Batch::new(vec![Op::CreateLogical {
        key: "ghost".into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content,
        adopt_as_base: true,
    }]);
    b.client_txn_id = Some(txn(99));
    disk.fail_next("sync");
    assert_eq!(store.commit(&b), Err(StoreError::Poisoned));
    assert_eq!(store.epoch(), epoch, "durable epoch only");
    assert_eq!(store.db().err(), Some(StoreError::Poisoned));
    assert_eq!(store.txn_status(&txn(99)), Err(StoreError::Poisoned));
    assert_eq!(store.read_row(Epoch(1)).err(), Some(StoreError::Poisoned));
    assert_eq!(
        store.diff(Epoch(0), epoch).err(),
        Some(StoreError::Poisoned)
    );
    assert_eq!(
        store.read_snapshot(LogicalId(0), epoch).err(),
        Some(StoreError::Poisoned)
    );
    assert_eq!(store.get_blob(&content), Err(StoreError::Poisoned));
}
#[test]
fn review7_valid_checksum_with_wrong_chain_fails_closed_even_at_the_tail() {
    let disk = SimDisk::new();
    let store = populated(&disk, 4);
    let epoch = store.epoch();
    let mut row = store.read_row(epoch).unwrap();
    drop(store);
    row.epoch = Epoch(epoch.0 + 1);
    row.prev_checksum = [9; 32]; // not the tail's checksum
    let (bytes, _) = log::frame(&row);
    let seg = segment(&disk);
    disk.append(&seg, &bytes).unwrap();
    disk.sync(&seg).unwrap();
    assert!(matches!(
        open(&disk),
        Err(StoreError::ChainBroken {
            fault: log::FrameFault::Chain,
            ..
        })
    ));
}
#[test]
fn review8_unbuildable_snapshot_is_ignored() {
    let disk = SimDisk::new();
    let store = populated(&disk, 6);
    let (epoch, digest) = (store.epoch(), store.db().unwrap().digest());
    let mut kernel = store.kernel().unwrap().export();
    drop(store);
    let state = &mut kernel.states[0];
    let inputs = &mut state.inputs;
    // A cycle: verifies as a snapshot file, but the core cannot rebuild it.
    let n = inputs.revisions.len() as u32;
    assert!(n >= 2);
    for (c, p) in [(0, 1), (1, 0)] {
        inputs.rules.push(Rule {
            conclusion: RevisionId(c),
            premises: vec![Incidence {
                premise: RevisionId(p),
                kind: EdgeKind::Pin,
            }],
            references: Vec::new(),
            enabled: true,
            origin_branch: MAIN,
            origin_epoch: epoch,
        });
        let g = state.to_global[2].len() as u32;
        state.to_global[2].push(g);
    }
    let scan = log::scan(&disk, &Path::new(DIR).join("log")).unwrap();
    let checksum = scan.frames.last().unwrap().checksum;
    contextdb_store::snapshot::write(&disk, Path::new(DIR), epoch, &checksum, &kernel).unwrap();
    let (store, report) = open(&disk).unwrap();
    assert_eq!(report.snapshot_epoch, None);
    assert_eq!(report.rejected_snapshots.len(), 1);
    assert_eq!(
        (store.epoch(), store.db().unwrap().digest()),
        (epoch, digest)
    );
}
#[test]
fn review9_poison_is_per_store_directory() {
    let disk = SimDisk::new();
    let (mut a, _) = Store::open(disk.clone(), Path::new("/a"), &OpenOptions::default()).unwrap();
    let b = with_blobs(&mut a, &contextdb_testkit::generate_p1(46).batches[0]);
    disk.fail_next("sync");
    assert_eq!(a.commit(&b), Err(StoreError::Poisoned));
    drop(a);
    assert!(matches!(
        Store::open(disk.clone(), Path::new("/a"), &OpenOptions::default()),
        Err(StoreError::RestartRequired)
    ));
    assert!(Store::open(disk.clone(), Path::new("/b"), &OpenOptions::default()).is_ok());
}
#[test]
fn review12_operator_open_does_not_create_a_store() {
    let disk = SimDisk::new();
    let options = OpenOptions {
        create: false,
        ..OpenOptions::default()
    };
    assert!(matches!(
        Store::open(disk.clone(), Path::new("/typo"), &options),
        Err(StoreError::NotAStore)
    ));
    assert!(!disk.exists(Path::new("/typo/log")));
}
#[test]
fn review_n6_failed_catch_up_rolls_back_to_the_old_watermark() {
    let tmp = std::env::temp_dir().join(format!("contextdb-catchup-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let proj = tmp.join("p.sqlite");
    let _ = std::fs::remove_file(&proj);
    let disk = SimDisk::new();
    let (mut store, _) = open_with(&disk, &proj);
    commit_some(&mut store, 51, 4);
    let mark = store.epoch();
    drop(store);
    // Later epochs land after a snapshot, so open leaves projections behind.
    let (mut store, _) = open(&disk).unwrap();
    commit_some(&mut store, 52, 6);
    store.snapshot().unwrap();
    drop(store);
    let (mut store, report) = open_with(&disk, &proj);
    assert_eq!(report.projection_watermark, mark);
    let reference = every_read(&store);
    store.fail_next_projection_write();
    assert!(store.catch_up_projections().is_err());
    assert_eq!(store.projection_watermark(), mark, "rolled back");
    assert_eq!(every_read(&store), reference);
    assert_eq!(store.catch_up_projections().unwrap(), store.epoch());
    assert_eq!(every_read(&store), reference);
    drop(store);
    let (store, report) = open_with(&disk, &proj);
    assert_eq!(report.projection_watermark, store.epoch(), "committed");
    let _ = std::fs::remove_dir_all(&tmp);
}

// ADR 0008 §3.2: group commit and the shared tail algorithm.

/// Commits `batches` as one group with fresh client ids; returns the results.
fn group(
    store: &mut Store<SimDisk>,
    batches: &[Batch],
    first_id: u64,
) -> Vec<Result<CommitResult, StoreError>> {
    let staged: Vec<Batch> = batches
        .iter()
        .enumerate()
        .map(|(n, b)| {
            let mut b = with_blobs(store, b);
            b.client_txn_id = Some(txn(first_id + n as u64));
            b
        })
        .collect();
    store.commit_group(&staged)
}
/// A store whose last group has `n` accepted frames; returns (disk, epoch before it).
fn last_group_of(n: usize) -> (SimDisk, Epoch) {
    let disk = SimDisk::new();
    let (mut store, _) = open(&disk).unwrap();
    let base = |key: String, c: u8| contextdb_testkit::create(&key, RecordKind::Fact, true, c);
    group(&mut store, &[Batch::new(vec![base("a".into(), 1)])], 0);
    let before = store.epoch();
    let batches: Vec<Batch> = (0..n)
        .map(|k| Batch::new(vec![base(format!("g{k}"), 10 + k as u8)]))
        .collect();
    assert!(group(&mut store, &batches, 100).iter().all(|r| r.is_ok()));
    assert_eq!(store.epoch().0, before.0 + n as u64);
    (disk, before)
}
fn frames(disk: &SimDisk) -> Vec<log::Located> {
    log::scan(disk, &Path::new(DIR).join("log")).unwrap().frames
}
#[test]
fn group_commit_held_replies_idempotency_and_one_sync() {
    let disk = SimDisk::new();
    let (mut store, _) = open(&disk).unwrap();
    let a = contextdb_testkit::create("a", RecordKind::Fact, true, 1);
    let b = contextdb_testkit::create("b", RecordKind::Fact, true, 2);
    let mut x = with_blobs(&mut store, &Batch::new(vec![a.clone()]));
    x.client_txn_id = Some(txn(1));
    let mut dup_other = with_blobs(&mut store, &Batch::new(vec![b]));
    dup_other.client_txn_id = Some(txn(1));
    let rejected = Batch::new(vec![a]); // key "a" exists once x is staged
    let rejected = with_blobs(&mut store, &rejected);
    let results = store.commit_group(&[x.clone(), x.clone(), dup_other, rejected]);
    let first = results[0].clone().unwrap();
    assert_eq!(
        results[1],
        Ok(first.clone()),
        "staged duplicate shares the result"
    );
    assert_eq!(
        results[2],
        Err(StoreError::Tx(TxError::TxnIdReuse { id: txn(1) }))
    );
    assert!(matches!(
        results[3],
        Err(StoreError::Tx(TxError::LogicalExists { .. }))
    ));
    assert_eq!(store.epoch(), Epoch(1), "one applied batch");
    // Every frame of a group carries {seq, idx, count}.
    let f = frames(&disk);
    assert_eq!(f.len(), 1);
    assert_eq!((f[0].row.group.seq, f[0].row.group.count), (1, 1));
    drop(store);
    let (_, report) = open(&disk).unwrap();
    assert_eq!(report.replayed, 1);
}
#[test]
fn group_tail_short_final_group_is_truncated() {
    let (disk, before) = last_group_of(3);
    let f = frames(&disk);
    let seg = f[0].segment.clone();
    disk.truncate(&seg, f[f.len() - 1].start).unwrap(); // 2 of 3 frames remain
    disk.sync(&seg).unwrap();
    let (store, report) = open(&disk).unwrap();
    assert_eq!(store.epoch(), before, "the whole short group is dropped");
    assert_eq!(
        report.truncated_tail.unwrap().reason,
        log::FrameFault::Incomplete
    );
}
#[test]
fn group_tail_torn_frame_inside_last_group_truncates_the_group() {
    let (disk, before) = last_group_of(3);
    let f = frames(&disk);
    // Damage the middle frame: the frame after it still has a valid checksum.
    disk.corrupt(&f[2].segment, f[2].start as usize + 30, 0x5A);
    let (store, report) = open(&disk).unwrap();
    assert_eq!(store.epoch(), before);
    assert!(report.truncated_tail.is_some());
}
#[test]
fn group_tail_damage_followed_by_a_later_group_fails_closed() {
    let (disk, _) = last_group_of(1);
    let (mut store, _) = open(&disk).unwrap();
    let more = contextdb_testkit::create("later", RecordKind::Fact, true, 99);
    group(&mut store, &[Batch::new(vec![more])], 200);
    drop(store);
    let f = frames(&disk);
    disk.corrupt(&f[1].segment, f[1].start as usize + 30, 0x5A);
    assert!(matches!(open(&disk), Err(StoreError::ChainBroken { .. })));
}
#[test]
fn group_tail_semantic_fault_at_a_boundary_fails_closed() {
    let (disk, _) = last_group_of(2);
    let f = frames(&disk);
    let last = f.last().unwrap();
    // Re-frame the last group's first frame with a skipped group seq: checksum
    // valid, chain valid, but the group sequence jumps (a writer bug).
    let first = &f[f.len() - 2];
    let mut row = first.row.clone();
    row.group.seq += 1;
    let (bytes, _) = log::frame(&row);
    disk.truncate(&first.segment, first.start).unwrap();
    disk.append(&first.segment, &bytes).unwrap();
    disk.sync(&first.segment).unwrap();
    let _ = last;
    assert!(matches!(
        open(&disk),
        Err(StoreError::ChainBroken {
            fault: log::FrameFault::Group,
            ..
        })
    ));
}
#[test]
fn group_tail_junk_after_a_complete_group_is_truncated() {
    let (disk, _) = last_group_of(2);
    let epoch = {
        let (s, _) = open(&disk).unwrap();
        s.epoch()
    };
    let seg = frames(&disk)[0].segment.clone();
    disk.append(&seg, &[0xAB; 37]).unwrap();
    disk.sync(&seg).unwrap();
    let (store, report) = open(&disk).unwrap();
    assert_eq!(store.epoch(), epoch, "complete groups are kept");
    assert_eq!(report.truncated_tail.unwrap().bytes, 37);
}
/// G4 for groups: inject at append, sync or post-sync on a group of 2–6
/// batches. Recovery keeps all of the group or none of it, the in-flight ids
/// agree, and retries apply at most once.
#[test]
fn g4_group_commit() {
    let (mut fired_total, mut seed) = (0, 0u64);
    while fired_total < runs() {
        seed += 1;
        let disk = SimDisk::new();
        let options = OpenOptions {
            segment_target: if seed % 2 == 1 {
                700
            } else {
                log::SEGMENT_TARGET
            },
            ..OpenOptions::default()
        };
        let reopen = |d: &SimDisk| Store::open(d.clone(), Path::new(DIR), &options).unwrap();
        let (mut store, _) = reopen(&disk);
        let log = contextdb_testkit::generate_p1(seed);
        let mut acked: Vec<(Batch, CommitResult)> = Vec::new();
        let mut batches = log.batches.iter().cloned().enumerate().peekable();
        let mut id = 0u64;
        // A prefix of 0–3 groups of 1–3 batches.
        for g in 0..seed % 4 {
            if batches.peek().is_none() {
                break;
            }
            let n = 1 + (seed + g) as usize % 3;
            let chunk: Vec<Batch> = batches.by_ref().take(n).map(|(_, b)| b).collect();
            let staged: Vec<Batch> = chunk
                .iter()
                .map(|b| {
                    let mut b = with_blobs(&mut store, b);
                    id += 1;
                    b.client_txn_id = Some(txn(id));
                    b
                })
                .collect();
            for (b, r) in staged.iter().zip(store.commit_group(&staged)) {
                if let Ok(r) = r {
                    acked.push((b.clone(), r));
                }
            }
        }
        let chunk: Vec<Batch> = batches
            .by_ref()
            .take(2 + seed as usize % 5)
            .map(|(_, b)| b)
            .collect();
        if chunk.len() < 2 {
            continue;
        }
        let staged: Vec<Batch> = chunk
            .iter()
            .map(|b| {
                let mut b = with_blobs(&mut store, b);
                id += 1;
                b.client_txn_id = Some(txn(id));
                b
            })
            .collect();
        let point = seed % 3;
        match point {
            0 => disk.fail_next("append"),
            1 => disk.fail_next("sync"),
            _ => store.crash_after_next_sync(),
        }
        let before = store.epoch();
        let results = store.commit_group(&staged);
        let accepted_any = results.iter().any(|r| r != &Err(StoreError::Poisoned));
        if accepted_any {
            continue; // every batch was rejected: nothing was written, nothing injected
        }
        fired_total += 1;
        drop(store);
        let crashed = disk.crash(FATES[seed as usize % 4], seed);
        let (mut store, _) = reopen(&crashed);
        // Reference: the acked prefix, plus the in-flight group if recovered.
        let mut reference = Db::default();
        for (b, _) in &acked {
            reference.commit(b).unwrap();
        }
        let mut with_group = reference.clone();
        let accepted = staged
            .iter()
            .filter(|b| with_group.commit(b).is_ok())
            .count() as u64;
        let recovered = store.epoch();
        let all = recovered.0 == before.0 + accepted;
        assert!(
            recovered == before || all,
            "seed {seed}: partial group recovered"
        );
        if point == 2 {
            assert!(all, "seed {seed}: a synced group was lost");
        }
        if point == 0 {
            assert_eq!(recovered, before, "seed {seed}: unwritten group recovered");
        }
        let expect = if all { &with_group } else { &reference };
        assert_eq!(store.db().unwrap().digest(), expect.digest(), "seed {seed}");
        for b in &staged {
            let status = store.txn_status(&b.client_txn_id.unwrap()).unwrap();
            if !all {
                assert_eq!(status, TxnStatus::NotCommitted);
            }
        }
        // Retrying the whole group applies each committed batch at most once:
        // committed ids return their original result; only batches that were
        // never committed (rejected or lost) may apply now (§6.1 contract 1).
        let status: Vec<TxnStatus> = staged
            .iter()
            .map(|b| store.txn_status(&b.client_txn_id.unwrap()).unwrap())
            .collect();
        let epoch = store.epoch();
        let retried = store.commit_group(&staged);
        let mut fresh = 0;
        for (st, r) in status.iter().zip(&retried) {
            match st {
                TxnStatus::Committed { result, .. } => {
                    assert_eq!(
                        r,
                        &Ok((**result).clone()),
                        "seed {seed}: committed retry changed"
                    )
                }
                TxnStatus::NotCommitted => fresh += u64::from(r.is_ok()),
            }
        }
        assert_eq!(
            store.epoch().0,
            epoch.0 + fresh,
            "seed {seed}: retry re-applied"
        );
    }
}
#[test]
fn log_reader_is_bounded_by_its_tail_and_replays_exactly() {
    let disk = SimDisk::new();
    let mut store = populated(&disk, 6);
    let reader = store.log_reader();
    let (tail, digest) = (reader.tail(), store.db().unwrap().digest());
    assert!(tail.0 >= 3);
    commit_some(&mut store, 61, 3);
    assert!(store.epoch() > tail, "later commits");
    assert_eq!(reader.replay_to(tail).unwrap().digest(), digest);
    assert!(
        reader.read_row(Epoch(tail.0 + 1)).is_err(),
        "bounded by tail"
    );
    for e in 1..=tail.0 {
        assert_eq!(
            reader.read_row(Epoch(e)).unwrap(),
            store.read_row(Epoch(e)).unwrap()
        );
    }
    // Readers work from other threads.
    let r2 = reader.clone();
    let d = std::thread::spawn(move || r2.replay_to(tail).unwrap().digest())
        .join()
        .unwrap();
    assert_eq!(d, digest);
}
#[test]
fn watermark_history_projection_records_net_advances() {
    let tmp = std::env::temp_dir().join(format!("contextdb-wm-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let proj = tmp.join("p.sqlite");
    let _ = std::fs::remove_file(&proj);
    let disk = SimDisk::new();
    let (mut store, _) = open_with(&disk, &proj);
    let watermark = |expected: Option<u8>, position: u8| {
        let mut b = Batch::new(vec![Op::ObserverWatermark {
            scope: "repo".into(),
            expected: expected.map(|v| vec![v]),
            position: vec![position],
        }]);
        b.channel = Channel::Observer;
        b
    };
    store.commit(&watermark(None, 1)).unwrap(); // epoch 1: set
    let content = store.put_blob(b"x").unwrap();
    store
        .commit(&Batch::new(vec![Op::CreateLogical {
            key: "x".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content,
            adopt_as_base: true,
        }]))
        .unwrap(); // epoch 2: unrelated
    store.commit(&watermark(Some(1), 1)).unwrap(); // epoch 3: same position, no advance
    store.commit(&watermark(Some(1), 2)).unwrap(); // epoch 4: advance
    let adv = |a, b| {
        store
            .watermark_advanced("repo", Epoch(a), Epoch(b))
            .unwrap()
    };
    assert_eq!(adv(0, 1), Some(true));
    assert_eq!(adv(1, 3), Some(false));
    assert_eq!(adv(1, 4), Some(true));
    assert_eq!(adv(4, 4), Some(false));
    let _ = std::fs::remove_dir_all(&tmp);
}
#[test]
fn doctor_cut_inside_a_group_moves_to_the_group_start() {
    let (disk, before) = last_group_of(3);
    let middle = Epoch(before.0 + 2);
    let plan = doctor::plan_truncate_before(&disk, Path::new(DIR), middle).unwrap();
    assert_eq!(plan.keep_through, before, "the whole group is dropped");
    assert_eq!(
        plan.dropped_epochs,
        vec![Epoch(before.0 + 1), middle, Epoch(before.0 + 3)]
    );
    doctor::truncate_before(&disk, Path::new(DIR), &plan).unwrap();
    let (store, report) = open(&disk).unwrap();
    assert_eq!(store.epoch(), before);
    assert!(
        report.truncated_tail.is_none(),
        "no partial group left behind"
    );
}
/// ADR 0008 §17: ordered state reaches media at the next full sync; on a
/// crash before one it either survived, or it did not and neither did
/// anything written after it.
#[test]
fn weak_sync_is_ordered_and_durable_after_a_full_sync() {
    let disk = SimDisk::new();
    disk.create_dir_all(Path::new("/d")).unwrap();
    disk.write_new(Path::new("/d/a"), b"weak").unwrap();
    disk.sync_weak(Path::new("/d/a")).unwrap();
    disk.write_new(Path::new("/d/b"), b"x").unwrap();
    disk.sync_dir(Path::new("/d")).unwrap();
    // sync_dir was the barrier for a; b was never synced, so its byte is
    // only a zero-filled unsynced tail.
    let after = disk.crash(TailFate::ZeroFill, 0);
    assert_eq!(after.read(Path::new("/d/a")).unwrap(), b"weak");
    assert_eq!(after.read(Path::new("/d/b")).unwrap(), b"\0");
    disk.append(Path::new("/d/a"), b"-more").unwrap();
    disk.sync_weak(Path::new("/d/a")).unwrap();
    disk.append(Path::new("/d/b"), b"y").unwrap();
    let mut outcomes = std::collections::BTreeSet::new();
    for seed in 0..16 {
        let c = disk.crash(TailFate::Torn, seed);
        let (a, b) = (
            c.read(Path::new("/d/a")).unwrap(),
            c.read(Path::new("/d/b")).unwrap(),
        );
        // b's later bytes never survive without a's ordered ones.
        assert!(a == b"weak-more" || b.is_empty(), "{a:?} {b:?}");
        outcomes.insert(a == b"weak-more");
    }
    assert_eq!(outcomes.len(), 2, "both outcomes are modelled");
    disk.sync(Path::new("/d/b")).unwrap();
    for seed in 0..4 {
        let c = disk.crash(TailFate::ZeroFill, seed);
        assert_eq!(c.read(Path::new("/d/a")).unwrap(), b"weak-more");
    }
}
/// The daemon path: staged payloads, then the group's frame. Whatever the
/// crash point and fate, a frame that survives finds its payloads.
#[test]
fn g4_staged_blobs_never_trail_their_frame() {
    for (n, step) in [
        None,
        Some("sync_weak"),
        Some("sync_dir_weak"),
        Some("append"),
        Some("sync"),
    ]
    .into_iter()
    .enumerate()
    {
        for seed in 0..(runs() / 4 + 1) {
            let disk = SimDisk::new();
            drop(populated(&disk, 1 + seed as usize % 4));
            // Reopened, so the group starts a new pack segment and every
            // step (including the ordered dir sync) fires (review finding 7).
            let (mut store, _) = open(&disk).unwrap();
            let before = store.epoch();
            let payloads: Vec<Vec<u8>> = (0..3)
                .map(|i| format!("staged-{seed}-{i}").into_bytes())
                .collect();
            let batches: Vec<Batch> = payloads
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    Batch::new(vec![Op::CreateLogical {
                        key: format!("staged-{seed}-{i}"),
                        kind: RecordKind::Fact,
                        scope_lane: "repo".into(),
                        content: *blake3::hash(p).as_bytes(),
                        adopt_as_base: true,
                    }])
                })
                .collect();
            if let Some(step) = step {
                disk.fail_next(step);
            }
            let refs: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
            if store.stage_blobs(&refs).is_ok() {
                let _ = store.commit_group(&batches);
            }
            drop(store);
            let crashed = disk.crash(FATES[(seed as usize + n) % 4], seed);
            let (store, _) = open(&crashed).unwrap();
            let landed = store.epoch().0 - before.0;
            assert!(landed == 0 || landed == 3, "{step:?} {seed}: {landed}");
            if landed > 0 {
                for p in &payloads {
                    let h = *blake3::hash(p).as_bytes();
                    assert_eq!(
                        store.get_blob(&h).unwrap(),
                        Payload::Available(p.clone()),
                        "{step:?} {seed}: frame survived without its payload"
                    );
                }
            }
        }
    }
}
/// Without the barrier the same harness finds frames whose payloads are gone.
/// A torn crash keeps a whole unsynced frame about once per frame length,
/// so the seed range covers several such crashes.
#[test]
fn g4_staged_harness_detects_a_missing_barrier() {
    let detected = (0..2000).any(|seed| {
        let disk = SimDisk::new();
        let store = populated(&disk, 1);
        let p = format!("unordered-{seed}").into_bytes();
        let h = *blake3::hash(&p).as_bytes();
        // Write the payload with no sync at all, then commit a frame using it.
        let blobs = Path::new(DIR).join("blobs");
        let name: String = h.iter().map(|b| format!("{b:02x}")).collect();
        disk.write_new(&blobs.join(name), &p).unwrap();
        disk.sync_dir(&blobs).unwrap();
        // Reopen so the store registers the unsynced payload.
        drop(store);
        let (mut store, _) = open(&disk).unwrap();
        let b = Batch::new(vec![Op::CreateLogical {
            key: format!("u{seed}"),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: h,
            adopt_as_base: true,
        }]);
        let before = store.epoch();
        disk.fail_next("sync");
        let _ = store.commit(&b);
        drop(store);
        let crashed = disk.crash(TailFate::Torn, seed);
        let (store, _) = open(&crashed).unwrap();

        store.epoch() > before && store.get_blob(&h).unwrap() != Payload::Available(p)
    });
    assert!(detected, "harness cannot see a frame outrun its payload");
}
/// Batched payloads: durable once `put_blobs` returns, with one full sync;
/// a failed barrier poisons the store and nothing it wrote is relied on.
#[test]
fn put_blobs_is_durable_on_return_and_poisons_on_failed_barrier() {
    let disk = SimDisk::new();
    let (mut store, _) = open(&disk).unwrap();
    store.put_blobs(&[b"one", b"two", b"one"]).unwrap();
    let crashed = disk.crash(TailFate::Garbage, 3);
    drop(store);
    let (store, _) = open(&crashed).unwrap();
    for b in [&b"one"[..], b"two"] {
        let h = *blake3::hash(b).as_bytes();
        assert_eq!(store.get_blob(&h).unwrap(), Payload::Available(b.to_vec()));
    }
    drop(store);
    let disk = SimDisk::new();
    let (mut store, _) = open(&disk).unwrap();
    disk.fail_next("sync");
    assert!(store.put_blobs(&[b"three"]).is_err());
    assert!(matches!(store.put_blob(b"four"), Err(StoreError::Poisoned)));
    let crashed = disk.crash(TailFate::Garbage, 4);
    drop(store);
    let (store, _) = open(&crashed).unwrap();
    let h = *blake3::hash(b"three").as_bytes();
    assert_eq!(
        store.get_blob(&h).unwrap(),
        Payload::Unavailable(Unavailable::Missing)
    );
}

fn pack_record(bytes: &[u8]) -> Vec<u8> {
    let mut r = b"CXBP".to_vec();
    r.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    r.extend_from_slice(blake3::hash(bytes).as_bytes());
    r.extend_from_slice(bytes);
    r
}
fn create_with(key: &str, content: ContentHash) -> Batch {
    Batch::new(vec![Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content,
        adopt_as_base: true,
    }])
}
/// P3 review finding 1: a killed process leaves a verifying pack record only
/// in the page cache. After a restart (no power loss) a retried write dedups
/// against it; a later power loss must not leave the acked frame without its
/// payload. Open syncs every pack segment before trusting it.
#[test]
fn g4_kill_restart_retry_never_loses_a_deduplicated_payload() {
    for seed in 0..(runs() / 2 + 1) {
        let disk = SimDisk::new();
        drop(populated(&disk, 1 + seed as usize % 3));
        let p = format!("payload-{seed}").into_bytes();
        let h = *blake3::hash(&p).as_bytes();
        // The killed process: appended to a fresh segment, never synced.
        let seg = Path::new(DIR).join("blobs").join("pack-00000099");
        disk.append(&seg, &pack_record(&p)).unwrap();
        // Restart; the client's retry carries the payload again.
        let (mut store, _) = open(&disk).unwrap();
        store.stage_blobs(&[&p]).unwrap();
        let acked = store.commit(&create_with(&format!("k{seed}"), h)).unwrap();
        drop(store);
        let crashed = disk.crash(FATES[seed as usize % 4], seed);
        let (store, _) = open(&crashed).unwrap();
        assert!(store.epoch() >= acked.epoch, "{seed}: acked frame lost");
        assert_eq!(
            store.get_blob(&h).unwrap(),
            Payload::Available(p),
            "{seed}: acked frame without its payload"
        );
    }
}
/// A killed process's verified but unsynced op_log frames are made durable
/// at open, so an epoch the restarted store reports never disappears.
#[test]
fn g4_kill_restart_makes_page_cache_frames_durable() {
    for seed in 0..(runs() / 2 + 1) {
        let disk = SimDisk::new();
        let mut store = populated(&disk, 2);
        store.put_blob(b"late").unwrap();
        // The frame is appended, then the process dies before its sync.
        disk.fail_next("sync");
        let _ = store.commit(&create_with("late", *blake3::hash(b"late").as_bytes()));
        drop(store);
        let disk = disk.restart();
        let (store, _) = open(&disk).unwrap();
        let seen = store.epoch();
        drop(store);
        let crashed = disk.crash(FATES[seed as usize % 4], seed);
        let (store, _) = open(&crashed).unwrap();
        assert_eq!(
            store.epoch(),
            seen,
            "{seed}: an epoch served after restart vanished"
        );
    }
}
/// A payload staged for an all-rejected group stays usable by a later group
/// of the same process: the later op_log flush makes it durable.
#[test]
fn g4_dedup_on_a_payload_of_a_rejected_group() {
    for seed in 0..(runs() / 4 + 1) {
        let disk = SimDisk::new();
        let mut store = populated(&disk, 1);
        let p = format!("rejected-{seed}").into_bytes();
        let h = *blake3::hash(&p).as_bytes();
        store.stage_blobs(&[&p]).unwrap();
        // Rejected: SetHead on a logical that does not exist.
        let bad = Batch::new(vec![Op::SetHead {
            logical: LogicalId(9_999).into(),
            expected: RevisionId(0).into(),
            new: RevisionId(0).into(),
        }]);
        assert!(store.commit_group(&[bad])[0].is_err());
        store.stage_blobs(&[&p]).unwrap();
        let acked = store.commit(&create_with(&format!("r{seed}"), h)).unwrap();
        drop(store);
        let crashed = disk.crash(FATES[seed as usize % 4], seed);
        let (store, _) = open(&crashed).unwrap();
        assert!(store.epoch() >= acked.epoch);
        assert_eq!(store.get_blob(&h).unwrap(), Payload::Available(p));
    }
}
