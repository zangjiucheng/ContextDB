//! `accept` on signed in-process chains (ADR 0018 §4, SPEC SD21.2–6):
//! genesis, roles, expected values, per-proposal decisions, positional
//! sequence numbers after rejected entries, team status (Pin-strict,
//! reference-only rules), the state limit and digest agreement.

use contextdb_model::RecordKind;
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::ops::{Edge, Hash, RuleSpec, TeamBatch, TeamOp, tags_hash};
use contextdb_team::replica::{EntryInput, Outcome, Replica};
use contextdb_team::sshsig;

const TEAM: [u8; 16] = [0x5f; 16];
const ALICE: [u8; 32] = [1; 32];
const BOB: [u8; 32] = [2; 32];
const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn doc(state_bytes: Option<u64>) -> TeamDoc {
    let limits = state_bytes.map_or(String::new(), |n| {
        format!("\n[limits]\nstate_bytes = {n}\n")
    });
    TeamDoc::from_toml(&format!(
        r#"version = 1
accept_version = 1
id = "{}"
admins = ["alice"]
[members.alice]
keys = ["{}"]
[members.bob]
keys = ["{}"]
[lanes.conventions]
writers = ["*members"]
curators = ["alice"]
{limits}"#,
        hex(&TEAM),
        sshsig::public_key(&ALICE).to_line(),
        sshsig::public_key(&BOB).to_line(),
    ))
    .unwrap()
}

/// A chain under construction: positions, parents and signatures are real.
struct Chain {
    seq: u64,
    parent: Option<String>,
    next_id: u8,
}

struct Entry {
    seq: u64,
    first: bool,
    raw: Vec<u8>,
    tree: Vec<(String, Vec<u8>)>,
}

impl Entry {
    fn input(&self) -> EntryInput<'_> {
        EntryInput {
            seq: self.seq,
            first_in_generation: self.first,
            commit: &self.raw,
            tree: &self.tree,
        }
    }
}

fn blob(bytes: &[u8]) -> (Hash, (String, Vec<u8>)) {
    let h = *blake3::hash(bytes).as_bytes();
    (h, (format!("blobs/{}", hex(&h)), bytes.to_vec()))
}

impl Chain {
    fn new() -> Self {
        Self {
            seq: 0,
            parent: None,
            next_id: 1,
        }
    }

