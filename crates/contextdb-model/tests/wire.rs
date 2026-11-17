use contextdb_model::*;
use std::collections::BTreeSet;
#[test]
fn every_op_and_reference_round_trips_in_versioned_jsonl() {
    let ops = vec![
        Op::CreateLogical {
            origin: Default::default(),
            key: "fact:a".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: [7; 32],
            adopt_as_base: true,
        },
        Op::AddRevision {
            logical: LogicalRef::Key("fact:a".into()),
            content: [8; 32],
            adopt_as_base: false,
        },
        Op::SetHead {
            logical: LogicalRef::New(0),
            expected: RevRef::New(0),
            new: RevRef::New(1),
        },
        Op::AssertBase {
            rev: RevisionId(1).into(),
        },
        Op::RetractBase {
            rev: RevRef::New(1),
        },
        Op::AddRule {
            conclusion: RevRef::New(1),
            incidences: vec![(RevisionId(0).into(), EdgeKind::Sem)],
            enabled: false,
        },
        Op::SetRuleEnabled {
            rule: RuleRef::New(0),
            enabled: true,
        },
        Op::Rebind {
            rule: RuleId(0).into(),
            from: RevisionId(0).into(),
            to: RevisionId(1).into(),
        },
        Op::RecordValidation {
            rule: RuleId(1).into(),
            curvec_seen: vec![(LogicalId(0), RevisionId(1))],
            verdict: Verdict::Rejected,
            validator: "人工验证".into(),
            meta: None,
            if_absent: false,
        },
        Op::RecordValidation {
            rule: RuleId(1).into(),
            curvec_seen: vec![(LogicalId(0), RevisionId(1))],
            verdict: Verdict::Verified,
            validator: "worker".into(),
            meta: Some([7; 32]),
            if_absent: true,
        },
        Op::SetPolicy {
            policy: PolicyDoc {
                version: 1,
                require_deps: BTreeSet::from([RecordKind::Summary, RecordKind::Plan]),
            },
        },
        Op::ObserverWatermark {
            gap: false,
            scope: "repo".into(),
            expected: Some(vec![1]),
            position: vec![2],
        },
        Op::SetMetadata {
            logical: LogicalId(0).into(),
            tags: BTreeSet::from(["tag".into()]),
            dirs: BTreeSet::new(),
        },
        Op::OpenArtifact {
            ty: ArtifactType::ImportBroken,
            target: RevRef::New(0),
            payload_hash: [0; 32],
        },
        Op::ResolveArtifact {
            id: ArtifactRef::New(0),
            note: None,
        },
        Op::ResolveArtifact {
            id: ArtifactRef::New(0),
            note: Some([8; 32]),
        },
    ];
    let mut batch = Batch::new(ops);
    batch.branch = BranchId(42);
    batch.principal = PrincipalId(2);
    batch.channel = Channel::Admin;
    batch.client_txn_id = Some([9; 16]);
    let log = EventLog {
        batches: vec![batch.clone()],
    };
    let bytes = batch.canonical_bytes();
    assert_eq!(Batch::from_bytes(&bytes).unwrap(), batch);
    assert_eq!(EventLog::from_jsonl(&log.to_jsonl()).unwrap(), log);
    for n in 0..bytes.len() {
        assert!(Batch::from_bytes(&bytes[..n]).is_err(), "truncation {n}");
    }
    let mut extra = bytes;
    extra.push(0);
    assert_eq!(Batch::from_bytes(&extra), Err(WireError::TrailingBytes));
}
#[test]
fn log_rejects_unknown_version_bad_hex_and_unbounded_lengths() {
    assert_eq!(
        EventLog::from_jsonl("{\"version\":5,\"batch_hex\":\"\"}"),
        Err(WireError::UnknownVersion)
    );
    for line in [
        "{}",
        "{\"version\":0,\"batch_hex\":\"z0\"}",
        "{\"version\":0,\"batch_hex\":\"0\"}",
        "{\"version\":0,\"batch_hex\":\"🦀\"}",
    ] {
        assert!(EventLog::from_jsonl(line).is_err());
    }
    assert!(Vec::<u8>::from_bytes(&u32::MAX.to_le_bytes()).is_err());
    assert!(bool::from_bytes(&[2]).is_err());
    assert!(Op::from_bytes(&[255]).is_err());
    assert_eq!(EventLog::from_jsonl("\n").unwrap(), EventLog::default());
}

#[test]
fn unordered_and_duplicate_collections_are_rejected() {
    for bytes in [vec![2, 0, 0, 0, 2, 1], vec![2, 0, 0, 0, 1, 1]] {
        assert_eq!(
            BTreeSet::<u8>::from_bytes(&bytes),
            Err(WireError::InvalidEncoding)
        );
    }
    assert_eq!(
        std::collections::BTreeMap::<u8, u8>::from_bytes(&[2, 0, 0, 0, 2, 0, 1, 0]),
        Err(WireError::InvalidEncoding)
    );
    let empty =
        EventLog::from_jsonl("{\"version\":0,\"batch_hex\":\"000000000000000000000000\"}").unwrap();
    assert_eq!(empty.batches, vec![Batch::new(vec![])]);
}

