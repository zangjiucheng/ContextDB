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
    for (n, step) in ["write", "sync", "rename", "sync_dir"]
        .into_iter()
        .enumerate()
    {
        for seed in 0..(runs() / 4 + 1) {
            let disk = SimDisk::new();
            let mut store = populated(&disk, 1 + seed as usize % 4);
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
            match store.get_blob(&hash) {
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
    let mut inputs = store.db().unwrap().inputs();
    drop(store);
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
    }
    let scan = log::scan(&disk, &Path::new(DIR).join("log")).unwrap();
    let checksum = scan.frames.last().unwrap().checksum;
    contextdb_store::snapshot::write(&disk, Path::new(DIR), epoch, &checksum, &inputs).unwrap();
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
