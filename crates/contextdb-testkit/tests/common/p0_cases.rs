use contextdb_core::{Db, policy_hash};
use contextdb_model::*;
use contextdb_oracle::{InputModel, cert_verdict, jacobi, kahn, summarize};
use contextdb_testkit::create;
use std::collections::{BTreeMap, BTreeSet};

fn r(n: u32) -> RevRef {
    RevisionId(n).into()
}
fn l(n: u32) -> LogicalRef {
    LogicalId(n).into()
}
fn j(n: u32) -> RuleRef {
    RuleId(n).into()
}
fn add(c: u32, ps: &[(u32, EdgeKind)]) -> Op {
    Op::AddRule {
        conclusion: r(c),
        incidences: ps.iter().map(|(n, k)| (r(*n), *k)).collect(),
        enabled: true,
    }
}
fn head(logical: u32, expected: u32, new: u32) -> Op {
    Op::SetHead {
        logical: l(logical),
        expected: r(expected),
        new: r(new),
    }
}
fn revision(logical: u32, content: u8, base: bool) -> Op {
    Op::AddRevision {
        logical: l(logical),
        content: [content; 32],
        adopt_as_base: base,
    }
}
fn validation(rule: u32, logical: u32, rev: u32, verdict: Verdict) -> Op {
    Op::RecordValidation {
        rule: j(rule),
        curvec_seen: vec![(LogicalId(logical), RevisionId(rev))],
        verdict,
        validator: "test".into(),
    }
}
struct Rig {
    db: Db<super::ExactMode, contextdb_core::CommitStats>,
    oracle: InputModel,
}
impl Default for Rig {
    fn default() -> Self {
        Self {
            db: Db::new(),
            oracle: InputModel::default(),
        }
    }
}
impl Rig {
    fn apply(&mut self, b: Batch) -> Result<CommitResult, TxError> {
        let digest = self.db.digest();
        let epoch = self.db.epoch();
        let original = self.db.inputs();
        let indexes = self.db.indexes().clone();
        let topo = self.db.topo();
        let reference = self.oracle.apply(&b);
        let result = self.db.commit(&b);
        assert_eq!(
            result.as_ref().map(|r| ApplyResult {
                epoch: r.epoch,
                allocations: r.allocations.clone()
            }),
            reference.as_ref().cloned()
        );
        assert_eq!(self.db.inputs(), *self.oracle.inputs());
        assert_eq!(self.db.derived(), &jacobi(self.oracle.inputs()));
        assert_eq!(self.db.derived(), &kahn(self.oracle.inputs()).unwrap());
        if result.is_err() {
            assert_eq!(self.db.digest(), digest);
            assert_eq!(self.db.epoch(), epoch);
            assert_eq!(self.db.inputs(), original);
            assert_eq!(self.db.indexes(), &indexes);
            assert_eq!(self.db.topo(), topo);
        }
        result
    }
    fn ok(&mut self, ops: Vec<Op>) -> CommitResult {
        self.apply(Batch::new(ops)).unwrap()
    }
    fn err(&mut self, ops: Vec<Op>, error: TxError) {
        assert_eq!(self.apply(Batch::new(ops)), Err(error));
    }
    fn nodes(&mut self, bases: &[bool]) {
        self.ok(bases
            .iter()
            .enumerate()
            .map(|(n, b)| create(&format!("n{n}"), RecordKind::Fact, *b, n as u8))
            .collect());
    }
    fn s(&self, n: u32) -> Summary {
        self.db.status(RevisionId(n)).unwrap()
    }
}

