//! P10 MirrorAttach (SPEC §6.1 contract 16, SD21.8; T120): a mirror created
//! on one branch becomes visible on another, with its head, by the replica
//! channel only. Every step runs on the kernel and the independent
//! `BranchOracle`; outcomes and every branch's canonical state must agree.

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
        Self {
            k: Kernel::default(),
            o: BranchOracle::new(|b| *blake3::hash(b).as_bytes()),
        }
    }
    fn check(&self) {
        for n in 0..self.o.branch_count() {
            let b = BranchId(n as u16);
            assert_eq!(
                kernel_canonical(&self.k, b),
                self.o.canonical(b),
                "branch {n}"
            );
        }
    }
    fn run(&mut self, b: Batch) -> Result<CommitResult, TxError> {
        let k = self.k.commit(&b);
        let o = self.o.apply(&b, true);
        match (&k, &o) {
            (Ok(_), Ok(_)) => {}
            (Err(x), Err(y)) => assert_eq!(x, y, "{b:?}"),
            _ => panic!("kernel {k:?} vs oracle {o:?} for {b:?}"),
        }
        self.check();
        k
    }
    fn ok(&mut self, b: Batch) -> CommitResult {
        self.run(b).expect("committed")
    }
    fn err(&mut self, b: Batch) -> TxError {
        self.run(b).expect_err("refused")
    }
}

fn on(branch: BranchId, channel: Channel, ops: Vec<Op>) -> Batch {
    let mut b = Batch::new(ops);
    b.branch = branch;
    b.channel = channel;
    b
}

fn create(path: &str, content: ContentHash, base: bool) -> Op {
    Op::CreateLogical {
        key: key(path),
        kind: RecordKind::Decision,
        scope_lane: format!("team:{}/main", "ab".repeat(16)),
        content,
        adopt_as_base: base,
        origin: Origin::Replica { team: TEAM },
    }
}

fn attach(path: &str, head: ContentHash, base: bool) -> Op {
    Op::MirrorAttach {
        key: key(path),
        head,
        base,
    }
}

#[test]
fn a_branch_forked_before_a_record_gets_it_by_attach() {
    let mut s = Both::new();
    s.ok(on(
        MAIN,
        Channel::Admin,
        vec![Op::CreateBranch { name: "b".into() }],
    ));
    let b = BranchId(1);
    // The root's record appears on main, with a second revision.
    s.ok(on(
        MAIN,
        Channel::Replica,
        vec![create("arch/a", [1; 32], false)],
    ));
    s.ok(on(
        MAIN,
        Channel::Replica,
        vec![Op::AddRevision {
            logical: LogicalRef::Key(key("arch/a")),
            content: [2; 32],
            adopt_as_base: true,
        }],
    ));
    // b gets it by attach, at main's head content, with base.
    s.ok(on(
        b,
        Channel::Replica,
        vec![attach("arch/a", [1; 32], false)],
    ));
    // Its other revision arrives by replica AddRevision (a dedup import),
    // then the head moves and the base follows, on b only.
    s.ok(on(
        b,
        Channel::Replica,
        vec![Op::AddRevision {
            logical: LogicalRef::Key(key("arch/a")),
            content: [2; 32],
            adopt_as_base: true,
        }],
    ));
    let v = s.k.branch(b).unwrap();
    let l = v.db().logical_by_key(&key("arch/a")).unwrap();
    let heads = v.db().head(l).unwrap();
    let r2 = (0..v.db().revision_count() as u32)
        .map(RevisionId)
        .find(|r| v.db().revision_content(*r).unwrap() == [2; 32])
        .unwrap();
    let (gl, gh, g2) = (
        v.global_logical(l),
        v.global_revision(heads),
        v.global_revision(r2),
    );
    s.ok(on(
        b,
        Channel::Replica,
        vec![Op::SetHead {
            logical: gl.into(),
            expected: gh.into(),
            new: g2.into(),
        }],
    ));
    // A local rule on b may depend on the attached mirror.
    let local = s.ok(on(
        b,
        Channel::Agent,
        vec![Op::CreateLogical {
            key: "dec:local".into(),
            kind: RecordKind::Decision,
            scope_lane: "repo".into(),
            content: [5; 32],
            adopt_as_base: false,
            origin: Origin::Local,
        }],
    ));
    s.ok(on(
        b,
        Channel::Agent,
        vec![Op::AddRule {
            conclusion: local.allocations.new_revisions[0].into(),
            incidences: vec![(g2.into(), EdgeKind::Pin)],
            enabled: true,
        }],
    ));
}

