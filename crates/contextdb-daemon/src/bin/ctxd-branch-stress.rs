//! `ctxd-branch-stress <clients> <seconds> [dir]` (ADR 0013 §11 G4): a real
//! daemon on local disk and N socket clients creating branches, committing
//! on them and merging them both ways. Invariants:
//! - coupled heads per branch: `pair-a` and `pair-b` always move in one
//!   batch, and a merge classifies them alike, so a recall on any branch
//!   never shows them out of step (I4 across branches);
//! - linearizable CAS: replaying the log, every successful CAS moved its
//!   branch's head from its `expected` to its new revision in its own epoch
//!   (merges move heads as well: a conflict kept on one side propagates
//!   back on the reverse merge, as in any three-way merge);
//! - racing `CreateBranch` of one name succeeds at most once, and the
//!   branch list has every name that succeeded;
//! - every attempt has exactly one outcome;
//! - replay: the coupled heads agree on every branch at every epoch, sampled
//!   branch reads equal the replayed kernel at their epoch, and the replay
//!   equals the reopened store.
//!
//! Exit code 1 on any violation.
use contextdb_core::Kernel;
use contextdb_daemon::{
    proto::*,
    server::{Client, Daemon, Options},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, StdDisk, Store};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const BRANCHES: [&str; 4] = ["main", "s0", "s1", "s2"];
