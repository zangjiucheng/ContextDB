//! P6 merge (SPEC SD19, ADR 0013 §6): merge base by containment through
//! sync points, three-way classification, conflicts and the gate, the cycle
//! recheck (T72), certification (T20a/T20b), read sets and dry runs.
use contextdb_core::Kernel;
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
fn revise(l: u32, content: u8) -> Op {
    Op::AddRevision {
        logical: LogicalRef::Existing(LogicalId(l)),
        content: [content; 32],
        adopt_as_base: true,
    }
}
fn set_head(l: u32, expected: u32, new: RevRef) -> Op {
    Op::SetHead {
        logical: LogicalRef::Existing(LogicalId(l)),
        expected: RevisionId(expected).into(),
        new,
    }
}
/// Moves logical `l` from `from` to a new revision with `content`.
fn move_to(l: u32, from: u32, content: u8) -> Vec<Op> {
    vec![revise(l, content), set_head(l, from, RevRef::New(0))]
}
fn rule(conclusion: u32, premise: u32, kind: EdgeKind) -> Op {
    Op::AddRule {
        conclusion: RevisionId(conclusion).into(),
        incidences: vec![(RevisionId(premise).into(), kind)],
        enabled: true,
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
fn merge_op(source: BranchId, require_certified: bool) -> Op {
    Op::Merge {
        source,
        require_certified,
        certify: Vec::new(),
    }
}
fn merge(k: &mut Kernel, source: BranchId, target: BranchId) -> (CommitResult, MergeReport) {
    let (r, report) = k
        .commit_detailed(&on(target, vec![merge_op(source, false)]))
        .unwrap();
    (r, report.unwrap())
}
fn head(k: &Kernel, b: BranchId, l: u32) -> Option<RevisionId> {
    k.branch(b).unwrap().head(LogicalId(l))
}
fn digest(k: &Kernel, b: BranchId) -> ContentHash {
    k.branch(b).unwrap().digest()
}
/// Main has A = r0 and B = r1, both base.
fn setup() -> Kernel {
    let mut k = Kernel::default();
    k.commit(&Batch::new(vec![create("a", 0), create("b", 1)]))
        .unwrap();
    k
}

/// The source's changes arrive; the target's own changes stay; the source
/// is untouched; a merge then fork lets the fork reference merged objects
/// (B1).
#[test]
fn merge_takes_source_changes_and_keeps_target_changes() {
    let mut k = setup();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&on(b, move_to(0, 0, 10))).unwrap(); // A → r2 on b
    let mut x = create("x", 11);
    if let Op::CreateLogical { adopt_as_base, .. } = &mut x {
        *adopt_as_base = false;
    }
    k.commit(&on(b, vec![x, rule(3, 1, EdgeKind::Pin)]))
        .unwrap(); // x = r3 ← B
    k.commit(&Batch::new(move_to(1, 1, 20))).unwrap(); // B → r4 on main
    let source_digest = digest(&k, b);
    let (r, report) = merge(&mut k, b, MAIN);
    assert_eq!(head(&k, MAIN, 0), Some(RevisionId(2)));
    assert_eq!(head(&k, MAIN, 1), Some(RevisionId(4)));
    assert_eq!(
        k.branch(MAIN).unwrap().logical_by_key("x"),
        Some(LogicalId(2))
    );
    assert_eq!(
        report.moved_heads,
        vec![(LogicalId(0), RevisionId(0), RevisionId(2))]
    );
    assert_eq!(report.newly_visible, [1, 2, 1, 0]);
    assert!(report.conflicts.is_empty());
    assert!(r.allocations.new_artifacts.is_empty());
    assert_eq!(digest(&k, b), source_digest, "the source is unchanged");
    // x's rule premise B@r1 is not B's head on main: x is stale there.
    assert!(matches!(
        k.branch(MAIN).unwrap().status(RevisionId(3)).unwrap(),
        Summary::Stale(_)
    ));
    // Merge, then fork, then reference (B1).
    let c = fork(&mut k, MAIN, "c");
    k.commit(&on(
        c,
        vec![Op::AssertBase {
            rev: RevisionId(3).into(),
        }],
    ))
    .unwrap();
}

/// A repeated merge takes only what changed since the last sync point, both
/// ways; merging back is a no-op once both agree.
#[test]
fn repeated_merges_both_ways() {
    let mut k = setup();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&on(b, move_to(0, 0, 10))).unwrap(); // r2
    merge(&mut k, b, MAIN);
    // Main moves A on; b moves B. A merge of b must not revert main's A.
    k.commit(&Batch::new(move_to(0, 2, 11))).unwrap(); // r3
    k.commit(&on(b, move_to(1, 1, 12))).unwrap(); // r4
    let (_, report) = merge(&mut k, b, MAIN);
    assert_eq!(head(&k, MAIN, 0), Some(RevisionId(3)));
    assert_eq!(head(&k, MAIN, 1), Some(RevisionId(4)));
    assert!(report.conflicts.is_empty());
    // And back: b takes main's A.
    let (_, report) = merge(&mut k, MAIN, b);
    assert_eq!(head(&k, b, 0), Some(RevisionId(3)));
    assert_eq!(
        report.moved_heads,
        vec![(LogicalId(0), RevisionId(2), RevisionId(3))]
    );
    let (_, report) = merge(&mut k, b, MAIN);
    assert!(report.moved_heads.is_empty() && report.newly_visible == [0; 4]);
}