#[test]
fn attach_errors() {
    let mut s = Both::new();
    s.ok(on(
        MAIN,
        Channel::Admin,
        vec![Op::CreateBranch { name: "b".into() }],
    ));
    let b = BranchId(1);
    s.ok(on(
        MAIN,
        Channel::Replica,
        vec![create("arch/a", [1; 32], true)],
    ));
    // No such mirror: UnknownRef.
    assert_eq!(
        s.err(on(
            b,
            Channel::Replica,
            vec![attach("arch/none", [1; 32], true)]
        )),
        TxError::UnknownRef
    );
    // A head no branch holds yet is created by the attach (a branch caught
    // up ahead of the others), on that branch only.
    s.ok(on(
        b,
        Channel::Admin,
        vec![Op::CreateBranch { name: "c".into() }],
    ));
    let c = BranchId(2);
    s.ok(on(
        c,
        Channel::Replica,
        vec![attach("arch/a", [9; 32], true)],
    ));
    {
        let v = s.k.branch(c).unwrap();
        let db = v.db();
        let l = db.logical_by_key(&key("arch/a")).unwrap();
        assert_eq!(db.revision_content(db.head(l).unwrap()).unwrap(), [9; 32]);
        let main = s.k.branch(MAIN).unwrap();
        let ml = main.db().logical_by_key(&key("arch/a")).unwrap();
        assert_eq!(
            main.db()
                .revision_content(main.db().head(ml).unwrap())
                .unwrap(),
            [1; 32]
        );
    }
    // Already visible here: LogicalExists.
    assert_eq!(
        s.err(on(
            MAIN,
            Channel::Replica,
            vec![attach("arch/a", [1; 32], true)]
        )),
        TxError::LogicalExists { key: key("arch/a") }
    );
    // Only the replica channel; an agent learns nothing about keys.
    for c in [Channel::Agent, Channel::Admin] {
        assert_eq!(
            s.err(on(b, c, vec![attach("arch/a", [1; 32], true)])),
            TxError::ReplicaReadOnly
        );
        assert_eq!(
            s.err(on(b, c, vec![attach("arch/none", [1; 32], true)])),
            TxError::ReplicaReadOnly
        );
    }
    // A local logical is never attached, even by key.
    s.ok(on(
        MAIN,
        Channel::Agent,
        vec![Op::CreateLogical {
            key: "dec:x".into(),
            kind: RecordKind::Decision,
            scope_lane: "repo".into(),
            content: [3; 32],
            adopt_as_base: false,
            origin: Origin::Local,
        }],
    ));
    assert_eq!(
        s.err(on(
            b,
            Channel::Replica,
            vec![Op::MirrorAttach {
                key: "dec:x".into(),
                head: [3; 32],
                base: false
            }]
        )),
        TxError::UnknownRef
    );
    // Attach twice in one batch: the second sees the first.
    assert_eq!(
        s.err(on(
            b,
            Channel::Replica,
            vec![
                attach("arch/a", [1; 32], true),
                attach("arch/a", [1; 32], true)
            ]
        )),
        TxError::LogicalExists { key: key("arch/a") }
    );
    s.ok(on(
        b,
        Channel::Replica,
        vec![attach("arch/a", [1; 32], true)],
    ));
}

#[test]
fn attach_and_create_in_one_batch() {
    let mut s = Both::new();
    s.ok(on(
        MAIN,
        Channel::Admin,
        vec![Op::CreateBranch { name: "b".into() }],
    ));
    let b = BranchId(1);
    s.ok(on(
        MAIN,
        Channel::Replica,
        vec![create("arch/a", [1; 32], true)],
    ));
    // Interleaved attach and create, then writes by key and by New(k).
    s.ok(on(
        b,
        Channel::Replica,
        vec![
            create("arch/new", [7; 32], false),
            attach("arch/a", [1; 32], true),
            Op::AddRevision {
                logical: LogicalRef::Key(key("arch/a")),
                content: [8; 32],
                adopt_as_base: false,
            },
            Op::AddRevision {
                logical: LogicalRef::New(0),
                content: [6; 32],
                adopt_as_base: false,
            },
            Op::SetMetadata {
                logical: LogicalRef::Key(key("arch/new")),
                tags: ["t".into()].into(),
                dirs: Default::default(),
            },
        ],
    ));
    // The new mirror can now be attached on main.
    s.ok(on(
        MAIN,
        Channel::Replica,
        vec![attach("arch/new", [7; 32], false)],
    ));
}

#[test]
fn what_if_and_snapshot_round_trip() {
    let mut s = Both::new();
    s.ok(on(
        MAIN,
        Channel::Admin,
        vec![Op::CreateBranch { name: "b".into() }],
    ));
    let b = BranchId(1);
    s.ok(on(
        MAIN,
        Channel::Replica,
        vec![create("arch/a", [1; 32], true)],
    ));
    // WHAT_IF promises the attach and changes nothing.
    let before = kernel_canonical(&s.k, b);
    s.k.what_if(&on(
        b,
        Channel::Replica,
        vec![attach("arch/a", [1; 32], true)],
    ))
    .unwrap();
    assert_eq!(kernel_canonical(&s.k, b), before);
    // A kernel exported and imported can still attach.
    let mut k2 = Kernel::import(s.k.export()).unwrap();
    k2.commit(&on(
        b,
        Channel::Replica,
        vec![attach("arch/a", [1; 32], true)],
    ))
    .unwrap();
    s.ok(on(
        b,
        Channel::Replica,
        vec![attach("arch/a", [1; 32], true)],
    ));
    assert_eq!(kernel_canonical(&k2, b), kernel_canonical(&s.k, b));
    let k3 = Kernel::import(k2.export()).unwrap();
    assert_eq!(kernel_canonical(&k3, b), kernel_canonical(&s.k, b));
}
