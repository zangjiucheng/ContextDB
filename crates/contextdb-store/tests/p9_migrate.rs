//! P9 milestone 6e (ADR 0017 §8.5, §10; T104): `ctx migrate` to format 5.
//!
//! - A format-4 store with plaintext packs and a loose blob migrates:
//!   payloads read back with equal hashes, frame checksums are unchanged,
//!   and no file holds a payload's plaintext.
//! - Every intermediate state (a kill, and a power cut, after each step and
//!   between sources) is refused at open with the right step, and a rerun
//!   completes to the result of an uninterrupted migration.
//! - The marker durable with the log still at format 4; a fault that skips
//!   step 2; a format-5 store holding a plaintext segment.
//! - The lane pre-check changes nothing when it refuses.
//! - Real projections (StdDisk): rebuilt into fresh files, so free pages
//!   and `-wal` files from before P9 hold no plaintext.
use contextdb_model::*;
use contextdb_store::{
    migrate,
    migrate5::{self, Faults, NoModes, Stop},
    *,
};
use std::path::{Path, PathBuf};

const DIR: &str = "/db";
const BODIES: [&str; 4] = [
    "quokka alpha body",
    "quokka bravo body",
    "quokka charlie body",
    "quokka loose body",
];

fn h(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}
fn create(key: &str, lane: &str, body: &str) -> Batch {
    Batch::new(vec![Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: lane.into(),
        content: h(body.as_bytes()),
        adopt_as_base: true,
    }])
}
/// A format-4 store as P8 left it: the log, two plaintext pack segments
/// and one loose blob file (pre-pack).
fn format4_store(disk: &dyn Durability, dir: &Path) {
    let batches: Vec<Batch> = (BODIES.iter().enumerate())
        .map(|(i, b)| create(&format!("k{i}"), "repo", b))
        .collect();
    migrate::write_format4(disk, dir, &batches).unwrap();
    let blobs = dir.join("blobs");
    disk.create_dir_all(&blobs).unwrap();
    let mut seg1 = plaintext_pack_record(BODIES[0].as_bytes());
    seg1.extend(plaintext_pack_record(BODIES[1].as_bytes()));
    for (name, bytes) in [
        ("pack-00000001", seg1),
        ("pack-00000002", plaintext_pack_record(BODIES[2].as_bytes())),
    ] {
        disk.write_new(&blobs.join(name), &bytes).unwrap();
        disk.sync(&blobs.join(name)).unwrap();
    }
    let loose: String = h(BODIES[3].as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    disk.write_new(&blobs.join(&loose), BODIES[3].as_bytes())
        .unwrap();
    disk.sync(&blobs.join(loose)).unwrap();
    disk.sync_dir(&blobs).unwrap();
    disk.sync_dir(dir).unwrap();
}
fn open(disk: &SimDisk) -> Result<Store<SimDisk>, StoreError> {
    Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).map(|(s, _)| s)
}
/// Every file under `dir` (SimDisk), recursively.
fn files(disk: &SimDisk, dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for n in disk.list(dir).unwrap_or_default() {
        let p = dir.join(&n);
        if disk.peek(&p).is_some() {
            out.push(p);
        } else {
            out.extend(files(disk, &p));
        }
    }
    out
}
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}
/// The payload state after a completed migration: each body readable.
fn payloads(store: &Store<SimDisk>) -> Vec<Payload> {
    BODIES
        .iter()
        .map(|b| store.get_blob(&h(b.as_bytes())).unwrap())
        .collect()
}
fn checksums(store: &Store<SimDisk>) -> Vec<ContentHash> {
    (1..=BODIES.len() as u64)
        .map(|e| store.checksum_at(Epoch(e)).unwrap())
        .collect()
}

