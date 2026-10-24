//! P6 visibility (SD19, ADR 0013 §2) and cross-branch dedup (SPEC §6.1
//! contract 5, ADR 0013 §11a): the fork boundary, inheritance through
//! forks, T73, rule imports, archived branches and T48 on a branch.
use contextdb_core::{Import, Kernel};
use contextdb_model::*;

fn create(key: &str, content: u8) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [content; 32],
        adopt_as_base: true,
    }
}
fn revise(l: u32, content: u8, adopt_as_base: bool) -> Op {
    Op::AddRevision {
        logical: LogicalRef::Existing(LogicalId(l)),
        content: [content; 32],
        adopt_as_base,
    }
}
fn rule(conclusion: RevRef, premise: RevRef) -> Op {
    Op::AddRule {
        conclusion,
        incidences: vec![(premise, EdgeKind::Pin)],
        enabled: true,
    }
}
fn rev(n: u32) -> RevRef {
    RevisionId(n).into()
}
fn on(branch: BranchId, ops: Vec<Op>) -> Batch {
    let mut b = Batch::new(ops);
    b.branch = branch;
    b
}
fn fork(k: &mut Kernel, from: BranchId, name: &str) -> BranchId {
    k.commit(&on(from, vec![Op::CreateBranch { name: name.into() }]))
        .unwrap();
    k.branch_by_name(name).unwrap()
}
fn visible(k: &Kernel, b: BranchId, r: u32) -> bool {
    k.branch(b).unwrap().is_visible_revision(RevisionId(r))
}
fn digest(k: &Kernel, b: BranchId) -> ContentHash {
    k.branch(b).unwrap().digest()
}
/// The branch's record of global revision `r`.
fn record(k: &Kernel, b: BranchId, content: u8) -> Revision {
    let inputs = k.branch(b).unwrap().db().inputs();
    inputs
        .revisions
        .into_iter()
        .find(|r| r.content == [content; 32])
        .unwrap()
}

/// A child sees what its parent saw at the fork and nothing the parent
/// creates later; a grandchild inherits through both forks (rule 3).
#[test]
fn fork_boundary_and_inheritance() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0)])).unwrap(); // r0
    let b = fork(&mut k, MAIN, "b");
    k.commit(&Batch::new(vec![create("late", 1)])).unwrap(); // r1, main only
    k.commit(&on(b, vec![create("on-b", 2)])).unwrap(); // r2, b only
    let c = fork(&mut k, b, "c");
    k.commit(&on(b, vec![create("b-late", 3)])).unwrap(); // r3, b after c forked
    assert!(visible(&k, b, 0) && !visible(&k, b, 1) && visible(&k, b, 2));
    assert!(visible(&k, c, 0) && !visible(&k, c, 1) && visible(&k, c, 2));
    assert!(!visible(&k, c, 3));
    assert!(!visible(&k, MAIN, 2) && !visible(&k, MAIN, 3));
    assert_eq!(k.branch(c).unwrap().info().parent, Some(b));
    // Main's later object is unknown on c.
    assert_eq!(
        k.commit(&on(c, vec![Op::AssertBase { rev: rev(1) }])),
        Err(TxError::UnknownRef)
    );
}

