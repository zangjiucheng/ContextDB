//! P11 milestone 4, the store's payload GC (ADR 0019 §4, §5.1–§5.3, §5.5).
//!
//! - T67, store part: after `gc_apply` and again after `compact`, PROP10
//!   holds from a snapshot and from a full replay; every K1, K2 and K3
//!   content and every pinned one reads back (with a branch case); a
//!   collected content reads `Collected`; staging it again makes it
//!   readable, which survives collecting or shredding another hash and a
//!   reopen; `doctor` refuses a truncation below the GC cutoff.
//! - Failure and crash points of phase 2 and of the compaction.
//! - The version barrier on a format-6 log.
//! - PROP20, store part: random histories with real blobs, GC at random
//!   points and cutoffs, staging again and reopens.
#[path = "../../contextdb-testkit/tests/common/branch_gen.rs"]
mod branch_gen;
use branch_gen::Gen;
use contextdb_core::{EpochDiff, Import, Kernel};
use contextdb_model::*;
use contextdb_store::gc::{self, GcPlan, GcReport};
use contextdb_store::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const DIR: &str = "/db";
fn dir() -> &'static Path {
    Path::new(DIR)
}
fn open(disk: &SimDisk) -> Store<SimDisk> {
    Store::open(disk.clone(), dir(), &OpenOptions::default())
        .unwrap()
        .0
}
/// Stands in for the daemon's `ReviewDoc.meta` decoder: a document
/// `review:<hash>` names `<hash>`.
fn review_meta(b: &[u8]) -> Vec<ContentHash> {
    (b.strip_prefix(b"review:"))
        .and_then(|h| h.try_into().ok())
        .into_iter()
        .collect()
}
fn plan(s: &Store<SimDisk>, cutoff: Epoch) -> GcPlan {
    gc::gc_plan(
        s.kernel().unwrap(),
        &s.log_reader(),
        s.epoch(),
        cutoff,
        &review_meta,
    )
    .unwrap()
}
fn collect(s: &mut Store<SimDisk>, cutoff: Epoch, pinned: &[ContentHash]) -> GcReport {
    let p = plan(s, cutoff);
    let held = s.epoch();
    s.gc_apply(&p, &pinned.iter().copied().collect(), held, &review_meta)
        .unwrap()
}
fn create(key: &str, content: ContentHash) -> Op {
    Op::CreateLogical {
        origin: Default::default(),
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content,
        adopt_as_base: false,
    }
}
fn add(key: &str, content: ContentHash) -> Op {
    Op::AddRevision {
        logical: LogicalRef::Key(key.into()),
        content,
        adopt_as_base: false,
    }
}
/// Content rewrites: each key gets a new revision that becomes its head.
fn supersede(s: &Store<SimDisk>, branch: BranchId, rewrites: &[(&str, ContentHash)]) -> Batch {
    let v = s.kernel().unwrap().branch(branch).unwrap();
    let mut ops = Vec::new();
    for (n, (key, content)) in rewrites.iter().enumerate() {
        let l = v.logical_by_key(key).unwrap();
        ops.push(add(key, *content));
        ops.push(Op::SetHead {
            logical: l.into(),
            expected: v.head(l).unwrap().into(),
            new: RevRef::New(n as u16),
        });
    }
    on(branch, ops)
}
fn on(branch: BranchId, ops: Vec<Op>) -> Batch {
    let mut b = Batch::new(ops);
    b.branch = branch;
    b
}
fn head_content(s: &Store<SimDisk>, branch: BranchId, key: &str) -> ContentHash {
    let v = s.kernel().unwrap().branch(branch).unwrap();
    let l = v.logical_by_key(key).unwrap();
    v.revision_content(v.head(l).unwrap()).unwrap()
}
fn revision(s: &Store<SimDisk>, content: &ContentHash) -> RevisionId {
    let v = s.kernel().unwrap().branch(MAIN).unwrap();
    v.revisions_with_content(content)[0]
}
fn read(s: &Store<SimDisk>, c: &ContentHash) -> Payload {
    s.get_blob(c).unwrap()
}
fn is(p: Payload, bytes: &[u8]) -> bool {
    p == Payload::Available(bytes.to_vec())
}
const COLLECTED: Payload = Payload::Unavailable(Unavailable::Collected);
fn sorted(v: &[ContentHash]) -> Vec<ContentHash> {
    let mut v = v.to_vec();
    v.sort();
    v
}

