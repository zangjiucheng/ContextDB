use contextdb_core::Db;
use contextdb_model::*;

fn main() -> Result<(), TxError> {
    let mut db = Db::default();
    let create = |key: &str, content, base| Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [content; 32],
        adopt_as_base: base,
    };
    let result = db.commit(&Batch::new(vec![
        create("fact:source", 1, true),
        create("fact:derived", 2, false),
        Op::AddRule {
            conclusion: RevRef::New(1),
            incidences: vec![(RevRef::New(0), EdgeKind::Pin)],
            enabled: true,
        },
    ]))?;
    let derived = result.allocations.new_revisions[1];
    assert_eq!(db.status(derived)?, Summary::Certified);
    let result = db.commit(&Batch::new(vec![Op::RetractBase {
        rev: result.allocations.new_revisions[0].into(),
    }]))?;
    assert_eq!(result.l0_flips, vec![RevisionId(0), derived]);
    assert_eq!(
        db.status(derived)?,
        Summary::Stale(StaleReason::Unsupported)
    );
    println!(
        "epoch={} derived={:?} flips={:?}",
        db.epoch().0,
        db.status(derived)?,
        result.l0_flips
    );
    Ok(())
}
