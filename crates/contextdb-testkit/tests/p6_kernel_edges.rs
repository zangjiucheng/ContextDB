//! P6 kernel edges the G3 mutation run found untested (ADR 0013 §6.2, §7,
//! §8 and §3.1): ValidationRecord conflicts in a merge and their payload,
//! the merge report's source, duplicate merge read sets, `main`'s identity
//! flag, a dry run of a batch that needs the import retry, snapshot
//! consistency on import, `state_diff`, the branch view's accessors and
//! epoch bounds, and a required entry the branch cannot see.
use contextdb_core::{Counting, IssueRequest, Kernel, LogLink};
use contextdb_model::*;
use contextdb_oracle::conflict_payload;
use std::collections::BTreeMap;

fn fact(key: &str, content: u8) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [content; 32],
        adopt_as_base: true,
    }
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
fn merge(k: &mut Kernel, into: BranchId, source: BranchId) -> (CommitResult, MergeReport) {
    let (r, report) = k
        .commit_detailed(&on(
            into,
            vec![Op::Merge {
                source,
                require_certified: false,
                certify: Vec::new(),
            }],
        ))
        .unwrap();
    (r, report.unwrap())
}
fn validate(rule: RuleId, verdict: Verdict, validator: &str) -> Op {
    Op::RecordValidation {
        rule: rule.into(),
        curvec_seen: vec![(LogicalId(0), RevisionId(0))],
        verdict,
        validator: validator.into(),
    }
}
/// `main` with a, b and a Sem rule a → b; two forks.
fn two_forks() -> (Kernel, BranchId, BranchId) {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![
        fact("a", 1),
        fact("b", 2),
        Op::AddRule {
            conclusion: RevRef::New(1),
            incidences: vec![(RevRef::New(0), EdgeKind::Sem)],
            enabled: true,
        },
    ]))
    .unwrap();
    let x = fork(&mut k, MAIN, "x");
    let y = fork(&mut k, MAIN, "y");
    (k, x, y)
}
fn enc(v: Option<(Verdict, &str)>) -> Vec<u8> {
    v.map_or(vec![0], |(verdict, by)| {
        [vec![1, verdict as u8], by.as_bytes().to_vec()].concat()
    })
}

/// Different verdicts for one (rule, curvec) conflict on the rule's
/// conclusion, with the canonical payload; the same verdict from another
/// validator converges.
#[test]
fn validation_records_conflict_or_converge() {
    let (mut k, x, y) = two_forks();
    k.commit(&on(x, vec![validate(RuleId(0), Verdict::Verified, "v1")]))
        .unwrap();
    k.commit(&on(y, vec![validate(RuleId(0), Verdict::Rejected, "v2")]))
        .unwrap();
    let (r, report) = merge(&mut k, y, x);
    assert_eq!(report.source, x);
    assert_eq!(report.conflicts.len(), 1, "{report:?}");
    let c = &report.conflicts[0];
    assert_eq!(
        (c.kind, c.logical, c.rule, c.target),
        (
            ConflictKind::Validation,
            Some(LogicalId(1)),
            Some(RuleId(0)),
            RevisionId(1)
        )
    );
    let values = [
        enc(None),
        enc(Some((Verdict::Verified, "v1"))),
        enc(Some((Verdict::Rejected, "v2"))),
    ];
    assert_eq!(
        c.payload,
        conflict_payload(r.epoch, x, ConflictKind::Validation as u8, 0, &values)
    );
    assert_eq!(report.convergent, 0);
    // The target's record stays.
    let view = k.branch(y).unwrap();
    let records = view.db().inputs().validations;
    assert_eq!(
        records.values().map(|v| v.verdict).collect::<Vec<_>>(),
        [Verdict::Rejected]
    );

    let (mut k, x, y) = two_forks();
    k.commit(&on(x, vec![validate(RuleId(0), Verdict::Verified, "v1")]))
        .unwrap();
    k.commit(&on(y, vec![validate(RuleId(0), Verdict::Verified, "v2")]))
        .unwrap();
    let (_, report) = merge(&mut k, y, x);
    assert!(report.conflicts.is_empty(), "{report:?}");
    assert_eq!(report.convergent, 1);
    assert_eq!(report.source, x);
}

