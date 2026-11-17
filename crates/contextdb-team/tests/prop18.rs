//! PROP18 (ADR 0018 §13): publish then import equals applying the
//! proposals to the replica directly whenever the scratch `accept` passes.
//!
//! Per seed: a random team state from signed entries; random proposals
//! entered into a real queue (both layers); sometimes a new root entry
//! lands after them, so publish rebuilds on a moved tip (§7.3); then the
//! real step 5 (`prepare_on`). The prepared entry, signed by the bound
//! member, is decided by `accept` on one clone of the replica; on another
//! clone the included proposals are applied directly with `scratch_apply`,
//! in queue order. Every included proposal must be accepted, and the two
//! replicas must agree on the keyed state and digest. Proposals left out
//! are the conflicted ones and those waiting for the next entry.
//!
//! Seeds: `CONTEXTDB_PROP18_SEEDS` (default 200; the gate runs 10,000).

use contextdb_model::*;
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::gitobj;
use contextdb_team::ops::{Edge, Hash, RuleSpec, TeamBatch, TeamOp, tags_hash};
use contextdb_team::proposals::{LocalPolicy, Queue, State};
use contextdb_team::publish::{Mode, prepare_on};
use contextdb_team::replica::{EntryInput, Outcome, Replica};
use contextdb_team::sshsig;
use contextdb_team::state::KeyedState;
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::{BTreeMap, BTreeSet};

const TEAM: [u8; 16] = [0x18; 16];
const KEYS: [[u8; 32]; 2] = [[1; 32], [2; 32]];
const MEMBERS: [&str; 2] = ["alice", "bob"];

struct Rng(ChaCha8Rng);
impl Rng {
    fn n(&mut self, n: usize) -> usize {
        self.0.next_u32() as usize % n.max(1)
    }
    fn p(&mut self, pct: u32) -> bool {
        self.0.next_u32() % 100 < pct
    }
}

fn content(i: u32) -> (Hash, Vec<u8>) {
    let b = format!("c{i}").into_bytes();
    (*blake3::hash(&b).as_bytes(), b)
}

/// The replica, its tip, and the next sequence number and proposal id.
struct Team {
    replica: Replica,
    tip: Option<String>,
    next_id: u32,
}

impl Team {
    fn new() -> Self {
        let mut t = Self {
            replica: Replica::new(TEAM, ObjectFormat::Sha1),
            tip: None,
            next_id: 1,
        };
        let doc = TeamDoc::from_toml(&format!(
            "version = 1\naccept_version = 1\nid = \"{}\"\nadmins = [\"alice\"]\n\
             [members.alice]\nkeys = [\"{}\"]\n[members.bob]\nkeys = [\"{}\"]\n\
             [lanes.main]\nwriters = [\"*members\"]\ncurators = [\"*members\"]\n",
            hex(&TEAM),
            sshsig::public_key(&KEYS[0]).to_line(),
            sshsig::public_key(&KEYS[1]).to_line(),
        ))
        .unwrap();
        t.entry(
            0,
            vec![vec![TeamOp::SetTeamAccess { doc: Box::new(doc) }]],
            vec![],
        );
        t
    }
    /// A signed entry by member `m` with these proposals; its decisions.
    fn entry(
        &mut self,
        m: usize,
        proposals: Vec<Vec<TeamOp>>,
        blobs: Vec<(Hash, Vec<u8>)>,
    ) -> Vec<Result<(), String>> {
        let n = proposals.len();
        let batch = TeamBatch { proposals }.encode();
        let seq = self.replica.seq().map_or(0, |s| s + 1);
        let mut tr: Vec<(&str, String)> = vec![
            ("Ctx-Team", hex(&TEAM)),
            ("Ctx-Team-Seq", seq.to_string()),
            ("Ctx-Author", MEMBERS[m].to_owned()),
            ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
            ("Ctx-Accept-Version", "1".to_owned()),
        ];
        for _ in 0..n {
            let mut id = [0u8; 16];
            id[..4].copy_from_slice(&self.next_id.to_be_bytes());
            self.next_id += 1;
            tr.push(("Ctx-Proposal", hex(&id)));
        }
        let mut tree = vec![("batch".to_owned(), batch)];
        let unique: BTreeMap<Hash, Vec<u8>> = blobs.into_iter().collect();
        tree.extend(
            unique
                .into_iter()
                .map(|(h, b)| (format!("blobs/{}", hex(&h)), b)),
        );
        let tree_hex = hex(&gitobj::tree_id(ObjectFormat::Sha1, &tree).unwrap());
        let parents: Vec<String> = self.tip.iter().cloned().collect();
        let unsigned = commit::build(&tree_hex, &parents, "e", &tr);
        let (oid, out) = decide(
            &mut self.replica,
            m,
            &unsigned,
            &tree,
            seq,
            self.tip.is_none(),
        );
        self.tip = Some(oid);
        match out {
            Outcome::Decided(d) => d
                .into_iter()
                .map(|r| r.map_err(|e| e.code.to_owned()))
                .collect(),
            Outcome::EntryRejected(r) => vec![Err(r.code.to_owned()); n],
            other => panic!("{other:?}"),
        }
    }
}

