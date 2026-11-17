//! P10 merge protection (SD19 team supplement; T120, T121): a merge never
//! carries replica-origin state or makes a mirror visible, and a merge that
//! would carry a rule or validation naming a mirror revision the target
//! cannot see is `MirrorLag`. Kernel and `BranchOracle` agree on every
//! outcome and every branch's canonical state.

use contextdb_core::Kernel;
use contextdb_model::*;
use contextdb_oracle::{BranchOracle, Canonical, canonical};

const TEAM: [u8; 16] = [0xab; 16];

fn key(path: &str) -> String {
    format!("team/{}/{path}", "ab".repeat(16))
}

fn kernel_canonical(k: &Kernel, b: BranchId) -> Canonical {
    let v = k.branch(b).unwrap();
    let db = v.db();
    let gl = |x: u32| v.global_logical(LogicalId(x)).0;
    let gr = |x: u32| v.global_revision(RevisionId(x)).0;
    let gj = |x: u32| v.global_rule(RuleId(x)).0;
    let ga = |x: u32| v.global_artifact(ArtifactId(x)).0;
    let unobserved = (0..db.logical_count() as u32)
        .filter(|l| db.anchor_unobserved(LogicalId(*l)))
        .map(gl)
        .collect();
    canonical(
        &db.inputs(),
        DerivedView::derived(db),
        [&gl, &gr, &gj, &ga],
        unobserved,
    )
}

struct Both {
    k: Kernel,
    o: BranchOracle,
}

impl Both {
    fn new() -> Self {
        let mut s = Self {
            k: Kernel::default(),
            o: BranchOracle::new(|b| *blake3::hash(b).as_bytes()),
        };
        s.ok(on(
            MAIN,
            Channel::Admin,
            vec![Op::CreateBranch { name: "b".into() }],
        ));
        s
    }
    fn run(&mut self, b: Batch) -> Result<CommitResult, TxError> {
        let k = self.k.commit(&b);
        let o = self.o.apply(&b, true);
        match (&k, &o) {
            (Ok(_), Ok(_)) => {}
            (Err(x), Err(y)) => assert_eq!(x, y, "{b:?}"),
            _ => panic!("kernel {k:?} vs oracle {o:?} for {b:?}"),
        }
        for n in 0..self.o.branch_count() {
            let br = BranchId(n as u16);
            assert_eq!(
                kernel_canonical(&self.k, br),
                self.o.canonical(br),
                "branch {n} after {b:?}"
            );
        }
        k
    }
    fn ok(&mut self, b: Batch) -> CommitResult {
        self.run(b).expect("committed")
    }
    fn err(&mut self, b: Batch) -> TxError {
        self.run(b).expect_err("refused")
    }
    /// A global revision id by its logical key and content, on branch `b`.
    fn rev(&self, b: BranchId, k: &str, content: ContentHash) -> RevisionId {
        let v = self.k.branch(b).unwrap();
        let db = v.db();
        let l = db.logical_by_key(k).unwrap();
        let r = (0..db.revision_count() as u32)
            .map(RevisionId)
            .find(|r| {
                db.revision_logical(*r) == Some(l) && db.revision_content(*r).unwrap() == content
            })
            .unwrap();
        v.global_revision(r)
    }
    fn logical(&self, b: BranchId, k: &str) -> LogicalId {
        let v = self.k.branch(b).unwrap();
        v.global_logical(v.db().logical_by_key(k).unwrap())
    }
}

fn on(branch: BranchId, channel: Channel, ops: Vec<Op>) -> Batch {
    let mut b = Batch::new(ops);
    b.branch = branch;
    b.channel = channel;
    b
}

fn mirror(path: &str, content: ContentHash) -> Op {
    Op::CreateLogical {
        key: key(path),
        kind: RecordKind::Decision,
        scope_lane: format!("team:{}/main", "ab".repeat(16)),
        content,
        adopt_as_base: true,
        origin: Origin::Replica { team: TEAM },
    }
}

fn local(k: &str, content: ContentHash) -> Op {
    Op::CreateLogical {
        key: k.into(),
        kind: RecordKind::Decision,
        scope_lane: "repo".into(),
        content,
        adopt_as_base: false,
        origin: Origin::Local,
    }
}

fn merge(source: BranchId) -> Op {
    Op::Merge {
        source,
        require_certified: false,
        certify: Vec::new(),
    }
}

const B: BranchId = BranchId(1);

#[test]
fn a_carried_rule_on_an_invisible_mirror_is_mirror_lag_until_catch_up() {
    for edge in [EdgeKind::Pin, EdgeKind::Cur, EdgeKind::Ref, EdgeKind::Sem] {
        let mut s = Both::new();
        // The mirror exists on b only (main has not caught up).
        s.ok(on(B, Channel::Replica, vec![mirror("arch/a", [1; 32])]));
        s.ok(on(B, Channel::Agent, vec![local("dec:x", [2; 32])]));
        let (x, a) = (
            s.rev(B, "dec:x", [2; 32]),
            s.rev(B, &key("arch/a"), [1; 32]),
        );
        // A Ref-only rule carries the reference too.
        s.ok(on(
            B,
            Channel::Agent,
            vec![Op::AddRule {
                conclusion: x.into(),
                incidences: vec![(a.into(), edge)],
                enabled: true,
            }],
        ));
        assert_eq!(
            s.err(on(MAIN, Channel::Agent, vec![merge(B)])),
            TxError::MirrorLag,
            "{edge:?}"
        );
        // Catch main up (attach), and the merge goes through without
        // touching the mirror.
        s.ok(on(
            MAIN,
            Channel::Replica,
            vec![Op::MirrorAttach {
                key: key("arch/a"),
                head: [1; 32],
                base: true,
            }],
        ));
        s.ok(on(MAIN, Channel::Agent, vec![merge(B)]));
    }
}