/// T104: the migration of a format-4 store.
#[test]
fn t104_a_format4_store_migrates_encrypted_with_every_payload() {
    let disk = SimDisk::new();
    let dir = Path::new(DIR);
    format4_store(&disk, dir);
    let old: Vec<ContentHash> = migrate::rows_for_test(&disk, dir, 4)
        .unwrap()
        .iter()
        .map(|r| log::checksum(&r.encode_body_format(4)))
        .collect();
    assert!(matches!(
        open(&disk),
        Err(StoreError::FormatMigrationRequired(4))
    ));
    let report = migrate::migrate(&disk, dir).unwrap();
    assert_eq!((report.from, report.to), (4, 5));
    let enc = report.encryption.expect("steps 3 and 4 ran");
    assert_eq!((enc.segments, enc.loose, enc.records), (2, 1, 4));
    assert_eq!(enc.missing, 0);
    assert!(!disk.exists(&dir.join(migrate5::MARKER)));
    let store = open(&disk).unwrap();
    for (b, p) in BODIES.iter().zip(payloads(&store)) {
        assert_eq!(p, Payload::Available(b.as_bytes().to_vec()));
    }
    assert_eq!(checksums(&store), old, "frame checksums unchanged");
    drop(store);
    // No file holds a payload's plaintext, except the log backup that the
    // migration keeps (it holds hashes, never bodies).
    for f in files(&disk, dir) {
        let bytes = disk.peek(&f).unwrap();
        for b in BODIES {
            assert!(!contains(&bytes, b.as_bytes()), "{} holds {b}", f.display());
        }
        assert!(
            !contains(&bytes, b"CXBP"),
            "{} holds a plaintext record",
            f.display()
        );
    }
    // A second run reports "already at format 5" and changes nothing.
    let before: Vec<_> = files(&disk, dir);
    let again = migrate::migrate(&disk, dir).unwrap();
    assert!(again.encryption.is_none());
    assert_eq!(files(&disk, dir), before);
}

/// The step an opener reports after stopping at `at`, on this store.
fn expected_step(at: Stop) -> u8 {
    match at {
        Stop::After(1) => 2,
        Stop::After(2) | Stop::Source(_) => 3,
        // No projection on a simulated disk: steps 4 to 6 leave only the
        // marker.
        _ => 6,
    }
}

/// T104 intermediate states: a kill (the process stops) and a power cut
/// (SimDisk crash) at every stop point; the opener refuses with the step,
/// `doctor` refuses, and a rerun completes to the uninterrupted result.
#[test]
fn t104_every_intermediate_state_is_refused_and_a_rerun_completes() {
    let reference = {
        let disk = SimDisk::new();
        format4_store(&disk, Path::new(DIR));
        migrate::migrate(&disk, Path::new(DIR)).unwrap();
        let s = open(&disk).unwrap();
        (payloads(&s), checksums(&s))
    };
    let stops = [
        Stop::After(1),
        Stop::After(2),
        Stop::Source(0),
        Stop::Source(1),
        Stop::After(3),
        Stop::After(4),
        Stop::After(5),
        Stop::After(6),
    ];
    for (n, at) in stops.into_iter().enumerate() {
        for power_cut in [false, true] {
            let disk = SimDisk::new();
            let dir = Path::new(DIR);
            format4_store(&disk, dir);
            let packs_before: Vec<_> = files(&disk, &dir.join("blobs"))
                .iter()
                .map(|f| disk.peek(f))
                .collect();
            let faults = Faults {
                stop: Some(at),
                skip_log: false,
            };
            match migrate::migrate_with(&disk, dir, &NoModes, &faults) {
                Err(StoreError::Io(m)) if m == migrate5::STOPPED => {}
                other => panic!("{at:?}: {other:?}"),
            }
            let disk = if power_cut {
                disk.crash(TailFate::Torn, n as u64)
            } else {
                disk.restart()
            };
            let step = expected_step(at);
            match open(&disk) {
                Err(StoreError::MigrationIncomplete { step: s }) => {
                    assert_eq!(s, step, "{at:?} power_cut={power_cut}")
                }
                Err(e) => panic!("{at:?}: {e:?}"),
                Ok(_) => panic!("{at:?}: opened mid-migration"),
            }
            assert!(matches!(
                doctor::plan_truncate_before(&disk, dir, Epoch(2)),
                Err(StoreError::MigrationIncomplete { .. })
            ));
            if at == Stop::After(1) {
                // A P8 binary ignores the marker: the log is still format 4
                // and the packs are untouched.
                assert_eq!(migrate::log_format(&disk, dir).unwrap(), Some(4));
                let now: Vec<_> = files(&disk, &dir.join("blobs"))
                    .iter()
                    .map(|f| disk.peek(f))
                    .collect();
                assert_eq!(now, packs_before);
            } else {
                // From step 2 on, a P8 binary refuses the format-5 log.
                assert_eq!(migrate::log_format(&disk, dir).unwrap(), Some(5));
            }
            let r = migrate::migrate(&disk, dir).unwrap();
            assert!(r.encryption.is_some(), "{at:?}");
            let s = open(&disk).unwrap();
            assert_eq!((payloads(&s), checksums(&s)), reference, "{at:?}");
        }
    }
}

