//! P6 read sets (SPEC §6.1 contract 12, ADR 0013 §4): `ExpectHeads` against
//! the pre-batch heads, at batch end after A5. Every case runs on the kernel
//! and the oracle, which must agree on results, inputs and derived state.
use contextdb_core::Db;
use contextdb_model::*;
use contextdb_oracle::{InputModel, jacobi};

fn create(key: &str, content: u8) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [content; 32],
        adopt_as_base: true,
    }
}
fn revise(l: u32, content: u8) -> Op {
    Op::AddRevision {
        logical: LogicalRef::Existing(LogicalId(l)),
        content: [content; 32],
        adopt_as_base: true,
    }
}
fn set_head(l: u32, expected: u32, new: RevRef) -> Op {
    Op::SetHead {
        logical: LogicalRef::Existing(LogicalId(l)),
        expected: RevisionId(expected).into(),
        new,
    }
}
fn expect(entries: &[(LogicalRef, u32)]) -> Op {
    Op::ExpectHeads {
        entries: entries
            .iter()
            .map(|(l, r)| (l.clone(), RevisionId(*r)))
            .collect(),
        source: ReadSource::Client,
    }
}
fn at(l: u32) -> LogicalRef {
    LogicalRef::Existing(LogicalId(l))
}

/// Applies `b` to both and checks they agree; returns the kernel's result.
fn both(db: &mut Db, oracle: &mut InputModel, b: &Batch) -> Result<CommitResult, TxError> {
    let want = oracle.apply(b);
    let got = db.commit(b);
    assert_eq!(
        got.as_ref().cloned().map(|r| ApplyResult {
            epoch: r.epoch,
            allocations: r.allocations.clone(),
        }),
        want.as_ref().cloned(),
        "kernel and oracle disagree on {b:?}"
    );
    assert_eq!(db.inputs(), *oracle.inputs());
    assert_eq!(db.derived(), &jacobi(oracle.inputs()));
    got
}
/// A, B and C exist at revisions 0, 1 and 2.
fn setup() -> (Db, InputModel) {
    let (mut db, mut oracle) = (Db::default(), InputModel::default());
    both(
        &mut db,
        &mut oracle,
        &Batch::new(vec![create("a", 0), create("b", 1), create("c", 2)]),
    )
    .unwrap();
    (db, oracle)
}

/// T19 (raw ExpectHeads): two plans read A and B and write different keys;
/// the second to commit fails, first committer wins.
#[test]
fn t19_write_skew_second_plan_fails() {
    let (mut db, mut oracle) = setup();
    let plan = [(at(0), 0), (at(1), 1)];
    // Plan 1 moves A to a new revision.
    let mut first = vec![expect(&plan), revise(0, 10)];
    first.push(set_head(0, 0, RevRef::New(0)));
    both(&mut db, &mut oracle, &Batch::new(first)).unwrap();
    // Plan 2 read the same heads and writes C only.
    let second = vec![expect(&plan), revise(2, 20), set_head(2, 2, RevRef::New(0))];
    assert_eq!(
        both(&mut db, &mut oracle, &Batch::new(second)),
        Err(TxError::ReadSetConflict {
            logical: LogicalId(0),
            read: RevisionId(0),
            now: RevisionId(3),
        })
    );
}

/// Review B3: a plan that reads A and supersedes it in the same batch commits.
#[test]
fn read_then_supersede_in_one_batch_commits() {
    let (mut db, mut oracle) = setup();
    let batch = vec![
        expect(&[(at(0), 0)]),
        revise(0, 10),
        set_head(0, 0, RevRef::New(0)),
    ];
    both(&mut db, &mut oracle, &Batch::new(batch)).unwrap();
    assert_eq!(db.head(LogicalId(0)), Some(RevisionId(3)));
}