type BranchState = (
    ContentHash,
    String,
    Option<BranchId>,
    Epoch,
    bool,
    Vec<(Import, Epoch)>,
    Vec<Result<EpochDiff, TxError>>,
);
/// PROP10's per-branch state (as `p6_prop10.rs`): the live kernel against
/// a full replay of the log.
fn branches(store: &Store<SimDisk>) -> Vec<BranchState> {
    let k = store.kernel().unwrap();
    let end = k.epoch();
    let replayed = store.log_reader().replay_kernel_to(end).unwrap();
    k.branches()
        .map(|info| {
            let view = k.branch(info.id).unwrap();
            assert_eq!(view.digest(), replayed.branch(info.id).unwrap().digest());
            let diffs = (0..=end.0)
                .map(|a| match Epoch(a) {
                    a if a >= view.initial_epoch() => view.diff(a, end),
                    a => replayed.branch(info.id).unwrap().diff(a, end),
                })
                .collect();
            (
                view.digest(),
                info.name.clone(),
                info.parent,
                info.fork_epoch,
                info.archived,
                view.imports().to_vec(),
                diffs,
            )
        })
        .collect()
}
fn files_under(disk: &SimDisk, sub: &str) -> Vec<(String, Vec<u8>)> {
    let d = dir().join(sub);
    (disk.list(&d).unwrap_or_default().into_iter())
        .filter_map(|n| Some((n.clone(), disk.read(&d.join(&n)).ok()?)))
        .collect()
}
fn bytes_under(disk: &SimDisk, sub: &str) -> usize {
    files_under(disk, sub).iter().map(|(_, b)| b.len()).sum()
}

