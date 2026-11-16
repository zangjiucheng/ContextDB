//! Compaction in process (ADR 0018 §8.6, SPEC SD21.13; the in-process half
//! of T118): one post-Compact state Σc for T, D, accept, resume and joiners;
//! entries after `Compact` ignored; invalid successors not followed; a
//! checkpoint larger than 8 MiB; the exact Compact reserve.

use contextdb_model::RecordKind;
use contextdb_team::checkpoint;
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::gitobj;
use contextdb_team::ops::{Edge, Hash, RuleSpec, TeamBatch, TeamOp, tags_hash};
use contextdb_team::replica::{EntryInput, Outcome, Replica};
use contextdb_team::sshsig;
use std::collections::BTreeMap;

const TEAM: [u8; 16] = [0xc0; 16];
const ALICE: [u8; 32] = [1; 32];
const BOB: [u8; 32] = [2; 32];

fn doc(state_bytes: u64) -> TeamDoc {
    TeamDoc::from_toml(&format!(
        "version = 1\naccept_version = 1\nid = \"{}\"\nadmins = [\"alice\"]\n\
         [members.alice]\nkeys = [\"{}\"]\n[members.bob]\nkeys = [\"{}\"]\n\
         [lanes.main]\nwriters = [\"*members\"]\ncurators = [\"*members\"]\n\
         [limits]\nstate_bytes = {state_bytes}\n",
        hex(&TEAM),
        sshsig::public_key(&ALICE).to_line(),
        sshsig::public_key(&BOB).to_line(),
    ))
    .unwrap()
}

struct Entry {
    oid: String,
    seq: u64,
    first: bool,
    raw: Vec<u8>,
    tree: Vec<(String, Vec<u8>)>,
}

impl Entry {
    fn input(&self) -> EntryInput<'_> {
        EntryInput {
            oid: &self.oid,
            seq: self.seq,
            first_in_generation: self.first,
            commit: &self.raw,
            tree: &self.tree,
        }
    }
}

fn oid_of(raw: &[u8]) -> String {
    hex(&gitobj::object_id(ObjectFormat::Sha1, "commit", raw).unwrap())
}

/// A chain builder that tracks the parent and the next sequence number.
struct Chain {
    seq: u64,
    parent: Option<String>,
    next_id: u8,
}

impl Chain {
    fn entry(
        &mut self,
        key: &[u8; 32],
        author: &str,
        proposals: Vec<Vec<TeamOp>>,
        blobs: Vec<(String, Vec<u8>)>,
    ) -> Entry {
        let batch = TeamBatch { proposals }.encode();
        let n = TeamBatch::decode(&batch).unwrap().proposals.len();
        let mut tr: Vec<(&str, String)> = vec![
            ("Ctx-Team", hex(&TEAM)),
            ("Ctx-Team-Seq", self.seq.to_string()),
            ("Ctx-Author", author.into()),
            ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
            ("Ctx-Accept-Version", "1".into()),
        ];
        for _ in 0..n {
            tr.push(("Ctx-Proposal", hex(&[self.next_id; 16])));
            self.next_id += 1;
        }
        let mut tree = vec![("batch".to_owned(), batch)];
        tree.extend(blobs);
        let tree_hex = hex(&gitobj::tree_id(ObjectFormat::Sha1, &tree).unwrap());
        let parents: Vec<String> = self.parent.iter().cloned().collect();
        let unsigned = commit::build(&tree_hex, &parents, "ctx team: entry", &tr);
        let raw = commit::with_signature(
            &unsigned,
            &sshsig::sign(key, "git", &unsigned),
            ObjectFormat::Sha1,
        );
        let e = Entry {
            oid: oid_of(&raw),
            seq: self.seq,
            first: self.parent.is_none(),
            raw,
            tree,
        };
        self.parent = Some(e.oid.clone());
        self.seq += 1;
        e
    }