/// T73: b and c fork from main, where A@0 exists. b creates A@1 with content
/// X and a rule concluding on it; c's `AddRevision(A, X)` returns A@1 and
/// makes it visible on c, but not b's rule, and A@1 is not base on c. Main is
/// unchanged.
#[test]
fn t73_cross_branch_revision_dedup() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0), create("p", 1)]))
        .unwrap(); // A@0 = r0, P = r1
    let b = fork(&mut k, MAIN, "b");
    let c = fork(&mut k, MAIN, "c");
    let on_b = k
        .commit(&on(
            b,
            vec![revise(0, 9, true), rule(RevRef::New(0), rev(1))],
        ))
        .unwrap();
    let (a1, j) = (
        on_b.allocations.new_revisions[0],
        on_b.allocations.new_rules[0],
    );
    assert_eq!(a1, RevisionId(2));
    let main_digest = digest(&k, MAIN);
    let on_c = k.commit(&on(c, vec![revise(0, 9, false)])).unwrap();
    assert_eq!(on_c.allocations.new_revisions, vec![a1]);
    assert!(visible(&k, c, a1.0));
    assert_eq!(
        k.branch(c).unwrap().imports(),
        &[(Import::Revision(a1), on_c.epoch)]
    );
    // Base is per branch: true on b, false on c.
    assert!(k.branch(b).unwrap().is_base(a1).unwrap());
    assert!(!k.branch(c).unwrap().is_base(a1).unwrap());
    // b's rule is invisible on c.
    assert_eq!(
        k.commit(&on(
            c,
            vec![Op::SetRuleEnabled {
                rule: j.into(),
                enabled: false
            }]
        )),
        Err(TxError::UnknownRef)
    );
    // The copy keeps the original's provenance.
    let copy = record(&k, c, 9);
    assert_eq!((copy.origin_branch, copy.origin_epoch), (b, on_b.epoch));
    assert_eq!(digest(&k, MAIN), main_digest);
    // The next fresh revision continues the global sequence.
    let fresh = k.commit(&on(c, vec![revise(0, 10, false)])).unwrap();
    assert_eq!(fresh.allocations.new_revisions, vec![RevisionId(3)]);
}

/// An import is inherited by forks of the importing branch, and a branch
/// that already sees the revision dedups locally (no new row).
#[test]
fn imports_are_inherited_and_local_dedup_records_nothing() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    let c = fork(&mut k, MAIN, "c");
    k.commit(&on(b, vec![revise(0, 9, false)])).unwrap(); // r1
    k.commit(&on(c, vec![revise(0, 9, false)])).unwrap(); // import r1
    let d = fork(&mut k, c, "d");
    assert!(visible(&k, d, 1));
    assert!(k.branch(d).unwrap().imports().is_empty());
    let again = k.commit(&on(c, vec![revise(0, 9, true)])).unwrap();
    assert_eq!(again.allocations.new_revisions, vec![RevisionId(1)]);
    assert_eq!(k.branch(c).unwrap().imports().len(), 1);
}

/// §11a: an identical canonical rule on another branch is reused: same
/// RuleId, its `New(k)` slot used, `enabled` from the Op, and it enters the
/// branch's cycle graph.
#[test]
fn cross_branch_rule_dedup() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![
        create("a", 0),
        create("b", 1),
        create("c", 2),
    ]))
    .unwrap(); // r0, r1, r2
    let b = fork(&mut k, MAIN, "b");
    let c = fork(&mut k, MAIN, "c");
    let j = k.commit(&on(b, vec![rule(rev(0), rev(1))])).unwrap();
    let j = j.allocations.new_rules[0];
    let reused = k
        .commit(&on(
            c,
            vec![
                Op::AddRule {
                    conclusion: rev(0),
                    incidences: vec![(rev(1), EdgeKind::Pin)],
                    enabled: false,
                },
                Op::SetRuleEnabled {
                    rule: RuleRef::New(0),
                    enabled: false,
                },
            ],
        ))
        .unwrap();
    assert_eq!(reused.allocations.new_rules, vec![j]);
    assert_eq!(
        k.branch(c).unwrap().imports(),
        &[(Import::Rule(j), reused.epoch)]
    );
    // Within c it is now a duplicate.
    assert!(matches!(
        k.commit(&on(c, vec![rule(rev(0), rev(1))])),
        Err(TxError::DuplicateRule { existing, .. }) if existing == j
    ));
    // A fresh rule after the import gets the next global id.
    let fresh = k.commit(&on(c, vec![rule(rev(2), rev(1))])).unwrap();
    assert_eq!(fresh.allocations.new_rules, vec![RuleId(1)]);
    // An imported rule enters the branch's cycle graph: on d, which has
    // A → B, importing b's B → A is refused.
    let d = fork(&mut k, MAIN, "d");
    k.commit(&on(d, vec![rule(rev(1), rev(0))])).unwrap();
    assert!(matches!(
        k.commit(&on(d, vec![rule(rev(0), rev(1))])),
        Err(TxError::CycleDetected { .. })
    ));
}

