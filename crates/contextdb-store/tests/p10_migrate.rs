//! T119 (ADR 0018 §12, SPEC §6.1 contract 16): op_log format 5 → 6. A P9
//! log — branches, a merge, a shred — migrates with every batch, batch
//! hash, idempotency record and frame checksum unchanged (format 6 adds no
//! frame field and keeps Local encodings); afterwards the store takes P10
//! batches (the replica channel, a replica-origin create, MirrorAttach, a
//! gap watermark) and replays them.
use contextdb_model::*;
use contextdb_store::{migrate, *};
use std::path::Path;

const DIR: &str = "/db";
const TEAM: [u8; 16] = [0xab; 16];

fn txn(n: u64) -> [u8; 16] {
    let mut id = [0; 16];
    id[..8].copy_from_slice(&n.to_le_bytes());
    id
}

fn fact(key: &str, content: u8) -> Op {
    Op::CreateLogical {
        origin: Origin::Local,
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [content; 32],
        adopt_as_base: true,
    }
}

fn on(branch: BranchId, channel: Channel, ops: Vec<Op>) -> Batch {
    let mut b = Batch::new(ops);
    b.branch = branch;
    b.channel = channel;
    b
}

#[test]
fn t119_format5_log_migrates_to_format6_and_takes_p10_batches() {
    let disk = SimDisk::new();
    let b1 = BranchId(1);
    let batches: Vec<Batch> = vec![
        on(MAIN, Channel::Agent, vec![fact("a", 1), fact("b", 2)]),
        on(
            MAIN,
            Channel::Admin,
            vec![Op::CreateBranch {
                name: "feature".into(),
            }],
        ),
        on(b1, Channel::Agent, vec![fact("c", 3)]),
        on(
            MAIN,
            Channel::Agent,
            vec![Op::Merge {
                source: b1,
                require_certified: false,
                certify: Vec::new(),
            }],
        ),
        on(
            MAIN,
            Channel::Admin,
            vec![Op::ShredContent { content: [9; 32] }],
        ),
    ]
    .into_iter()
    .enumerate()
    .map(|(n, mut b)| {
        b.client_txn_id = Some(txn(n as u64));
        b
    })
    .collect();
    let n = migrate::write_format5(&disk, Path::new(DIR), &batches).unwrap();
    assert_eq!(n, batches.len() as u64);
    let old_rows = migrate::rows_for_test(&disk, Path::new(DIR), 5).unwrap();
    let old: Vec<_> = old_rows
        .iter()
        .map(|r| (r.batch_blob.clone(), r.batch_hash, r.client_hash))
        .collect();
    let old_sums: Vec<_> = old_rows
        .iter()
        .map(|r| log::checksum(&r.encode_body_format(5)))
        .collect();
    // A format-6 build refuses the format-5 log until it is migrated.
    assert!(matches!(
        Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()),
        Err(StoreError::FormatMigrationRequired(5))
    ));
    let report = migrate::migrate(&disk, Path::new(DIR)).unwrap();
    assert_eq!((report.from, report.to, report.frames), (5, 6, n));
    assert!(report.changed.is_empty(), "{:?}", report.changed);
    assert!(disk.exists(&Path::new(DIR).join("log.v5.bak")));
    let new: Vec<_> = migrate::rows_for_test(&disk, Path::new(DIR), log::MIN_FORMAT)
        .unwrap()
        .into_iter()
        .map(|r| (r.batch_blob, r.batch_hash, r.client_hash))
        .collect();
    assert_eq!(
        old, new,
        "batches, hashes and idempotency records unchanged"
    );
    let (store, open) = Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).unwrap();
    assert_eq!(open.replayed, n);
    let sums: Vec<_> = (1..=n)
        .map(|e| store.checksum_at(Epoch(e)).unwrap())
        .collect();
    assert_eq!(
        old_sums, sums,
        "frame checksums survive 5 -> 6: certificates keep their log link"
    );
    // Migrating again is a no-op.
    drop(store);
    let again = migrate::migrate(&disk, Path::new(DIR)).unwrap();
    assert_eq!((again.from, again.to, again.frames), (6, 6, 0));
    let (mut store, _) =
        Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).unwrap();
    // P10 batches are now durable: a mirror on main, attached on the
    // feature branch, a gap watermark.
    let hex = "ab".repeat(16);
    let body = store.put_blob(b"team decision").unwrap();
    let key = format!("team/{hex}/conv/a");
    let mirror = on(
        MAIN,
        Channel::Replica,
        vec![Op::CreateLogical {
            origin: Origin::Replica { team: TEAM },
            key: key.clone(),
            kind: RecordKind::Decision,
            scope_lane: format!("team:{hex}/main"),
            content: body,
            adopt_as_base: true,
        }],
    );
    let attach = on(
        b1,
        Channel::Replica,
        vec![
            Op::MirrorAttach {
                key: key.clone(),
                head: body,
                base: true,
            },
            Op::ObserverWatermark {
                scope: format!("team:{hex}"),
                expected: None,
                position: b"oid".to_vec(),
                gap: true,
            },
        ],
    );
    store.commit(&mirror).unwrap();
    store.commit(&attach).unwrap();
    let digest = store.kernel().unwrap().branch(b1).unwrap().db().digest();
    drop(store);
    let (store, open) = Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).unwrap();
    assert_eq!(open.replayed, n + 2);
    let k = store.kernel().unwrap();
    assert_eq!(k.branch(b1).unwrap().db().digest(), digest);
    let main = k.branch(MAIN).unwrap();
    let l = main.db().logical_by_key(&key).unwrap();
    assert_eq!(main.db().logical_origin(l), Origin::Replica { team: TEAM });
}
