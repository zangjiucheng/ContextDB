//! P7 milestone 1 (ADR 0015 §3): ValidationRecord metadata, the `if_absent`
//! CAS (`ValidationExists`, T81 at the kernel), `MissingBlob` for the new
//! blob fields at their §6.2 slot, and merge classification without `meta`.
//! Every kernel result is compared with the independent input model.
use contextdb_core::{Db, Kernel};
use contextdb_model::*;
use contextdb_oracle::InputModel;
use std::collections::BTreeSet;

fn create(key: &str, content: u8) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [content; 32],
        adopt_as_base: true,
    }
}
fn validate(verdict: Verdict, meta: Option<u8>, if_absent: bool) -> Op {
    Op::RecordValidation {
        rule: RuleId(0).into(),
        curvec_seen: vec![(LogicalId(0), RevisionId(2))],
        verdict,
        validator: "v".into(),
        meta: meta.map(|m| [m; 32]),
        if_absent,
    }
}
/// a = r0, b = r1 ← Sem a@r0; a moves to r2, so rule 0 is Pending at
/// curvec [(a, r2)].
fn setup() -> Vec<Batch> {
    vec![
        Batch::new(vec![create("a", 0), create("b", 1)]),
        Batch::new(vec![Op::AddRule {
            conclusion: RevisionId(1).into(),
            incidences: vec![(RevisionId(0).into(), EdgeKind::Sem)],
            enabled: true,
        }]),
        Batch::new(vec![
            Op::AddRevision {
                logical: LogicalId(0).into(),
                content: [2; 32],
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: LogicalId(0).into(),
                expected: RevisionId(0).into(),
                new: RevRef::New(0),
            },
        ]),
    ]
}
/// Engine and model side by side; every result must agree.
struct Both {
    db: Db,
    model: InputModel,
}
impl Both {
    fn new() -> Self {
        let mut both = Self {
            db: Db::default(),
            model: InputModel::default(),
        };
        for b in setup() {
            both.ok(&b);
        }
        both
    }
    fn commit_with(&mut self, b: &Batch, blobs: &dyn BlobPresence) -> Result<(), TxError> {
        let got = self.db.commit_with(b, blobs).map(|_| ());
        let want = self.model.apply_with(b, blobs).map(|_| ());
        assert_eq!(got, want, "engine and model disagree on {b:?}");
        got
    }
    fn commit(&mut self, b: &Batch) -> Result<(), TxError> {
        self.commit_with(b, &AllPresent)
    }
    fn ok(&mut self, b: &Batch) {
        self.commit(b).unwrap();
    }
    fn record(&self) -> Option<ValidationRecord> {
        let key = (RuleId(0), vec![(LogicalId(0), RevisionId(2))]);
        let got = self.db.inputs().validations.get(&key).cloned();
        assert_eq!(got.as_ref(), self.model.inputs().validations.get(&key));
        got
    }
}

/// T81 at the kernel: a worker's `if_absent` record never replaces an
/// existing verdict; an admin record (no `if_absent`) still does (SD10).
#[test]
fn t81_if_absent_never_overwrites_an_existing_verdict() {
    let mut s = Both::new();
    s.ok(&Batch::new(vec![validate(
        Verdict::Verified,
        Some(7),
        true,
    )]));
    let first = s.record().unwrap();
    assert_eq!(
        (first.verdict, first.meta),
        (Verdict::Verified, Some([7; 32]))
    );
    assert_eq!(
        s.commit(&Batch::new(vec![validate(Verdict::Rejected, None, true)])),
        Err(TxError::ValidationExists {
            rule: RuleId(0),
            curvec: vec![(LogicalId(0), RevisionId(2))],
            verdict: Verdict::Verified,
        })
    );
    assert_eq!(s.record(), Some(first), "the rejected batch left nothing");
    // The admin path: latest wins.
    s.ok(&Batch::new(vec![validate(Verdict::Rejected, None, false)]));
    assert_eq!(s.record().unwrap().verdict, Verdict::Rejected);
    // CurvecMoved comes before ValidationExists (§6.2 CAS class).
    let mut stale = validate(Verdict::Verified, None, true);
    if let Op::RecordValidation { curvec_seen, .. } = &mut stale {
        *curvec_seen = vec![(LogicalId(0), RevisionId(0))];
    }
    assert!(matches!(
        s.commit(&Batch::new(vec![stale])),
        Err(TxError::CurvecMoved { .. })
    ));
}