#[test]
fn a_merge_carries_no_mirror_state_and_no_visibility() {
    let mut s = Both::new();
    s.ok(on(MAIN, Channel::Replica, vec![mirror("arch/a", [1; 32])]));
    s.ok(on(
        B,
        Channel::Replica,
        vec![Op::MirrorAttach {
            key: key("arch/a"),
            head: [1; 32],
            base: true,
        }],
    ));
    // On b: a new head, a base flip, tags; and a mirror only b has.
    s.ok(on(
        B,
        Channel::Replica,
        vec![
            Op::AddRevision {
                logical: LogicalRef::Key(key("arch/a")),
                content: [3; 32],
                adopt_as_base: true,
            },
            Op::SetMetadata {
                logical: LogicalRef::Key(key("arch/a")),
                tags: ["t".into()].into(),
                dirs: Default::default(),
            },
            mirror("arch/only-b", [4; 32]),
        ],
    ));
    let (l, h1, h3) = (
        s.logical(B, &key("arch/a")),
        s.rev(B, &key("arch/a"), [1; 32]),
        s.rev(B, &key("arch/a"), [3; 32]),
    );
    s.ok(on(
        B,
        Channel::Replica,
        vec![Op::SetHead {
            logical: l.into(),
            expected: h1.into(),
            new: h3.into(),
        }],
    ));
    s.ok(on(
        B,
        Channel::Replica,
        vec![Op::RetractBase { rev: h1.into() }],
    ));
    // Plus an ordinary local change, which the merge does carry.
    s.ok(on(B, Channel::Agent, vec![local("dec:y", [5; 32])]));
    let before = kernel_canonical(&s.k, MAIN);
    s.ok(on(MAIN, Channel::Agent, vec![merge(B)]));
    let after = kernel_canonical(&s.k, MAIN);
    let main = s.k.branch(MAIN).unwrap();
    let db = main.db();
    assert!(
        db.logical_by_key(&key("arch/only-b")).is_none(),
        "a merge never makes a mirror visible"
    );
    assert!(db.logical_by_key("dec:y").is_some());
    let ml = db.logical_by_key(&key("arch/a")).unwrap();
    assert_eq!(
        db.revision_content(db.head(ml).unwrap()).unwrap(),
        [1; 32],
        "the mirror's head is main's own"
    );
    assert_ne!(before, after);
}

#[test]
fn a_taken_validation_naming_an_invisible_mirror_revision_is_mirror_lag() {
    let mut s = Both::new();
    // Mirror on both branches at r1; a Sem rule on it on main, visible on b
    // by fork order: create everything on main first, then fork c.
    s.ok(on(MAIN, Channel::Replica, vec![mirror("arch/a", [1; 32])]));
    s.ok(on(MAIN, Channel::Agent, vec![local("dec:x", [2; 32])]));
    let (x, a1) = (
        s.rev(MAIN, "dec:x", [2; 32]),
        s.rev(MAIN, &key("arch/a"), [1; 32]),
    );
    s.ok(on(
        MAIN,
        Channel::Agent,
        vec![Op::AddRule {
            conclusion: x.into(),
            incidences: vec![(a1.into(), EdgeKind::Sem)],
            enabled: true,
        }],
    ));
    s.ok(on(
        MAIN,
        Channel::Admin,
        vec![Op::CreateBranch { name: "c".into() }],
    ));
    let c = BranchId(2);
    // On c the root moved the mirror to r3 (not on main); a validation
    // records the rule against it.
    s.ok(on(
        c,
        Channel::Replica,
        vec![Op::AddRevision {
            logical: LogicalRef::Key(key("arch/a")),
            content: [3; 32],
            adopt_as_base: true,
        }],
    ));
    let (l, a3) = (
        s.logical(c, &key("arch/a")),
        s.rev(c, &key("arch/a"), [3; 32]),
    );
    s.ok(on(
        c,
        Channel::Replica,
        vec![Op::SetHead {
            logical: l.into(),
            expected: a1.into(),
            new: a3.into(),
        }],
    ));
    let j = {
        let v = s.k.branch(c).unwrap();
        v.global_rule(RuleId(0))
    };
    s.ok(on(
        c,
        Channel::Agent,
        vec![Op::RecordValidation {
            rule: j.into(),
            curvec_seen: vec![(l, a3)],
            verdict: Verdict::Verified,
            validator: "v".into(),
            meta: None,
            if_absent: false,
        }],
    ));
    assert_eq!(
        s.err(on(MAIN, Channel::Agent, vec![merge(c)])),
        TxError::MirrorLag
    );
}
