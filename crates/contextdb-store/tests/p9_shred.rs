//! P9 milestone 6b (ADR 0017 §8.3): `ShredContent` in the store.
//!
//! - T59 (store half): a shred changes no topology, status or digest; the
//!   payload reads `Shredded`, before and after a restart and from a
//!   snapshot; re-sending the bytes and later ops naming the hash are
//!   `ContentShredded`; the op is admin only, alone in its batch, on
//!   `main`, never dry-run.
//! - PROP16: random multi-branch histories with shreds against the same
//!   history without them, epochs renumbered.
#[path = "../../contextdb-testkit/tests/common/branch_gen.rs"]
mod branch_gen;
use branch_gen::Gen;
use contextdb_model::*;
use contextdb_store::*;
use std::collections::BTreeSet;
use std::path::Path;

const DIR: &str = "/db";
fn open(disk: &SimDisk) -> Store<SimDisk> {
    Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default())
        .unwrap()
        .0
}
fn shred(contents: &[ContentHash]) -> Batch {
    let mut b = Batch::new(
        contents
            .iter()
            .map(|c| Op::ShredContent { content: *c })
            .collect(),
    );
    b.channel = Channel::Admin;
    b
}
fn create(key: &str, content: ContentHash) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content,
        adopt_as_base: true,
    }
}
fn digests(store: &Store<SimDisk>) -> Vec<(String, ContentHash)> {
    let k = store.kernel().unwrap();
    k.branches()
        .map(|b| (b.name.clone(), k.branch(b.id).unwrap().digest()))
        .collect()
}

/// T59, store half.
#[test]
fn t59_a_shred_changes_nothing_but_the_payload() {
    let disk = SimDisk::new();
    let mut store = open(&disk);
    let a = store
        .put_blob(b"the zanzibar vendor holds the keys")
        .unwrap();
    let b = store.put_blob(b"rotate keys yearly").unwrap();
    store
        .commit(&Batch::new(vec![create("a", a), create("b", b)]))
        .unwrap();
    store
        .commit(&Batch::new(vec![Op::AddRule {
            conclusion: RevisionId(1).into(),
            incidences: vec![(RevisionId(0).into(), EdgeKind::Sem)],
            enabled: true,
        }]))
        .unwrap();
    let before = digests(&store);
    let status = store
        .kernel()
        .unwrap()
        .branch(MAIN)
        .unwrap()
        .node(RevisionId(1));

    // Refusals: the agent channel, a mixed batch, a dry run, a branch.
    let mut agent = shred(&[a]);
    agent.channel = Channel::Agent;
    assert!(matches!(
        store.commit(&agent),
        Err(StoreError::Tx(TxError::ChannelRestricted { .. }))
    ));
    let mut mixed = shred(&[a]);
    mixed.ops.push(create("c", b));
    assert_eq!(
        store.commit(&mixed),
        Err(StoreError::Tx(TxError::SoleOpRequired {
            op: "ShredContent".into()
        }))
    );
    assert!(matches!(
        store.kernel().unwrap().clone().what_if(&shred(&[a])),
        Err(TxError::ChannelRestricted { .. })
    ));

    let r = store.commit(&shred(&[a])).unwrap();
    assert!(r.status_diff.is_empty() && r.allocations == Allocations::default());
    assert_eq!(digests(&store), before, "no derivation changed");
    assert_eq!(
        store
            .kernel()
            .unwrap()
            .branch(MAIN)
            .unwrap()
            .node(RevisionId(1)),
        status
    );
    assert_eq!(
        store.get_blob(&a).unwrap(),
        Payload::Unavailable(Unavailable::Shredded)
    );
    assert_eq!(
        store.get_blob(&b).unwrap(),
        Payload::Available(b"rotate keys yearly".into())
    );
    // Re-sending the bytes, and a later op naming the hash.
    assert_eq!(
        store.put_blob(b"the zanzibar vendor holds the keys"),
        Err(StoreError::Tx(TxError::ContentShredded { content: a }))
    );
    assert_eq!(
        store.commit(&Batch::new(vec![Op::AddRevision {
            logical: LogicalId(1).into(),
            content: a,
            adopt_as_base: true,
        }])),
        Err(StoreError::Tx(TxError::ContentShredded { content: a }))
    );
    assert_eq!(
        store.commit(&Batch::new(vec![create("c", a)])),
        Err(StoreError::Tx(TxError::ContentShredded { content: a }))
    );
    // A restart (replay), then a snapshot and a restart from it.
    let epoch = store.epoch();
    drop(store);
    let mut store = open(&disk);
    assert_eq!((store.epoch(), digests(&store)), (epoch, before.clone()));
    assert_eq!(
        store.get_blob(&a).unwrap(),
        Payload::Unavailable(Unavailable::Shredded)
    );
    store.snapshot().unwrap();
    drop(store);
    let store = open(&disk);
    assert_eq!(
        store
            .kernel()
            .unwrap()
            .shredded()
            .keys()
            .copied()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([a])
    );
    assert_eq!(
        store.get_blob(&a).unwrap(),
        Payload::Unavailable(Unavailable::Shredded)
    );
    assert_eq!(digests(&store), before);
}

