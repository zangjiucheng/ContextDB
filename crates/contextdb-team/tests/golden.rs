//! The golden corpus (ADR 0018 §4.2, §13; T112): one small signed chain per
//! structural check and per rejection reason, each followed by a valid
//! entry, with the expected outcome of every entry and the final digest.
//! `accept_version 1` is frozen: a build that decides any of them
//! differently fails here. `CONTEXTDB_BLESS=1` rewrites the corpus (only
//! for a new corpus entry, never to change an existing outcome).

use contextdb_model::RecordKind;
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::enc::{Reader, Writer};
use contextdb_team::gitobj;
use contextdb_team::ops::{Edge, Hash, RuleSpec, TeamBatch, TeamOp, tags_hash};
use contextdb_team::replica::{EntryInput, Outcome, Replica};
use contextdb_team::sshsig;
use std::path::PathBuf;

const TEAM: [u8; 16] = [0x60; 16];
const ALICE: [u8; 32] = [1; 32];
const BOB: [u8; 32] = [2; 32];
const CAROL: [u8; 32] = [3; 32];

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/team-chains")
}

fn doc(extra: &str, accept_version: u32, state_bytes: u64) -> TeamDoc {
    TeamDoc::from_toml(&format!(
        "version = 1\naccept_version = {accept_version}\nid = \"{}\"\nadmins = [\"alice\"]\n\
         [members.alice]\nkeys = [\"{}\"]\n[members.bob]\nkeys = [\"{}\"]\n\
         [lanes.main]\nwriters = [\"*members\"]\ncurators = [\"alice\"]\n\
         [lanes.arch]\nwriters = [\"alice\"]\ncurators = [\"alice\"]\n{extra}\
         [limits]\nstate_bytes = {state_bytes}\n",
        hex(&TEAM),
        sshsig::public_key(&ALICE).to_line(),
        sshsig::public_key(&BOB).to_line(),
    ))
    .unwrap()
}

#[derive(Clone)]
struct Entry {
    oid: String,
    seq: u64,
    first: bool,
    raw: Vec<u8>,
    tree: Vec<(String, Vec<u8>)>,
}

/// Knobs a scenario turns to break one thing about an entry.
#[derive(Default, Clone)]
struct Tweak {
    seq: Option<String>,
    author: Option<&'static str>,
    key: Option<[u8; 32]>,
    unsigned: bool,
    team: Option<String>,
    batch_hash: Option<String>,
    accept_version: Option<String>,
    drop_proposal_trailer: bool,
    extra_parent: bool,
    extra_file: Option<(String, Vec<u8>)>,
    raw_batch: Option<Vec<u8>>,
    proposal_id: Option<[u8; 16]>,
}

struct Chain {
    seq: u64,
    parent: Option<String>,
    next_id: u8,
    entries: Vec<Entry>,
}

impl Chain {
    fn new() -> Self {
        Self {
            seq: 0,
            parent: None,
            next_id: 1,
            entries: Vec::new(),
        }
    }
    fn push(
        &mut self,
        key: &[u8; 32],
        author: &'static str,
        proposals: Vec<Vec<TeamOp>>,
        blobs: Vec<(String, Vec<u8>)>,
        t: Tweak,
    ) -> &mut Self {
        let batch = t.raw_batch.clone().unwrap_or_else(|| {
            TeamBatch {
                proposals: proposals.clone(),
            }
            .encode()
        });
        let mut tr: Vec<(&str, String)> = vec![
            ("Ctx-Team", t.team.clone().unwrap_or_else(|| hex(&TEAM))),
            (
                "Ctx-Team-Seq",
                t.seq.clone().unwrap_or_else(|| self.seq.to_string()),
            ),
            ("Ctx-Author", t.author.unwrap_or(author).into()),
            (
                "Ctx-Batch-Hash",
                t.batch_hash
                    .clone()
                    .unwrap_or_else(|| TeamBatch::hash_hex(&batch)),
            ),
            (
                "Ctx-Accept-Version",
                t.accept_version.clone().unwrap_or_else(|| "1".into()),
            ),
        ];
        let n = if t.drop_proposal_trailer {
            proposals.len().saturating_sub(1)
        } else {
            proposals.len()
        };
        for _ in 0..n {
            let id = t.proposal_id.unwrap_or([self.next_id; 16]);
            tr.push(("Ctx-Proposal", hex(&id)));
            self.next_id += 1;
        }
        let mut tree = vec![("batch".to_owned(), batch)];
        tree.extend(blobs);
        tree.extend(t.extra_file.clone());
        let tree_hex = hex(&gitobj::tree_id(ObjectFormat::Sha1, &tree).unwrap());
        let mut parents: Vec<String> = self.parent.iter().cloned().collect();
        if t.extra_parent {
            parents.push("1".repeat(40));
        }
        let unsigned = commit::build(&tree_hex, &parents, "ctx team: entry", &tr);
        let raw = if t.unsigned {
            unsigned
        } else {
            commit::with_signature(
                &unsigned,
                &sshsig::sign(&t.key.unwrap_or(*key), "git", &unsigned),
                ObjectFormat::Sha1,
            )
        };
        let oid = hex(&gitobj::object_id(ObjectFormat::Sha1, "commit", &raw).unwrap());
        self.entries.push(Entry {
            oid: oid.clone(),
            seq: self.seq,
            first: self.parent.is_none(),
            raw,
            tree,
        });
        self.parent = Some(oid);
        self.seq += 1;
        self
    }
    fn ok(
        &mut self,
        key: &[u8; 32],
        author: &'static str,
        p: Vec<Vec<TeamOp>>,
        b: Vec<(String, Vec<u8>)>,
    ) -> &mut Self {
        self.push(key, author, p, b, Tweak::default())
    }
    fn genesis(d: TeamDoc) -> Self {
        let mut c = Self::new();
        c.ok(
            &ALICE,
            "alice",
            vec![vec![TeamOp::SetTeamAccess { doc: Box::new(d) }]],
            vec![],
        );
        c
    }
}