/// A batch may name an object it imported, and the fresh objects after it,
/// by the global ids they receive.
#[test]
fn staged_imports_are_named_by_global_id() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    let c = fork(&mut k, MAIN, "c");
    k.commit(&on(b, vec![revise(0, 9, false)])).unwrap(); // r1 on b
    let r = k
        .commit(&on(
            c,
            vec![
                revise(0, 9, false),            // imports r1
                Op::AssertBase { rev: rev(1) }, // names it
                revise(0, 10, false),           // fresh: r2
                Op::AssertBase { rev: rev(2) },
            ],
        ))
        .unwrap();
    assert_eq!(
        r.allocations.new_revisions,
        vec![RevisionId(1), RevisionId(2)]
    );
    let c_view = k.branch(c).unwrap();
    assert!(c_view.is_base(RevisionId(1)).unwrap());
    assert!(c_view.is_base(RevisionId(2)).unwrap());
    // Naming the import before the Op that stages it is unknown.
    assert_eq!(
        k.commit(&on(
            MAIN,
            vec![Op::AssertBase { rev: rev(1) }, revise(0, 9, false)]
        )),
        Err(TxError::UnknownRef)
    );
}

/// Archived branches keep their objects: forks taken before the archive
/// still see them, and other branches can still import them. No branch can
/// be forked from an archived one.
#[test]
fn archived_branches() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&on(b, vec![revise(0, 9, false)])).unwrap(); // r1
    let child = fork(&mut k, b, "b-child");
    k.commit(&on(b, vec![Op::ArchiveBranch { name: "b".into() }]))
        .unwrap();
    assert!(visible(&k, child, 1));
    assert_eq!(
        k.commit(&on(b, vec![Op::CreateBranch { name: "x".into() }])),
        Err(TxError::BranchArchived { name: "b".into() })
    );
    let r = k.commit(&Batch::new(vec![revise(0, 9, false)])).unwrap();
    assert_eq!(r.allocations.new_revisions, vec![RevisionId(1)]);
}

/// T48 on a branch: one failing CAS in a multi-head batch rolls back all of
/// it, including staged imports.
#[test]
fn t48_multi_head_atomicity_on_a_branch() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0), create("b", 1)]))
        .unwrap();
    let b = fork(&mut k, MAIN, "b");
    let c = fork(&mut k, MAIN, "c");
    k.commit(&on(b, vec![revise(0, 9, false)])).unwrap(); // r2 on b
    let before = digest(&k, c);
    let set_head = |l: u32, expected: u32, new: RevRef| Op::SetHead {
        logical: LogicalRef::Existing(LogicalId(l)),
        expected: rev(expected),
        new,
    };
    let err = k.commit(&on(
        c,
        vec![
            revise(0, 9, false),
            set_head(0, 0, RevRef::New(0)),
            revise(1, 8, false),
            set_head(1, 1, RevRef::New(1)),
            // Stale: A's head is now the staged import, r2.
            revise(0, 7, false),
            set_head(0, 0, RevRef::New(2)),
        ],
    ));
    assert_eq!(
        err,
        Err(TxError::HeadConflict {
            logical: LogicalId(0),
            expected: RevisionId(0),
            actual: RevisionId(2),
        })
    );
    assert_eq!(digest(&k, c), before);
    assert!(!visible(&k, c, 2));
    assert!(k.branch(c).unwrap().imports().is_empty());
}

/// Prefixes every key a batch creates or names.
fn rename_keys(b: &Batch, prefix: &str) -> Batch {
    let l = |l: &LogicalRef| match l {
        LogicalRef::Key(k) => LogicalRef::Key(format!("{prefix}{k}")),
        other => other.clone(),
    };
    let mut b = b.clone();
    for op in &mut b.ops {
        match op {
            Op::CreateLogical { key, .. } => *key = format!("{prefix}{key}"),
            Op::AddRevision { logical, .. }
            | Op::SetHead { logical, .. }
            | Op::SetMetadata { logical, .. } => *logical = l(logical),
            Op::ExpectHeads { entries, .. } => {
                for (logical, _) in entries {
                    *logical = l(logical);
                }
            }
            _ => {}
        }
    }
    b
}

