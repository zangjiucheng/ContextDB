//! P11 collected payloads (ADR 0019 §5.4; SPEC §6.2 `MissingBlob`, T107):
//! an op that makes a revision live again refuses with `MissingBlob` while
//! GC has collected its content and the bytes were not staged again. Team
//! catch-up is exempt; shredded contents keep P9 semantics; a WHAT_IF given
//! the same presence answers as its commit.
use contextdb_core::Kernel;
use contextdb_model::*;
use std::collections::BTreeSet;

/// The store's answer after GC: every payload present (a WHAT_IF carries
/// none), these hashes collected and not received again.
#[derive(Default)]
struct Gc(BTreeSet<ContentHash>);
impl BlobPresence for Gc {
    fn contains(&self, _: &ContentHash) -> bool {
        true
    }
    fn collected(&self, content: &ContentHash) -> bool {
        self.0.contains(content)
    }
}
fn gc(contents: &[u8]) -> Gc {
    Gc(contents.iter().map(|c| [*c; 32]).collect())
}

const TEAM: [u8; 16] = [0xab; 16];

fn create(key: &str, content: u8) -> Op {
    Op::CreateLogical {
        origin: Default::default(),
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [content; 32],
        adopt_as_base: true,
    }
}
fn rev(n: u32) -> RevRef {
    RevisionId(n).into()
}
fn set_head(l: u32, expected: u32, new: u32) -> Op {
    Op::SetHead {
        logical: LogicalRef::Existing(LogicalId(l)),
        expected: rev(expected),
        new: rev(new),
    }
}
fn rule(conclusion: u32, incidences: &[(u32, EdgeKind)]) -> Op {
    Op::AddRule {
        conclusion: rev(conclusion),
        incidences: incidences.iter().map(|(r, k)| (rev(*r), *k)).collect(),
        enabled: true,
    }
}
fn on(branch: BranchId, channel: Channel, ops: Vec<Op>) -> Batch {
    let mut b = Batch::new(ops);
    b.branch = branch;
    b.channel = channel;
    b
}
fn agent(ops: Vec<Op>) -> Batch {
    Batch::new(ops)
}

/// Main: a = r0 [1] then r3 [2] (head), b = r1 [3], c = r2 [4]; rule j0
/// concludes r1 from r3. GC then collects [1], the content of r0.
fn setup() -> Kernel {
    let mut k = Kernel::default();
    k.commit(&agent(vec![create("a", 1), create("b", 3), create("c", 4)]))
        .unwrap();
    k.commit(&agent(vec![
        Op::AddRevision {
            logical: LogicalRef::Existing(LogicalId(0)),
            content: [2; 32],
            adopt_as_base: true,
        },
        Op::SetHead {
            logical: LogicalRef::Existing(LogicalId(0)),
            expected: rev(0),
            new: RevRef::New(0),
        },
    ]))
    .unwrap();
    k.commit(&agent(vec![rule(1, &[(3, EdgeKind::Sem)])]))
        .unwrap();
    k
}

/// The WHAT_IF and the commit with `presence` give the same result.
fn both(k: &mut Kernel, b: &Batch, presence: &dyn BlobPresence) -> Result<(), TxError> {
    let dry = k.what_if_detailed_with(b, presence).map(|_| ());
    let real = k.commit_with(b, presence).map(|_| ());
    assert_eq!(dry, real, "{b:?}");
    real
}

/// `b` refuses with `MissingBlob{[1]}` while [1] is collected, in the dry
/// run and the commit; once the store has the bytes again it commits.
fn refused_then_admitted(b: Batch) {
    let mut k = setup();
    assert_eq!(
        both(&mut k, &b, &gc(&[1])),
        Err(TxError::MissingBlob { content: [1; 32] }),
        "{b:?}"
    );
    assert_eq!(both(&mut k, &b, &Gc::default()), Ok(()), "{b:?}");
}

#[test]
fn set_head_to_a_collected_revision() {
    // A SetHead-only batch takes the mutable fast path.
    refused_then_admitted(agent(vec![set_head(0, 3, 0)]));
    // With a structural op it is staged.
    refused_then_admitted(agent(vec![create("z", 20), set_head(0, 3, 0)]));
}

#[test]
fn assert_base_of_a_collected_revision() {
    refused_then_admitted(agent(vec![Op::AssertBase { rev: rev(0) }]));
    refused_then_admitted(agent(vec![create("z", 20), Op::AssertBase { rev: rev(0) }]));
}

#[test]
fn add_rule_naming_a_collected_revision() {
    // As conclusion, premise and reference.
    refused_then_admitted(agent(vec![rule(0, &[(2, EdgeKind::Sem)])]));
    refused_then_admitted(agent(vec![rule(2, &[(0, EdgeKind::Pin)])]));
    refused_then_admitted(agent(vec![rule(
        2,
        &[(1, EdgeKind::Sem), (0, EdgeKind::Ref)],
    )]));
}

#[test]
fn rebind_to_a_collected_revision() {
    refused_then_admitted(agent(vec![Op::Rebind {
        rule: RuleId(0).into(),
        from: rev(3),
        to: rev(0),
    }]));
}

