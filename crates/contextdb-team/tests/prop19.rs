//! PROP19 (ADR 0018 §5.2, §13): random team chains next to random local
//! branch histories — forks, archives, local rules on mirrors, merges — with
//! catch-ups at random times, in random branch order.
//!
//! 1. Every open branch whose `team:` watermark is at entry *s* holds
//!    exactly the projection at *s* (heads, base bits — on the head only,
//!    true iff certified — revisions and tags).
//! 2. Whenever a team record's `(head, base)` changed at some entry between
//!    two of a branch's catch-up positions, the catch-up batches moved that
//!    mirror's head or flipped one of its base bits (no change cancels out
//!    in a net batch: the split rule).
//!
//! Seeds: `CONTEXTDB_PROP19_SEEDS` (default 100; the gate runs 10,000).

use contextdb_core::Kernel;
use contextdb_model::*;
use contextdb_team::catchup::{self, History, Projection};
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::gitobj;
use contextdb_team::ops::{Edge, Hash, RuleSpec, TeamBatch, TeamOp, tags_hash};
use contextdb_team::replica::{EntryInput, Outcome, Replica};
use contextdb_team::sshsig;
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::{BTreeMap, BTreeSet};

const TEAM: [u8; 16] = [0x19; 16];
const ALICE: [u8; 32] = [1; 32];

struct Rng(ChaCha8Rng);
impl Rng {
    fn n(&mut self, n: usize) -> usize {
        self.0.next_u32() as usize % n.max(1)
    }
    fn p(&mut self, pct: u32) -> bool {
        self.0.next_u32() % 100 < pct
    }
}

fn content(i: usize) -> (Hash, (String, Vec<u8>)) {
    let b = format!("c{i}").into_bytes();
    let h = *blake3::hash(&b).as_bytes();
    (h, (format!("blobs/{}", hex(&h)), b))
}

/// The team side: a replica fed signed entries, and its history.
struct Team {
    replica: Replica,
    history: History,
    seq: u64,
    parent: Option<String>,
    next_id: u32,
}