/// Object kinds for [`remap`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum K {
    Logical,
    Revision,
    Rule,
    Artifact,
}
/// A rewritten reference: an id, or the batch's `k`-th new object.
enum To {
    Id(u32),
    New(u16),
}
/// Rewrites every `Existing` id in a batch. Raw ids (read sets, curvecs)
/// cannot become `New`; `None` from `raw` drops the batch.
fn remap(
    b: &Batch,
    f: &dyn Fn(K, u32) -> To,
    raw: &dyn Fn(K, u32) -> Option<u32>,
) -> Option<Batch> {
    let l = |x: &LogicalRef| match x {
        LogicalRef::Existing(g) => match f(K::Logical, g.0) {
            To::Id(n) => LogicalRef::Existing(LogicalId(n)),
            To::New(k) => LogicalRef::New(k),
        },
        other => other.clone(),
    };
    let r = |x: &RevRef| match x {
        RevRef::Existing(g) => match f(K::Revision, g.0) {
            To::Id(n) => RevRef::Existing(RevisionId(n)),
            To::New(k) => RevRef::New(k),
        },
        other => *other,
    };
    let j = |x: &RuleRef| match x {
        RuleRef::Existing(g) => match f(K::Rule, g.0) {
            To::Id(n) => RuleRef::Existing(RuleId(n)),
            To::New(k) => RuleRef::New(k),
        },
        other => *other,
    };
    let a = |x: &ArtifactRef| match x {
        ArtifactRef::Existing(g) => match f(K::Artifact, g.0) {
            To::Id(n) => ArtifactRef::Existing(ArtifactId(n)),
            To::New(k) => ArtifactRef::New(k),
        },
        other => *other,
    };
    let mut out = b.clone();
    for op in &mut out.ops {
        match op {
            Op::AddRevision { logical, .. } | Op::SetMetadata { logical, .. } => {
                *logical = l(logical)
            }
            Op::SetHead {
                logical,
                expected,
                new,
            } => {
                *logical = l(logical);
                *expected = r(expected);
                *new = r(new);
            }
            Op::AssertBase { rev } | Op::RetractBase { rev } => *rev = r(rev),
            Op::AddRule {
                conclusion,
                incidences,
                ..
            } => {
                *conclusion = r(conclusion);
                for (p, _) in incidences {
                    *p = r(p);
                }
            }
            Op::SetRuleEnabled { rule, .. } => *rule = j(rule),
            Op::Rebind { rule, from, to } => {
                *rule = j(rule);
                *from = r(from);
                *to = r(to);
            }
            Op::RecordValidation {
                rule, curvec_seen, ..
            } => {
                *rule = j(rule);
                for (x, y) in curvec_seen {
                    *x = LogicalId(raw(K::Logical, x.0)?);
                    *y = RevisionId(raw(K::Revision, y.0)?);
                }
            }
            Op::OpenArtifact { target, .. } => *target = r(target),
            Op::ResolveArtifact { id } => *id = a(id),
            Op::ExpectHeads { entries, .. } => {
                for (x, y) in entries {
                    *x = l(x);
                    *y = RevisionId(raw(K::Revision, y.0)?);
                }
            }
            _ => {}
        }
    }
    Some(out)
}
fn allocated(r: &CommitResult, k: K) -> Vec<u32> {
    match k {
        K::Logical => r.allocations.new_logicals.iter().map(|x| x.0).collect(),
        K::Revision => r.allocations.new_revisions.iter().map(|x| x.0).collect(),
        K::Rule => r.allocations.new_rules.iter().map(|x| x.0).collect(),
        K::Artifact => r.allocations.new_artifacts.iter().map(|x| x.0).collect(),
    }
}

