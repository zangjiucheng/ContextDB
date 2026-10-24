//! The P6 Conflict gate (SD11 `¬Conflicted(r)`, SD17 as amended, ADR 0013
//! §6.4): an open `Conflict` artifact blocks L2 and CERTIFIED of its target
//! without changing S or L1 (T51); resolving it releases the gate.
use contextdb_core::{CommitStats, Counting, Db, FullRecompute};
use contextdb_model::*;
use contextdb_oracle::{jacobi, kahn};
use contextdb_testkit::generate;
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};

fn create(key: &str, content: u8) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [content; 32],
        adopt_as_base: true,
    }
}
fn conflict(target: u32) -> Op {
    Op::OpenArtifact {
        ty: ArtifactType::Conflict,
        target: RevisionId(target).into(),
        payload_hash: [target as u8; 32],
    }
}

/// T51: the gate blocks CERTIFIED (CONFLICTED) and L2 downstream (PENDING
/// Semantic), leaves S and L1, and resolving releases it.
#[test]
fn t51_open_conflict_blocks_certified_only() {
    let mut db = Db::default();
    db.commit(&Batch::new(vec![
        create("a", 0),
        Op::CreateLogical {
            key: "d".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: [1; 32],
            adopt_as_base: false,
        },
        Op::AddRule {
            conclusion: RevisionId(1).into(),
            incidences: vec![(RevisionId(0).into(), EdgeKind::Cur)],
            enabled: true,
        },
    ]))
    .unwrap();
    assert_eq!(db.status(RevisionId(0)).unwrap(), Summary::Certified);
    assert_eq!(db.status(RevisionId(1)).unwrap(), Summary::Certified);
    let opened = db.commit(&Batch::new(vec![conflict(0)])).unwrap();
    let a = db.node(RevisionId(0)).unwrap();
    assert!(a.l0 && a.l1 && !a.l2 && a.dimensions.conflicted);
    assert_eq!(db.status(RevisionId(0)).unwrap(), Summary::Conflicted);
    let d = db.node(RevisionId(1)).unwrap();
    assert!(d.l0 && d.l1 && !d.l2 && !d.dimensions.conflicted);
    assert_eq!(
        db.status(RevisionId(1)).unwrap(),
        Summary::Pending(PendingReason::Semantic)
    );
    assert_eq!(opened.l0_flips, vec![]);
    assert_eq!(opened.status_diff, vec![RevisionId(0), RevisionId(1)]);
    // Resolving releases it.
    db.commit(&Batch::new(vec![Op::ResolveArtifact {
        id: opened.allocations.new_artifacts[0].into(),
    }]))
    .unwrap();
    assert_eq!(db.status(RevisionId(0)).unwrap(), Summary::Certified);
    assert_eq!(db.status(RevisionId(1)).unwrap(), Summary::Certified);
    // Other artifact types do not gate.
    db.commit(&Batch::new(vec![Op::OpenArtifact {
        ty: ArtifactType::PendingReview,
        target: RevisionId(0).into(),
        payload_hash: [9; 32],
    }]))
    .unwrap();
    assert_eq!(db.status(RevisionId(0)).unwrap(), Summary::Certified);
}

/// Generated histories with Conflicts opened and resolved: the maintained
/// state equals both oracles after every commit, in both modes, and a
/// rebuild from inputs agrees (PROP1/PROP10 for the gate).
#[test]
fn gate_matches_the_oracles() {
    let mut gated = 0;
    for seed in 0..500 {
        let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0x6a7e);
        let mut db = Db::<Counting, CommitStats>::new();
        let mut full = Db::<FullRecompute, CommitStats>::new();
        for b in generate(seed).batches {
            let mut b = b;
            let inputs = db.inputs();
            let n = inputs.revisions.len() as u32;
            if n > 0 && rng.next_u32() % 2 == 0 {
                b.ops.push(conflict(rng.next_u32() % n));
            }
            let open: Vec<_> = (inputs.artifacts.iter().enumerate())
                .filter(|(_, a)| a.open)
                .map(|(n, _)| ArtifactId(n as u32))
                .collect();
            if !open.is_empty() && rng.next_u32() % 3 == 0 {
                let id = open[rng.next_u32() as usize % open.len()];
                b.ops.push(Op::ResolveArtifact { id: id.into() });
            }
            let (got, want) = (db.commit(&b), full.commit(&b));
            assert_eq!(got, want, "seed {seed}");
            let inputs = db.inputs();
            let derived = contextdb_model::DerivedView::derived(&db).clone();
            assert_eq!(derived, jacobi(&inputs), "seed {seed}");
            assert_eq!(Ok(derived.clone()), kahn(&inputs), "seed {seed}");
            assert_eq!(&derived, contextdb_model::DerivedView::derived(&full));
            gated += derived
                .nodes
                .iter()
                .filter(|s| s.dimensions.conflicted)
                .count();
        }
        let rebuilt = Db::<Counting, CommitStats>::from_inputs(db.inputs(), db.epoch()).unwrap();
        assert_eq!(
            contextdb_model::DerivedView::derived(&rebuilt),
            contextdb_model::DerivedView::derived(&db),
            "seed {seed}"
        );
    }
    assert!(gated > 1000, "conflicted nodes observed: {gated}");
}
