//! P6 branch-aware reads (ADR 0013 §7; protocol 4): `BranchView` answers
//! every read with global ids. With only `main` it equals the single-branch
//! database; on branches, ids are global and certificates are per branch
//! (T62 across branches, `BranchMismatch`); `main` annotates `BRANCH_ONLY`
//! in current-mode reads only (S13).
use contextdb_core::{Db, IssueRequest, Kernel, LogLink};
use contextdb_model::*;
use std::collections::BTreeMap;

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
fn request(branch: BranchId) -> IssueRequest {
    IssueRequest {
        session_id: [1; 16],
        principal: PrincipalId(0),
        branch,
        log_checksum: [0; 32],
        code_commit: "c".into(),
        strict_no_aba: false,
    }
}
fn entry(l: LogicalId, r: RevisionId) -> CertItem {
    CertItem {
        logical: l,
        rev: r,
        required: true,
        via: Via::Head,
        since_epoch: Epoch(0),
    }
}
const LINK: LogLink = LogLink {
    matches: true,
    current_checksum: [0; 32],
};

/// With only `main`, every read through the view equals the database's.
#[test]
fn main_view_equals_db() {
    let live = BTreeMap::new();
    for seed in 0..300 {
        let (mut k, mut db) = (Kernel::default(), Db::default());
        for b in contextdb_testkit::generate_p1(seed).batches {
            assert_eq!(k.commit(&b), db.commit(&b));
        }
        let v = k.branch(MAIN).unwrap();
        for n in 0..db.revision_count() as u32 {
            let r = RevisionId(n);
            assert_eq!(v.node(r), db.node(r), "seed {seed} r{n}");
            assert_eq!(v.summary_at(r, &live), db.summary_at(r, &live));
            assert_eq!(v.why(r), db.why(r));
            assert_eq!(v.why_depth(r, 3), db.why_depth(r, 3));
            assert_eq!(v.why_not(r, 3), db.why_not(r, 3));
            assert_eq!(v.impact_reachable(&[r]), db.impact_reachable(&[r]));
            let mut rules = db.rules_concluding(r);
            for (_, _, premises, refs) in &mut rules {
                premises.sort_unstable();
                refs.sort_unstable();
            }
            assert_eq!(v.rules_concluding(r), rules);
            assert_eq!(
                v.read_historical(r, k.epoch()),
                db.read_historical(r, db.epoch())
            );
        }
        for n in 0..db.logical_count() as u32 {
            let l = LogicalId(n);
            assert_eq!(v.current(l), db.current(l));
            assert_eq!(
                v.read_snapshot(l, k.epoch()),
                db.read_snapshot(l, db.epoch())
            );
            let head = db.head(l).unwrap();
            let entries = [entry(l, head)];
            let (vc, dc) = (
                v.issue_certificate(&request(MAIN), &entries, &live),
                db.issue_certificate(&request(MAIN), &entries, &live),
            );
            assert_eq!(vc, dc, "seed {seed} l{n}");
            if let Ok(c) = vc {
                assert_eq!(
                    v.validate_certificate(&c, &live, LINK),
                    db.validate_certificate(&c, &live, LINK)
                );
            }
        }
        assert_eq!(v.diff(Epoch(0), k.epoch()), db.diff(Epoch(0), db.epoch()));
    }
}

/// S13 `BRANCH_ONLY`: on `main`, A@0 is superseded by A@1 and branch b sets
/// its head back to A@0. `main` reads A@0 as BRANCH_ONLY (summary STALE
/// HeadSuperseded) in current-mode reads only; its digest is unchanged;
/// after b is archived A@0 reads Superseded again. Never on b itself.
#[test]
fn s13_branch_only_on_main() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0)])).unwrap(); // A@0 = r0
    k.commit(&Batch::new(vec![
        Op::AddRevision {
            logical: LogicalRef::Existing(LogicalId(0)),
            content: [1; 32],
            adopt_as_base: true,
        },
        Op::SetHead {
            logical: LogicalRef::Existing(LogicalId(0)),
            expected: RevisionId(0).into(),
            new: RevRef::New(0),
        },
    ]))
    .unwrap(); // A@1 = r1
    let b = fork(&mut k, MAIN, "b");
    let main_digest = k.branch(MAIN).unwrap().digest();
    let at = k.epoch();
    k.commit(&on(
        b,
        vec![Op::SetHead {
            logical: LogicalRef::Existing(LogicalId(0)),
            expected: RevisionId(1).into(),
            new: RevisionId(0).into(),
        }],
    ))
    .unwrap();
    let main = k.branch(MAIN).unwrap();
    let a0 = main.node(RevisionId(0)).unwrap();
    assert_eq!(a0.dimensions.currency, Currency::BranchOnly);
    assert_eq!(
        main.status(RevisionId(0)).unwrap(),
        Summary::Stale(StaleReason::HeadSuperseded)
    );
    assert_eq!(main.digest(), main_digest, "read-time only");
    // Historical reads never report it.
    let historical = main.read_historical(RevisionId(0), at).unwrap();
    assert_eq!(historical.dimensions.currency, Currency::Superseded);
    // On b, its own head is Current; b never reports BRANCH_ONLY.
    let on_b = k.branch(b).unwrap();
    assert_eq!(
        on_b.node(RevisionId(0)).unwrap().dimensions.currency,
        Currency::Current
    );
    assert_eq!(
        on_b.node(RevisionId(1)).unwrap().dimensions.currency,
        Currency::Superseded
    );
    // Archived branches no longer count.
    k.commit(&on(b, vec![Op::ArchiveBranch { name: "b".into() }]))
        .unwrap();
    let main = k.branch(MAIN).unwrap();
    assert_eq!(
        main.node(RevisionId(0)).unwrap().dimensions.currency,
        Currency::Superseded
    );
}

