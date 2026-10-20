//! PROP10 (core part) and ADR 0006 §7: a database rebuilt from an input snapshot
//! equals the incrementally maintained one, and both return identical
//! CommitResults and errors for every later batch.
use contextdb_core::Db;
use contextdb_model::*;

#[test]
fn rebuild_from_inputs_equals_incremental_10000_seeds() {
    for seed in 0..10_000u64 {
        let log = if seed % 2 == 0 {
            contextdb_testkit::generate(seed)
        } else {
            contextdb_testkit::generate_p1(seed)
        };
        let cut = (seed as usize * 7) % (log.batches.len() + 1);
        let mut live = Db::default();
        for b in &log.batches[..cut] {
            let _ = live.commit(b);
        }
        let start = live.epoch();
        let mut rebuilt: Db = Db::from_inputs(live.inputs(), start).unwrap();
        assert_eq!(
            rebuilt.digest(),
            live.digest(),
            "seed {seed}: rebuild digest"
        );
        assert_eq!(
            DerivedView::derived(&rebuilt),
            &contextdb_oracle::jacobi(&rebuilt.inputs()),
            "seed {seed}: rebuild vs oracle"
        );
        for (n, b) in log.batches[cut..].iter().enumerate() {
            assert_eq!(rebuilt.commit(b), live.commit(b), "seed {seed} batch {n}");
        }
        assert_eq!(rebuilt.digest(), live.digest(), "seed {seed}: final digest");
        let end = live.epoch();
        for a in start.0..=end.0 {
            for b in [start.0, end.0] {
                assert_eq!(
                    rebuilt.diff(Epoch(a), Epoch(b)),
                    live.diff(Epoch(a), Epoch(b)),
                    "seed {seed}: diff {a}->{b}"
                );
            }
        }
        if start.0 > 0 {
            assert!(rebuilt.diff(Epoch(start.0 - 1), end).is_err());
        }
    }
}

/// T07b / T13 (core part): Historical and Snapshot reads at every retained
/// epoch equal what a reader at that epoch saw, byte for byte, including after
/// a rebuild (for epochs at or after the rebuild point).
#[test]
fn read_modes_reproduce_past_states_1000_seeds() {
    for seed in 0..1_000u64 {
        let log = contextdb_testkit::generate_p1(seed);
        let mut db = Db::default();
        let mut seen = vec![observe(&db)];
        for b in &log.batches {
            if db.commit(b).is_ok() {
                seen.push(observe(&db));
            }
        }
        let cut = Epoch((seed % (db.epoch().0 + 1)).min(db.epoch().0));
        let mut replay = Db::default();
        let mut next = 0;
        while replay.epoch() < cut {
            let _ = replay.commit(&log.batches[next]);
            next += 1;
        }
        let mut rebuilt: Db = Db::from_inputs(replay.inputs(), cut).unwrap();
        for b in &log.batches[next..] {
            let _ = rebuilt.commit(b);
        }
        assert_eq!(rebuilt.digest(), db.digest(), "seed {seed}");
        for (e, (heads, nodes)) in seen.iter().enumerate() {
            let e = Epoch(e as u64);
            for target in std::iter::once(&db).chain((e >= cut).then_some(&rebuilt)) {
                for (l, h) in heads.iter().enumerate() {
                    let (r, s) = target.read_snapshot(LogicalId(l as u32), e).unwrap();
                    assert_eq!(
                        (r, s.canonical_bytes()),
                        (*h, nodes[h.index()].canonical_bytes())
                    );
                }
                for (r, s) in nodes.iter().enumerate() {
                    let read = target.read_historical(RevisionId(r as u32), e).unwrap();
                    assert_eq!(
                        read.canonical_bytes(),
                        s.canonical_bytes(),
                        "seed {seed} e {e:?}"
                    );
                }
                // Objects created after `e` do not exist at `e`.
                let later = RevisionId(nodes.len() as u32);
                if later.index() < db.inputs().revisions.len() {
                    assert_eq!(target.read_historical(later, e), Err(TxError::UnknownRef));
                }
            }
        }
        if cut.0 > 0 {
            assert_eq!(
                rebuilt.read_historical(RevisionId(0), Epoch(cut.0 - 1)),
                Err(TxError::UnknownRef)
            );
        }
    }
}
fn observe(db: &Db) -> (Vec<RevisionId>, Vec<NodeState>) {
    let inputs = db.inputs();
    let heads = inputs.heads.clone();
    let nodes = (0..inputs.revisions.len())
        .map(|r| db.node(RevisionId(r as u32)).unwrap())
        .collect();
    (heads, nodes)
}
