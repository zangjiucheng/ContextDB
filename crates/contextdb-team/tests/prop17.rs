//! PROP17 (ADR 0018 §13, G3): random signed chains from several members,
//! valid and invalid entries mixed (invalid ones followed by valid ones),
//! reference-only rules and Pins to old revisions among the rules. Every
//! entry's outcome and the keyed state after it must equal an independent
//! oracle (`contextdb_oracle::team`), and two edges must agree on decisions
//! and digest. Seeds: `CONTEXTDB_PROP17_SEEDS` (default 200; the gate runs
//! 10,000 on the remote host).

use contextdb_model::RecordKind;
use contextdb_oracle::team::{ODoc, OEdge, OOp, ORule, TeamOracle};
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::ops::{Edge, Hash, RuleSpec, TeamBatch, TeamOp, tags_hash};
use contextdb_team::replica::{EntryInput, Outcome, Replica};
use contextdb_team::sshsig;
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::{BTreeMap, BTreeSet};

const TEAM: [u8; 16] = [0x17; 16];
const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const MEMBERS: [(&str, u8); 3] = [("alice", 1), ("bob", 2), ("carol", 3)];
const KINDS: [RecordKind; 5] = [
    RecordKind::Decision,
    RecordKind::Plan,
    RecordKind::Fact,
    RecordKind::Convention,
    RecordKind::Evidence,
];

fn sk(b: u8) -> [u8; 32] {
    [b; 32]
}

/// Three document variants; lane `l3` exists only in the last.
fn team_doc(variant: u32) -> TeamDoc {
    let keys: String = MEMBERS
        .iter()
        .map(|(m, b)| {
            format!(
                "[members.{m}]\nkeys = [\"{}\"]\n",
                sshsig::public_key(&sk(*b)).to_line()
            )
        })
        .collect();
    let (l2w, l2c, deps, l3) = match variant {
        0 => ("[\"alice\", \"bob\"]", "[\"*admins\"]", "[]", ""),
        1 => ("[\"*members\"]", "[\"alice\", \"carol\"]", "[\"plan\"]", ""),
        _ => (
            "[\"alice\", \"bob\"]",
            "[\"*admins\"]",
            "[]",
            "[lanes.l3]\nwriters = [\"carol\"]\ncurators = [\"carol\"]\n",
        ),
    };
    TeamDoc::from_toml(&format!(
        "version = 1\naccept_version = 1\nid = \"{}\"\nadmins = [\"alice\"]\n{keys}\
         [lanes.l1]\nwriters = [\"*members\"]\ncurators = [\"alice\"]\n\
         [lanes.l2]\nwriters = {l2w}\ncurators = {l2c}\n{l3}\
         [policy]\nrequire_deps = {deps}\n",
        hex(&TEAM)
    ))
    .unwrap()
}

/// The oracle's document: roles expanded to member sets independently.
fn odoc(d: &TeamDoc) -> ODoc {
    let members: BTreeSet<String> = d.members.keys().cloned().collect();
    let expand = |s: &BTreeSet<String>| -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for w in s {
            match w.as_str() {
                "*members" => out.extend(members.iter().cloned()),
                "*admins" => out.extend(d.admins.iter().cloned()),
                m => {
                    out.insert(m.to_owned());
                }
            }
        }
        out
    };
    ODoc {
        admins: d.admins.clone(),
        members: members.clone(),
        writers: d
            .lanes
            .iter()
            .map(|(l, x)| (l.clone(), expand(&x.writers)))
            .collect(),
        curators: d
            .lanes
            .iter()
            .map(|(l, x)| (l.clone(), expand(&x.curators)))
            .collect(),
        require_deps: d.require_deps.clone(),
    }
}

fn oedge(e: Edge) -> OEdge {
    match e {
        Edge::Pin(h) => OEdge::Pin(h),
        Edge::Cur(h) => OEdge::Cur(h),
        Edge::Ref(h) => OEdge::Ref(h),
    }
}

fn orule(r: &RuleSpec) -> ORule {
    ORule {
        conclusion: r.conclusion.clone(),
        incidences: r
            .incidences
            .iter()
            .map(|(p, e)| (p.clone(), oedge(*e)))
            .collect(),
    }
}