    /// Builds the next entry; `tweak` may edit the trailers before signing.
    fn entry_with(
        &mut self,
        signer: &[u8; 32],
        author: &str,
        proposals: Vec<Vec<TeamOp>>,
        blobs: Vec<(String, Vec<u8>)>,
        tweak: impl FnOnce(&mut Vec<(&'static str, String)>),
    ) -> Entry {
        let batch = TeamBatch { proposals }.encode();
        let mut trailers: Vec<(&'static str, String)> = vec![
            ("Ctx-Team", hex(&TEAM)),
            ("Ctx-Team-Seq", self.seq.to_string()),
            ("Ctx-Author", author.into()),
            ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
            ("Ctx-Accept-Version", "1".into()),
        ];
        let n = TeamBatch::decode(&batch).unwrap().proposals.len();
        for _ in 0..n {
            trailers.push(("Ctx-Proposal", hex(&[self.next_id; 16])));
            self.next_id += 1;
        }
        tweak(&mut trailers);
        let parents: Vec<String> = self.parent.iter().cloned().collect();
        let unsigned = commit::build(TREE, &parents, "ctx team: entry", &trailers);
        let raw = commit::with_signature(
            &unsigned,
            &sshsig::sign(signer, "git", &unsigned),
            ObjectFormat::Sha1,
        );
        let oid = hex(&blake3::hash(&raw).as_bytes()[..20]);
        let mut tree = vec![("batch".to_owned(), batch)];
        tree.extend(blobs);
        let e = Entry {
            seq: self.seq,
            first: self.parent.is_none(),
            raw,
            tree,
        };
        self.parent = Some(oid);
        self.seq += 1;
        e
    }

    fn entry(
        &mut self,
        signer: &[u8; 32],
        author: &str,
        proposals: Vec<Vec<TeamOp>>,
        blobs: Vec<(String, Vec<u8>)>,
    ) -> Entry {
        self.entry_with(signer, author, proposals, blobs, |_| {})
    }
}

fn new_op(path: &str, content: Hash, base: bool) -> TeamOp {
    TeamOp::New {
        path: path.into(),
        lane: "conventions".into(),
        kind: RecordKind::Convention,
        content,
        adopt_as_base: base,
    }
}

fn genesis(c: &mut Chain, r: &mut Replica, state_bytes: Option<u64>) {
    let g = c.entry(
        &ALICE,
        "alice",
        vec![vec![TeamOp::SetTeamAccess {
            doc: Box::new(doc(state_bytes)),
        }]],
        vec![],
    );
    assert_eq!(r.accept(&g.input()), Outcome::Decided(vec![Ok(())]));
}

fn decided(o: Outcome) -> Vec<Result<(), &'static str>> {
    match o {
        Outcome::Decided(v) => v.into_iter().map(|d| d.map_err(|r| r.code)).collect(),
        other => panic!("not decided: {other:?}"),
    }
}

#[test]
fn roles_expected_values_and_team_status() {
    let (mut c, mut r) = (Chain::new(), Replica::new(TEAM, ObjectFormat::Sha1));
    genesis(&mut c, &mut r, None);
    let (a1, b_a1) = blob(b"naming: snake_case");
    let (b1, b_b1) = blob(b"modules: one per feature");
    // Bob may write but not curate: adopt_as_base is his to ask, not to do.
    let e = c.entry(
        &BOB,
        "bob",
        vec![
            vec![new_op("conv/naming", a1, true)],
            vec![new_op("conv/naming", a1, false)],
        ],
        vec![b_a1.clone()],
    );
    assert_eq!(
        decided(r.accept(&e.input())),
        vec![Err("Unauthorized(Curate)"), Ok(())]
    );
    assert_eq!(r.certified("conv/naming"), Some(false));
    // Alice asserts base with the expected bit; a stale expectation fails.
    let e = c.entry(
        &ALICE,
        "alice",
        vec![
            vec![TeamOp::SetBase {
                path: "conv/naming".into(),
                content: a1,
                expected: true,
                base: true,
            }],
            vec![TeamOp::SetBase {
                path: "conv/naming".into(),
                content: a1,
                expected: false,
                base: true,
            }],
        ],
        vec![],
    );
    assert_eq!(
        decided(r.accept(&e.input())),
        vec![Err("BaseConflict"), Ok(())]
    );
    assert_eq!(r.certified("conv/naming"), Some(true));
    // B depends on A by Pin; a reference-only rule supports nothing.
    let pin = RuleSpec {
        conclusion: ("conv/modules".into(), b1),
        incidences: vec![("conv/naming".into(), Edge::Pin(a1))],
    };
    let refonly = RuleSpec {
        conclusion: ("conv/other".into(), b1),
        incidences: vec![("conv/naming".into(), Edge::Ref(a1))],
    };
    let e = c.entry(
        &BOB,
        "bob",
        vec![
            vec![
                new_op("conv/modules", b1, false),
                TeamOp::AddRule {
                    rule: pin.clone(),
                    enabled: true,
                },
            ],
            vec![
                new_op("conv/other", b1, false),
                TeamOp::AddRule {
                    rule: refonly,
                    enabled: true,
                },
            ],
        ],
        vec![b_b1],
    );
    assert_eq!(decided(r.accept(&e.input())), vec![Ok(()), Ok(())]);
    assert_eq!(r.certified("conv/modules"), Some(true));
    assert_eq!(
        r.certified("conv/other"),
        Some(false),
        "reference-only rule must not support"
    );
    // Superseding A: the Pin no longer holds (Pin-strict), B goes stale.
    let (a2, b_a2) = blob(b"naming: kebab-case");
    let e = c.entry(
        &ALICE,
        "alice",
        vec![vec![
            TeamOp::Revise {
                path: "conv/naming".into(),
                content: a2,
                adopt_as_base: true,
            },
            TeamOp::SetHead {
                path: "conv/naming".into(),
                expected: a1,
                new: a2,
            },
        ]],
        vec![b_a2],
    );
    assert_eq!(decided(r.accept(&e.input())), vec![Ok(())]);
    assert_eq!(r.certified("conv/naming"), Some(true));
    assert_eq!(
        r.certified("conv/modules"),
        Some(false),
        "Pin to an old head supports nothing"
    );
    // A stale SetHead expectation and a moved Cur premise are rejected.
    let cur = RuleSpec {
        conclusion: ("conv/modules".into(), b1),
        incidences: vec![("conv/naming".into(), Edge::Cur(a1))],
    };
    let e = c.entry(
        &BOB,
        "bob",
        vec![
            vec![TeamOp::SetHead {
                path: "conv/naming".into(),
                expected: a1,
                new: a2,
            }],
            vec![TeamOp::AddRule {
                rule: cur,
                enabled: true,
            }],
            vec![TeamOp::SetTags {
                path: "conv/naming".into(),
                expected: tags_hash(&Default::default()),
                tags: ["style".to_owned()].into(),
            }],
            vec![TeamOp::SetTags {
                path: "conv/naming".into(),
                expected: tags_hash(&Default::default()),
                tags: ["x".to_owned()].into(),
            }],
        ],
        vec![],
    );
    assert_eq!(
        decided(r.accept(&e.input())),
        vec![
            Err("HeadConflict"),
            Err("CurMoved"),
            Ok(()),
            Err("TagsConflict")
        ]
    );
    // Disabling the Pin rule needs the expected flag.
    let e = c.entry(
        &BOB,
        "bob",
        vec![vec![TeamOp::SetRuleEnabled {
            rule: pin,
            expected: true,
            enabled: false,
        }]],
        vec![],
    );
    assert_eq!(decided(r.accept(&e.input())), vec![Ok(())]);
}

#[test]
fn rejected_entries_keep_their_position() {
    let (mut c, mut r) = (Chain::new(), Replica::new(TEAM, ObjectFormat::Sha1));
    genesis(&mut c, &mut r, None);
    let (a1, b_a1) = blob(b"a");
    // Wrong Ctx-Team-Seq, a forged author, an unknown trailer member: each is
    // rejected as a whole and still takes its position.
    let bad_seq = c.entry_with(
        &BOB,
        "bob",
        vec![vec![new_op("k/a", a1, false)]],
        vec![b_a1.clone()],
        |t| t[1].1 = "99".into(),
    );
    assert!(matches!(r.accept(&bad_seq.input()), Outcome::EntryRejected(ref x) if x.code == "Seq"));
    let forged = c.entry(
        &BOB,
        "alice",
        vec![vec![new_op("k/a", a1, false)]],
        vec![b_a1.clone()],
    );
    assert!(
        matches!(r.accept(&forged.input()), Outcome::EntryRejected(ref x) if x.code == "Signature")
    );
    let stranger = c.entry(
        &[9; 32],
        "carol",
        vec![vec![new_op("k/a", a1, false)]],
        vec![b_a1.clone()],
    );
    assert!(
        matches!(r.accept(&stranger.input()), Outcome::EntryRejected(ref x) if x.code == "NotTeamMember")
    );
    // Missing blob: the proposal is rejected; the bytes must be sent again.
    let e = c.entry(&BOB, "bob", vec![vec![new_op("k/a", a1, false)]], vec![]);
    assert_eq!(decided(r.accept(&e.input())), vec![Err("MissingBlob")]);
    // The next valid entry is numbered by position and accepted.
    let ok = c.entry(
        &BOB,
        "bob",
        vec![vec![new_op("k/a", a1, false)]],
        vec![b_a1],
    );
    assert_eq!(ok.seq, 5);
    assert_eq!(decided(r.accept(&ok.input())), vec![Ok(())]);
    assert_eq!(r.seq(), Some(5));
    // A tree with anything but `batch` and named blobs is rejected.
    let mut odd = c.entry(
        &BOB,
        "bob",
        vec![vec![TeamOp::Expect {
            path: "k/a".into(),
            content: a1,
        }]],
        vec![],
    );
    odd.tree.push(("README".into(), b"x".to_vec()));
    assert!(matches!(r.accept(&odd.input()), Outcome::EntryRejected(ref x) if x.code == "Tree"));
}

#[test]
fn duplicate_proposal_and_admin_ops() {
    let (mut c, mut r) = (Chain::new(), Replica::new(TEAM, ObjectFormat::Sha1));
    genesis(&mut c, &mut r, None);
    let (a1, b_a1) = blob(b"a");
    let e = c.entry(
        &BOB,
        "bob",
        vec![vec![new_op("k/a", a1, false)]],
        vec![b_a1.clone()],
    );
    assert_eq!(decided(r.accept(&e.input())), vec![Ok(())]);
    // The same (member, proposal id) again is not applied twice.
    let id = hex(&[c.next_id - 1; 16]);
    let again = c.entry_with(
        &BOB,
        "bob",
        vec![vec![TeamOp::Expect {
            path: "k/a".into(),
            content: a1,
        }]],
        vec![],
        |t| t[5].1 = id,
    );
    assert_eq!(
        decided(r.accept(&again.input())),
        vec![Err("DuplicateProposal")]
    );
    // Shred and SetTeamAccess are admin-only; SetTeamAccess is a sole op.
    let e = c.entry(
        &BOB,
        "bob",
        vec![vec![TeamOp::TeamShred { content: a1 }]],
        vec![],
    );
    assert_eq!(decided(r.accept(&e.input())), vec![Err("AdminOnly")]);
    let e = c.entry(
        &ALICE,
        "alice",
        vec![
            vec![TeamOp::SetTeamAccess {
                doc: Box::new(doc(None)),
            }],
            vec![TeamOp::TeamShred { content: a1 }],
        ],
        vec![],
    );
    assert!(
        matches!(r.accept(&e.input()), Outcome::EntryRejected(ref x) if x.code == "SoleOpRequired")
    );
    let e = c.entry(
        &ALICE,
        "alice",
        vec![vec![TeamOp::TeamShred { content: a1 }]],
        vec![],
    );
    assert_eq!(decided(r.accept(&e.input())), vec![Ok(())]);
    // Shredded bytes can never come back, under any key.
    let e = c.entry(
        &BOB,
        "bob",
        vec![vec![new_op("k/b", a1, false)]],
        vec![b_a1],
    );
    assert_eq!(decided(r.accept(&e.input())), vec![Err("ContentShredded")]);
}

#[test]
fn state_limit_is_per_proposal() {
    // Room for the document and one small record, not for a 100 KiB one.
    let (mut c, mut r) = (Chain::new(), Replica::new(TEAM, ObjectFormat::Sha1));
    genesis(&mut c, &mut r, Some(4096));
    let (small, b_small) = blob(b"small");
    let big_bytes = vec![b'x'; 100 * 1024];
    let (big, b_big) = blob(&big_bytes);
    let e = c.entry(
        &BOB,
        "bob",
        vec![
            vec![new_op("k/small", small, false)],
            vec![new_op("k/big", big, false)],
            vec![TeamOp::Expect {
                path: "k/small".into(),
                content: small,
            }],
        ],
        vec![b_small, b_big],
    );
    assert_eq!(
        decided(r.accept(&e.input())),
        vec![Ok(()), Err("TeamStateLimit"), Ok(())]
    );
    let s = r.keyed_state().unwrap();
    assert!(s.ckpt_size_reserved(|_| 5) <= 4096);
}

#[test]
fn two_edges_agree_on_decisions_and_digest() {
    let mut c = Chain::new();
    let (mut r1, mut r2) = (
        Replica::new(TEAM, ObjectFormat::Sha1),
        Replica::new(TEAM, ObjectFormat::Sha1),
    );
    let mut entries = Vec::new();
    entries.push(c.entry(
        &ALICE,
        "alice",
        vec![vec![TeamOp::SetTeamAccess {
            doc: Box::new(doc(None)),
        }]],
        vec![],
    ));
    for i in 0..20u8 {
        let (h, b) = blob(&[i; 8]);
        let path = format!("k/{}", i % 5);
        let ops = if i < 5 {
            vec![new_op(&path, h, i % 2 == 0)]
        } else {
            vec![TeamOp::Revise {
                path: path.clone(),
                content: h,
                adopt_as_base: false,
            }]
        };
        let signer = if i % 3 == 0 {
            (&ALICE, "alice")
        } else {
            (&BOB, "bob")
        };
        entries.push(c.entry(signer.0, signer.1, vec![ops], vec![b]));
    }
    let o1: Vec<_> = entries.iter().map(|e| r1.accept(&e.input())).collect();
    let o2: Vec<_> = entries.iter().map(|e| r2.accept(&e.input())).collect();
    assert_eq!(o1, o2);
    assert_eq!(r1.digest(), r2.digest());
    assert!(r1.digest().is_some());
}