#[test]
fn t01_t02_t03_conjunction_alternatives_and_canonical_witness() {
    let mut x = Rig::default();
    x.nodes(&[true, true, false, true]);
    x.ok(vec![
        add(2, &[(0, EdgeKind::Pin), (1, EdgeKind::Pin)]),
        add(2, &[(3, EdgeKind::Pin)]),
    ]);
    assert_eq!(x.db.derived().nodes[2].support, 2);
    assert_eq!(
        x.db.why(RevisionId(2)).unwrap().witness,
        Some(Witness::Rule(RuleId(0)))
    );
    let result = x.ok(vec![Op::RetractBase { rev: r(0) }]);
    assert_eq!(result.l0_flips, vec![RevisionId(0)]);
    let d = x.db.derived();
    assert_eq!(d.unsat, vec![1, 0]);
    assert_eq!(d.active, vec![false, true]);
    assert_eq!(d.nodes[2].support, 1);
    assert_eq!(d.nodes[2].witness[0], Some(Witness::Rule(RuleId(1))));
    x.ok(vec![Op::RetractBase { rev: r(3) }]);
    assert_eq!(x.db.derived().nodes[2].support, 0);
    assert_eq!(x.s(2), Summary::Stale(StaleReason::Unsupported));
    let mut y = Rig::default();
    y.nodes(&[true, true, false]);
    y.ok(vec![add(2, &[(0, EdgeKind::Pin), (1, EdgeKind::Pin)])]);
    y.ok(vec![Op::RetractBase { rev: r(0) }]);
    assert!(!y.db.derived().nodes[2].l0);
    assert_eq!(y.db.derived().unsat, vec![1]);
}
#[test]
fn t04_chain_exact_flips_and_alternative_save() {
    for alternative in [false, true] {
        let mut x = Rig::default();
        x.nodes(&[true, false, false, false, true]);
        let mut ops = vec![
            add(1, &[(0, EdgeKind::Pin)]),
            add(2, &[(1, EdgeKind::Pin)]),
            add(3, &[(2, EdgeKind::Pin)]),
        ];
        if alternative {
            ops.push(add(2, &[(4, EdgeKind::Pin)]));
        }
        x.ok(ops);
        let result = x.ok(vec![Op::RetractBase { rev: r(0) }]);
        assert_eq!(
            result.l0_flips,
            if alternative {
                vec![RevisionId(0), RevisionId(1)]
            } else {
                (0..4).map(RevisionId).collect()
            }
        );
        assert_eq!(x.db.derived().nodes[3].l0, alternative);
    }
}
#[test]
fn t06abc_cur_sem_and_alternative_why_not() {
    for edge in [EdgeKind::Cur, EdgeKind::Sem] {
        for alternative in [false, true] {
            let mut x = Rig::default();
            x.nodes(&[true, false, true]);
            let mut ops = vec![add(1, &[(0, edge)]), revision(0, 90, true)];
            if alternative {
                ops.push(add(1, &[(2, EdgeKind::Pin)]));
            }
            x.ok(ops);
            x.ok(vec![head(0, 0, 3)]);
            if alternative {
                assert_eq!(x.s(1), Summary::Certified);
                assert_eq!(
                    x.db.derived().nodes[1].support,
                    if edge == EdgeKind::Cur { 1 } else { 2 }
                );
            } else if edge == EdgeKind::Cur {
                assert_eq!(x.s(1), Summary::Stale(StaleReason::Unsupported));
                let why = x.db.why_not(RevisionId(1), 2).unwrap();
                assert!(
                    why.rules[0]
                        .incidences
                        .contains(&IncidenceFailure::NotCurrent {
                            premise: RevisionId(0),
                            head: RevisionId(3)
                        })
                );
            } else {
                assert_eq!(x.s(1), Summary::Pending(PendingReason::Semantic));
                assert!(x.db.derived().nodes[1].l0);
            }
        }
    }
}
#[test]
fn t07ac_t63_refs_are_non_supporting_and_why_lists_all() {
    let mut x = Rig::default();
    x.nodes(&[true, false, true]);
    x.ok(vec![
        add(1, &[(0, EdgeKind::Pin), (2, EdgeKind::Ref)]),
        add(1, &[(0, EdgeKind::Cur)]),
    ]);
    let why = x.db.why(RevisionId(1)).unwrap();
    assert_eq!(why.active_rules, vec![RuleId(0), RuleId(1)]);
    assert_eq!(why.references, vec![(RuleId(0), RevisionId(2))]);
    assert!(x.db.indexes().rules_by_premise.row(2).is_empty());
    assert_eq!(x.db.indexes().refs_by_target.row(2), vec![RuleId(0)]);
    x.ok(vec![
        Op::RetractBase { rev: r(2) },
        revision(2, 9, true),
        head(2, 2, 3),
    ]);
    assert_eq!(x.s(1), Summary::Certified);
    assert_eq!(x.db.why(RevisionId(1)).unwrap(), why);
}
#[test]
fn t09abcd_canonical_strength_versions_and_duplicate_disabled() {
    let mut x = Rig::default();
    x.nodes(&[true, false]);
    x.ok(vec![revision(0, 3, true)]);
    x.ok(vec![add(
        1,
        &[
            (0, EdgeKind::Pin),
            (0, EdgeKind::Ref),
            (0, EdgeKind::Sem),
            (0, EdgeKind::Cur),
            (0, EdgeKind::Pin),
        ],
    )]);
    assert_eq!(
        x.db.inputs().rules[0].premises,
        vec![Incidence {
            premise: RevisionId(0),
            kind: EdgeKind::Cur
        }]
    );
    assert!(x.db.inputs().rules[0].references.is_empty());
    x.err(
        vec![add(1, &[(0, EdgeKind::Cur)])],
        TxError::DuplicateRule {
            existing: RuleId(0),
            enabled: true,
        },
    );
    x.ok(vec![Op::SetRuleEnabled {
        rule: j(0),
        enabled: false,
    }]);
    x.err(
        vec![add(1, &[(0, EdgeKind::Cur)])],
        TxError::DuplicateRule {
            existing: RuleId(0),
            enabled: false,
        },
    );
    for a in [EdgeKind::Pin, EdgeKind::Cur, EdgeKind::Ref] {
        for b in [EdgeKind::Pin, EdgeKind::Cur] {
            x.err(
                vec![add(1, &[(0, a), (2, b)])],
                TxError::InvalidRule(InvalidRule::ConflictingVersions(LogicalId(0))),
            );
        }
    }
    // Disabled rules still track incidence satisfaction.
    x.ok(vec![Op::RetractBase { rev: r(0) }]);
    assert_eq!(x.db.derived().unsat, vec![1]);
    assert_eq!(x.db.derived().sat, vec![vec![false]]);
}
#[test]
fn t09efgho_rule_admission_and_error_precedence() {
    let mut x = Rig::default();
    x.ok(vec![
        create("e", RecordKind::Evidence, true, 0),
        create("s", RecordKind::Summary, true, 1),
        create("f", RecordKind::Fact, false, 2),
    ]);
    x.err(
        vec![add(0, &[])],
        TxError::InvalidRule(InvalidRule::EmptyPremises),
    );
    x.err(
        vec![add(0, &[(0, EdgeKind::Pin)])],
        TxError::InvalidRule(InvalidRule::SelfPremise),
    );
    x.err(
        vec![add(0, &[(1, EdgeKind::Pin)])],
        TxError::InvalidRule(InvalidRule::ForbiddenConclusion),
    );
    let mut observer = Batch::new(vec![create("code:a", RecordKind::CodeAnchor, true, 7)]);
    observer.channel = Channel::Observer;
    x.apply(observer).unwrap();
    x.err(
        vec![add(3, &[(0, EdgeKind::Pin)])],
        TxError::InvalidRule(InvalidRule::ForbiddenConclusion),
    );
    let mut y = Rig::default();
    y.nodes(&vec![true; 1026]);
    y.err(
        vec![add(
            1025,
            &(0..1025).map(|n| (n, EdgeKind::Pin)).collect::<Vec<_>>(),
        )],
        TxError::InvalidRule(InvalidRule::TooManyPremises),
    );
}
#[test]
fn t09i_evidence_immutable_before_cas_and_dedup() {
    let mut x = Rig::default();
    x.ok(vec![
        create("e", RecordKind::Evidence, true, 0),
        create("f", RecordKind::Fact, true, 1),
    ]);
    x.err(vec![revision(0, 0, true)], TxError::ImmutableKind);
    x.err(vec![head(0, 1, 0)], TxError::ImmutableKind);
    x.err(vec![head(0, 99, 0)], TxError::UnknownRef);
}
#[test]
fn t09j_summary_sem_only() {
    let mut x = Rig::default();
    x.ok(vec![
        create("s", RecordKind::Summary, true, 0),
        create("f", RecordKind::Fact, false, 1),
        create("root", RecordKind::Fact, true, 2),
    ]);
    for kind in [EdgeKind::Pin, EdgeKind::Cur] {
        x.err(
            vec![add(1, &[(0, kind)])],
            TxError::InvalidRule(InvalidRule::ForbiddenPremise),
        );
    }
    // A reference carries no status, so it may name a Summary (A2 r1.6.11).
    x.ok(vec![add(1, &[(0, EdgeKind::Ref), (2, EdgeKind::Pin)])]);
    x.ok(vec![add(1, &[(0, EdgeKind::Sem)])]);
    assert_eq!(x.s(1), Summary::Certified);
}
#[test]
fn t09w_reference_only_rules_never_support() {
    let mut x = Rig::default();
    x.ok(vec![
        create("s", RecordKind::Summary, true, 0),
        create("f", RecordKind::Fact, false, 1),
        create("g", RecordKind::Fact, true, 2),
    ]);
    x.ok(vec![add(1, &[(0, EdgeKind::Ref)])]);
    assert_eq!(x.db.derived().unsat, vec![0]);
    assert_eq!(x.db.derived().active, vec![false]);
    assert_eq!(x.s(1), Summary::Stale(StaleReason::Unsupported));
    x.ok(vec![Op::AssertBase { rev: r(1) }]);
    assert_eq!(x.s(1), Summary::Certified);
    // Its references never move the conclusion, whatever their status.
    x.ok(vec![Op::RetractBase { rev: r(0) }]);
    assert_eq!(x.s(1), Summary::Certified);
    x.ok(vec![Op::SetRuleEnabled {
        rule: RuleId(0).into(),
        enabled: false,
    }]);
    x.ok(vec![Op::SetRuleEnabled {
        rule: RuleId(0).into(),
        enabled: true,
    }]);
    assert_eq!(x.db.derived().active, vec![false]);
    x.ok(vec![Op::RetractBase { rev: r(1) }]);
    assert_eq!(x.s(1), Summary::Stale(StaleReason::Unsupported));
    // A rule with one premise and references still fires on the premise alone.
    x.ok(vec![add(1, &[(0, EdgeKind::Ref), (2, EdgeKind::Pin)])]);
    assert_eq!(x.s(1), Summary::Certified);
    x.err(
        vec![add(1, &[(0, EdgeKind::Ref)])],
        TxError::DuplicateRule {
            existing: RuleId(0),
            enabled: true,
        },
    );
}
#[test]
fn t09kl_metadata_and_import_artifact_do_not_change_derivation() {
    let mut x = Rig::default();
    x.nodes(&[true]);
    let d = x.db.derived_digest();
    x.ok(vec![Op::SetMetadata {
        logical: l(0),
        tags: BTreeSet::from(["tag".into()]),
        dirs: BTreeSet::from(["src".into()]),
    }]);
    assert_eq!(x.db.derived_digest(), d);
    x.ok(vec![Op::SetMetadata {
        logical: l(0),
        tags: BTreeSet::new(),
        dirs: BTreeSet::from(["src/lib".into()]),
    }]);
    x.err(
        vec![Op::SetMetadata {
            logical: l(0),
            tags: BTreeSet::new(),
            dirs: BTreeSet::from(["other".into()]),
        }],
        TxError::InvalidMetadata,
    );
    x.ok(vec![Op::OpenArtifact {
        ty: ArtifactType::ImportBroken,
        target: r(0),
        payload_hash: [1; 32],
    }]);
    assert_eq!(x.db.derived_digest(), d);
    assert!(x.db.inputs().rules.is_empty());
    assert!(x.db.inputs().artifacts[0].open);
}
#[test]
fn t09mv_policy_final_batch_enabled_and_no_base() {
    let mut x = Rig::default();
    x.nodes(&[true]);
    let mut b = Batch::new(vec![Op::SetPolicy {
        policy: PolicyDoc {
            version: 1,
            require_deps: BTreeSet::from([RecordKind::Plan]),
        },
    }]);
    b.channel = Channel::Admin;
    x.apply(b).unwrap();
    let plan = create("p", RecordKind::Plan, false, 7);
    let mut disabled = add(1, &[(0, EdgeKind::Pin)]);
    if let Op::AddRule { enabled, .. } = &mut disabled {
        *enabled = false;
    }
    x.err(
        vec![plan.clone(), disabled],
        TxError::PolicyViolation {
            kind: RecordKind::Plan,
            logical: LogicalId(1),
            reason: PolicyReason::NoEnabledRule,
        },
    );
    x.err(
        vec![
            create("p", RecordKind::Plan, true, 7),
            add(1, &[(0, EdgeKind::Pin)]),
        ],
        TxError::PolicyViolation {
            kind: RecordKind::Plan,
            logical: LogicalId(1),
            reason: PolicyReason::BaseNotAllowed,
        },
    );
    x.err(
        vec![
            plan.clone(),
            add(1, &[(0, EdgeKind::Pin)]),
            Op::AssertBase { rev: r(1) },
        ],
        TxError::PolicyViolation {
            kind: RecordKind::Plan,
            logical: LogicalId(1),
            reason: PolicyReason::BaseNotAllowed,
        },
    );
    x.ok(vec![plan, add(1, &[(0, EdgeKind::Pin)])]);
    x.ok(vec![Op::SetRuleEnabled {
        rule: j(0),
        enabled: false,
    }]);
    assert_eq!(x.s(1), Summary::Stale(StaleReason::Unsupported));
}
#[test]
fn t09n_rebind_errors_atomicity_and_enabled_preservation() {
    let mut x = Rig::default();
    x.nodes(&[true, false, true]);
    x.ok(vec![
        revision(0, 8, true),
        add(1, &[(0, EdgeKind::Cur)]),
        add(1, &[(3, EdgeKind::Cur)]),
    ]);
    x.err(
        vec![Op::Rebind {
            rule: j(0),
            from: r(2),
            to: r(2),
        }],
        TxError::InvalidRebind {
            reason: RebindReason::NotPremise,
        },
    );
    x.err(
        vec![Op::Rebind {
            rule: j(0),
            from: r(0),
            to: r(0),
        }],
        TxError::InvalidRebind {
            reason: RebindReason::SameRevision,
        },
    );
    x.err(
        vec![Op::Rebind {
            rule: j(0),
            from: r(0),
            to: r(2),
        }],
        TxError::LogicalMismatch,
    );
    x.err(
        vec![Op::Rebind {
            rule: j(0),
            from: r(0),
            to: r(3),
        }],
        TxError::DuplicateRule {
            existing: RuleId(1),
            enabled: true,
        },
    );
    x.ok(vec![
        add(2, &[(0, EdgeKind::Pin)]),
        Op::SetRuleEnabled {
            rule: j(2),
            enabled: false,
        },
        Op::Rebind {
            rule: j(2),
            from: r(0),
            to: r(3),
        },
    ]);
    assert!(!x.db.inputs().rules[3].enabled);
}
#[test]
fn t09pq_local_refs_dedup_and_id_rollback() {
    let mut x = Rig::default();
    x.err(
        vec![Op::AssertBase {
            rev: RevRef::New(0),
        }],
        TxError::UnknownRef,
    );
    x.err(
        vec![
            create("a", RecordKind::Fact, true, 1),
            Op::AssertBase {
                rev: RevRef::New(1),
            },
        ],
        TxError::UnknownRef,
    );
    let result = x.ok(vec![
        create("a", RecordKind::Fact, false, 1),
        Op::AddRevision {
            logical: LogicalRef::New(0),
            content: [1; 32],
            adopt_as_base: true,
        },
        Op::AssertBase {
            rev: RevRef::New(1),
        },
    ]);
    assert_eq!(
        result.allocations.new_revisions,
        vec![RevisionId(0), RevisionId(0)]
    );
    assert_eq!(result.allocations.new_logicals, vec![LogicalId(0)]);
    assert_eq!(x.db.inputs().revisions.len(), 1);
    assert!(x.db.inputs().base[0]);
    x.ok(vec![Op::AddRevision {
        logical: LogicalRef::Key("a".into()),
        content: [2; 32],
        adopt_as_base: false,
    }]);
}
#[test]
fn t09rs_keys_policy_hash_and_channel_errors() {
    let mut x = Rig::default();
    for key in ["A", "a@v1", "code:x", ""] {
        x.err(
            vec![create(key, RecordKind::Fact, true, 0)],
            TxError::InvalidKey { key: key.into() },
        );
    }
    x.nodes(&[true]);
    x.err(
        vec![create("n0", RecordKind::Fact, true, 0)],
        TxError::LogicalExists { key: "n0".into() },
    );
    x.err(
        vec![create("code:x", RecordKind::CodeAnchor, true, 0)],
        TxError::KindRestricted,
    );
    let p = PolicyDoc {
        version: 1,
        require_deps: BTreeSet::from([RecordKind::Summary, RecordKind::Plan]),
    };
    let q = PolicyDoc {
        version: 1,
        require_deps: [RecordKind::Plan, RecordKind::Summary]
            .into_iter()
            .collect(),
    };
    assert_eq!(policy_hash(&p), policy_hash(&q));
    x.err(
        vec![Op::SetPolicy { policy: p }],
        TxError::ChannelRestricted {
            op: "SetPolicy".into(),
            channel: Channel::Agent,
        },
    );
    let mut b = Batch::new(vec![Op::SetPolicy {
        policy: PolicyDoc {
            version: 9,
            ..Default::default()
        },
    }]);
    b.channel = Channel::Admin;
    assert_eq!(x.apply(b), Err(TxError::InvalidPolicy));
    let mut b = Batch::new(vec![add(0, &[])]);
    b.channel = Channel::Observer;
    assert_eq!(
        x.apply(b),
        Err(TxError::ChannelRestricted {
            op: "AddRule".into(),
            channel: Channel::Observer
        })
    );
    let mut b = Batch::new(vec![create("bad", RecordKind::CodeAnchor, true, 0)]);
    b.channel = Channel::Observer;
    assert_eq!(x.apply(b), Err(TxError::InvalidKey { key: "bad".into() }));
}
#[test]
fn t09t_observer_anchor_delete_and_watermark_cas() {
    let mut x = Rig::default();
    x.nodes(&[true]);
    let mut b = Batch::new(vec![create("code:a", RecordKind::CodeAnchor, true, 1)]);
    b.channel = Channel::Observer;
    x.apply(b).unwrap();
    let mut b = Batch::new(vec![
        Op::RetractBase { rev: r(1) },
        Op::OpenArtifact {
            ty: ArtifactType::AnchorBroken,
            target: r(1),
            payload_hash: [0; 32],
        },
        Op::ObserverWatermark {
            scope: "repo".into(),
            expected: None,
            position: vec![1],
        },
    ]);
    b.channel = Channel::Observer;
    x.apply(b).unwrap();
    assert!(!x.db.inputs().base[1]);
    assert_eq!(x.db.inputs().watermarks["repo"], vec![1]);
    for (ty, target) in [(ArtifactType::Conflict, 1), (ArtifactType::AnchorBroken, 0)] {
        let mut b = Batch::new(vec![Op::OpenArtifact {
            ty,
            target: r(target),
            payload_hash: [0; 32],
        }]);
        b.channel = Channel::Observer;
        assert_eq!(
            x.apply(b),
            Err(TxError::ChannelRestricted {
                op: "OpenArtifact".into(),
                channel: Channel::Observer
            })
        );
    }
    x.err(
        vec![Op::OpenArtifact {
            ty: ArtifactType::AnchorBroken,
            target: r(1),
            payload_hash: [0; 32],
        }],
        TxError::ChannelRestricted {
            op: "OpenArtifact".into(),
            channel: Channel::Agent,
        },
    );
    let mut b = Batch::new(vec![Op::ObserverWatermark {
        scope: "repo".into(),
        expected: None,
        position: vec![2],
    }]);
    b.channel = Channel::Observer;
    assert_eq!(
        x.apply(b),
        Err(TxError::WatermarkConflict {
            scope: "repo".into(),
            expected: None,
            actual: Some(vec![1])
        })
    );
}
#[test]
fn t09u_artifact_local_binding_and_resolve() {
    let mut x = Rig::default();
    x.nodes(&[true]);
    let result = x.ok(vec![
        Op::OpenArtifact {
            ty: ArtifactType::Conflict,
            target: r(0),
            payload_hash: [0; 32],
        },
        Op::ResolveArtifact {
            id: ArtifactRef::New(0),
        },
    ]);
    assert_eq!(result.allocations.new_artifacts, vec![ArtifactId(0)]);
    assert!(!x.db.inputs().artifacts[0].open);
    x.err(
        vec![Op::ResolveArtifact {
            id: ArtifactId(0).into(),
        }],
        TxError::ArtifactNotOpen { id: ArtifactId(0) },
    );
    assert_eq!(x.s(0), Summary::Certified);
}
#[test]
fn t10_t11_t25_cycle_disabled_rollback_and_motik_lfp() {
    let mut x = Rig::default();
    x.nodes(&[true, false]);
    let original = x.db.digest();
    assert!(matches!(
        x.apply(Batch::new(vec![
            add(1, &[(0, EdgeKind::Pin)]),
            add(0, &[(1, EdgeKind::Pin)])
        ])),
        Err(TxError::CycleDetected { op_index: 1, .. })
    ));
    assert_eq!(x.db.digest(), original);
    x.ok(vec![
        add(0, &[(1, EdgeKind::Pin)]),
        Op::SetRuleEnabled {
            rule: j(0),
            enabled: false,
        },
    ]);
    assert!(matches!(
        x.apply(Batch::new(vec![add(1, &[(0, EdgeKind::Pin)])])),
        Err(TxError::CycleDetected { .. })
    ));
    let mut raw = x.db.inputs();
    raw.rules[0].enabled = true;
    raw.rules.push(Rule {
        conclusion: RevisionId(1),
        premises: vec![Incidence {
            premise: RevisionId(0),
            kind: EdgeKind::Pin,
        }],
        references: vec![],
        enabled: true,
        origin_branch: MAIN,
        origin_epoch: Epoch(0),
    });
    assert!(jacobi(&raw).nodes.iter().all(|n| n.l0));
    raw.base.fill(false);
    assert!(jacobi(&raw).nodes.iter().all(|n| !n.l0 && !n.l1 && !n.l2));
    assert!(kahn(&raw).is_err());
}
#[test]
fn t12_t64_sequential_cas_heads_do_not_retract_base() {
    let mut x = Rig::default();
    x.nodes(&[true]);
    x.ok(vec![revision(0, 7, true), revision(0, 8, true)]);
    x.ok(vec![head(0, 0, 1)]);
    x.err(
        vec![head(0, 0, 2)],
        TxError::HeadConflict {
            logical: LogicalId(0),
            expected: RevisionId(0),
            actual: RevisionId(1),
        },
    );
    assert!(x.db.inputs().base[0]);
    let result = x.ok(vec![
        head(0, 1, 2),
        head(0, 2, 1),
        Op::RetractBase { rev: r(0) },
        Op::AssertBase { rev: r(0) },
    ]);
    assert!(result.net_delta.head_moves.is_empty());
    assert!(result.net_delta.base_flips.is_empty());
    assert!(result.status_diff.is_empty());
}
#[test]
fn t15bc_t15d_t16b_sem_transitivity_validation_sem_head_and_aba() {
    let mut x = Rig::default();
    x.nodes(&[true, false, false]);
    x.ok(vec![
        add(1, &[(0, EdgeKind::Sem)]),
        add(2, &[(1, EdgeKind::Pin)]),
        revision(0, 7, true),
    ]);
    x.ok(vec![head(0, 0, 3)]);
    assert_eq!(x.s(1), Summary::Pending(PendingReason::Semantic));
    assert_eq!(x.s(2), Summary::Pending(PendingReason::Semantic));
    x.ok(vec![validation(0, 0, 3, Verdict::Rejected)]);
    assert_eq!(x.s(1), Summary::Stale(StaleReason::Rejected));
    assert_eq!(x.s(2), Summary::Stale(StaleReason::Rejected));
    assert!(x.db.derived().nodes[2].l0);
    let result = x.ok(vec![validation(0, 0, 3, Verdict::Verified)]);
    assert!(result.l0_flips.is_empty());
    assert_eq!(result.status_diff, vec![RevisionId(1), RevisionId(2)]);
    assert_eq!(x.s(1), Summary::Certified);
    x.ok(vec![Op::RetractBase { rev: r(3) }]);
    assert!(x.db.inputs().base[0]);
    assert_eq!(x.s(1), Summary::Stale(StaleReason::Unsupported));
    assert_eq!(x.s(2), Summary::Stale(StaleReason::Unsupported));
    x.ok(vec![head(0, 3, 0), validation(0, 0, 0, Verdict::Rejected)]);
    assert_eq!(x.db.derived().sem[0], SemState::Verified);
    assert_eq!(x.s(2), Summary::Certified);
    x.err(
        vec![validation(0, 0, 3, Verdict::Verified)],
        TxError::CurvecMoved {
            rule: RuleId(0),
            seen: vec![(LogicalId(0), RevisionId(3))],
            current: vec![(LogicalId(0), RevisionId(0))],
        },
    );
    x.err(
        vec![validation(1, 0, 0, Verdict::Verified)],
        TxError::NoSemIncidence { rule: RuleId(1) },
    );
}
#[test]
fn t15e_t26_virtual_sem_cycles_and_revision_dag() {
    for edge in [EdgeKind::Sem, EdgeKind::Pin, EdgeKind::Cur] {
        let mut x = Rig::default();
        x.nodes(&[true, false]);
        x.ok(vec![revision(0, 9, false), add(1, &[(0, edge)])]);
        let result = x.apply(Batch::new(vec![add(2, &[(1, EdgeKind::Pin)])]));
        if edge == EdgeKind::Sem {
            assert!(matches!(result, Err(TxError::CycleDetected { .. })));
        } else {
            result.unwrap();
        }
    }
}
#[test]
fn t24_pin_strict_cur_and_evidence() {
    for edge in [EdgeKind::Pin, EdgeKind::Cur] {
        let mut x = Rig::default();
        x.nodes(&[true, false]);
        x.ok(vec![
            add(1, &[(0, edge)]),
            revision(0, 9, true),
            head(0, 0, 2),
        ]);
        assert_eq!(
            x.s(1),
            Summary::Stale(if edge == EdgeKind::Pin {
                StaleReason::SupersededPinnedPremise(RevisionId(0))
            } else {
                StaleReason::Unsupported
            })
        );
        assert_eq!(x.db.derived().nodes[1].l0, edge == EdgeKind::Pin);
    }
    let mut x = Rig::default();
    x.ok(vec![
        create("e", RecordKind::Evidence, true, 0),
        create("f", RecordKind::Fact, false, 1),
        add(1, &[(0, EdgeKind::Pin)]),
    ]);
    assert_eq!(x.s(1), Summary::Certified);
}
#[test]
fn t29_epoch_overflow_and_watermark_capacity_are_atomic() {
    let mut x = Rig {
        db: Db::at_epoch(Epoch(u64::MAX)),
        oracle: InputModel::at_epoch(Epoch(u64::MAX)),
    };
    x.err(
        vec![create("a", RecordKind::Fact, true, 0)],
        TxError::CapacityExceeded {
            kind: CapacityKind::Epoch,
        },
    );
    let mut y = Rig::default();
    let mut b = Batch::new(vec![Op::ObserverWatermark {
        scope: "s".into(),
        expected: None,
        position: vec![0; 65],
    }]);
    b.channel = Channel::Observer;
    assert_eq!(
        y.apply(b),
        Err(TxError::CapacityExceeded {
            kind: CapacityKind::WatermarkPosition
        })
    );
}
#[test]
fn t62_cert_oracle_does_not_observe_epoch() {
    let mut a = InputModel::default();
    let mut b = InputModel::at_epoch(Epoch(99));
    let batch = Batch::new(vec![create("a", RecordKind::Fact, true, 0)]);
    a.apply(&batch).unwrap();
    b.apply(&batch).unwrap();
    assert_ne!(a.epoch(), b.epoch());
    let vector = vec![CertEntry {
        logical: LogicalId(0),
        revision: RevisionId(0),
        required: true,
    }];
    assert_eq!(
        cert_verdict(&vector, a.inputs(), &BTreeMap::new()),
        CertVerdict::Valid
    );
    assert_eq!(
        cert_verdict(&vector, a.inputs(), &BTreeMap::new()),
        cert_verdict(&vector, b.inputs(), &BTreeMap::new())
    );
}
#[test]
fn s13_rows_fn_summary_precedence_and_semantic_witness() {
    let good = Dimensions {
        authorized: true,
        conflicted: false,
        currency: Currency::Current,
        structural: Structural::Supported,
        l1: true,
        l2: true,
        semantic: SemState::NotRequired,
        stale_reason: StaleReason::Unsupported,
        observation: Observation::Fresh,
    };
    assert_eq!(summarize(&good), Summary::Certified);
    let mut d = good.clone();
    d.l2 = false;
    assert_eq!(summarize(&d), Summary::Pending(PendingReason::Semantic));
    d.structural = Structural::Unknown;
    assert_eq!(
        summarize(&d),
        Summary::Pending(PendingReason::StructuralUnknown)
    );
    d.observation = Observation::Lagging;
    assert_eq!(
        summarize(&d),
        Summary::Pending(PendingReason::ObservationLag)
    );
    d.structural = Structural::Unsupported;
    assert_eq!(summarize(&d), Summary::Stale(StaleReason::Unsupported));
    d.currency = Currency::Superseded;
    assert_eq!(summarize(&d), Summary::Stale(StaleReason::HeadSuperseded));
    d.conflicted = true;
    assert_eq!(summarize(&d), Summary::Conflicted);
    d.authorized = false;
    assert_eq!(summarize(&d), Summary::Unavailable);
    for reason in [
        StaleReason::Unsupported,
        StaleReason::Rejected,
        StaleReason::SupersededPinnedPremise(RevisionId(0)),
    ] {
        let mut d = good.clone();
        d.l1 = false;
        d.l2 = false;
        d.stale_reason = reason.clone();
        assert_eq!(summarize(&d), Summary::Stale(reason));
    }
    let mut d = good;
    d.currency = Currency::BranchOnly;
    assert_eq!(summarize(&d), Summary::Stale(StaleReason::HeadSuperseded));
    let mut x = Rig::default();
    x.nodes(&[true, false, true]);
    x.ok(vec![
        add(1, &[(0, EdgeKind::Sem)]),
        add(1, &[(2, EdgeKind::Pin)]),
        revision(0, 9, true),
        head(0, 0, 3),
    ]);
    assert_eq!(
        x.db.derived().nodes[1].dimensions.semantic,
        SemState::NotRequired
    );
    assert_eq!(
        x.db.derived().nodes[1].witness[2],
        Some(Witness::Rule(RuleId(1)))
    );
    x.ok(vec![Op::AssertBase { rev: r(1) }]);
    assert_eq!(x.db.derived().nodes[1].witness, [Some(Witness::Base); 3]);
    x.ok(vec![
        Op::RetractBase { rev: r(1) },
        Op::SetRuleEnabled {
            rule: j(0),
            enabled: false,
        },
        Op::SetRuleEnabled {
            rule: j(1),
            enabled: false,
        },
    ]);
    assert_eq!(
        x.db.derived().nodes[1].dimensions.semantic,
        SemState::NotApplicable
    );
}