/// Ops that make no revision live again ignore collection: retracting a
/// base, a rule on live revisions, and the old entry points (AllPresent,
/// as replay uses) admit everything.
#[test]
fn other_ops_and_all_present_ignore_collection() {
    let mut k = setup();
    let collected = gc(&[1]);
    for b in [
        agent(vec![Op::RetractBase { rev: rev(0) }]),
        agent(vec![rule(2, &[(1, EdgeKind::Sem)])]),
    ] {
        assert_eq!(both(&mut k.clone(), &b, &collected), Ok(()), "{b:?}");
    }
    let b = agent(vec![set_head(0, 3, 0)]);
    assert!(k.what_if(&b).is_ok());
    assert!(k.what_if_detailed(&b).is_ok());
    assert!(k.commit(&b).is_ok());
}

/// A batch that carries the content again (`AddRevision` deduplicating to
/// the old revision, as `ctx_supersede` sends it) brings the bytes with the
/// commit: the WHAT_IF, which carries none, admits it as the commit does.
#[test]
fn a_batch_carrying_the_bytes_again_is_admitted() {
    let mut k = setup();
    let b = agent(vec![
        Op::AddRevision {
            logical: LogicalRef::Existing(LogicalId(0)),
            content: [1; 32],
            adopt_as_base: false,
        },
        set_head(0, 3, 0),
    ]);
    let dry = k.what_if_detailed_with(&b, &gc(&[1])).map(|_| ());
    // The store wrote the commit's bytes first: no longer collected.
    let real = k.commit_with(&b, &Gc::default()).map(|_| ());
    assert_eq!((dry, real), (Ok(()), Ok(())));
    assert_eq!(
        k.branch(MAIN).unwrap().head(LogicalId(0)),
        Some(RevisionId(0))
    );
}

/// P9 semantics for a shredded content: these ops never checked it, and
/// collection does not change that.
#[test]
fn shredded_content_is_unaffected() {
    let mut k = setup();
    k.commit(&on(
        MAIN,
        Channel::Admin,
        vec![Op::ShredContent { content: [1; 32] }],
    ))
    .unwrap();
    let collected = gc(&[1]);
    for b in [
        agent(vec![set_head(0, 3, 0)]),
        agent(vec![create("z", 20), Op::AssertBase { rev: rev(0) }]),
        agent(vec![rule(2, &[(0, EdgeKind::Sem)])]),
    ] {
        assert_eq!(both(&mut k.clone(), &b, &collected), Ok(()), "{b:?}");
    }
}

fn mirror_key(path: &str) -> String {
    format!("team/{}/{path}", "ab".repeat(16))
}
fn mirror(path: &str, content: u8) -> Op {
    Op::CreateLogical {
        key: mirror_key(path),
        kind: RecordKind::Decision,
        scope_lane: format!("team:{}/main", "ab".repeat(16)),
        content: [content; 32],
        adopt_as_base: false,
        origin: Origin::Replica { team: TEAM },
    }
}

/// Team catch-up is never refused for local content: the replica channel's
/// SetHead, AssertBase and MirrorAttach of a collected revision are
/// admitted (ADR 0019 §5.4). `MirrorAttach` runs only on that channel, so
/// it is always exempt.
#[test]
fn team_catch_up_is_exempt() {
    let mut k = setup();
    k.commit(&on(
        MAIN,
        Channel::Admin,
        vec![Op::CreateBranch { name: "f".into() }],
    ))
    .unwrap();
    let f = k.branch_by_name("f").unwrap();
    // Mirror m on main: r4 [7], then r5 [8] as head.
    k.commit(&on(MAIN, Channel::Replica, vec![mirror("m", 7)]))
        .unwrap();
    k.commit(&on(
        MAIN,
        Channel::Replica,
        vec![
            Op::AddRevision {
                logical: LogicalRef::Existing(LogicalId(3)),
                content: [8; 32],
                adopt_as_base: false,
            },
            Op::SetHead {
                logical: LogicalRef::Existing(LogicalId(3)),
                expected: rev(4),
                new: RevRef::New(0),
            },
        ],
    ))
    .unwrap();
    let collected = gc(&[7, 8]);
    for ops in [
        vec![set_head(3, 5, 4)],
        vec![Op::AssertBase { rev: rev(4) }],
        vec![mirror("n", 9), set_head(3, 5, 4)],
    ] {
        let b = on(MAIN, Channel::Replica, ops);
        assert_eq!(both(&mut k.clone(), &b, &collected), Ok(()), "{b:?}");
    }
    // The fork never saw m: attach its existing head revision.
    let attach = on(
        f,
        Channel::Replica,
        vec![Op::MirrorAttach {
            key: mirror_key("m"),
            head: [8; 32],
            base: false,
        }],
    );
    assert_eq!(both(&mut k, &attach, &collected), Ok(()));
}

