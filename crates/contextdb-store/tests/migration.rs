//! SPEC T68: format-0 and format-1 op_logs migrate to the current format
//! (format 2, group framing, ADR 0008 §3.2); replay state matches the original
//! batches, and originally acked results survive for idempotent retries.
use contextdb_core::Db;
use contextdb_model::*;
use contextdb_store::{migrate, *};
use std::path::Path;

const DIR: &str = "/db";
fn txn(n: u64) -> [u8; 16] {
    let mut id = [0; 16];
    id[..8].copy_from_slice(&n.to_le_bytes());
    id
}
#[test]
fn format0_log_migrates_with_identical_state_t68() {
    let disk = SimDisk::new();
    let batches: Vec<Batch> = contextdb_testkit::generate_p1(5)
        .batches
        .into_iter()
        .enumerate()
        .map(|(n, mut b)| {
            b.client_txn_id = Some(txn(n as u64));
            b
        })
        .collect();
    let written = migrate::write_legacy(&disk, Path::new(DIR), &batches).unwrap();
    assert!(written > 3);
    // Opening an older format without migrating fails closed.
    assert!(matches!(
        Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()),
        Err(StoreError::FormatMigrationRequired(0))
    ));
    let report = migrate::migrate(&disk, Path::new(DIR)).unwrap();
    assert_eq!(
        (report.from, report.to, report.frames),
        (0, log::FORMAT_VERSION, written)
    );
    assert!(disk.exists(&Path::new(DIR).join("log.v0.bak")));
    let (mut store, open) =
        Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).unwrap();
    assert_eq!(open.replayed, written);
    // Replay state equals the original batches through a fresh kernel.
    let mut reference = Db::default();
    let mut acked = Vec::new();
    for b in &batches {
        if let Ok(r) = reference.commit(b) {
            acked.push((b.clone(), r));
        }
    }
    assert_eq!(store.db().unwrap().digest(), reference.digest());
    // Idempotent retries return what was originally acked (format 0 had no status_diff).
    for (b, r) in &acked {
        let original = CommitResult {
            status_diff: Vec::new(),
            ..r.clone()
        };
        assert_eq!(store.commit(b).unwrap(), original);
    }
    // Migrating again is a no-op.
    drop(store);
    assert_eq!(migrate::migrate(&disk, Path::new(DIR)).unwrap().frames, 0);
}
#[test]
fn corrupt_legacy_log_aborts_migration_untouched() {
    let disk = SimDisk::new();
    migrate::write_legacy(
        &disk,
        Path::new(DIR),
        &contextdb_testkit::generate_p1(6).batches,
    )
    .unwrap();
    let dir = Path::new(DIR).join("log");
    let seg = dir.join(disk.list(&dir).unwrap()[0].clone());
    disk.corrupt(&seg, contextdb_store::log::HEADER_LEN + 30, 0x99);
    assert!(matches!(
        migrate::migrate(&disk, Path::new(DIR)),
        Err(StoreError::ChainBroken { .. })
    ));
    // The old log is still the live log.
    assert_eq!(migrate::log_format(&disk, Path::new(DIR)).unwrap(), Some(0));
}
#[test]
fn format1_log_migrates_to_group_framing_t68() {
    let disk = SimDisk::new();
    let batches: Vec<Batch> = contextdb_testkit::generate_p1(6)
        .batches
        .into_iter()
        .enumerate()
        .map(|(n, mut b)| {
            b.client_txn_id = Some(txn(n as u64));
            b
        })
        .collect();
    let written = migrate::write_format1(&disk, Path::new(DIR), &batches).unwrap();
    assert!(matches!(
        Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()),
        Err(StoreError::FormatMigrationRequired(1))
    ));
    let report = migrate::migrate(&disk, Path::new(DIR)).unwrap();
    assert_eq!((report.from, report.frames), (1, written));
    assert!(disk.exists(&Path::new(DIR).join("log.v1.bak")));
    let (mut store, open) =
        Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).unwrap();
    assert_eq!(open.replayed, written);
    let mut reference = Db::default();
    let mut acked = Vec::new();
    for b in &batches {
        if let Ok(r) = reference.commit(b) {
            acked.push((b.clone(), r));
        }
    }
    assert_eq!(store.db().unwrap().digest(), reference.digest());
    // Format-1 results were already current: retries return them unchanged.
    for (b, r) in &acked {
        assert_eq!(&store.commit(b).unwrap(), r);
    }
    // New commits continue the group sequence after the migrated groups.
    let content = store.put_blob(b"after").unwrap();
    store
        .commit(&Batch::new(vec![Op::CreateLogical {
            key: "after".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content,
            adopt_as_base: true,
        }]))
        .unwrap();
    let epoch = store.epoch();
    drop(store);
    let (store, _) = Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).unwrap();
    assert_eq!(store.epoch(), epoch);
}