/// Ids on a branch are global even when the branch database renumbers.
#[test]
fn branch_reads_use_global_ids() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("p", 0)])).unwrap(); // r0
    let b = fork(&mut k, MAIN, "b");
    // Main allocates r1 and a rule, so b's next ids are renumbered.
    k.commit(&Batch::new(vec![
        create("q", 1),
        Op::AddRule {
            conclusion: RevRef::New(0),
            incidences: vec![(RevisionId(0).into(), EdgeKind::Pin)],
            enabled: true,
        },
    ]))
    .unwrap();
    let mut x = create("x", 2);
    if let Op::CreateLogical { adopt_as_base, .. } = &mut x {
        *adopt_as_base = false;
    }
    let r = k
        .commit(&on(
            b,
            vec![
                x,
                Op::AddRule {
                    conclusion: RevRef::New(0),
                    incidences: vec![(RevisionId(0).into(), EdgeKind::Pin)],
                    enabled: true,
                },
            ],
        ))
        .unwrap();
    let (rev, rule) = (r.allocations.new_revisions[0], r.allocations.new_rules[0]);
    assert_eq!((rev, rule), (RevisionId(2), RuleId(1)));
    let v = k.branch(b).unwrap();
    let node = v.node(rev).unwrap();
    assert_eq!(node.witness[0], Some(Witness::Rule(rule)));
    assert_eq!(v.why(rev).unwrap().active_rules, vec![rule]);
    assert_eq!(v.rules_concluding(rev)[0].0, rule);
    assert_eq!(v.revision_logical(rev), Some(LogicalId(2)));
    assert_eq!(
        v.logicals().collect::<Vec<_>>(),
        vec![LogicalId(0), LogicalId(2)]
    );
    let impact = v.impact_reachable(&[RevisionId(0)]).unwrap();
    assert_eq!(impact.supported, vec![rev]);
    // A certificate names global ids and hashes them.
    let live = BTreeMap::new();
    let c = v
        .issue_certificate(&request(b), &[entry(LogicalId(2), rev)], &live)
        .unwrap();
    let ids: Vec<_> = c.vector.iter().map(|i| (i.logical, i.rev)).collect();
    assert_eq!(
        ids,
        vec![(LogicalId(0), RevisionId(0)), (LogicalId(2), rev)]
    );
    assert_eq!(c.cert_id, *blake3::hash(&c.body_bytes()).as_bytes());
    assert_eq!(c.issued_epoch, k.epoch(), "the global epoch");
}

/// T62 across branches: commits on other branches never change a
/// certificate's verdict; on another branch it is `BranchMismatch`.
#[test]
fn t62_certificates_are_per_branch() {
    let live = BTreeMap::new();
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0), create("b", 1)]))
        .unwrap();
    let b = fork(&mut k, MAIN, "b");
    let entries = [entry(LogicalId(0), RevisionId(0))];
    let mut strict = request(b);
    strict.strict_no_aba = true;
    let cert = k
        .branch(b)
        .unwrap()
        .issue_certificate(&strict, &entries, &live)
        .unwrap();
    // Main moves A and retracts B; b is untouched.
    k.commit(&Batch::new(vec![
        Op::AddRevision {
            logical: LogicalRef::Existing(LogicalId(0)),
            content: [9; 32],
            adopt_as_base: true,
        },
        Op::SetHead {
            logical: LogicalRef::Existing(LogicalId(0)),
            expected: RevisionId(0).into(),
            new: RevRef::New(0),
        },
        Op::RetractBase {
            rev: RevisionId(1).into(),
        },
    ]))
    .unwrap();
    k.commit(&on(b, vec![create("c", 2)])).unwrap();
    assert_eq!(
        k.branch(b)
            .unwrap()
            .validate_certificate(&cert, &live, LINK),
        Ok(Validation::Valid)
    );
    assert_eq!(
        k.branch(MAIN)
            .unwrap()
            .validate_certificate(&cert, &live, LINK),
        Ok(Validation::Invalid {
            reason: InvalidReason::BranchMismatch,
            diffs: Vec::new(),
        })
    );
    // A move on b itself invalidates it (strict).
    k.commit(&on(
        b,
        vec![
            Op::AddRevision {
                logical: LogicalRef::Existing(LogicalId(0)),
                content: [8; 32],
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: LogicalRef::Existing(LogicalId(0)),
                expected: RevisionId(0).into(),
                new: RevRef::New(0),
            },
        ],
    ))
    .unwrap();
    assert!(matches!(
        k.branch(b)
            .unwrap()
            .validate_certificate(&cert, &live, LINK),
        Ok(Validation::Invalid { .. })
    ));
}