/// Both sides move a head differently: the target keeps its head and a
/// Conflict on it blocks CERTIFIED; the same move on both is convergent.
#[test]
fn head_conflict_and_convergence() {
    let mut k = setup();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&on(b, move_to(0, 0, 10))).unwrap(); // r2
    k.commit(&Batch::new(move_to(0, 0, 20))).unwrap(); // r3
    // Convergent: both move B to the same content (one id by dedup).
    k.commit(&on(b, move_to(1, 1, 30))).unwrap(); // r4
    k.commit(&Batch::new(move_to(1, 1, 30))).unwrap(); // imports r4
    let (r, report) = merge(&mut k, b, MAIN);
    assert_eq!(head(&k, MAIN, 0), Some(RevisionId(3)));
    // B's head and r4's base bit.
    assert_eq!(report.convergent, 2);
    assert_eq!(report.conflicts.len(), 1);
    let c = &report.conflicts[0];
    assert_eq!(
        (c.kind, c.logical, c.target),
        (ConflictKind::Head, Some(LogicalId(0)), RevisionId(3))
    );
    assert_eq!(r.allocations.new_artifacts, vec![c.artifact]);
    let main = k.branch(MAIN).unwrap();
    assert_eq!(main.status(RevisionId(3)).unwrap(), Summary::Conflicted);
    let inputs = main.db().inputs();
    let art = inputs.artifacts.last().unwrap();
    assert_eq!(art.ty, ArtifactType::Conflict);
    assert_eq!(art.payload_hash, *blake3::hash(&c.payload).as_bytes());
    // The default merge refuses a conflict (step 6).
    let b2 = fork(&mut k, b, "b2");
    k.commit(&on(b2, move_to(0, 2, 40))).unwrap();
    k.commit(&Batch::new(vec![Op::ResolveArtifact {
        id: c.artifact.into(),
    }]))
    .unwrap();
    k.commit(&Batch::new(move_to(0, 3, 41))).unwrap();
    let refused = k.commit(&Batch::new(vec![merge_op(b2, true)]));
    assert!(
        matches!(
            refused,
            Err(TxError::MergeNotCertified {
                logical: LogicalId(0),
                status: Summary::Conflicted
            })
        ),
        "{refused:?}"
    );
}

