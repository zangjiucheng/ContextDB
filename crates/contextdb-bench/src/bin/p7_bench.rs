//! `p7_bench [rounds=5] [commits] [corpus] [pending]` (ADR 0015 §11 G5), on
//! the reference Mac:
//!
//! - **commit latency with workers busy vs idle.** A real `ctxd`
//!   (fullfsync); one socket writer commits ordinary records every 5 ms
//!   (measured) while a second moves premises of 16 Sem rules twice a
//!   second (2 jobs per move); idle: no `validators.toml`; busy: 4 workers with a command
//!   validator that answers after 20 ms. Rounds alternate idle and busy on
//!   fresh directories; budget (owner decision 2026-11-03): pooled p50
//!   ≤ 1.10× and p99 ≤ 1.25× idle.
//! - **candidates per new head.** A `corpus`-record recall index; each of
//!   500 single-head revisions is indexed and its pairs generated (the
//!   indexer thread's work, off the writer); p50/p99/max per head.
//! - **long-poll wake.** A `ctx_changes` waiter on the admin socket; the
//!   time from the epoch's publication (seen by a spinning reader) to the
//!   waiter's response; 500 samples.
//! - **memory at `pending` Pending rules.** Discovery of that many rules:
//!   time, job rows, `jobs.sqlite` bytes and RSS growth; the candidate
//!   table's rows against `per_head` × heads.
use contextdb_daemon::{
    actor,
    candidates::{self, Candidates},
    jobs::{self, JobTable},
    proto::*,
    server::{Client, Daemon, Options},
    validators::{Loaded, Scheduler},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, recall::RecallIndex};