/// T68 2→3 (ADR 0013 §8): a format-2 log is refused until migrated; the
/// migration re-derives every result under the P6 kernel, whose Conflict
/// gate changes some of them; those epochs are reported and their acked
/// results survive for idempotent retries.
#[test]
fn format2_log_migrates_to_format3_and_reports_changed_results() {
    let disk = SimDisk::new();
    let fact = |key: &str, content: u8| Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [content; 32],
        adopt_as_base: true,
    };
    let conflict = Op::OpenArtifact {
        ty: ArtifactType::Conflict,
        target: RevisionId(0).into(),
        payload_hash: [1; 32],
    };
    let batches: Vec<Batch> = [vec![fact("a", 0)], vec![conflict], vec![fact("b", 1)]]
        .into_iter()
        .enumerate()
        .map(|(n, ops)| {
            let mut b = Batch::new(ops);
            b.client_txn_id = Some(txn(n as u64));
            b
        })
        .collect();
    // The P5 kernel had no gate: opening a Conflict changed no status.
    let pre_gate = |b: &Batch, r: CommitResult| {
        if matches!(b.ops[0], Op::OpenArtifact { .. }) {
            CommitResult {
                status_diff: Vec::new(),
                ..r
            }
        } else {
            r
        }
    };
    let written = migrate::write_format2_with(&disk, Path::new(DIR), &batches, &pre_gate).unwrap();
    assert_eq!(written, 3);
    assert!(matches!(
        Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()),
        Err(StoreError::FormatMigrationRequired(2))
    ));
    let report = migrate::migrate(&disk, Path::new(DIR)).unwrap();
    assert_eq!((report.from, report.to, report.frames), (2, 3, 3));
    assert_eq!(report.changed, vec![Epoch(2)]);
    assert!(disk.exists(&Path::new(DIR).join("log.v2.bak")));
    let (mut store, open) =
        Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).unwrap();
    assert_eq!(open.replayed, 3);
    let row = store.read_row(Epoch(2)).unwrap();
    assert_eq!(row.result().unwrap().status_diff, vec![RevisionId(0)]);
    assert_eq!(
        row.trailer().unwrap(),
        Some(ResultTrailer {
            branch: MAIN,
            merge: None
        })
    );
    // An idempotent retry returns what the client was told.
    assert!(store.commit(&batches[1]).unwrap().status_diff.is_empty());
    assert_eq!(
        store.db().unwrap().status(RevisionId(0)).unwrap(),
        Summary::Conflicted
    );
}

