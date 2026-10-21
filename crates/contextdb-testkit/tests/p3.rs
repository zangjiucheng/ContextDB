//! P3 engine goldens (SPEC Gate P3 G2): T06d-sem, T16, T16b-cert, T21,
//! T62-P3 and T65. Every commit is also checked against the oracle.
use contextdb_core::{Db, HistoryUnavailable, IssueRequest, LogLink};
use contextdb_model::*;
use contextdb_oracle::jacobi;
use std::collections::BTreeMap;

const SCOPE: &str = "repo";

struct Rig {
    db: Db,
}
impl Rig {
    fn new() -> Self {
        Self { db: Db::default() }
    }
    fn ok(&mut self, ops: Vec<Op>) -> CommitResult {
        self.batch(Batch::new(ops))
    }
    fn observer(&mut self, ops: Vec<Op>) -> CommitResult {
        let mut b = Batch::new(ops);
        b.channel = Channel::Observer;
        self.batch(b)
    }
    fn batch(&mut self, b: Batch) -> CommitResult {
        let r = self.db.commit(&b).expect("batch accepted");
        assert_eq!(self.db.derived(), &jacobi(&self.db.inputs()));
        r
    }
    fn summary(&self, r: u32) -> Summary {
        self.db.status(RevisionId(r)).unwrap()
    }
    fn issue(&self, entries: &[(u32, u32)], strict: bool) -> Result<Certificate, IssueFailure> {
        let items: Vec<CertItem> = entries
            .iter()
            .map(|(l, r)| CertItem {
                logical: LogicalId(*l),
                rev: RevisionId(*r),
                required: true,
                via: Via::Head,
                since_epoch: self.db.epoch(),
            })
            .collect();
        self.db
            .issue_certificate(&request(strict), &items, &BTreeMap::new())
    }
    fn validate(&self, c: &Certificate, live: &BTreeMap<String, Vec<u8>>) -> Validation {
        self.db
            .validate_certificate(
                c,
                live,
                LogLink {
                    matches: true,
                    current_checksum: [0; 32],
                },
            )
            .unwrap()
    }
}
fn request(strict: bool) -> IssueRequest {
    IssueRequest {
        session_id: [1; 16],
        principal: PrincipalId(1),
        branch: MAIN,
        log_checksum: [0; 32],
        code_commit: "head".into(),
        strict_no_aba: strict,
    }
}
fn create(key: &str, base: bool, content: u8) -> Op {
    contextdb_testkit::create(key, RecordKind::Fact, base, content)
}
fn revision(l: u32, content: u8) -> Op {
    Op::AddRevision {
        logical: LogicalId(l).into(),
        content: [content; 32],
        adopt_as_base: true,
    }
}
fn head(l: u32, expected: u32, new: u32) -> Op {
    Op::SetHead {
        logical: LogicalId(l).into(),
        expected: RevisionId(expected).into(),
        new: RevisionId(new).into(),
    }
}
fn rule(c: u32, premises: &[(u32, EdgeKind)]) -> Op {
    Op::AddRule {
        conclusion: RevisionId(c).into(),
        incidences: premises
            .iter()
            .map(|(p, k)| (RevisionId(*p).into(), *k))
            .collect(),
        enabled: true,
    }
}
fn validate_rule(j: u32, x: u32, v: u32) -> Op {
    Op::RecordValidation {
        rule: RuleId(j).into(),
        curvec_seen: vec![(LogicalId(x), RevisionId(v))],
        verdict: Verdict::Verified,
        validator: "human".into(),
    }
}
/// X (logical 0): v1 = r0 base. D (logical 1): r1 with rule 0 `D ← X@r0 [Sem]`.
fn sem_fixture() -> Rig {
    let mut rig = Rig::new();
    rig.ok(vec![create("x", true, 0), create("d", false, 1)]);
    rig.ok(vec![rule(1, &[(0, EdgeKind::Sem)])]);
    assert_eq!(rig.summary(1), Summary::Certified);
    rig
}