    /// The successor root commit for an accepted `Compact` with `compact_oid`.
    fn successor(
        &mut self,
        key: &[u8; 32],
        author: &str,
        compact_oid: &str,
        files: Vec<(String, Vec<u8>)>,
    ) -> Entry {
        let tree_hex = hex(&gitobj::tree_id(ObjectFormat::Sha1, &files).unwrap());
        let tr: Vec<(&str, String)> = vec![
            ("Ctx-Team", hex(&TEAM)),
            ("Ctx-Team-Seq", self.seq.to_string()),
            ("Ctx-Author", author.into()),
            ("Ctx-Accept-Version", "1".into()),
            ("Ctx-Compacts", compact_oid.into()),
        ];
        let unsigned = commit::build(&tree_hex, &[], "ctx team: checkpoint", &tr);
        let raw = commit::with_signature(
            &unsigned,
            &sshsig::sign(key, "git", &unsigned),
            ObjectFormat::Sha1,
        );
        let e = Entry {
            oid: oid_of(&raw),
            seq: self.seq,
            first: true,
            raw,
            tree: files,
        };
        self.parent = Some(e.oid.clone());
        self.seq += 1;
        e
    }
}

fn blob(bytes: Vec<u8>) -> (Hash, (String, Vec<u8>)) {
    let h = *blake3::hash(&bytes).as_bytes();
    (h, (format!("blobs/{}", hex(&h)), bytes))
}

fn new_op(path: &str, h: Hash, base: bool) -> TeamOp {
    TeamOp::New {
        path: path.into(),
        lane: "main".into(),
        kind: RecordKind::Decision,
        content: h,
        adopt_as_base: base,
    }
}

fn ok(o: Outcome) {
    match o {
        Outcome::Decided(v) => assert!(v.iter().all(Result::is_ok), "{v:?}"),
        other => panic!("{other:?}"),
    }
}

/// A team with records, a superseded head moved back to old content, a rule,
/// tags and a shred.
fn populated(state_bytes: u64) -> (Chain, Replica) {
    let mut c = Chain {
        seq: 0,
        parent: None,
        next_id: 1,
    };
    let mut r = Replica::new(TEAM, ObjectFormat::Sha1);
    ok(r.accept(
        &c.entry(
            &ALICE,
            "alice",
            vec![vec![TeamOp::SetTeamAccess {
                doc: Box::new(doc(state_bytes)),
            }]],
            vec![],
        )
        .input(),
    ));
    let (a1, ba1) = blob(b"a1".to_vec());
    let (a2, ba2) = blob(b"a2".to_vec());
    let (b1, bb1) = blob(b"b1".to_vec());
    let (gone, bgone) = blob(b"secret".to_vec());
    ok(r.accept(
        &c.entry(
            &BOB,
            "bob",
            vec![vec![
                new_op("arch/a", a1, true),
                new_op("arch/b", b1, false),
                new_op("arch/g", gone, false),
            ]],
            vec![ba1, bb1, bgone],
        )
        .input(),
    ));
    let rule = RuleSpec {
        conclusion: ("arch/b".into(), b1),
        incidences: vec![("arch/a".into(), Edge::Pin(a1))],
    };
    ok(r.accept(
        &c.entry(
            &BOB,
            "bob",
            vec![vec![
                TeamOp::AddRule {
                    rule,
                    enabled: true,
                },
                TeamOp::SetTags {
                    path: "arch/a".into(),
                    expected: tags_hash(&Default::default()),
                    tags: ["db".to_owned()].into(),
                },
                TeamOp::Revise {
                    path: "arch/a".into(),
                    content: a2,
                    adopt_as_base: true,
                },
                TeamOp::SetHead {
                    path: "arch/a".into(),
                    expected: a1,
                    new: a2,
                },
            ]],
            vec![ba2],
        )
        .input(),
    ));
    // Back to old content: the joiner must reproduce the history, not just heads.
    ok(r.accept(
        &c.entry(
            &BOB,
            "bob",
            vec![vec![TeamOp::SetHead {
                path: "arch/a".into(),
                expected: a2,
                new: a1,
            }]],
            vec![],
        )
        .input(),
    ));
    ok(r.accept(
        &c.entry(
            &ALICE,
            "alice",
            vec![vec![TeamOp::TeamShred { content: gone }]],
            vec![],
        )
        .input(),
    ));
    (c, r)
}

