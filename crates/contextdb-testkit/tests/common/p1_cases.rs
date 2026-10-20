use contextdb_core::{CommitStats, Db};
use contextdb_model::*;
use contextdb_oracle::{InputModel, jacobi, kahn};
use contextdb_testkit::create;
fn r(n: u32) -> RevRef {
    RevisionId(n).into()
}
fn rule(c: u32, ps: &[(u32, EdgeKind)]) -> Op {
    Op::AddRule {
        conclusion: r(c),
        incidences: ps.iter().map(|(p, k)| (r(*p), *k)).collect(),
        enabled: true,
    }
}
fn head(l: u32, old: u32, new: u32) -> Op {
    Op::SetHead {
        logical: LogicalId(l).into(),
        expected: r(old),
        new: r(new),
    }
}
fn rev(l: u32, c: u8, base: bool) -> Op {
    Op::AddRevision {
        logical: LogicalId(l).into(),
        content: [c; 32],
        adopt_as_base: base,
    }
}
struct Rig {
    db: Db<super::ExactMode, CommitStats>,
    oracle: InputModel,
}
impl Rig {
    fn new(bases: &[bool]) -> Self {
        let mut s = Self {
            db: Db::new(),
            oracle: InputModel::default(),
        };
        s.ok(bases
            .iter()
            .enumerate()
            .map(|(n, b)| create(&format!("n{n}"), RecordKind::Fact, *b, n as u8))
            .collect());
        s
    }
    fn ok(&mut self, ops: Vec<Op>) -> CommitResult {
        let b = Batch::new(ops);
        self.oracle.apply(&b).unwrap();
        let got = self.db.commit(&b).unwrap();
        assert_eq!(self.db.inputs(), *self.oracle.inputs());
        assert_eq!(self.db.derived(), &jacobi(self.oracle.inputs()));
        assert_eq!(self.db.derived(), &kahn(self.oracle.inputs()).unwrap());
        got
    }
}
#[test]
fn t08_t08b_t22_net_batch_and_head_base_mixed() {
    let mut s = Rig::new(&[true, false, false, false]);
    s.ok(vec![
        rule(2, &[(0, EdgeKind::Pin)]),
        rule(2, &[(1, EdgeKind::Pin)]),
        rule(3, &[(2, EdgeKind::Pin)]),
    ]);
    let c = s.ok(vec![
        Op::RetractBase { rev: r(0) },
        Op::AssertBase { rev: r(1) },
    ]);
    assert_eq!(c.l0_flips, vec![RevisionId(0), RevisionId(1)]);
    // C has a changed witness. Its truth stays true, and D stays byte-identical.
    assert!(c.status_diff.contains(&RevisionId(2)));
    assert!(!c.status_diff.contains(&RevisionId(3)));
    assert_eq!(s.db.derived().nodes[2].support, 1);
    let c = s.ok(vec![
        Op::AssertBase { rev: r(0) },
        Op::RetractBase { rev: r(0) },
    ]);
    assert!(c.net_delta.base_flips.is_empty());
    assert!(c.l0_flips.is_empty());
    assert!(c.status_diff.is_empty());
    s.ok(vec![rev(1, 99, true), rule(3, &[(1, EdgeKind::Cur)])]);
    s.ok(vec![head(1, 1, 4), Op::RetractBase { rev: r(1) }]);
    assert!(!s.db.derived().sat[3][0]);
    s.ok(vec![Op::AssertBase { rev: r(1) }]);
    assert!(!s.db.derived().sat[3][0]);
    s.ok(vec![head(1, 4, 1)]);
    assert!(s.db.derived().sat[3][0]);
}
#[test]
fn t23_t27_insert_rule_assert_premise_disabled_updates() {
    let mut s = Rig::new(&[false, false, false]);
    s.ok(vec![
        rule(1, &[(0, EdgeKind::Pin)]),
        Op::AssertBase { rev: r(0) },
        rule(2, &[(1, EdgeKind::Pin)]),
    ]);
    assert!(s.db.derived().nodes[2].l0);
    s.ok(vec![Op::SetRuleEnabled {
        rule: RuleId(0).into(),
        enabled: false,
    }]);
    s.ok(vec![Op::RetractBase { rev: r(0) }]);
    assert_eq!(s.db.derived().unsat[0], 1);
    s.ok(vec![Op::SetRuleEnabled {
        rule: RuleId(0).into(),
        enabled: true,
    }]);
    assert!(!s.db.derived().nodes[2].l0);
    s.ok(vec![Op::AssertBase { rev: r(0) }]);
    assert!(s.db.derived().nodes[2].l0);
}
#[test]
fn t06d_cur_rebind_and_t07d_impact_undo_diff() {
    let mut s = Rig::new(&[true, false, false, true]);
    s.ok(vec![
        rev(0, 90, true),
        rule(1, &[(0, EdgeKind::Cur)]),
        rule(2, &[(3, EdgeKind::Pin), (0, EdgeKind::Ref)]),
    ]);
    let impact = s.db.impact_reachable(&[RevisionId(0)]).unwrap();
    assert_eq!(impact.supported, vec![RevisionId(1)]);
    assert_eq!(impact.reference_only, vec![RevisionId(2)]);
    let start = s.db.epoch();
    let digest = s.db.digest();
    let stats = s.db.stats().clone();
    let indexes = s.db.indexes().clone();
    let topo = s.db.dynamic_topo().clone();
    let batch = Batch::new(vec![
        head(0, 0, 4),
        Op::Rebind {
            rule: RuleId(0).into(),
            from: r(0),
            to: r(4),
        },
    ]);
    let dry = s.db.impact_exact(&batch).unwrap();
    assert_eq!(s.db.digest(), digest);
    assert_eq!(s.db.epoch(), start);
    assert_eq!(s.db.stats(), &stats);
    assert_eq!(s.db.indexes(), &indexes);
    assert_eq!(s.db.dynamic_topo(), &topo);
    assert!(s.db.diff(start, start).unwrap().changes.is_empty());
    let actual = s.ok(batch.ops);
    assert_eq!(actual, dry);
    assert_eq!(s.db.status(RevisionId(1)).unwrap(), Summary::Certified);
    let finish = s.db.epoch();
    let diff = s.db.diff(start, finish).unwrap();
    assert_eq!(
        diff.changes.iter().map(|c| c.revision).collect::<Vec<_>>(),
        actual.status_diff
    );
    let reverse = s.db.diff(finish, start).unwrap();
    for (a, b) in diff.changes.iter().zip(reverse.changes) {
        assert_eq!(a.before, b.after);
        assert_eq!(a.after, b.before);
    }
    assert!(s.db.diff(Epoch(finish.0 + 1), start).is_err());
    let invalid = Batch::new(vec![Op::AssertBase { rev: r(0) }, head(0, 0, 4)]);
    let digest = s.db.digest();
    assert!(s.db.impact_exact(&invalid).is_err());
    assert_eq!(s.db.digest(), digest);
}
#[test]
fn history_cancellation_and_node_created_between_epochs() {
    let mut s = Rig::new(&[true]);
    let start = s.db.epoch();
    s.ok(vec![Op::RetractBase { rev: r(0) }]);
    s.ok(vec![Op::AssertBase { rev: r(0) }]);
    assert!(s.db.diff(start, s.db.epoch()).unwrap().changes.is_empty());
    s.ok(vec![create("new", RecordKind::Fact, false, 42)]);
    let d = s.db.diff(start, s.db.epoch()).unwrap();
    assert_eq!(d.changes.len(), 1);
    assert!(d.changes[0].before.is_none());
}
#[test]
fn mongodb_postgresql_scenario_and_stale_reason_with_alternative() {
    let mut s = Rig::new(&[true, false, false, true, false]);
    s.ok(vec![
        rule(1, &[(0, EdgeKind::Sem)]),
        rule(2, &[(0, EdgeKind::Cur)]),
        rule(4, &[(0, EdgeKind::Pin)]),
        rule(4, &[(3, EdgeKind::Pin)]),
        rev(0, 100, true),
    ]);
    s.ok(vec![head(0, 0, 5)]);
    assert_eq!(
        s.db.status(RevisionId(1)).unwrap(),
        Summary::Pending(PendingReason::Semantic)
    );
    assert_eq!(
        s.db.status(RevisionId(2)).unwrap(),
        Summary::Stale(StaleReason::Unsupported)
    );
    assert_eq!(s.db.status(RevisionId(4)).unwrap(), Summary::Certified);
    assert_eq!(
        s.db.node(RevisionId(4)).unwrap().dimensions.stale_reason,
        StaleReason::SupersededPinnedPremise(RevisionId(0))
    );
    let proof = s.db.why_depth(RevisionId(4), 0).unwrap();
    assert!(proof.truncated);
}
#[test]
fn metamorphic_disjoint_writes_and_idempotent_ops() {
    let mut a = Rig::new(&[false, false]);
    let mut b = Rig::new(&[false, false]);
    a.ok(vec![
        Op::AssertBase { rev: r(0) },
        Op::AssertBase { rev: r(1) },
    ]);
    b.ok(vec![
        Op::AssertBase { rev: r(1) },
        Op::AssertBase { rev: r(0) },
        Op::AssertBase { rev: r(0) },
    ]);
    assert_eq!(a.db.digest(), b.db.digest());
    let mut c = Rig::new(&[false, false]);
    c.ok(vec![Op::AssertBase { rev: r(0) }]);
    c.ok(vec![Op::AssertBase { rev: r(1) }]);
    assert_eq!(a.db.digest(), c.db.digest());
}

