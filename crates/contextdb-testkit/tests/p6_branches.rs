//! P6 branches through the multi-branch kernel (SPEC SD19, ADR 0013):
//! main-only equivalence with the single-branch database, the branch table,
//! isolation (T47), the key namespace exception (T75), per-branch cycle
//! graphs (T72, first half), O(1) forks and global ids.
use contextdb_core::{Db, Kernel};
use contextdb_model::*;
use contextdb_testkit::generate;

fn create(key: &str, content: u8) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [content; 32],
        adopt_as_base: true,
    }
}
fn on(branch: BranchId, ops: Vec<Op>) -> Batch {
    let mut b = Batch::new(ops);
    b.branch = branch;
    b
}
fn fork(k: &mut Kernel, from: BranchId, name: &str) -> BranchId {
    k.commit(&on(from, vec![Op::CreateBranch { name: name.into() }]))
        .unwrap();
    k.branch_by_name(name).unwrap()
}
fn digest(k: &Kernel, b: BranchId) -> ContentHash {
    k.branch(b).unwrap().digest()
}
fn rule(conclusion: u32, premise: u32) -> Op {
    Op::AddRule {
        conclusion: RevisionId(conclusion).into(),
        incidences: vec![(RevisionId(premise).into(), EdgeKind::Pin)],
        enabled: true,
    }
}

/// With only `main`, the kernel is the single-branch database: same
/// results, errors, epochs and digests on 1,000 generated traces.
#[test]
fn main_only_kernel_equals_db() {
    for seed in 0..1000 {
        let trace = generate(seed);
        let (mut db, mut k) = (Db::default(), Kernel::default());
        for (tx, b) in trace.batches.iter().enumerate() {
            assert_eq!(k.commit(b), db.commit(b), "seed {seed} tx {tx}");
            assert_eq!(k.epoch(), db.epoch(), "seed {seed} tx {tx}");
            assert_eq!(digest(&k, MAIN), db.digest(), "seed {seed} tx {tx}");
        }
    }
}

/// A branch forked from an empty `main` that nobody else writes is the
/// single-branch database too, one epoch later (the fork's).
#[test]
fn lone_branch_equals_db() {
    for seed in 0..300 {
        let trace = generate(seed);
        let mut k = Kernel::default();
        let b = fork(&mut k, MAIN, "b");
        let mut db = Db::default();
        for (tx, batch) in trace.batches.iter().enumerate() {
            let want = db.commit(batch).map(|mut r| {
                r.epoch = Epoch(r.epoch.0 + 1);
                r
            });
            let mut on_b = batch.clone();
            on_b.branch = b;
            assert_eq!(k.commit(&on_b), want, "seed {seed} tx {tx}");
            let view = k.branch(b).unwrap();
            assert_eq!(
                view.db().derived_digest(),
                db.derived_digest(),
                "seed {seed} tx {tx}"
            );
        }
    }
}

