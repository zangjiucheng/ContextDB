mod counting {
    type ExactMode = contextdb_core::Counting;
    mod cases {
        include!("common/p1_cases.rs");
    }
}
mod full {
    type ExactMode = contextdb_core::FullRecompute;
    mod cases {
        include!("common/p1_cases.rs");
    }
}
use contextdb_core::{CommitStats, Counting, Db, NoStats};
use contextdb_model::*;
use contextdb_testkit::create;
#[test]
fn t05_t30_locality_and_no_downstream_visit_after_alternative_insert() {
    assert_eq!(std::mem::size_of::<NoStats>(), 0);
    let mut db = Db::<Counting, CommitStats>::new();
    let mut ops: Vec<_> = (0..7)
        .map(|n| {
            create(
                &format!("n{n}"),
                RecordKind::Fact,
                n == 0 || n == 4 || n == 6,
                n as u8,
            )
        })
        .collect();
    for (p, c) in [(0, 1), (1, 2), (2, 3), (4, 5)] {
        ops.push(Op::AddRule {
            conclusion: RevisionId(c).into(),
            incidences: vec![(RevisionId(p).into(), EdgeKind::Pin)],
            enabled: true,
        });
    }
    db.commit(&Batch::new(ops)).unwrap();
    db.commit(&Batch::new(vec![Op::AddRule {
        conclusion: RevisionId(1).into(),
        incidences: vec![(RevisionId(6).into(), EdgeKind::Pin)],
        enabled: true,
    }]))
    .unwrap();
    assert_eq!(db.stats().popped_nodes, vec![RevisionId(1)]);
    db.commit(&Batch::new(vec![Op::RetractBase {
        rev: RevisionId(0).into(),
    }]))
    .unwrap();
    assert_eq!(db.stats().popped_nodes, vec![RevisionId(0), RevisionId(1)]);
    assert_eq!(db.stats().incidences_visited, 1);
    assert!(db.current_l0(LogicalId(3)).unwrap().1);
}

#[test]
fn early_witness_change_stops_status_propagation() {
    let mut db = Db::<Counting, CommitStats>::new();
    let graph = contextdb_testkit::shape(
        contextdb_testkit::Shape::MultiSupport { p_alt: 0.3 },
        1000,
        42,
    );
    for b in graph.batches {
        db.commit(&b).unwrap();
    }
    for op in [
        Op::AssertBase {
            rev: RevisionId(3).into(),
        },
        Op::RetractBase {
            rev: RevisionId(3).into(),
        },
    ] {
        let _ = db.derived(); // a materialized full snapshot must not force O(N) drop
        let result = db.commit(&Batch::new(vec![op])).unwrap();
        assert_eq!(result.status_diff, vec![RevisionId(3)]);
        assert_eq!(db.stats().incidences_visited, 0);
        assert_eq!(db.stats().status_nodes, vec![RevisionId(3)]);
    }
}