/// A merge batch's `ExpectHeads` may list a logical twice only with one
/// revision.
#[test]
fn merge_read_set_duplicates() {
    let (mut k, x, _) = two_forks();
    k.commit(&on(x, vec![fact("c", 3)])).unwrap();
    let merge_with = |entries: Vec<(LogicalRef, RevisionId)>| {
        on(
            MAIN,
            vec![
                Op::ExpectHeads {
                    entries,
                    source: ReadSource::Client,
                },
                Op::Merge {
                    source: x,
                    require_certified: false,
                    certify: Vec::new(),
                },
            ],
        )
    };
    let a = LogicalRef::Existing(LogicalId(0));
    let twice = merge_with(vec![(a.clone(), RevisionId(0)), (a.clone(), RevisionId(0))]);
    assert!(k.what_if(&twice).is_ok());
    let differ = merge_with(vec![(a.clone(), RevisionId(0)), (a, RevisionId(1))]);
    assert_eq!(
        k.what_if(&differ),
        Err(TxError::InvalidReadSet {
            logical: LogicalId(0)
        })
    );
}

/// `main`'s ids equal the global ids until `main` sees an object created
/// elsewhere first.
#[test]
fn main_identity_until_renumbered() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![fact("a", 1)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    assert!(k.main_is_identity());
    k.commit(&on(b, vec![fact("x", 2)])).unwrap();
    k.commit(&Batch::new(vec![fact("y", 3)])).unwrap();
    assert!(!k.main_is_identity(), "y is global 2, local 1 on main");
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![fact("a", 1)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&Batch::new(vec![fact("y", 3)])).unwrap();
    k.commit(&on(b, vec![fact("x", 2)])).unwrap();
    assert!(k.main_is_identity(), "only b is renumbered");
}

/// A batch that names, by global id, a fresh object it stages after an
/// import is staged twice; the dry run agrees with the commit.
#[test]
fn dry_run_of_a_retried_batch() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![fact("a", 1)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    let imported = |content| Op::AddRevision {
        logical: LogicalId(0).into(),
        content: [content; 32],
        adopt_as_base: false,
    };
    k.commit(&on(b, vec![imported(9)])).unwrap(); // global r1, on b only
    let next = RevisionId(2);
    let batch = Batch::new(vec![
        imported(9), // an import of r1
        imported(8), // fresh: global r2
        Op::AssertBase { rev: next.into() },
    ]);
    let dry = k.what_if(&batch).unwrap();
    let real = k.commit(&batch).unwrap();
    assert_eq!(dry, real);
    assert_eq!(real.allocations.new_revisions, [RevisionId(1), next]);
    assert_eq!(real.net_delta.base_flips, [next]);
}

/// A snapshot whose global ids leave a gap is refused.
#[test]
fn import_refuses_inconsistent_ids() {
    let (k, _, _) = two_forks();
    let mut s = k.export();
    for state in &mut s.states {
        for g in &mut state.to_global[2] {
            *g += 5;
        }
    }
    assert!(Kernel::<Counting>::import(s).is_err());
    assert!(Kernel::<Counting>::import(k.export()).is_ok());
}

/// `state_diff` names the first differing part.
#[test]
fn state_diff_detects_differences() {
    let (k, x, _) = two_forks();
    let (a, b) = (k.branch(MAIN).unwrap(), k.branch(x).unwrap());
    assert_eq!(a.db().state_diff(b.db()), None);
    let mut k2 = Kernel::default();
    k2.commit(&Batch::new(vec![fact("other", 7)])).unwrap();
    assert_eq!(
        a.db().state_diff(k2.branch(MAIN).unwrap().db()),
        Some("inputs")
    );
}

