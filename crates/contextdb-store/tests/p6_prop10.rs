//! PROP10 per branch (ADR 0013 §11 G3): reopening a multi-branch store from
//! the op_log, with or without a snapshot, rebuilds every branch exactly as
//! the incrementally maintained kernel had it: digest, branch record,
//! imports, and the branch's DIFF from every epoch to the tail (below the
//! kernel's first retained epoch through log replay, as the daemon reads).
#[path = "../../contextdb-testkit/tests/common/branch_gen.rs"]
mod branch_gen;
use branch_gen::Gen;
use contextdb_core::{EpochDiff, Import};
use contextdb_model::*;
use contextdb_store::*;
use std::path::Path;

const DIR: &str = "/db";
fn open(disk: &SimDisk) -> (Store<SimDisk>, OpenReport) {
    Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).unwrap()
}
fn with_blobs(store: &mut Store<SimDisk>, batch: &Batch) -> Batch {
    let mut b = batch.clone();
    for op in &mut b.ops {
        if let Op::CreateLogical { content, .. } | Op::AddRevision { content, .. } = op {
            *content = store.put_blob(content).unwrap();
        }
    }
    b
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
fn branches(store: &Store<SimDisk>) -> Vec<BranchState> {
    let k = store.kernel().unwrap();
    let end = k.epoch();
    let replayed = store.log_reader().replay_kernel_to(end).unwrap();
    k.branches()
        .map(|info| {
            let view = k.branch(info.id).unwrap();
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

#[test]
fn prop10_every_branch_rebuilds_from_the_op_log() {
    let seeds: u64 = std::env::var("CONTEXTDB_P6_PROP10_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    let (mut multi, mut snapshots) = (0, 0);
    for seed in 0..seeds {
        let disk = SimDisk::new();
        let (mut store, _) = open(&disk);
        let mut g = Gen::new(seed ^ 0x10);
        let steps = 20 + g.pick(30);
        let snap_at = g.pick(steps + 1);
        for n in 0..steps {
            if n == snap_at && seed % 2 == 0 {
                store.snapshot().unwrap();
                snapshots += 1;
            }
            let b = g.step(store.kernel().unwrap());
            let b = with_blobs(&mut store, &b);
            let _ = store.commit(&b);
        }
        let (epoch, before) = (store.epoch(), branches(&store));
        multi += usize::from(before.len() > 1);
        drop(store);
        let (store, _) = open(&disk);
        assert_eq!(store.epoch(), epoch, "seed {seed}");
        assert_eq!(branches(&store), before, "seed {seed}");
    }
    assert!(
        multi as u64 * 2 > seeds && snapshots > 0,
        "{multi} {snapshots}"
    );
}
