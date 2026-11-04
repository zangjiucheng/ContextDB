//! P7 milestone 4 (ADR 0015 §4.2–§4.4): validator jobs: discovery, claims,
//! leases, recovery and the label mapping. T49, T50, T53, T84 and the
//! discovery quiescence property.
use contextdb_daemon::{
    actor::{self, ActorHandle},
    jobs::{self, Job, JobState, JobTable},
    validators::{self, Configured, LLM_PRINCIPAL, Loaded, Ran, Scheduler, TRUSTED_PRINCIPAL},
};
use contextdb_model::*;
use contextdb_proto::docs::{Label, ReviewDoc, Suggestion, ValidationMeta, ValidatorKind};
use contextdb_proto::proto::ValidatorsAction;
use contextdb_store::{OpenOptions, Payload, SimDisk, Store};
use contextdb_validate::config::{Filter, Limits};
use contextdb_validate::mock::{Mock, Script};
use contextdb_validate::{Outcome, Task, Validator, ValidatorError};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

type Actor = ActorHandle<SimDisk>;

fn temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ctx-p7-jobs-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
fn actor() -> Actor {
    let disk = SimDisk::new();
    let (store, _) =
        Store::open(disk, std::path::Path::new("/db"), &OpenOptions::default()).unwrap();
    actor::spawn(store).unwrap()
}
fn write_on(a: &Actor, branch: BranchId, blobs: Vec<Vec<u8>>, ops: Vec<Op>) -> CommitResult {
    static TXN: AtomicU64 = AtomicU64::new(1);
    let mut batch = Batch::new(ops);
    batch.branch = branch;
    batch.channel = Channel::Admin;
    let mut id = [0xEE; 16];
    id[..8].copy_from_slice(&TXN.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    batch.client_txn_id = Some(id);
    a.write(blobs, batch).unwrap().unwrap()
}
fn write(a: &Actor, blobs: Vec<Vec<u8>>, ops: Vec<Op>) -> CommitResult {
    write_on(a, MAIN, blobs, ops)
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
fn h(body: &[u8]) -> ContentHash {
    *blake3::hash(body).as_bytes()
}
/// a = r0 (Fact), b = r1 (Decision) ← Sem a@r0; a moves to r2: rule 0 is
/// Pending at [(a, r2)].
fn setup(a: &Actor) -> HeadVector {
    write(
        a,
        vec![b"a: postgres".to_vec(), b"b: use sql joins".to_vec()],
        vec![
            Op::CreateLogical {
                key: "a".into(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: h(b"a: postgres"),
                adopt_as_base: true,
            },
            Op::CreateLogical {
                key: "b".into(),
                kind: RecordKind::Decision,
                scope_lane: "repo".into(),
                content: h(b"b: use sql joins"),
                adopt_as_base: true,
            },
        ],
    );
    write(
        a,
        Vec::new(),
        vec![Op::AddRule {
            conclusion: RevisionId(1).into(),
            incidences: vec![(RevisionId(0).into(), EdgeKind::Sem)],
            enabled: true,
        }],
    );
    move_head(a, MAIN, LogicalId(0), b"a: mongodb")
}
/// Adds a revision of `l` with `body` and makes it the head; returns the
/// new single-premise vector.
fn move_head(a: &Actor, branch: BranchId, l: LogicalId, body: &[u8]) -> HeadVector {
    let snap = a.snapshot();
    let head = snap.view(branch).head(l).unwrap();
    drop(snap);
    let r = write_on(
        a,
        branch,
        vec![body.to_vec()],
        vec![
            Op::AddRevision {
                logical: l.into(),
                content: h(body),
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: l.into(),
                expected: head.into(),
                new: RevRef::New(0),
            },
        ],
    );
    vec![(l, r.allocations.new_revisions[0])]
}
fn limits() -> Limits {
    Limits {
        workers: 1,
        lease_secs: 30,
        attempts: 3,
        llm_calls_per_hour: 200,
    }
}
fn loaded(v: Arc<dyn Validator>, limits: Limits) -> Loaded {
    Loaded {
        limits,
        candidates: Default::default(),
        validators: vec![Configured {
            filter: Filter::default(),
            validator: v,
            note: "test".into(),
            pairs: false,
        }],
        error: None,
    }
}
fn scheduler(a: &Actor, v: Arc<dyn Validator>) -> Arc<Scheduler<SimDisk>> {
    let dir = temp("s");
    let mut s = Scheduler::new(
        a.clone(),
        JobTable::open(&dir.join(jobs::FILE)).unwrap(),
        None,
        loaded(v, limits()),
    );
    // Tests do not wait out retry backoff.
    Arc::get_mut(&mut s).unwrap().retry_base = Duration::ZERO;
    s
}
fn mock(kind: ValidatorKind, label: Label) -> Arc<Mock> {
    let m = Arc::new(Mock::new("mock", kind));
    m.set_default(Script::Reply(Mock::outcome(label)));
    m
}
fn job_id(branch: BranchId, rule: RuleId, curvec: &HeadVector, generation: u32) -> [u8; 16] {
    jobs::txn_id(&jobs::claim_key(branch, rule, curvec), generation)
}
fn validations(a: &Actor, branch: BranchId) -> usize {
    a.snapshot().view(branch).db().inputs().validations.len()
}
fn reviews(a: &Actor, branch: BranchId) -> Vec<Artifact> {
    let snap = a.snapshot();
    let view = snap.view(branch);
    (view.artifacts())
        .map(|(_, x)| x)
        .filter(|x| x.ty == ArtifactType::PendingReview)
        .collect()
}
fn state(s: &Scheduler<SimDisk>, branch: BranchId, curvec: &HeadVector) -> JobState {
    let key = jobs::claim_key(branch, RuleId(0), curvec);
    s.jobs.get(&key).unwrap().unwrap().state
}

#[test]
fn discovery_scans_at_start_then_follows_head_moves() {
    let a = actor();
    let v1 = setup(&a);
    let s = scheduler(&a, mock(ValidatorKind::Deterministic, Label::Reinforcement));
    assert_eq!(s.discover_once().unwrap(), 1, "start-up scan");
    assert_eq!(s.discover_once().unwrap(), 0, "nothing new");
    let v2 = move_head(&a, MAIN, LogicalId(0), b"a: sqlite");
    assert_eq!(s.discover_once().unwrap(), 1, "the head move's rule");
    assert_eq!(state(&s, MAIN, &v1), JobState::Queued);
    assert_eq!(state(&s, MAIN, &v2), JobState::Queued);
    // The old key is no longer Pending at its vector: claimed only to be
    // marked Superseded; the new one is validated.
    assert_eq!(s.work_once().unwrap(), Some(Ran::Superseded));
    assert_eq!(
        s.work_once().unwrap(),
        Some(Ran::Recorded(Verdict::Verified))
    );
    assert_eq!(state(&s, MAIN, &v1), JobState::Superseded);
    assert_eq!(state(&s, MAIN, &v2), JobState::Done);
    assert_eq!(
        a.snapshot().db().status(RevisionId(1)).unwrap(),
        Summary::Certified
    );
    // The new record is no job; nothing loops.
    assert_eq!(s.discover_once().unwrap(), 0);
    assert_eq!(s.work_once().unwrap(), None);
}

/// A trusted outcome is recorded with its metadata, under the trusted
/// worker principal, `if_absent`.
#[test]
fn trusted_conflict_records_rejected_with_metadata() {
    let a = actor();
    let v = setup(&a);
    let s = scheduler(&a, mock(ValidatorKind::Deterministic, Label::Conflict));
    s.discover_once().unwrap();
    assert_eq!(
        s.work_once().unwrap(),
        Some(Ran::Recorded(Verdict::Rejected))
    );
    let snap = a.snapshot();
    let record = snap.db().inputs().validations[&(RuleId(0), v.clone())].clone();
    assert_eq!(record.verdict, Verdict::Rejected);
    let Payload::Available(bytes) = snap.log.read_blob(&record.meta.unwrap()) else {
        panic!("meta blob")
    };
    let meta = ValidationMeta::from_bytes(&bytes).unwrap();
    assert_eq!(meta.kind, ValidatorKind::Deterministic);
    assert_eq!(meta.label, Label::Conflict);
    assert_eq!(meta.job, Some(job_id(MAIN, RuleId(0), &v, 0)));
    let row = snap.log.read_row(snap.epoch).unwrap().batch().unwrap();
    assert_eq!(row.principal, TRUSTED_PRINCIPAL);
    assert_eq!(row.channel, Channel::Admin);
    assert!(matches!(
        row.ops[0],
        Op::RecordValidation {
            if_absent: true,
            ..
        }
    ));
}

/// BoundaryChange/NewRule from a trusted validator writes no record; the
/// outcome stays in the job row, and discovery does not loop on it.
#[test]
fn trusted_boundary_change_is_kept_in_the_row() {
    let a = actor();
    let v = setup(&a);
    let s = scheduler(
        &a,
        mock(ValidatorKind::Deterministic, Label::BoundaryChange),
    );
    s.discover_once().unwrap();
    assert_eq!(
        s.work_once().unwrap(),
        Some(Ran::Kept(Label::BoundaryChange))
    );
    assert_eq!(validations(&a, MAIN), 0);
    let row = s
        .jobs
        .get(&jobs::claim_key(MAIN, RuleId(0), &v))
        .unwrap()
        .unwrap();
    assert_eq!(row.state, JobState::Done);
    let meta = ValidationMeta::from_bytes(&row.outcome_meta.unwrap()).unwrap();
    assert_eq!(meta.label, Label::BoundaryChange);
    assert_eq!(s.discover_once().unwrap(), 0);
    assert_eq!(s.work_once().unwrap(), None);
}

/// T50: an LLM `conflict` opens a PendingReview (suggestion Reject) on the
/// conclusion under the LLM principal; no record, `sem` stays Pending.
#[test]
fn t50_llm_conflict_opens_a_review_only() {
    let a = actor();
    let v = setup(&a);
    let s = scheduler(&a, mock(ValidatorKind::Llm, Label::Conflict));
    s.discover_once().unwrap();
    assert_eq!(
        s.work_once().unwrap(),
        Some(Ran::Reviewed(Suggestion::Reject))
    );
    assert_eq!(validations(&a, MAIN), 0);
    let snap = a.snapshot();
    assert_eq!(
        snap.view(MAIN).rule_semantic(RuleId(0)).unwrap().sem,
        SemState::Pending
    );
    let r = reviews(&a, MAIN);
    assert_eq!(r.len(), 1);
    assert!(r[0].open);
    assert_eq!(r[0].target, RevisionId(1));
    assert_eq!(r[0].author, LLM_PRINCIPAL);
    let Payload::Available(bytes) = snap.log.read_blob(&r[0].payload_hash) else {
        panic!("review blob")
    };
    let doc = ReviewDoc::from_bytes(&bytes).unwrap();
    assert_eq!(doc.suggestion, Suggestion::Reject);
    assert_eq!(
        (doc.branch, doc.rule, doc.curvec),
        (MAIN, Some(RuleId(0)), v)
    );
    let Payload::Available(meta) = snap.log.read_blob(&doc.meta) else {
        panic!("meta blob")
    };
    assert_eq!(
        ValidationMeta::from_bytes(&meta).unwrap().kind,
        ValidatorKind::Llm
    );
}

/// T50 property: over 10,000 random outcomes the LLM mapping never builds a
/// `RecordValidation`, and the pre-submit guard refuses one that does.
#[test]
fn t50_property_llm_batches_never_carry_a_record() {
    let mut rng = ChaCha8Rng::seed_from_u64(0x5050);
    let labels = [
        Label::Reinforcement,
        Label::None,
        Label::Conflict,
        Label::BoundaryChange,
        Label::NewRule,
    ];
    for n in 0..10_000u32 {
        let outcome = Outcome {
            label: labels[(rng.next_u32() % 5) as usize],
            rationale: format!("r{n}"),
            evidence: Vec::new(),
            model: Some("m".into()),
            prompt_sha256: None,
            tokens: Some((rng.next_u32() % 1000, rng.next_u32() % 100)),
        };
        let job = Job {
            key: [n as u8; 32],
            branch: BranchId((rng.next_u32() % 3) as u16),
            rule: RuleId(rng.next_u32() % 50),
            curvec: vec![(
                LogicalId(rng.next_u32() % 9),
                RevisionId(rng.next_u32() % 99),
            )],
            generation: rng.next_u32() % 3,
            token: 1,
            state: JobState::Running,
            lease_until: 0,
            attempts: 0,
            last_error: None,
            outcome_meta: None,
        };
        let plan = validators::plan(
            ValidatorKind::Llm,
            &outcome,
            &job,
            RevisionId(1),
            [1; 32],
            "llm",
        )
        .unwrap();
        let validators::Plan::Commit { principal, ops, .. } = plan else {
            panic!("an LLM outcome always opens a review");
        };
        assert_eq!(principal, LLM_PRINCIPAL);
        assert!(ops.iter().all(|op| matches!(
            op,
            Op::OpenArtifact {
                ty: ArtifactType::PendingReview,
                ..
            }
        )));
    }
    let mut batch = Batch::new(vec![Op::RecordValidation {
        rule: RuleId(0).into(),
        curvec_seen: Vec::new(),
        verdict: Verdict::Verified,
        validator: "llm".into(),
        meta: None,
        if_absent: true,
    }]);
    batch.principal = LLM_PRINCIPAL;
    assert!(validators::refuse_llm_validation(&batch).is_some());
    batch.principal = TRUSTED_PRINCIPAL;
    assert!(validators::refuse_llm_validation(&batch).is_none());
}

/// T49: the vector moves while the validator runs; the commit fails with
/// `CurvecMoved`, the job is Superseded, and the new vector is a new job.
#[test]
fn t49_curvec_moved_during_validation_supersedes_and_requeues() {
    let a = actor();
    let v1 = setup(&a);
    let m = mock(ValidatorKind::Deterministic, Label::Reinforcement);
    m.script(
        job_id(MAIN, RuleId(0), &v1, 0),
        vec![Script::Delay(Duration::from_millis(600))],
    );
    let s = scheduler(&a, m.clone());
    s.discover_once().unwrap();
    let worker = {
        let s = s.clone();
        std::thread::spawn(move || s.work_once().unwrap())
    };
    // Wait until the validator is running, then move the premise.
    let start = Instant::now();
    while m.calls() == 0 {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    let v2 = move_head(&a, MAIN, LogicalId(0), b"a: cassandra");
    assert_eq!(worker.join().unwrap(), Some(Ran::Superseded));
    assert_eq!(state(&s, MAIN, &v1), JobState::Superseded);
    assert_eq!(validations(&a, MAIN), 0);
    assert_eq!(s.discover_once().unwrap(), 1);
    assert_eq!(
        s.work_once().unwrap(),
        Some(Ran::Recorded(Verdict::Verified))
    );
    assert_eq!(state(&s, MAIN, &v2), JobState::Done);
    assert_eq!(validations(&a, MAIN), 1);
}

/// ABA: v1 → v2 → v1. The Superseded v1 row is requeued when v1 is Pending
/// again, which the kernel CAS allows.
#[test]
fn superseded_key_is_requeued_after_aba() {
    let a = actor();
    let v1 = setup(&a);
    let s = scheduler(&a, mock(ValidatorKind::Deterministic, Label::Reinforcement));
    s.discover_once().unwrap();
    let r_v1 = v1[0].1;
    let v2 = move_head(&a, MAIN, LogicalId(0), b"a: v2");
    s.discover_once().unwrap();
    assert_eq!(s.work_once().unwrap(), Some(Ran::Superseded));
    assert_eq!(state(&s, MAIN, &v1), JobState::Superseded);
    // Back to v1's revision.
    write(
        &a,
        Vec::new(),
        vec![Op::SetHead {
            logical: LogicalId(0).into(),
            expected: v2[0].1.into(),
            new: r_v1.into(),
        }],
    );
    assert_eq!(s.discover_once().unwrap(), 1, "requeued");
    assert_eq!(state(&s, MAIN, &v1), JobState::Queued);
    // v2's row is now stale; v1 runs.
    let mut ran = Vec::new();
    while let Some(r) = s.work_once().unwrap() {
        ran.push(r);
    }
    assert!(ran.contains(&Ran::Recorded(Verdict::Verified)), "{ran:?}");
    assert_eq!(state(&s, MAIN, &v1), JobState::Done);
}

/// T53: of several claimers (separate connections to one table) exactly one
/// claims a row.
#[test]
fn t53_exactly_one_of_concurrent_claimers_wins() {
    let dir = temp("claim");
    let path = dir.join(jobs::FILE);
    let t = JobTable::open(&path).unwrap();
    t.discover(MAIN, RuleId(0), &vec![(LogicalId(0), RevisionId(2))])
        .unwrap();
    for round in 0..20 {
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (path, barrier) = (path.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let t = JobTable::open(&path).unwrap();
                    barrier.wait();
                    t.claim(1_000, 60_000, |_| true).unwrap()
                })
            })
            .collect();
        let won: Vec<Job> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(won.len(), 1, "round {round}");
        // Back to Queued for the next round.
        assert!(t.recover().unwrap() == 1);
    }
}

/// Review finding M1: rows no validator's filter takes stay Queued; more
/// than one page of them (256) must not hide a later row a validator takes.
#[test]
fn declined_rows_never_hide_a_claimable_one() {
    let dir = temp("declined");
    let t = JobTable::open(&dir.join(jobs::FILE)).unwrap();
    let dev = BranchId(1);
    let keys: Vec<_> = (0..600)
        .map(|j| (dev, RuleId(j), vec![(LogicalId(0), RevisionId(2))]))
        .collect();
    assert_eq!(t.discover_many(&keys).unwrap(), 600);
    t.discover(MAIN, RuleId(9_999), &vec![(LogicalId(0), RevisionId(2))])
        .unwrap();
    let job = t.claim(1_000, 60_000, |j| j.branch == MAIN).unwrap();
    assert_eq!(job.map(|j| (j.branch, j.rule)), Some((MAIN, RuleId(9_999))));
    assert!(
        t.claim(1_000, 60_000, |j| j.branch == MAIN)
            .unwrap()
            .is_none()
    );
}

/// T53: a worker that committed and died before marking the row: after
/// recovery the generation's commit is found through `TxnStatus` and the
/// validator is not called again; exactly one record or review.
#[test]
fn t53_crash_after_commit_before_finish() {
    for kind in [ValidatorKind::Deterministic, ValidatorKind::Llm] {
        let a = actor();
        let v = setup(&a);
        let m = mock(kind, Label::Conflict);
        let s = scheduler(&a, m.clone());
        s.discover_once().unwrap();
        // The dead worker: claimed, validated, committed, never finished.
        let job = s.jobs.claim(now_ms(), 60_000, |_| true).unwrap().unwrap();
        let outcome = Mock::outcome(Label::Conflict);
        let meta = ValidationMeta {
            v: 1,
            kind,
            validator: "mock".into(),
            version: "mock".into(),
            model: None,
            prompt_sha256: None,
            label: Label::Conflict,
            rationale: outcome.rationale.clone(),
            evidence: Vec::new(),
            tokens_in: None,
            tokens_out: None,
            job: Some(job.client_txn_id()),
            based_on: None,
            at: 0,
        };
        let (hash, bytes) = contextdb_proto::docs::blob(&meta).unwrap();
        let validators::Plan::Commit {
            principal,
            mut blobs,
            ops,
            ..
        } = validators::plan(kind, &outcome, &job, RevisionId(1), hash, "mock").unwrap()
        else {
            panic!()
        };
        blobs.insert(0, bytes);
        let mut batch = Batch::new(ops);
        batch.channel = Channel::Admin;
        batch.principal = principal;
        batch.client_txn_id = Some(job.client_txn_id());
        a.write(blobs, batch).unwrap().unwrap();
        // Restart.
        assert_eq!(s.jobs.recover().unwrap(), 1);
        assert_eq!(s.work_once().unwrap(), Some(Ran::AlreadyCommitted));
        assert_eq!(m.calls(), 0, "{kind:?}: no second call");
        assert_eq!(state(&s, MAIN, &v), JobState::Done);
        let (records, open) = (validations(&a, MAIN), reviews(&a, MAIN).len());
        match kind {
            ValidatorKind::Llm => assert_eq!((records, open), (0, 1)),
            _ => assert_eq!((records, open), (1, 0)),
        }
    }
}

/// T53: a worker that died after claiming (before or during the call):
/// recovery requeues the row and the job runs once.
#[test]
fn t53_crash_after_claim_requeues() {
    let a = actor();
    let v = setup(&a);
    let s = scheduler(&a, mock(ValidatorKind::Llm, Label::Reinforcement));
    s.discover_once().unwrap();
    let dead = s.jobs.claim(now_ms(), 60_000, |_| true).unwrap().unwrap();
    assert_eq!(s.work_once().unwrap(), None, "the row is Running");
    assert_eq!(s.jobs.recover().unwrap(), 1);
    assert_eq!(
        s.work_once().unwrap(),
        Some(Ran::Reviewed(Suggestion::Verify))
    );
    // The dead holder's token no longer holds the row.
    assert!(
        !s.jobs
            .finish(&dead.key, dead.token, JobState::Failed, None, None)
            .unwrap()
    );
    assert_eq!(state(&s, MAIN, &v), JobState::Done);
    assert_eq!(reviews(&a, MAIN).len(), 1);
}

/// A validator that ignores its deadline on the first call.
struct Late {
    calls: AtomicUsize,
    kind: ValidatorKind,
}
impl Validator for Late {
    fn kind(&self) -> ValidatorKind {
        self.kind
    }
    fn name(&self) -> &str {
        "late"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn validate(&self, _: &Task, _: Instant) -> Result<Outcome, ValidatorError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(2500));
        }
        Ok(Mock::outcome(Label::Conflict))
    }
}

