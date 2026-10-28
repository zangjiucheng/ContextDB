use contextdb_core::Db;
use contextdb_model::*;
use contextdb_oracle::{InputModel, jacobi, kahn};
use contextdb_testkit::{Coverage, generate};

#[test]
fn prop8_i1_error_equivalence_1000_chacha8_seeds() {
    let mut coverage = Coverage::default();
    let mut reference_only = 0;
    for seed in 0..1000 {
        let trace = generate(seed);
        assert_eq!(trace, generate(seed));
        assert!(trace.batches.len() <= 12);
        assert_eq!(EventLog::from_jsonl(&trace.to_jsonl()).unwrap(), trace);
        let mut db = Db::default();
        let mut reference = InputModel::default();
        coverage.seeds += 1;
        for (tx, b) in trace.batches.iter().enumerate() {
            assert!((1..=5).contains(&b.ops.len()));
            let before = db.derived().clone();
            let digest = db.digest();
            let epoch = db.epoch();
            let indexes = db.indexes().clone();
            let topo = db.topo();
            let want = reference.apply(b);
            let actual = db.commit(b);
            coverage.record(b, &want);
            if actual.as_ref().map(|r| ApplyResult {
                epoch: r.epoch,
                allocations: r.allocations.clone(),
            }) != want.as_ref().cloned()
            {
                std::fs::create_dir_all("../../results").unwrap();
                std::fs::write(
                    format!("../../results/p0-failure-{seed}.jsonl"),
                    trace.to_jsonl(),
                )
                .unwrap();
                panic!("seed {seed} tx {tx}: front-end={actual:?} oracle={want:?}");
            }
            assert_eq!(db.epoch(), reference.epoch(), "seed {seed} tx {tx}");
            assert_eq!(db.inputs(), *reference.inputs(), "seed {seed} tx {tx}");
            let fixed = jacobi(reference.inputs());
            assert_eq!(
                kahn(reference.inputs()).unwrap(),
                fixed,
                "seed {seed} tx {tx}"
            );
            assert_eq!(db.derived(), &fixed);
            for n in &fixed.nodes {
                assert!(!n.l2 || n.l1);
                assert!(!n.l1 || n.l0);
            }
            assert_eq!(
                Inputs::from_bytes(&db.inputs().canonical_bytes()).unwrap(),
                db.inputs()
            );
            assert_eq!(
                DerivedSnapshot::from_bytes(&fixed.canonical_bytes()).unwrap(),
                fixed
            );
            if let Ok(result) = actual {
                let flips: Vec<_> = fixed
                    .nodes
                    .iter()
                    .enumerate()
                    .filter(|(n, v)| v.l0 != before.nodes.get(*n).is_some_and(|old| old.l0))
                    .map(|(n, _)| RevisionId(n as u32))
                    .collect();
                let diff: Vec<_> = fixed
                    .nodes
                    .iter()
                    .enumerate()
                    .filter(|(n, v)| before.nodes.get(*n) != Some(*v))
                    .map(|(n, _)| RevisionId(n as u32))
                    .collect();
                assert_eq!(result.l0_flips, flips);
                assert_eq!(result.status_diff, diff);
            } else {
                assert_eq!(db.digest(), digest);
                assert_eq!(db.epoch(), epoch);
                assert_eq!(db.indexes(), &indexes);
                assert_eq!(db.topo(), topo);
                assert_eq!(db.derived(), &before);
            }
            let i = db.inputs();
            reference_only += i.rules.iter().filter(|j| j.premises.is_empty()).count();
            assert!(i.logicals.len() <= 6);
            assert!(i.revisions.len() <= 14);
            assert!(i.rules.len() <= 10);
            assert!(
                i.rules
                    .iter()
                    .all(|j| j.premises.len() + j.references.len() > 0 && j.premises.len() <= 3)
            );
        }
    }
    assert_eq!(coverage.seeds, 1000);
    assert_eq!(coverage.commits + coverage.rejects, 12000);
    assert!(coverage.commits > 1000 && coverage.rejects > 0);
    for name in [
        "CreateLogical",
        "AddRevision",
        "SetHead",
        "AssertBase",
        "RetractBase",
        "AddRule",
        "SetRuleEnabled",
        "Rebind",
        "RecordValidation",
    ] {
        assert!(coverage.ops.contains_key(name), "missing {name}");
    }
    for name in [
        "HeadConflict",
        "CycleDetected",
        "DuplicateRule",
        "InvalidRebind",
        "CurvecMoved",
    ] {
        assert!(coverage.errors.contains_key(name), "missing {name}");
    }
    assert!(reference_only > 0, "no reference-only rule was generated");
    eprintln!("P0 coverage: {coverage:?}, reference-only rule states {reference_only}");
}