/// The metadata changes no derived state: the same verdict with and without
/// `meta` gives the same derived digest.
#[test]
fn meta_never_changes_derived_state() {
    let mut a = Both::new();
    let mut b = Both::new();
    a.ok(&Batch::new(vec![validate(Verdict::Verified, None, false)]));
    b.ok(&Batch::new(vec![validate(
        Verdict::Verified,
        Some(3),
        true,
    )]));
    assert_eq!(a.db.derived_digest(), b.db.derived_digest());
    assert_ne!(a.db.digest(), b.db.digest(), "the input differs");
}

/// `MissingBlob` covers `meta` and `note`, before NoSemIncidence and the
/// CAS class (§6.2 order); present blobs pass.
#[test]
fn missing_blob_covers_meta_and_note() {
    let mut s = Both::new();
    let none: BTreeSet<ContentHash> = BTreeSet::new();
    let meta: BTreeSet<ContentHash> = [[7; 32]].into();
    assert_eq!(
        s.commit_with(
            &Batch::new(vec![validate(Verdict::Verified, Some(7), true)]),
            &none
        ),
        Err(TxError::MissingBlob { content: [7; 32] })
    );
    // A stale curvec with a missing blob reports the blob first.
    let mut stale = validate(Verdict::Verified, Some(7), false);
    if let Op::RecordValidation { curvec_seen, .. } = &mut stale {
        *curvec_seen = Vec::new();
    }
    assert_eq!(
        s.commit_with(&Batch::new(vec![stale]), &none),
        Err(TxError::MissingBlob { content: [7; 32] })
    );
    s.commit_with(
        &Batch::new(vec![validate(Verdict::Verified, Some(7), true)]),
        &meta,
    )
    .unwrap();
    // ResolveArtifact's note, before ArtifactNotOpen.
    s.ok(&Batch::new(vec![Op::OpenArtifact {
        ty: ArtifactType::Conflict,
        target: RevisionId(1).into(),
        payload_hash: [1; 32],
    }]));
    let resolve = |note: u8| {
        Batch::new(vec![Op::ResolveArtifact {
            id: ArtifactId(0).into(),
            note: Some([note; 32]),
        }])
    };
    assert_eq!(
        s.commit_with(&resolve(5), &none),
        Err(TxError::MissingBlob { content: [5; 32] })
    );
    let note: BTreeSet<ContentHash> = [[5; 32]].into();
    s.commit_with(&resolve(5), &note).unwrap();
    assert_eq!(
        s.commit_with(&resolve(6), &none),
        Err(TxError::MissingBlob { content: [6; 32] }),
        "the blob is checked before ArtifactNotOpen"
    );
    assert!(matches!(
        s.commit(&resolve(6)),
        Err(TxError::ArtifactNotOpen { .. })
    ));
}