/// T53: the lease expires while the first holder is still in its call; a
/// second worker takes the row over and commits; the late result commits
/// nothing more (same `client_txn_id`, stale token).
#[test]
fn t53_lease_expiry_late_result_commits_once() {
    for kind in [ValidatorKind::Deterministic, ValidatorKind::Llm] {
        let a = actor();
        let v = setup(&a);
        let late = Arc::new(Late {
            calls: AtomicUsize::new(0),
            kind,
        });
        let s = scheduler(&a, late.clone());
        s.set_loaded(loaded(
            late.clone(),
            Limits {
                lease_secs: 1,
                ..limits()
            },
        ));
        s.discover_once().unwrap();
        let first = {
            let s = s.clone();
            std::thread::spawn(move || s.work_once().unwrap())
        };
        while late.calls.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(1300));
        // Expiry, then the takeover.
        let second = s.work_once().unwrap();
        assert!(
            matches!(second, Some(Ran::Recorded(_) | Ran::Reviewed(_))),
            "{second:?}"
        );
        let first = first.join().unwrap();
        assert!(
            matches!(first, Some(Ran::Lost | Ran::AlreadyCommitted)),
            "{kind:?}: {first:?}"
        );
        assert_eq!(state(&s, MAIN, &v), JobState::Done);
        let (records, open) = (validations(&a, MAIN), reviews(&a, MAIN).len());
        match kind {
            ValidatorKind::Llm => assert_eq!((records, open), (0, 1)),
            _ => assert_eq!((records, open), (1, 0)),
        }
    }
}