/// Builds the `Compact` the way an admin's ctxd does: Σc from the replica.
fn compact_entry(
    c: &mut Chain,
    r: &Replica,
    key: &[u8; 32],
    author: &str,
) -> (Entry, Vec<(String, Vec<u8>)>) {
    let id = [c.next_id; 16];
    let sc = r.sigma_c(author, id, c.seq).unwrap();
    let files = r.checkpoint_files(&sc, r.generation() + 1);
    let tree = gitobj::tree_id(ObjectFormat::Sha1, &files).unwrap();
    let e = c.entry(
        key,
        author,
        vec![vec![TeamOp::Compact {
            generation: r.generation() + 1,
            tree,
            digest: sc.digest(),
        }]],
        vec![],
    );
    (e, files)
}

#[test]
fn follower_and_joiner_agree_and_entries_after_compact_are_ignored() {
    let (mut c, mut follower) = populated(1 << 30);
    let before = follower.digest();
    let (ce, files) = compact_entry(&mut c, &follower, &ALICE, "alice");
    ok(follower.accept(&ce.input()));
    assert!(follower.pending().is_some());
    // Σc is exactly the replica after Compact: --resume rebuilds the same T.
    let resumed = follower.checkpoint_files(&follower.keyed_state().unwrap(), 1);
    assert_eq!(
        gitobj::tree_id(ObjectFormat::Sha1, &resumed).unwrap(),
        follower.pending().unwrap().tree
    );
    assert_ne!(follower.digest(), before);
    // Junk appended to generation 0 after Compact: ignored, no sequence.
    let digest_at_compact = follower.digest();
    let seq_at_compact = follower.seq();
    let junk = c.entry(
        &BOB,
        "bob",
        vec![vec![TeamOp::Expect {
            path: "arch/a".into(),
            content: [0; 32],
        }]],
        vec![],
    );
    assert_eq!(follower.accept(&junk.input()), Outcome::Ignored);
    assert_eq!(
        (follower.digest(), follower.seq()),
        (digest_at_compact, seq_at_compact)
    );
    c.seq -= 1; // the junk took no sequence number
    // The successor: a root commit with tree T naming the Compact.
    let mut gen1 = Chain {
        seq: c.seq,
        parent: None,
        next_id: c.next_id,
    };
    let f = gen1.successor(&ALICE, "alice", &ce.oid, files.clone());
    assert_eq!(follower.accept(&f.input()), Outcome::Successor);
    assert_eq!(
        (follower.generation(), follower.seq()),
        (1, Some(ce.seq + 1))
    );
    // A joiner given F out of band reaches the same state and digest.
    let mut joiner = Replica::join_checkpoint(TEAM, ObjectFormat::Sha1, &f.raw, &f.tree).unwrap();
    assert_eq!(joiner.digest(), follower.digest());
    assert_eq!(joiner.keyed_state(), follower.keyed_state());
    assert_eq!(joiner.certified("arch/b"), follower.certified("arch/b"));
    assert_eq!(
        joiner.certified("arch/b"),
        Some(true),
        "the Pin holds: the head moved back to a1"
    );
    // Both continue identically in generation 1.
    let (x, bx) = blob(b"x".to_vec());
    let next = gen1.entry(
        &BOB,
        "bob",
        vec![
            vec![new_op("arch/x", x, false)],
            vec![TeamOp::SetHead {
                path: "arch/a".into(),
                expected: [9; 32],
                new: x,
            }],
        ],
        vec![bx],
    );
    let o1 = follower.accept(&next.input());
    assert_eq!(o1, joiner.accept(&next.input()));
    assert_eq!(follower.digest(), joiner.digest());
}

