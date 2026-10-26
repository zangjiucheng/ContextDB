//! P6 G5 (initial, ADR 0013 §11 and §3.1), on the M1 Pro:
//! - fork time (median and largest of 50) and live heap per fork at
//!   10³–10⁶ revisions (constant expected), and
//!   the first write after a fork, which copies the branch (§3.1, O(N));
//! - `WHAT_IF` p95 ≤ 2× the equivalent commit (in memory, no sync);
//! - a commit on a branch p95 ≤ 1.2× the same commit on `main`, the first
//!   (copying) write included, with 0 and 10⁴ private rules on shared
//!   premises;
//! - per-branch heap and the bytes a sync point retains;
//! - durable write latency through the actor, `expect_cert_valid` on/off.
//!
//! Usage: branch_bench [sizes=1000,10000,100000,1000000] [samples=1000] [cert_samples=300]
use contextdb_core::Kernel;
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, StdDisk, Store, session::SessionLog};
use contextdb_testkit::{Shape, shape};
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};
use std::{alloc::System, hint::black_box, time::Instant};

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;
fn live_heap() -> isize {
    let s = ALLOCATOR.stats();
    s.bytes_allocated as isize - s.bytes_deallocated as isize
}
fn pct(v: &mut [u128], p: usize) -> u128 {
    v.sort_unstable();
    v[(v.len() * p).div_ceil(100).saturating_sub(1)]
}
fn on(branch: BranchId, ops: Vec<Op>) -> Batch {
    let mut b = Batch::new(ops);
    b.branch = branch;
    b
}
/// The P1 hub shape with `n` revisions on `main`.
fn loaded(n: usize) -> Kernel {
    let mut k = Kernel::default();
    for b in &shape(Shape::Hub, n, 42).batches {
        k.commit(b).unwrap();
    }
    k
}
/// The P1 G5 commit: toggles a rule concluding one of the last leaves, so
/// each commit changes support or the canonical witness.
fn toggles(k: &Kernel, n: usize) -> Vec<Op> {
    let db = k.branch(MAIN).unwrap();
    let db = db.db();
    (0..16)
        .map(|step| {
            let r = RevisionId((n - 1 - step % 8) as u32);
            let j = db
                .indexes()
                .rules_by_concl
                .row(r.index())
                .first()
                .copied()
                .unwrap();
            Op::SetRuleEnabled {
                rule: j.into(),
                enabled: (step / 8) % 2 != 0,
            }
        })
        .collect()
}
/// p95 of `samples` commits of the toggle sequence on `branch`, from now.
fn commit_p95(k: &mut Kernel, branch: BranchId, ops: &[Op], samples: usize) -> (u128, u128) {
    let mut t = Vec::with_capacity(samples);
    for s in 0..samples {
        let b = on(branch, vec![ops[s % ops.len()].clone()]);
        let start = Instant::now();
        black_box(k.commit(&b).unwrap());
        t.push(start.elapsed().as_nanos());
    }
    let first = t[0];
    (pct(&mut t, 95), first)
}
/// 10⁴ rules on `branch` whose premises are the shared leaves the toggles
/// reach, each concluding a fresh fact of the branch.
fn private_rules(k: &mut Kernel, branch: BranchId, n: usize) {
    for chunk in 0..100 {
        let mut ops = Vec::new();
        for i in 0..100 {
            let id = chunk * 100 + i;
            ops.push(Op::CreateLogical {
                key: format!("private{id}"),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: [(id % 251) as u8; 32],
                adopt_as_base: false,
            });
            ops.push(Op::AddRule {
                conclusion: RevRef::New(i as u16),
                incidences: vec![(RevisionId((n - 1 - id % 8) as u32).into(), EdgeKind::Cur)],
                enabled: true,
            });
        }
        k.commit(&on(branch, ops)).unwrap();
    }
}

