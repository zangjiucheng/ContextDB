//! P11 milestone-1 goldens (ADR 0019 §9 G2): slice boundaries, settledness
//! of unrelated nodes and the chain S bound, P-1 through a Sem head, and
//! I-L1's refusal of kernel entries with a frontier.
use contextdb_core::{Budget, CHUNK, Db, Drain, SEARCH, summarize};
use contextdb_model::*;

fn create(key: &str, content: u8, base: bool) -> Op {
    let mut hash = [0; 32];
    hash[..key.len().min(31)].copy_from_slice(&key.as_bytes()[..key.len().min(31)]);
    hash[31] = content;
    Op::CreateLogical {
        origin: Default::default(),
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: hash,
        adopt_as_base: base,
    }
}
fn rule(conclusion: RevRef, premise: RevRef, kind: EdgeKind) -> Op {
    Op::AddRule {
        conclusion,
        incidences: vec![(premise, kind)],
        enabled: true,
    }
}
fn retract(r: u32) -> Batch {
    Batch::new(vec![Op::RetractBase {
        rev: RevisionId(r).into(),
    }])
}
fn summary(db: &Db, r: RevisionId) -> Summary {
    summarize(&db.node(r).unwrap().dimensions)
}

/// A hub with `n` Pin dependents, then one unrelated base record.
fn hub(n: u32) -> Db {
    let mut db = Db::default();
    db.commit(&Batch::new(vec![create("hub", 0, true)]))
        .unwrap();
    for i in 0..n {
        db.commit(&Batch::new(vec![
            create(&format!("d{i}"), 0, false),
            rule(RevRef::New(0), RevisionId(0).into(), EdgeKind::Pin),
        ]))
        .unwrap();
    }
    db.commit(&Batch::new(vec![create("unrelated", 0, true)]))
        .unwrap();
    db
}

#[test]
fn a_hub_cascade_pauses_in_fan_out_and_leaves_unrelated_nodes_settled() {
    let mut db = hub(20);
    let mut eager = hub(20);
    let unrelated = RevisionId(21);
    let want = eager.commit(&retract(0)).unwrap();
    let got = db
        .commit_lazy(&retract(0), Budget { units: 3, chunk: 2 })
        .unwrap();
    assert!(db.has_frontier());
    assert!(got.l0_flips.is_empty() && got.status_diff.is_empty());
    // R5: seeding, then E units plus at most one chunk (and one node's
    // witness scan, which is not counted).
    let seeding = 2; // the hub's push and its walk seed
    assert!(db.frontier_units().unwrap() <= seeding + 3 + 2 + 2);
    assert!(db.settled(unrelated));
    assert_eq!(summary(&db, unrelated), Summary::Certified);
    let pending = (1..=20)
        .map(RevisionId)
        .filter(|r| summary(&db, *r) == Summary::Pending(PendingReason::StructuralUnknown))
        .count();
    assert!(
        pending > 0,
        "dependents read StructuralUnknown while the hub fans out"
    );
    // Slices of one unit still make progress, a chunk at a time.
    let mut slices = 0;
    let settled = loop {
        slices += 1;
        if let Some(done) = db.drain(Drain::Units(1)) {
            break done;
        }
    };
    assert!(slices > 10, "{slices} slices");
    assert_eq!(settled.l0_flips, want.l0_flips);
    assert_eq!(settled.status_diff, want.status_diff);
    assert_eq!(db.digest(), eager.digest());
    for r in 1..=20 {
        assert_eq!(
            summary(&db, RevisionId(r)),
            Summary::Stale(StaleReason::Unsupported)
        );
    }
}