/// Failures count attempts and end in `Failed`; `retry --failed` bumps the
/// generation, and the next commit is not answered by an earlier attempt.
#[test]
fn failed_after_attempts_then_retry_bumps_the_generation() {
    let a = actor();
    let v = setup(&a);
    let m = mock(ValidatorKind::Deterministic, Label::Reinforcement);
    m.set_default(Script::Fail(ValidatorError::Failed("boom".into())));
    let s = scheduler(&a, m.clone());
    s.discover_once().unwrap();
    for _ in 0..3 {
        assert!(matches!(s.work_once().unwrap(), Some(Ran::Failed(_))));
    }
    assert_eq!(state(&s, MAIN, &v), JobState::Failed);
    assert_eq!(s.work_once().unwrap(), None);
    let report = s.report(ValidatorsAction::Status).unwrap();
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].attempts, 3);
    assert!(
        report.failed[0]
            .last_error
            .as_deref()
            .unwrap()
            .contains("boom")
    );
    m.set_default(Script::Reply(Mock::outcome(Label::Reinforcement)));
    let report = s
        .report(ValidatorsAction::Retry {
            key: None,
            failed: true,
        })
        .unwrap();
    assert_eq!(report.retried, 1);
    let row = s
        .jobs
        .get(&jobs::claim_key(MAIN, RuleId(0), &v))
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.state, row.generation, row.attempts),
        (JobState::Queued, 1, 0)
    );
    assert_eq!(
        s.work_once().unwrap(),
        Some(Ran::Recorded(Verdict::Verified))
    );
}