/// Old tags while the new fields are unset; tags 18/19 otherwise; a tag-18
/// or tag-19 encoding without a new field is not canonical.
#[test]
fn format4_tags_are_canonical() {
    let old = validate(Verdict::Verified, None, false);
    assert_eq!(old.canonical_bytes()[0], 8);
    let new = validate(Verdict::Verified, None, true);
    assert_eq!(new.canonical_bytes()[0], 18);
    assert_eq!(Op::from_bytes(&new.canonical_bytes()), Ok(new));
    let mut forged = old.canonical_bytes();
    forged[0] = 18;
    forged.extend([0u8, 0u8]); // meta None, if_absent false
    assert_eq!(Op::from_bytes(&forged), Err(WireError::InvalidEncoding));
    let resolve = Op::ResolveArtifact {
        id: ArtifactId(3).into(),
        note: None,
    };
    assert_eq!(resolve.canonical_bytes()[0], 13);
    let noted = Op::ResolveArtifact {
        id: ArtifactId(3).into(),
        note: Some([1; 32]),
    };
    assert_eq!(noted.canonical_bytes()[0], 19);
    assert_eq!(Op::from_bytes(&noted.canonical_bytes()), Ok(noted));
    // A record without metadata encodes as in format 3 (digests unchanged).
    let bare = ValidationRecord {
        verdict: Verdict::Rejected,
        validator: "v".into(),
        meta: None,
    };
    let mut v3 = vec![1u8];
    "v".to_owned().encode(&mut v3);
    assert_eq!(bare.canonical_bytes(), v3);
    let with = ValidationRecord {
        meta: Some([4; 32]),
        ..bare
    };
    assert_eq!(
        ValidationRecord::from_bytes(&with.canonical_bytes()),
        Ok(with)
    );
}

/// Merge classifies validation records without `meta` (ADR 0015 §3.1): a
/// source that only changed `meta` changes nothing; a record taken from the
/// source keeps its `meta`.
#[test]
fn merge_classifies_validations_without_meta() {
    let on = |branch: BranchId, ops: Vec<Op>| {
        let mut b = Batch::new(ops);
        b.branch = branch;
        b
    };
    let mut k = Kernel::default();
    for b in setup() {
        k.commit(&b).unwrap();
    }
    k.commit(&Batch::new(vec![Op::CreateBranch { name: "b".into() }]))
        .unwrap();
    let b = k.branch_by_name("b").unwrap();
    // Taken: only the source has a record.
    k.commit(&on(b, vec![validate(Verdict::Verified, Some(9), true)]))
        .unwrap();
    let merge = |k: &mut Kernel| {
        k.commit_detailed(&Batch::new(vec![Op::Merge {
            source: b,
            require_certified: false,
            certify: Vec::new(),
        }]))
        .unwrap()
        .1
        .unwrap()
    };
    let report = merge(&mut k);
    assert!(report.conflicts.is_empty());
    let key = (RuleId(0), vec![(LogicalId(0), RevisionId(2))]);
    let main = |k: &Kernel| {
        k.branch(MAIN)
            .unwrap()
            .db()
            .inputs()
            .validations
            .get(&key)
            .cloned()
    };
    assert_eq!(main(&k).unwrap().meta, Some([9; 32]), "taken with its meta");
    // The source then changes only meta: nothing to take, no conflict.
    k.commit(&on(b, vec![validate(Verdict::Verified, Some(10), false)]))
        .unwrap();
    let report = merge(&mut k);
    assert!(report.conflicts.is_empty());
    assert_eq!(
        main(&k).unwrap().meta,
        Some([9; 32]),
        "the target keeps its record"
    );
}

/// EventLog v2 (ADR 0015 §9): a batch using a format-4 field is written as
/// v2, and a v1 line carrying one is refused.
#[test]
fn eventlog_v2_carries_format4_ops() {
    let new = Batch::new(vec![validate(Verdict::Verified, Some(1), false)]);
    let old = Batch::new(vec![validate(Verdict::Verified, None, false)]);
    assert_eq!(EventLog::version_of(&new), 2);
    assert_eq!(EventLog::version_of(&old), 0);
    let log = EventLog {
        batches: vec![old, new],
    };
    let text = log.to_jsonl();
    assert!(text.starts_with("{\"version\":0,"), "{text}");
    assert!(text.contains("{\"version\":2,"), "{text}");
    assert_eq!(EventLog::from_jsonl(&text), Ok(log));
    let downgraded = text.replace("{\"version\":2,", "{\"version\":1,");
    assert_eq!(
        EventLog::from_jsonl(&downgraded),
        Err(WireError::UnknownVersion)
    );
}