/// The branch table: names, sole-op batches, archiving, unknown branches.
#[test]
fn branch_table() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0)])).unwrap();
    let b = fork(&mut k, MAIN, "feature/x");
    assert_eq!(k.branch(b).unwrap().info().parent, Some(MAIN));
    assert_eq!(k.branch(b).unwrap().info().fork_epoch, Epoch(2));
    let err = |k: &mut Kernel, batch| k.commit(&batch).unwrap_err();
    assert_eq!(
        err(
            &mut k,
            on(
                MAIN,
                vec![Op::CreateBranch {
                    name: "feature/x".into()
                }]
            )
        ),
        TxError::BranchExists {
            name: "feature/x".into()
        }
    );
    for name in ["main", "", "-x", "/x", "x/", "a..b", "a//b", "a b", "ä"] {
        assert_eq!(
            err(
                &mut k,
                on(MAIN, vec![Op::CreateBranch { name: name.into() }])
            ),
            TxError::InvalidBranchName { name: name.into() },
            "{name:?}"
        );
    }
    assert_eq!(
        err(
            &mut k,
            on(
                MAIN,
                vec![Op::CreateBranch { name: "y".into() }, create("z", 9)]
            )
        ),
        TxError::SoleOpRequired {
            op: "CreateBranch".into()
        }
    );
    assert_eq!(
        err(&mut k, on(BranchId(9), vec![create("z", 9)])),
        TxError::UnknownBranch
    );
    // Archiving: only the batch's own branch, never main.
    assert_eq!(
        err(
            &mut k,
            on(
                MAIN,
                vec![Op::ArchiveBranch {
                    name: "main".into()
                }]
            )
        ),
        TxError::InvalidBranchName {
            name: "main".into()
        }
    );
    assert_eq!(
        err(
            &mut k,
            on(
                MAIN,
                vec![Op::ArchiveBranch {
                    name: "feature/x".into()
                }]
            )
        ),
        TxError::InvalidBranchName {
            name: "feature/x".into()
        }
    );
    k.commit(&on(
        b,
        vec![Op::ArchiveBranch {
            name: "feature/x".into(),
        }],
    ))
    .unwrap();
    assert_eq!(
        err(&mut k, on(b, vec![create("z", 9)])),
        TxError::BranchArchived {
            name: "feature/x".into()
        }
    );
    // A failed batch never advances the epoch.
    let epoch = k.epoch();
    assert!(k.commit(&on(b, vec![create("z", 9)])).is_err());
    assert_eq!(k.epoch(), epoch);
}

/// A fork shares its parent's state (O(1)) until either side writes.
#[test]
fn fork_shares_state_until_a_write() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    assert!(
        k.branch(b)
            .unwrap()
            .shares_state_with(&k.branch(MAIN).unwrap())
    );
    // Same data; the fork counts in the child's digest (ADR 0013 §3).
    assert_eq!(
        k.branch(b).unwrap().db().digest(),
        k.branch(MAIN).unwrap().db().digest()
    );
    assert_ne!(digest(&k, b), digest(&k, MAIN));
    k.commit(&on(b, vec![create("x", 1)])).unwrap();
    assert!(
        !k.branch(b)
            .unwrap()
            .shares_state_with(&k.branch(MAIN).unwrap())
    );
}

/// T47: commits on a branch never change main: its digest stays byte for
/// byte, and main cannot reference what the branch created.
#[test]
fn t47_branch_isolation() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0), create("b", 1)]))
        .unwrap();
    let b = fork(&mut k, MAIN, "b");
    let main_digest = digest(&k, MAIN);
    // A new revision and a rule on the branch (naming the staged revision
    // by the global id it receives, 2), then disable the rule, open an
    // artifact and set a policy.
    let r = k.commit(&on(b, vec![create("c", 2), rule(2, 0)])).unwrap();
    let (rev, rule_id) = (r.allocations.new_revisions[0], r.allocations.new_rules[0]);
    k.commit(&on(
        b,
        vec![Op::SetRuleEnabled {
            rule: rule_id.into(),
            enabled: false,
        }],
    ))
    .unwrap();
    k.commit(&on(
        b,
        vec![Op::OpenArtifact {
            ty: ArtifactType::PendingReview,
            target: RevisionId(0).into(),
            payload_hash: [7; 32],
        }],
    ))
    .unwrap();
    let mut policy = on(
        b,
        vec![Op::SetPolicy {
            policy: PolicyDoc {
                version: 1,
                require_deps: [RecordKind::Plan].into_iter().collect(),
            },
        }],
    );
    policy.channel = Channel::Admin;
    k.commit(&policy).unwrap();
    assert_eq!(digest(&k, MAIN), main_digest, "main unchanged");
    // Main cannot see the branch's revision or rule.
    assert!(!k.branch(MAIN).unwrap().is_visible_revision(rev));
    assert_eq!(k.branch(MAIN).unwrap().node(rev), Err(TxError::UnknownRef));
    for op in [
        Op::AssertBase { rev: rev.into() },
        Op::SetRuleEnabled {
            rule: rule_id.into(),
            enabled: true,
        },
        rule(0, rev.0),
    ] {
        assert_eq!(k.commit(&Batch::new(vec![op])), Err(TxError::UnknownRef));
    }
    assert_eq!(digest(&k, MAIN), main_digest);
}