fn kernel_part(n: usize, samples: usize) -> bool {
    let mut pass = true;
    let mut k = loaded(n);
    let ops = toggles(&k, n);
    // Fork: 50 CreateBranch commits of `main`; the median and the largest
    // time, and the heap each fork adds.
    let heap = live_heap();
    let mut forks = Vec::new();
    for f in 0..50 {
        let start = Instant::now();
        k.commit(&Batch::new(vec![Op::CreateBranch {
            name: format!("b{f}"),
        }]))
        .unwrap();
        forks.push(start.elapsed().as_nanos());
    }
    let fork_heap = (live_heap() - heap) / 50;
    let fork_max_ns = *forks.iter().max().unwrap();
    let fork_ns = pct(&mut forks, 50);
    let b = k.branch_by_name("b0").unwrap();
    // First write on the branch copies it; then the branch's p95, with
    // that first write among the samples.
    let heap = live_heap();
    let (branch_p95, first_write_ns) = commit_p95(&mut k, b, &ops, samples);
    let branch_heap = live_heap() - heap;
    // `main` shares nothing with the branch any more; the same sequence on a
    // fresh copy of `main` that never forked is the reference.
    let mut reference = loaded(n);
    let (main_p95, _) = commit_p95(&mut reference, MAIN, &ops, samples);
    let ratio = branch_p95 as f64 / main_p95 as f64;
    // The budget's comparison in one kernel: after a fork, the branch and
    // `main` take the same commits alternately, each first write (a copy)
    // among the samples. Two kernels differ by heap layout at ~1 µs.
    let mut ab = loaded(n);
    ab.commit(&Batch::new(vec![Op::CreateBranch { name: "ab".into() }]))
        .unwrap();
    let ab_branch = ab.branch_by_name("ab").unwrap();
    let (mut tb, mut tm) = (Vec::new(), Vec::new());
    for s in 0..samples {
        for (branch, t) in [(ab_branch, &mut tb), (MAIN, &mut tm)] {
            let batch = on(branch, vec![ops[s % ops.len()].clone()]);
            let start = Instant::now();
            black_box(ab.commit(&batch).unwrap());
            t.push(start.elapsed().as_nanos());
        }
    }
    let (ib, im) = (pct(&mut tb, 95), pct(&mut tm, 95));
    let interleaved = ib as f64 / im as f64;
    pass &= interleaved <= 1.2;
    // WHAT_IF against the commit it previews, on `main`.
    let (mut dry, mut real) = (Vec::new(), Vec::new());
    for s in 0..samples {
        let b = Batch::new(vec![ops[s % ops.len()].clone()]);
        let start = Instant::now();
        black_box(reference.what_if(&b).unwrap());
        dry.push(start.elapsed().as_nanos());
        let start = Instant::now();
        black_box(reference.commit(&b).unwrap());
        real.push(start.elapsed().as_nanos());
    }
    let (dry_p95, real_p95) = (pct(&mut dry, 95), pct(&mut real, 95));
    let what_if_ratio = dry_p95 as f64 / real_p95 as f64;
    pass &= what_if_ratio <= 2.0;
    // A merge keeps both sides' states in the sync point: the next write on
    // each side copies, and the old states stay retained.
    let heap = live_heap();
    k.commit(&Batch::new(vec![Op::Merge {
        source: b,
        require_certified: false,
        certify: Vec::new(),
    }]))
    .unwrap();
    let start = Instant::now();
    k.commit(&on(MAIN, vec![ops[0].clone()])).unwrap();
    let post_merge_main_ns = start.elapsed().as_nanos();
    let start = Instant::now();
    k.commit(&on(b, vec![ops[1].clone()])).unwrap();
    let post_merge_branch_ns = start.elapsed().as_nanos();
    let sync_heap = live_heap() - heap;
    println!(
        "n={n} fork_p50_us={:.2} fork_max_us={:.1} fork_heap_B={fork_heap} first_write_us={:.1} branch_heap_after_{samples}_commits_B={branch_heap} \
         branch_commit_p95_us={:.1} unforked_main_commit_p95_us={:.1} branch_over_unforked_main={ratio:.3} branch_over_main_interleaved={interleaved:.3} (p95_ns {ib}/{im}) \
         what_if_p95_us={:.1} commit_p95_us={:.1} what_if_over_commit={what_if_ratio:.3} \
         merge_plus_two_writes_heap_B={sync_heap} post_merge_first_write_main_us={:.1} post_merge_first_write_branch_us={:.1}",
        fork_ns as f64 / 1e3,
        fork_max_ns as f64 / 1e3,
        first_write_ns as f64 / 1e3,
        branch_p95 as f64 / 1e3,
        main_p95 as f64 / 1e3,
        dry_p95 as f64 / 1e3,
        real_p95 as f64 / 1e3,
        post_merge_main_ns as f64 / 1e3,
        post_merge_branch_ns as f64 / 1e3,
    );
    // 10⁴ private rules on the shared premises the toggles reach: `main`'s
    // commit before and after the branch adds them, in one kernel.
    let mut k = loaded(n);
    k.commit(&Batch::new(vec![Op::CreateBranch { name: "b".into() }]))
        .unwrap();
    let b = k.branch_by_name("b").unwrap();
    commit_p95(&mut k, MAIN, &ops, samples); // main's first write copies
    let (main_before, _) = commit_p95(&mut k, MAIN, &ops, samples);
    private_rules(&mut k, b, n);
    let (main_p95_private, _) = commit_p95(&mut k, MAIN, &ops, samples);
    let (branch_p95_private, _) = commit_p95(&mut k, b, &ops, samples);
    let isolation = main_p95_private as f64 / main_before as f64;
    println!(
        "n={n} private_rules=10000 main_commit_p95_us={:.1} (before {:.1}) main_with_over_without={isolation:.3} \
         branch_commit_p95_us={:.1} (the branch propagates into its 10⁴ rules)",
        main_p95_private as f64 / 1e3,
        main_before as f64 / 1e3,
        branch_p95_private as f64 / 1e3,
    );
    pass &= isolation <= 1.2;
    pass
}

