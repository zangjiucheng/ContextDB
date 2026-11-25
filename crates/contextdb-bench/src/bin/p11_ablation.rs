//! P11 milestone 5, the layout ablation's core metric set (ADR 0019 §6):
//! one shape per process, one output line.
//!
//! Usage: p11_ablation <multi_support|hub|chain> [n=1000000] [samples=1000] [E=LAZY_BUDGET]
//!
//! `E` exists so a small smoke run can defer its cascade; ablation runs
//! leave it at the default.
use contextdb_bench::p11::*;
use contextdb_core::{Budget, CommitStats, Counting, Db, Drain, LAZY_BUDGET};
use contextdb_model::*;
use contextdb_testkit::{Shape, shape};
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};
use std::{alloc::System, hint::black_box, time::Instant};

/// Live requested heap bytes, as `contextdb-bench` (P1) counts them.
#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;
fn live_heap() -> isize {
    let s = ALLOCATOR.stats();
    s.bytes_allocated as isize - s.bytes_deallocated as isize
}
const CASCADES: usize = 3;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: p11_ablation <multi_support|hub|chain> [n=1000000] [samples=1000] [E]";
    let name = args.get(1).map(String::as_str).unwrap_or_default();
    let kind = parse_shape(name).expect(usage);
    let arg = |i: usize, default: u64| args.get(i).map_or(default, |s| s.parse().expect(usage));
    let n = arg(2, 1_000_000) as usize;
    let samples = arg(3, 1000) as usize;
    let budget = Budget::new(arg(4, LAZY_BUDGET));
    assert!(samples >= 100 && n >= 16, "{usage}");
    let mut errors = Errors::default();

    // Allocated before the baseline, so they do not count against the db.
    let mut commit = Vec::with_capacity(samples);
    let mut why = Vec::with_capacity(samples);
    let baseline_heap = live_heap();
    let generated = shape(kind, n, 42);
    let stats = generated.stats.clone();
    let expected = expected_retract_flips(&generated);
    let mut db = Db::<Counting, CommitStats>::new();
    let load = Instant::now();
    for batch in &generated.batches {
        if errors.ok("load", db.commit(batch)).is_none() {
            errors.finish(&format!("p11_ablation shape={name} n={n}"));
        }
    }
    let load_ms = ms(load);
    drop(generated);
    black_box(db.derived()); // the snapshot cache P1's live heap includes

    // Small commits near the tail that change state: P1's choice for hub
    // and multi_support. In chain, revision r's rule needs r-1, so the
    // enabling half runs upward from n-8 to keep each commit a flip.
    let mut commit_flips = 0;
    let mut disabled = std::collections::BTreeMap::new();
    for step in 0..samples {
        let (k, enabling) = (step % 8, (step / 8) % 2 != 0);
        let r = match kind {
            Shape::Chain if enabling => RevisionId((n - 8 + k) as u32),
            _ => RevisionId((n - 1 - k) as u32),
        };
        let Some(&j) = db.indexes().rules_by_concl.row(r.index()).first() else {
            errors.fail("commit", format_args!("{r:?} concludes no rule"));
            continue;
        };
        if enabling {
            disabled.remove(&r);
        } else {
            disabled.insert(r, j);
        }
        let batch = Batch::new(vec![Op::SetRuleEnabled {
            rule: j.into(),
            enabled: enabling,
        }]);
        let begin = Instant::now();
        let result = db.commit(&batch);
        commit.push(begin.elapsed().as_nanos());
        if let Some(result) = errors.ok("commit", result) {
            commit_flips += result.l0_flips.len();
            if !matches!(kind, Shape::MultiSupport { .. }) {
                errors.check(
                    "commit work",
                    result.l0_flips == [r],
                    format_args!("step {step}: l0_flips {:?}, want [{r:?}]", result.l0_flips),
                );
            }
        }
        let begin = Instant::now();
        let answer = db.why_depth(r, 3);
        why.push(begin.elapsed().as_nanos());
        errors.ok("why_depth3", answer.map(black_box));
    }
    let heap = (live_heap() - baseline_heap) as f64 / stats.incidences as f64;
    // Back to the loaded state, untimed, so the cascades match the shape.
    for (_, j) in disabled {
        let batch = Batch::new(vec![Op::SetRuleEnabled {
            rule: j.into(),
            enabled: true,
        }]);
        errors.ok("re-enable", db.commit(&batch));
    }

    // Eager cascades: revision 0's base retracted (timed), then asserted.
    let mut eager = Vec::new();
    let mut eager_diff = None;
    let mut units = 0;
    for _ in 0..CASCADES {
        let t = Instant::now();
        let r = db.commit(&toggle(false));
        eager.push(ms(t));
        units = db.stats().units;
        if let Some(r) = errors.ok("eager retract", r) {
            check_flips(&mut errors, "eager retract", r.l0_flips.len(), expected);
            eager_diff.get_or_insert(r.status_diff);
        }
        restore(&mut db, &mut errors, expected);
    }
    let eager_diff = eager_diff.unwrap_or_default();

    // Lazy cascades with the default budget, drained to the end.
    let (mut ack, mut drain, mut total) = (Vec::new(), Vec::new(), Vec::new());
    let mut deferred = 0;
    for _ in 0..CASCADES {
        let t = Instant::now();
        let r = db.commit_lazy(&toggle(false), budget);
        let ack_ms = ms(t);
        let t = Instant::now();
        let settled = db.drain(Drain::Full);
        let drain_ms = ms(t);
        ack.push(ack_ms);
        drain.push(drain_ms);
        total.push(ack_ms + drain_ms);
        let Some(r) = errors.ok("lazy retract", r) else {
            continue;
        };
        let (flips, diff) = match settled {
            Some(s) => {
                deferred += 1;
                (s.l0_flips.len(), s.status_diff)
            }
            None => (r.l0_flips.len(), r.status_diff),
        };
        check_flips(&mut errors, "lazy retract", flips, expected);
        errors.check(
            "lazy status_diff",
            diff == eager_diff,
            format_args!("{} nodes, eager {}", diff.len(), eager_diff.len()),
        );
        errors.check(
            "lazy units",
            db.stats().units == units,
            format_args!("{} units, eager {units}", db.stats().units),
        );
        restore(&mut db, &mut errors, expected);
    }
    black_box(&db);
    errors.finish(&format!(
        "p11_ablation shape={name} n={n} samples={samples} E={} load_ms={load_ms:.0} live_heap_B_per_incidence={heap:.2} commit_p99_ns={} why_depth3_p99_ns={} cascade_eager_ms={:.1} cascade_lazy_ack_ms={:.2} cascade_lazy_drain_ms={:.1} cascade_lazy_total_ms={:.1} lazy_deferred={deferred}/{CASCADES} units={units} status_diff={} l0_flips={expected} incidences={} commit_flips={commit_flips}",
        budget.units,
        p99(&mut commit),
        p99(&mut why),
        median(&mut eager),
        median(&mut ack),
        median(&mut drain),
        median(&mut total),
        eager_diff.len(),
        stats.incidences,
    ));
}
fn check_flips(errors: &mut Errors, what: &str, got: usize, expected: usize) {
    errors.check(
        what,
        got == expected,
        format_args!("{got} l0 flips, the shape gives {expected}"),
    );
}
/// Asserts revision 0's base again, eagerly and untimed.
fn restore(db: &mut Db<Counting, CommitStats>, errors: &mut Errors, expected: usize) {
    if let Some(r) = errors.ok("restore", db.commit(&toggle(true))) {
        check_flips(errors, "restore", r.l0_flips.len(), expected);
    }
}