/// T72, second half: `B → A` on b, `A → B` on main; merging b into main
/// is refused with `CycleDetected` and main is unchanged.
#[test]
fn t72_merge_rechecks_the_cycle_graph() {
    let mut k = setup();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&on(b, vec![rule(0, 1, EdgeKind::Pin)])).unwrap();
    k.commit(&Batch::new(vec![rule(1, 0, EdgeKind::Pin)]))
        .unwrap();
    let before = digest(&k, MAIN);
    let epoch = k.epoch();
    let err = k.commit(&Batch::new(vec![merge_op(b, false)]));
    assert!(
        matches!(err, Err(TxError::CycleDetected { op_index: 0, .. })),
        "{err:?}"
    );
    assert_eq!(digest(&k, MAIN), before);
    assert_eq!(k.epoch(), epoch);
}

/// T20a/T20b: on b, `D2[Cur] → D1@v1`; main moves D1 to v2. Merging b into
/// main leaves D2 STALE, and with `require_certified` the merge is refused.
#[test]
fn t20_merge_and_certification() {
    let mut k = setup(); // D1 = A (r0)
    let b = fork(&mut k, MAIN, "b");
    k.commit(&on(
        b,
        vec![
            Op::CreateLogical {
                key: "d2".into(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: [5; 32],
                adopt_as_base: false,
            },
            rule(2, 0, EdgeKind::Cur),
        ],
    ))
    .unwrap(); // D2 = r2
    k.commit(&Batch::new(move_to(0, 0, 9))).unwrap(); // D1 → r3
    let before = digest(&k, MAIN);
    assert_eq!(
        k.commit(&Batch::new(vec![merge_op(b, true)])),
        Err(TxError::MergeNotCertified {
            logical: LogicalId(2),
            status: Summary::Stale(StaleReason::Unsupported),
        })
    );
    assert_eq!(digest(&k, MAIN), before, "T20b: refused, main unchanged");
    merge(&mut k, b, MAIN);
    assert!(matches!(
        k.branch(MAIN).unwrap().status(RevisionId(2)).unwrap(),
        Summary::Stale(_)
    ));
    // `certify` names what must be CERTIFIED; unknown names are UnknownRef.
    let c = fork(&mut k, MAIN, "c");
    k.commit(&on(c, vec![create("ok", 7)])).unwrap();
    let certify = |l: LogicalRef| Op::Merge {
        source: c,
        require_certified: true,
        certify: vec![l],
    };
    assert_eq!(
        k.commit(&Batch::new(vec![certify(LogicalRef::Key("nope".into()))])),
        Err(TxError::UnknownRef)
    );
    k.commit(&Batch::new(vec![certify(LogicalRef::Key("ok".into()))]))
        .unwrap();
}

/// The B2 revert example: S moves s0 → s1, s1 reaches T through C, then S
/// reverts to s0. Merging S into T takes the revert.
#[test]
fn revert_through_a_third_branch() {
    let mut k = setup();
    let s = fork(&mut k, MAIN, "s");
    let c = fork(&mut k, MAIN, "c");
    let t = fork(&mut k, MAIN, "t");
    k.commit(&on(s, move_to(0, 0, 10))).unwrap(); // s1 = r2
    merge(&mut k, s, c);
    merge(&mut k, c, t);
    assert_eq!(head(&k, t, 0), Some(RevisionId(2)));
    k.commit(&on(s, vec![set_head(0, 2, RevisionId(0).into())]))
        .unwrap(); // revert
    let (_, report) = merge(&mut k, s, t);
    assert_eq!(head(&k, t, 0), Some(RevisionId(0)));
    assert!(report.conflicts.is_empty());
}

/// Criss-cross: two maximal bases that disagree on a boolean conflict; on
/// which they agree, the value merges normally.
#[test]
fn criss_cross_fails_closed() {
    let mut k = setup();
    let x = fork(&mut k, MAIN, "x");
    let y = fork(&mut k, MAIN, "y");
    // x retracts A's base, y retracts B's base.
    k.commit(&on(
        x,
        vec![Op::RetractBase {
            rev: RevisionId(0).into(),
        }],
    ))
    .unwrap();
    k.commit(&on(
        y,
        vec![Op::RetractBase {
            rev: RevisionId(1).into(),
        }],
    ))
    .unwrap();
    // Cross-merge: two sync points, neither containing the other.
    let xs = fork(&mut k, x, "xs");
    let ys = fork(&mut k, y, "ys");
    merge(&mut k, ys, x);
    merge(&mut k, xs, y);
    // Now x re-asserts A, and y keeps it retracted: x and y differ on A,
    // and the two bases (x before, y before) disagree on A too.
    k.commit(&on(
        x,
        vec![Op::AssertBase {
            rev: RevisionId(0).into(),
        }],
    ))
    .unwrap();
    let (_, report) = merge(&mut k, x, y);
    let kinds: Vec<_> = report.conflicts.iter().map(|c| c.kind).collect();
    assert_eq!(kinds, vec![ConflictKind::Base], "{report:?}");
    assert_eq!(report.conflicts[0].target, RevisionId(0));
    assert!(!k.branch(y).unwrap().is_base(RevisionId(0)).unwrap());
}

/// Policy conflicts refuse the merge; metadata conflicts are listed, with
/// `dirs` intersected by prefix when both narrowed.
#[test]
fn policy_and_metadata() {
    let mut k = setup();
    let b = fork(&mut k, MAIN, "b");
    let narrow = |l: u32, dirs: &[&str]| Op::SetMetadata {
        logical: LogicalRef::Existing(LogicalId(l)),
        tags: Default::default(),
        dirs: dirs.iter().map(|d| d.to_string()).collect(),
    };
    k.commit(&on(b, vec![narrow(0, &["src", "docs"])])).unwrap();
    k.commit(&Batch::new(vec![narrow(0, &["src/core", "tests"])]))
        .unwrap();
    let (_, report) = merge(&mut k, b, MAIN);
    assert!(report.metadata_conflicts.is_empty());
    let dirs = k.branch(MAIN).unwrap().db().inputs().metadata[0]
        .dirs
        .clone();
    assert_eq!(dirs, ["src/core".to_owned()].into());
    // Disjoint narrowings: listed, target keeps its value.
    let c = fork(&mut k, MAIN, "c");
    k.commit(&on(c, vec![narrow(1, &["docs"])])).unwrap();
    k.commit(&Batch::new(vec![narrow(1, &["tests"])])).unwrap();
    let (_, report) = merge(&mut k, c, MAIN);
    assert_eq!(
        report.metadata_conflicts,
        vec![(LogicalId(1), MetadataField::Dirs)]
    );
    assert!(report.conflicts.is_empty(), "metadata never gates");
    // Policy changed differently on both sides.
    let d = fork(&mut k, MAIN, "d");
    let policy = |branch, kind| {
        let mut x = on(
            branch,
            vec![Op::SetPolicy {
                policy: PolicyDoc {
                    version: 1,
                    require_deps: [kind].into_iter().collect(),
                },
            }],
        );
        x.channel = Channel::Admin;
        x
    };
    k.commit(&policy(d, RecordKind::Plan)).unwrap();
    k.commit(&policy(MAIN, RecordKind::Decision)).unwrap();
    assert_eq!(
        k.commit(&Batch::new(vec![merge_op(d, false)])),
        Err(TxError::MergeConflict {
            kind: MergeConflictKind::Policy
        })
    );
}

/// Merge validation, read sets, archived sources and dry runs.
#[test]
fn merge_checks_read_sets_and_dry_runs() {
    let mut k = setup();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&on(b, move_to(0, 0, 10))).unwrap();
    assert_eq!(
        k.commit(&Batch::new(vec![merge_op(MAIN, false)])),
        Err(TxError::InvalidMerge)
    );
    assert_eq!(
        k.commit(&Batch::new(vec![merge_op(BranchId(77), false)])),
        Err(TxError::InvalidMerge)
    );
    assert_eq!(
        k.commit(&Batch::new(vec![merge_op(b, false), create("z", 3)])),
        Err(TxError::SoleOpRequired { op: "Merge".into() })
    );
    let read = |r: u32| Op::ExpectHeads {
        entries: vec![(LogicalRef::Existing(LogicalId(1)), RevisionId(r))],
        source: ReadSource::Client,
    };
    k.commit(&Batch::new(move_to(1, 1, 20))).unwrap(); // B → r3 on main
    assert_eq!(
        k.commit(&Batch::new(vec![read(1), merge_op(b, false)])),
        Err(TxError::ReadSetConflict {
            logical: LogicalId(1),
            read: RevisionId(1),
            now: RevisionId(3),
        })
    );
    // A dry run changes nothing and promises the real result.
    let before = (digest(&k, MAIN), k.epoch());
    let batch = Batch::new(vec![read(3), merge_op(b, false)]);
    let dry = k.what_if_detailed(&batch).unwrap();
    assert_eq!((digest(&k, MAIN), k.epoch()), before);
    // Archived sources can still be merged.
    k.commit(&on(b, vec![Op::ArchiveBranch { name: "b".into() }]))
        .unwrap();
    let real = k.commit_detailed(&batch).unwrap();
    assert_eq!(real.1, dry.1);
    assert_eq!(real.0.allocations, dry.0.allocations);
    assert_eq!(head(&k, MAIN, 0), Some(RevisionId(2)));
}