#[test]
fn t09n_t29_successful_rebind_and_mutable_column_undo() {
    let mut x = Rig::default();
    x.nodes(&[true, false]);
    x.ok(vec![
        revision(0, 9, true),
        add(1, &[(0, EdgeKind::Cur)]),
        head(0, 0, 2),
    ]);
    assert_eq!(x.s(1), Summary::Stale(StaleReason::Unsupported));
    let result = x.ok(vec![Op::Rebind {
        rule: j(0),
        from: r(0),
        to: r(2),
    }]);
    assert_eq!(result.allocations.new_rules, vec![RuleId(1)]);
    assert!(!x.db.inputs().rules[0].enabled);
    assert!(x.db.inputs().rules[1].enabled);
    assert_eq!(x.s(1), Summary::Certified);
    x.err(
        vec![
            Op::SetRuleEnabled {
                rule: j(1),
                enabled: false,
            },
            head(0, 2, 0),
            revision(0, 10, false),
            Op::AssertBase { rev: r(999) },
        ],
        TxError::UnknownRef,
    );
    assert!(x.db.inputs().rules[1].enabled);
    assert_eq!(x.db.inputs().heads[0], RevisionId(2));
    assert_eq!(x.db.inputs().revisions.len(), 3);
    let before = x.db.digest();
    let epoch = x.db.epoch();
    x.ok(vec![]);
    assert_eq!(x.db.digest(), before);
    assert_eq!(x.db.epoch(), Epoch(epoch.0 + 1));
}

