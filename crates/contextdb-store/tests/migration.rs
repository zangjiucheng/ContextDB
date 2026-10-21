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