/// The view's accessors answer in global ids on a renumbered branch, and
/// historical reads below the global epoch are served.
#[test]
fn view_accessors_on_a_renumbered_branch() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![fact("a", 1)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&Batch::new(vec![fact("m", 2), fact("n", 3)]))
        .unwrap();
    k.commit(&on(
        b,
        vec![
            fact("own", 4),
            Op::AddRule {
                conclusion: RevRef::New(0),
                incidences: vec![(RevisionId(0).into(), EdgeKind::Cur)],
                enabled: true,
            },
        ],
    ))
    .unwrap();
    let mid = k.epoch();
    k.commit(&on(
        b,
        vec![Op::SetRuleEnabled {
            rule: RuleId(0).into(),
            enabled: false,
        }],
    ))
    .unwrap();
    let view = k.branch(b).unwrap();
    let own = view.logical_by_key("own").unwrap();
    assert_eq!(own, LogicalId(3), "global 3, local 1");
    let r = view.head(own).unwrap();
    assert_eq!(view.epoch(), k.epoch());
    assert_ne!(view.epoch(), Epoch(0));
    assert_eq!(view.logical_count(), 2);
    assert_eq!(
        view.logical_info(own),
        Some(("own".into(), RecordKind::Fact, "repo".into()))
    );
    assert_eq!(view.revision_content(r), Ok([4; 32]));
    assert_eq!(view.rule_enabled(RuleId(0)), Some(false));
    assert_eq!(
        view.read_snapshot(own, mid).map(|(rev, _)| rev),
        Ok(r),
        "a snapshot below the global epoch"
    );
    assert!(view.diff(Epoch(0), mid).is_ok());
    assert!(view.diff(Epoch(0), Epoch(k.epoch().0 + 1)).is_err());
    let main = k.branch(MAIN).unwrap();
    assert_eq!(main.rule_enabled(RuleId(0)), None, "invisible on main");
    // An imported kernel's databases start at the snapshot epoch.
    let copy = Kernel::<Counting>::import(k.export()).unwrap();
    assert_eq!(copy.branch(b).unwrap().initial_epoch(), k.epoch());
}

/// Observation support and the unobserved bit of a merged anchor, through
/// the view.
#[test]
fn view_observation_support_and_unobserved_anchors() {
    let mut k = Kernel::default();
    // `t` is not base, so its support is the rule on the anchor.
    let t = Op::CreateLogical {
        key: "t".into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [2; 32],
        adopt_as_base: false,
    };
    k.commit(&Batch::new(vec![fact("pad", 1), t])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&Batch::new(vec![fact("m", 3)])).unwrap();
    let mut observe = on(
        b,
        vec![Op::CreateLogical {
            key: "code:repo/a.rs".into(),
            kind: RecordKind::CodeAnchor,
            scope_lane: "repo".into(),
            content: [200; 32],
            adopt_as_base: true,
        }],
    );
    observe.channel = Channel::Observer;
    let anchor_rev = k.commit(&observe).unwrap().allocations.new_revisions[0];
    k.commit(&on(
        b,
        vec![Op::AddRule {
            conclusion: RevisionId(1).into(),
            incidences: vec![(anchor_rev.into(), EdgeKind::Cur)],
            enabled: true,
        }],
    ))
    .unwrap();
    let view = k.branch(b).unwrap();
    let support = view.observation_support(&[RevisionId(1)]).unwrap();
    assert!(support.contains(&anchor_rev), "{support:?}");
    let anchor = view.logical_by_key("code:repo/a.rs").unwrap();
    assert!(
        !view.anchor_unobserved(anchor),
        "observed where it was made"
    );
    merge(&mut k, MAIN, b);
    let main = k.branch(MAIN).unwrap();
    assert!(main.anchor_unobserved(anchor), "not yet observed on main");
}