/// Siblings b and c fork after a trace's first batch and both run its
/// tail (c under renamed keys). c imports the revisions and rules b already
/// created on shared logicals, yet its database equals a standalone
/// database's that runs the same tail, and outcomes match op for op. The
/// trace's ids are standalone ids: pre-batch ones are translated through
/// c's id map; staged ones become `New(k)`, and are then resubmitted named
/// by the global ids they received.
#[test]
fn sibling_replay_imports_match_a_standalone_db() {
    let (mut imported, mut renamed_staged, mut skipped) = (0, 0, 0);
    for seed in 0..1000 {
        let trace = contextdb_testkit::generate(seed);
        let (first, tail) = trace.batches.split_first().unwrap();
        let mut k = Kernel::default();
        k.commit(first).unwrap();
        let b = fork(&mut k, MAIN, "b");
        let c = fork(&mut k, MAIN, "c");
        let mut db = contextdb_core::Db::default();
        db.commit(first).unwrap();
        for batch in tail {
            let mut on_b = batch.clone();
            on_b.branch = b;
            let _ = k.commit(&on_b);
        }
        for (tx, batch) in tail.iter().enumerate() {
            let batch = rename_keys(batch, "c-");
            let lens = [
                db.logical_count(),
                db.revision_count(),
                db.rule_count(),
                db.artifact_count(),
            ];
            let len = |kind: K| lens[kind as usize] as u32;
            let want = db.commit(&batch);
            let view = k.branch(c).unwrap();
            let global = |kind: K, n: u32| match kind {
                K::Logical => view.global_logical(LogicalId(n)).0,
                K::Revision => view.global_revision(RevisionId(n)).0,
                K::Rule => view.global_rule(RuleId(n)).0,
                K::Artifact => view.global_artifact(ArtifactId(n)).0,
            };
            let staged_ref = std::cell::Cell::new(false);
            let to_c = |kind: K, n: u32| {
                if n < len(kind) {
                    return To::Id(global(kind, n));
                }
                staged_ref.set(true);
                let slot = want
                    .as_ref()
                    .ok()
                    .and_then(|r| allocated(r, kind).iter().position(|x| *x == n));
                match slot {
                    Some(slot) => To::New(slot as u16),
                    // Unknown in the standalone database too.
                    None => To::Id(u32::MAX - 1),
                }
            };
            let raw = |kind: K, n: u32| (n < len(kind)).then(|| global(kind, n));
            let Some(mut on_c) = remap(&batch, &to_c, &raw) else {
                continue;
            };
            if want.is_err() && staged_ref.get() {
                // Staged ids of a failed batch cannot be translated; both
                // sides are unchanged by a failure anyway.
                skipped += 1;
                continue;
            }
            on_c.branch = c;
            let mut probe = k.clone();
            let got = probe.commit(&on_c);
            let shape = |r: &Result<CommitResult, TxError>| match r {
                Ok(r) => Ok([K::Logical, K::Revision, K::Rule, K::Artifact]
                    .map(|kind| allocated(r, kind).len())),
                Err(e) => Err(std::mem::discriminant(e)),
            };
            assert_eq!(
                shape(&got),
                shape(&want),
                "seed {seed} tx {tx}: {got:?} vs {want:?}"
            );
            // Name staged objects by the global ids they received.
            let resubmit = match (&got, staged_ref.get()) {
                (Ok(r), true) => {
                    renamed_staged += 1;
                    let by_id = |kind: K, n: u32| match to_c(kind, n) {
                        To::New(slot) => To::Id(allocated(r, kind)[slot as usize]),
                        id => id,
                    };
                    remap(&batch, &by_id, &raw).map(|mut x| {
                        x.branch = c;
                        x
                    })
                }
                _ => None,
            };
            match resubmit {
                Some(named) => assert_eq!(k.commit(&named), got, "seed {seed} tx {tx}"),
                None => k = probe,
            }
            let view = k.branch(c).unwrap();
            assert_eq!(
                view.db().derived_digest(),
                db.derived_digest(),
                "seed {seed} tx {tx}"
            );
        }
        imported += k.branch(c).unwrap().imports().len();
    }
    assert!(imported > 100, "the property exercises imports: {imported}");
    assert!(renamed_staged > 20, "and staged names: {renamed_staged}");
    eprintln!("imports {imported}, staged names {renamed_staged}, skipped {skipped}");
}