/// The T67 history. Epochs: 1 creates a, c, p, d, f (and the orphan is
/// staged but never named); 2 supersedes a0, c0, p0; 3 pins a0 in a rule;
/// 4 opens a review naming meta `m`; 5 supersedes a1; 6 forks `x` (it
/// inherits d0, and its sync point keeps every head of then); 7
/// supersedes d0 on main and resolves the review with note `n`. The cutoff
/// is 7. In the window: 8 supersedes c1; 9 supersedes the inherited d0 on
/// `x`.
struct History {
    contents: BTreeMap<&'static str, (ContentHash, Vec<u8>)>,
    cutoff: Epoch,
}
impl History {
    fn c(&self, name: &str) -> ContentHash {
        self.contents[name].0
    }
    fn bytes(&self, name: &str) -> &[u8] {
        &self.contents[name].1
    }
    fn build(s: &mut Store<SimDisk>) -> Self {
        let mut contents = BTreeMap::new();
        let m = s.put_blob(b"validator meta").unwrap();
        contents.insert("m", (m, b"validator meta".to_vec()));
        let mut doc = b"review:".to_vec();
        doc.extend_from_slice(&m);
        let texts: [(&'static str, Vec<u8>); 15] = [
            ("a0", b"a v0".to_vec()),
            ("a1", b"a v1".to_vec()),
            ("a2", b"a v2".to_vec()),
            ("c0", b"c v0".to_vec()),
            ("c1", b"c v1".to_vec()),
            ("c2", b"c v2".to_vec()),
            ("p0", b"p v0".to_vec()),
            ("p1", b"p v1".to_vec()),
            ("d0", b"d v0".to_vec()),
            ("d1", b"d v1".to_vec()),
            ("dx", b"d on x".to_vec()),
            ("f", b"filler".to_vec()),
            ("orphan", b"staged, never named".to_vec()),
            ("doc", doc),
            ("n", b"resolution note".to_vec()),
        ];
        for (name, bytes) in texts {
            contents.insert(name, (s.put_blob(&bytes).unwrap(), bytes));
        }
        let h = |n: &str| contents[n].0;
        s.commit(&Batch::new(vec![
            create("a", h("a0")),
            create("c", h("c0")),
            create("p", h("p0")),
            create("d", h("d0")),
            create("f", h("f")),
        ]))
        .unwrap();
        let b = supersede(s, MAIN, &[("a", h("a1")), ("c", h("c1")), ("p", h("p1"))]);
        s.commit(&b).unwrap();
        assert_eq!(head_content(s, MAIN, "p"), h("p1"));
        let (a0, f) = (revision(s, &h("a0")), revision(s, &h("f")));
        s.commit(&Batch::new(vec![Op::AddRule {
            conclusion: f.into(),
            incidences: vec![(a0.into(), EdgeKind::Pin)],
            enabled: true,
        }]))
        .unwrap();
        let review = s
            .commit(&Batch::new(vec![Op::OpenArtifact {
                ty: ArtifactType::PendingReview,
                target: f.into(),
                payload_hash: h("doc"),
            }]))
            .unwrap()
            .allocations
            .new_artifacts[0];
        s.commit(&supersede(s, MAIN, &[("a", h("a2"))])).unwrap();
        s.commit(&Batch::new(vec![Op::CreateBranch { name: "x".into() }]))
            .unwrap();
        let x = s.kernel().unwrap().branch_by_name("x").unwrap();
        let mut b = supersede(s, MAIN, &[("d", h("d1"))]);
        b.ops.push(Op::ResolveArtifact {
            id: review.into(),
            note: Some(h("n")),
        });
        s.commit(&b).unwrap();
        let cutoff = s.epoch();
        assert_eq!(cutoff, Epoch(7));
        let b = supersede(s, MAIN, &[("c", h("c2"))]);
        s.commit(&b).unwrap();
        let b = supersede(s, x, &[("d", h("dx"))]);
        s.commit(&b).unwrap();
        assert_eq!(head_content(s, x, "d"), h("dx"));
        assert_eq!(head_content(s, MAIN, "d"), h("d1"));
        History { contents, cutoff }
    }
}

/// T67, store part.
#[test]
fn t67_gc_keeps_k1_to_k4_collects_the_rest_and_compacts() {
    let disk = SimDisk::new();
    let mut s = open(&disk);
    let h = History::build(&mut s);
    let before = branches(&s);
    let p = plan(&s, h.cutoff);
    assert_eq!(
        sorted(&p.candidates.iter().map(|c| c.content).collect::<Vec<_>>()),
        sorted(&[h.c("a1"), h.c("c0"), h.c("p0"), h.c("orphan")]),
        "phase 1"
    );
    assert!(p.kept_k1 > 0 && p.kept_k2 == 3 && p.kept_k3 > 0, "{p:?}");
    let pinned: BTreeSet<ContentHash> = [h.c("p0")].into();
    let r = s.gc_apply(&p, &pinned, s.epoch(), &review_meta).unwrap();
    let collected = sorted(&[h.c("a1"), h.c("c0"), h.c("orphan")]);
    assert_eq!(sorted(&r.collected), collected);
    assert_eq!((r.generation, r.rechecked_out, r.now), (1, 1, Epoch(9)));
    assert_eq!(s.gc_ledger().len(), 3);
    let kept = [
        "a0", "a2", "c1", "c2", "p0", "p1", "d0", "d1", "dx", "f", "doc", "m", "n",
    ];
    let check = |s: &Store<SimDisk>, collected: &[&str], live: &[&str]| {
        for name in live {
            assert!(is(read(s, &h.c(name)), h.bytes(name)), "{name} reads back");
        }
        let reader = s.log_reader();
        for name in collected {
            assert_eq!(read(s, &h.c(name)), COLLECTED, "{name}");
            assert_eq!(reader.read_blob(&h.c(name)), COLLECTED, "{name}");
            assert!(reader.is_collected(&h.c(name)));
        }
    };
    check(&s, &["a1", "c0", "orphan"], &kept);
    // PROP10: GC changed nothing the kernel or the log holds; then from a
    // snapshot and from a full replay.
    assert_eq!(branches(&s), before);
    s.snapshot().unwrap();
    drop(s);
    let s = open(&disk);
    assert_eq!(branches(&s), before, "from a snapshot");
    check(&s, &["a1", "c0", "orphan"], &kept);
    drop(s);
    for (name, _) in files_under(&disk, "snap") {
        disk.remove(&dir().join("snap").join(name)).unwrap();
    }
    let mut s = open(&disk);
    assert_eq!(branches(&s), before, "from a full replay");
    check(&s, &["a1", "c0", "orphan"], &kept);
    // Staging again makes a payload readable (an agent supersedes back to
    // the old text, ADR 0019 §5.3); it survives collecting one hash,
    // shredding another and a reopen.
    assert_eq!(s.put_blob(h.bytes("a1")).unwrap(), h.c("a1"));
    assert!(is(read(&s, &h.c("a1")), h.bytes("a1")));
    let (a1, a2) = (revision(&s, &h.c("a1")), revision(&s, &h.c("a2")));
    s.commit(&Batch::new(vec![Op::SetHead {
        logical: LogicalRef::Key("a".into()),
        expected: a2.into(),
        new: a1.into(),
    }]))
    .unwrap();
    let orphan2 = s.put_blob(b"another orphan").unwrap();
    let r2 = collect(&mut s, h.cutoff, &[h.c("p0")]);
    assert_eq!(r2.collected, vec![orphan2]);
    assert_eq!(r2.generation, 2);
    let mut shred = Batch::new(vec![Op::ShredContent { content: h.c("p0") }]);
    shred.channel = Channel::Admin;
    s.commit(&shred).unwrap();
    let after_gc = branches(&s);
    let gc_cutoff = h.cutoff;
    drop(s);
    let s = open(&disk);
    let live: Vec<&str> = kept
        .iter()
        .copied()
        .filter(|n| *n != "p0")
        .chain(["a1"])
        .collect();
    check(&s, &["c0", "orphan"], &live);
    assert_eq!(read(&s, &orphan2), COLLECTED);
    assert_eq!(
        read(&s, &h.c("p0")),
        Payload::Unavailable(Unavailable::Shredded)
    );
    assert_eq!(branches(&s), after_gc);
    drop(s);
    // doctor: a truncation below the highest cutoff is refused unless
    // forced; one at it is not.
    let refused = doctor::plan_truncate_before(&disk, dir(), Epoch(5)).unwrap();
    assert_eq!(refused.collected_cutoff, Some(gc_cutoff));
    assert_eq!(
        doctor::truncate_before(&disk, dir(), &refused),
        Err(StoreError::TruncationBelowCollected { cutoff: gc_cutoff })
    );
    let at = doctor::plan_truncate_before(&disk, dir(), Epoch(gc_cutoff.0 + 1)).unwrap();
    assert_eq!(at.collected_cutoff, None);

    // Compaction: an unloadable snapshot goes, the newest valid ones stay.
    let mut s = open(&disk);
    s.snapshot().unwrap();
    drop(s);
    disk.write_new(&dir().join("snap/snap-00000000000000000003.bin"), b"junk")
        .unwrap();
    let packs = bytes_under(&disk, "blobs");
    let report = gc::compact(disk.clone(), dir()).unwrap();
    assert!(report.segments_rewritten > 0, "{report:?}");
    // a1's first record, c0, the orphans, p0's shredded record.
    assert_eq!(report.records_dropped, 5, "{report:?}");
    assert_eq!(
        packs - bytes_under(&disk, "blobs"),
        report.bytes_reclaimed as usize
    );
    assert_eq!(
        (report.ledger_before, report.ledger_after),
        (4, 3),
        "a1, staged again, leaves the ledger"
    );
    assert_eq!(
        report.snapshots_removed,
        vec!["snap-00000000000000000003.bin"]
    );
    let s = open(&disk);
    assert_eq!(branches(&s), after_gc, "PROP10 after compaction");
    check(&s, &["c0", "orphan"], &live);
    assert_eq!(read(&s, &orphan2), COLLECTED);
    assert_eq!(s.gc_ledger().len(), 3);
    drop(s);
    // A second compaction finds nothing to drop.
    let again = gc::compact(disk.clone(), dir()).unwrap();
    assert_eq!((again.segments_rewritten, again.records_dropped), (0, 0));
}

/// Builds the T67 history and plans at its cutoff.
fn setup() -> (SimDisk, Store<SimDisk>, History, GcPlan) {
    let disk = SimDisk::new();
    let mut s = open(&disk);
    let h = History::build(&mut s);
    let p = plan(&s, h.cutoff);
    (disk, s, h, p)
}
/// Which of the history's contents read `Collected`; every other one
/// reads back.
fn collected_set(s: &Store<SimDisk>, h: &History) -> BTreeSet<&'static str> {
    let mut out = BTreeSet::new();
    for (name, (c, bytes)) in &h.contents {
        match read(s, c) {
            COLLECTED => {
                out.insert(*name);
            }
            p => assert!(is(p, bytes), "{name}"),
        }
    }
    out
}
fn full() -> BTreeSet<&'static str> {
    ["a1", "c0", "orphan", "p0"].into()
}