#[test]
fn t06d_sem_validation_or_rebind_restores_without_new_revision() {
    let mut rig = sem_fixture();
    rig.ok(vec![revision(0, 10), head(0, 0, 2)]);
    assert_eq!(rig.summary(1), Summary::Pending(PendingReason::Semantic));
    // A validation record for the new vector certifies D: no new revision.
    let revisions = rig.db.revision_count();
    let result = rig.ok(vec![validate_rule(0, 0, 2)]);
    assert_eq!(rig.summary(1), Summary::Certified);
    assert_eq!(rig.db.revision_count(), revisions);
    assert_eq!(result.status_diff, vec![RevisionId(1)]);
    // Rebind is the other way back: the asserted vector becomes current.
    let mut rig = sem_fixture();
    rig.ok(vec![revision(0, 10), head(0, 0, 2)]);
    rig.ok(vec![Op::Rebind {
        rule: RuleId(0).into(),
        from: RevisionId(0).into(),
        to: RevisionId(2).into(),
    }]);
    assert_eq!(rig.summary(1), Summary::Certified);
}

#[test]
fn t16_certificate_bound_to_old_head_is_invalid() {
    let mut rig = Rig::new();
    rig.ok(vec![create("x", true, 0)]);
    let cert = rig.issue(&[(0, 0)], false).unwrap();
    rig.ok(vec![revision(0, 1), head(0, 0, 1)]);
    assert_eq!(
        rig.validate(&cert, &BTreeMap::new()),
        Validation::Invalid {
            reason: InvalidReason::Changed,
            diffs: vec![CertDiff {
                logical: LogicalId(0),
                cert_rev: Some(RevisionId(0)),
                current: Some(RevisionId(1)),
                summary: Some(Summary::Certified),
            }],
        }
    );
}

#[test]
fn t16b_cert_aba_valid_unless_strict_and_anchor_base_aba_is_strict_invalid() {
    let mut rig = Rig::new();
    rig.ok(vec![create("x", true, 0), create("d", false, 1)]);
    rig.ok(vec![rule(1, &[(0, EdgeKind::Cur)])]);
    let loose = rig.issue(&[(1, 1)], false).unwrap();
    let strict = rig.issue(&[(1, 1)], true).unwrap();
    assert_eq!(
        strict.vector.iter().map(|v| v.rev).collect::<Vec<_>>(),
        vec![RevisionId(0), RevisionId(1)]
    );
    // X: v1 → v2 → v1 (ABA) across two batches.
    rig.ok(vec![revision(0, 5), head(0, 0, 2)]);
    rig.ok(vec![head(0, 2, 0)]);
    assert_eq!(rig.validate(&loose, &BTreeMap::new()), Validation::Valid);
    let Validation::Invalid {
        reason: InvalidReason::StrictChange,
        diffs,
    } = rig.validate(&strict, &BTreeMap::new())
    else {
        panic!("strict ABA must be Invalid{{StrictChange}}");
    };
    assert_eq!(diffs[0].logical, LogicalId(0));

    // Anchor base retracted and restored (observer channel) under strict.
    let mut rig = Rig::new();
    rig.observer(vec![Op::CreateLogical {
        key: "code:repo/a.rs".into(),
        kind: RecordKind::CodeAnchor,
        scope_lane: SCOPE.into(),
        content: [7; 32],
        adopt_as_base: true,
    }]);
    rig.ok(vec![create("plan", false, 1)]);
    rig.ok(vec![rule(1, &[(0, EdgeKind::Cur)])]);
    let loose = rig.issue(&[(1, 1)], false).unwrap();
    let strict = rig.issue(&[(1, 1)], true).unwrap();
    rig.observer(vec![Op::RetractBase {
        rev: RevisionId(0).into(),
    }]);
    rig.observer(vec![Op::AssertBase {
        rev: RevisionId(0).into(),
    }]);
    assert_eq!(rig.validate(&loose, &BTreeMap::new()), Validation::Valid);
    assert!(matches!(
        rig.validate(&strict, &BTreeMap::new()),
        Validation::Invalid {
            reason: InvalidReason::StrictChange,
            ..
        }
    ));
}

#[test]
fn t21_two_effective_versions_of_one_logical_is_ambiguous() {
    let mut rig = Rig::new();
    rig.ok(vec![create("x", true, 0), revision(0, 1)]);
    assert_eq!(
        rig.issue(&[(0, 0), (0, 1)], false),
        Err(IssueFailure::Ambiguous {
            logical: LogicalId(0)
        })
    );
    assert_eq!(rig.issue(&[], false), Err(IssueFailure::Empty));
}

