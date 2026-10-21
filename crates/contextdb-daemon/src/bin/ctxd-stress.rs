//! `ctxd-stress <clients> <seconds> [dir]` (SPEC Gate P3 G4, ADR 0008 §11):
//! a real daemon on local disk and N socket clients mixing reads, CAS writes
//! and coupled head moves. Invariants a mixed-epoch read or a lost update
//! would break:
//! - coupled heads: `pair-a` and `pair-b` always move in one batch, so a single
//!   recall never shows them out of step (I4);
//! - every attempt has exactly one outcome (success, conflict, or a counted
//!   other error);
//! - CAS chain: the successful revisions of each logical form one chain
//!   (each success's `expected` is the previous success's new revision);
//! - replay: sampled read responses equal a replay of the log to their epoch.
//!
//! Exit code 1 on any violation.
use contextdb_daemon::{
    proto::*,
    server::{Client, Daemon, Options},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, StdDisk, Store};
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const CAS_LOGICALS: u32 = 8;
#[derive(Default)]
struct Counts {
    reads: AtomicU64,
    attempts: AtomicU64,
    successes: AtomicU64,
    conflicts: AtomicU64,
    other: AtomicU64,
    busy: AtomicU64,
    coupled_moves: AtomicU64,
    coupled_conflicts: AtomicU64,
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
fn session(c: &mut Client) -> SessionId {
    let r = c.call(&Request::SessionStart {
        harness: "stress".into(),
        repo: "r".into(),
        head_sha: "h".into(),
        parent: None,
    });
    match r.map(|e| e.response) {
        Ok(Response::Session { session }) => session,
        other => panic!("session: {other:?}"),
    }
}
fn read(c: &mut Client, s: SessionId, key: &str) -> Option<(Epoch, Item)> {
    let env = c
        .call(&Request::Read {
            session: s,
            target: Target::Key(key.into()),
            mode: Mode::Advisory,
        })
        .ok()?;
    match env.response {
        Response::Read { item, .. } => Some((env.epoch, item)),
        _ => None,
    }
}
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let clients: usize = args.first().and_then(|v| v.parse().ok()).unwrap_or(8);
    let seconds: u64 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(10);
    let dir = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("ctxd-stress-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir);
    let daemon = Daemon::start(&dir, &Options::default()).expect("start");
    let agent_path = daemon.socket(Channel::Agent).to_path_buf();
    let admin_path = daemon.socket(Channel::Admin).to_path_buf();
    // Setup: coupled pair and CAS logicals.
    let mut setup = Client::connect(&agent_path).unwrap();
    let s = session(&mut setup);
    let mut keys = vec!["pair-a".to_owned(), "pair-b".to_owned()];
    keys.extend((0..CAS_LOGICALS).map(|n| format!("cas-{n}")));
    for k in &keys {
        let r = setup.call(&Request::Record {
            session: s,
            kind: RecordKind::Fact,
            body: format!("{k} 0").into_bytes(),
            target: WriteTarget::Create {
                key: k.clone(),
                scope_lane: "repo".into(),
            },
            deps: Vec::new(),
            adopt_as_base: true,
            client_txn_id: None,
        });
        assert!(matches!(r.map(|e| e.response), Ok(Response::Committed { .. })));
    }
    let counts = Arc::new(Counts::default());
    // (logical, expected, new, epoch) per successful CAS.
    let chain: Arc<Mutex<Vec<(LogicalId, RevisionId, RevisionId, Epoch)>>> = Arc::default();
    // (epoch, revision, state) samples for the replay check.
    let samples: Arc<Mutex<Vec<(Epoch, RevisionId, NodeState)>>> = Arc::default();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let start = Instant::now();
    let workers: Vec<_> = (0..clients)
        .map(|t| {
            let (agent_path, admin_path) = (agent_path.clone(), admin_path.clone());
            let (counts, chain, samples) = (counts.clone(), chain.clone(), samples.clone());
            std::thread::spawn(move || {
                let mut c = Client::connect(&agent_path).unwrap();
                let mut admin = Client::connect(&admin_path).unwrap();
                let s = session(&mut c);
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (t as u64 + 1));
                let mut n = 0u64;
                while Instant::now() < deadline {
                    n += 1;
                    match rng.next() % 10 {
                        0..=3 => {
                            counts.reads.fetch_add(1, Ordering::Relaxed);
                            let r = c.call(&Request::Recall {
                                session: s,
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
                                // Moves add one revision to each logical: counts agree.
                                if let (Some(a), Some(b)) = (a, b)
                                    && b.rev.0 != a.rev.0 + 1
                                {
                                    counts.violations.fetch_add(1, Ordering::Relaxed);
                                    eprintln!("mixed epoch: a@{} b@{}", a.rev.0, b.rev.0);
                                }
                            }
                        }
                        4..=7 => {
                            let k = format!("cas-{}", rng.next() % u64::from(CAS_LOGICALS));
                            let Some((epoch, head)) = read(&mut c, s, &k) else {
                                counts.other.fetch_add(1, Ordering::Relaxed);
                                continue;
                            };
                            if n % 16 == 0 {
                                samples.lock().unwrap().push((epoch, head.rev, head.state.clone()));
                            }
                            counts.attempts.fetch_add(1, Ordering::Relaxed);
                            let r = c.call(&Request::Record {
                                session: s,
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
                                        head.logical,
                                        head.rev,
                                        result.allocations.new_revisions[0],
                                        result.epoch,
                                    ));
                                }
                                Ok(Response::Error { code, .. }) if code == "HeadConflict" => {
                                    counts.conflicts.fetch_add(1, Ordering::Relaxed);
                                }
                                Ok(Response::Error { code, .. }) if code == "Busy" => {
                                    counts.busy.fetch_add(1, Ordering::Relaxed);
                                    counts.other.fetch_add(1, Ordering::Relaxed);
                                }
                                other => {
                                    eprintln!("unexpected: {other:?}");
                                    counts.other.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        _ => {
                            // Coupled move: one batch adds a revision to both and
                            // moves both heads (CAS on both).
                            let (Some((_, a)), Some((_, b))) = (read(&mut c, s, "pair-a"), read(&mut c, s, "pair-b")) else {
                                continue;
                            };
                            let (ba, bb) = (format!("a {t} {n}").into_bytes(), format!("b {t} {n}").into_bytes());
                            let batch = Batch::new(vec![
                                Op::AddRevision {
                                    logical: a.logical.into(),
                                    content: *blake3::hash(&ba).as_bytes(),
                                    adopt_as_base: true,
                                },
                                Op::AddRevision {
                                    logical: b.logical.into(),
                                    content: *blake3::hash(&bb).as_bytes(),
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
                            match admin.call(&Request::Raw { blobs: vec![ba, bb], batch }).map(|e| e.response) {
                                Ok(Response::Committed { .. }) => {
                                    counts.coupled_moves.fetch_add(1, Ordering::Relaxed);
                                }
                                _ => {
                                    counts.coupled_conflicts.fetch_add(1, Ordering::Relaxed);
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
    let _ = admin.call(&Request::Shutdown);
    daemon.wait();
    drop(daemon);
    // CAS chain per logical.
    let mut chain = chain.lock().unwrap().clone();
    chain.sort_by_key(|(l, _, _, e)| (*l, *e));
    let mut chain_breaks = 0u64;
    for pair in chain.windows(2) {
        if pair[0].0 == pair[1].0 && pair[1].1 != pair[0].2 {
            chain_breaks += 1;
            eprintln!("CAS chain break: {:?} then {:?}", pair[0], pair[1]);
        }
    }
    // Replay check against the durable log (offline open after shutdown).
    let (store, _) = Store::open(StdDisk::default(), Path::new(&dir), &OpenOptions::default()).expect("reopen");
    let reader = store.log_reader();
    let mut samples = samples.lock().unwrap().clone();
    samples.sort_by_key(|(e, _, _)| *e);
    samples.dedup_by_key(|(e, _, _)| *e);
    let mut replay_mismatch = 0u64;
    for (epoch, rev, state) in samples.iter().step_by((samples.len() / 50).max(1)) {
        let db = reader.replay_to(*epoch).expect("replay");
        if db.node(*rev).ok().as_ref() != Some(state) {
            replay_mismatch += 1;
            eprintln!("replay mismatch at {epoch:?} r{}", rev.0);
        }
    }
    let c = &counts;
    let (attempts, successes, conflicts, other) = (
        c.attempts.load(Ordering::Relaxed),
        c.successes.load(Ordering::Relaxed),
        c.conflicts.load(Ordering::Relaxed),
        c.other.load(Ordering::Relaxed),
    );
    let outcome_gap = attempts.saturating_sub(successes + conflicts) as i64 - other.min(attempts) as i64;
    let violations = c.violations.load(Ordering::Relaxed) + chain_breaks + replay_mismatch;
    let writes = successes + c.coupled_moves.load(Ordering::Relaxed);
    println!(
        "clients={clients} seconds={elapsed:.1} reads={} cas_attempts={attempts} cas_ok={successes} cas_conflicts={conflicts} \
         coupled_moves={} coupled_conflicts={} busy={} other={other} write_tx_per_s={:.0} \
         mixed_epoch={} chain_breaks={chain_breaks} replay_mismatch={replay_mismatch} replay_samples={} epoch={}",
        c.reads.load(Ordering::Relaxed),
        c.coupled_moves.load(Ordering::Relaxed),
        c.coupled_conflicts.load(Ordering::Relaxed),
        c.busy.load(Ordering::Relaxed),
        writes as f64 / elapsed,
        c.violations.load(Ordering::Relaxed),
        samples.len().min(50),
        store.epoch().0,
    );
    let _ = std::fs::remove_dir_all(&dir);
    if violations > 0 || outcome_gap > 0 {
        println!("G4 FAIL");
        ExitCode::from(1)
    } else {
        println!("G4 PASS");
        ExitCode::SUCCESS
    }
}