#[test]
fn a_failure_before_the_ledger_changes_nothing() {
    let (disk, mut s, h, p) = setup();
    let (keys, blobs) = (files_under(&disk, "keys"), files_under(&disk, "blobs"));
    // The first durable step creates `gc/`; its directory sync fails.
    disk.fail_next("sync_dir");
    assert!(matches!(
        s.gc_apply(&p, &BTreeSet::new(), s.epoch(), &review_meta),
        Err(StoreError::Io(_))
    ));
    assert!(!disk.exists(&dir().join("gc/ledger")));
    assert_eq!(
        (files_under(&disk, "keys"), files_under(&disk, "blobs")),
        (keys, blobs)
    );
    assert_eq!(collected_set(&s, &h), BTreeSet::new());
    // Not poisoned: it commits, and the retry collects.
    s.commit(&Batch::new(vec![create("z", h.c("f"))])).unwrap();
    let r = s
        .gc_apply(
            &plan(&s, h.cutoff),
            &BTreeSet::new(),
            s.epoch(),
            &review_meta,
        )
        .unwrap();
    assert_eq!(r.collected.len(), 4);
    assert_eq!(collected_set(&s, &h), full());
}

/// A complete ledger append, then the key-log rewrite (c) stops after each
/// of its steps: the store is poisoned. A restart, or a power cut under
/// every tail fate, reopens to the same collected set; the key log is the
/// old generation or the new one.
#[test]
fn a_crash_after_the_ledger_reopens_to_the_same_collected_set() {
    for step in [Some(0u8), Some(1), Some(2), Some(3), Some(4), None] {
        for seed in 0..4u64 {
            for fate in [TailFate::Torn, TailFate::ZeroFill, TailFate::OutOfOrder] {
                let (disk, mut s, h, p) = setup();
                let names = |d: &SimDisk| -> BTreeSet<String> {
                    (files_under(d, "keys").into_iter())
                        .map(|(n, _)| n)
                        .filter(|n| !n.ends_with(".tmp"))
                        .collect()
                };
                let old = names(&disk);
                s.stop_key_rewrite_at(step);
                let r = s.gc_apply(&p, &BTreeSet::new(), s.epoch(), &review_meta);
                assert_eq!(r.is_err(), step.is_some(), "{step:?}");
                if step.is_some() {
                    assert_eq!(s.put_blob(b"x"), Err(StoreError::Poisoned));
                    // From (b) on, readers see `Collected`.
                    assert_eq!(s.log_reader().read_blob(&h.c("a1")), COLLECTED);
                }
                drop(s);
                let durable = disk.crash(fate, seed);
                // The old generation, or one new segment numbered above it.
                let new: Vec<String> = names(&durable).difference(&old).cloned().collect();
                assert!(new.len() <= 1, "{step:?}: {new:?}");
                assert!(new.iter().all(|n| old.iter().all(|o| o < n)));
                for d in [durable, disk.restart()] {
                    let s = open(&d);
                    assert_eq!(collected_set(&s, &h), full(), "{step:?} {seed} {fate:?}");
                    drop(s);
                    // A second open has nothing left to apply.
                    let s = open(&d);
                    assert_eq!(collected_set(&s, &h), full());
                }
            }
        }
    }
}

