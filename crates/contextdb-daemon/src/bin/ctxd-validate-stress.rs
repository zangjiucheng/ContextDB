//! `ctxd-validate-stress <seconds> [dir]` (ADR 0015 §11 G4): a real daemon
//! on local disk with 4 validator workers, and 4 socket writers moving the
//! premises of 8 Sem rules, two writers on `main` and two on `dev`. `main`
//! is validated by a trusted command validator (records), `dev` by an
//! untrusted one (PendingReviews), each answering `reinforcement` or
//! `conflict` from a checksum of its task.
//!
//! After the writers stop and the workers settle, the invariants:
//! - **no duplicate:** the log holds at most one `RecordValidation` per
//!   `(branch, rule, curvec)`, every one from the trusted worker principal,
//!   and at most one worker PendingReview per `(branch, rule, curvec)`;
//!   no batch from the LLM principal carries a `RecordValidation`;
//! - **no loss:** no rule on `main` is still Pending, and every Pending
//!   rule on `dev` has a worker review naming its current vector;
//! - every job row is Done or Superseded (none Queued, Running or Failed).
//!
//! Exit code 1 on any violation.
use contextdb_daemon::{
    proto::*,
    server::{Client, Daemon, Options},
    validators::{LLM_PRINCIPAL, TRUSTED_PRINCIPAL},
};
use contextdb_model::*;
use contextdb_proto::docs::ReviewDoc;
use contextdb_store::Payload;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const RULES: u32 = 8;
const CONFIG: &str = r#"
[limits]
workers = 4
lease_secs = 30
attempts = 3
llm_calls_per_hour = 100000
[[validator]]
name = "trusted"
type = "command"
command = ["/bin/sh", "-c", "x=$(cksum | cut -c1); case $x in [0-4]) l=reinforcement;; *) l=conflict;; esac; printf '{\"label\": \"%s\", \"rationale\": \"stress\", \"evidence\": []}' $l"]
trusted = true
branches = ["main"]
[[validator]]
name = "llm"
type = "command"
command = ["/bin/sh", "-c", "x=$(cksum | cut -c1); case $x in [0-4]) l=reinforcement;; *) l=conflict;; esac; printf '{\"label\": \"%s\", \"rationale\": \"stress\", \"evidence\": []}' $l"]
trusted = false
branches = ["dev"]
"#;