#[test]
fn t10_cycle_path_and_csr_index_ids_are_exact() {
    let mut x = Rig::default();
    x.nodes(&[true, false, false, false]);
    x.ok(vec![
        add(1, &[(0, EdgeKind::Pin)]),
        add(2, &[(1, EdgeKind::Cur), (3, EdgeKind::Ref)]),
    ]);
    assert_eq!(
        x.db.indexes().rules_by_premise,
        vec![vec![IncId(0)], vec![IncId(1)], vec![], vec![]]
    );
    // The CSR comparison must discriminate both row contents and row count.
    assert_ne!(
        x.db.indexes().rules_by_premise,
        vec![vec![IncId(1)], vec![IncId(0)], vec![], vec![]]
    );
    assert_ne!(
        x.db.indexes().rules_by_premise,
        vec![vec![IncId(0)], vec![IncId(1)], vec![]]
    );
    assert_eq!(x.db.indexes().pin_by_rev.row(0), vec![IncId(0)]);
    assert_eq!(x.db.indexes().exact_by_rev.row(1), vec![IncId(1)]);
    x.err(
        vec![add(0, &[(2, EdgeKind::Pin)])],
        TxError::CycleDetected {
            op_index: 0,
            path: vec![
                CycleNode::Revision(RevisionId(0)),
                CycleNode::Revision(RevisionId(1)),
                CycleNode::Revision(RevisionId(2)),
                CycleNode::Revision(RevisionId(0)),
            ],
        },
    );
}