/// The ledger's own sync fails after a complete append: poisoned, and a
/// restart applies the entries.
#[test]
fn a_ledger_sync_failure_poisons_and_reopen_applies_it() {
    let (disk, mut s, h, _) = setup();
    // A first collection creates `gc/`; the second's first sync is the
    // ledger's.
    let orphan = s.put_blob(b"first orphan").unwrap();
    collect(
        &mut s,
        h.cutoff,
        &[h.c("a1"), h.c("c0"), h.c("p0"), h.c("orphan")],
    );
    assert_eq!(read(&s, &orphan), COLLECTED);
    let p = plan(&s, h.cutoff);
    disk.fail_next("sync");
    assert_eq!(
        s.gc_apply(&p, &BTreeSet::new(), s.epoch(), &review_meta),
        Err(StoreError::Poisoned)
    );
    assert_eq!(s.commit(&Batch::new(vec![])), Err(StoreError::Poisoned));
    drop(s);
    let s = open(&disk.restart());
    assert_eq!(collected_set(&s, &h), full());
    assert_eq!(read(&s, &orphan), COLLECTED);
}

/// A kill during compaction leaves `compact/pack-N.new`; open removes it,
/// and the compaction runs again.
#[test]
fn a_kill_during_compaction_leaves_a_leftover_that_open_removes() {
    let (disk, mut s, h, p) = setup();
    s.gc_apply(&p, &BTreeSet::new(), s.epoch(), &review_meta)
        .unwrap();
    drop(s);
    let packs = files_under(&disk, "blobs");
    disk.fail_next("rename");
    assert!(gc::compact(disk.clone(), dir()).is_err());
    assert_eq!(files_under(&disk, "blobs"), packs, "blobs/ untouched");
    let leftover: Vec<String> = files_under(&disk, "compact")
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(leftover.len(), 1);
    assert!(leftover[0].starts_with("pack-") && leftover[0].ends_with(".new"));
    for d in [disk.restart(), disk.crash(TailFate::Torn, 3)] {
        let s = open(&d);
        assert!(files_under(&d, "compact").is_empty(), "open removed it");
        assert_eq!(collected_set(&s, &h), full());
    }
    let report = gc::compact(disk.clone(), dir()).unwrap();
    assert_eq!(report.records_dropped, 4);
    let s = open(&disk);
    assert_eq!(collected_set(&s, &h), full());
}