#[test]
fn t62_p3_unrelated_commits_keep_certificate_valid_and_validate_has_no_epoch() {
    // Signature check: validation takes no epoch parameter (SD16).
    type Validate = fn(
        &Db,
        &Certificate,
        &BTreeMap<String, Vec<u8>>,
        LogLink,
    ) -> Result<Validation, HistoryUnavailable>;
    let _: Validate = Db::validate_certificate;
    let mut rig = Rig::new();
    rig.ok(vec![create("x", true, 0), create("unrelated", true, 1)]);
    let cert = rig.issue(&[(0, 0)], true).unwrap();
    for n in 0..5u8 {
        rig.ok(vec![revision(1, 10 + n)]);
    }
    assert!(rig.db.epoch() > cert.issued_epoch);
    assert_eq!(rig.validate(&cert, &BTreeMap::new()), Validation::Valid);
}

#[test]
fn t65_verdict_priority() {
    // Invalid and Unknown together → Invalid: X superseded, the anchor lagging.
    let mut rig = Rig::new();
    rig.observer(vec![Op::CreateLogical {
        key: "code:repo/a.rs".into(),
        kind: RecordKind::CodeAnchor,
        scope_lane: SCOPE.into(),
        content: [7; 32],
        adopt_as_base: true,
    }]);
    rig.observer(vec![Op::ObserverWatermark {
        scope: SCOPE.into(),
        expected: None,
        position: vec![1],
    }]);
    rig.ok(vec![create("x", true, 0), create("y", false, 1)]);
    rig.ok(vec![rule(2, &[(0, EdgeKind::Cur)])]);
    let cert = rig.issue(&[(1, 1), (2, 2)], false).unwrap();
    let lagging = BTreeMap::from([(SCOPE.to_owned(), vec![2])]);
    assert!(matches!(
        rig.validate(&cert, &lagging),
        Validation::Unknown { .. }
    ));
    rig.ok(vec![revision(1, 9), head(1, 1, 3)]);
    assert!(matches!(
        rig.validate(&cert, &lagging),
        Validation::Invalid {
            reason: InvalidReason::Changed,
            ..
        }
    ));

    // Pending and Unknown together → Unknown.
    let mut rig = sem_fixture();
    rig.observer(vec![Op::CreateLogical {
        key: "code:repo/a.rs".into(),
        kind: RecordKind::CodeAnchor,
        scope_lane: SCOPE.into(),
        content: [7; 32],
        adopt_as_base: true,
    }]);
    rig.ok(vec![create("y", false, 3)]);
    rig.ok(vec![rule(3, &[(2, EdgeKind::Cur)])]);
    let cert = rig.issue(&[(1, 1), (3, 3)], false).unwrap();
    rig.ok(vec![revision(0, 10), head(0, 0, 4)]);
    assert!(matches!(
        rig.validate(&cert, &BTreeMap::new()),
        Validation::Pending { .. }
    ));
    let lagging = BTreeMap::from([(SCOPE.to_owned(), vec![5])]);
    assert!(matches!(
        rig.validate(&cert, &lagging),
        Validation::Unknown { .. }
    ));

    // Sem head moved and revalidated → ValidRecertified (vector X v1 → v2).
    let mut rig = sem_fixture();
    let loose = rig.issue(&[(1, 1)], false).unwrap();
    let strict = rig.issue(&[(1, 1)], true).unwrap();
    rig.ok(vec![revision(0, 10), head(0, 0, 2)]);
    rig.ok(vec![validate_rule(0, 0, 2)]);
    let Validation::ValidRecertified { new_cert, diffs } = rig.validate(&loose, &BTreeMap::new())
    else {
        panic!("revalidated Sem head must recertify");
    };
    assert_eq!(
        diffs,
        vec![CertDiff {
            logical: LogicalId(0),
            cert_rev: Some(RevisionId(0)),
            current: Some(RevisionId(2)),
            summary: Some(Summary::Certified),
        }]
    );
    assert_eq!(rig.validate(&new_cert, &BTreeMap::new()), Validation::Valid);
    // Strict: the same head move is Invalid{StrictChange}, never recertified.
    assert!(matches!(
        rig.validate(&strict, &BTreeMap::new()),
        Validation::Invalid {
            reason: InvalidReason::StrictChange,
            ..
        }
    ));
}