/// Over the hourly LLM call cap a job is given back, not failed.
#[test]
fn llm_call_cap_defers_jobs() {
    let a = actor();
    let v = setup(&a);
    let m = mock(ValidatorKind::Llm, Label::None);
    let s = scheduler(&a, m.clone());
    s.set_loaded(loaded(
        m.clone(),
        Limits {
            llm_calls_per_hour: 0,
            ..limits()
        },
    ));
    s.discover_once().unwrap();
    assert_eq!(s.work_once().unwrap(), Some(Ran::Deferred));
    assert_eq!(m.calls(), 0);
    let row = s
        .jobs
        .get(&jobs::claim_key(MAIN, RuleId(0), &v))
        .unwrap()
        .unwrap();
    assert_eq!((row.state, row.attempts), (JobState::Queued, 0));
    assert_eq!(s.work_once().unwrap(), None, "not claimable yet");
}

/// No validator configured: discovery queues, nothing is called.
#[test]
fn no_validator_no_call() {
    let a = actor();
    setup(&a);
    let dir = temp("none");
    let s = Scheduler::new(
        a.clone(),
        JobTable::open(&dir.join(jobs::FILE)).unwrap(),
        None,
        Loaded::default(),
    );
    assert_eq!(s.discover_once().unwrap(), 1);
    assert_eq!(s.work_once().unwrap(), None);
    let report = s.report(ValidatorsAction::Check).unwrap();
    assert!(report.check[0].contains("no validator configured"));
    assert_eq!(report.jobs[0].value, 1, "queued");
}

