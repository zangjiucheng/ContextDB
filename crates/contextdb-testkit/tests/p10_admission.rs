//! P10 admission (SPEC SD21.7, §2.5 A7/A8, §6.1 contracts 6, 7 and 9;
//! T109 and T120): the replica channel, replica-origin logicals, reserved
//! namespaces, the A5 and shred exemptions. Every step runs on the kernel
//! and on the independent `BranchOracle`, which must agree.

use contextdb_core::Kernel;
use contextdb_model::*;
use contextdb_oracle::BranchOracle;

const TEAM: [u8; 16] = [0xab; 16];

fn hex() -> String {
    "ab".repeat(16)
}

/// Errors compare exactly unless they may name staged objects.
fn same_error(a: &TxError, b: &TxError) -> bool {
    use TxError::*;
    match (a, b) {
        (HeadConflict { .. } | PolicyViolation { .. } | InvalidRule(_), _) => {
            std::mem::discriminant(a) == std::mem::discriminant(b)
        }
        _ => a == b,
    }
}

struct Both {
    k: Kernel,
    o: BranchOracle,
}

impl Both {
    fn new() -> Self {
        Self {
            k: Kernel::default(),
            o: BranchOracle::new(|b| *blake3::hash(b).as_bytes()),
        }
    }
    /// Commits on both; their outcomes must agree.
    fn run(&mut self, b: Batch) -> Result<CommitResult, TxError> {
        let k = self.k.commit(&b);
        let o = self.o.apply(&b, true);
        match (&k, &o) {
            (Ok(_), Ok(_)) => {}
            (Err(x), Err(y)) => assert!(same_error(x, y), "kernel {x:?} vs oracle {y:?} for {b:?}"),
            _ => panic!("kernel {k:?} vs oracle {o:?} for {b:?}"),
        }
        k
    }
    fn ok(&mut self, b: Batch) -> CommitResult {
        self.run(b).expect("committed")
    }
    fn err(&mut self, b: Batch) -> TxError {
        self.run(b).expect_err("refused")
    }
}

fn batch(channel: Channel, ops: Vec<Op>) -> Batch {
    let mut b = Batch::new(ops);
    b.channel = channel;
    b
}

fn mirror_create(path: &str, kind: RecordKind, content: ContentHash, base: bool) -> Op {
    Op::CreateLogical {
        key: format!("team/{}/{path}", hex()),
        kind,
        scope_lane: format!("team:{}/main", hex()),
        content,
        adopt_as_base: base,
        origin: Origin::Replica { team: TEAM },
    }
}

fn local_create(key: &str, lane: &str, kind: RecordKind, content: ContentHash, base: bool) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind,
        scope_lane: lane.into(),
        content,
        adopt_as_base: base,
        origin: Origin::Local,
    }
}