/// Stores the payload of each content field (synthetic hashes in the
/// generator) and rewrites the batch to name blake3 of it; `None` when a
/// payload was shredded.
fn with_blobs(store: &mut Store<SimDisk>, batch: &Batch) -> Option<Batch> {
    let mut b = batch.clone();
    for op in &mut b.ops {
        if let Op::CreateLogical { content, .. } | Op::AddRevision { content, .. } = op {
            *content = match store.put_blob(content) {
                Ok(h) => h,
                Err(StoreError::Tx(TxError::ContentShredded { .. })) => return None,
                Err(e) => panic!("{e:?}"),
            };
        }
    }
    Some(b)
}
fn contents(store: &Store<SimDisk>) -> BTreeSet<ContentHash> {
    let k = store.kernel().unwrap();
    let mut out = BTreeSet::new();
    for info in k.branches() {
        let v = k.branch(info.id).unwrap();
        for l in v.logicals().collect::<Vec<_>>() {
            if let Some(r) = v.head(l) {
                out.insert(v.revision_content(r).unwrap());
            }
        }
    }
    out
}
/// Every branch's inputs (epochs mapped through `map`), node states and
/// fork epoch: topology and statuses modulo the epoch renumbering.
type Fingerprint = Vec<(String, Epoch, Inputs, Vec<NodeState>)>;
fn fingerprint(store: &Store<SimDisk>, map: &dyn Fn(Epoch) -> Epoch) -> Fingerprint {
    let k = store.kernel().unwrap();
    k.branches()
        .map(|info| {
            let db = k.branch(info.id).unwrap().db();
            let mut inputs = db.inputs();
            for l in &mut inputs.logicals {
                l.origin_epoch = map(l.origin_epoch);
            }
            for r in &mut inputs.revisions {
                r.origin_epoch = map(r.origin_epoch);
            }
            for j in &mut inputs.rules {
                j.origin_epoch = map(j.origin_epoch);
            }
            for a in &mut inputs.artifacts {
                a.origin_epoch = map(a.origin_epoch);
                // A merge's canonical conflict payload names the merge
                // epoch (ADR 0013 §6.2), so its hash moves with the
                // renumbering; everything else about the artifact is kept.
                if a.ty == ArtifactType::Conflict {
                    a.payload_hash = [0; 32];
                }
            }
            let nodes = (0..db.revision_count())
                .map(|r| db.node(RevisionId(r as u32)).unwrap())
                .collect();
            (info.name.clone(), map(info.fork_epoch), inputs, nodes)
        })
        .collect()
}
/// A result without its epoch, which the shreds renumber.
fn unnumbered(r: &CommitResult) -> CommitResult {
    CommitResult {
        epoch: Epoch(0),
        ..r.clone()
    }
}

