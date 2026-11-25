//! PROP1-lazy (ADR 0019 §9 G3, SPEC §8.2): the P1 suite with cascade-bounded
//! maintenance, in lockstep with eager `Counting`, `FullRecompute` and the
//! oracle. Each step draws the budget `E` from {1, 2, 3, 8, 64, ∞} and the
//! chunk `C` from {1, 2, 4096}, then drains in random slices (`Units`,
//! `Rank`) before the next commit. At every point, for every node:
//! - settled ⇒ its state equals the oracle's final state (so `settled` is
//!   sound) and its history equals eager history at every epoch;
//! - unsettled ⇒ it reads `PENDING(StructuralUnknown)`, or a higher-priority
//!   state the oracle also gives.
//!
//! After the full drain the drained diffs, derived state, digest, history
//! and stats equal eager.
use contextdb_core::{Budget, CommitStats, Counting, Db, Drain, FullRecompute, summarize};
use contextdb_model::*;
use contextdb_oracle::{InputModel, jacobi};
use contextdb_testkit::generate_p1;

const SEEDS: u64 = 10_000;
const BUDGETS: [u64; 6] = [1, 2, 3, 8, 64, u64::MAX];
const CHUNKS: [usize; 3] = [1, 2, 4096];

/// SplitMix64: the schedule's own deterministic randomness.
struct Mix(u64);
impl Mix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

type Eager = Db<Counting, CommitStats>;

/// Every node's reads against the oracle's final state and eager history.
fn check_reads(lazy: &Eager, eager: &Eager, fixed: &DerivedSnapshot, inputs: &Inputs) -> usize {
    let mut unsettled = 0;
    let epoch = lazy.epoch();
    for (n, want) in fixed.nodes.iter().enumerate() {
        let r = RevisionId(n as u32);
        let got = lazy.node(r).unwrap();
        if lazy.settled(r) {
            assert_eq!(&got, want, "settled {r:?} reads its final state");
            for e in eager.initial_epoch().0..=epoch.0 {
                assert_eq!(
                    lazy.read_historical(r, Epoch(e)),
                    eager.read_historical(r, Epoch(e)),
                    "history of settled {r:?} at {e}"
                );
            }
            let (head, l0) = lazy.current_l0(inputs.revisions[n].logical).unwrap();
            if head == r {
                assert_eq!(l0, Some(want.l0));
            }
        } else {
            unsettled += 1;
            let summary = summarize(&got.dimensions);
            let oracle = summarize(&want.dimensions);
            assert!(
                summary == Summary::Pending(PendingReason::StructuralUnknown) || summary == oracle,
                "unsettled {r:?} reads {summary:?}, oracle {oracle:?}"
            );
            assert_eq!(got.dimensions.structural, Structural::Unknown);
            assert_eq!(got.dimensions.conflicted, want.dimensions.conflicted);
            assert_eq!(got.dimensions.currency, want.dimensions.currency);
            assert_eq!(lazy.why(r), Err(TxError::Unsettled));
            let (head, l0) = lazy.current_l0(inputs.revisions[n].logical).unwrap();
            if head == r {
                assert_eq!(l0, None);
            }
        }
    }
    // A logical's head at the frontier's epoch comes from the inputs.
    for (l, head) in inputs.heads.iter().enumerate() {
        let (r, _) = lazy.read_snapshot(LogicalId(l as u32), epoch).unwrap();
        assert_eq!(r, *head);
    }
    unsettled
}

