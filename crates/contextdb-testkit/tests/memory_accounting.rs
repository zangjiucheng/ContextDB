//! ADR 0005: category accounting must never over-count live heap (that would mask
//! another scope's upper bound) and must leave only small untracked overhead.
//! One test function: the allocator counters are process-global.
use contextdb_core::{Db, MemoryFootprint};
use contextdb_model::*;
use contextdb_testkit::{Shape, shape};
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};
use std::{alloc::System, collections::BTreeSet};

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;
fn live() -> isize {
    let s = ALLOCATOR.stats();
    s.bytes_allocated as isize - s.bytes_deallocated as isize
}
/// (category lower bound, bytes actually released when the Db is dropped).
/// A clone has exact capacities, so values do not depend on growth history or
/// on features (paranoid materializes the snapshot during commits).
fn measure(db: Db) -> (MemoryFootprint, usize) {
    let _ = contextdb_model::DerivedView::derived(&db);
    let db = db.clone();
    let footprint = db.memory_footprint();
    let before = live();
    drop(db);
    (footprint, (before - live()) as usize)
}
fn load(batches: &[Batch]) -> Db {
    let mut db = Db::default();
    for b in batches {
        db.commit(b).unwrap();
    }
    db
}
fn check(name: &str, db: Db, slack: f64) -> MemoryFootprint {
    let (footprint, freed) = measure(db);
    let lower = footprint.lower_bound_bytes();
    eprintln!("{name}: lower={lower} freed={freed} {footprint:?}");
    assert!(
        lower <= freed,
        "{name}: accounting over-counts ({lower} > {freed})"
    );
    assert!(
        (freed - lower) as f64 <= slack * freed as f64,
        "{name}: untracked {} exceeds {slack} of {freed}",
        freed - lower
    );
    footprint
}
/// Exact category values pin the accounting arithmetic and exact-reserve
/// publication. Update together with an ADR 0005 re-measurement when the
/// storage layout changes intentionally (64-bit targets, pinned toolchain).
fn golden(f: &MemoryFootprint, expected: [usize; 7]) {
    let actual = [
        f.inputs,
        f.indexes,
        f.maintenance,
        f.topology,
        f.query_cache,
        f.history,
        f.history_changes,
    ];
    assert_eq!(actual, expected);
    assert_eq!(f.hot_bytes(), expected[1] + expected[2] + expected[3]);
    assert_eq!(f.lower_bound_bytes(), expected[..6].iter().sum::<usize>());
}
#[test]
fn accounting_is_a_tight_lower_bound_of_live_heap() {
    // Allocator calibration: growth, in-place collect and drop leave no residual.
    // The counters are process-global, so another thread (the test harness)
    // may allocate during a round; one clean round out of five proves the
    // counters exact.
    let residuals: Vec<isize> = (0..5)
        .map(|_| {
            let base = live();
            let grown: Vec<u64> = (0..100_000).collect();
            let narrowed: Vec<u32> = grown.into_iter().map(|x| x as u32).collect();
            drop(narrowed);
            live() - base
        })
        .collect();
    assert!(residuals.contains(&0), "allocator residuals {residuals:?}");

    let multi = check(
        "multi_support",
        load(&shape(Shape::MultiSupport { p_alt: 0.3 }, 5_000, 7).batches),
        0.01,
    );
    golden(
        &multi,
        [749584, 349708, 141892, 471840, 704098, 519908, 9997],
    );
    check("hub", load(&shape(Shape::Hub, 5_000, 7).batches), 0.01);
    // Sparse metadata, many interned lanes, references and B-tree tables.
    let mut batches = shape(Shape::Chain, 2_000, 7).batches;
    batches.push(Batch::new(
        (0..500)
            .map(|n| Op::CreateLogical {
                key: format!("extra-{n}"),
                kind: RecordKind::Fact,
                scope_lane: format!("lane-{}", n % 50),
                content: [n as u8; 32],
                adopt_as_base: true,
            })
            .collect(),
    ));
    batches.push(Batch::new(
        (0..300u32)
            .map(|n| Op::SetMetadata {
                logical: LogicalRef::Existing(LogicalId(2_000 + n)),
                tags: BTreeSet::from([format!("tag-{n}"), "shared".into()]),
                dirs: BTreeSet::from([format!("src/m{}", n % 7)]),
            })
            .collect(),
    ));
    // Rules with Sem, Cur and Ref incidences, validation records and open artifacts.
    let rule = RuleId(1_999);
    batches.push(Batch::new(
        (0..20u32)
            .map(|k| Op::AddRule {
                conclusion: RevisionId(2_100 + k).into(),
                incidences: vec![
                    (RevisionId(2_000 + k).into(), EdgeKind::Sem),
                    (RevisionId(1_000 + k).into(), EdgeKind::Cur),
                    (RevisionId(k).into(), EdgeKind::Ref),
                ],
                enabled: true,
            })
            .collect(),
    ));
    batches.push(Batch::new(
        (0..20u32)
            .map(|k| Op::RecordValidation {
                rule: RuleId(rule.0 + k).into(),
                curvec_seen: vec![(LogicalId(2_000 + k), RevisionId(2_000 + k))],
                verdict: Verdict::Verified,
                validator: format!("validator-{k}"),
            })
            .chain((0..30u32).map(|k| Op::OpenArtifact {
                ty: ArtifactType::PendingReview,
                target: RevisionId(k).into(),
                payload_hash: [k as u8; 32],
            }))
            .collect(),
    ));
    let db = load(&batches);
    let inputs = db.inputs();
    assert_eq!((inputs.validations.len(), inputs.artifacts.len()), (20, 30));
    assert_eq!(
        inputs
            .rules
            .iter()
            .map(|j| j.references.len())
            .sum::<usize>(),
        20
    );
    // B-tree heavy (metadata, validations, adjacency overflow rows): the lower
    // bound counts full 11-slot leaves; real leaves are ~2/3 full.
    let lanes = check("metadata_lanes", db, 0.04);
    golden(
        &lanes,
        [496388, 100584, 51192, 195992, 306647, 235180, 4519],
    );
}