#[test]
fn bad_compacts_are_rejected_and_the_generation_goes_on() {
    let (mut c, mut r) = populated(1 << 30);
    // A wrong digest, a wrong tree, a non-admin: ordinary rejections.
    let id = [c.next_id; 16];
    let sc = r.sigma_c("alice", id, c.seq).unwrap();
    let tree = gitobj::tree_id(ObjectFormat::Sha1, &r.checkpoint_files(&sc, 1)).unwrap();
    let wrong_digest = c.entry(
        &ALICE,
        "alice",
        vec![vec![TeamOp::Compact {
            generation: 1,
            tree: tree.clone(),
            digest: [7; 32],
        }]],
        vec![],
    );
    let codes = |o: Outcome| match o {
        Outcome::Decided(v) => v
            .into_iter()
            .map(|d| d.err().map(|r| r.code))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        codes(r.accept(&wrong_digest.input())),
        vec![Some("CompactDigest")]
    );
    let id = [c.next_id; 16];
    let sc = r.sigma_c("alice", id, c.seq).unwrap();
    let wrong_tree = c.entry(
        &ALICE,
        "alice",
        vec![vec![TeamOp::Compact {
            generation: 1,
            tree: vec![0; 20],
            digest: sc.digest(),
        }]],
        vec![],
    );
    assert_eq!(
        codes(r.accept(&wrong_tree.input())),
        vec![Some("CompactTree")]
    );
    let (by_bob, _) = compact_entry(&mut c, &r, &BOB, "bob");
    assert_eq!(codes(r.accept(&by_bob.input())), vec![Some("AdminOnly")]);
    assert!(r.pending().is_none());
    // The generation goes on; a correct Compact still works afterwards.
    let (x, bx) = blob(b"after".to_vec());
    ok(r.accept(
        &c.entry(
            &BOB,
            "bob",
            vec![vec![new_op("arch/after", x, false)]],
            vec![bx],
        )
        .input(),
    ));
    let (ce, files) = compact_entry(&mut c, &r, &ALICE, "alice");
    ok(r.accept(&ce.input()));
    // Invalid successors are not followed and change nothing.
    let mut g = Chain {
        seq: c.seq,
        parent: None,
        next_id: c.next_id,
    };
    let mut tampered = files.clone();
    tampered.push(("extra".into(), b"x".to_vec()));
    for (name, e) in [
        ("tree", g.successor(&ALICE, "alice", &ce.oid, tampered)),
        (
            "compacts",
            Chain {
                seq: c.seq,
                parent: None,
                next_id: 1,
            }
            .successor(&ALICE, "alice", &"0".repeat(40), files.clone()),
        ),
        (
            "signer",
            Chain {
                seq: c.seq,
                parent: None,
                next_id: 1,
            }
            .successor(&BOB, "bob", &ce.oid, files.clone()),
        ),
        (
            "seq",
            Chain {
                seq: c.seq + 3,
                parent: None,
                next_id: 1,
            }
            .successor(&ALICE, "alice", &ce.oid, files.clone()),
        ),
    ] {
        assert!(
            matches!(r.accept(&e.input()), Outcome::InvalidSuccessor(_)),
            "{name} followed"
        );
        assert!(r.pending().is_some());
    }
    let f = Chain {
        seq: c.seq,
        parent: None,
        next_id: 1,
    }
    .successor(&ALICE, "alice", &ce.oid, files);
    assert_eq!(r.accept(&f.input()), Outcome::Successor);
}