/// EventLog v1 (ADR 0013 §8): batches with P6 features export as v1 lines,
/// others stay v0; a v0 line carrying a P6 batch is refused.
#[test]
fn eventlog_v1_carries_branch_ops_only() {
    let plain = Batch::new(vec![Op::AssertBase {
        rev: RevisionId(0).into(),
    }]);
    let mut branch = Batch::new(vec![Op::CreateBranch { name: "b".into() }]);
    branch.branch = BranchId(1);
    let log = EventLog {
        batches: vec![plain.clone(), branch.clone()],
    };
    let text = log.to_jsonl();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("{\"version\":0,"));
    assert!(lines[1].starts_with("{\"version\":1,"));
    assert_eq!(EventLog::from_jsonl(&text).unwrap(), log);
    let forged = lines[1].replacen("\"version\":1", "\"version\":0", 1);
    assert_eq!(
        EventLog::from_jsonl(&forged),
        Err(WireError::UnknownVersion)
    );
}

/// P10 (SPEC §6.1 contract 16, SD21.7): replica-origin creates, MirrorAttach,
/// `gap` watermarks and the Replica channel round-trip at EventLog v4, while
/// Local creates, `gap = false` watermarks and Local logicals keep today's
/// tags and bytes.
#[test]
fn p10_replica_encodings_and_local_bytes_unchanged() {
    let team = [0xab; 16];
    let local = Op::CreateLogical {
        key: "fact:a".into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [7; 32],
        adopt_as_base: true,
        origin: Origin::Local,
    };
    let replica = Op::CreateLogical {
        key: format!("team/{}/k", "ab".repeat(16)),
        kind: RecordKind::Decision,
        scope_lane: format!("team:{}/main", "ab".repeat(16)),
        content: [7; 32],
        adopt_as_base: false,
        origin: Origin::Replica { team },
    };
    let wm = |gap| Op::ObserverWatermark {
        scope: format!("team:{}", "ab".repeat(16)),
        expected: None,
        position: vec![1, 2],
        gap,
    };
    let attach = Op::MirrorAttach {
        key: "team/x/k".into(),
        head: [9; 32],
        base: true,
    };
    // Tags: Local keeps 0 and 10; the new shapes take 22, 24, 23.
    assert_eq!(local.canonical_bytes()[0], 0);
    assert_eq!(replica.canonical_bytes()[0], 22);
    assert_eq!(wm(false).canonical_bytes()[0], 10);
    assert_eq!(wm(true).canonical_bytes()[0], 24);
    assert_eq!(attach.canonical_bytes()[0], 23);
    // A replica create is a Local create plus the team, nothing else.
    let mut l2 = local.canonical_bytes();
    l2[0] = 22;
    let mut as_replica = Op::CreateLogical {
        key: "fact:a".into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [7; 32],
        adopt_as_base: true,
        origin: Origin::Replica { team },
    }
    .canonical_bytes();
    assert_eq!(as_replica.split_off(l2.len()), team.to_vec());
    assert_eq!(as_replica, l2);
    for op in [&local, &replica, &wm(false), &wm(true), &attach] {
        assert_eq!(&Op::from_bytes(&op.canonical_bytes()).unwrap(), op);
    }
    // Logical: the kind byte's high bit marks a Replica origin.
    let mk = |origin| Logical {
        key: "k".into(),
        kind: RecordKind::Plan,
        scope_lane: "l".into(),
        owner: PrincipalId(0),
        origin_branch: MAIN,
        origin_epoch: Epoch(3),
        origin,
    };
    let lb = mk(Origin::Local).canonical_bytes();
    let rb = mk(Origin::Replica { team }).canonical_bytes();
    assert_eq!(lb[5], RecordKind::Plan as u8);
    assert_eq!(rb[5], RecordKind::Plan as u8 | 0x80);
    assert_eq!(rb.len(), lb.len() + 16);
    assert_eq!(
        Logical::from_bytes(&rb).unwrap(),
        mk(Origin::Replica { team })
    );
    assert_eq!(Logical::from_bytes(&lb).unwrap(), mk(Origin::Local));
    // EventLog: v4 for anything P10, and a v4 batch under a lower version
    // is refused.
    let mut b = Batch::new(vec![wm(false)]);
    b.channel = Channel::Replica;
    for batch in [
        b,
        Batch::new(vec![replica]),
        Batch::new(vec![attach]),
        Batch::new(vec![wm(true)]),
    ] {
        assert_eq!(EventLog::version_of(&batch), 4);
        assert_eq!(Batch::from_bytes(&batch.canonical_bytes()).unwrap(), batch);
        let log = EventLog {
            batches: vec![batch],
        };
        let text = log.to_jsonl();
        assert_eq!(EventLog::from_jsonl(&text).unwrap(), log);
        let forged = text.replacen("\"version\":4", "\"version\":3", 1);
        assert_eq!(
            EventLog::from_jsonl(&forged),
            Err(WireError::UnknownVersion)
        );
    }
    assert_eq!(EventLog::version_of(&Batch::new(vec![local])), 0);
}