/// Merge cost against the history of merges: two branches exchange a fact
/// and merge both ways `merges` times; the p95 of the last 100 merges
/// against the first 100 (the merge base search must not grow with the
/// number of sync points), and the heap the kernel holds afterwards (sync
/// points keep only states that can still be bases).
fn merge_history(merges: usize) -> (u128, u128, isize) {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![Op::CreateLogical {
        key: "seed".into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [1; 32],
        adopt_as_base: true,
    }]))
    .unwrap();
    k.commit(&Batch::new(vec![Op::CreateBranch { name: "b".into() }]))
        .unwrap();
    let b = k.branch_by_name("b").unwrap();
    let mut t = Vec::with_capacity(merges);
    let heap = live_heap();
    for n in 0..merges {
        let (into, source) = if n % 2 == 0 { (MAIN, b) } else { (b, MAIN) };
        k.commit(&on(
            source,
            vec![Op::CreateLogical {
                key: format!("f{n}"),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: [(n % 251) as u8; 32],
                adopt_as_base: true,
            }],
        ))
        .unwrap();
        let start = Instant::now();
        k.commit(&on(
            into,
            vec![Op::Merge {
                source,
                require_certified: false,
                certify: Vec::new(),
            }],
        ))
        .unwrap();
        t.push(start.elapsed().as_nanos());
    }
    let retained = live_heap() - heap - (t.capacity() * 16) as isize;
    let (mut first, mut last) = (t[..100].to_vec(), t[t.len() - 100..].to_vec());
    (pct(&mut first, 95), pct(&mut last, 95), retained)
}