/// PROP16 (ADR 0017 §11): a history with shreds against the same history
/// without them. Each generated batch goes to both stores; a batch the
/// shredded store refuses for a shredded payload is dropped from both (the
/// generator never re-writes a shredded content). Results modulo the epoch,
/// every branch's digest, and replay (from the log, and half the seeds from
/// a snapshot) agree; payloads are `Shredded` for exactly the shredded
/// contents.
#[test]
fn prop16_shreds_change_nothing_but_the_shredded_payloads() {
    let seeds: u64 = std::env::var("CONTEXTDB_P9_SHRED_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let (mut shredded_total, mut dropped, mut multi) = (0usize, 0usize, 0usize);
    for seed in 0..seeds {
        let (da, db) = (SimDisk::new(), SimDisk::new());
        let (mut a, mut b) = (open(&da), open(&db));
        let mut g = Gen::new(seed ^ 0x16);
        let steps = 20 + g.pick(30);
        let snap_at = g.pick(steps + 1);
        let mut shredded: BTreeSet<ContentHash> = BTreeSet::new();
        let mut shred_epochs: Vec<Epoch> = Vec::new();
        for n in 0..steps {
            if n == snap_at && seed % 2 == 0 {
                a.snapshot().unwrap();
            }
            if g.coin(15) {
                let live: Vec<ContentHash> = contents(&a).difference(&shredded).copied().collect();
                if !live.is_empty() {
                    let pick: Vec<ContentHash> = (0..1 + g.pick(2))
                        .map(|_| live[g.pick(live.len())])
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    shred_epochs.push(a.commit(&shred(&pick)).unwrap().epoch);
                    shredded.extend(pick);
                    continue;
                }
            }
            let batch = g.step(a.kernel().unwrap());
            let Some(ba) = with_blobs(&mut a, &batch) else {
                dropped += 1;
                continue;
            };
            let ra = a.commit(&ba);
            if let Err(StoreError::Tx(TxError::ContentShredded { .. })) = ra {
                dropped += 1;
                continue;
            }
            let bb = with_blobs(&mut b, &batch).expect("nothing shredded in b");
            assert_eq!(ba, bb, "seed {seed}: same content addresses");
            let rb = b.commit(&bb);
            match (&ra, &rb) {
                (Ok(x), Ok(y)) => assert_eq!(unnumbered(x), unnumbered(y), "seed {seed}"),
                (Err(x), Err(y)) => assert_eq!(x, y, "seed {seed}"),
                _ => panic!("seed {seed}: {ra:?} vs {rb:?}"),
            }
        }
        let map = |e: Epoch| Epoch(e.0 - shred_epochs.iter().filter(|s| **s < e).count() as u64);
        let (fa, fb) = (fingerprint(&a, &map), fingerprint(&b, &|e| e));
        assert_eq!(fa.len(), fb.len(), "seed {seed}: branches");
        for (x, y) in fa.iter().zip(&fb) {
            let at = format!("seed {seed}, branch {}", x.0);
            assert_eq!((&x.0, x.1), (&y.0, y.1), "{at}");
            assert_eq!(x.2.logicals, y.2.logicals, "{at}: logicals");
            assert_eq!(x.2.revisions, y.2.revisions, "{at}: revisions");
            assert_eq!(x.2.heads, y.2.heads, "{at}: heads");
            assert_eq!(x.2.base, y.2.base, "{at}: base");
            assert_eq!(x.2.rules, y.2.rules, "{at}: rules");
            assert_eq!(x.2.validations, y.2.validations, "{at}: validations");
            assert_eq!(x.2.artifacts, y.2.artifacts, "{at}: artifacts");
            assert_eq!(x.2.metadata, y.2.metadata, "{at}: metadata");
            assert_eq!(x.2.watermarks, y.2.watermarks, "{at}: watermarks");
            assert_eq!(x.2.policy, y.2.policy, "{at}: policy");
            assert_eq!(x.3, y.3, "{at}: statuses");
        }
        if shred_epochs.is_empty() {
            assert_eq!(digests(&a), digests(&b), "seed {seed}");
        }
        multi += usize::from(digests(&a).len() > 1);
        let check = |s: &Store<SimDisk>| {
            assert_eq!(
                s.kernel()
                    .unwrap()
                    .shredded()
                    .keys()
                    .copied()
                    .collect::<BTreeSet<_>>(),
                shredded,
                "seed {seed}"
            );
            for c in contents(s) {
                let p = s.get_blob(&c).unwrap();
                if shredded.contains(&c) {
                    assert_eq!(
                        p,
                        Payload::Unavailable(Unavailable::Shredded),
                        "seed {seed}"
                    );
                } else {
                    assert!(matches!(p, Payload::Available(_)), "seed {seed}");
                }
            }
        };
        check(&a);
        for c in contents(&b) {
            assert!(matches!(b.get_blob(&c).unwrap(), Payload::Available(_)));
        }
        let before = digests(&a);
        drop(a);
        let a = open(&da);
        assert_eq!(digests(&a), before, "seed {seed}: replay");
        check(&a);
        shredded_total += shredded.len();
    }
    assert!(
        shredded_total > seeds as usize && multi > 0,
        "{shredded_total} {multi}"
    );
    eprintln!(
        "PROP16: {seeds} seeds, {shredded_total} shredded, {dropped} dropped, {multi} multi-branch"
    );
}

fn files_under(disk: &SimDisk, dir: &Path) -> Vec<(String, Vec<u8>)> {
    (disk.list(dir).unwrap_or_default().into_iter())
        .filter_map(|n| Some((n.clone(), disk.read(&dir.join(&n)).ok()?)))
        .collect()
}
fn holds(disk: &SimDisk, hash: &ContentHash) -> Vec<String> {
    files_under(disk, &Path::new(DIR).join("keys"))
        .into_iter()
        .filter(|(_, b)| b.windows(32).any(|w| w == hash))
        .map(|(n, _)| n)
        .collect()
}

/// T103, the key log (ADR 0017 §8.2): the rewrite stops after each of its
/// steps, then the power is cut under every tail fate.
/// - Before recovery, the durable disk is read without opening the store:
///   once the rewrite completed, no file in `keys/` holds the shredded
///   hash's key record; before that, `key_log_purged_through` never
///   claimed the shred.
/// - After recovery, the key is gone, the payload reads `Shredded`, other
///   payloads read back, and the purge covers the shred.
#[test]
fn t103_the_key_log_purge_survives_a_power_cut_at_every_step() {
    for step in [Some(0u8), Some(1), Some(2), Some(3), Some(4), None] {
        for seed in 0..6u64 {
            for fate in [TailFate::Torn, TailFate::ZeroFill, TailFate::OutOfOrder] {
                let disk = SimDisk::new();
                let mut store = open(&disk);
                let secret = store
                    .put_blob(b"the zanzibar vendor holds the keys")
                    .unwrap();
                let kept = store.put_blob(b"rotate keys yearly").unwrap();
                store
                    .commit(&Batch::new(vec![create("a", secret), create("b", kept)]))
                    .unwrap();
                assert!(!holds(&disk, &secret).is_empty(), "the key is on disk");
                store.stop_key_rewrite_at(step);
                let e = store.commit(&shred(&[secret])).unwrap().epoch;
                let done = store.key_log_purged_through() >= e;
                assert_eq!(done, step.is_none(), "{step:?}");
                drop(store);
                let after = disk.crash(fate, seed);
                if done {
                    assert_eq!(
                        holds(&after, &secret),
                        Vec::<String>::new(),
                        "{step:?} seed {seed} {fate:?}: before recovery"
                    );
                }
                let store = open(&after);
                assert_eq!(
                    holds(&after, &secret),
                    Vec::<String>::new(),
                    "{step:?} seed {seed} {fate:?}: after recovery"
                );
                assert!(store.key_log_purged_through() >= e);
                assert_eq!(
                    store.get_blob(&secret).unwrap(),
                    Payload::Unavailable(Unavailable::Shredded)
                );
                assert_eq!(
                    store.get_blob(&kept).unwrap(),
                    Payload::Available(b"rotate keys yearly".into()),
                    "{step:?} seed {seed} {fate:?}"
                );
                assert!(
                    !files_under(&after, &Path::new(DIR).join("keys"))
                        .iter()
                        .any(|(n, _)| n.ends_with(".tmp"))
                );
            }
        }
    }
}

/// G4 (ADR 0017 §11): a full disk during the key-log rewrite. Every write,
/// append, sync, rename and unlink of the rewrite fails in turn. The shred
/// commits (its log frame is durable before the rewrite starts), the purge
/// is not claimed, and the next open, after a restart or a power cut,
/// finishes it: the key is in no file, the payload reads `Shredded`, and
/// the other payload reads back.
#[test]
fn g4_a_full_disk_during_the_key_log_rewrite_is_finished_at_open() {
    let mut hit = 0;
    for op in ["write", "append", "sync", "sync_dir", "rename", "remove"] {
        for n in 0..64u32 {
            let disk = SimDisk::new();
            let mut store = open(&disk);
            let secret = store
                .put_blob(b"the zanzibar vendor holds the keys")
                .unwrap();
            let kept = store.put_blob(b"rotate keys yearly").unwrap();
            store
                .commit(&Batch::new(vec![create("a", secret), create("b", kept)]))
                .unwrap();
            disk.fail_nth(op, n);
            let Ok(r) = store.commit(&shred(&[secret])) else {
                // The failure hit the commit itself, before the rewrite.
                continue;
            };
            if store.key_log_purged_through() >= r.epoch {
                // The n-th such operation is past the rewrite.
                break;
            }
            hit += 1;
            drop(store);
            for power_cut in [false, true] {
                let after = if power_cut {
                    disk.crash(TailFate::Torn, u64::from(n))
                } else {
                    disk.restart()
                };
                let store = open(&after);
                assert!(store.key_log_purged_through() >= r.epoch, "{op}#{n}");
                assert_eq!(holds(&after, &secret), Vec::<String>::new(), "{op}#{n}");
                assert_eq!(
                    store.get_blob(&secret).unwrap(),
                    Payload::Unavailable(Unavailable::Shredded)
                );
                assert_eq!(
                    store.get_blob(&kept).unwrap(),
                    Payload::Available(b"rotate keys yearly".into()),
                    "{op}#{n} power_cut={power_cut}"
                );
            }
        }
    }
    assert!(hit >= 5, "the faults reached the rewrite only {hit} times");
}