#[test]
fn a8_only_the_replica_channel_writes_mirrors() {
    let mut s = Both::new();
    let m = s.ok(batch(
        Channel::Replica,
        vec![mirror_create("conv/a", RecordKind::Decision, [1; 32], true)],
    ));
    let ml = m.allocations.new_logicals[0];
    let mr = m.allocations.new_revisions[0];
    let l = s.ok(batch(
        Channel::Agent,
        vec![local_create(
            "dec:b",
            "repo",
            RecordKind::Decision,
            [2; 32],
            false,
        )],
    ));
    let lr = l.allocations.new_revisions[0];
    // Every non-replica write to the mirror: ReplicaReadOnly.
    for (name, op) in [
        (
            "revise",
            Op::AddRevision {
                logical: ml.into(),
                content: [3; 32],
                adopt_as_base: false,
            },
        ),
        (
            "tags",
            Op::SetMetadata {
                logical: ml.into(),
                tags: ["x".into()].into(),
                dirs: Default::default(),
            },
        ),
        ("retract", Op::RetractBase { rev: mr.into() }),
        ("assert", Op::AssertBase { rev: mr.into() }),
    ] {
        for c in [Channel::Agent, Channel::Admin] {
            assert_eq!(
                s.err(batch(c, vec![op.clone()])),
                TxError::ReplicaReadOnly,
                "{name} on {c:?}"
            );
        }
    }
    // A local rule may depend on a mirror, never conclude one; no artifact
    // names a mirror.
    s.ok(batch(
        Channel::Agent,
        vec![Op::AddRule {
            conclusion: lr.into(),
            incidences: vec![(mr.into(), EdgeKind::Pin)],
            enabled: true,
        }],
    ));
    assert_eq!(
        s.err(batch(
            Channel::Agent,
            vec![Op::AddRule {
                conclusion: mr.into(),
                incidences: vec![(lr.into(), EdgeKind::Pin)],
                enabled: true
            }]
        )),
        TxError::ReplicaReadOnly
    );
    assert_eq!(
        s.err(batch(
            Channel::Agent,
            vec![Op::OpenArtifact {
                ty: ArtifactType::Conflict,
                target: mr.into(),
                payload_hash: [0; 32]
            }]
        )),
        TxError::ReplicaReadOnly
    );
    // The replica channel writes its mirror (tags, revisions, base) and
    // nothing else.
    s.ok(batch(
        Channel::Replica,
        vec![
            Op::AddRevision {
                logical: ml.into(),
                content: [4; 32],
                adopt_as_base: false,
            },
            Op::SetMetadata {
                logical: ml.into(),
                tags: ["db".into()].into(),
                dirs: Default::default(),
            },
        ],
    ));
    assert_eq!(
        s.err(batch(
            Channel::Replica,
            vec![Op::SetMetadata {
                logical: ml.into(),
                tags: Default::default(),
                dirs: ["src".into()].into()
            }]
        )),
        TxError::InvalidMetadata
    );
    for (name, op) in [
        (
            "local revise",
            Op::AddRevision {
                logical: l.allocations.new_logicals[0].into(),
                content: [5; 32],
                adopt_as_base: false,
            },
        ),
        (
            "rule",
            Op::AddRule {
                conclusion: lr.into(),
                incidences: vec![(mr.into(), EdgeKind::Ref)],
                enabled: true,
            },
        ),
        (
            "read set",
            Op::ExpectHeads {
                entries: vec![(ml.into(), mr)],
                source: ReadSource::Client,
            },
        ),
        (
            "local create",
            local_create("dec:c", "repo", RecordKind::Decision, [6; 32], false),
        ),
    ] {
        assert!(
            matches!(
                s.err(batch(Channel::Replica, vec![op])),
                TxError::ChannelRestricted { .. }
            ),
            "{name}"
        );
    }
    // Fast-path batches (head and base moves only) follow the same rules.
    let r1 = s
        .ok(batch(
            Channel::Replica,
            vec![Op::AddRevision {
                logical: ml.into(),
                content: [7; 32],
                adopt_as_base: false,
            }],
        ))
        .allocations
        .new_revisions[0];
    s.ok(batch(
        Channel::Replica,
        vec![Op::SetHead {
            logical: ml.into(),
            expected: mr.into(),
            new: r1.into(),
        }],
    ));
    assert_eq!(
        s.err(batch(
            Channel::Agent,
            vec![Op::SetHead {
                logical: ml.into(),
                expected: r1.into(),
                new: mr.into()
            }]
        )),
        TxError::ReplicaReadOnly
    );
    assert!(matches!(
        s.err(batch(
            Channel::Replica,
            vec![Op::AssertBase { rev: lr.into() }]
        )),
        TxError::ChannelRestricted { .. }
    ));
    // Branch ops and merges are never the replica channel's.
    assert!(matches!(
        s.err(batch(
            Channel::Replica,
            vec![Op::CreateBranch { name: "b".into() }]
        )),
        TxError::ChannelRestricted { .. }
    ));
    // MirrorAttach of a mirror already visible here.
    assert_eq!(
        s.err(batch(
            Channel::Replica,
            vec![Op::MirrorAttach {
                key: format!("team/{}/conv/a", hex()),
                head: [1; 32],
                base: true
            }]
        )),
        TxError::LogicalExists {
            key: format!("team/{}/conv/a", hex())
        }
    );
    assert_eq!(
        s.err(batch(
            Channel::Agent,
            vec![Op::MirrorAttach {
                key: format!("team/{}/conv/a", hex()),
                head: [1; 32],
                base: true
            }]
        )),
        TxError::ReplicaReadOnly
    );
}