/// The observer domain (§6.3, §6.2 N9/R2): an anchor the target already
/// sees keeps the target's head and base; a newly visible anchor arrives
/// with the source's head as an initial observation but reads
/// `PENDING(ObservationLag)` until the target's next watermark for its
/// scope; `AnchorBroken` is never imported.
#[test]
fn observer_domain() {
    let observed = |branch, ops| {
        let mut b = on(branch, ops);
        b.channel = Channel::Observer;
        b
    };
    let anchor = |key: &str, content: u8| Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::CodeAnchor,
        scope_lane: "fs".into(),
        content: [content; 32],
        adopt_as_base: true,
    };
    let watermark = |expected: Option<Vec<u8>>, position: u8| Op::ObserverWatermark {
        scope: "fs".into(),
        expected,
        position: vec![position],
    };
    let mut k = Kernel::default();
    k.commit(&observed(
        MAIN,
        vec![anchor("code:a.rs#f", 0), watermark(None, 1)],
    ))
    .unwrap(); // anchor A = r0
    let b = fork(&mut k, MAIN, "b");
    // On b the observer moves A and creates a new anchor N, and A breaks.
    k.commit(&observed(
        b,
        vec![
            revise(0, 10),
            set_head(0, 0, RevRef::New(0)),
            anchor("code:n.rs#g", 20),
            Op::OpenArtifact {
                ty: ArtifactType::AnchorBroken,
                target: RevisionId(0).into(),
                payload_hash: [3; 32],
            },
        ],
    ))
    .unwrap(); // A → r1, N = r2
    let (_, report) = merge(&mut k, b, MAIN);
    let main = k.branch(MAIN).unwrap();
    assert_eq!(
        main.head(LogicalId(0)),
        Some(RevisionId(0)),
        "A stays the target's"
    );
    assert!(report.moved_heads.is_empty());
    assert_eq!(
        report.newly_visible,
        [1, 2, 0, 0],
        "AnchorBroken is not imported"
    );
    let n = main.logical_by_key("code:n.rs#g").unwrap();
    assert_eq!(main.head(n), Some(RevisionId(2)));
    // A's new revision arrives unobserved and not base.
    assert!(!main.is_base(RevisionId(1)).unwrap());
    // N lags until main's observer commits a watermark for "fs".
    let local_n = LogicalId(main.db().logical_count() as u32 - 1);
    assert!(main.db().anchor_unobserved(local_n));
    let live: std::collections::BTreeMap<_, _> = [("fs".to_owned(), vec![1])].into();
    let local_head = main.db().head(local_n).unwrap();
    assert_eq!(
        main.db().summary_at(local_head, &live).unwrap(),
        Summary::Pending(PendingReason::ObservationLag)
    );
    k.commit(&observed(MAIN, vec![watermark(Some(vec![1]), 2)]))
        .unwrap();
    let main = k.branch(MAIN).unwrap();
    assert!(!main.db().anchor_unobserved(local_n));
    let live: std::collections::BTreeMap<_, _> = [("fs".to_owned(), vec![2])].into();
    assert_eq!(
        main.db().summary_at(local_head, &live).unwrap(),
        Summary::Certified
    );
}