/// Signs `unsigned` as member `m` and decides it on `replica`.
fn decide(
    replica: &mut Replica,
    m: usize,
    unsigned: &[u8],
    tree: &[(String, Vec<u8>)],
    seq: u64,
    first: bool,
) -> (String, Outcome) {
    let raw = commit::with_signature(
        unsigned,
        &sshsig::sign(&KEYS[m], "git", unsigned),
        ObjectFormat::Sha1,
    );
    let oid = hex(&gitobj::object_id(ObjectFormat::Sha1, "commit", &raw).unwrap());
    let out = replica.accept(&EntryInput {
        oid: &oid,
        seq,
        first_in_generation: first,
        commit: &raw,
        tree,
    });
    (oid, out)
}

/// A random proposal over the state: valid most of the time, with stale
/// expectations, existing keys, cycles and the like now and then.
fn gen_ops(rng: &mut Rng, st: &KeyedState, fresh: &mut u32) -> (Vec<TeamOp>, Vec<(Hash, Vec<u8>)>) {
    let mut ops = Vec::new();
    let mut blobs = Vec::new();
    let recs = &st.records;
    for _ in 0..1 + rng.n(3) {
        let pick = |rng: &mut Rng| (!recs.is_empty()).then(|| &recs[rng.n(recs.len())]);
        let op = match rng.n(7) {
            0 => {
                *fresh += 1;
                let (c, b) = content(*fresh);
                blobs.push((c, b));
                let path = match pick(rng) {
                    Some(r) if rng.p(10) => r.path.clone(),
                    _ => format!("k/{}", *fresh),
                };
                TeamOp::New {
                    path,
                    lane: "main".into(),
                    kind: RecordKind::Decision,
                    content: c,
                    adopt_as_base: rng.p(60),
                }
            }
            1 => {
                let Some(r) = pick(rng) else { continue };
                *fresh += 1;
                let (c, b) = content(*fresh);
                blobs.push((c, b));
                TeamOp::Revise {
                    path: r.path.clone(),
                    content: c,
                    adopt_as_base: rng.p(50),
                }
            }
            2 => {
                let Some(r) = pick(rng) else { continue };
                let new = r.revisions[rng.n(r.revisions.len())].content;
                TeamOp::SetHead {
                    path: r.path.clone(),
                    expected: if rng.p(90) { r.head } else { new },
                    new,
                }
            }
            3 => {
                let Some(r) = pick(rng) else { continue };
                let v = &r.revisions[rng.n(r.revisions.len())];
                let expected = if rng.p(90) { v.base } else { !v.base };
                TeamOp::SetBase {
                    path: r.path.clone(),
                    content: v.content,
                    expected,
                    base: !expected,
                }
            }
            4 => {
                let Some(r) = pick(rng) else { continue };
                let mut inc: Vec<(String, Edge)> = (0..1 + rng.n(2))
                    .filter_map(|_| {
                        let o = pick(rng)?;
                        Some((
                            o.path.clone(),
                            match rng.n(3) {
                                0 => Edge::Pin(o.head),
                                1 => Edge::Cur(o.head),
                                _ => Edge::Ref(o.revisions[rng.n(o.revisions.len())].content),
                            },
                        ))
                    })
                    .collect();
                inc.sort();
                inc.dedup();
                TeamOp::AddRule {
                    rule: RuleSpec {
                        conclusion: (r.path.clone(), r.head),
                        incidences: inc,
                    },
                    enabled: rng.p(80),
                }
            }
            5 => {
                let Some(r) = pick(rng) else { continue };
                let expected = if rng.p(90) {
                    tags_hash(&r.tags)
                } else {
                    tags_hash(&BTreeSet::from(["x".to_owned()]))
                };
                TeamOp::SetTags {
                    path: r.path.clone(),
                    expected,
                    tags: [format!("t{}", rng.n(3))].into(),
                }
            }
            _ => {
                let Some(r) = pick(rng) else { continue };
                TeamOp::Expect {
                    path: r.path.clone(),
                    content: if rng.p(90) {
                        r.head
                    } else {
                        r.revisions[rng.n(r.revisions.len())].content
                    },
                }
            }
        };
        ops.push(op);
    }
    (ops, blobs)
}