fn blob(bytes: &[u8]) -> (Hash, (String, Vec<u8>)) {
    let h = *blake3::hash(bytes).as_bytes();
    (h, (format!("blobs/{}", hex(&h)), bytes.to_vec()))
}

fn new(path: &str, lane: &str, kind: RecordKind, h: Hash, base: bool) -> TeamOp {
    TeamOp::New {
        path: path.into(),
        lane: lane.into(),
        kind,
        content: h,
        adopt_as_base: base,
    }
}

/// Every scenario: a base team, one breaking entry, one valid entry after.
fn scenarios() -> Vec<(&'static str, Chain)> {
    let d = || doc("", 1, 1 << 30);
    let (a, ba) = blob(b"a");
    let (a2, ba2) = blob(b"a2");
    let (b, bb) = blob(b"b");
    let fact = RecordKind::Fact;
    let valid = |c: &mut Chain| {
        let (z, bz) = blob(format!("after {}", c.seq).as_bytes());
        c.ok(
            &BOB,
            "bob",
            vec![vec![new(
                &format!("after/{}", c.seq),
                "main",
                fact,
                z,
                false,
            )]],
            vec![bz],
        );
    };
    // A team with one record `k/a` (base, by alice) and `k/b` (bob).
    let base = |d: TeamDoc| {
        let mut c = Chain::genesis(d);
        c.ok(
            &ALICE,
            "alice",
            vec![vec![
                new("k/a", "main", fact, a, true),
                new("k/b", "main", fact, b, false),
            ]],
            vec![ba.clone(), bb.clone()],
        );
        c
    };
    let mut out: Vec<(&'static str, Chain)> = Vec::new();
    let mut add = |name: &'static str, mut c: Chain| {
        valid(&mut c);
        out.push((name, c));
    };
    let op = |p: Vec<TeamOp>| vec![p];
    // Structural (entry-level) rejections.
    for (name, t) in [
        (
            "seq-wrong",
            Tweak {
                seq: Some("99".into()),
                ..Default::default()
            },
        ),
        (
            "seq-leading-zero",
            Tweak {
                seq: Some("02".into()),
                ..Default::default()
            },
        ),
        (
            "forged-author",
            Tweak {
                author: Some("alice"),
                ..Default::default()
            },
        ),
        (
            "not-member",
            Tweak {
                author: Some("carol"),
                key: Some(CAROL),
                ..Default::default()
            },
        ),
        (
            "unsigned",
            Tweak {
                unsigned: true,
                ..Default::default()
            },
        ),
        (
            "wrong-team",
            Tweak {
                team: Some("ff".repeat(16)),
                ..Default::default()
            },
        ),
        (
            "batch-hash",
            Tweak {
                batch_hash: Some("00".repeat(32)),
                ..Default::default()
            },
        ),
        (
            "accept-version-trailer",
            Tweak {
                accept_version: Some("2".into()),
                ..Default::default()
            },
        ),
        (
            "proposal-count",
            Tweak {
                drop_proposal_trailer: true,
                ..Default::default()
            },
        ),
        (
            "two-parents",
            Tweak {
                extra_parent: true,
                ..Default::default()
            },
        ),
        (
            "odd-tree-file",
            Tweak {
                extra_file: Some(("README".into(), b"x".to_vec())),
                ..Default::default()
            },
        ),
        (
            "blob-name",
            Tweak {
                extra_file: Some((format!("blobs/{}", "00".repeat(32)), b"x".to_vec())),
                ..Default::default()
            },
        ),
        (
            "bad-cxtb",
            Tweak {
                raw_batch: Some(b"CXTB\x00\x01garbage".to_vec()),
                ..Default::default()
            },
        ),
    ] {
        let mut c = base(d());
        c.push(
            &BOB,
            "bob",
            op(vec![TeamOp::Expect {
                path: "k/a".into(),
                content: a,
            }]),
            vec![],
            t,
        );
        add(name, c);
    }
    let mut c = base(d());
    c.ok(
        &ALICE,
        "alice",
        vec![
            vec![TeamOp::SetTeamAccess { doc: Box::new(d()) }],
            vec![TeamOp::Expect {
                path: "k/a".into(),
                content: a,
            }],
        ],
        vec![],
    );
    add("sole-op", c);
    let (big, bbig) = blob(&vec![7u8; (1 << 20) + 1]);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![new("k/big", "main", fact, big, false)]),
        vec![bbig],
    );
    add("blob-too-large", c);
    // Per-proposal rejections.
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![new("k/x", "nope", fact, a2, false)]),
        vec![ba2.clone()],
    );
    add("unknown-lane", c);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![new("k/x", "arch", fact, a2, false)]),
        vec![ba2.clone()],
    );
    add("unauthorized-write", c);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![new("k/x", "main", fact, a2, true)]),
        vec![ba2.clone()],
    );
    add("unauthorized-curate", c);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![new("k/a", "main", fact, a2, false)]),
        vec![ba2.clone()],
    );
    add("logical-exists", c);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![new("k/x", "main", fact, a2, false)]),
        vec![],
    );
    add("missing-blob", c);
    let mut c = base(d());
    c.ok(
        &ALICE,
        "alice",
        op(vec![TeamOp::TeamShred { content: a2 }]),
        vec![],
    );
    c.ok(
        &BOB,
        "bob",
        op(vec![new("k/x", "main", fact, a2, false)]),
        vec![ba2.clone()],
    );
    add("content-shredded", c);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::Expect {
            path: "k/none".into(),
            content: a,
        }]),
        vec![],
    );
    add("unknown-key", c);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::SetHead {
            path: "k/a".into(),
            expected: a,
            new: a2,
        }]),
        vec![],
    );
    add("unknown-revision", c);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::SetHead {
            path: "k/a".into(),
            expected: a2,
            new: a,
        }]),
        vec![],
    );
    add("head-conflict", c);
    let mut c = base(d());
    c.ok(
        &ALICE,
        "alice",
        op(vec![TeamOp::SetBase {
            path: "k/a".into(),
            content: a,
            expected: false,
            base: true,
        }]),
        vec![],
    );
    add("base-conflict", c);
    let rule = RuleSpec {
        conclusion: ("k/b".into(), b),
        incidences: vec![("k/a".into(), Edge::Cur(a2))],
    };
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::AddRule {
            rule,
            enabled: true,
        }]),
        vec![],
    );
    add("cur-moved", c);
    let rule = RuleSpec {
        conclusion: ("k/b".into(), b),
        incidences: vec![("k/a".into(), Edge::Pin(a))],
    };
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::SetRuleEnabled {
            rule: rule.clone(),
            expected: true,
            enabled: false,
        }]),
        vec![],
    );
    add("unknown-rule", c);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::AddRule {
            rule: rule.clone(),
            enabled: true,
        }]),
        vec![],
    );
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::SetRuleEnabled {
            rule: rule.clone(),
            expected: false,
            enabled: true,
        }]),
        vec![],
    );
    add("enabled-conflict", c);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::AddRule {
            rule: rule.clone(),
            enabled: true,
        }]),
        vec![],
    );
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::AddRule {
            rule: rule.clone(),
            enabled: true,
        }]),
        vec![],
    );
    add("kernel-duplicate-rule", c);
    let empty = RuleSpec {
        conclusion: ("k/b".into(), b),
        incidences: vec![],
    };
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::AddRule {
            rule: empty,
            enabled: true,
        }]),
        vec![],
    );
    add("kernel-invalid-rule", c);
    let back = RuleSpec {
        conclusion: ("k/a".into(), a),
        incidences: vec![("k/b".into(), Edge::Pin(b))],
    };
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::AddRule {
            rule: rule.clone(),
            enabled: true,
        }]),
        vec![],
    );
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::AddRule {
            rule: back,
            enabled: true,
        }]),
        vec![],
    );
    add("kernel-cycle", c);
    let mut c = base(d());
    let (e, be) = blob(b"evidence");
    c.ok(
        &BOB,
        "bob",
        op(vec![new("k/e", "main", RecordKind::Evidence, e, false)]),
        vec![be],
    );
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::Revise {
            path: "k/e".into(),
            content: a2,
            adopt_as_base: false,
        }]),
        vec![ba2.clone()],
    );
    add("kernel-immutable-kind", c);
    let mut c = base(doc("[policy]\nrequire_deps = [\"plan\"]\n", 1, 1 << 30));
    c.ok(
        &ALICE,
        "alice",
        op(vec![new("k/p", "main", RecordKind::Plan, a2, true)]),
        vec![ba2.clone()],
    );
    add("kernel-policy-violation", c);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::SetTags {
            path: "k/a".into(),
            expected: tags_hash(&["x".to_owned()].into()),
            tags: Default::default(),
        }]),
        vec![],
    );
    add("tags-conflict", c);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::Expect {
            path: "k/a".into(),
            content: a2,
        }]),
        vec![],
    );
    add("read-set-conflict", c);
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::TeamShred { content: a2 }]),
        vec![],
    );
    add("admin-only", c);
    let mut c = base(d());
    let mut no_main = d();
    no_main.lanes.remove("main");
    c.ok(
        &ALICE,
        "alice",
        op(vec![TeamOp::SetTeamAccess {
            doc: Box::new(no_main),
        }]),
        vec![],
    );
    add("lane-in-use", c);
    let mut c = base(d());
    let id = [c.next_id - 1; 16];
    c.push(
        &ALICE,
        "alice",
        op(vec![TeamOp::Expect {
            path: "k/a".into(),
            content: a,
        }]),
        vec![],
        Tweak {
            proposal_id: Some(id),
            ..Default::default()
        },
    );
    add("duplicate-proposal", c);
    let mut c = base(doc("", 1, 2048));
    let (h, bh) = blob(&vec![1u8; 4096]);
    c.ok(
        &BOB,
        "bob",
        vec![
            vec![new("k/h", "main", fact, h, false)],
            vec![TeamOp::Expect {
                path: "k/a".into(),
                content: a,
            }],
        ],
        vec![bh],
    );
    add("team-state-limit", c);
    // A head moved back to old content, then a Pin to an old revision and a
    // reference-only rule (team status).
    let mut c = base(d());
    c.ok(
        &ALICE,
        "alice",
        op(vec![
            TeamOp::Revise {
                path: "k/a".into(),
                content: a2,
                adopt_as_base: true,
            },
            TeamOp::SetHead {
                path: "k/a".into(),
                expected: a,
                new: a2,
            },
        ]),
        vec![ba2.clone()],
    );
    c.ok(
        &BOB,
        "bob",
        op(vec![
            TeamOp::AddRule {
                rule: rule.clone(),
                enabled: true,
            },
            TeamOp::AddRule {
                rule: RuleSpec {
                    conclusion: ("k/b".into(), b),
                    incidences: vec![("k/a".into(), Edge::Ref(a2))],
                },
                enabled: true,
            },
        ]),
        vec![],
    );
    c.ok(
        &BOB,
        "bob",
        op(vec![TeamOp::SetHead {
            path: "k/a".into(),
            expected: a2,
            new: a,
        }]),
        vec![],
    );
    add("status-pin-old-and-ref-only", c);
    // Final state: k/a superseded (the Pin on its old head supports
    // nothing, k/b stale) and k/r supported only by a reference-only rule.
    let (r0, br0) = blob(b"r0");
    let mut c = base(d());
    c.ok(
        &BOB,
        "bob",
        op(vec![
            TeamOp::AddRule {
                rule: rule.clone(),
                enabled: true,
            },
            new("k/r", "main", fact, r0, false),
            TeamOp::AddRule {
                rule: RuleSpec {
                    conclusion: ("k/r".into(), r0),
                    incidences: vec![("k/a".into(), Edge::Ref(a))],
                },
                enabled: true,
            },
        ]),
        vec![br0],
    );
    c.ok(
        &ALICE,
        "alice",
        op(vec![
            TeamOp::Revise {
                path: "k/a".into(),
                content: a2,
                adopt_as_base: true,
            },
            TeamOp::SetHead {
                path: "k/a".into(),
                expected: a,
                new: a2,
            },
        ]),
        vec![ba2.clone()],
    );
    add("status-pin-superseded", c);
    out
}