#[test]
fn prop1_lazy_10000_seeds() {
    // [steps, deferred commits, slices, rank drains, unsettled reads]
    let mut totals = [0u64; 5];
    let seeds = std::env::var("PROP1_LAZY_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SEEDS);
    for seed in 0..seeds {
        let trace = generate_p1(seed);
        let mut mix = Mix(seed ^ 0x5011_0019);
        let mut eager = Eager::new();
        let mut lazy = Eager::new();
        let mut full = Db::<FullRecompute, CommitStats>::new();
        let mut oracle = InputModel::default();
        for (tx, batch) in trace.batches.iter().enumerate() {
            let run = || {
                let budget = Budget {
                    units: BUDGETS[mix.below(6) as usize],
                    chunk: CHUNKS[mix.below(3) as usize],
                };
                let want = eager.commit(batch);
                let _ = oracle.apply(batch);
                let rebuilt = full.commit(batch);
                let got = lazy.commit_lazy(batch, budget);
                assert_eq!(got.as_ref().err(), want.as_ref().err());
                assert_eq!(rebuilt.as_ref().err(), want.as_ref().err());
                let Ok(want) = want else {
                    assert!(!lazy.has_frontier());
                    assert_eq!(lazy.digest(), eager.digest());
                    return;
                };
                let got = got.unwrap();
                assert_eq!(got.epoch, want.epoch);
                assert_eq!(got.allocations, want.allocations);
                assert_eq!(got.net_delta, want.net_delta);
                assert_eq!(lazy.inputs(), *oracle.inputs());
                let fixed = jacobi(oracle.inputs());
                let settled = if lazy.has_frontier() {
                    totals[1] += 1;
                    assert!(got.l0_flips.is_empty() && got.status_diff.is_empty());
                    assert_eq!(lazy.frontier_epoch(), Some(want.epoch));
                    loop {
                        totals[4] += check_reads(&lazy, &eager, &fixed, oracle.inputs()) as u64;
                        // I-L1: no batch or dry run enters an unsettled kernel
                        // (debug builds panic; checked in the goldens).
                        let how = match mix.below(4) {
                            0 => {
                                totals[3] += 1;
                                let cursor = lazy.cursor().unwrap();
                                Drain::Rank(cursor + mix.below(4) as usize)
                            }
                            1 => Drain::Full,
                            _ => Drain::Units(BUDGETS[mix.below(5) as usize]),
                        };
                        totals[2] += 1;
                        let done = lazy.drain(how);
                        if let Drain::Rank(k) = how
                            && done.is_none()
                        {
                            for n in 0..fixed.nodes.len() {
                                let r = RevisionId(n as u32);
                                if lazy.dynamic_topo().rank(&CycleNode::Revision(r)) <= k {
                                    assert!(lazy.settled(r), "{r:?} at or below {k}");
                                }
                            }
                        }
                        if let Some(done) = done {
                            break done;
                        }
                    }
                } else {
                    contextdb_core::Settled {
                        epoch: got.epoch,
                        l0_flips: got.l0_flips.clone(),
                        status_diff: got.status_diff.clone(),
                    }
                };
                assert!(!lazy.has_frontier());
                assert_eq!(settled.epoch, want.epoch);
                assert_eq!(settled.l0_flips, want.l0_flips);
                assert_eq!(settled.status_diff, want.status_diff);
                assert_eq!(rebuilt.unwrap().status_diff, want.status_diff);
                assert_eq!(lazy.derived(), &fixed);
                assert_eq!(full.derived(), &fixed);
                assert_eq!(lazy.digest(), eager.digest());
                assert_eq!(lazy.stats(), eager.stats());
                assert_eq!(check_reads(&lazy, &eager, &fixed, oracle.inputs()), 0);
                for from in 0..=want.epoch.0 {
                    assert_eq!(
                        lazy.diff(Epoch(from), want.epoch),
                        eager.diff(Epoch(from), want.epoch)
                    );
                }
            };
            if let Err(failure) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
                eprintln!("PROP1-lazy seed {seed} tx {tx}");
                std::panic::resume_unwind(failure);
            }
            totals[0] += 1;
        }
    }
    eprintln!("PROP1-lazy [steps, deferred, slices, rank drains, unsettled reads]={totals:?}");
    // The schedule must exercise deferral, slicing and unsettled reads.
    assert!(totals[1] * 5 >= totals[0], "{totals:?}");
    assert!(totals[3] > 0 && totals[4] > 0, "{totals:?}");
}