/// Filters: a job no validator takes stays Queued.
#[test]
fn filters_route_jobs() {
    let a = actor();
    let v = setup(&a);
    let m = mock(ValidatorKind::Deterministic, Label::None);
    let s = scheduler(&a, m.clone());
    let mut l = loaded(m.clone(), limits());
    l.validators[0].filter.kinds = vec![RecordKind::Plan];
    s.set_loaded(l);
    s.discover_once().unwrap();
    assert_eq!(s.work_once().unwrap(), None);
    assert_eq!(state(&s, MAIN, &v), JobState::Queued);
    let mut l = loaded(m.clone(), limits());
    l.validators[0].filter.kinds = vec![RecordKind::Decision];
    s.set_loaded(l);
    assert_eq!(
        s.work_once().unwrap(),
        Some(Ran::Recorded(Verdict::Verified))
    );
}

/// T84: the same rule Pending on two branches is two jobs and two records;
/// a branch created after start-up is picked up by discovery.
#[test]
fn t84_two_branches_two_jobs_two_records() {
    let a = actor();
    setup(&a);
    let s = scheduler(&a, mock(ValidatorKind::Deterministic, Label::Reinforcement));
    assert_eq!(s.discover_once().unwrap(), 1);
    write(
        &a,
        Vec::new(),
        vec![Op::CreateBranch { name: "dev".into() }],
    );
    let dev = a.snapshot().kernel.branch_by_name("dev").unwrap();
    assert_eq!(s.discover_once().unwrap(), 1, "the fork's pending rule");
    let mut ran = Vec::new();
    while let Some(r) = s.work_once().unwrap() {
        ran.push(r);
    }
    assert_eq!(ran, vec![Ran::Recorded(Verdict::Verified); 2]);
    assert_eq!(validations(&a, MAIN), 1);
    assert_eq!(validations(&a, dev), 1);
    // A head move on dev is a job on dev only.
    move_head(&a, dev, LogicalId(0), b"a: dev only");
    assert_eq!(s.discover_once().unwrap(), 1);
    assert_eq!(
        s.work_once().unwrap(),
        Some(Ran::Recorded(Verdict::Verified))
    );
    assert_eq!(validations(&a, MAIN), 1);
    assert_eq!(validations(&a, dev), 2);
}