fn oop(
    op: &TeamOp,
    tags_now: &BTreeMap<String, BTreeSet<String>>,
    expected_tags: &BTreeMap<Hash, BTreeSet<String>>,
) -> OOp {
    match op {
        TeamOp::New {
            path,
            lane,
            kind,
            content,
            adopt_as_base,
        } => OOp::New {
            path: path.clone(),
            lane: lane.clone(),
            kind: *kind,
            content: *content,
            base: *adopt_as_base,
        },
        TeamOp::Revise {
            path,
            content,
            adopt_as_base,
        } => OOp::Revise {
            path: path.clone(),
            content: *content,
            base: *adopt_as_base,
        },
        TeamOp::SetHead {
            path,
            expected,
            new,
        } => OOp::SetHead {
            path: path.clone(),
            expected: *expected,
            new: *new,
        },
        TeamOp::SetBase {
            path,
            content,
            expected,
            base,
        } => OOp::SetBase {
            path: path.clone(),
            content: *content,
            expected: *expected,
            base: *base,
        },
        TeamOp::AddRule { rule, enabled } => OOp::AddRule {
            rule: orule(rule),
            enabled: *enabled,
        },
        TeamOp::SetRuleEnabled {
            rule,
            expected,
            enabled,
        } => OOp::SetRuleEnabled {
            rule: orule(rule),
            expected: *expected,
            enabled: *enabled,
        },
        TeamOp::SetTags {
            path,
            expected,
            tags,
        } => OOp::SetTags {
            path: path.clone(),
            // The oracle compares sets; an expectation no set matches is a set
            // no record can hold.
            expected: expected_tags.get(expected).cloned().unwrap_or_else(|| {
                let _ = tags_now;
                ["<none>".to_owned()].into()
            }),
            tags: tags.clone(),
        },
        TeamOp::Expect { path, content } => OOp::Expect {
            path: path.clone(),
            content: *content,
        },
        TeamOp::SetTeamAccess { doc } => OOp::SetTeamAccess { doc: odoc(doc) },
        TeamOp::TeamShred { content } => OOp::TeamShred { content: *content },
        TeamOp::Compact { .. } => unreachable!("not generated in milestone 1"),
    }
}

struct Rng(ChaCha8Rng);
impl Rng {
    fn n(&mut self, n: u32) -> u32 {
        self.0.next_u32() % n
    }
    fn p(&mut self, pct: u32) -> bool {
        self.n(100) < pct
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> Option<&'a T> {
        (!v.is_empty()).then(|| &v[self.n(v.len() as u32) as usize])
    }
}

fn content(i: u32) -> (Hash, Vec<u8>) {
    let bytes = format!("content {i}").into_bytes();
    (*blake3::hash(&bytes).as_bytes(), bytes)
}

