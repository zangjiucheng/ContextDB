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