/// T84: one validator review per (branch, rule, curvec). Within a job
/// generation the `client_txn_id` dedups; beyond it (an earlier
/// generation's commit no longer known, here a review already open), the
/// worker finds the open review and opens none. A manual retry opens a new
/// one anyway, since it was asked for.
#[test]
fn t84_validator_reviews_dedup_by_curvec() {
    let a = actor();
    let v = setup(&a);
    let meta = ValidationMeta {
        v: 1,
        kind: ValidatorKind::Llm,
        validator: "mock".into(),
        version: "mock".into(),
        model: None,
        prompt_sha256: None,
        label: Label::Conflict,
        rationale: "earlier".into(),
        evidence: Vec::new(),
        tokens_in: None,
        tokens_out: None,
        job: None,
        based_on: None,
        at: 0,
    };
    let (meta_hash, meta_bytes) = contextdb_proto::docs::blob(&meta).unwrap();
    let doc = ReviewDoc {
        v: 1,
        suggestion: Suggestion::Reject,
        branch: MAIN,
        rule: Some(RuleId(0)),
        curvec: v.clone(),
        sides: Vec::new(),
        meta: meta_hash,
    };
    let (doc_hash, doc_bytes) = contextdb_proto::docs::blob(&doc).unwrap();
    let mut batch = Batch::new(vec![Op::OpenArtifact {
        ty: ArtifactType::PendingReview,
        target: RevisionId(1).into(),
        payload_hash: doc_hash,
    }]);
    batch.channel = Channel::Admin;
    batch.principal = LLM_PRINCIPAL;
    batch.client_txn_id = Some([0x84; 16]);
    a.write(vec![meta_bytes, doc_bytes], batch)
        .unwrap()
        .unwrap();

    let m = mock(ValidatorKind::Llm, Label::Conflict);
    let s = scheduler(&a, m.clone());
    assert_eq!(s.discover_once().unwrap(), 1);
    assert_eq!(s.work_once().unwrap(), Some(Ran::AlreadyReviewed));
    assert_eq!(m.calls(), 0);
    assert_eq!(reviews(&a, MAIN).len(), 1);
    // A review for another vector does not count: a new vector, a new review.
    let v2 = move_head(&a, MAIN, LogicalId(0), b"a: v2");
    s.discover_once().unwrap();
    let mut ran = Vec::new();
    while let Some(r) = s.work_once().unwrap() {
        ran.push(r);
    }
    assert!(ran.contains(&Ran::Reviewed(Suggestion::Reject)), "{ran:?}");
    assert_eq!(reviews(&a, MAIN).len(), 2);
    assert_eq!(state(&s, MAIN, &v2), JobState::Done);
    // A manual retry of the first key (generation 1) is not deduped; it is
    // Superseded here only because the vector moved on.
    assert!(s.jobs.retry(&jobs::claim_key(MAIN, RuleId(0), &v)).unwrap());
    assert_eq!(s.work_once().unwrap(), Some(Ran::Superseded));
}

