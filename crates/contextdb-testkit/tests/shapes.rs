use contextdb_core::Db;
use contextdb_model::*;
use contextdb_oracle::jacobi;
use contextdb_testkit::{Shape, shape};
#[test]
fn shape_stats_and_replay_are_exact() {
    for kind in [
        Shape::Chain,
        Shape::Tree { fanout: 3 },
        Shape::Hub,
        Shape::MultiSupport { p_alt: 0.3 },
    ] {
        let graph = shape(kind, 100, 42);
        assert_eq!(graph.batches, shape(kind, 100, 42).batches);
        let mut db = Db::default();
        for b in &graph.batches {
            db.commit(b).unwrap();
        }
        let i = db.inputs();
        let stats = graph.stats;
        assert_eq!(stats.revisions, i.revisions.len());
        assert_eq!(stats.rules, i.rules.len());
        assert_eq!(
            stats.incidences,
            i.rules.iter().map(|j| j.premises.len()).sum()
        );
        assert_eq!(stats.bases, i.base.iter().filter(|b| **b).count());
        assert_eq!(db.derived(), &jacobi(&i));
        match kind {
            Shape::Chain => assert_eq!(stats.depth, 99),
            Shape::Tree { .. } => assert_eq!(stats.max_fan_out, 3),
            Shape::Hub => {
                assert_eq!(stats.depth, 1);
                assert_eq!(stats.max_fan_out, 97);
                assert_eq!(stats.incidences, 291);
            }
            Shape::MultiSupport { .. } => assert!(stats.alternatives > 0),
        }
    }
}