/// Format 3 stores branches: branch commits and merges are durable,
/// replay checks their trailers, and protocol-3 readers are refused once
/// `main`'s ids are renumbered.
#[test]
fn branches_are_durable_and_replayed() {
    let disk = SimDisk::new();
    let open = |disk: &SimDisk| {
        Store::open(
            disk.clone(),
            Path::new(DIR),
            &OpenOptions {
                create: true,
                ..OpenOptions::default()
            },
        )
        .unwrap()
        .0
    };
    let fact = |store: &mut Store<SimDisk>, key: &str| Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: store.put_blob(key.as_bytes()).unwrap(),
        adopt_as_base: true,
    };
    let on = |branch, ops| {
        let mut b = Batch::new(ops);
        b.branch = branch;
        b
    };
    let mut store = open(&disk);
    let a = fact(&mut store, "a");
    store.commit(&Batch::new(vec![a])).unwrap();
    store
        .commit(&Batch::new(vec![Op::CreateBranch { name: "b".into() }]))
        .unwrap();
    let b = BranchId(1);
    let x = fact(&mut store, "x");
    store.commit(&on(b, vec![x])).unwrap();
    assert_eq!(
        store.db().unwrap().logical_count(),
        1,
        "main is still identity"
    );
    let y = fact(&mut store, "y");
    store.commit(&Batch::new(vec![y])).unwrap(); // main: local 1, global 2
    assert_eq!(store.db().unwrap_err(), StoreError::BranchAwareReadRequired);
    let merged = store
        .commit(&Batch::new(vec![Op::Merge {
            source: b,
            require_certified: false,
            certify: Vec::new(),
        }]))
        .unwrap();
    let row = store.read_row(merged.epoch).unwrap();
    let trailer = row.trailer().unwrap().unwrap();
    assert_eq!(trailer.branch, MAIN);
    assert_eq!(trailer.merge.unwrap().newly_visible, [1, 1, 0, 0]);
    let digests = |s: &Store<SimDisk>| -> Vec<ContentHash> {
        let k = s.kernel().unwrap();
        k.branches()
            .map(|b| k.branch(b.id).unwrap().digest())
            .collect()
    };
    let before = digests(&store);
    drop(store);
    let store = open(&disk);
    assert_eq!(digests(&store), before, "replay rebuilds every branch");
}

/// Format-3 snapshots hold every branch: reopening from one replays only
/// the tail and rebuilds each branch exactly; commits continue.
#[test]
fn multi_branch_snapshot_round_trip() {
    let disk = SimDisk::new();
    let open = |disk: &SimDisk| {
        Store::open(
            disk.clone(),
            Path::new(DIR),
            &OpenOptions {
                create: true,
                ..OpenOptions::default()
            },
        )
        .unwrap()
    };
    let fact = |store: &mut Store<SimDisk>, key: &str| Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: store.put_blob(key.as_bytes()).unwrap(),
        adopt_as_base: true,
    };
    let on = |branch, ops| {
        let mut b = Batch::new(ops);
        b.branch = branch;
        b
    };
    let (mut store, _) = open(&disk);
    let a = fact(&mut store, "a");
    store.commit(&Batch::new(vec![a])).unwrap();
    store
        .commit(&Batch::new(vec![Op::CreateBranch { name: "b".into() }]))
        .unwrap();
    let b = BranchId(1);
    let x = fact(&mut store, "x");
    store.commit(&on(b, vec![x])).unwrap();
    store
        .commit(&Batch::new(vec![Op::Merge {
            source: b,
            require_certified: false,
            certify: Vec::new(),
        }]))
        .unwrap();
    let at = store.snapshot().unwrap();
    let y = fact(&mut store, "y");
    store.commit(&on(b, vec![y])).unwrap();
    let digests = |s: &Store<SimDisk>| -> Vec<ContentHash> {
        let k = s.kernel().unwrap();
        k.branches()
            .map(|b| k.branch(b.id).unwrap().digest())
            .collect()
    };
    let before = digests(&store);
    drop(store);
    let (mut store, report) = open(&disk);
    assert_eq!(report.snapshot_epoch, Some(at));
    assert_eq!(report.replayed, 1);
    assert_eq!(digests(&store), before);
    let z = fact(&mut store, "z");
    store.commit(&on(b, vec![z])).unwrap();
}