use contextdb_validate::config::Limits;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn pct(v: &mut [Duration], p: usize) -> f64 {
    v.sort();
    v[(v.len() * p).div_ceil(100).saturating_sub(1)].as_secs_f64() * 1000.
}
fn rss_kb() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}
fn h(b: &[u8]) -> ContentHash {
    content_hash(b)
}
fn temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "p7-bench-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
fn raw(c: &mut Client, blobs: Vec<Vec<u8>>, ops: Vec<Op>) -> Response {
    c.call(&Request::Raw {
        blobs,
        batch: Batch::new(ops),
    })
    .expect("call")
    .response
}
fn committed(r: Response) -> CommitResult {
    match r {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
fn counter(r: &[Counter], name: &str) -> u64 {
    r.iter().find(|c| c.name == name).map_or(0, |c| c.value)
}

const BUSY: &str = r#"
[limits]
workers = 4
[[validator]]
name = "slow"
type = "command"
command = ["/bin/sh", "-c", "cat > /dev/null; sleep 0.02; echo '{\"label\": \"reinforcement\", \"rationale\": \"ok\", \"evidence\": []}'"]
trusted = true
"#;

/// Premise moves per second in the background, in both modes (owner
/// decision 2026-11-03: the budget is measured at a realistic rate).
const MOVES_PER_SEC: u64 = 2;

/// One round on a fresh daemon: `commits` ordinary commits (16 Facts that
/// no rule depends on), every 5 ms, while a second connection moves the
/// premises of 16 Sem rules `MOVES_PER_SEC` times a second (each move
/// makes 2 rules Pending). Returns the commit latencies and the validator
/// records committed meanwhile.
fn commit_round(busy: bool, commits: usize) -> (Vec<Duration>, u64) {
    let dir = temp(if busy { "busy" } else { "idle" });
    if busy {
        std::fs::write(dir.join("validators.toml"), BUSY).unwrap();
    }
    let d = Daemon::start(&dir, &Options::default()).expect("start");
    let mut c = Client::connect(d.socket(Channel::Admin)).unwrap();
    const P: u32 = 16; // premises
    // 16 rules over 16 premises, two premises each: a move makes 2 rules
    // Pending (G6's generator: 0.61 jobs per move on average, max 2).
    const R: u32 = 16; // rule conclusions
    const W: u32 = 16; // measured Facts, outside every rule
    let mut ops = Vec::new();
    let mut blobs = Vec::new();
    for i in 0..P + R + W {
        let body = format!("record {i} v0");
        ops.push(Op::CreateLogical {
            key: format!("k{i}"),
            kind: if (P..P + R).contains(&i) {
                RecordKind::Decision
            } else {
                RecordKind::Fact
            },
            scope_lane: "repo".into(),
            content: h(body.as_bytes()),
            adopt_as_base: true,
        });
        blobs.push(body.into_bytes());
    }
    committed(raw(&mut c, blobs, ops));
    let rules = (0..R)
        .map(|j| Op::AddRule {
            conclusion: RevisionId(P + j).into(),
            incidences: vec![
                (RevisionId(j % P).into(), EdgeKind::Sem),
                (RevisionId((j + 1) % P).into(), EdgeKind::Sem),
            ],
            enabled: true,
        })
        .collect();
    committed(raw(&mut c, Vec::new(), rules));
    // A logical's head, moved to a fresh revision.
    fn mv(c: &mut Client, l: u32, head: RevisionId, body: String) -> (RevisionId, Duration) {
        let t = Instant::now();
        let r = committed(raw(
            c,
            vec![body.clone().into_bytes()],
            vec![
                Op::AddRevision {
                    logical: LogicalId(l).into(),
                    content: h(body.as_bytes()),
                    adopt_as_base: true,
                },
                Op::SetHead {
                    logical: LogicalId(l).into(),
                    expected: head.into(),
                    new: RevRef::New(0),
                },
            ],
        ));
        (r.allocations.new_revisions[0], t.elapsed())
    }
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let mover = {
        let (stop, socket) = (stop.clone(), d.socket(Channel::Admin).to_path_buf());
        std::thread::spawn(move || {
            let mut c = Client::connect(&socket).unwrap();
            let mut heads: Vec<RevisionId> = (0..P).map(RevisionId).collect();
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let l = (n * 7) % P;
                heads[l as usize] = mv(&mut c, l, heads[l as usize], format!("premise {l} n{n}")).0;
                n += 1;
                std::thread::sleep(Duration::from_millis(1000 / MOVES_PER_SEC));
            }
        })
    };
    let first = P + R;
    let mut heads: Vec<RevisionId> = (first..first + W).map(RevisionId).collect();
    let mut lat = Vec::with_capacity(commits);
    // Warm-up, not measured.
    for n in 0..commits + 100 {
        let w = (n as u32 * 7) % W;
        let (rev, t) = mv(
            &mut c,
            first + w,
            heads[w as usize],
            format!("fact {w} n{n}"),
        );
        heads[w as usize] = rev;
        if n >= 100 {
            lat.push(t);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    stop.store(true, Ordering::Relaxed);
    mover.join().unwrap();
    let records = match (c.call(&Request::Validators {
        action: ValidatorsAction::Status,
    }))
    .unwrap()
    .response
    {
        Response::Validators { report } => counter(&report.stats, "records"),
        _ => 0,
    };
    d.stop();
    let _ = std::fs::remove_dir_all(&dir);
    (lat, records)
}

const WORDS: usize = 3000;
/// A pseudo-random record body: 30 words, skewed to the frequent ones.
fn body(seed: u64, n: u64) -> String {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ n;
    (0..30)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let r = (x % 1_000_000) as f64 / 1_000_000.0;
            format!("w{}", ((r * r * r) * WORDS as f64) as usize)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn candidates_bench(corpus: u32) -> (Vec<Duration>, Duration, usize, usize, u64) {
    let disk = SimDisk::new();
    let (store, _) = Store::open(disk, Path::new("/db"), &OpenOptions::default()).unwrap();
    let a = actor::spawn(store).unwrap();
    let dir = temp("cand");
    let recall = Mutex::new(RecallIndex::open(Some(&dir.join("recall.sqlite"))).unwrap());
    let table = Candidates::open(Some(&dir.join(candidates::FILE))).unwrap();
    let stop = AtomicBool::new(false);
    let admin = |ops: Vec<Op>, blobs: Vec<Vec<u8>>| {
        let mut b = Batch::new(ops);
        b.channel = Channel::Admin;
        a.write(blobs, b).unwrap().unwrap()
    };
    for chunk in (0..corpus).collect::<Vec<_>>().chunks(1000) {
        let mut ops = Vec::new();
        let mut blobs = Vec::new();
        for &i in chunk {
            let text = body(1, i as u64);
            ops.push(Op::CreateLogical {
                key: format!("rec/{i}"),
                kind: [RecordKind::Decision, RecordKind::Fact, RecordKind::Plan][i as usize % 3],
                scope_lane: "repo".into(),
                content: h(text.as_bytes()),
                adopt_as_base: true,
            });
            blobs.push(text.into_bytes());
        }
        admin(ops, blobs);
    }
    let t = Instant::now();
    let snap = a.snapshot();
    candidates::index(&snap, &recall);
    candidates::generate(&snap, &recall, &table, &stop);
    drop(snap);
    let backfill = t.elapsed();
    // Where a head's time goes: idf terms, then the OR query.
    {
        let idx = recall.lock().unwrap();
        let (mut tt, mut q) = (Vec::new(), Vec::new());
        for n in 0..200u64 {
            let text = body(3, n);
            let t = Instant::now();
            let terms = idx.top_terms(&text, candidates::TERMS).unwrap();
            tt.push(t.elapsed());
            let t = Instant::now();
            let _ = idx.any_of(&terms, "repo", MAIN, 36).unwrap();
            q.push(t.elapsed());
        }
        println!(
            "candidates: top_terms p50 {:.2} p99 {:.2} ms; any_of p50 {:.2} p99 {:.2} ms",
            pct(&mut tt, 50),
            pct(&mut tt, 99),
            pct(&mut q, 50),
            pct(&mut q, 99)
        );
    }
    let mut lat = Vec::new();
    let mut index_lat = Vec::new();
    for n in 0..500u64 {
        let l = LogicalId(((n * 7919) % corpus as u64) as u32);
        let head = a.snapshot().view(MAIN).head(l).unwrap();
        let text = body(2, n);
        admin(
            vec![
                Op::AddRevision {
                    logical: l.into(),
                    content: h(text.as_bytes()),
                    adopt_as_base: true,
                },
                Op::SetHead {
                    logical: l.into(),
                    expected: head.into(),
                    new: RevRef::New(0),
                },
            ],
            vec![text.into_bytes()],
        );
        let snap = a.snapshot();
        let t = Instant::now();
        candidates::index(&snap, &recall);
        let indexed = t.elapsed();
        candidates::generate(&snap, &recall, &table, &stop);
        lat.push(t.elapsed());
        index_lat.push(indexed);
    }
    println!(
        "candidates: index step p50 {:.2} p99 {:.2} ms",
        pct(&mut index_lat, 50),
        pct(&mut index_lat, 99)
    );
    let rows = table.all().unwrap().len();
    let heads = table.stats.heads.load(Ordering::Relaxed);
    let bytes = std::fs::metadata(dir.join(candidates::FILE))
        .map(|m| m.len())
        .unwrap_or(0);
    let _ = std::fs::remove_dir_all(&dir);
    (lat, backfill, rows, heads as usize, bytes)
}

fn long_poll_bench(samples: usize) -> Vec<Duration> {
    let dir = temp("poll");
    let d = Daemon::start(&dir, &Options::default()).expect("start");
    let sock = d.socket(Channel::Admin).to_path_buf();
    let mut c = Client::connect(&sock).unwrap();
    committed(raw(
        &mut c,
        vec![b"x0".to_vec()],
        vec![Op::CreateLogical {
            key: "x".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: h(b"x0"),
            adopt_as_base: true,
        }],
    ));
    let mut head = RevisionId(0);
    let mut out = Vec::new();
    for n in 0..samples {
        let tail = d.service.actor.snapshot().epoch;
        let waiter = {
            let sock = sock.clone();
            std::thread::spawn(move || {
                let mut w = Client::connect(&sock).unwrap();
                let r = w
                    .call(&Request::Changes {
                        binding: None,
                        branch: None,
                        since_epoch: tail,
                        scope: None,
                        max_items: 10,
                        wait_ms: 10_000,
                    })
                    .unwrap();
                (Instant::now(), r.response)
            })
        };
        let service = d.service.clone();
        let spinner = std::thread::spawn(move || {
            loop {
                if service.actor.snapshot().epoch > tail {
                    return Instant::now();
                }
                std::hint::spin_loop();
            }
        });
        std::thread::sleep(Duration::from_millis(15));
        let text = format!("x{}", n + 1);
        let r = committed(raw(
            &mut c,
            vec![text.clone().into_bytes()],
            vec![
                Op::AddRevision {
                    logical: LogicalId(0).into(),
                    content: h(text.as_bytes()),
                    adopt_as_base: true,
                },
                Op::SetHead {
                    logical: LogicalId(0).into(),
                    expected: head.into(),
                    new: RevRef::New(0),
                },
            ],
        ));
        head = r.allocations.new_revisions[0];
        let published = spinner.join().unwrap();
        let (woke, resp) = waiter.join().unwrap();
        assert!(
            matches!(&resp, Response::Changes { items, .. } if !items.is_empty()),
            "{resp:?}"
        );
        out.push(woke.saturating_duration_since(published));
    }
    d.stop();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn memory_bench(pending: u32) {
    let disk = SimDisk::new();
    let (store, _) = Store::open(disk, Path::new("/db"), &OpenOptions::default()).unwrap();
    let a = actor::spawn(store).unwrap();
    let admin = |ops: Vec<Op>, blobs: Vec<Vec<u8>>| {
        let mut b = Batch::new(ops);
        b.channel = Channel::Admin;
        a.write(blobs, b).unwrap().unwrap()
    };
    admin(
        vec![Op::CreateLogical {
            key: "a".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: h(b"a0"),
            adopt_as_base: true,
        }],
        vec![b"a0".to_vec()],
    );
    for start in (0..pending).step_by(5000) {
        let end = (start + 5000).min(pending);
        let mut ops = Vec::new();
        let mut blobs = Vec::new();
        for k in start..end {
            let text = format!("decision {k}");
            ops.push(Op::CreateLogical {
                key: format!("d{k}"),
                kind: RecordKind::Decision,
                scope_lane: "repo".into(),
                content: h(text.as_bytes()),
                adopt_as_base: true,
            });
            blobs.push(text.into_bytes());
        }
        admin(ops, blobs);
        admin(
            (start..end)
                .map(|k| Op::AddRule {
                    conclusion: RevisionId(1 + k).into(),
                    incidences: vec![(RevisionId(0).into(), EdgeKind::Sem)],
                    enabled: true,
                })
                .collect(),
            Vec::new(),
        );
    }
    admin(
        vec![
            Op::AddRevision {
                logical: LogicalId(0).into(),
                content: h(b"a1"),
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: LogicalId(0).into(),
                expected: RevisionId(0).into(),
                new: RevRef::New(0),
            },
        ],
        vec![b"a1".to_vec()],
    );
    assert_eq!(
        a.snapshot().view(MAIN).pending_rules().len(),
        pending as usize
    );
    let dir = temp("mem");
    let s = Scheduler::new(
        a.clone(),
        JobTable::open(&dir.join(jobs::FILE)).unwrap(),
        None,
        Loaded {
            limits: Limits::default(),
            candidates: Default::default(),
            validators: Vec::new(),
            error: None,
        },
    );
    let before = rss_kb();
    let t = Instant::now();
    let queued = s.discover_once().unwrap();
    let took = t.elapsed();
    let after = rss_kb();
    // A second pass finds nothing new (keys bound the table).
    let again = s.discover_once().unwrap();
    let bytes: u64 = ["", "-wal", "-shm"]
        .iter()
        .filter_map(|x| std::fs::metadata(dir.join(format!("{}{x}", jobs::FILE))).ok())
        .map(|m| m.len())
        .sum();
    let rows = s.jobs.all().unwrap().len();
    println!(
        "memory: {pending} pending rules: discovery {:.0} ms, {queued} queued ({again} on a second pass), {rows} job rows, jobs.sqlite {} KiB ({:.0} B/row), RSS +{} KiB",
        took.as_secs_f64() * 1000.,
        bytes / 1024,
        bytes as f64 / rows.max(1) as f64,
        after.saturating_sub(before)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rounds: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let commits: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1000);
    let corpus: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let pending: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let mut pass = true;

    let (mut idle, mut busy) = (Vec::new(), Vec::new());
    for round in 0..rounds {
        let (mut i, _) = commit_round(false, commits);
        let (mut b, records) = commit_round(true, commits);
        println!(
            "commit round {round}: idle p50 {:.2} p99 {:.2} ms; busy p50 {:.2} p99 {:.2} ms ({records} worker records)",
            pct(&mut i, 50),
            pct(&mut i, 99),
            pct(&mut b, 50),
            pct(&mut b, 99)
        );
        idle.extend(i);
        busy.extend(b);
    }
    // Owner decision (2026-11-03): p99 within 25% and p50 within 10%, pooled
    // over interleaved rounds, at a realistic premise-move rate.
    let (ip99, bp99) = (pct(&mut idle, 99), pct(&mut busy, 99));
    let (ip50, bp50) = (pct(&mut idle, 50), pct(&mut busy, 50));
    let ok = bp99 <= ip99 * 1.25 && bp50 <= ip50 * 1.10;
    pass &= ok;
    println!(
        "commit pooled ({MOVES_PER_SEC} premise moves/s): p50 idle {ip50:.2} busy {bp50:.2} ms (ratio {:.3}, budget ≤ 1.10); p99 idle {ip99:.2} busy {bp99:.2} ms (ratio {:.3}, budget ≤ 1.25): {}",
        bp50 / ip50,
        bp99 / ip99,
        if ok { "PASS" } else { "FAIL" }
    );

    let (mut lat, backfill, rows, heads, bytes) = candidates_bench(corpus);
    let p99 = pct(&mut lat, 99);
    let ok = p99 < 20.;
    pass &= ok;
    println!(
        "candidates: corpus {corpus}, backfill {:.0} ms; per new head p50 {:.2} p99 {p99:.2} max {:.2} ms (budget p99 < 20): {}; table {rows} rows for {heads} heads (≤ {} = per_head × heads), {} KiB",
        backfill.as_secs_f64() * 1000.,
        pct(&mut lat, 50),
        pct(&mut lat, 100),
        if ok { "PASS" } else { "FAIL" },
        heads * 5,
        bytes / 1024
    );
    pass &= rows <= heads * 5;

    let mut wake = long_poll_bench(500);
    let p99 = pct(&mut wake, 99);
    let ok = p99 < 10.;
    pass &= ok;
    println!(
        "long-poll wake: p50 {:.3} p99 {p99:.3} max {:.3} ms (budget p99 < 10): {}",
        pct(&mut wake, 50),
        pct(&mut wake, 100),
        if ok { "PASS" } else { "FAIL" }
    );

    memory_bench(pending);
    println!("p7_g5_pass: {pass}");
}