impl Team {
    fn new() -> Self {
        let mut t = Self {
            replica: Replica::new(TEAM, ObjectFormat::Sha1),
            history: History::default(),
            seq: 0,
            parent: None,
            next_id: 1,
        };
        let doc = TeamDoc::from_toml(&format!(
            "version = 1\naccept_version = 1\nid = \"{}\"\nadmins = [\"alice\"]\n\
             [members.alice]\nkeys = [\"{}\"]\n[lanes.main]\nwriters = [\"*members\"]\ncurators = [\"*members\"]\n",
            hex(&TEAM),
            sshsig::public_key(&ALICE).to_line()
        ))
        .unwrap();
        t.entry(vec![TeamOp::SetTeamAccess { doc: Box::new(doc) }], vec![]);
        t
    }
    fn entry(&mut self, ops: Vec<TeamOp>, blobs: Vec<(String, Vec<u8>)>) {
        let batch = TeamBatch {
            proposals: vec![ops],
        }
        .encode();
        let mut id = [0u8; 16];
        id[..4].copy_from_slice(&self.next_id.to_be_bytes());
        self.next_id += 1;
        let tr = vec![
            ("Ctx-Team", hex(&TEAM)),
            ("Ctx-Team-Seq", self.seq.to_string()),
            ("Ctx-Author", "alice".to_owned()),
            ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
            ("Ctx-Accept-Version", "1".to_owned()),
            ("Ctx-Proposal", hex(&id)),
        ];
        let mut tree = vec![("batch".to_owned(), batch)];
        tree.extend(blobs);
        let tree_hex = hex(&gitobj::tree_id(ObjectFormat::Sha1, &tree).unwrap());
        let parents: Vec<String> = self.parent.iter().cloned().collect();
        let unsigned = commit::build(&tree_hex, &parents, "e", &tr);
        let raw = commit::with_signature(
            &unsigned,
            &sshsig::sign(&ALICE, "git", &unsigned),
            ObjectFormat::Sha1,
        );
        let oid = hex(&gitobj::object_id(ObjectFormat::Sha1, "commit", &raw).unwrap());
        let out = self.replica.accept(&EntryInput {
            oid: &oid,
            seq: self.seq,
            first_in_generation: self.parent.is_none(),
            commit: &raw,
            tree: &tree,
        });
        assert!(
            matches!(out, Outcome::Decided(_) | Outcome::EntryRejected(_)),
            "{out:?}"
        );
        self.history
            .record(&oid, catchup::projection(&self.replica));
        self.parent = Some(oid);
        self.seq += 1;
    }
    /// A random entry over four records: creates, revisions, head moves
    /// (often back to an old head), base toggles, Pin rules, tags.
    fn random(&mut self, rng: &mut Rng) {
        let proj = self.history.current().clone();
        let path = format!("k/{}", rng.n(4));
        let (c, blob) = content(rng.n(6));
        let op = match (proj.get(&path), rng.n(6)) {
            (None, _) => TeamOp::New {
                path: path.clone(),
                lane: "main".into(),
                kind: RecordKind::Decision,
                content: c,
                adopt_as_base: rng.p(60),
            },
            (Some(_), 0) => TeamOp::Revise {
                path: path.clone(),
                content: c,
                adopt_as_base: false,
            },
            (Some(st), 1) | (Some(st), 2) => TeamOp::SetHead {
                path: path.clone(),
                expected: st.head,
                new: st.revisions[rng.n(st.revisions.len())],
            },
            (Some(st), 3) => {
                // Toggle the head's base bit (its record's base, at the root).
                let base = self
                    .replica
                    .keyed_state()
                    .unwrap()
                    .records
                    .iter()
                    .find(|r| r.path == path)
                    .unwrap()
                    .revisions
                    .iter()
                    .find(|v| v.content == st.head)
                    .unwrap()
                    .base;
                TeamOp::SetBase {
                    path: path.clone(),
                    content: st.head,
                    expected: base,
                    base: !base,
                }
            }
            (Some(st), 4) => {
                let other = format!("k/{}", rng.n(4));
                match proj.get(&other) {
                    Some(o) if other != path => TeamOp::AddRule {
                        rule: RuleSpec {
                            conclusion: (path.clone(), st.head),
                            incidences: vec![(other, Edge::Pin(o.head))],
                        },
                        enabled: true,
                    },
                    _ => TeamOp::Revise {
                        path: path.clone(),
                        content: c,
                        adopt_as_base: true,
                    },
                }
            }
            (Some(st), _) => TeamOp::SetTags {
                path: path.clone(),
                expected: tags_hash(&st.tags),
                tags: [format!("t{}", rng.n(3))].into(),
            },
        };
        self.entry(vec![op], vec![blob]);
    }
}

fn on(b: BranchId, channel: Channel, ops: Vec<Op>) -> Batch {
    let mut x = Batch::new(ops);
    x.branch = b;
    x.channel = channel;
    x
}

/// Invariant 1 for one branch.
fn check_branch(k: &Kernel, b: BranchId, h: &History, at: &str) {
    let v = k.branch(b).unwrap();
    let db = v.db();
    let Some(w) = db.watermark(&catchup::scope(TEAM)) else {
        // Never caught up: no mirror of the team may be here.
        let any = (0..db.logical_count() as u32)
            .any(|l| db.logical_origin(LogicalId(l)) != Origin::Local);
        assert!(!any, "{at}: mirrors on {b:?} without a watermark");
        return;
    };
    let w = String::from_utf8(w.to_vec()).unwrap();
    let n = h
        .position(Some(&w))
        .unwrap_or_else(|| panic!("{at}: position {w} not in the history"));
    let want: Projection = h.projection_at(n);
    let prefix = format!("team/{}/", hex(&TEAM));
    let mut seen = BTreeSet::new();
    for l in 0..db.logical_count() as u32 {
        let l = LogicalId(l);
        if db.logical_origin(l) == Origin::Local {
            continue;
        }
        let (key, kind, _) = db.logical_info(l).unwrap();
        let path = key.strip_prefix(&prefix).unwrap().to_owned();
        let st = want
            .get(&path)
            .unwrap_or_else(|| panic!("{at}: {path} on {b:?} is not in the projection"));
        let head = db.head(l).unwrap();
        assert_eq!(
            db.revision_content(head).unwrap(),
            st.head,
            "{at}: head of {path} on {b:?}"
        );
        assert_eq!(kind, st.kind);
        let revs: BTreeMap<Hash, bool> = (0..db.revision_count() as u32)
            .map(RevisionId)
            .filter(|r| db.revision_logical(*r) == Some(l))
            .map(|r| (db.revision_content(r).unwrap(), db.is_base(r).unwrap()))
            .collect();
        let want_revs: BTreeSet<Hash> = st.revisions.iter().copied().collect();
        assert_eq!(
            revs.keys().copied().collect::<BTreeSet<_>>(),
            want_revs,
            "{at}: revisions of {path} on {b:?}"
        );
        for (c, base) in &revs {
            assert_eq!(
                *base,
                *c == st.head && st.certified,
                "{at}: base of {path}@{} on {b:?}",
                hex(&c[..4])
            );
        }
        let tags = v.metadata(v.global_logical(l)).unwrap().tags;
        assert_eq!(tags, st.tags, "{at}: tags of {path} on {b:?}");
        seen.insert(path);
    }
    let all: BTreeSet<String> = want.keys().cloned().collect();
    assert_eq!(seen, all, "{at}: mirrors on {b:?}");
}