const CAS_LOGICALS: u32 = 4;
const RACE_NAMES: u64 = 24;
#[derive(Default)]
struct Counts {
    reads: AtomicU64,
    attempts: AtomicU64,
    successes: AtomicU64,
    conflicts: AtomicU64,
    other: AtomicU64,
    coupled_moves: AtomicU64,
    coupled_conflicts: AtomicU64,
    merges: AtomicU64,
    merge_conflicts: AtomicU64,
    merge_refused: AtomicU64,
    creates: AtomicU64,
    create_exists: AtomicU64,
    violations: AtomicU64,
}
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}
type CasStep = (usize, LogicalId, RevisionId, RevisionId, Epoch);
type Sample = (Epoch, BranchId, RevisionId, NodeState);
fn session(c: &mut Client, branch: &str) -> SessionId {
    let r = c.call(&Request::SessionStart {
        harness: "stress".into(),
        repo: "r".into(),
        head_sha: "h".into(),
        parent: None,
        branch: (branch != "main").then(|| branch.to_owned()),
        git_branch: None,
    });
    match r.map(|e| e.response) {
        Ok(Response::Session { session }) => session,
        other => panic!("session on {branch}: {other:?}"),
    }
}
fn read(c: &mut Client, s: SessionId, key: &str) -> Option<(Epoch, Item)> {
    let env = c
        .call(&Request::Read {
            binding: Binding::root(s),
            target: Target::Key(key.into()),
            mode: Mode::Advisory,
        })
        .ok()?;
    match env.response {
        Response::Read { item, .. } => Some((env.epoch, item)),
        _ => None,
    }
}
fn record(c: &mut Client, s: SessionId, key: &str) {
    let r = c.call(&Request::Record {
        binding: Binding::root(s),
        kind: RecordKind::Fact,
        body: format!("{key} 0").into_bytes(),
        target: WriteTarget::Create {
            key: key.into(),
            scope_lane: "repo".into(),
        },
        deps: Vec::new(),
        adopt_as_base: true,
        client_txn_id: None,
    });
    assert!(
        matches!(
            r.as_ref().map(|e| &e.response),
            Ok(Response::Committed { .. })
        ),
        "{key}: {r:?}"
    );
}
fn branch_ids(c: &mut Client) -> BTreeMap<String, BranchId> {
    match c.call(&Request::BranchList).map(|e| e.response) {
        Ok(Response::Branches { branches }) => {
            branches.into_iter().map(|b| (b.name, b.id)).collect()
        }
        other => panic!("{other:?}"),
    }
}
fn digests(k: &Kernel) -> Vec<ContentHash> {
    k.branches()
        .map(|b| k.branch(b.id).unwrap().digest())
        .collect()
}
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let clients: usize = args.first().and_then(|v| v.parse().ok()).unwrap_or(8);
    let seconds: u64 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(10);
    let dir = args.get(2).map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir().join(format!("ctxd-branch-stress-{}", std::process::id()))
    });
    let _ = std::fs::remove_dir_all(&dir);
    let daemon = Daemon::start(&dir, &Options::default()).expect("start");
    let agent_path = daemon.socket(Channel::Agent).to_path_buf();
    let admin_path = daemon.socket(Channel::Admin).to_path_buf();
    // Setup: the coupled pair on main, the branches, and each branch's own
    // CAS logicals.
    let mut setup = Client::connect(&agent_path).unwrap();
    let s = session(&mut setup, "main");
    record(&mut setup, s, "pair-a");
    record(&mut setup, s, "pair-b");
    for name in &BRANCHES[1..] {
        let r = setup.call(&Request::BranchCreate {
            name: (*name).into(),
            from: None,
            client_txn_id: None,
        });
        assert!(matches!(
            r.map(|e| e.response),
            Ok(Response::Committed { .. })
        ));
    }
    for name in BRANCHES {
        let s = session(&mut setup, name);
        for k in 0..CAS_LOGICALS {
            record(&mut setup, s, &format!("cas-{name}-{k}"));
        }
    }
    let ids = branch_ids(&mut setup);
    let ids: Arc<Vec<BranchId>> = Arc::new(BRANCHES.iter().map(|n| ids[*n]).collect());
    let counts = Arc::new(Counts::default());
    let chain: Arc<Mutex<Vec<CasStep>>> = Arc::default();
    let samples: Arc<Mutex<Vec<Sample>>> = Arc::default();
    let created: Arc<Mutex<BTreeMap<String, u32>>> = Arc::default();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let start = Instant::now();
    let workers: Vec<_> = (0..clients)
        .map(|t| {
            let (agent_path, admin_path) = (agent_path.clone(), admin_path.clone());
            let (counts, chain, samples, created, ids) = (
                counts.clone(),
                chain.clone(),
                samples.clone(),
                created.clone(),
                ids.clone(),
            );
            std::thread::spawn(move || {
                let mut c = Client::connect(&agent_path).unwrap();
                let mut admin = Client::connect(&admin_path).unwrap();
                let sessions: Vec<SessionId> =
                    BRANCHES.iter().map(|b| session(&mut c, b)).collect();
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (t as u64 + 1));
                let mut n = 0u64;
                while Instant::now() < deadline {
                    n += 1;
                    let bi = (rng.next() % BRANCHES.len() as u64) as usize;
                    let s = sessions[bi];
                    match rng.next() % 20 {
                        0..=5 => {
                            counts.reads.fetch_add(1, Ordering::Relaxed);
                            let r = c.call(&Request::Recall {
                                binding: Binding::root(s),
                                query: "pair-".into(),
                                scope: "repo".into(),
                                mode: Mode::Advisory,
                                limit: 2,
                            });
                            if let Ok(Envelope {
                                response: Response::Recall { items, .. },
                                ..
                            }) = r
                            {
                                let a = items.iter().find(|i| i.key == "pair-a");
                                let b = items.iter().find(|i| i.key == "pair-b");
                                if let (Some(a), Some(b)) = (a, b)
                                    && b.rev.0 != a.rev.0 + 1
                                {
                                    counts.violations.fetch_add(1, Ordering::Relaxed);
                                    eprintln!(
                                        "mixed heads on {}: a@{} b@{}",
                                        BRANCHES[bi], a.rev.0, b.rev.0
                                    );
                                }
                            }
                        }
                        6..=10 => {
                            let k = format!(
                                "cas-{}-{}",
                                BRANCHES[bi],
                                rng.next() % u64::from(CAS_LOGICALS)
                            );
                            let Some((epoch, head)) = read(&mut c, s, &k) else {
                                counts.other.fetch_add(1, Ordering::Relaxed);
                                continue;
                            };
                            if n.is_multiple_of(8) {
                                samples.lock().unwrap().push((
                                    epoch,
                                    ids[bi],
                                    head.rev,
                                    head.state.clone(),
                                ));
                            }
                            counts.attempts.fetch_add(1, Ordering::Relaxed);
                            let r = c.call(&Request::Record {
                                binding: Binding::root(s),
                                kind: RecordKind::Fact,
                                body: format!("{k} {t} {n}").into_bytes(),
                                target: WriteTarget::Revise {
                                    logical: head.logical,
                                    expected_head: head.rev,
                                },
                                deps: Vec::new(),
                                adopt_as_base: true,
                                client_txn_id: None,
                            });
                            match r.map(|e| e.response) {
                                Ok(Response::Committed { result }) => {
                                    counts.successes.fetch_add(1, Ordering::Relaxed);
                                    chain.lock().unwrap().push((
                                        bi,
                                        head.logical,
                                        head.rev,
                                        result.allocations.new_revisions[0],
                                        result.epoch,
                                    ));
                                }
                                Ok(Response::Error { code, .. }) if code == "HeadConflict" => {
                                    counts.conflicts.fetch_add(1, Ordering::Relaxed);
                                }
                                other => {
                                    eprintln!("unexpected: {other:?}");
                                    counts.other.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        11..=14 => {
                            let (Some((_, a)), Some((_, b))) =
                                (read(&mut c, s, "pair-a"), read(&mut c, s, "pair-b"))
                            else {
                                continue;
                            };
                            let (ba, bb) = (
                                format!("a {t} {n}").into_bytes(),
                                format!("b {t} {n}").into_bytes(),
                            );
                            let mut batch = Batch::new(vec![
                                Op::AddRevision {
                                    logical: a.logical.into(),
                                    content: content_hash(&ba),
                                    adopt_as_base: true,
                                },
                                Op::AddRevision {
                                    logical: b.logical.into(),
                                    content: content_hash(&bb),
                                    adopt_as_base: true,
                                },
                                Op::SetHead {
                                    logical: a.logical.into(),
                                    expected: a.rev.into(),
                                    new: RevRef::New(0),
                                },
                                Op::SetHead {
                                    logical: b.logical.into(),
                                    expected: b.rev.into(),
                                    new: RevRef::New(1),
                                },
                            ]);
                            batch.branch = ids[bi];
                            match admin
                                .call(&Request::Raw {
                                    blobs: vec![ba, bb],
                                    batch,
                                })
                                .map(|e| e.response)
                            {
                                Ok(Response::Committed { .. }) => {
                                    counts.coupled_moves.fetch_add(1, Ordering::Relaxed);
                                }
                                _ => {
                                    counts.coupled_conflicts.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        15..=18 => {
                            let source = (bi + 1 + (rng.next() % 3) as usize) % BRANCHES.len();
                            let r = c.call(&Request::Merge {
                                binding: None,
                                source: BRANCHES[source].into(),
                                into: Some(BRANCHES[bi].into()),
                                require_certified: false,
                                certify: Vec::new(),
                                client_txn_id: None,
                            });
                            match r.map(|e| e.response) {
                                Ok(Response::Merged { report, .. }) => {
                                    counts.merges.fetch_add(1, Ordering::Relaxed);
                                    counts.merge_conflicts.fetch_add(
                                        report.conflicts.len() as u64,
                                        Ordering::Relaxed,
                                    );
                                }
                                // A refusal by the merge rules (no stress merge should
                                // hit one: policy never changes, certification is off).
                                Ok(Response::Error { code, detail }) if code != "Timeout" => {
                                    eprintln!("merge refused: {code} {detail}");
                                    counts.merge_refused.fetch_add(1, Ordering::Relaxed);
                                }
                                other => {
                                    eprintln!("unexpected merge: {other:?}");
                                    counts.other.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        _ => {
                            let name = format!("race-{}", rng.next() % RACE_NAMES);
                            let r = c.call(&Request::BranchCreate {
                                name: name.clone(),
                                from: Some(BRANCHES[bi].into()),
                                client_txn_id: None,
                            });
                            match r.map(|e| e.response) {
                                Ok(Response::Committed { .. }) => {
                                    counts.creates.fetch_add(1, Ordering::Relaxed);
                                    *created.lock().unwrap().entry(name).or_default() += 1;
                                }
                                Ok(Response::Error { code, .. }) if code == "BranchExists" => {
                                    counts.create_exists.fetch_add(1, Ordering::Relaxed);
                                }
                                other => {
                                    eprintln!("unexpected create: {other:?}");
                                    counts.other.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    let elapsed = start.elapsed().as_secs_f64();
    let mut admin = Client::connect(&admin_path).unwrap();
    let listed = branch_ids(&mut admin);
    let _ = admin.call(&Request::Shutdown);
    daemon.wait();
    drop(daemon);
    let mut violations = counts.violations.load(Ordering::Relaxed);
    // Racing creates: at most one success per name, and each is listed.
    for (name, wins) in created.lock().unwrap().iter() {
        if *wins != 1 || !listed.contains_key(name) {
            violations += 1;
            eprintln!(
                "create race on {name}: {wins} successes, listed {}",
                listed.contains_key(name)
            );
        }
    }
    // One replay of the durable log, epoch by epoch: every successful CAS
    // moved its branch's head from `expected` to `new` in its own epoch
    // (merges move heads too, so a per-logical chain is not an invariant);
    // the coupled heads agree on every branch at every epoch; sampled reads
    // equal the replayed state at their epoch.
    let (store, _) =
        Store::open(StdDisk::default(), Path::new(&dir), &OpenOptions::default()).expect("reopen");
    let mut by_epoch: BTreeMap<Epoch, Vec<CasStep>> = BTreeMap::new();
    for step in chain.lock().unwrap().iter() {
        by_epoch.entry(step.4).or_default().push(*step);
    }
    let mut sampled: BTreeMap<Epoch, Vec<Sample>> = BTreeMap::new();
    for s in samples.lock().unwrap().iter() {
        sampled.entry(s.0).or_default().push(s.clone());
    }
    let (mut cas_breaks, mut replay_mismatch, mut mixed_epochs, mut checked) =
        (0u64, 0u64, 0u64, 0u64);
    let mut k = Kernel::default();
    let head = |k: &Kernel, b: BranchId, l: LogicalId| k.branch(b).and_then(|v| v.head(l));
    for e in 1..=store.epoch().0 {
        let e = Epoch(e);
        let batch = store.read_row(e).expect("row").batch().expect("batch");
        let before: Vec<_> = (by_epoch.get(&e).into_iter().flatten())
            .map(|s| head(&k, ids[s.0], s.1))
            .collect();
        k.commit(&batch).expect("replay");
        for (s, before) in by_epoch.get(&e).into_iter().flatten().zip(before) {
            checked += 1;
            if before != Some(s.2) || head(&k, ids[s.0], s.1) != Some(s.3) {
                cas_breaks += 1;
                eprintln!("CAS {s:?} at {e:?}: head was {before:?}");
            }
        }
        for (_, branch, rev, state) in sampled.get(&e).into_iter().flatten() {
            if k.branch(*branch).and_then(|v| v.node(*rev).ok()).as_ref() != Some(state) {
                replay_mismatch += 1;
                eprintln!("replay mismatch at {e:?} on {branch:?} r{}", rev.0);
            }
        }
        for info in k.branches() {
            let v = k.branch(info.id).unwrap();
            let h = |key| v.logical_by_key(key).and_then(|l| v.head(l));
            if let (Some(a), Some(b)) = (h("pair-a"), h("pair-b"))
                && b.0 != a.0 + 1
            {
                mixed_epochs += 1;
                eprintln!("mixed heads on {} at {e:?}: a@{} b@{}", info.name, a.0, b.0);
            }
        }
    }
    let replay_samples = sampled.values().map(Vec::len).sum::<usize>();
    if digests(&k) != digests(store.kernel().expect("kernel")) {
        replay_mismatch += 1;
        eprintln!("replayed kernel differs from the reopened store");
    }
    let c = &counts;
    let (attempts, successes, conflicts, other) = (
        c.attempts.load(Ordering::Relaxed),
        c.successes.load(Ordering::Relaxed),
        c.conflicts.load(Ordering::Relaxed),
        c.other.load(Ordering::Relaxed),
    );
    let outcome_gap =
        attempts.saturating_sub(successes + conflicts) as i64 - other.min(attempts) as i64;
    violations += cas_breaks + replay_mismatch + mixed_epochs;
    println!(
        "clients={clients} seconds={elapsed:.1} reads={} cas_attempts={attempts} cas_ok={successes} cas_conflicts={conflicts} \
         coupled_moves={} coupled_conflicts={} merges={} merge_conflicts={} merge_refused={} creates={} create_exists={} \
         branches={} other={other} mixed_heads={} cas_checked={checked} cas_breaks={cas_breaks} replay_mismatch={replay_mismatch} \
         replay_samples={replay_samples} mixed_epochs={mixed_epochs} epoch={}",
        c.reads.load(Ordering::Relaxed),
        c.coupled_moves.load(Ordering::Relaxed),
        c.coupled_conflicts.load(Ordering::Relaxed),
        c.merges.load(Ordering::Relaxed),
        c.merge_conflicts.load(Ordering::Relaxed),
        c.merge_refused.load(Ordering::Relaxed),
        c.creates.load(Ordering::Relaxed),
        c.create_exists.load(Ordering::Relaxed),
        listed.len(),
        c.violations.load(Ordering::Relaxed),
        store.epoch().0,
    );
    let refused = counts.merge_refused.load(Ordering::Relaxed);
    if violations > 0 || outcome_gap > 0 || other > 0 || refused > 0 {
        println!("G4 FAIL (store kept at {})", dir.display());
        ExitCode::from(1)
    } else {
        let _ = std::fs::remove_dir_all(&dir);
        println!("G4 PASS");
        ExitCode::SUCCESS
    }
}