/// Keys resolve; unknown logicals and revisions are `UnknownRef`.
#[test]
fn references_resolve_or_are_unknown() {
    let (mut db, mut oracle) = setup();
    both(
        &mut db,
        &mut oracle,
        &Batch::new(vec![expect(&[(LogicalRef::Key("b".into()), 1)])]),
    )
    .unwrap();
    for bad in [
        expect(&[(LogicalRef::Key("missing".into()), 0)]),
        expect(&[(at(9), 0)]),
        expect(&[(at(0), 9)]),
    ] {
        assert_eq!(
            both(&mut db, &mut oracle, &Batch::new(vec![bad])),
            Err(TxError::UnknownRef)
        );
    }
}

/// A logical created in the batch has no pre-batch head; one logical may not
/// be read at two revisions.
#[test]
fn invalid_read_sets() {
    let (mut db, mut oracle) = setup();
    let cases = [
        (
            vec![create("d", 3), expect(&[(LogicalRef::New(0), 3)])],
            LogicalId(3),
        ),
        (
            vec![create("d", 3), expect(&[(LogicalRef::Key("d".into()), 3)])],
            LogicalId(3),
        ),
        (vec![expect(&[(at(0), 0), (at(0), 1)])], LogicalId(0)),
        (
            vec![expect(&[(at(0), 0)]), expect(&[(at(0), 1)])],
            LogicalId(0),
        ),
    ];
    for (ops, logical) in cases {
        assert_eq!(
            both(&mut db, &mut oracle, &Batch::new(ops)),
            Err(TxError::InvalidReadSet { logical })
        );
    }
    // Repeating the same entry is fine.
    both(
        &mut db,
        &mut oracle,
        &Batch::new(vec![expect(&[(at(0), 0)]), expect(&[(at(0), 0)])]),
    )
    .unwrap();
}

/// End of batch: A5 `PolicyViolation` is reported before `ReadSetConflict`.
#[test]
fn policy_violation_precedes_read_set_conflict() {
    let (mut db, mut oracle) = setup();
    let policy = Op::SetPolicy {
        policy: PolicyDoc {
            version: 1,
            require_deps: [RecordKind::Decision].into_iter().collect(),
        },
    };
    let mut admin = Batch::new(vec![policy]);
    admin.channel = Channel::Admin;
    both(&mut db, &mut oracle, &admin).unwrap();
    let decision = Op::CreateLogical {
        key: "d".into(),
        kind: RecordKind::Decision,
        scope_lane: "repo".into(),
        content: [7; 32],
        adopt_as_base: true,
    };
    // A stale read and a base decision: A5 wins.
    let err = both(
        &mut db,
        &mut oracle,
        &Batch::new(vec![expect(&[(at(0), 1)]), decision]),
    );
    assert!(
        matches!(err, Err(TxError::PolicyViolation { .. })),
        "{err:?}"
    );
}

/// A stale read alone fails atomically: nothing in the batch commits.
#[test]
fn stale_read_rejects_the_whole_batch() {
    let (mut db, mut oracle) = setup();
    let digest = db.digest();
    assert!(matches!(
        both(
            &mut db,
            &mut oracle,
            &Batch::new(vec![create("d", 3), expect(&[(at(1), 0)])]),
        ),
        Err(TxError::ReadSetConflict { .. })
    ));
    assert_eq!(db.digest(), digest);
    assert_eq!(db.logical_by_key("d"), None);
}

/// Branch ops never reach a single-branch engine: both reject them.
#[test]
fn branch_ops_need_the_multi_branch_kernel() {
    let (mut db, mut oracle) = setup();
    for op in [
        Op::CreateBranch { name: "b".into() },
        Op::ArchiveBranch { name: "b".into() },
        Op::Merge {
            source: BranchId(1),
            require_certified: true,
            certify: Vec::new(),
        },
    ] {
        assert_eq!(
            both(&mut db, &mut oracle, &Batch::new(vec![op])),
            Err(TxError::UnknownBranch)
        );
    }
}
