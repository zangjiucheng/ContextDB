use contextdb_core::{CommitStats, Counting, Db, FullRecompute, NaiveTopo};
use contextdb_model::*;
use contextdb_oracle::{InputModel, jacobi, kahn};
use contextdb_testkit::generate_p1;
use std::collections::BTreeSet;
#[test]
fn prop1_through_prop9_10000_chacha8_seeds() {
    let mut totals = [0u64; 7];
    for seed in 0..10_000 {
        let trace = generate_p1(seed);
        assert_eq!(trace, generate_p1(seed));
        let mut db = Db::<Counting, CommitStats>::new();
        let mut replay = Db::<Counting, CommitStats>::new();
        let mut full = Db::<FullRecompute, CommitStats>::new();
        let mut oracle = InputModel::default();
        for (tx, batch) in trace.batches.iter().enumerate() {
            let before = oracle.inputs().clone();
            let prev = jacobi(&before);
            let digest = db.digest();
            let epoch = db.epoch();
            let pk = db.dynamic_topo().clone();
            let stats = db.stats().clone();
            let want = protect(seed, tx, &trace, || oracle.apply(batch));
            let actual = protect(seed, tx, &trace, || db.commit(batch));
            let again = protect(seed, tx, &trace, || replay.commit(batch));
            let rebuilt = protect(seed, tx, &trace, || full.commit(batch));
            let check = || {
                assert_eq!(actual, again);
                assert_eq!(db.stats(), replay.stats());
                assert_eq!(actual, rebuilt);
                assert_eq!(
                    actual.as_ref().map(|a| ApplyResult {
                        epoch: a.epoch,
                        allocations: a.allocations.clone()
                    }),
                    want.as_ref().cloned()
                );
                assert_eq!(db.inputs(), *oracle.inputs());
                let fixed = jacobi(oracle.inputs());
                assert_eq!(kahn(oracle.inputs()).unwrap(), fixed);
                assert_eq!(db.derived(), &fixed);
                assert_eq!(full.derived(), &fixed);
                assert!(db.dynamic_topo().valid());
                assert!(NaiveTopo::build(oracle.inputs(), 0).is_ok());
                let inputs = oracle.inputs();
                assert!(
                    inputs.logicals.len() <= 6
                        && inputs.revisions.len() <= 14
                        && inputs.rules.len() <= 10
                );
                assert!((1..=5).contains(&batch.ops.len()));
                for n in &fixed.nodes {
                    assert!(!n.l2 || n.l1);
                    assert!(!n.l1 || n.l0);
                }
                if let Ok(result) = &actual {
                    let flips: Vec<_> = fixed
                        .nodes
                        .iter()
                        .enumerate()
                        .filter(|(n, v)| v.l0 != prev.nodes.get(*n).is_some_and(|old| old.l0))
                        .map(|(n, _)| RevisionId(n as u32))
                        .collect();
                    assert_eq!(result.l0_flips, flips);
                    let diff: Vec<_> = fixed
                        .nodes
                        .iter()
                        .enumerate()
                        .filter(|(n, v)| prev.nodes.get(*n) != Some(*v))
                        .map(|(n, _)| RevisionId(n as u32))
                        .collect();
                    assert_eq!(result.status_diff, diff);
                    // PROP3: support-graph reachability from actual seeds, independent traversal.
                    let mut reached: BTreeSet<_> = db.stats().seed_nodes.iter().copied().collect();
                    loop {
                        let mut next = reached.clone();
                        for j in &inputs.rules {
                            if j.premises.iter().any(|p| reached.contains(&p.premise)) {
                                next.insert(j.conclusion);
                            }
                        }
                        if next == reached {
                            break;
                        }
                        reached = next;
                    }
                    assert!(db.stats().popped_nodes.iter().all(|r| reached.contains(r)));
                    let mixed = result.net_delta.head_moves.iter().any(|(_, old, new)| {
                        result
                            .net_delta
                            .base_flips
                            .iter()
                            .any(|r| Some(*r) == *old || r == new)
                    });
                    totals[1] += u64::from(mixed);
                    let saved = inputs
                        .rules
                        .iter()
                        .enumerate()
                        .take(before.rules.len())
                        .any(|(n, j)| {
                            prev.active[n]
                                && !fixed.active[n]
                                && prev.nodes[j.conclusion.index()].l0
                                && fixed.nodes[j.conclusion.index()].l0
                                && fixed.nodes[j.conclusion.index()].support > 0
                        });
                    totals[2] += u64::from(saved);
                    let inserted = result.net_delta.new_rules.iter().any(|j| {
                        inputs.rules[j.index()]
                            .premises
                            .iter()
                            .any(|p| p.premise.index() >= before.revisions.len())
                    });
                    totals[5] += u64::from(inserted);
                    totals[6] += u64::from(db.stats().pk_slow_paths > 0);
                } else {
                    assert_eq!(db.digest(), digest);
                    assert_eq!(db.epoch(), epoch);
                    assert_eq!(db.dynamic_topo(), &pk);
                    assert_eq!(db.stats(), &stats);
                }
            };
            if let Err(failure) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(check)) {
                std::fs::create_dir_all("../../results").unwrap();
                std::fs::write(
                    format!("../../results/p1-failure-{seed}.jsonl"),
                    trace.to_jsonl(),
                )
                .unwrap();
                eprintln!("seed {seed} tx {tx}");
                std::panic::resume_unwind(failure);
            }
            totals[0] += 1;
            totals[3] += u64::from(matches!(actual, Err(TxError::CycleDetected { .. })));
            totals[4] += u64::from(matches!(actual, Err(TxError::HeadConflict { .. })));
        }
    }
    for (n, percent) in [(1, 5), (2, 5), (3, 3), (4, 3), (5, 3), (6, 2)] {
        assert!(
            totals[n] * 100 >= totals[0] * percent,
            "coverage {n}: {totals:?}"
        );
    }
    eprintln!(
        "P1 coverage [tx,mixed-head-base,alternative-save,cycle,CAS,new-premise-rule,PK-slow]={totals:?}"
    );
}

fn protect<R>(seed: u64, tx: usize, trace: &EventLog, f: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(failure) => {
            std::fs::create_dir_all("../../results").unwrap();
            std::fs::write(
                format!("../../results/p1-failure-{seed}.jsonl"),
                trace.to_jsonl(),
            )
            .unwrap();
            eprintln!("seed {seed} tx {tx}");
            std::panic::resume_unwind(failure)
        }
    }
}