/// A rediscovered key in the same generation (a lost job table) is answered
/// by `TxnStatus`: no second call, no second review.
#[test]
fn rediscovered_job_is_answered_by_its_txn_status() {
    let a = actor();
    setup(&a);
    let m = mock(ValidatorKind::Llm, Label::Conflict);
    let s = scheduler(&a, m.clone());
    s.discover_once().unwrap();
    assert_eq!(
        s.work_once().unwrap(),
        Some(Ran::Reviewed(Suggestion::Reject))
    );
    let fresh = scheduler(&a, m.clone());
    assert_eq!(fresh.discover_once().unwrap(), 1);
    assert_eq!(fresh.work_once().unwrap(), Some(Ran::AlreadyCommitted));
    assert_eq!((m.calls(), reviews(&a, MAIN).len()), (1, 1));
}

/// Discovery quiescence (G3): after any history, incremental discovery
/// (once per commit) has a live row for every key a full scan finds
/// Pending, on every branch.
#[test]
fn discovery_quiescence_property() {
    for seed in 0..12u64 {
        let mut rng = ChaCha8Rng::seed_from_u64(0xD15C0 ^ seed);
        let a = actor();
        let s = scheduler(&a, mock(ValidatorKind::Deterministic, Label::None));
        let mut branches = vec![MAIN];
        let mut n = 0u32;
        s.discover_once().unwrap();
        for _ in 0..60 {
            n += 1;
            let b = branches[(rng.next_u32() as usize) % branches.len()];
            let snap = a.snapshot();
            let view = snap.view(b);
            let visible: Vec<LogicalId> = view.logicals().collect();
            let rules = view.db().rule_count() as u32;
            drop(snap);
            match rng.next_u32() % 8 {
                0 | 1 => {
                    let body = format!("body {n}");
                    write_on(
                        &a,
                        b,
                        vec![body.clone().into_bytes()],
                        vec![Op::CreateLogical {
                            key: format!("k{n}"),
                            kind: RecordKind::Decision,
                            scope_lane: "repo".into(),
                            content: h(body.as_bytes()),
                            adopt_as_base: true,
                        }],
                    );
                }
                2 | 3 if visible.len() >= 2 => {
                    let snap = a.snapshot();
                    let view = snap.view(b);
                    let c = visible[(rng.next_u32() as usize) % visible.len()];
                    let p = visible[(rng.next_u32() as usize) % visible.len()];
                    if c == p {
                        continue;
                    }
                    let (c, p) = (view.head(c).unwrap(), view.head(p).unwrap());
                    drop(snap);
                    let mut batch = Batch::new(vec![Op::AddRule {
                        conclusion: c.into(),
                        incidences: vec![(p.into(), EdgeKind::Sem)],
                        enabled: true,
                    }]);
                    batch.branch = b;
                    batch.channel = Channel::Admin;
                    batch.client_txn_id = Some([
                        n as u8, 0xAA, seed as u8, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    ]);
                    // A cycle is refused by the kernel; that is fine.
                    let _ = a.write(Vec::new(), batch).unwrap();
                }
                4 | 5 if !visible.is_empty() => {
                    let l = visible[(rng.next_u32() as usize) % visible.len()];
                    move_head(&a, b, l, format!("rev {n}").as_bytes());
                }
                6 if rules > 0 => {
                    let snap = a.snapshot();
                    let view = snap.view(b);
                    let j = view.global_rule(RuleId(rng.next_u32() % rules));
                    let enabled = view.rule_enabled(j).unwrap();
                    drop(snap);
                    write_on(
                        &a,
                        b,
                        Vec::new(),
                        vec![Op::SetRuleEnabled {
                            rule: j.into(),
                            enabled: !enabled,
                        }],
                    );
                }
                7 if branches.len() < 3 => {
                    write_on(
                        &a,
                        b,
                        Vec::new(),
                        vec![Op::CreateBranch {
                            name: format!("b{n}"),
                        }],
                    );
                    let id = a
                        .snapshot()
                        .kernel
                        .branch_by_name(&format!("b{n}"))
                        .unwrap();
                    branches.push(id);
                }
                _ => {}
            }
            s.discover_once().unwrap();
            // Validate sometimes, so Done rows and records mix in.
            if rng.next_u32() % 4 == 0 {
                let _ = s.work_once().unwrap();
                s.discover_once().unwrap();
            }
        }
        let snap = a.snapshot();
        for info in snap.kernel.branches() {
            let view = snap.kernel.branch(info.id).unwrap();
            for (j, curvec) in view.pending_rules() {
                let row = s
                    .jobs
                    .get(&jobs::claim_key(info.id, j, &curvec))
                    .unwrap()
                    .unwrap_or_else(|| {
                        panic!("seed {seed}: no row for {:?} {j:?} {curvec:?}", info.id)
                    });
                assert_ne!(row.state, JobState::Superseded, "seed {seed}");
            }
        }
    }
}