/// T75: a key taken on a branch is taken everywhere; the error tells
/// nothing else, and main is unchanged.
#[test]
fn t75_global_key_namespace() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&on(b, vec![create("k", 1)])).unwrap();
    let main_digest = digest(&k, MAIN);
    assert_eq!(
        k.commit(&Batch::new(vec![create("k", 2)])),
        Err(TxError::LogicalExists { key: "k".into() })
    );
    assert_eq!(digest(&k, MAIN), main_digest);
    // The key is not visible on main: a reference by key is UnknownRef.
    assert_eq!(k.branch(MAIN).unwrap().logical_by_key("k"), None);
    assert_eq!(
        k.commit(&Batch::new(vec![Op::AddRevision {
            logical: LogicalRef::Key("k".into()),
            content: [3; 32],
            adopt_as_base: true,
        }])),
        Err(TxError::UnknownRef)
    );
    // And the other way round: main's new key is taken on the branch.
    k.commit(&Batch::new(vec![create("m", 4)])).unwrap();
    assert_eq!(
        k.commit(&on(b, vec![create("m", 5)])),
        Err(TxError::LogicalExists { key: "m".into() })
    );
}

/// T72, first half: cycle graphs are per branch. With `B → A` on a branch,
/// main can still add `A → B`.
#[test]
fn t72_cycle_graphs_are_per_branch() {
    let mut k = Kernel::default();
    // A = revision 0, B = revision 1.
    k.commit(&Batch::new(vec![create("a", 0), create("b", 1)]))
        .unwrap();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&on(b, vec![rule(0, 1)])).unwrap();
    k.commit(&Batch::new(vec![rule(1, 0)])).unwrap();
    // Within one branch the cycle is still refused.
    assert!(matches!(
        k.commit(&on(b, vec![rule(1, 0)])),
        Err(TxError::CycleDetected { .. })
    ));
}

/// Global ids never repeat across branches, and results carry global ids.
#[test]
fn global_ids_are_unique_across_branches() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    let on_b = k.commit(&on(b, vec![create("x", 1)])).unwrap();
    let on_main = k.commit(&Batch::new(vec![create("y", 2)])).unwrap();
    assert_eq!(on_b.allocations.new_logicals, vec![LogicalId(1)]);
    assert_eq!(on_b.allocations.new_revisions, vec![RevisionId(1)]);
    assert_eq!(on_main.allocations.new_logicals, vec![LogicalId(2)]);
    assert_eq!(on_main.allocations.new_revisions, vec![RevisionId(2)]);
    assert_eq!(
        on_main.net_delta.head_moves,
        vec![(LogicalId(2), None, RevisionId(2))]
    );
    let main = k.branch(MAIN).unwrap();
    assert_eq!(main.logical_by_key("y"), Some(LogicalId(2)));
    assert_eq!(main.head(LogicalId(2)), Some(RevisionId(2)));
    assert_eq!(main.head(LogicalId(1)), None);
    let branch = k.branch(b).unwrap();
    assert_eq!(branch.head(LogicalId(1)), Some(RevisionId(1)));
    assert_eq!(branch.head(LogicalId(0)), Some(RevisionId(0)));
}

/// Read sets through the kernel use and report global ids.
#[test]
fn read_sets_through_the_kernel() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&Batch::new(vec![create("pad", 9)])).unwrap();
    // On the branch, "x" gets global logical 2 and revision 2.
    k.commit(&on(b, vec![create("x", 1)])).unwrap();
    k.commit(&on(
        b,
        vec![
            Op::AddRevision {
                logical: LogicalRef::Existing(LogicalId(2)),
                content: [2; 32],
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: LogicalRef::Existing(LogicalId(2)),
                expected: RevisionId(2).into(),
                new: RevRef::New(0),
            },
        ],
    ))
    .unwrap();
    assert_eq!(
        k.commit(&on(
            b,
            vec![Op::ExpectHeads {
                entries: vec![(LogicalRef::Existing(LogicalId(2)), RevisionId(2))],
                source: ReadSource::Client,
            }],
        )),
        Err(TxError::ReadSetConflict {
            logical: LogicalId(2),
            read: RevisionId(2),
            now: RevisionId(3),
        })
    );
}
