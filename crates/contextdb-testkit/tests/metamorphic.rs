//! PROP7's permitted mutable permutation is SetHead(A) / AssertBase(B),
//! with disjoint read/write resources and no A5 dependency coupling.
//! Additional creation/artifact reordering tests identity normalization, without
//! claiming allocator-conflicting operations meet the sufficient-condition rule.
//! Split New(k) references are rewritten to the corresponding Existing IDs.
use contextdb_core::Db;
use contextdb_model::*;
use contextdb_oracle::jacobi;
use contextdb_testkit::create;
fn normalized(mut i: Inputs) -> Inputs {
    let mut ls: Vec<_> = i.logicals.into_iter().enumerate().collect();
    ls.sort_by(|a, b| a.1.key.cmp(&b.1.key));
    let mut lm = vec![LogicalId(0); ls.len()];
    for (n, (old, l)) in ls.iter_mut().enumerate() {
        lm[*old] = LogicalId(n as u32);
        l.origin_epoch = Epoch(0);
    }
    let mut rs: Vec<_> = i.revisions.into_iter().enumerate().collect();
    for (_, r) in &mut rs {
        r.logical = lm[r.logical.index()];
        r.origin_epoch = Epoch(0);
    }
    rs.sort_by_key(|(_, r)| (r.logical, r.content));
    let mut rm = vec![RevisionId(0); rs.len()];
    for (n, (old, _)) in rs.iter().enumerate() {
        rm[*old] = RevisionId(n as u32);
    }
    let mut js: Vec<_> = i.rules.into_iter().enumerate().collect();
    for (_, j) in &mut js {
        j.origin_epoch = Epoch(0);
        j.conclusion = rm[j.conclusion.index()];
        for p in &mut j.premises {
            p.premise = rm[p.premise.index()];
        }
        j.premises.sort();
        for r in &mut j.references {
            *r = rm[r.index()];
        }
        j.references.sort();
    }
    js.sort_by(|a, b| {
        (a.1.conclusion, &a.1.premises, &a.1.references).cmp(&(
            b.1.conclusion,
            &b.1.premises,
            &b.1.references,
        ))
    });
    let mut jm = vec![RuleId(0); js.len()];
    for (n, (old, _)) in js.iter().enumerate() {
        jm[*old] = RuleId(n as u32);
    }
    i.heads = ls
        .iter()
        .map(|(old, _)| rm[i.heads[*old].index()])
        .collect();
    i.metadata = ls.iter().map(|(old, _)| i.metadata[*old].clone()).collect();
    i.base = rs.iter().map(|(old, _)| i.base[*old]).collect();
    i.logicals = ls.into_iter().map(|(_, l)| l).collect();
    i.revisions = rs.into_iter().map(|(_, r)| r).collect();
    i.rules = js.into_iter().map(|(_, j)| j).collect();
    i.validations = i
        .validations
        .into_iter()
        .map(|((j, v), r)| {
            let mut v: Vec<_> = v
                .into_iter()
                .map(|(l, r)| (lm[l.index()], rm[r.index()]))
                .collect();
            v.sort();
            ((jm[j.index()], v), r)
        })
        .collect();
    for a in &mut i.artifacts {
        a.target = rm[a.target.index()];
        a.origin_epoch = Epoch(0);
    }
    i.artifacts
        .sort_by_key(|a| (a.ty as u8, a.target, a.payload_hash));
    i
}
fn apply(db: &mut Db, ops: Vec<Op>) -> CommitResult {
    let result = db.commit(&Batch::new(ops)).unwrap();
    assert_eq!(db.derived(), &jacobi(&db.inputs()));
    result
}
fn build(seed: u64, variant: usize) -> Inputs {
    let mut db = Db::default();
    let names = ["a", "b", "e", "d"];
    let order = if variant == 0 {
        vec![0, 1, 2, 3]
    } else if seed.is_multiple_of(2) {
        vec![3, 2, 1, 0]
    } else {
        vec![2, 0, 3, 1]
    };
    let mut ops: Vec<_> = order
        .iter()
        .map(|n| {
            create(
                names[*n],
                match n {
                    2 => RecordKind::Evidence,
                    3 => RecordKind::Summary,
                    _ => RecordKind::Fact,
                },
                *n == 0 || *n == 2,
                (*n as u8).wrapping_add(seed as u8),
            )
        })
        .collect();
    let local = |n| RevRef::New(order.iter().position(|v| *v == n).unwrap() as u16);
    let edge = if seed.is_multiple_of(3) {
        EdgeKind::Cur
    } else {
        EdgeKind::Pin
    };
    let first = Op::AddRule {
        conclusion: local(1),
        incidences: vec![(local(0), edge)],
        enabled: true,
    };
    if variant == 0 {
        ops.push(first);
        apply(&mut db, ops);
    } else {
        for op in ops {
            apply(&mut db, vec![op]);
        }
        apply(
            &mut db,
            vec![Op::AddRule {
                conclusion: RevisionId(order.iter().position(|v| *v == 1).unwrap() as u32).into(),
                incidences: vec![(
                    RevisionId(order.iter().position(|v| *v == 0).unwrap() as u32).into(),
                    edge,
                )],
                enabled: true,
            }],
        );
    }
    let i = db.inputs();
    let lid = |key: &str| LogicalId(i.logicals.iter().position(|l| l.key == key).unwrap() as u32);
    let a = lid("a");
    let b = lid("b");
    let d = lid("d");
    let old = i.heads[a.index()];
    apply(
        &mut db,
        vec![Op::AddRule {
            conclusion: i.heads[d.index()].into(),
            incidences: vec![(old.into(), EdgeKind::Sem)],
            enabled: true,
        }],
    );
    let artifacts = vec![
        Op::OpenArtifact {
            ty: ArtifactType::Conflict,
            target: old.into(),
            payload_hash: [1; 32],
        },
        Op::OpenArtifact {
            ty: ArtifactType::ImportBroken,
            target: i.heads[b.index()].into(),
            payload_hash: [2; 32],
        },
    ];
    if variant == 0 {
        apply(&mut db, artifacts);
    } else {
        for op in artifacts.into_iter().rev() {
            apply(&mut db, vec![op]);
        }
    }
    let revision = apply(
        &mut db,
        vec![Op::AddRevision {
            logical: a.into(),
            content: [200; 32],
            adopt_as_base: true,
        }],
    );
    let new = revision.allocations.new_revisions[0];
    let updates = vec![
        Op::SetHead {
            logical: a.into(),
            expected: old.into(),
            new: new.into(),
        },
        Op::AssertBase {
            rev: i.heads[b.index()].into(),
        },
    ];
    if variant == 0 {
        apply(&mut db, updates);
    } else {
        for op in updates.into_iter().rev() {
            apply(&mut db, vec![op]);
        }
    }
    apply(
        &mut db,
        vec![Op::RecordValidation {
            rule: RuleId(1).into(),
            curvec_seen: vec![(a, new)],
            verdict: Verdict::Verified,
            validator: "metamorphic".into(),
        }],
    );
    normalized(db.inputs())
}
#[test]
fn prop7_identity_renaming_permutation_split_merge_1000_seeds() {
    for seed in 0..1000 {
        let a = build(seed, 0);
        let b = build(seed, 1);
        assert_eq!(a.canonical_bytes(), b.canonical_bytes(), "seed {seed}");
        assert_eq!(jacobi(&a), jacobi(&b));
    }
}