fn outcome_line(o: &Outcome) -> String {
    match o {
        Outcome::EntryRejected(r) => format!("entry-rejected {}", r.code),
        Outcome::Decided(v) => {
            let parts: Vec<String> = v
                .iter()
                .map(|d| match d {
                    Ok(()) => "ok".to_owned(),
                    Err(r) if r.code == "Kernel" => format!("Kernel:{}", r.detail),
                    Err(r) => r.code.to_owned(),
                })
                .collect();
            format!("decided {}", parts.join(" "))
        }
        Outcome::Stall(s) => format!("stall {s:?}"),
        other => format!("{other:?}"),
    }
}

fn run_chain(entries: &[Entry]) -> String {
    let mut r = Replica::new(TEAM, ObjectFormat::Sha1);
    let mut out = String::new();
    for e in entries {
        let o = r.accept(&EntryInput {
            oid: &e.oid,
            seq: e.seq,
            first_in_generation: e.first,
            commit: &e.raw,
            tree: &e.tree,
        });
        out.push_str(&format!("{} {}\n", e.seq, outcome_line(&o)));
    }
    let statuses: Vec<String> = r
        .keyed_state()
        .map(|s| {
            s.records
                .iter()
                .map(|x| {
                    format!(
                        "{}={}",
                        x.path,
                        if x.certified { "certified" } else { "stale" }
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    out.push_str(&format!("status {}\n", statuses.join(" ")));
    out.push_str(&format!(
        "digest {}\n",
        r.digest().map(|d| hex(&d)).unwrap_or_default()
    ));
    out
}

fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut w = Writer::default();
    w.raw(b"CXTG").count(entries.len());
    for e in entries {
        w.str(&e.oid)
            .u64(e.seq)
            .bool(e.first)
            .bytes(&e.raw)
            .count(e.tree.len());
        for (p, b) in &e.tree {
            w.str(p).bytes(b);
        }
    }
    w.0
}

fn decode(bytes: &[u8]) -> Vec<Entry> {
    let mut r = Reader(bytes);
    assert_eq!(r.take(4).unwrap(), b"CXTG");
    let n = r.count(1).unwrap();
    (0..n)
        .map(|_| {
            let oid = r.str().unwrap().to_owned();
            let seq = r.u64().unwrap();
            let first = r.bool().unwrap();
            let raw = r.bytes().unwrap().to_vec();
            let k = r.count(8).unwrap();
            let tree = (0..k)
                .map(|_| (r.str().unwrap().to_owned(), r.bytes().unwrap().to_vec()))
                .collect();
            Entry {
                oid,
                seq,
                first,
                raw,
                tree,
            }
        })
        .collect()
}

#[test]
fn golden_corpus_outcomes_are_frozen() {
    let dir = corpus_dir().join("v1");
    if std::env::var("CONTEXTDB_BLESS").is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(&dir).unwrap();
        for (name, c) in scenarios() {
            let chain = dir.join(format!("{name}.chain"));
            if !chain.exists() {
                std::fs::write(&chain, encode(&c.entries)).unwrap();
                std::fs::write(dir.join(format!("{name}.expected")), run_chain(&c.entries))
                    .unwrap();
            }
        }
    }
    let mut seen = 0;
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .expect("tests/team-chains/v1")
        .map(|e| e.unwrap().path())
        .collect();
    names.sort();
    for path in names
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "chain"))
    {
        let entries = decode(&std::fs::read(&path).unwrap());
        let expected = std::fs::read_to_string(path.with_extension("expected")).unwrap();
        assert_eq!(run_chain(&entries), expected, "{}", path.display());
        seen += 1;
    }
    assert!(
        seen >= scenarios().len(),
        "corpus has {seen} chains; run with CONTEXTDB_BLESS=1 to add new ones"
    );
}
