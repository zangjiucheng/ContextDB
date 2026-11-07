//! P9 milestone 2 (ADR 0017 §4.1, §10): `SetAccess` is durable in op_log
//! format 5, survives replay and snapshots, and a format-4 log migrates to
//! format 5 with every batch and frame checksum unchanged.
use contextdb_model::*;
use contextdb_store::{migrate, *};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const DIR: &str = "/db";
fn doc(extra: &[(&str, u32)]) -> AccessDoc {
    let mut principals: BTreeMap<String, u32> = [("alice".to_owned(), 501)].into();
    principals.extend(extra.iter().map(|(n, u)| ((*n).to_owned(), *u)));
    let members: BTreeSet<String> = ["*members".to_owned()].into();
    AccessDoc {
        version: 1,
        owner: "alice".into(),
        principals,
        groups: BTreeMap::new(),
        lanes: [(
            "repo".to_owned(),
            LaneEntry {
                members: members.clone(),
                writers: members,
                ..LaneEntry::owner_only()
            },
        )]
        .into(),
        code_lanes: LaneEntry::code_default(),
        personal_egress: false,
        default_lane: LaneEntry::owner_only(),
    }
}
fn set_access(d: AccessDoc) -> Batch {
    let mut b = Batch::new(vec![Op::SetAccess { doc: Box::new(d) }]);
    b.channel = Channel::Admin;
    b
}
fn open(disk: &SimDisk) -> (Store<SimDisk>, OpenReport) {
    Store::open(
        disk.clone(),
        Path::new(DIR),
        &OpenOptions {
            create: true,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}
fn ids(s: &Store<SimDisk>) -> Option<BTreeMap<String, PrincipalId>> {
    s.kernel().unwrap().access().map(|a| a.ids.clone())
}

#[test]
fn the_access_document_survives_replay_and_snapshots() {
    let disk = SimDisk::new();
    let (mut store, _) = open(&disk);
    let content = store.put_blob(b"a").unwrap();
    store
        .commit(&Batch::new(vec![Op::CreateLogical {
            key: "a".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content,
            adopt_as_base: true,
        }]))
        .unwrap();
    store.commit(&set_access(doc(&[("bob", 502)]))).unwrap();
    store.commit(&set_access(doc(&[("carol", 503)]))).unwrap();
    let before = ids(&store).unwrap();
    assert_eq!(before["bob"], PrincipalId(16));
    assert_eq!(before["carol"], PrincipalId(17));
    let doc_before = store.kernel().unwrap().access().unwrap().doc.clone();
    drop(store);
    // Replay from the log.
    let (store, report) = open(&disk);
    assert_eq!(report.replayed, 3);
    assert_eq!(ids(&store).unwrap(), before);
    assert_eq!(store.kernel().unwrap().access().unwrap().doc, doc_before);
    // From a snapshot, then one more document replayed on top.
    let mut store = store;
    let at = store.snapshot().unwrap();
    store
        .commit(&set_access(doc(&[("carol", 503), ("dave", 504)])))
        .unwrap();
    drop(store);
    let (store, report) = open(&disk);
    assert_eq!(report.snapshot_epoch, Some(at));
    assert_eq!(report.replayed, 1);
    let after = ids(&store).unwrap();
    assert_eq!(after["dave"], PrincipalId(18));
    assert_eq!(after["bob"], PrincipalId(16), "a dropped name keeps its id");
}

/// ADR 0017 §10 (milestone 2: ops only): a format-4 log migrates to format
/// 5; old ops keep their tags, so batches, hashes and frame checksums are
/// unchanged and certificates keep their log links.
#[test]
fn format4_log_migrates_to_format5_byte_identical() {
    let disk = SimDisk::new();
    let batches: Vec<Batch> = (0..3u8)
        .map(|n| {
            let mut b = Batch::new(vec![Op::CreateLogical {
                key: format!("k{n}"),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: [n; 32],
                adopt_as_base: true,
            }]);
            b.client_txn_id = Some([n + 1; 16]);
            b
        })
        .collect();
    assert_eq!(
        migrate::write_format4(&disk, Path::new(DIR), &batches).unwrap(),
        3
    );
    let old_rows = migrate::rows_for_test(&disk, Path::new(DIR), 4).unwrap();
    let old: Vec<_> = (old_rows.iter())
        .map(|r| {
            (
                r.batch_blob.clone(),
                log::checksum(&r.encode_body_format(4)),
            )
        })
        .collect();
    assert!(matches!(
        Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()),
        Err(StoreError::FormatMigrationRequired(4))
    ));
    let report = migrate::migrate(&disk, Path::new(DIR)).unwrap();
    assert_eq!((report.from, report.to, report.frames), (4, 5, 3));
    assert!(report.changed.is_empty(), "{:?}", report.changed);
    assert!(disk.exists(&Path::new(DIR).join("log.v4.bak")));
    let (mut store, _) =
        Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).unwrap();
    let new: Vec<_> = (1..=3)
        .map(|e| store.checksum_at(Epoch(e)).unwrap())
        .collect();
    let new_blobs: Vec<_> = migrate::rows_for_test(&disk, Path::new(DIR), 5)
        .unwrap()
        .into_iter()
        .map(|r| r.batch_blob)
        .collect();
    assert_eq!(
        old.iter().map(|(b, _)| b.clone()).collect::<Vec<_>>(),
        new_blobs
    );
    assert_eq!(old.iter().map(|(_, s)| *s).collect::<Vec<_>>(), new);
    // The migrated store takes format-5 ops.
    store.commit(&set_access(doc(&[]))).unwrap();
    assert!(ids(&store).is_some());
}
