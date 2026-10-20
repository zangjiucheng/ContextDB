//! ADR 0006 §3 step 2: MissingBlob at its §6.2 per-Op slot, identical in core and oracle.
use contextdb_core::Db;
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
/// Commits `setup` with every blob present, then `batch` with only `present`.
fn both(setup: &[Batch], batch: &Batch, present: &[u8]) -> Result<CommitResult, TxError> {
    let blobs: BTreeSet<ContentHash> = present.iter().map(|b| [*b; 32]).collect();
    let mut db = Db::default();
    let mut model = InputModel::default();
    for b in setup {
        db.commit(b).unwrap();
        model.apply(b).unwrap();
    }
    let before = db.digest();
    let core = db.commit_with(batch, &blobs);
    let oracle = model.apply_with(batch, &blobs);
    assert_eq!(
        core.as_ref().err(),
        oracle.as_ref().err(),
        "error equivalence"
    );
    if core.is_err() {
        assert_eq!(db.digest(), before, "rejected batch must not change state");
    }
    core
}
#[test]
fn missing_blob_follows_per_op_check_order() {
    // Present payloads commit normally.
    assert!(both(&[], &Batch::new(vec![create("a", 1)]), &[1]).is_ok());
    // CreateLogical: InvalidKey precedes MissingBlob; MissingBlob precedes LogicalExists.
    assert_eq!(
        both(&[], &Batch::new(vec![create("Bad", 1)]), &[]),
        Err(TxError::InvalidKey { key: "Bad".into() })
    );
    assert_eq!(
        both(
            &[Batch::new(vec![create("a", 1)])],
            &Batch::new(vec![create("a", 2)]),
            &[]
        ),
        Err(TxError::MissingBlob { content: [2; 32] })
    );
    // First failing Op wins: an earlier UnknownRef beats a later missing blob.
    let unknown = Op::AssertBase {
        rev: RevisionId(9).into(),
    };
    assert_eq!(
        both(&[], &Batch::new(vec![unknown, create("b", 3)]), &[]),
        Err(TxError::UnknownRef)
    );
    // AddRevision checks every occurrence, dedup hits included.
    let again = Op::AddRevision {
        logical: LogicalId(0).into(),
        content: [1; 32],
        adopt_as_base: false,
    };
    assert_eq!(
        both(
            &[Batch::new(vec![create("a", 1)])],
            &Batch::new(vec![again]),
            &[]
        ),
        Err(TxError::MissingBlob { content: [1; 32] })
    );
}