#[test]
fn reserved_namespaces_and_team_scopes() {
    let mut s = Both::new();
    for op in [
        local_create(
            &format!("team/{}/x", hex()),
            "repo",
            RecordKind::Fact,
            [1; 32],
            false,
        ),
        local_create(
            "fact:x",
            &format!("team:{}/main", hex()),
            RecordKind::Fact,
            [1; 32],
            false,
        ),
        local_create("team/anything", "repo", RecordKind::Fact, [1; 32], false),
    ] {
        for c in [Channel::Agent, Channel::Admin] {
            assert_eq!(
                s.err(batch(c, vec![op.clone()])),
                TxError::ReservedNamespace
            );
        }
    }
    // A replica-origin logical lives in its own team's namespace.
    let other = Op::CreateLogical {
        key: format!("team/{}/x", "cd".repeat(16)),
        kind: RecordKind::Fact,
        scope_lane: format!("team:{}/main", hex()),
        content: [1; 32],
        adopt_as_base: false,
        origin: Origin::Replica { team: TEAM },
    };
    assert!(matches!(
        s.err(batch(Channel::Replica, vec![other])),
        TxError::InvalidKey { .. }
    ));
    let wrong_lane = Op::CreateLogical {
        key: format!("team/{}/x", hex()),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [1; 32],
        adopt_as_base: false,
        origin: Origin::Replica { team: TEAM },
    };
    assert!(matches!(
        s.err(batch(Channel::Replica, vec![wrong_lane])),
        TxError::InvalidLane { .. }
    ));
    // `team:` watermarks: replica only, `gap` allowed there and nowhere else.
    let wm = |scope: &str, gap: bool, expected: Option<Vec<u8>>| Op::ObserverWatermark {
        scope: scope.into(),
        expected,
        position: vec![1],
        gap,
    };
    let team_scope = format!("team:{}", hex());
    s.ok(batch(Channel::Replica, vec![wm(&team_scope, false, None)]));
    s.ok(batch(
        Channel::Replica,
        vec![wm(&team_scope, true, Some(vec![1]))],
    ));
    for (c, op) in [
        (Channel::Observer, wm(&team_scope, false, Some(vec![1]))),
        (Channel::Agent, wm(&team_scope, false, Some(vec![1]))),
        (Channel::Replica, wm("code:repo", false, None)),
        (Channel::Observer, wm("code:repo", true, None)),
    ] {
        assert!(
            matches!(
                s.err(batch(c, vec![op.clone()])),
                TxError::ChannelRestricted { .. }
            ),
            "{c:?} {op:?}"
        );
    }
    s.ok(batch(Channel::Observer, vec![wm("code:repo", false, None)]));
}

#[test]
fn a5_exempts_mirrors_by_origin() {
    let mut s = Both::new();
    s.ok(batch(
        Channel::Replica,
        vec![mirror_create("dec/a", RecordKind::Decision, [1; 32], true)],
    ));
    let local = s.ok(batch(
        Channel::Agent,
        vec![local_create(
            "dec:b",
            "repo",
            RecordKind::Decision,
            [2; 32],
            false,
        )],
    ));
    // The full scan a SetPolicy runs passes over the base mirror ...
    let deps = Op::SetPolicy {
        policy: PolicyDoc {
            version: 1,
            require_deps: [RecordKind::Decision].into(),
        },
    };
    s.ok(batch(Channel::Admin, vec![deps]));
    // ... a new base mirror is allowed, a new base local record is not.
    s.ok(batch(
        Channel::Replica,
        vec![mirror_create("dec/c", RecordKind::Decision, [3; 32], true)],
    ));
    assert!(matches!(
        s.err(batch(
            Channel::Agent,
            vec![local_create(
                "dec:d",
                "repo",
                RecordKind::Decision,
                [4; 32],
                true
            )]
        )),
        TxError::PolicyViolation { .. }
    ));
    let _ = local;
}

#[test]
fn a_local_shred_never_refuses_a_replica_write() {
    // Shred is a store-level input the oracle does not model: kernel only.
    let mut k = Kernel::default();
    let secret = [9; 32];
    let m = k
        .commit(&batch(
            Channel::Replica,
            vec![mirror_create("k/a", RecordKind::Fact, [1; 32], true)],
        ))
        .unwrap();
    let ml = m.allocations.new_logicals[0];
    k.commit(&batch(
        Channel::Admin,
        vec![Op::ShredContent { content: secret }],
    ))
    .unwrap();
    assert!(matches!(
        k.commit(&batch(
            Channel::Agent,
            vec![local_create(
                "fact:s",
                "repo",
                RecordKind::Fact,
                secret,
                false
            )]
        )),
        Err(TxError::ContentShredded { .. })
    ));
    // The replica records the root's revision anyway; it reads unavailable.
    k.commit(&batch(
        Channel::Replica,
        vec![Op::AddRevision {
            logical: ml.into(),
            content: secret,
            adopt_as_base: false,
        }],
    ))
    .unwrap();
    k.commit(&batch(
        Channel::Replica,
        vec![mirror_create("k/s", RecordKind::Fact, secret, false)],
    ))
    .unwrap();
}