/// T104: the marker durable and the log still at format 4, no swap begun.
/// A fault that skips step 2 cannot report success.
#[test]
fn t104_marker_durable_with_the_log_still_at_format4() {
    let disk = SimDisk::new();
    let dir = Path::new(DIR);
    format4_store(&disk, dir);
    disk.write_new(&dir.join(migrate5::MARKER), b"5\n").unwrap();
    disk.sync(&dir.join(migrate5::MARKER)).unwrap();
    disk.sync_dir(dir).unwrap();
    assert!(matches!(
        open(&disk),
        Err(StoreError::MigrationIncomplete { step: 2 })
    ));
    let skip = Faults {
        stop: None,
        skip_log: true,
    };
    assert!(matches!(
        migrate::migrate_with(&disk, dir, &NoModes, &skip),
        Err(StoreError::MigrationIncomplete { step: 2 })
    ));
    assert!(disk.exists(&dir.join(migrate5::MARKER)), "no success");
    assert_eq!(migrate::log_format(&disk, dir).unwrap(), Some(4));
    migrate::migrate(&disk, dir).unwrap();
    assert!(open(&disk).is_ok());
}

/// T104: no success, and no open, from the log version alone: a format-5
/// store holding a plaintext segment (a milestone-2 migration, or a lost
/// marker), and one with the marker and nothing else left.
#[test]
fn t104_a_format5_store_with_plaintext_is_incomplete() {
    let disk = SimDisk::new();
    let dir = Path::new(DIR);
    {
        let (mut s, _) = Store::open(
            disk.clone(),
            dir,
            &OpenOptions {
                create: true,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let c = s.put_blob(b"new body").unwrap();
        s.commit(&Batch::new(vec![Op::CreateLogical {
            key: "n".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: c,
            adopt_as_base: true,
        }]))
        .unwrap();
    }
    let old = dir.join("blobs").join("pack-00000050");
    disk.write_new(&old, &plaintext_pack_record(b"old body"))
        .unwrap();
    disk.sync(&old).unwrap();
    disk.sync_dir(&dir.join("blobs")).unwrap();
    assert!(matches!(
        open(&disk),
        Err(StoreError::MigrationIncomplete { step: 3 })
    ));
    let r = migrate::migrate(&disk, dir).unwrap();
    assert_eq!(r.encryption.unwrap().segments, 1);
    let s = open(&disk).unwrap();
    assert_eq!(
        s.get_blob(&h(b"old body")).unwrap(),
        Payload::Available(b"old body".to_vec())
    );
    drop(s);
    // The marker alone: refused at step 6, and the rerun completes.
    disk.write_new(&dir.join(migrate5::MARKER), b"5\n").unwrap();
    assert!(matches!(
        open(&disk),
        Err(StoreError::MigrationIncomplete { step: 6 })
    ));
    migrate::migrate(&disk, dir).unwrap();
    assert!(open(&disk).is_ok());
}

/// Step 0: a `personal:<x>` lane with an invalid name refuses the migration
/// and changes nothing.
#[test]
fn t104_the_lane_pre_check_changes_nothing() {
    let disk = SimDisk::new();
    let dir = Path::new(DIR);
    migrate::write_format4(&disk, dir, &[create("k", "personal:Not Valid", "x")]).unwrap();
    let before: Vec<_> = files(&disk, dir).iter().map(|f| disk.peek(f)).collect();
    match migrate::migrate(&disk, dir) {
        Err(StoreError::Tx(TxError::InvalidAccess { reason })) => {
            assert!(reason.contains("personal:Not Valid"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    let after: Vec<_> = files(&disk, dir).iter().map(|f| disk.peek(f)).collect();
    assert_eq!(after, before);
    assert!(!disk.exists(&dir.join(migrate5::MARKER)));
}

/// Shredded contents in a plaintext segment are dropped, not re-encrypted.
#[test]
fn t104_shredded_plaintext_is_not_carried_over() {
    let disk = SimDisk::new();
    let dir = Path::new(DIR);
    {
        let (mut s, _) = Store::open(
            disk.clone(),
            dir,
            &OpenOptions {
                create: true,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let c = s.put_blob(b"to shred").unwrap();
        s.commit(&Batch::new(vec![Op::CreateLogical {
            key: "s".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: c,
            adopt_as_base: true,
        }]))
        .unwrap();
        let mut b = Batch::new(vec![Op::ShredContent { content: c }]);
        b.channel = Channel::Admin;
        s.commit(&b).unwrap();
    }
    // A plaintext copy left from before P9.
    let old = dir.join("blobs").join("pack-00000050");
    disk.write_new(&old, &plaintext_pack_record(b"to shred"))
        .unwrap();
    disk.sync(&old).unwrap();
    disk.sync_dir(&dir.join("blobs")).unwrap();
    let enc = migrate::migrate(&disk, dir).unwrap().encryption.unwrap();
    assert_eq!((enc.dropped_shredded, enc.records), (1, 0));
    for f in files(&disk, dir) {
        assert!(
            !contains(&disk.peek(&f).unwrap(), b"to shred"),
            "{}",
            f.display()
        );
    }
}

mod projections {
    //! Real files: the projections live outside `Durability`.
    use super::*;
    use rusqlite::Connection;

    const SECRET: &str = "zanzibar-pre-p9-secret";

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ctx-p9-migrate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    /// A P8 projection: rows deleted without `secure_delete`, so the text
    /// stays in free pages, and more rows left in the `-wal`.
    fn p8_projection(path: &Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA secure_delete=OFF; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE t(body TEXT);",
        )
        .unwrap();
        for i in 0..200 {
            c.execute("INSERT INTO t VALUES (?1)", [format!("{SECRET} {i}")])
                .unwrap();
        }
        c.execute("INSERT INTO t VALUES ('kept row')", []).unwrap();
        c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
        c.execute("DELETE FROM t WHERE body != 'kept row'", [])
            .unwrap();
        c.execute("INSERT INTO t VALUES (?1)", [format!("{SECRET} in wal")])
            .unwrap();
        c.execute("DELETE FROM t WHERE body LIKE '%in wal'", [])
            .unwrap();
        // Leak the connection so the `-wal` stays (a killed daemon).
        std::mem::forget(c);
    }
    fn holds(dir: &Path, needle: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_file() && contains(&std::fs::read(&p).unwrap(), needle) {
                out.push(p.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
        out
    }

    /// T104 projections: fresh files with the stamp, no pre-P9 plaintext
    /// in free pages or `-wal`, the same rows; a kill between projections
    /// is refused at step 4 and the rerun completes.
    #[test]
    fn t104_projections_are_rebuilt_without_old_free_pages() {
        let dir = temp("proj");
        let disk = StdDisk::default();
        format4_store(&disk, &dir);
        for name in purge::FILES {
            p8_projection(&dir.join(name));
        }
        let before = holds(&dir, SECRET.as_bytes());
        assert!(
            before.iter().any(|n| n == "recall.sqlite"),
            "the setup leaves text in free pages: {before:?}"
        );
        let faults = Faults {
            stop: Some(Stop::Projection(1)),
            skip_log: false,
        };
        assert!(migrate::migrate_with(&disk, &dir, &NoModes, &faults).is_err());
        assert!(matches!(
            Store::open(disk.clone(), &dir, &OpenOptions::default()),
            Err(StoreError::MigrationIncomplete { step: 4 })
        ));
        let r = migrate::migrate(&disk, &dir).unwrap();
        let enc = r.encryption.unwrap();
        assert_eq!(
            enc.projections.len(),
            2,
            "two were rebuilt by the first run"
        );
        assert_eq!(holds(&dir, SECRET.as_bytes()), Vec::<String>::new());
        for name in purge::FILES {
            let p = dir.join(name);
            assert!(purge::stamped(&p).unwrap(), "{name}");
            let c = Connection::open(&p).unwrap();
            let rows: Vec<String> = c
                .prepare("SELECT body FROM t")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(rows, vec!["kept row".to_owned()], "{name}");
        }
        assert!(holds(&dir, BODIES[0].as_bytes()).is_empty());
        let (store, _) = Store::open(disk.clone(), &dir, &OpenOptions::default()).unwrap();
        assert_eq!(
            store.get_blob(&h(BODIES[3].as_bytes())).unwrap(),
            Payload::Available(BODIES[3].as_bytes().to_vec())
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