#[test]
fn a_checkpoint_over_8_mib_compacts_and_joins() {
    let mut c = Chain {
        seq: 0,
        parent: None,
        next_id: 1,
    };
    let mut r = Replica::new(TEAM, ObjectFormat::Sha1);
    ok(r.accept(
        &c.entry(
            &ALICE,
            "alice",
            vec![vec![TeamOp::SetTeamAccess {
                doc: Box::new(doc(1 << 30)),
            }]],
            vec![],
        )
        .input(),
    ));
    // Twelve records of 900 KiB each, one entry per record: each entry is
    // legal, the state is over 10 MiB.
    for i in 0..12u8 {
        let (h, b) = blob(vec![i; 900 * 1024]);
        ok(r.accept(
            &c.entry(
                &BOB,
                "bob",
                vec![vec![new_op(&format!("big/{i}"), h, true)]],
                vec![b],
            )
            .input(),
        ));
    }
    let (ce, files) = compact_entry(&mut c, &r, &ALICE, "alice");
    let total: usize = files.iter().map(|f| f.1.len()).sum();
    assert!(total > 8 << 20, "{total}");
    ok(r.accept(&ce.input()));
    let f = Chain {
        seq: c.seq,
        parent: None,
        next_id: 1,
    }
    .successor(&ALICE, "alice", &ce.oid, files.clone());
    assert_eq!(r.accept(&f.input()), Outcome::Successor);
    let joiner = Replica::join_checkpoint(TEAM, ObjectFormat::Sha1, &f.raw, &f.tree).unwrap();
    assert_eq!(joiner.digest(), r.digest());
    // The encoded size is the exact ckpt_size, which the index records.
    let map: BTreeMap<String, Vec<u8>> = files.iter().cloned().collect();
    let dec = checkpoint::decode(&map).unwrap();
    assert_eq!(dec.header.total, total as u64);
    // Damage: a missing blob is fetchable, a corrupt chunk is invalid.
    let mut missing = map.clone();
    let blob_path = missing
        .keys()
        .find(|k| k.starts_with("blobs/"))
        .unwrap()
        .clone();
    missing.remove(&blob_path);
    assert_eq!(
        checkpoint::decode(&missing).unwrap_err(),
        checkpoint::CkptError::Missing(blob_path)
    );
    let mut corrupt = map.clone();
    corrupt.get_mut(&checkpoint::chunk_path(0)).unwrap()[10] ^= 1;
    assert!(matches!(
        checkpoint::decode(&corrupt),
        Err(checkpoint::CkptError::Invalid(_))
    ));
    let mut extra = map;
    extra.insert("checkpoint/state/999999".into(), vec![1]);
    assert!(matches!(
        checkpoint::decode(&extra),
        Err(checkpoint::CkptError::Invalid(_))
    ));
}

#[test]
fn the_compact_reserve_is_exact() {
    // The document's encoded size does not depend on the limit's value (a
    // fixed-width u64), so the state a history reaches has the same size
    // under any limit. Measure the bound under a large limit, then replay
    // the same history with the limit at the bound and one byte below.
    let (h, b) = blob(b"last".to_vec());
    let history = |limit: u64| -> (Chain, Replica, Outcome) {
        let (mut c, mut r) = populated(1 << 30);
        ok(r.accept(
            &c.entry(
                &ALICE,
                "alice",
                vec![vec![TeamOp::SetTeamAccess {
                    doc: Box::new(doc(limit)),
                }]],
                vec![],
            )
            .input(),
        ));
        let o = r.accept(
            &c.entry(
                &BOB,
                "bob",
                vec![vec![new_op("arch/last", h, false)]],
                vec![b.clone()],
            )
            .input(),
        );
        (c, r, o)
    };
    let (_, probe, o) = history(1 << 30);
    ok(o);
    let need = probe
        .keyed_state()
        .unwrap()
        .ckpt_size_reserved(|h| probe.blob(h).map_or(0, |b| b.len() as u64));
    // At the bound: accepted, and a Compact then fits without a second
    // reserve (it checks only its actual size).
    let (mut c, r, o) = history(need);
    ok(o);
    let mut r = r;
    let (ce, files) = compact_entry(&mut c, &r, &ALICE, "alice");
    ok(r.accept(&ce.input()));
    let total: u64 = files.iter().map(|f| f.1.len() as u64).sum();
    assert!(total <= need, "{total} > {need}");
    // One byte below: the proposal alone is refused.
    let (_, _, o) = history(need - 1);
    match o {
        Outcome::Decided(d) => {
            assert_eq!(d[0].as_ref().err().map(|r| r.code), Some("TeamStateLimit"))
        }
        other => panic!("{other:?}"),
    }
}
