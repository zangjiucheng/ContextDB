use contextdb_core::{CommitStats, Counting, Db};
use contextdb_model::*;
use contextdb_testkit::{Shape, shape};
use std::{hint::black_box, time::Instant};
fn percentile(values: &mut [u128]) -> u128 {
    values.sort_unstable();
    values[(values.len() * 99).div_ceil(100).saturating_sub(1)]
}
fn rss() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap()
        * 1024
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let name = args.get(1).map(String::as_str).unwrap_or("hub");
    let n: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(100_000);
    let samples: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(1000);
    assert!(samples >= 100);
    let kind = match name {
        "hub" => Shape::Hub,
        "multi_support" => Shape::MultiSupport { p_alt: 0.3 },
        _ => panic!("shape must be hub or multi_support"),
    };
    let baseline = rss();
    let generated = shape(kind, n, 42);
    let stats = generated.stats.clone();
    let mut db = Db::<Counting, CommitStats>::new();
    let load = Instant::now();
    for batch in &generated.batches {
        db.commit(batch).unwrap();
    }
    drop(generated);
    let load_ms = load.elapsed().as_millis();
    let mut early = Vec::with_capacity(samples);
    for step in 0..samples {
        let op = if step % 2 == 0 {
            Op::AssertBase {
                rev: RevisionId(3).into(),
            }
        } else {
            Op::RetractBase {
                rev: RevisionId(3).into(),
            }
        };
        let batch = Batch::new(vec![op]);
        black_box(db.derived()); // full snapshot cache is populated, outside timing
        let start = Instant::now();
        black_box(db.commit(&batch).unwrap());
        early.push(start.elapsed().as_nanos());
        assert_eq!(db.stats().incidences_visited, 0);
        assert_eq!(db.stats().status_nodes, vec![RevisionId(3)]);
    }
    let early = percentile(&mut early);
    println!("early_witness_only_commit_p99_ns={early}");
    let mut commit = Vec::with_capacity(samples);
    let mut current = Vec::with_capacity(samples);
    let mut why = Vec::with_capacity(samples);
    let mut max_visited = 0;
    // Last leaves in hub; near-tail bounded fan-out in multi_support. Each selected
    // update really changes support/base or canonical witness. No empty commits.
    for step in 0..samples {
        let r = RevisionId((n - 1 - step % 8) as u32);
        let j = db.indexes().rules_by_concl[r.index()][0];
        let op = Op::SetRuleEnabled {
            rule: j.into(),
            enabled: (step / 8) % 2 != 0,
        };
        let batch = Batch::new(vec![op]);
        let begin = Instant::now();
        black_box(db.commit(&batch).unwrap());
        commit.push(begin.elapsed().as_nanos());
        max_visited = max_visited.max(db.stats().incidences_visited);
        let begin = Instant::now();
        black_box(db.current_l0(LogicalId(r.0)).unwrap());
        current.push(begin.elapsed().as_nanos());
        let begin = Instant::now();
        black_box(db.why_depth(r, 3).unwrap());
        why.push(begin.elapsed().as_nanos());
    }
    let live_rss = rss();
    let memory = db.memory_footprint();
    println!(
        "retained_allocation_lower_bound={memory:?} lower_bound_B_per_incidence={:.2}",
        memory.lower_bound_bytes() as f64 / stats.incidences as f64
    );
    let commit = percentile(&mut commit);
    let current = percentile(&mut current);
    let why = percentile(&mut why);
    println!("shape={name} seed=42 stats={stats:?}");
    println!(
        "samples={samples} load_ms={load_ms} E_visited_max={max_visited} commit_p99_ns={commit} current_l0_p99_ns={current} why_depth3_p99_ns={why} baseline_rss_bytes={baseline} loaded_rss_bytes={live_rss} rss_delta_B_per_incidence={:.2}",
        (live_rss.saturating_sub(baseline)) as f64 / stats.incidences as f64
    );
    println!(
        "time_budgets_pass={} memory_budget={}",
        max_visited <= 100
            && early <= 1_000_000
            && commit <= 1_000_000
            && current <= 50_000
            && why <= 200_000,
        if memory.lower_bound_bytes() > 64 * stats.incidences {
            "FAIL_LOWER_BOUND"
        } else {
            "UNVERIFIED_LOWER_BOUND_ONLY"
        }
    );
    // Keep db alive until all measurements are printed.
    black_box(&db);
}