#[test]
fn s13_failed_incidence_order_and_sem_head_pending() {
    let mut x = Rig::default();
    x.nodes(&[true, true, false]);
    x.ok(vec![
        add(2, &[(1, EdgeKind::Pin), (0, EdgeKind::Pin)]),
        revision(0, 7, true),
        revision(1, 8, true),
        head(0, 0, 3),
        head(1, 1, 4),
    ]);
    assert_eq!(
        x.s(2),
        Summary::Stale(StaleReason::SupersededPinnedPremise(RevisionId(0)))
    );
    // The asserted Sem premise remains supported; its current head is Pending.
    let mut y = Rig::default();
    y.nodes(&[true, true, false]);
    y.ok(vec![
        revision(0, 9, false),
        revision(1, 10, true),
        add(3, &[(1, EdgeKind::Sem)]),
        add(2, &[(0, EdgeKind::Sem)]),
        head(0, 0, 3),
        head(1, 1, 4),
        validation(1, 0, 3, Verdict::Verified),
    ]);
    assert_eq!(y.s(3), Summary::Pending(PendingReason::Semantic));
    assert_eq!(y.s(2), Summary::Pending(PendingReason::Semantic));
    assert!(y.db.derived().nodes[2].l1);
    assert!(!y.db.derived().nodes[2].l2);
    let why = y.db.why_not(RevisionId(2), 2).unwrap();
    assert_eq!(why.rules[0].current, vec![(LogicalId(0), RevisionId(3))]);
    assert!(
        why.rules[0]
            .upstream
            .iter()
            .any(|u| u.revision == RevisionId(3))
    );
}
