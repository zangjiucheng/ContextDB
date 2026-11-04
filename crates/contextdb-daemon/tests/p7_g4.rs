//! P7 G4 robustness (ADR 0015 §11).
//!
//! - **Process death at each job step.** A real `ctxd` child aborts (no
//!   destructors, as SIGKILL) at `claimed`, `called`, `committed` or `done`
//!   (`CTX_JOB_CRASH`, debug builds only); the restarted daemon ends with
//!   exactly one record (trusted) or one PendingReview (LLM), and calls the
//!   validator again only if the first call's result was lost.
//! - **Disk full.** The blob store refuses the worker's commit, and the job
//!   table refuses to grow: no record is lost or duplicated, and the work
//!   completes once space returns.
use contextdb_daemon::{
    actor::{self, ActorHandle},
    jobs::{self, JobState, JobTable},
    validators::{Configured, Loaded, Ran, Scheduler},
};
use contextdb_model::*;
use contextdb_proto::client::Client;
use contextdb_proto::docs::{Label, ValidatorKind};
use contextdb_proto::proto::*;
use contextdb_store::{OpenOptions, SimDisk, Store, TailFate};
use contextdb_validate::config::{Filter, Limits};
use contextdb_validate::mock::{Mock, Script};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ctx-p7-g4-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
fn h(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}
fn runs() -> usize {
    std::env::var("CONTEXTDB_P7_G4_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

/// A `ctxd` child on `dir`; returns it and its admin socket.
fn spawn(dir: &Path, crash: Option<&str>) -> (Child, PathBuf) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ctxd"));
    cmd.arg(dir).stdout(Stdio::piped()).stderr(Stdio::null());
    match crash {
        Some(step) => cmd.env("CTX_JOB_CRASH", step),
        None => cmd.env_remove("CTX_JOB_CRASH"),
    };
    let mut child = cmd.spawn().unwrap();
    let out = BufReader::new(child.stdout.take().unwrap());
    let mut admin = None;
    for line in out.lines() {
        let line = line.unwrap();
        if let Some(p) = line.strip_prefix("listening ")
            && p.ends_with("admin.sock")
        {
            admin = Some(PathBuf::from(p));
            break;
        }
    }
    (child, admin.expect("ctxd printed its admin socket"))
}
fn raw(c: &mut Client, blobs: Vec<Vec<u8>>, ops: Vec<Op>) -> CommitResult {
    let r = c
        .call(&Request::Raw {
            blobs,
            batch: Batch::new(ops),
        })
        .unwrap();
    match r.response {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
/// a (Fact) and b (Decision) ← Sem a; a moves, so the rule is Pending.
fn pending_rule(c: &mut Client) {
    let fact = |key: &str, kind, body: &str| Op::CreateLogical {
        key: key.into(),
        kind,
        scope_lane: "repo".into(),
        content: h(body.as_bytes()),
        adopt_as_base: true,
    };
    raw(
        c,
        vec![b"a: postgres".to_vec(), b"b: use sql joins".to_vec()],
        vec![
            fact("a", RecordKind::Fact, "a: postgres"),
            fact("b", RecordKind::Decision, "b: use sql joins"),
        ],
    );
    raw(
        c,
        Vec::new(),
        vec![Op::AddRule {
            conclusion: RevisionId(1).into(),
            incidences: vec![(RevisionId(0).into(), EdgeKind::Sem)],
            enabled: true,
        }],
    );
    raw(
        c,
        vec![b"a: mongodb".to_vec()],
        vec![
            Op::AddRevision {
                logical: LogicalId(0).into(),
                content: h(b"a: mongodb"),
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: LogicalId(0).into(),
                expected: RevisionId(0).into(),
                new: RevRef::New(0),
            },
        ],
    );
}
fn validators_toml(dir: &Path, trusted: bool) {
    let calls = dir.join("calls");
    let script = format!(
        "cat > /dev/null; echo x >> '{}'; echo '{{\"label\": \"conflict\", \"rationale\": \"moved\", \"evidence\": []}}'",
        calls.display()
    );
    let toml = format!(
        "[limits]\nworkers = 1\n[[validator]]\nname = \"v\"\ntype = \"command\"\ncommand = [\"/bin/sh\", \"-c\", {script:?}]\ntrusted = {trusted}\n"
    );
    std::fs::write(dir.join("validators.toml"), toml).unwrap();
}
fn calls(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("calls"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}
fn report(c: &mut Client) -> ValidatorsReport {
    match (c.call(&Request::Validators {
        action: ValidatorsAction::Status,
    }))
    .unwrap()
    .response
    {
        Response::Validators { report } => report,
        other => panic!("{other:?}"),
    }
}
fn count(r: &[Counter], name: &str) -> u64 {
    r.iter().find(|c| c.name == name).map_or(0, |c| c.value)
}
/// Records and PendingReviews on main, read from the reopened store
/// after the daemon has stopped.
fn outcome(dir: &Path) -> (usize, usize) {
    let disk = contextdb_store::StdDisk::default();
    let (store, _) = Store::open(disk, dir, &OpenOptions::default()).unwrap();
    let k = store.kernel().unwrap();
    let view = k.branch(MAIN).unwrap();
    let reviews = (view.artifacts())
        .filter(|(_, x)| x.ty == ArtifactType::PendingReview)
        .count();
    (view.db().inputs().validations.len(), reviews)
}

/// Kills `ctxd` at each job step, restarts it, and checks that the job
/// ends once.
#[test]
fn kill_at_each_job_step_ends_with_one_outcome() {
    if !cfg!(debug_assertions) {
        eprintln!("crash points are compiled out of release builds; skipped");
        return;
    }
    // (step, validator calls in total)
    for (step, want_calls) in [("claimed", 1), ("called", 2), ("committed", 1), ("done", 1)] {
        for trusted in [true, false] {
            for run in 0..runs() {
                let dir = temp(step);
                validators_toml(&dir, trusted);
                let (mut child, sock) = spawn(&dir, Some(step));
                {
                    let mut c = Client::connect(&sock).unwrap();
                    pending_rule(&mut c);
                }
                let deadline = Instant::now() + Duration::from_secs(30);
                let status = loop {
                    if let Some(s) = child.try_wait().unwrap() {
                        break s;
                    }
                    assert!(Instant::now() < deadline, "{step}: ctxd did not crash");
                    std::thread::sleep(Duration::from_millis(10));
                };
                assert!(!status.success(), "{step}: {status:?}");
                let (mut child, sock) = spawn(&dir, None);
                let mut c = Client::connect(&sock).unwrap();
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    let r = report(&mut c);
                    if count(&r.jobs, "done") == 1 {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "{step} trusted={trusted} run {run}: {r:?}"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = c.call(&Request::Shutdown);
                let _ = child.wait();
                let want = if trusted { (1, 0) } else { (0, 1) };
                assert_eq!(outcome(&dir), want, "{step} trusted={trusted} run {run}");
                assert_eq!(
                    calls(&dir),
                    want_calls,
                    "{step} trusted={trusted} run {run}: validator calls"
                );
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}

type Actor = ActorHandle<SimDisk>;
fn write(a: &Actor, blobs: Vec<Vec<u8>>, ops: Vec<Op>) -> CommitResult {
    let mut batch = Batch::new(ops);
    batch.channel = Channel::Admin;
    a.write(blobs, batch).unwrap().unwrap()
}
/// `n` Decisions, each ← Sem a single moved Fact: `n` Pending rules.
fn many_pending(a: &Actor, n: u32) {
    let mut ops = vec![Op::CreateLogical {
        key: "a".into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: h(b"a0"),
        adopt_as_base: true,
    }];
    let mut blobs = vec![b"a0".to_vec()];
    for k in 0..n {
        let body = format!("decision {k}");
        ops.push(Op::CreateLogical {
            key: format!("d{k}"),
            kind: RecordKind::Decision,
            scope_lane: "repo".into(),
            content: h(body.as_bytes()),
            adopt_as_base: true,
        });
        blobs.push(body.into_bytes());
    }
    write(a, blobs, ops);
    write(
        a,
        Vec::new(),
        (1..=n)
            .map(|k| Op::AddRule {
                conclusion: RevisionId(k).into(),
                incidences: vec![(RevisionId(0).into(), EdgeKind::Sem)],
                enabled: true,
            })
            .collect(),
    );
    write(
        a,
        vec![b"a1".to_vec()],
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
    );
}
fn scheduler(a: &Actor, dir: &Path, m: Arc<Mock>) -> Arc<Scheduler<SimDisk>> {
    let mut s = Scheduler::new(
        a.clone(),
        JobTable::open(&dir.join(jobs::FILE)).unwrap(),
        None,
        Loaded {
            limits: Limits {
                workers: 1,
                lease_secs: 30,
                attempts: 5,
                llm_calls_per_hour: 1000,
            },
            candidates: Default::default(),
            validators: vec![Configured {
                filter: Filter::default(),
                validator: m,
                note: "test".into(),
                pairs: false,
            }],
            error: None,
        },
    );
    Arc::get_mut(&mut s).unwrap().retry_base = Duration::ZERO;
    s
}
/// Runs the worker until nothing is claimable, a job is requeued for a
/// store failure, or the job table fails three times.
fn drain(s: &Scheduler<SimDisk>) -> (Vec<Ran>, usize) {
    let (mut out, mut errors) = (Vec::new(), 0);
    loop {
        match s.work_once() {
            Ok(Some(r)) => {
                let requeued = matches!(r, Ran::Requeued(_));
                out.push(r);
                if requeued {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) if errors < 3 => errors += 1,
            Err(_) => break,
        }
        assert!(out.len() < 10_000);
    }
    (out, errors)
}
fn records(a: &Actor) -> usize {
    a.snapshot().view(MAIN).db().inputs().validations.len()
}

/// The blob store fails the worker's commit (a full disk at the pack
/// append): the job is retried, and ends with exactly one record; the
/// validator is called once more for the lost result, never more.
#[test]
fn blob_store_full_during_a_worker_commit() {
    for op in ["append", "sync", "write"] {
        let disk = SimDisk::new();
        let (store, _) =
            Store::open(disk.clone(), Path::new("/db"), &OpenOptions::default()).unwrap();
        let a = actor::spawn(store).unwrap();
        many_pending(&a, 1);
        let dir = temp("blob");
        let m = Arc::new(Mock::new("mock", ValidatorKind::Deterministic));
        m.set_default(Script::Reply(Mock::outcome(Label::Reinforcement)));
        let s = scheduler(&a, &dir, m.clone());
        s.discover_once().unwrap();
        disk.fail_next(op);
        let (first, _) = drain(&s);
        let (a, s) = if records(&a) == 1 {
            (a, s)
        } else {
            // A failed write or sync poisons the store (fail-stop): the job
            // was requeued without using an attempt; restart on what is
            // durable, as the daemon would.
            assert!(
                matches!(first.last(), Some(Ran::Requeued(_))),
                "{op}: {first:?}"
            );
            drop(s);
            let after = disk.crash(TailFate::Torn, 7);
            let (store, _) = Store::open(after, Path::new("/db"), &OpenOptions::default()).unwrap();
            let a = actor::spawn(store).unwrap();
            let s = scheduler(&a, &dir, m.clone());
            s.jobs.recover().unwrap();
            s.discover_once().unwrap();
            let (again, _) = drain(&s);
            assert!(
                matches!(again.last(), Some(Ran::Recorded(_))),
                "{op}: {first:?} then {again:?}"
            );
            (a, s)
        };
        assert_eq!(records(&a), 1, "{op}");
        assert!(m.calls() <= 2, "{op}: {} calls", m.calls());
        let counts = s.jobs.counts().unwrap();
        assert_eq!(counts[JobState::Failed as usize], 0, "{op}: {counts:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The job table cannot grow (a full disk): discovery fails without
/// losing its place, claims and finishes keep working on existing rows,
/// and once space returns every Pending rule gets exactly one record.
#[test]
fn job_table_full_loses_and_duplicates_nothing() {
    let disk = SimDisk::new();
    let (store, _) = Store::open(disk, Path::new("/db"), &OpenOptions::default()).unwrap();
    let a = actor::spawn(store).unwrap();
    let dir = temp("jobs");
    let m = Arc::new(Mock::new("mock", ValidatorKind::Deterministic));
    m.set_default(Script::Reply(Mock::outcome(Label::Reinforcement)));
    let s = scheduler(&a, &dir, m.clone());
    s.discover_once().unwrap();
    s.jobs.limit_pages(Some(0)).unwrap();
    many_pending(&a, 400);
    let full = s.discover_once();
    assert!(full.is_err(), "the full table refuses new rows: {full:?}");
    let (_, errors) = drain(&s);
    let before = records(&a);
    assert!(before < 400, "{before}");
    // Space returns; the daemon restarts (rows a dead claim left Running
    // are requeued).
    s.jobs.limit_pages(None).unwrap();
    s.jobs.recover().unwrap();
    s.discover_once().unwrap();
    let (ran, late) = drain(&s);
    assert_eq!(late, 0);
    assert_eq!(records(&a), 400);
    assert!(
        m.calls() <= 400 + errors,
        "{} calls, {errors} table errors",
        m.calls()
    );
    assert!(a.snapshot().view(MAIN).pending_rules().is_empty());
    let counts = s.jobs.counts().unwrap();
    assert_eq!(counts[JobState::Done as usize], 400, "{counts:?}");
    eprintln!(
        "job table full: {before} records before space returned, {errors} table errors, {} jobs after",
        ran.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