fn run(seed: u64, stats: &mut BTreeMap<&'static str, usize>) {
    let mut rng = Rng(ChaCha8Rng::seed_from_u64(seed ^ 0x18));
    let mut team = Team::new();
    let mut fresh = 0u32;
    // A random history.
    for _ in 0..rng.n(12) {
        let st = team.replica.keyed_state().unwrap();
        let (ops, blobs) = gen_ops(&mut rng, &st, &mut fresh);
        if !ops.is_empty() {
            team.entry(rng.n(2), vec![ops], blobs);
        }
    }
    // The queue, as member `m` binds it.
    let m = rng.n(2);
    let dir = std::env::temp_dir().join(format!("ctx-prop18-{}-{seed}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut queue = Queue::open(&dir).unwrap();
    let policy = LocalPolicy {
        access: None,
        version: b"v1",
    };
    for _ in 0..1 + rng.n(6) {
        let st = team.replica.keyed_state().unwrap();
        let (ops, blobs) = gen_ops(&mut rng, &st, &mut fresh);
        if ops.is_empty() {
            continue;
        }
        let blobs: BTreeMap<Hash, Vec<u8>> = blobs.into_iter().collect();
        let r = queue.propose(
            &team.replica,
            MEMBERS[m],
            PrincipalId::STORE_OWNER,
            ops,
            blobs,
            &policy,
        );
        *stats
            .entry(if r.is_ok() {
                "enqueued"
            } else {
                "refused-at-enqueue"
            })
            .or_default() += 1;
    }
    // Sometimes the tip moves before publish (§7.3).
    if rng.p(40) {
        let st = team.replica.keyed_state().unwrap();
        let (ops, blobs) = gen_ops(&mut rng, &st, &mut fresh);
        if !ops.is_empty() {
            team.entry(1 - m, vec![ops], blobs);
            *stats.entry("tip-moved").or_default() += 1;
        }
    }
    let queued: BTreeSet<[u8; 16]> = (queue.proposals.iter())
        .filter(|p| p.state == State::Queued)
        .map(|p| p.id)
        .collect();
    let prepared = prepare_on(
        &team.replica,
        team.tip.as_deref(),
        &mut queue,
        MEMBERS[m],
        &policy,
        Mode::Auto,
    )
    .unwrap();
    let conflicted: BTreeSet<[u8; 16]> = prepared.conflicted.iter().map(|(id, _)| *id).collect();
    *stats.entry("conflicted").or_default() += conflicted.len();
    let Some(e) = prepared.entry else {
        assert!(conflicted.is_subset(&queued), "seed {seed}");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let included: BTreeSet<[u8; 16]> = e.proposals.iter().copied().collect();
    assert!(included.is_disjoint(&conflicted), "seed {seed}");
    assert!(included.is_subset(&queued), "seed {seed}");
    // Publish then import.
    let mut imported = team.replica.clone();
    let (_, out) = decide(
        &mut imported,
        m,
        &e.payload,
        &e.tree,
        e.seq,
        e.parent.is_none(),
    );
    let Outcome::Decided(d) = out else {
        panic!("seed {seed}: the prepared entry was not decided: {out:?}")
    };
    assert_eq!(d.len(), e.proposals.len(), "seed {seed}");
    for (id, r) in e.proposals.iter().zip(&d) {
        assert!(
            r.is_ok(),
            "seed {seed}: proposal {} rejected at import: {r:?}",
            hex(id)
        );
    }
    // Applied directly.
    let mut direct = team.replica.clone();
    for id in &e.proposals {
        let p = queue.get(id).unwrap().clone();
        direct
            .scratch_apply(MEMBERS[m], p.id, &p.ops, &queue.blobs_of(&p))
            .unwrap_or_else(|r| panic!("seed {seed}: scratch of an included proposal: {r:?}"));
    }
    // The entry itself takes one sequence number; nothing else differs.
    let (a, mut b) = (
        imported.keyed_state().unwrap(),
        direct.keyed_state().unwrap(),
    );
    assert_eq!(a.seq, e.seq, "seed {seed}: seq");
    assert_eq!(a.records, b.records, "seed {seed}: records");
    assert_eq!(a.rules, b.rules, "seed {seed}: rules");
    assert_eq!(a.accepted, b.accepted, "seed {seed}: accepted ids");
    b.seq = e.seq;
    assert_eq!(a, b, "seed {seed}: keyed state");
    assert_eq!(a.digest(), b.digest(), "seed {seed}: digest");
    *stats.entry("published").or_default() += e.proposals.len();
    *stats.entry("entries").or_default() += 1;
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prop18_publish_then_import_equals_direct_application() {
    let seeds: u64 = std::env::var("CONTEXTDB_PROP18_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let mut stats = BTreeMap::new();
    for seed in 0..seeds {
        run(seed, &mut stats);
    }
    eprintln!("PROP18 over {seeds} seeds: {stats:?}");
    for s in ["published", "conflicted", "tip-moved", "entries"] {
        assert!(
            stats.get(s).copied().unwrap_or(0) > 0,
            "PROP18 never reached {s}"
        );
    }
}