/// Paths whose `(head, certified)` changed at some entry in `(from, to]`.
fn root_changes(h: &History, from: usize, to: usize) -> BTreeSet<String> {
    let mut value: BTreeMap<String, (Hash, bool)> = h
        .projection_at(from)
        .into_iter()
        .map(|(p, s)| (p, (s.head, s.certified)))
        .collect();
    let mut changed = BTreeSet::new();
    for (_, delta) in &h.entries[from..to] {
        for (p, s) in delta {
            let v = (s.head, s.certified);
            if value.get(p).is_some_and(|old| *old != v) {
                changed.insert(p.clone());
            }
            value.insert(p.clone(), v);
        }
    }
    changed
}

fn run(seed: u64, stats: &mut BTreeMap<&'static str, usize>) {
    let mut rng = Rng(ChaCha8Rng::seed_from_u64(seed ^ 0x19));
    let mut team = Team::new();
    let mut k = Kernel::default();
    let mut local = 0u32;
    for step in 0..60 {
        let at = format!("seed {seed} step {step}");
        let open: Vec<BranchId> = k.branches().filter(|b| !b.archived).map(|b| b.id).collect();
        match rng.n(100) {
            0..=44 => team.random(&mut rng),
            45..=54 if open.len() < 5 => {
                let from = open[rng.n(open.len())];
                let name = format!("b{step}");
                k.commit(&on(from, Channel::Admin, vec![Op::CreateBranch { name }]))
                    .unwrap();
            }
            55..=57 if open.len() > 1 => {
                let b = open[1 + rng.n(open.len() - 1)];
                let name = k.branch(b).unwrap().info().name.clone();
                k.commit(&on(b, Channel::Admin, vec![Op::ArchiveBranch { name }]))
                    .unwrap();
            }
            58..=69 => {
                // A local record depending on a visible mirror revision.
                let b = open[rng.n(open.len())];
                let v = k.branch(b).unwrap();
                let db = v.db();
                let mirrors: Vec<RevisionId> = (0..db.revision_count() as u32)
                    .map(RevisionId)
                    .filter(|r| {
                        db.logical_origin(db.revision_logical(*r).unwrap()) != Origin::Local
                    })
                    .map(|r| v.global_revision(r))
                    .collect();
                local += 1;
                let mut ops = vec![Op::CreateLogical {
                    key: format!("dec:l{local}"),
                    kind: RecordKind::Decision,
                    scope_lane: "repo".into(),
                    content: [local as u8; 32],
                    adopt_as_base: false,
                    origin: Origin::Local,
                }];
                if !mirrors.is_empty() {
                    let m = mirrors[rng.n(mirrors.len())];
                    let edge = [EdgeKind::Pin, EdgeKind::Cur, EdgeKind::Ref][rng.n(3)];
                    ops.push(Op::AddRule {
                        conclusion: RevRef::New(0),
                        incidences: vec![(m.into(), edge)],
                        enabled: true,
                    });
                }
                let _ = k.commit(&on(b, Channel::Agent, ops));
            }
            70..=77 if open.len() > 1 => {
                let (s, t) = (open[rng.n(open.len())], open[rng.n(open.len())]);
                let r = k.commit(&on(
                    t,
                    Channel::Agent,
                    vec![Op::Merge {
                        source: s,
                        require_certified: false,
                        certify: Vec::new(),
                    }],
                ));
                *stats
                    .entry(match r {
                        Ok(_) => "merge",
                        Err(TxError::MirrorLag) => "mirror-lag",
                        Err(_) => "merge-refused",
                    })
                    .or_default() += 1;
            }
            _ => {
                // Catch up: everything, or one branch at a time in a random
                // order (another branch first may create a mirror main then
                // attaches).
                let mut targets = open.clone();
                if rng.p(50) {
                    targets = vec![open[rng.n(open.len())]];
                }
                for b in targets {
                    let before = k
                        .branch(b)
                        .unwrap()
                        .db()
                        .watermark(&catchup::scope(TEAM))
                        .map(|w| String::from_utf8(w.to_vec()).unwrap());
                    let from = team.history.position(before.as_deref()).unwrap_or(0);
                    let done = catchup::catch_up(&mut k, b, TEAM, &team.history)
                        .unwrap_or_else(|e| panic!("{at}: catch-up of {b:?}: {e:?}"));
                    *stats.entry("batches").or_default() += done.len();
                    if done.len() > 1 {
                        *stats.entry("split").or_default() += 1;
                    }
                    // Invariant 2: every root change shows on the branch.
                    let changed = root_changes(&team.history, from, team.history.entries.len());
                    let v = k.branch(b).unwrap();
                    let mut moved = BTreeSet::new();
                    for r in &done {
                        for (l, _, _) in &r.net_delta.head_moves {
                            moved.insert(*l);
                        }
                        for rev in &r.net_delta.base_flips {
                            if let Some(l) = v.revision_logical(*rev) {
                                moved.insert(l);
                            }
                        }
                    }
                    let db = v.db();
                    let prefix = format!("team/{}/", hex(&TEAM));
                    let moved_paths: BTreeSet<String> = moved
                        .into_iter()
                        .filter_map(|g| {
                            let l = (0..db.logical_count() as u32)
                                .map(LogicalId)
                                .find(|l| v.global_logical(*l) == g)?;
                            Some(db.logical_info(l)?.0.strip_prefix(&prefix)?.to_owned())
                        })
                        .collect();
                    for p in &changed {
                        assert!(
                            moved_paths.contains(p),
                            "{at}: root changed {p} but catch-up of {b:?} shows no change"
                        );
                        *stats.entry("root-changes-seen").or_default() += 1;
                    }
                }
            }
        }
        for b in k
            .branches()
            .filter(|b| !b.archived)
            .map(|b| b.id)
            .collect::<Vec<_>>()
        {
            check_branch(&k, b, &team.history, &at);
        }
    }
}

