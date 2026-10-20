//! Nightly finite-state exploration; all transitions use public transaction APIs.
use contextdb_core::{CommitStats, Counting, Db};
use contextdb_model::*;
use contextdb_oracle::{InputModel, jacobi, kahn};
use contextdb_testkit::create;
use std::collections::{BTreeSet, VecDeque};
fn key(db: &Db<Counting, CommitStats>) -> Vec<u8> {
    let mut i = db.inputs();
    for l in &mut i.logicals {
        l.origin_epoch = Epoch(0);
    }
    for r in &mut i.revisions {
        r.origin_epoch = Epoch(0);
    }
    for j in &mut i.rules {
        j.origin_epoch = Epoch(0);
    }
    i.canonical_bytes()
}
#[test]
#[ignore = "nightly bounded exhaustive gate; run --release --ignored --nocapture"]
fn bounded_depth6() {
    let depth: usize = std::env::var("CONTEXTDB_EXHAUSTIVE_DEPTH")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(6);
    assert!((6..=8).contains(&depth));
    let r = |n| RevRef::Existing(RevisionId(n));
    let add = |c, p, k| Op::AddRule {
        conclusion: r(c),
        incidences: vec![(r(p), k)],
        enabled: true,
    };
    let mut alphabet = Vec::new();
    for n in 0..4 {
        alphabet.push(Op::AssertBase { rev: r(n) });
        alphabet.push(Op::RetractBase { rev: r(n) });
    }
    for (old, new) in [(0, 3), (3, 0)] {
        alphabet.push(Op::SetHead {
            logical: LogicalId(0).into(),
            expected: r(old),
            new: r(new),
        });
    }
    alphabet.extend([
        add(2, 0, EdgeKind::Cur),
        add(2, 0, EdgeKind::Sem),
        add(0, 2, EdgeKind::Pin),
    ]);
    for n in 0..3 {
        for enabled in [false, true] {
            alphabet.push(Op::SetRuleEnabled {
                rule: RuleId(n).into(),
                enabled,
            });
        }
    }
    for current in [0, 3] {
        for verdict in [Verdict::Verified, Verdict::Rejected, Verdict::Pending] {
            alphabet.push(Op::RecordValidation {
                rule: RuleId(1).into(),
                curvec_seen: vec![(LogicalId(0), RevisionId(current))],
                verdict,
                validator: "exhaustive".into(),
            });
        }
    }
    let init = Batch::new(vec![
        create("a", RecordKind::Fact, false, 0),
        create("b", RecordKind::Fact, false, 1),
        create("c", RecordKind::Fact, false, 2),
        Op::AddRevision {
            logical: LogicalId(0).into(),
            content: [3; 32],
            adopt_as_base: false,
        },
    ]);
    let mut db = Db::<Counting, CommitStats>::new();
    let mut oracle = InputModel::default();
    db.commit(&init).unwrap();
    oracle.apply(&init).unwrap();
    let mut seen = BTreeSet::from([key(&db)]);
    let mut queue = VecDeque::from([(db, oracle, 0)]);
    let (mut transitions, mut accepted, mut rejected) = (0u64, 0u64, 0u64);
    let (mut pending, mut semantic_rejected) = (0u64, 0u64);
    while let Some((db, oracle, d)) = queue.pop_front() {
        if d == depth {
            continue;
        }
        for op in &alphabet {
            let mut next = db.clone();
            let mut model = oracle.clone();
            let batch = Batch::new(vec![op.clone()]);
            let before = next.derived().clone();
            let digest = next.digest();
            let result = next.commit(&batch);
            let want = model.apply(&batch);
            transitions += 1;
            assert_eq!(
                result.as_ref().map(|a| ApplyResult {
                    epoch: a.epoch,
                    allocations: a.allocations.clone()
                }),
                want.as_ref().cloned()
            );
            assert_eq!(next.inputs(), *model.inputs());
            let fixed = jacobi(model.inputs());
            pending += u64::from(fixed.sem.contains(&SemState::Pending));
            semantic_rejected += u64::from(fixed.sem.contains(&SemState::Rejected));
            assert_eq!(next.derived(), &fixed);
            assert_eq!(kahn(model.inputs()).unwrap(), fixed);
            assert!(next.dynamic_topo().valid());
            if let Ok(result) = result {
                accepted += 1;
                let flips: Vec<_> = fixed
                    .nodes
                    .iter()
                    .enumerate()
                    .filter(|(n, v)| before.nodes[*n].l0 != v.l0)
                    .map(|(n, _)| RevisionId(n as u32))
                    .collect();
                assert_eq!(result.l0_flips, flips);
                let diff: Vec<_> = fixed
                    .nodes
                    .iter()
                    .enumerate()
                    .filter(|(n, v)| before.nodes[*n] != **v)
                    .map(|(n, _)| RevisionId(n as u32))
                    .collect();
                assert_eq!(result.status_diff, diff);
                if seen.insert(key(&next)) {
                    queue.push_back((next, model, d + 1));
                }
            } else {
                rejected += 1;
                assert_eq!(next.digest(), digest);
            }
        }
    }
    assert!(pending > 0 && semantic_rejected > 0);
    eprintln!(
        "semantic_pending_transitions={pending} semantic_rejected_transitions={semantic_rejected}"
    );
    eprintln!(
        "bounded exhaustive: depth={depth} alphabet={} states={} transitions={transitions} accepted={accepted} rejected={rejected} violations=0",
        alphabet.len(),
        seen.len()
    );
}