/// §9 order inside a merge batch: an `ExpectHeads` error comes before the
/// merge's own when it precedes the Merge, after them when it follows.
#[test]
fn read_set_errors_follow_op_order() {
    let mut k = setup();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&on(b, vec![rule(0, 1, EdgeKind::Pin)])).unwrap();
    k.commit(&Batch::new(vec![rule(1, 0, EdgeKind::Pin)]))
        .unwrap();
    let unknown = Op::ExpectHeads {
        entries: vec![(LogicalRef::Key("missing".into()), RevisionId(0))],
        source: ReadSource::Client,
    };
    assert_eq!(
        k.commit(&Batch::new(vec![unknown.clone(), merge_op(b, false)])),
        Err(TxError::UnknownRef)
    );
    assert!(matches!(
        k.commit(&Batch::new(vec![merge_op(b, false), unknown])),
        Err(TxError::CycleDetected { .. })
    ));
}

/// A merge checks A5 on the merged state (ADR 0013 r0.5): taking a policy
/// that the target's base revisions violate refuses the merge.
#[test]
fn merge_checks_policy_on_the_merged_state() {
    let mut k = setup();
    let b = fork(&mut k, MAIN, "b");
    k.commit(&Batch::new(vec![Op::CreateLogical {
        key: "plan".into(),
        kind: RecordKind::Plan,
        scope_lane: "repo".into(),
        content: [5; 32],
        adopt_as_base: true,
    }]))
    .unwrap();
    let mut policy = on(
        b,
        vec![Op::SetPolicy {
            policy: PolicyDoc {
                version: 1,
                require_deps: [RecordKind::Plan].into_iter().collect(),
            },
        }],
    );
    policy.channel = Channel::Admin;
    k.commit(&policy).unwrap();
    let before = digest(&k, MAIN);
    assert_eq!(
        k.commit(&Batch::new(vec![merge_op(b, false)])),
        Err(TxError::PolicyViolation {
            kind: RecordKind::Plan,
            logical: LogicalId(2),
            reason: PolicyReason::BaseNotAllowed,
        })
    );
    assert_eq!(digest(&k, MAIN), before);
}