/// SD21 lag (ADR 0018 §8.3, §8.5): a read over a mirror lags while the
/// live position of its team is missing or differs from the committed
/// watermark; once the store left the team (watermark `TEAM_LEFT`) it
/// never lags, whatever the live position.
#[test]
fn a_mirror_read_lags_until_its_team_is_left() {
    use std::collections::BTreeMap;
    let mut s = Both::new();
    let m = s.ok(batch(
        Channel::Replica,
        vec![mirror_create("conv/a", RecordKind::Decision, [1; 32], true)],
    ));
    let mr = m.allocations.new_revisions[0];
    let scope = team_scope(&TEAM);
    let watermark = |position: &[u8], expected: Option<&[u8]>| {
        batch(
            Channel::Replica,
            vec![Op::ObserverWatermark {
                gap: false,
                scope: scope.clone(),
                expected: expected.map(<[u8]>::to_vec),
                position: position.to_vec(),
            }],
        )
    };
    s.ok(watermark(b"g0:1", None));
    let summary = |s: &Both, live: &BTreeMap<String, Vec<u8>>| {
        s.k.branch(MAIN).unwrap().summary_at(mr, live).unwrap()
    };
    let lag = Summary::Pending(PendingReason::ReplicationLag);
    let missing = BTreeMap::new();
    let same = BTreeMap::from([(scope.clone(), b"g0:1".to_vec())]);
    let moved = BTreeMap::from([(scope.clone(), b"g0:2".to_vec())]);
    assert_eq!(summary(&s, &missing), lag);
    assert_eq!(summary(&s, &moved), lag);
    assert_eq!(summary(&s, &same), Summary::Certified);
    s.ok(watermark(TEAM_LEFT, Some(b"g0:1")));
    assert_eq!(summary(&s, &missing), Summary::Certified);
    assert_eq!(summary(&s, &moved), Summary::Certified);
}

/// The blob check (ADR 0006 §3, SD21.7, ADR 0018 §11): a replica write of
/// a payload this store shredded needs no blob, since it reads unavailable;
/// every other write without its blob is `MissingBlob`, on every channel
/// and for every op that names a payload (create, revision, attach).
#[test]
fn only_a_replica_write_of_shredded_content_skips_the_blob_check() {
    use std::collections::BTreeSet;
    // Shred is a store-level input the oracle does not model: kernel only.
    let mut k = Kernel::default();
    // `fresh` is shredded and held by no branch, so an attach of it has
    // no revision to reuse.
    let (secret, fresh, absent) = ([9; 32], [7; 32], [8; 32]);
    let none: BTreeSet<ContentHash> = BTreeSet::new();
    let on = |branch: BranchId, channel: Channel, ops: Vec<Op>| {
        let mut b = batch(channel, ops);
        b.branch = branch;
        b
    };
    k.commit(&batch(
        Channel::Admin,
        vec![Op::CreateBranch { name: "b".into() }],
    ))
    .unwrap();
    let fork = BranchId(1);
    let m = k
        .commit(&batch(
            Channel::Replica,
            vec![mirror_create("k/a", RecordKind::Fact, [1; 32], true)],
        ))
        .unwrap();
    let ml = m.allocations.new_logicals[0];
    let local = k
        .commit(&batch(
            Channel::Agent,
            vec![local_create(
                "fact:l",
                "repo",
                RecordKind::Fact,
                [2; 32],
                true,
            )],
        ))
        .unwrap()
        .allocations
        .new_logicals[0];
    k.commit(&batch(
        Channel::Admin,
        vec![
            Op::ShredContent { content: secret },
            Op::ShredContent { content: fresh },
        ],
    ))
    .unwrap();
    let revise = |logical: LogicalId, content| Op::AddRevision {
        logical: logical.into(),
        content,
        adopt_as_base: false,
    };
    let attach = |head| Op::MirrorAttach {
        key: format!("team/{}/k/a", hex()),
        head,
        base: false,
    };
    let missing = |r: Result<CommitResult, TxError>| r.expect_err("refused");
    // Unshredded and absent: MissingBlob, replica or not.
    assert_eq!(
        missing(k.commit_with(&batch(Channel::Replica, vec![revise(ml, absent)]), &none)),
        TxError::MissingBlob { content: absent }
    );
    assert_eq!(
        missing(k.commit_with(
            &batch(
                Channel::Replica,
                vec![mirror_create("k/b", RecordKind::Fact, absent, false)]
            ),
            &none
        )),
        TxError::MissingBlob { content: absent }
    );
    assert_eq!(
        missing(k.commit_with(&on(fork, Channel::Replica, vec![attach(absent)]), &none)),
        TxError::MissingBlob { content: absent }
    );
    assert_eq!(
        missing(k.commit_with(&batch(Channel::Agent, vec![revise(local, absent)]), &none)),
        TxError::MissingBlob { content: absent }
    );
    assert_eq!(
        missing(k.commit_with(
            &batch(
                Channel::Agent,
                vec![local_create(
                    "fact:m",
                    "repo",
                    RecordKind::Fact,
                    absent,
                    false
                )]
            ),
            &none
        )),
        TxError::MissingBlob { content: absent }
    );
    // Shredded and absent: the replica records it anyway.
    k.commit_with(&batch(Channel::Replica, vec![revise(ml, secret)]), &none)
        .unwrap();
    k.commit_with(
        &batch(
            Channel::Replica,
            vec![mirror_create("k/s", RecordKind::Fact, secret, false)],
        ),
        &none,
    )
    .unwrap();
    k.commit_with(&on(fork, Channel::Replica, vec![attach(fresh)]), &none)
        .unwrap();
}