/// The version barrier (ADR 0019 §4): on a format-6 log, `gc_apply` alone
/// (no commit) makes the newest segment format 7, so a format-6 reader
/// refuses the store, and its refusal reads nothing under `keys/`, `gc/`
/// or `blobs/`.
#[test]
fn gc_apply_passes_the_version_barrier() {
    let disk = SimDisk::new();
    let texts: [&[u8]; 3] = [b"v0", b"v1", b"other"];
    let h: Vec<ContentHash> = texts.iter().map(|t| *blake3::hash(t).as_bytes()).collect();
    let batches = [
        Batch::new(vec![create("a", h[0]), create("b", h[2])]),
        Batch::new(vec![
            add("a", h[1]),
            Op::SetHead {
                logical: LogicalRef::Key("a".into()),
                expected: RevisionId(0).into(),
                new: RevRef::New(0),
            },
        ]),
    ];
    assert_eq!(migrate::write_format6(&disk, dir(), &batches).unwrap(), 2);
    let mut s = open(&disk);
    for t in texts {
        s.put_blob(t).unwrap();
    }
    let log_dir = dir().join("log");
    let formats = |d: &SimDisk| -> Vec<u32> {
        let mut segs = log::segments(d, &log_dir).unwrap();
        segs.sort();
        (segs.iter())
            .map(|p| log::parse_header(&d.read(p).unwrap()).unwrap().format)
            .collect()
    };
    assert_eq!(formats(&disk), vec![6]);
    let r = collect(&mut s, Epoch(2), &[]);
    assert_eq!(r.collected, vec![h[0]]);
    assert_eq!(formats(&disk), vec![6, 7], "rolled over before the ledger");
    assert_eq!(s.epoch(), Epoch(2), "no commit");
    drop(s);
    let durable = disk.crash(TailFate::Torn, 1);
    assert_eq!(formats(&durable), vec![6, 7], "durable");
    let state = |d: &SimDisk| {
        ["keys", "gc", "blobs", "log"]
            .map(|sub| files_under(d, sub))
            .to_vec()
    };
    let at = state(&durable);
    let old =
        log::scan_with_formats::<log::LogRow>(&log::OP_LOG, &durable, &log_dir, 6, 6).unwrap();
    assert_eq!(old.newer_format, Some(7));
    assert_eq!(state(&durable), at);
    let s = open(&durable);
    assert_eq!(read(&s, &h[0]), COLLECTED);
    assert!(is(read(&s, &h[1]), b"v1"));
}