fn h(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}
fn raw(c: &mut Client, branch: BranchId, blobs: Vec<Vec<u8>>, ops: Vec<Op>) -> Response {
    let mut batch = Batch::new(ops);
    batch.branch = branch;
    c.call(&Request::Raw { blobs, batch })
        .expect("call")
        .response
}
fn counter(r: &[Counter], name: &str) -> u64 {
    r.iter().find(|c| c.name == name).map_or(0, |c| c.value)
}
fn report(c: &mut Client) -> ValidatorsReport {
    match (c.call(&Request::Validators {
        action: ValidatorsAction::Status,
    }))
    .expect("call")
    .response
    {
        Response::Validators { report } => report,
        other => panic!("{other:?}"),
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let dir = args.next().map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir().join(format!("ctxd-validate-stress-{}", std::process::id()))
    });
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("validators.toml"), CONFIG).expect("config");
    let daemon = Daemon::start(&dir, &Options::default()).expect("start");
    let admin = daemon.socket(Channel::Admin).to_path_buf();
    let mut setup = Client::connect(&admin).expect("connect");
    let loaded = report(&mut setup);
    if loaded.error.is_some() || loaded.validators.len() != 2 {
        eprintln!(
            "validators.toml: {:?} {:?}",
            loaded.error, loaded.validators
        );
        return ExitCode::from(2);
    }

    // p0..p7 (Fact) and c0..c7 (Decision); c_i ← Sem p_i, Sem p_{i+1}.
    let mut ops = Vec::new();
    let mut blobs = Vec::new();
    for (prefix, kind) in [("p", RecordKind::Fact), ("c", RecordKind::Decision)] {
        for i in 0..RULES {
            let body = format!("{prefix}{i} v0");
            ops.push(Op::CreateLogical {
                key: format!("{prefix}{i}"),
                kind,
                scope_lane: "repo".into(),
                content: h(body.as_bytes()),
                adopt_as_base: true,
            });
            blobs.push(body.into_bytes());
        }
    }
    assert!(matches!(
        raw(&mut setup, MAIN, blobs, ops),
        Response::Committed { .. }
    ));
    let rules = (0..RULES)
        .map(|i| Op::AddRule {
            conclusion: RevisionId(RULES + i).into(),
            incidences: vec![
                (RevisionId(i).into(), EdgeKind::Sem),
                (RevisionId((i + 1) % RULES).into(), EdgeKind::Sem),
            ],
            enabled: true,
        })
        .collect();
    assert!(matches!(
        raw(&mut setup, MAIN, Vec::new(), rules),
        Response::Committed { .. }
    ));
    let r = setup
        .call(&Request::BranchCreate {
            name: "dev".into(),
            from: None,
            client_txn_id: None,
        })
        .expect("call")
        .response;
    assert!(!matches!(r, Response::Error { .. }), "{r:?}");
    let dev = (daemon.service.actor.snapshot().kernel)
        .branch_by_name("dev")
        .expect("dev");

    // Writers: 0 and 1 on main, 2 and 3 on dev.
    let moves = Arc::new(AtomicU64::new(0));
    let conflicts = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let writers: Vec<_> = (0..4u64)
        .map(|w| {
            let admin = admin.clone();
            let service = daemon.service.clone();
            let (moves, conflicts) = (moves.clone(), conflicts.clone());
            std::thread::spawn(move || {
                let branch = if w < 2 { MAIN } else { dev };
                let mut c = Client::connect(&admin).expect("connect");
                let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ w;
                let mut n = 0u64;
                while Instant::now() < deadline {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    let l = LogicalId((rng % RULES as u64) as u32);
                    let head = service.actor.snapshot().view(branch).head(l).expect("head");
                    n += 1;
                    let body = format!("p{} w{w} n{n}", l.0);
                    let r = raw(
                        &mut c,
                        branch,
                        vec![body.clone().into_bytes()],
                        vec![
                            Op::AddRevision {
                                logical: l.into(),
                                content: h(body.as_bytes()),
                                adopt_as_base: true,
                            },
                            Op::SetHead {
                                logical: l.into(),
                                expected: head.into(),
                                new: RevRef::New(0),
                            },
                        ],
                    );
                    match r {
                        Response::Committed { .. } => moves.fetch_add(1, Ordering::Relaxed),
                        _ => conflicts.fetch_add(1, Ordering::Relaxed),
                    };
                    std::thread::sleep(Duration::from_millis(2 + rng % 20));
                }
            })
        })
        .collect();
    let mut last = Instant::now();
    for w in writers {
        while !w.is_finished() {
            std::thread::sleep(Duration::from_millis(200));
            if last.elapsed() >= Duration::from_secs(60) {
                last = Instant::now();
                let r = report(&mut setup);
                eprintln!(
                    "moves {} jobs {:?} calls {}",
                    moves.load(Ordering::Relaxed),
                    (r.jobs.iter())
                        .map(|c| (c.name.clone(), c.value))
                        .collect::<Vec<_>>(),
                    counter(&r.stats, "calls")
                );
            }
        }
        w.join().expect("writer");
    }

    // Settle: discovery caught up and no row Queued or Running.
    let settle = Instant::now();
    let r = loop {
        let epoch = daemon.service.actor.snapshot().epoch;
        std::thread::sleep(Duration::from_millis(500));
        let r = report(&mut setup);
        let busy = counter(&r.jobs, "queued") + counter(&r.jobs, "running");
        if busy == 0 && daemon.service.actor.snapshot().epoch == epoch {
            // One more discovery round after the last commit.
            std::thread::sleep(Duration::from_millis(1500));
            let again = report(&mut setup);
            if counter(&again.jobs, "queued") + counter(&again.jobs, "running") == 0 {
                break again;
            }
        }
        if settle.elapsed() > Duration::from_secs(600) {
            eprintln!("did not settle: {r:?}");
            return ExitCode::from(1);
        }
    };

    let mut violations = Vec::new();
    let snap = daemon.service.actor.snapshot();
    // The log: records and worker reviews per (branch, rule, curvec).
    let mut records: BTreeMap<(BranchId, RuleId, HeadVector), u32> = BTreeMap::new();
    let mut reviews: BTreeMap<(BranchId, Option<RuleId>, HeadVector), u32> = BTreeMap::new();
    let mut llm_records = 0;
    for e in 1..=snap.epoch.0 {
        let row = snap.log.read_row(Epoch(e)).expect("row");
        let batch = row.batch().expect("batch");
        for op in &batch.ops {
            match op {
                Op::RecordValidation {
                    rule: RuleRef::Existing(rule),
                    curvec_seen,
                    ..
                } => {
                    if batch.principal == LLM_PRINCIPAL {
                        llm_records += 1;
                    }
                    if batch.principal != TRUSTED_PRINCIPAL {
                        violations.push(format!("record by {:?} at {e}", batch.principal));
                    }
                    *records
                        .entry((batch.branch, *rule, curvec_seen.clone()))
                        .or_default() += 1;
                }
                Op::OpenArtifact {
                    ty: ArtifactType::PendingReview,
                    payload_hash,
                    ..
                } if batch.principal == LLM_PRINCIPAL => match snap.log.read_blob(payload_hash) {
                    Payload::Available(b) => match ReviewDoc::from_bytes(&b) {
                        Ok(d) => *reviews.entry((d.branch, d.rule, d.curvec)).or_default() += 1,
                        Err(e) => violations.push(format!("review doc at {e:?}")),
                    },
                    other => violations.push(format!("review blob at {e}: {other:?}")),
                },
                _ => {}
            }
        }
    }
    if llm_records > 0 {
        violations.push(format!("{llm_records} records from the LLM principal"));
    }
    for (k, n) in records.iter().filter(|(_, n)| **n > 1) {
        violations.push(format!("duplicate record {k:?} ×{n}"));
    }
    for (k, n) in reviews.iter().filter(|(_, n)| **n > 1) {
        violations.push(format!("duplicate review {k:?} ×{n}"));
    }
    let pending_main = snap.view(MAIN).pending_rules();
    if !pending_main.is_empty() {
        violations.push(format!("lost on main: {pending_main:?}"));
    }
    for (rule, curvec) in snap.view(dev).pending_rules() {
        if !reviews.contains_key(&(dev, Some(rule), curvec.clone())) {
            violations.push(format!("lost on dev: {rule:?} {curvec:?}"));
        }
    }
    for state in ["queued", "running", "failed"] {
        if counter(&r.jobs, state) > 0 {
            violations.push(format!("{} jobs {state}", counter(&r.jobs, state)));
        }
    }
    let used: BTreeSet<_> = records.keys().map(|k| k.0).collect();
    println!(
        "validate-stress {seconds}s: moves {} (cas conflicts {}), epoch {}, records {} on {:?}, worker reviews {}, jobs done {} superseded {}, calls {}, store errors {}, violations {}",
        moves.load(Ordering::Relaxed),
        conflicts.load(Ordering::Relaxed),
        snap.epoch.0,
        records.len(),
        used,
        reviews.len(),
        counter(&r.jobs, "done"),
        counter(&r.jobs, "superseded"),
        counter(&r.stats, "calls"),
        counter(&r.stats, "store_errors"),
        violations.len()
    );
    for v in violations.iter().take(20) {
        println!("VIOLATION {v}");
    }
    drop(snap);
    daemon.stop();
    let _ = std::fs::remove_dir_all(&dir);
    if violations.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