fn merge(source: BranchId) -> Batch {
    agent(vec![Op::Merge {
        source,
        require_certified: false,
        certify: Vec::new(),
    }])
}
fn fork(k: &mut Kernel, name: &str, ops: Vec<Op>) -> BranchId {
    k.commit(&agent(vec![Op::CreateBranch { name: name.into() }]))
        .unwrap();
    let b = k.branch_by_name(name).unwrap();
    k.commit(&on(b, Channel::Agent, ops)).unwrap();
    b
}

/// A merge that would make a collected revision a head or a rule member on
/// the target refuses; one that does not merges. The dry run agrees.
#[test]
fn merges_that_revive_a_collected_revision() {
    let mut k = setup();
    // Changed on the forks before GC collected [1].
    let head = fork(&mut k, "head", vec![set_head(0, 3, 0)]);
    let member = fork(&mut k, "member", vec![rule(2, &[(0, EdgeKind::Pin)])]);
    // A base bit taken from the source makes [1] (revision 0) live, as
    // AssertBase does: retracted on main, asserted on the fork.
    k.commit(&agent(vec![Op::RetractBase { rev: rev(0) }]))
        .unwrap();
    let based = fork(&mut k, "based", vec![Op::AssertBase { rev: rev(0) }]);
    let unrelated = fork(&mut k, "unrelated", vec![create("x", 9)]);
    let collected = gc(&[1]);
    for source in [head, member, based] {
        let b = merge(source);
        assert_eq!(
            both(&mut k.clone(), &b, &collected),
            Err(TxError::MissingBlob { content: [1; 32] }),
            "{b:?}"
        );
        assert_eq!(both(&mut k.clone(), &b, &Gc::default()), Ok(()));
    }
    assert_eq!(both(&mut k, &merge(unrelated), &collected), Ok(()));
}

type Outcome = Result<(), TxError>;
/// §6.2 order: `UnknownRef → ChannelRestricted → KindRestricted →
/// ImmutableKind → InvalidKey → InvalidPolicy → MissingBlob →
/// InvalidMetadata → LogicalMismatch → InvalidRebind → InvalidRule → … →
/// CAS class`, and a batch returns its first failing op's first failing
/// check.
#[test]
fn missing_blob_takes_its_place_in_the_check_order() {
    let mut k = setup();
    k.commit(&agent(vec![Op::CreateLogical {
        origin: Default::default(),
        key: "e".into(),
        kind: RecordKind::Evidence,
        scope_lane: "repo".into(),
        content: [10; 32],
        adopt_as_base: true,
    }]))
    .unwrap(); // l3 = r4
    let collected = gc(&[1]);
    let missing = Err(TxError::MissingBlob { content: [1; 32] });
    let cases: Vec<(Batch, Outcome, Outcome)> = vec![
        // Earlier checks win.
        (
            agent(vec![rule(0, &[(99, EdgeKind::Sem)])]),
            Err(TxError::UnknownRef),
            Err(TxError::UnknownRef),
        ),
        (
            on(MAIN, Channel::Observer, vec![set_head(0, 3, 0)]),
            Err(TxError::ChannelRestricted {
                op: "SetHead".into(),
                channel: Channel::Observer,
            }),
            Err(TxError::ChannelRestricted {
                op: "SetHead".into(),
                channel: Channel::Observer,
            }),
        ),
        (
            agent(vec![set_head(3, 4, 0)]),
            Err(TxError::ImmutableKind),
            Err(TxError::ImmutableKind),
        ),
        (
            agent(vec![create("Bad", 20), set_head(0, 3, 0)]),
            Err(TxError::InvalidKey { key: "Bad".into() }),
            Err(TxError::InvalidKey { key: "Bad".into() }),
        ),
        // MissingBlob wins over the later ones.
        (
            agent(vec![set_head(1, 1, 0)]),
            missing.clone(),
            Err(TxError::LogicalMismatch),
        ),
        (
            agent(vec![create("z", 20), set_head(1, 1, 0)]),
            missing.clone(),
            Err(TxError::LogicalMismatch),
        ),
        (
            agent(vec![Op::Rebind {
                rule: RuleId(0).into(),
                from: rev(2),
                to: rev(0),
            }]),
            missing.clone(),
            Err(TxError::LogicalMismatch),
        ),
        (
            agent(vec![rule(0, &[(0, EdgeKind::Sem)])]),
            missing.clone(),
            Err(TxError::InvalidRule(InvalidRule::SelfPremise)),
        ),
        (
            agent(vec![set_head(0, 0, 0)]),
            missing.clone(),
            Err(TxError::HeadConflict {
                logical: LogicalId(0),
                expected: RevisionId(0),
                actual: RevisionId(3),
            }),
        ),
        // The first failing op's error.
        (
            agent(vec![set_head(0, 3, 0), rule(2, &[(2, EdgeKind::Sem)])]),
            missing.clone(),
            Err(TxError::InvalidRule(InvalidRule::SelfPremise)),
        ),
    ];
    for (b, with, without) in cases {
        assert_eq!(both(&mut k.clone(), &b, &collected), with, "{b:?}");
        assert_eq!(both(&mut k.clone(), &b, &Gc::default()), without, "{b:?}");
    }
}