/// A certificate entry the branch cannot see, marked required, is a changed
/// entry; advisory, it is skipped.
#[test]
fn invisible_required_entry_is_changed() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![fact("a", 1)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&on(b, vec![fact("x", 2)])).unwrap();
    let view = k.branch(MAIN).unwrap();
    let entry = |logical, rev, required| CertItem {
        logical,
        rev,
        required,
        via: Via::Head,
        since_epoch: Epoch(0),
    };
    let request = IssueRequest {
        session_id: [1; 16],
        principal: PrincipalId(1),
        branch: MAIN,
        log_checksum: [0; 32],
        code_commit: "c".into(),
        strict_no_aba: false,
    };
    let none = BTreeMap::new();
    let cert = view
        .issue_certificate(&request, &[entry(LogicalId(0), RevisionId(0), true)], &none)
        .unwrap();
    let link = LogLink {
        matches: true,
        current_checksum: [0; 32],
    };
    let with = |required| {
        let mut c = cert.clone();
        c.entries.push(entry(LogicalId(1), RevisionId(1), required));
        c
    };
    assert_eq!(
        view.validate_certificate(&with(true), &none, link).unwrap(),
        Validation::Invalid {
            reason: InvalidReason::Changed,
            diffs: vec![CertDiff {
                logical: LogicalId(1),
                cert_rev: Some(RevisionId(1)),
                current: None,
                summary: None,
            }],
        }
    );
    assert!(matches!(
        view.validate_certificate(&with(false), &none, link)
            .unwrap(),
        Validation::Valid | Validation::ValidRecertified { .. }
    ));
}

/// A dry run that creates a logical with metadata leaves nothing behind:
/// the next logical to take its id has no metadata (WHAT_IF rollback).
#[test]
fn dry_run_drops_metadata_of_new_logicals() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![fact("a", 1)])).unwrap();
    let tagged = Batch::new(vec![
        fact("t", 2),
        Op::SetMetadata {
            logical: LogicalRef::New(0),
            tags: ["x".to_owned()].into(),
            dirs: ["src".to_owned()].into(),
        },
    ]);
    let before = k.branch(MAIN).unwrap().db().clone();
    k.what_if(&tagged).unwrap();
    assert_eq!(k.branch(MAIN).unwrap().db().state_diff(&before), None);
    k.commit(&Batch::new(vec![fact("u", 3)])).unwrap();
    let inputs = k.branch(MAIN).unwrap().db().inputs();
    assert_eq!(inputs.metadata[1], Metadata::default());
}

/// Rolling back many new keys exercises the key table's probe chains and
/// backward shifts: afterwards every old key still resolves and the new
/// ones are free again.
#[test]
fn dry_run_rolls_back_colliding_keys() {
    let mut k = Kernel::default();
    let old: Vec<Op> = (0..600).map(|n| fact(&format!("old{n}"), 1)).collect();
    k.commit(&Batch::new(old)).unwrap();
    let new: Vec<Op> = (0..900).map(|n| fact(&format!("new{n}"), 2)).collect();
    let batch = Batch::new(new);
    let before = k.branch(MAIN).unwrap().db().clone();
    k.what_if(&batch).unwrap();
    let db = k.branch(MAIN).unwrap().db();
    assert_eq!(db.state_diff(&before), None);
    for n in 0..600 {
        assert_eq!(db.logical_by_key(&format!("old{n}")), Some(LogicalId(n)));
    }
    assert!(k.commit(&batch).is_ok(), "the new keys are free");
}

/// Sync points that can no longer be merge bases drop their state (they
/// stay in the containment graph): merging two branches back and forth
/// keeps a bounded number of states, survives a snapshot round trip, and
/// later merges still find their bases.
#[test]
fn superseded_sync_points_are_pruned() {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![fact("a", 1)])).unwrap();
    let b = fork(&mut k, MAIN, "b");
    for n in 0..50u8 {
        let (into, source) = if n % 2 == 0 { (MAIN, b) } else { (b, MAIN) };
        k.commit(&on(source, vec![fact(&format!("f{n}"), n)]))
            .unwrap();
        merge(&mut k, into, source);
    }
    let s = k.export();
    let kept = s
        .syncs
        .iter()
        .filter(|x| x.3 != contextdb_core::PRUNED)
        .count();
    assert_eq!(s.syncs.len(), 51);
    assert!(kept <= 3, "{kept} sync states kept");
    let mut copy = Kernel::<Counting>::import(s).unwrap();
    for k in [&mut k, &mut copy] {
        k.commit(&on(b, vec![fact("late", 9)])).unwrap();
        let (_, report) = merge(k, MAIN, b);
        assert_eq!(report.newly_visible, [1, 1, 0, 0]);
        assert!(report.conflicts.is_empty());
    }
    assert_eq!(
        copy.branch(MAIN).unwrap().digest(),
        k.branch(MAIN).unwrap().digest()
    );
}