/// The contents a branch's state keeps live: heads, bases and every rule
/// member.
fn live_contents(k: &Kernel) -> BTreeSet<ContentHash> {
    let mut out = BTreeSet::new();
    for info in k.branches() {
        let inputs = k.branch(info.id).unwrap().db().inputs();
        let c = |r: &RevisionId| inputs.revisions[r.0 as usize].content;
        out.extend(inputs.heads.iter().map(c));
        out.extend(
            (inputs.revisions.iter().zip(&inputs.base))
                .filter(|(_, b)| **b)
                .map(|(r, _)| r.content),
        );
        for j in &inputs.rules {
            out.insert(c(&j.conclusion));
            out.extend(j.premises.iter().map(|p| c(&p.premise)));
            out.extend(j.references.iter().map(c));
        }
    }
    out
}
/// Every content live at some epoch in `[cutoff, now]`, from one replay.
fn live_since(s: &Store<SimDisk>, cutoff: Epoch) -> BTreeSet<ContentHash> {
    let reader = s.log_reader();
    let mut k = Kernel::default();
    let mut out = BTreeSet::new();
    if cutoff.0 == 0 {
        out.extend(live_contents(&k));
    }
    for e in 1..=s.epoch().0 {
        let batch = reader.read_row(Epoch(e)).unwrap().batch().unwrap();
        k.commit(&batch).unwrap();
        if e >= cutoff.0 {
            out.extend(live_contents(&k));
        }
    }
    out
}
/// Stages the bytes of every collected content `batch` names through an
/// existing revision (the §5.4 rule; the kernel's check lands separately).
fn restage(s: &mut Store<SimDisk>, batch: &Batch, bytes: &BTreeMap<ContentHash, Vec<u8>>) -> usize {
    let k = s.kernel().unwrap();
    let Some(view) = k.branch(batch.branch) else {
        return 0;
    };
    let mut named = Vec::new();
    for op in &batch.ops {
        let mut push = |r: &RevRef| {
            if let RevRef::Existing(r) = r {
                named.extend(view.revision_content(*r).ok());
            }
        };
        match op {
            Op::SetHead { new, .. } => push(new),
            Op::AssertBase { rev } => push(rev),
            Op::AddRule {
                conclusion,
                incidences,
                ..
            } => {
                push(conclusion);
                incidences.iter().for_each(|(r, _)| push(r));
            }
            _ => {}
        }
    }
    let mut n = 0;
    for c in named {
        if read(s, &c) == COLLECTED {
            s.put_blob(&bytes[&c]).unwrap();
            n += 1;
        }
    }
    n
}