/// Random ops built from the oracle's view, so most are plausible and some
/// carry stale expectations, unknown names or forbidden shapes.
fn gen_ops(rng: &mut Rng, o: &TeamOracle, used: &mut BTreeSet<u32>) -> Vec<TeamOp> {
    let recs = o.records();
    let rules: Vec<ORule> = o.rules().into_keys().collect();
    let paths: Vec<String> = recs.keys().cloned().collect();
    let any_path = |rng: &mut Rng| -> String {
        if rng.p(80)
            && let Some(p) = rng.pick(&paths)
        {
            return p.clone();
        }
        format!("k/{}", rng.n(6))
    };
    let mut c = |rng: &mut Rng| -> Hash {
        let i = rng.n(14);
        used.insert(i);
        content(i).0
    };
    let rev_of = |rng: &mut Rng, p: &str, fallback: Hash| -> Hash {
        recs.get(p)
            .and_then(|r| rng.pick(&r.revisions).map(|v| v.0))
            .unwrap_or(fallback)
    };
    let mut ops = Vec::new();
    for _ in 0..1 + rng.n(3) {
        let op = match rng.n(19) {
            0..=3 => TeamOp::New {
                path: format!("k/{}", rng.n(6)),
                lane: ["l1", "l2", "l3"][rng.n(3) as usize].into(),
                kind: KINDS[rng.n(5) as usize],
                content: c(rng),
                adopt_as_base: rng.p(40),
            },
            4..=5 => TeamOp::Revise {
                path: any_path(rng),
                content: c(rng),
                adopt_as_base: rng.p(25),
            },
            6..=7 => {
                let p = any_path(rng);
                let head = recs.get(&p).map(|r| r.head).unwrap_or([0; 32]);
                let fb = c(rng);
                TeamOp::SetHead {
                    path: p.clone(),
                    expected: if rng.p(85) { head } else { fb },
                    new: rev_of(rng, &p, fb),
                }
            }
            8..=9 => {
                let p = any_path(rng);
                let fb = c(rng);
                let h = rev_of(rng, &p, fb);
                let now = recs
                    .get(&p)
                    .and_then(|r| r.revisions.iter().find(|v| v.0 == h))
                    .is_some_and(|v| v.1);
                TeamOp::SetBase {
                    path: p,
                    content: h,
                    expected: if rng.p(85) { now } else { !now },
                    base: rng.p(60),
                }
            }
            10..=12 => {
                let cp = any_path(rng);
                let fb = c(rng);
                let ch = rev_of(rng, &cp, fb);
                let mut inc = BTreeSet::new();
                for _ in 0..rng.n(4) {
                    let p = any_path(rng);
                    let fb = c(rng);
                    let head = recs.get(&p).map(|r| r.head).unwrap_or(fb);
                    let e = match rng.n(3) {
                        0 => Edge::Pin(rev_of(rng, &p, fb)),
                        1 => Edge::Cur(if rng.p(85) { head } else { fb }),
                        _ => Edge::Ref(rev_of(rng, &p, fb)),
                    };
                    inc.insert((p, e));
                }
                TeamOp::AddRule {
                    rule: RuleSpec {
                        conclusion: (cp, ch),
                        incidences: inc.into_iter().collect(),
                    },
                    enabled: rng.p(85),
                }
            }
            13 => match rng.pick(&rules).cloned() {
                Some(r) => {
                    let now = o.rules()[&r];
                    let spec = RuleSpec {
                        conclusion: r.conclusion.clone(),
                        incidences: r
                            .incidences
                            .iter()
                            .map(|(p, e)| {
                                (
                                    p.clone(),
                                    match e {
                                        OEdge::Pin(h) => Edge::Pin(*h),
                                        OEdge::Cur(h) => Edge::Cur(*h),
                                        OEdge::Ref(h) => Edge::Ref(*h),
                                    },
                                )
                            })
                            .collect(),
                    };
                    TeamOp::SetRuleEnabled {
                        rule: spec,
                        expected: if rng.p(85) { now } else { !now },
                        enabled: rng.p(50),
                    }
                }
                None => TeamOp::Expect {
                    path: any_path(rng),
                    content: c(rng),
                },
            },
            14..=15 => {
                let p = any_path(rng);
                let now = recs.get(&p).map(|r| r.tags.clone()).unwrap_or_default();
                let pool = ["a", "b", "c"];
                let tags: BTreeSet<String> = pool
                    .iter()
                    .filter(|_| rng.p(50))
                    .map(|s| (*s).to_owned())
                    .collect();
                let expected = if rng.p(85) {
                    tags_hash(&now)
                } else {
                    tags_hash(&["zz".to_owned()].into())
                };
                TeamOp::SetTags {
                    path: p,
                    expected,
                    tags,
                }
            }
            16..=17 => {
                let p = any_path(rng);
                let head = recs.get(&p).map(|r| r.head).unwrap_or([0; 32]);
                let fb = c(rng);
                TeamOp::Expect {
                    path: p,
                    content: if rng.p(85) { head } else { fb },
                }
            }
            _ => TeamOp::TeamShred { content: c(rng) },
        };
        ops.push(op);
    }
    ops
}

enum Fault {
    None,
    BadSeq,
    Forged,
    Outsider,
    NoBlobs,
}

