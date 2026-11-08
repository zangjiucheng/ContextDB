//! Operator CLI over a real filesystem store (StdDisk, F_FULLFSYNC on macOS):
//! the production durability path smoke test (ADR 0006 §10, review n9).
use contextdb_model::*;
use contextdb_store::{OpenOptions, ProjectionFile, StdDisk, Store};
use std::process::Command;

fn ctx(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(args)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}
#[test]
fn replay_snapshot_doctor_on_a_real_store() {
    let dir = std::env::temp_dir().join(format!("contextdb-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let options = OpenOptions {
        projections: ProjectionFile::Path(dir.join(contextdb_store::PROJECTIONS_FILE)),
        ..OpenOptions::default()
    };
    let mut committed = 0;
    {
        let (mut store, _) = Store::open(StdDisk::default(), &dir, &options).unwrap();
        for b in contextdb_testkit::generate_p1(3).batches {
            let mut b = b.clone();
            for op in &mut b.ops {
                if let Op::CreateLogical { content, .. } | Op::AddRevision { content, .. } = op {
                    *content = store.put_blob(content).unwrap();
                }
            }
            if store.commit(&b).is_ok() {
                committed += 1;
            }
        }
    }
    assert!(committed > 3);
    let (ok, out) = ctx(&["replay", dir.to_str().unwrap()]);
    assert!(ok, "{out}");
    assert!(
        out.contains(&format!("epoch={committed} replayed={committed}")),
        "{out}"
    );
    let (ok, out) = ctx(&["snapshot", dir.to_str().unwrap()]);
    assert!(
        ok && out.contains(&format!("snapshot at epoch {committed}")),
        "{out}"
    );
    let (ok, out) = ctx(&["doctor", dir.to_str().unwrap(), "--rebuild"]);
    assert!(ok, "{out}");
    assert!(out.contains("replayed=0"), "snapshot used: {out}");
    assert!(
        out.contains(&format!("projections complete through epoch {committed}")),
        "{out}"
    );
    assert!(
        out.contains("oracle check: derived state == jacobi"),
        "{out}"
    );
    assert!(out.contains("payloads: 0 unavailable"), "{out}");
    // Dry-run truncation lists acked loss and changes nothing.
    let (ok, out) = ctx(&["doctor", dir.to_str().unwrap(), "--truncate-before", "2"]);
    assert!(
        ok && out.contains("ACKED DATA LOSS") && out.contains("dry run"),
        "{out}"
    );
    let (ok, out) = ctx(&["replay", dir.to_str().unwrap()]);
    assert!(ok && out.contains(&format!("epoch={committed}")), "{out}");
    // Export round-trips through the EventLog reader.
    let (ok, out) = ctx(&["export", dir.to_str().unwrap()]);
    assert!(ok, "{out}");
    let exported = EventLog::from_jsonl(&out).unwrap();
    assert_eq!(exported.batches.len(), committed);
    let _ = std::fs::remove_dir_all(&dir);
}

/// T104 through the binary (ADR 0017 §10): `ctx doctor` refuses a store
/// marked mid-migration and names `ctx migrate`; `ctx migrate` completes
/// (log, packs, modes) and a second run reports "already at format 5".
#[test]
fn migrate_completes_and_openers_refuse_a_marked_store() {
    use contextdb_store::{Durability, migrate, migrate5};
    let dir = std::env::temp_dir().join(format!("contextdb-cli-migrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let disk = StdDisk::default();
    let body = b"pre-p9 body".to_vec();
    let batch = Batch::new(vec![Op::CreateLogical {
        key: "k".into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: *blake3::hash(&body).as_bytes(),
        adopt_as_base: true,
    }]);
    migrate::write_format4(&disk, &dir, &[batch]).unwrap();
    let blobs = dir.join("blobs");
    disk.create_dir_all(&blobs).unwrap();
    disk.write_new(
        &blobs.join("pack-00000001"),
        &contextdb_store::plaintext_pack_record(&body),
    )
    .unwrap();
    disk.write_new(&dir.join(migrate5::MARKER), b"5\n").unwrap();
    let d = dir.to_str().unwrap();
    let (ok, out) = ctx(&["doctor", d]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("MigrationIncomplete { step: 2 }") && out.contains("run `ctx migrate"),
        "{out}"
    );
    let (ok, out) = ctx(&["migrate", d]);
    assert!(ok, "{out}");
    assert!(out.contains("migrated format 4 -> 5"), "{out}");
    assert!(
        out.contains("re-encrypted 1 records from 1 pack segments"),
        "{out}"
    );
    assert!(!dir.join(migrate5::MARKER).exists());
    let (ok, out) = ctx(&["migrate", d]);
    assert!(ok && out.contains("already at format 5"), "{out}");
    let (ok, out) = ctx(&["doctor", d]);
    assert!(ok, "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}