// Ground, acyclic expansion of Hu–Motik–Horrocks (2018), Example 3:
// A(a), A(b), A(d); B(a,c), B(b,c), B(c,d), B(d,e).
// https://arxiv.org/html/1711.03987#S4.SS1
// B facts are fixed true constants folded into each ground rule.
#[test]
fn t31_hu_motik_horrocks_example3_dag() {
    let mut s = Rig::new(&[true, true, false, true, false]);
    s.ok(vec![
        rule(2, &[(0, EdgeKind::Pin)]),
        rule(2, &[(1, EdgeKind::Pin)]),
        rule(3, &[(2, EdgeKind::Pin)]),
        rule(4, &[(3, EdgeKind::Pin)]),
    ]);
    assert_eq!(s.db.counting_state().support, vec![0, 0, 2, 1, 1]);
    let result = s.ok(vec![Op::RetractBase { rev: r(0) }]);
    assert_eq!(result.l0_flips, vec![RevisionId(0)]);
    assert_eq!(s.db.counting_state().support, vec![0, 0, 1, 1, 1]);
    assert!(s.db.node(RevisionId(4)).unwrap().l2);
}

#[test]
fn admission_inclusive_boundaries_and_metadata_errors() {
    let mut s = Rig::new(&[true]);
    s.ok(vec![create(&"a".repeat(256), RecordKind::Fact, false, 0)]);
    for n in [257, 258] {
        let b = Batch::new(vec![create(&"a".repeat(n), RecordKind::Fact, false, 0)]);
        assert_eq!(s.db.commit(&b), s.oracle.apply(&b).map(|_| unreachable!()));
    }
    let meta = |paths: &[&str]| Op::SetMetadata {
        logical: LogicalId(0).into(),
        tags: std::collections::BTreeSet::from(["tag".into()]),
        dirs: paths.iter().map(|s| (*s).into()).collect(),
    };
    s.ok(vec![meta(&[])]);
    for path in ["", "/a", "a//b", "a/./b", "a/../b", "a/*"] {
        let b = Batch::new(vec![meta(&[path])]);
        assert_eq!(s.db.commit(&b), Err(TxError::InvalidMetadata));
        assert_eq!(s.oracle.apply(&b), Err(TxError::InvalidMetadata));
    }
    s.ok(vec![meta(&["a"])]);
    s.ok(vec![meta(&["a"])]);
    let b = Batch::new(vec![meta(&["ab"])]);
    assert!(s.db.commit(&b).is_err());
    assert!(s.oracle.apply(&b).is_err());
    let mut s = Rig::new(&vec![true; 1025]);
    s.ok(vec![rule(
        1024,
        &(0..1024).map(|n| (n, EdgeKind::Pin)).collect::<Vec<_>>(),
    )]);
    assert_eq!(s.db.derived().unsat[0], 0);
}
#[test]
fn observer_scope_position_and_anchor_update_channels() {
    let mut s = Rig::new(&[true]);
    let mut b = Batch::new(vec![create("code:a", RecordKind::CodeAnchor, true, 1)]);
    b.channel = Channel::Observer;
    s.oracle.apply(&b).unwrap();
    s.db.commit(&b).unwrap();
    let b = Batch::new(vec![rev(1, 9, false)]);
    assert_eq!(s.db.commit(&b), Err(TxError::KindRestricted));
    assert_eq!(s.oracle.apply(&b), Err(TxError::KindRestricted));
    let mut b = Batch::new(vec![rev(1, 9, true), head(1, 1, 2)]);
    b.channel = Channel::Observer;
    s.oracle.apply(&b).unwrap();
    s.db.commit(&b).unwrap();
    for n in [256, 257, 258] {
        let mut b = Batch::new(vec![Op::ObserverWatermark {
            scope: "s".repeat(n),
            expected: None,
            position: vec![1; 64],
        }]);
        b.channel = Channel::Observer;
        let actual = s.db.commit(&b);
        let expected = s.oracle.apply(&b);
        assert_eq!(actual.is_ok(), expected.is_ok());
        if n == 256 {
            actual.unwrap();
        } else {
            assert_eq!(
                actual,
                Err(TxError::CapacityExceeded {
                    kind: CapacityKind::WatermarkScope
                })
            );
        }
    }
    assert_eq!(s.db.inputs(), *s.oracle.inputs());
}
#[test]
fn artifact_nonzero_id_and_observer_resolve() {
    let mut s = Rig::new(&[true]);
    let open = || Op::OpenArtifact {
        ty: ArtifactType::Conflict,
        target: r(0),
        payload_hash: [0; 32],
    };
    s.ok(vec![
        open(),
        open(),
        Op::ResolveArtifact {
            id: ArtifactId(1).into(),
        },
    ]);
    assert!(s.db.inputs().artifacts[0].open);
    assert!(!s.db.inputs().artifacts[1].open);
    let mut b = Batch::new(vec![
        create("code:a", RecordKind::CodeAnchor, true, 1),
        Op::OpenArtifact {
            ty: ArtifactType::AnchorBroken,
            target: r(1),
            payload_hash: [1; 32],
        },
    ]);
    b.channel = Channel::Observer;
    s.oracle.apply(&b).unwrap();
    s.db.commit(&b).unwrap();
    let mut b = Batch::new(vec![Op::ResolveArtifact {
        id: ArtifactId(2).into(),
    }]);
    assert_eq!(
        s.db.commit(&b),
        Err(TxError::ChannelRestricted {
            op: "ResolveArtifact".into(),
            channel: Channel::Agent
        })
    );
    assert!(s.oracle.apply(&b).is_err());
    b.channel = Channel::Observer;
    s.oracle.apply(&b).unwrap();
    s.db.commit(&b).unwrap();
    assert_eq!(s.db.inputs(), *s.oracle.inputs());
}

#[test]
fn t08_multiple_related_heads_only_publish_final_state() {
    let mut s = Rig::new(&[true, true, false, false]);
    s.ok(vec![
        rule(2, &[(0, EdgeKind::Cur), (1, EdgeKind::Cur)]),
        rule(3, &[(2, EdgeKind::Pin)]),
        rev(0, 90, true),
        rev(1, 91, true),
    ]);
    let result = s.ok(vec![
        head(0, 0, 4),
        head(1, 1, 5),
        Op::RetractBase { rev: r(0) },
        Op::RetractBase { rev: r(1) },
    ]);
    assert_eq!(result.l0_flips, (0..4).map(RevisionId).collect::<Vec<_>>());
    assert_eq!(s.db.derived().unsat[0], 2);
    let result = s.ok(vec![head(0, 4, 0), head(0, 0, 4)]);
    assert!(result.net_delta.head_moves.is_empty());
    assert!(result.status_diff.is_empty());
}
