use contextdb_model::*;
use std::collections::BTreeSet;
#[test]
fn every_op_and_reference_round_trips_in_versioned_jsonl() {
    let ops = vec![
        Op::CreateLogical {
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
        },
        Op::SetPolicy {
            policy: PolicyDoc {
                version: 1,
                require_deps: BTreeSet::from([RecordKind::Summary, RecordKind::Plan]),
            },
        },
        Op::ObserverWatermark {
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
        EventLog::from_jsonl("{\"version\":2,\"batch_hex\":\"\"}"),
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