#[test]
fn prop19_mirrors_equal_the_projection_and_keep_their_history() {
    let seeds: u64 = std::env::var("CONTEXTDB_PROP19_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let mut stats = BTreeMap::new();
    for seed in 0..seeds {
        run(seed, &mut stats);
    }
    eprintln!("PROP19 over {seeds} seeds: {stats:?}");
    if seeds >= 100 {
        for s in ["batches", "split", "merge", "root-changes-seen"] {
            assert!(
                stats.get(s).copied().unwrap_or(0) > 0,
                "PROP19 never reached {s}"
            );
        }
    }
}

/// A catch-up writer that dies after `left` batches (a process kill):
/// committed batches stay (they are durable in the op_log), the rest never
/// happen.
struct Killed<'a> {
    k: &'a mut Kernel,
    left: usize,
}
impl catchup::Writer for Killed<'_> {
    type Error = TxError;
    fn kernel(&self) -> &Kernel {
        self.k
    }
    fn commit(&mut self, b: &Batch) -> Result<CommitResult, TxError> {
        if self.left == 0 {
            return Err(TxError::UnknownRef);
        }
        self.left -= 1;
        self.k.commit(b)
    }
}

/// T114, §8.4's rows "replica ahead of a branch's mirrors" and "some
/// branches written, others not", by kill point: from random branch states
/// (forks at different team positions, local dependents of mirrors), the
/// catch-up of every open branch is killed after each of its batches in
/// turn, then run again as at the next start. Every branch then equals the
/// uninterrupted run's, digest for digest, and holds the projection at the
/// tip. Seeds: `CONTEXTDB_KILL_SEEDS` (default 200).
#[test]
fn t114_a_catch_up_killed_after_any_batch_recovers_on_restart() {
    let seeds: u64 = std::env::var("CONTEXTDB_KILL_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let (mut points, mut multi) = (0usize, 0usize);
    for seed in 0..seeds {
        let mut rng = Rng(ChaCha8Rng::seed_from_u64(seed ^ 0x114));
        let mut team = Team::new();
        let mut k = Kernel::default();
        let mut local = 0u32;
        for step in 0..30 {
            let open: Vec<BranchId> = k.branches().filter(|b| !b.archived).map(|b| b.id).collect();
            match rng.n(10) {
                0..=4 => team.random(&mut rng),
                5 if open.len() < 4 => {
                    let from = open[rng.n(open.len())];
                    k.commit(&on(
                        from,
                        Channel::Admin,
                        vec![Op::CreateBranch {
                            name: format!("b{step}"),
                        }],
                    ))
                    .unwrap();
                }
                6 | 7 => {
                    let b = open[rng.n(open.len())];
                    catchup::catch_up(&mut k, b, TEAM, &team.history).unwrap();
                }
                _ => {
                    let b = open[rng.n(open.len())];
                    let v = k.branch(b).unwrap();
                    let db = v.db();
                    let mirrors: Vec<RevisionId> = (0..db.revision_count() as u32)
                        .map(RevisionId)
                        .filter(|r| {
                            db.logical_origin(db.revision_logical(*r).unwrap()) != Origin::Local
                        })
                        .map(|r| v.global_revision(r))
                        .collect();
                    if mirrors.is_empty() {
                        continue;
                    }
                    local += 1;
                    let m = mirrors[rng.n(mirrors.len())];
                    let edge = [EdgeKind::Pin, EdgeKind::Cur, EdgeKind::Ref][rng.n(3)];
                    let _ = k.commit(&on(
                        b,
                        Channel::Agent,
                        vec![
                            Op::CreateLogical {
                                key: format!("dec:l{local}"),
                                kind: RecordKind::Decision,
                                scope_lane: "repo".into(),
                                content: [local as u8; 32],
                                adopt_as_base: false,
                                origin: Origin::Local,
                            },
                            Op::AddRule {
                                conclusion: RevRef::New(0),
                                incidences: vec![(m.into(), edge)],
                                enabled: true,
                            },
                        ],
                    ));
                }
            }
        }
        team.random(&mut rng);
        let open: Vec<BranchId> = k.branches().filter(|b| !b.archived).map(|b| b.id).collect();
        let mut reference = k.clone();
        let total = catchup::catch_up_all(&mut reference, TEAM, &team.history).unwrap();
        if total > 1 {
            multi += 1;
        }
        for kill in 0..total {
            let at = format!("seed {seed} killed after batch {kill} of {total}");
            let mut c = k.clone();
            let r = catchup::catch_up_all(
                &mut Killed {
                    k: &mut c,
                    left: kill,
                },
                TEAM,
                &team.history,
            );
            assert!(r.is_err(), "{at}: the kill did not happen");
            catchup::catch_up_all(&mut c, TEAM, &team.history)
                .unwrap_or_else(|e| panic!("{at}: restart: {e:?}"));
            for &b in &open {
                assert_eq!(
                    c.branch(b).unwrap().digest(),
                    reference.branch(b).unwrap().digest(),
                    "{at}: {b:?}"
                );
                check_branch(&c, b, &team.history, &at);
            }
            points += 1;
        }
    }
    eprintln!("kill points: {points} over {seeds} seeds ({multi} with more than one batch)");
    assert!(points > 0 && multi > 0);
}