const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
fn call(s: &Service<StdDisk>, req: Request) -> Response {
    s.handle(AGENT, req).response
}
/// Durable `ctx_commit` latency of a session holding a certificate over
/// `reads` certified reads, with and without `expect_cert_valid`.
fn actor_part(reads: usize, samples: usize) {
    let dir = std::env::temp_dir().join(format!("ctxdb-branch-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let disk = StdDisk::default();
    let (store, _) = Store::open(
        disk.clone(),
        &dir,
        &OpenOptions {
            create: true,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let (log, events, _) = SessionLog::open(disk, &dir, false, 1 << 20).unwrap();
    let s = Service::new(actor::spawn(store).unwrap(), log, events).unwrap();
    let session = match call(
        &s,
        Request::SessionStart {
            harness: "bench".into(),
            repo: "r".into(),
            head_sha: "h".into(),
            parent: None,
            branch: None,
            git_branch: None,
        },
    ) {
        Response::Session { session } => session,
        other => panic!("{other:?}"),
    };
    let record = |key: &str| -> CommitResult {
        match call(
            &s,
            Request::Record {
                binding: Binding::root(session),
                kind: RecordKind::Fact,
                body: key.as_bytes().to_vec(),
                target: WriteTarget::Create {
                    key: key.into(),
                    scope_lane: "repo".into(),
                },
                deps: Vec::new(),
                adopt_as_base: true,
                client_txn_id: None,
            },
        ) {
            Response::Committed { result } => result,
            other => panic!("{other:?}"),
        }
    };
    for n in 0..reads {
        record(&format!("read{n}"));
    }
    let own = record("own").allocations.new_revisions[0];
    for n in 0..reads {
        match call(
            &s,
            Request::Read {
                binding: Binding::root(session),
                target: Target::Key(format!("read{n}")),
                mode: Mode::Certified,
            },
        ) {
            Response::Read { item, .. } => assert_eq!(item.summary, Summary::Certified),
            other => panic!("{other:?}"),
        }
    }
    // The session's certificate.
    match call(
        &s,
        Request::Check {
            binding: Binding::root(session),
            cert_id: None,
            live_pos: Vec::new(),
            action: None,
            mode: None,
            harness_version: String::new(),
            strict: false,
        },
    ) {
        Response::Check {
            validation: Validation::Valid | Validation::ValidRecertified { .. },
            ..
        } => {}
        other => panic!("{other:?}"),
    }
    for expect in [false, true] {
        let mut t = Vec::with_capacity(samples);
        for n in 0..samples {
            let op = if n % 2 == 0 {
                Op::RetractBase { rev: own.into() }
            } else {
                Op::AssertBase { rev: own.into() }
            };
            let start = Instant::now();
            let r = call(
                &s,
                Request::Commit {
                    binding: Binding::root(session),
                    ops: vec![op],
                    blobs: Vec::new(),
                    expect_cert_valid: expect,
                    client_txn_id: None,
                },
            );
            t.push(start.elapsed().as_nanos());
            assert!(matches!(r, Response::Committed { .. }), "{r:?}");
        }
        let (p50, p95, p99) = (pct(&mut t, 50), pct(&mut t, 95), pct(&mut t, 99));
        println!(
            "actor reads={reads} expect_cert_valid={expect} commit_p50_ms={:.2} p95_ms={:.2} p99_ms={:.2}",
            p50 as f64 / 1e6,
            p95 as f64 / 1e6,
            p99 as f64 / 1e6
        );
    }
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sizes: Vec<usize> = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("1000,10000,100000,1000000")
        .split(',')
        .map(|s| s.parse().unwrap())
        .collect();
    let samples: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(1000);
    let cert_samples: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(300);
    let mut pass = true;
    for &n in &sizes {
        pass &= kernel_part(n, samples);
    }
    let (first, last, heap) = merge_history(2000);
    let growth = last as f64 / first as f64;
    println!(
        "merge_history merges=2000 first100_p95_us={:.1} last100_p95_us={:.1} growth={growth:.2} kernel_heap_B={heap}",
        first as f64 / 1e3,
        last as f64 / 1e3
    );
    for reads in [10, 100] {
        actor_part(reads, cert_samples);
    }
    println!("p6_g5_kernel_pass={pass}");
}