#[test]
fn an_unrelated_node_above_more_than_s_ancestors_reads_unsettled() {
    // Chain a0 → … → a9, then chain b0 → … → b(S+10) built later, so every
    // b ranks above the cursor of a cascade down the a chain.
    let len = SEARCH as u16 + 10;
    let mut db = Db::default();
    db.commit(&Batch::new(vec![create("a0", 0, true)])).unwrap();
    for i in 1..10u32 {
        db.commit(&Batch::new(vec![
            create(&format!("a{i}"), 0, false),
            rule(RevRef::New(0), RevisionId(i - 1).into(), EdgeKind::Pin),
        ]))
        .unwrap();
    }
    // One batch, so the debug rebuild check runs once rather than per link.
    let mut ops = vec![create("b0", 0, true)];
    for i in 1..len {
        ops.push(create(&format!("b{i}"), 0, false));
        ops.push(rule(RevRef::New(i), RevRef::New(i - 1), EdgeKind::Pin));
    }
    db.commit(&Batch::new(ops)).unwrap();
    let (b0, b_last) = (RevisionId(10), RevisionId(9 + u32::from(len)));
    db.commit_lazy(&retract(0), Budget { units: 1, chunk: 1 })
        .unwrap();
    assert!(db.has_frontier());
    // b0 has no ancestor: settled. The chain's end has more than S
    // ancestors at or above the cursor: conservatively unsettled.
    assert!(db.settled(b0));
    assert!(!db.settled(b_last));
    assert_eq!(
        summary(&db, b_last),
        Summary::Pending(PendingReason::StructuralUnknown)
    );
    db.drain(Drain::Full).unwrap();
    assert_eq!(summary(&db, b_last), Summary::Certified);
    assert_eq!(
        summary(&db, RevisionId(9)),
        Summary::Stale(StaleReason::Unsupported)
    );
}

#[test]
fn p1_holds_through_a_sem_head_ranked_just_below_its_conclusion() {
    // c ← Sem(x); x moves to x2. The walk must read x2's layer state after
    // x2's own counting pop, whatever the slice boundaries.
    let build = || {
        let mut db = Db::default();
        db.commit(&Batch::new(vec![create("x", 0, true)])).unwrap();
        db.commit(&Batch::new(vec![
            create("c", 0, false),
            rule(RevRef::New(0), RevisionId(0).into(), EdgeKind::Sem),
        ]))
        .unwrap();
        db.commit(&Batch::new(vec![Op::AddRevision {
            logical: LogicalRef::Existing(LogicalId(0)),
            content: [7; 32],
            adopt_as_base: true,
        }]))
        .unwrap();
        db
    };
    let move_head = Batch::new(vec![Op::SetHead {
        logical: LogicalRef::Existing(LogicalId(0)),
        expected: RevisionId(0).into(),
        new: RevisionId(2).into(),
    }]);
    let mut eager = build();
    let want = eager.commit(&move_head).unwrap();
    for chunk in [1, 2, CHUNK] {
        for units in [1, 2, 3, 5] {
            let mut db = build();
            db.commit_lazy(&move_head, Budget { units, chunk }).unwrap();
            let settled = loop {
                for n in 0..3 {
                    let r = RevisionId(n);
                    if db.settled(r) {
                        assert_eq!(db.node(r).unwrap(), eager.node(r).unwrap());
                    }
                }
                let done = if db.has_frontier() {
                    db.drain(Drain::Units(1))
                } else {
                    break None;
                };
                if done.is_some() {
                    break done;
                }
            };
            if let Some(settled) = settled {
                assert_eq!(settled.status_diff, want.status_diff);
            }
            assert_eq!(db.digest(), eager.digest());
            assert_eq!(
                summary(&db, RevisionId(1)),
                Summary::Pending(PendingReason::Semantic)
            );
        }
    }
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "undrained frontier")]
fn a_commit_with_a_frontier_panics_in_debug() {
    let mut db = hub(8);
    db.commit_lazy(&retract(0), Budget { units: 1, chunk: 1 })
        .unwrap();
    assert!(db.has_frontier());
    let _ = db.commit(&retract(21));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "undrained frontier")]
fn a_dry_run_with_a_frontier_panics_in_debug() {
    let mut db = hub(8);
    db.commit_lazy(&retract(0), Budget { units: 1, chunk: 1 })
        .unwrap();
    let _ = db.what_if(&retract(21));
}

#[test]
fn diffs_and_why_wait_for_settlement() {
    let mut db = hub(8);
    let epoch = db.epoch();
    let at = db
        .commit_lazy(&retract(0), Budget { units: 1, chunk: 1 })
        .unwrap()
        .epoch;
    assert_eq!(db.diff(epoch, at), Err(TxError::Unsettled));
    assert_eq!(db.why(RevisionId(8)), Err(TxError::Unsettled));
    // Below the frontier's epoch history is exact.
    assert!(db.diff(Epoch(0), epoch).is_ok());
    db.drain(Drain::Full).unwrap();
    assert!(db.diff(epoch, at).is_ok());
    assert!(db.why(RevisionId(8)).is_ok());
}