/// R1 (retention): S → C at e1, C → T at e2, S reverts, S → C again at e3.
/// Merging S into T still needs S@e1 as its base: T takes S's revert.
#[test]
fn r1_every_sync_point_is_kept() {
    let mut k = setup();
    let s = fork(&mut k, MAIN, "s");
    let c = fork(&mut k, MAIN, "c");
    let t = fork(&mut k, MAIN, "t");
    k.commit(&on(s, move_to(0, 0, 10))).unwrap(); // s1 = r2
    merge(&mut k, s, c); // e1
    merge(&mut k, c, t); // e2
    k.commit(&on(s, vec![set_head(0, 2, RevisionId(0).into())]))
        .unwrap(); // revert to s0
    merge(&mut k, s, c); // e3
    assert_eq!(head(&k, c, 0), Some(RevisionId(0)));
    let (_, report) = merge(&mut k, s, t);
    assert_eq!(head(&k, t, 0), Some(RevisionId(0)), "the revert arrives");
    assert!(report.conflicts.is_empty());
}

/// R3: criss-cross conflicts on `enabled` and on policy. Two maximal bases
/// disagree; the target keeps its value and a Conflict targets the rule's
/// conclusion, while a policy conflict refuses the merge.
#[test]
fn r3_criss_cross_on_enabled_and_policy() {
    let mut k = setup();
    // C = r2 with rule J: C ← A (Pin), enabled.
    let mut c = create("c", 2);
    if let Op::CreateLogical { adopt_as_base, .. } = &mut c {
        *adopt_as_base = false;
    }
    k.commit(&Batch::new(vec![c, rule(2, 0, EdgeKind::Pin)]))
        .unwrap();
    let j = RuleId(0);
    let x = fork(&mut k, MAIN, "x");
    let y = fork(&mut k, MAIN, "y");
    let disable = |enabled| Op::SetRuleEnabled {
        rule: j.into(),
        enabled,
    };
    let policy = |branch, kind| {
        let mut b = on(
            branch,
            vec![Op::SetPolicy {
                policy: PolicyDoc {
                    version: 1,
                    require_deps: [kind].into_iter().collect(),
                },
            }],
        );
        b.channel = Channel::Admin;
        b
    };
    // x disables J and narrows the policy; xs and ys capture both sides.
    k.commit(&on(x, vec![disable(false)])).unwrap();
    k.commit(&policy(x, RecordKind::Plan)).unwrap();
    let xs = fork(&mut k, x, "xs");
    let ys = fork(&mut k, y, "ys");
    merge(&mut k, ys, x);
    merge(&mut k, xs, y); // y takes J disabled and the policy
    // Now x re-enables J: the bases (y's: enabled, x's: disabled) disagree.
    k.commit(&on(x, vec![disable(true)])).unwrap();
    let (_, report) = merge(&mut k, x, y);
    let enabled: Vec<_> = (report.conflicts.iter())
        .filter(|c| c.kind == ConflictKind::Enabled)
        .collect();
    assert_eq!(enabled.len(), 1, "{report:?}");
    assert_eq!(
        (enabled[0].rule, enabled[0].target),
        (Some(j), RevisionId(2))
    );
    assert_eq!(
        k.branch(y).unwrap().rule_enabled(j),
        Some(false),
        "target keeps its value"
    );
    // That merge's sync point now dominates both bases: the next one has a
    // single base again, and x's new policy simply arrives.
    k.commit(&policy(x, RecordKind::Decision)).unwrap();
    k.commit(&on(y, vec![merge_op(x, false)])).unwrap();
}

/// R3 on policy: with two disagreeing maximal bases, a third policy on the
/// source refuses the merge.
#[test]
fn r3_criss_cross_policy_refuses() {
    let mut k = setup();
    let x = fork(&mut k, MAIN, "x");
    let y = fork(&mut k, MAIN, "y");
    let policy = |branch, kind| {
        let mut b = on(
            branch,
            vec![Op::SetPolicy {
                policy: PolicyDoc {
                    version: 1,
                    require_deps: [kind].into_iter().collect(),
                },
            }],
        );
        b.channel = Channel::Admin;
        b
    };
    k.commit(&policy(x, RecordKind::Plan)).unwrap();
    let xs = fork(&mut k, x, "xs");
    let ys = fork(&mut k, y, "ys");
    merge(&mut k, ys, x);
    merge(&mut k, xs, y); // y takes x's policy
    k.commit(&policy(x, RecordKind::Decision)).unwrap();
    let before = digest(&k, y);
    assert_eq!(
        k.commit(&on(y, vec![merge_op(x, false)])),
        Err(TxError::MergeConflict {
            kind: MergeConflictKind::Policy
        })
    );
    assert_eq!(digest(&k, y), before);
}