/// PROP20, store part (ADR 0019 §5): random multi-branch histories with
/// real blobs, GC at random points with random cutoffs, staging again and
/// reopens. Every content live at some epoch in `[cutoff, now]` (K1, and
/// K3 by its meaning) that read back before a GC reads back after it and
/// after a reopen; every content live at the end reads back, since each
/// one made live again had its bytes staged again.
#[test]
fn prop20_gc_never_collects_a_kept_content() {
    let seeds: u64 = std::env::var("CONTEXTDB_P11_GC_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000);
    let (mut collected, mut restaged, mut gcs, mut multi) = (0usize, 0usize, 0usize, 0usize);
    for seed in 0..seeds {
        let disk = SimDisk::new();
        let mut s = open(&disk);
        let mut g = Gen::new(seed ^ 0x20);
        let mut bytes: BTreeMap<ContentHash, Vec<u8>> = BTreeMap::new();
        let steps = 20 + g.pick(30);
        for _ in 0..steps {
            if g.coin(15) && s.epoch().0 > 0 {
                let now = s.epoch();
                let cutoff = Epoch(g.pick(now.0 as usize + 1) as u64);
                let keep = live_since(&s, cutoff);
                let readable: Vec<ContentHash> = (keep.iter())
                    .filter(|c| matches!(read(&s, c), Payload::Available(_)))
                    .copied()
                    .collect();
                let r = collect(&mut s, cutoff, &[]);
                for c in &r.collected {
                    assert!(!keep.contains(c), "seed {seed}: collected a kept content");
                    assert_eq!(read(&s, c), COLLECTED, "seed {seed}");
                }
                for c in &readable {
                    assert!(is(read(&s, c), &bytes[c]), "seed {seed}");
                }
                collected += r.collected.len();
                gcs += 1;
                if g.coin(30) {
                    drop(s);
                    s = open(&disk);
                    for c in &readable {
                        assert!(is(read(&s, c), &bytes[c]), "seed {seed}: reopen");
                    }
                }
                continue;
            }
            let mut batch = g.step(s.kernel().unwrap());
            for op in &mut batch.ops {
                if let Op::CreateLogical { content, .. } | Op::AddRevision { content, .. } = op {
                    let raw = content.to_vec();
                    *content = s.put_blob(&raw).unwrap();
                    bytes.insert(*content, raw);
                }
            }
            restaged += restage(&mut s, &batch, &bytes);
            let _ = s.commit(&batch);
        }
        let check = |s: &Store<SimDisk>| {
            for c in live_contents(s.kernel().unwrap()) {
                if let Some(b) = bytes.get(&c) {
                    assert!(is(read(s, &c), b), "seed {seed}: live content readable");
                }
            }
        };
        check(&s);
        multi += usize::from(s.kernel().unwrap().branches().count() > 1);
        drop(s);
        check(&open(&disk));
    }
    assert!(
        collected > 0 && gcs > 0 && multi > 0,
        "{collected} {gcs} {multi}"
    );
    eprintln!(
        "PROP20: {seeds} seeds, {gcs} collections, {collected} collected, {restaged} staged again, {multi} multi-branch"
    );
}

/// ADR 0019 §5.4 through the store's presence: a SetHead back to a
/// collected revision refuses with `MissingBlob`, on the WHAT_IF as on the
/// commit, until the bytes are staged again.
#[test]
fn reviving_a_collected_revision_needs_its_bytes_again() {
    let disk = SimDisk::new();
    let mut s = open(&disk);
    let v1 = s.put_blob(b"one").unwrap();
    s.commit(&on(MAIN, vec![create("k", v1)])).unwrap();
    let v2 = s.put_blob(b"two").unwrap();
    let next = supersede(&s, MAIN, &[("k", v2)]);
    s.commit(&next).unwrap();
    let now = s.epoch();
    assert!(collect(&mut s, now, &[]).collected.contains(&v1));
    assert_eq!(read(&s, &v1), COLLECTED);
    let v = s.kernel().unwrap().branch(MAIN).unwrap();
    let l = v.logical_by_key("k").unwrap();
    let back = on(
        MAIN,
        vec![Op::SetHead {
            logical: l.into(),
            expected: v.head(l).unwrap().into(),
            new: revision(&s, &v1).into(),
        }],
    );
    let missing = Err(StoreError::Tx(TxError::MissingBlob { content: v1 }));
    assert_eq!(s.what_if_detailed(&back).map(|_| ()), missing);
    assert_eq!(s.commit(&back).map(|_| ()), missing);
    assert_eq!(s.put_blob(b"one").unwrap(), v1);
    s.what_if_detailed(&back).unwrap();
    s.commit(&back).unwrap();
    assert!(is(read(&s, &v1), b"one"));
}
