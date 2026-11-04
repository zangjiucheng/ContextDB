//! P7 G3 properties (ADR 0015 §11).
//!
//! - **PROP1 with artifact Ops and `meta`/`if_absent`.** `generate_p7`
//!   traces (Conflict and PendingReview opens and resolves, notes,
//!   `RecordValidation` with metadata and the CAS) through the incremental
//!   kernel, a replay and full recomputation, compared exactly with the
//!   input model and both derived oracles (Jacobi and Kahn).
//! - **PROP7 metamorphic.** Rewriting every `meta` and note (to another
//!   hash, or to none, or from none to one) changes no commit outcome and
//!   no derived state, on one branch and through forks and merges.
#[path = "common/branch_gen.rs"]
mod branch_gen;
use branch_gen::*;
use contextdb_core::{CommitStats, Counting, Db, FullRecompute, Kernel};
use contextdb_model::*;
use contextdb_oracle::{InputModel, current_vector, jacobi, kahn};
use contextdb_testkit::generate_p7;
use std::collections::BTreeMap;

fn seeds(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn prop1_with_artifacts_meta_and_if_absent() {
    let mut ops: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut errors: BTreeMap<String, u64> = BTreeMap::new();
    let (mut with_meta, mut notes, mut exists) = (0u64, 0u64, 0u64);
    for seed in 0..seeds("CONTEXTDB_P7_PROP1_SEEDS", 10_000) {
        let trace = generate_p7(seed);
        assert_eq!(trace, generate_p7(seed));
        let mut db = Db::<Counting, CommitStats>::new();
        let mut replay = Db::<Counting, CommitStats>::new();
        let mut full = Db::<FullRecompute, CommitStats>::new();
        let mut oracle = InputModel::default();
        for (tx, batch) in trace.batches.iter().enumerate() {
            let digest = db.digest();
            let want = oracle.apply(batch);
            let actual = db.commit(batch);
            let again = replay.commit(batch);
            let rebuilt = full.commit(batch);
            let at = || format!("seed {seed} tx {tx}: {batch:?}");
            assert_eq!(actual, again, "{}", at());
            assert_eq!(actual, rebuilt, "{}", at());
            assert_eq!(
                actual.as_ref().map(|a| ApplyResult {
                    epoch: a.epoch,
                    allocations: a.allocations.clone()
                }),
                want.as_ref().cloned(),
                "{}",
                at()
            );
            assert_eq!(db.inputs(), *oracle.inputs(), "{}", at());
            let fixed = jacobi(oracle.inputs());
            assert_eq!(kahn(oracle.inputs()).unwrap(), fixed, "{}", at());
            assert_eq!(db.derived(), &fixed, "{}", at());
            assert_eq!(full.derived(), &fixed, "{}", at());
            match &actual {
                Ok(_) => {
                    for op in &batch.ops {
                        *ops.entry(op.name()).or_default() += 1;
                        match op {
                            Op::RecordValidation { meta: Some(_), .. } => with_meta += 1,
                            Op::ResolveArtifact { note: Some(_), .. } => notes += 1,
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    assert_eq!(db.digest(), digest, "{}", at());
                    exists += u64::from(matches!(e, TxError::ValidationExists { .. }));
                    let name = format!("{e:?}");
                    let name = name.split([' ', '{', '(']).next().unwrap_or("").to_owned();
                    *errors.entry(name).or_default() += 1;
                }
            }
        }
    }
    eprintln!("P7 PROP1 committed ops {ops:?}");
    eprintln!("P7 PROP1 rejections {errors:?}");
    eprintln!(
        "P7 PROP1 records with meta {with_meta}, resolves with a note {notes}, ValidationExists {exists}"
    );
    let n = seeds("CONTEXTDB_P7_PROP1_SEEDS", 10_000);
    assert!(with_meta * 10 >= n, "{with_meta}");
    assert!(notes * 50 >= n, "{notes}");
    assert!(exists * 20 >= n, "{exists}");
    assert!(ops.get("OpenArtifact").copied().unwrap_or(0) * 4 >= n);
}

/// Every `meta` and note rewritten: Some(h) → Some(h') or None, None →
/// Some. `if_absent` and everything else stays.
fn rewrite(b: &Batch, salt: u64) -> Batch {
    let mut b = b.clone();
    let swap = |h: &Option<ContentHash>, n: usize| -> Option<ContentHash> {
        match (h, (salt as usize + n) % 3) {
            (None, _) => Some([0xEE; 32]),
            (Some(_), 0) => None,
            (Some(h), _) => Some(*blake3::hash(h).as_bytes()),
        }
    };
    for (n, op) in b.ops.iter_mut().enumerate() {
        match op {
            Op::RecordValidation { meta, .. } => *meta = swap(meta, n),
            Op::ResolveArtifact { note, .. } => *note = swap(note, n),
            _ => {}
        }
    }
    b
}
fn bare(mut i: Inputs) -> Inputs {
    for v in i.validations.values_mut() {
        v.meta = None;
    }
    i
}

#[test]
fn prop7_meta_and_notes_change_no_derived_state() {
    for seed in 0..seeds("CONTEXTDB_P7_PROP7_SEEDS", 10_000) {
        let trace = generate_p7(seed);
        let mut a = Db::<Counting, CommitStats>::new();
        let mut b = Db::<Counting, CommitStats>::new();
        for (tx, batch) in trace.batches.iter().enumerate() {
            let other = rewrite(batch, seed + tx as u64);
            let (ra, rb) = (a.commit(batch), b.commit(&other));
            assert_eq!(ra, rb, "seed {seed} tx {tx}");
            assert_eq!(
                a.derived_digest(),
                b.derived_digest(),
                "seed {seed} tx {tx}"
            );
            assert_eq!(bare(a.inputs()), bare(b.inputs()), "seed {seed} tx {tx}");
        }
    }
}

/// A validation record for a random rule of `branch` at its current vector,
/// in global ids, with metadata and a random `if_absent`.
fn validation(g: &mut Gen, k: &Kernel, branch: BranchId) -> Option<Batch> {
    let view = k.branch(branch).unwrap();
    let i = view.db().inputs();
    if i.rules.is_empty() {
        return None;
    }
    let sem: Vec<_> = (0..i.rules.len() as u32)
        .map(RuleId)
        .filter(|j| !current_vector(&i, &i.rules[j.index()]).is_empty())
        .collect();
    let id = match sem.len() {
        0 => RuleId(g.pick(i.rules.len()) as u32),
        n => sem[g.pick(n)],
    };
    let curvec = current_vector(&i, &i.rules[id.index()])
        .into_iter()
        .map(|(l, r)| (view.global_logical(l), view.global_revision(r)))
        .collect();
    Some(on_branch(
        branch,
        vec![Op::RecordValidation {
            rule: view.global_rule(id).into(),
            curvec_seen: curvec,
            verdict: [Verdict::Verified, Verdict::Rejected, Verdict::Pending][g.pick(3)],
            validator: "worker".into(),
            meta: g.coin(75).then_some([g.pick(200) as u8; 32]),
            if_absent: g.coin(50),
        }],
    ))
}
fn branch_states(k: &Kernel) -> Vec<(BranchId, ContentHash, Inputs)> {
    k.branches()
        .map(|info| {
            let view = k.branch(info.id).unwrap();
            (
                info.id,
                view.db().derived_digest(),
                bare(view.db().inputs()),
            )
        })
        .collect()
}

#[test]
fn prop7_meta_and_notes_through_forks_and_merges() {
    let (mut merges, mut records, mut resolves) = (0u64, 0u64, 0u64);
    let n = seeds("CONTEXTDB_P7_PROP7_SEEDS", 10_000) / 4;
    for seed in 0..n {
        let mut g = Gen::new(seed);
        let (mut a, mut b) = (Kernel::default(), Kernel::default());
        for step in 0..30 + g.pick(30) {
            let batch = match g.pick(100) {
                0..=24 => {
                    let branch = g.branch(&a);
                    match validation(&mut g, &a, branch) {
                        Some(v) => v,
                        None => g.step(&a),
                    }
                }
                _ => g.step(&a),
            };
            // Notes on resolves, so the rewrite has something to change.
            let mut batch = batch;
            for op in &mut batch.ops {
                if let Op::ResolveArtifact { note, .. } = op
                    && g.coin(60)
                {
                    *note = Some([g.pick(200) as u8; 32]);
                }
            }
            let other = rewrite(&batch, seed + step as u64);
            let (ra, rb) = (a.commit(&batch), b.commit(&other));
            assert_eq!(ra, rb, "seed {seed} step {step}: {batch:?}");
            if ra.is_ok() {
                for op in &batch.ops {
                    match op {
                        Op::Merge { .. } => merges += 1,
                        Op::RecordValidation { .. } => records += 1,
                        Op::ResolveArtifact { .. } => resolves += 1,
                        _ => {}
                    }
                }
            }
            assert_eq!(
                branch_states(&a),
                branch_states(&b),
                "seed {seed} step {step}: {batch:?}"
            );
        }
    }
    eprintln!("P7 PROP7 committed merges {merges}, records {records}, resolves {resolves}");
    assert!(
        merges >= n && records >= n && resolves >= n / 2,
        "{n} seeds"
    );
}
