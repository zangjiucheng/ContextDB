//! P6 `WHAT_IF` (SPEC SD19, ADR 0013 §5): a journalled dry run returns what
//! the commit would return and leaves the whole database as it was (T14).
use contextdb_core::{CommitStats, Counting, Db, FullRecompute, Kernel, Maintainer, StatsSink};
use contextdb_model::*;
use contextdb_testkit::{generate, generate_p1};

fn create(key: &str, content: u8, lane: &str) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: lane.into(),
        content: [content; 32],
        adopt_as_base: true,
    }
}
fn rev(n: u32) -> RevRef {
    RevisionId(n).into()
}

/// Before every commit of a trace: `what_if` equals the commit's result and
/// changes no part of the database.
fn check<M: Maintainer, S: StatsSink + PartialEq>(seed: u64, batches: &[Batch]) {
    let mut db = Db::<M, S>::new();
    for (tx, b) in batches.iter().enumerate() {
        if tx % 3 == 0 {
            // A materialized snapshot must be kept, not patched.
            let _ = DerivedView::derived(&db);
        }
        let before = db.clone();
        let dry = db.what_if(b);
        assert_eq!(db.state_diff(&before), None, "seed {seed} tx {tx}: {b:?}");
        let real = db.commit(b);
        assert_eq!(dry, real, "seed {seed} tx {tx}");
    }
}

#[test]
fn what_if_equals_commit_and_restores_everything() {
    for seed in 0..1000 {
        check::<Counting, CommitStats>(seed, &generate(seed).batches);
    }
    for seed in 0..300 {
        check::<Counting, CommitStats>(seed, &generate_p1(seed).batches);
        check::<FullRecompute, CommitStats>(seed, &generate(seed).batches);
    }
}

/// A database that dry-runs every batch first ends equal to one that never
/// does.
#[test]
fn dry_runs_leave_no_trace() {
    for seed in 0..300 {
        let (mut a, mut b) = (Db::<Counting, CommitStats>::new(), Db::new());
        for batch in &generate(seed).batches {
            let _ = a.what_if(batch);
            let _ = a.what_if(batch);
            let _ = (a.commit(batch), b.commit(batch));
        }
        assert_eq!(a.state_diff(&b), None, "seed {seed}");
    }
}

/// T14: a WHAT_IF with AddRule, SetRuleEnabled and RecordValidation (plus new
/// logicals, a new lane, metadata, an artifact, a watermark and a policy)
/// leaves the digest, epoch and arena lengths unchanged.
#[test]
fn t14_what_if_changes_nothing() {
    let mut db = Db::<Counting, CommitStats>::new();
    db.commit(&Batch::new(vec![
        create("a", 0, "repo"),
        create("b", 1, "repo"),
        create("c", 2, "repo"),
        Op::AddRule {
            conclusion: rev(2),
            incidences: vec![(rev(0), EdgeKind::Sem)],
            enabled: true,
        },
        Op::OpenArtifact {
            ty: ArtifactType::PendingReview,
            target: rev(0),
            payload_hash: [5; 32],
        },
    ]))
    .unwrap();
    let before = db.clone();
    let (digest, epoch) = (db.digest(), db.epoch());
    let counts = |db: &Db<Counting, CommitStats>| {
        (
            db.logical_count(),
            db.revision_count(),
            db.rule_count(),
            db.artifact_count(),
        )
    };
    let lens = counts(&db);
    let ops = vec![
        Op::AddRule {
            conclusion: rev(1),
            incidences: vec![(rev(0), EdgeKind::Pin)],
            enabled: true,
        },
        Op::SetRuleEnabled {
            rule: RuleId(0).into(),
            enabled: false,
        },
        Op::RecordValidation {
            rule: RuleId(0).into(),
            curvec_seen: vec![(LogicalId(0), RevisionId(0))],
            verdict: Verdict::Verified,
            validator: "t14".into(),
        },
        create("d", 3, "elsewhere"),
        Op::SetMetadata {
            logical: LogicalRef::Existing(LogicalId(0)),
            tags: ["x".to_owned()].into(),
            dirs: Default::default(),
        },
        Op::ResolveArtifact {
            id: ArtifactId(0).into(),
        },
        Op::OpenArtifact {
            ty: ArtifactType::PendingReview,
            target: rev(1),
            payload_hash: [6; 32],
        },
        Op::AddRevision {
            logical: LogicalRef::Existing(LogicalId(0)),
            content: [9; 32],
            adopt_as_base: true,
        },
        Op::SetHead {
            logical: LogicalRef::Existing(LogicalId(0)),
            expected: rev(0),
            new: RevRef::New(1),
        },
    ];
    let dry = db.what_if(&Batch::new(ops.clone())).unwrap();
    assert_eq!(dry.allocations.new_rules, vec![RuleId(1)]);
    assert_eq!(db.digest(), digest);
    assert_eq!(db.epoch(), epoch);
    assert_eq!(counts(&db), lens);
    assert_eq!(db.state_diff(&before), None);
    // Admin-only inputs too.
    let mut admin = Batch::new(vec![Op::SetPolicy {
        policy: PolicyDoc {
            version: 1,
            require_deps: [RecordKind::Plan].into_iter().collect(),
        },
    }]);
    admin.channel = Channel::Admin;
    db.what_if(&admin).unwrap();
    let mut observer = Batch::new(vec![Op::ObserverWatermark {
        scope: "fs".into(),
        expected: None,
        position: vec![1, 2],
    }]);
    observer.channel = Channel::Observer;
    db.what_if(&observer).unwrap();
    assert_eq!(db.state_diff(&before), None);
    // And the real commit gives what the dry run promised.
    assert_eq!(db.commit(&Batch::new(ops)).unwrap(), dry);
}