fn run(seed: u64, hist: &mut BTreeMap<String, u64>) {
    let mut rng = Rng(ChaCha8Rng::seed_from_u64(seed));
    let mut oracle = TeamOracle::new(&hex(&TEAM));
    let mut edges = [
        Replica::new(TEAM, ObjectFormat::Sha1),
        Replica::new(TEAM, ObjectFormat::Sha1),
    ];
    let mut parent: Option<String> = None;
    let mut next_id: u32 = 1;
    let mut accepted_ids: Vec<(String, [u8; 16])> = Vec::new();
    let mut tags_seen: BTreeMap<Hash, BTreeSet<String>> = BTreeMap::new();
    let entries = 6 + rng.n(20) as u64;
    for seq in 0..entries {
        let fresh_id = |next_id: &mut u32| {
            let mut id = [0u8; 16];
            id[..4].copy_from_slice(&next_id.to_be_bytes());
            id[4..12].copy_from_slice(&seed.to_be_bytes());
            *next_id += 1;
            id
        };
        // Who signs, what, and which fault (none for genesis).
        let (mut author, mut key) = ("alice", sk(1));
        let mut used = BTreeSet::new();
        let proposals: Vec<Vec<TeamOp>> = if seq == 0 {
            vec![vec![TeamOp::SetTeamAccess {
                doc: Box::new(team_doc(0)),
            }]]
        } else if rng.p(5) {
            vec![vec![TeamOp::SetTeamAccess {
                doc: Box::new(team_doc(rng.n(3))),
            }]]
        } else {
            let (m, b) = MEMBERS[rng.n(3) as usize];
            (author, key) = (m, sk(b));
            (0..1 + rng.n(3))
                .map(|_| gen_ops(&mut rng, &oracle, &mut used))
                .collect()
        };
        let fault = if seq == 0 {
            Fault::None
        } else {
            match rng.n(100) {
                0..=2 => Fault::BadSeq,
                3..=5 => Fault::Forged,
                6..=7 => Fault::Outsider,
                8..=11 => Fault::NoBlobs,
                _ => Fault::None,
            }
        };
        let mut ids: Vec<[u8; 16]> = proposals.iter().map(|_| fresh_id(&mut next_id)).collect();
        // Sometimes replay an id this author already had accepted.
        if seq > 0
            && rng.p(5)
            && let Some((_, id)) = accepted_ids.iter().find(|(m, _)| m == author)
        {
            ids[0] = *id;
        }
        match fault {
            Fault::Forged => key = sk(if author == "bob" { 3 } else { 2 }),
            Fault::Outsider => (author, key) = ("dave", sk(9)),
            _ => {}
        }
        let batch = TeamBatch {
            proposals: proposals.clone(),
        }
        .encode();
        let mut trailers: Vec<(&str, String)> = vec![
            ("Ctx-Team", hex(&TEAM)),
            (
                "Ctx-Team-Seq",
                if matches!(fault, Fault::BadSeq) {
                    (seq + 7).to_string()
                } else {
                    seq.to_string()
                },
            ),
            ("Ctx-Author", author.into()),
            ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
            ("Ctx-Accept-Version", "1".into()),
        ];
        for id in &ids {
            trailers.push(("Ctx-Proposal", hex(id)));
        }
        let parents: Vec<String> = parent.iter().cloned().collect();
        let unsigned = commit::build(TREE, &parents, "ctx team: entry", &trailers);
        let raw = commit::with_signature(
            &unsigned,
            &sshsig::sign(&key, "git", &unsigned),
            ObjectFormat::Sha1,
        );
        parent = Some(hex(&blake3::hash(&raw).as_bytes()[..20]));
        let mut tree = vec![("batch".to_owned(), batch)];
        let mut carried = BTreeSet::new();
        if !matches!(fault, Fault::NoBlobs) {
            for i in &used {
                let (h, bytes) = content(*i);
                carried.insert(h);
                tree.push((format!("blobs/{}", hex(&h)), bytes));
            }
        }
        let input = EntryInput {
            seq,
            first_in_generation: seq == 0,
            commit: &raw,
            tree: &tree,
        };
        let outcomes: Vec<Outcome> = edges.iter_mut().map(|r| r.accept(&input)).collect();
        assert_eq!(
            outcomes[0], outcomes[1],
            "seed {seed} seq {seq}: edges disagree"
        );
        let structural = matches!(fault, Fault::BadSeq | Fault::Forged | Fault::Outsider);
        match &outcomes[0] {
            Outcome::EntryRejected(r) => {
                assert!(
                    structural,
                    "seed {seed} seq {seq}: unexpected entry rejection {r:?}"
                );
                *hist.entry(format!("entry:{}", r.code)).or_default() += 1;
                oracle.reject_entry(seq);
            }
            Outcome::Decided(ds) => {
                assert!(!structural, "seed {seed} seq {seq}: fault not detected");
                for t in edges[0].keyed_state().iter().flat_map(|s| s.records.iter()) {
                    tags_seen.insert(tags_hash(&t.tags), t.tags.clone());
                }
                tags_seen.insert(tags_hash(&BTreeSet::new()), BTreeSet::new());
                let tags_now = BTreeMap::new();
                let oprops: Vec<([u8; 16], Vec<OOp>)> = ids
                    .iter()
                    .zip(&proposals)
                    .map(|(id, ops)| {
                        (
                            *id,
                            ops.iter()
                                .map(|op| oop(op, &tags_now, &tags_seen))
                                .collect(),
                        )
                    })
                    .collect();
                let want = oracle.entry(seq, author, &oprops, &carried);
                let got: Vec<Result<(), String>> = ds
                    .iter()
                    .map(|d| {
                        d.clone().map_err(|r| {
                            if r.code == "Kernel" {
                                format!("Kernel:{}", r.detail)
                            } else {
                                r.code.to_owned()
                            }
                        })
                    })
                    .collect();
                assert_eq!(
                    got, want,
                    "seed {seed} seq {seq}: decisions differ\n{proposals:?}"
                );
                for g in &got {
                    *hist
                        .entry(g.clone().err().unwrap_or_else(|| "Ok".into()))
                        .or_default() += 1;
                }
                for r in edges[0].keyed_state().iter().flat_map(|s| s.records.iter()) {
                    *hist.entry(format!("status:{}", r.certified)).or_default() += 1;
                }
                for (id, d) in ids.iter().zip(ds) {
                    if d.is_ok() {
                        accepted_ids.push((author.to_owned(), *id));
                    }
                }
            }
            Outcome::Stall(s) => panic!("seed {seed} seq {seq}: stall {s:?}"),
        }
        // The keyed state equals the oracle's after every entry.
        let state = edges[0].keyed_state().expect("after genesis");
        let orecs = oracle.records();
        assert_eq!(
            state.records.len(),
            orecs.len(),
            "seed {seed} seq {seq}: record count"
        );
        for r in &state.records {
            let o = &orecs[&r.path];
            assert_eq!(
                (&r.lane, r.kind, &r.tags, r.head),
                (&o.lane, o.kind, &o.tags, o.head),
                "seed {seed} seq {seq}: {}",
                r.path
            );
            let revs: Vec<(Hash, bool, bool)> = r
                .revisions
                .iter()
                .map(|v| (v.content, v.base, v.shredded))
                .collect();
            assert_eq!(
                revs, o.revisions,
                "seed {seed} seq {seq}: revisions of {}",
                r.path
            );
            assert_eq!(
                r.certified, o.certified,
                "seed {seed} seq {seq}: team status of {}",
                r.path
            );
        }
        let orules: BTreeMap<RuleSpec, bool> = oracle
            .rules()
            .into_iter()
            .map(|(r, e)| {
                (
                    RuleSpec {
                        conclusion: r.conclusion,
                        incidences: r
                            .incidences
                            .into_iter()
                            .map(|(p, e)| {
                                (
                                    p,
                                    match e {
                                        OEdge::Pin(h) => Edge::Pin(h),
                                        OEdge::Cur(h) => Edge::Cur(h),
                                        OEdge::Ref(h) => Edge::Ref(h),
                                    },
                                )
                            })
                            .collect(),
                    },
                    e,
                )
            })
            .collect();
        assert_eq!(state.rules, orules, "seed {seed} seq {seq}: rules");
        assert_eq!(&state.shred, oracle.shred(), "seed {seed} seq {seq}: shred");
        assert_eq!(
            &state.accepted,
            oracle.accepted(),
            "seed {seed} seq {seq}: accepted ids"
        );
        assert_eq!(
            Some(state.seq),
            oracle.seq(),
            "seed {seed} seq {seq}: sequence"
        );
        assert_eq!(edges[0].digest(), edges[1].digest());
    }
}

#[test]
fn prop17_random_chains_match_the_oracle() {
    let seeds: u64 = std::env::var("CONTEXTDB_PROP17_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let mut hist = BTreeMap::new();
    for seed in 0..seeds {
        run(seed, &mut hist);
    }
    eprintln!("PROP17 outcomes over {seeds} seeds: {hist:?}");
    // The generator must reach every important path, or the run proves little.
    if seeds >= 200 {
        for code in [
            "Ok",
            "HeadConflict",
            "BaseConflict",
            "CurMoved",
            "TagsConflict",
            "ReadSetConflict",
            "Unauthorized(Write)",
            "Unauthorized(Curate)",
            "LogicalExists",
            "UnknownKey",
            "MissingBlob",
            "ContentShredded",
            "DuplicateProposal",
            "AdminOnly",
            "Kernel:InvalidRule",
            "Kernel:ImmutableKind",
            "entry:Seq",
            "entry:Signature",
            "entry:NotTeamMember",
            "status:true",
            "status:false",
        ] {
            assert!(
                hist.get(code).copied().unwrap_or(0) > 0,
                "PROP17 never reached {code}"
            );
        }
    }
}
