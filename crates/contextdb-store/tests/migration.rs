//! SPEC T68: a format-0 op_log migrates to format 1, replay state matches the
//! original batches, and originally acked results survive for idempotent retries.
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
    assert_eq!((report.from, report.to, report.frames), (0, 1, written));
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
    assert_eq!(store.db().digest(), reference.digest());
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