/// A rejected dry run is rejected like the commit and changes nothing.
#[test]
fn rejected_what_if() {
    let mut db = Db::<Counting, CommitStats>::new();
    db.commit(&Batch::new(vec![
        create("a", 0, "repo"),
        create("b", 1, "repo"),
    ]))
    .unwrap();
    let before = db.clone();
    let cycle = Batch::new(vec![
        Op::AddRule {
            conclusion: rev(1),
            incidences: vec![(rev(0), EdgeKind::Pin)],
            enabled: true,
        },
        Op::AddRule {
            conclusion: rev(0),
            incidences: vec![(rev(1), EdgeKind::Pin)],
            enabled: true,
        },
    ]);
    let dry = db.what_if(&cycle);
    assert!(matches!(dry, Err(TxError::CycleDetected { .. })));
    assert_eq!(db.state_diff(&before), None);
    assert_eq!(db.commit(&cycle), dry);
}

/// Through the kernel, a dry run on a branch reports the global ids a commit
/// would allocate (imports included) and changes no branch, id counter or
/// epoch.
#[test]
fn kernel_what_if_on_a_branch() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0, "repo")])).unwrap();
    let fork = |k: &mut Kernel, name: &str| {
        k.commit(&Batch::new(vec![Op::CreateBranch { name: name.into() }]))
            .unwrap();
        k.branch_by_name(name).unwrap()
    };
    let (b, c) = (fork(&mut k, "b"), fork(&mut k, "c"));
    let on = |branch, ops| {
        let mut x = Batch::new(ops);
        x.branch = branch;
        x
    };
    let revise = |content| Op::AddRevision {
        logical: LogicalRef::Existing(LogicalId(0)),
        content: [content; 32],
        adopt_as_base: false,
    };
    k.commit(&on(b, vec![revise(9)])).unwrap(); // r1 on b
    let batch = on(c, vec![revise(9), revise(10), create("x", 4, "repo")]);
    let digests: Vec<_> = [MAIN, b, c].map(|x| k.branch(x).unwrap().digest()).into();
    let epoch = k.epoch();
    let dry = k.what_if(&batch).unwrap();
    assert_eq!(
        dry.allocations.new_revisions,
        vec![RevisionId(1), RevisionId(2), RevisionId(3)]
    );
    assert_eq!(dry.allocations.new_logicals, vec![LogicalId(1)]);
    assert_eq!(k.epoch(), epoch);
    let after: Vec<_> = [MAIN, b, c].map(|x| k.branch(x).unwrap().digest()).into();
    assert_eq!(after, digests);
    assert!(k.branch(c).unwrap().imports().is_empty());
    // Branch ops are validated, not applied.
    assert_eq!(
        k.what_if(&on(c, vec![Op::CreateBranch { name: "b".into() }])),
        Err(TxError::BranchExists { name: "b".into() })
    );
    k.what_if(&on(c, vec![Op::CreateBranch { name: "new".into() }]))
        .unwrap();
    assert_eq!(k.branch_by_name("new"), None);
    k.what_if(&on(c, vec![Op::ArchiveBranch { name: "c".into() }]))
        .unwrap();
    assert!(!k.branch(c).unwrap().info().archived);
    // The real commit allocates exactly what the dry run said.
    assert_eq!(k.commit(&batch).unwrap(), dry);
}
