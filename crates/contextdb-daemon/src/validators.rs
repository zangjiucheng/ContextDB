//! P7 validator jobs (ADR 0015 §4.2–§4.4): discovery, the worker pool,
//! leases and recovery, outside the writer.
//!
//! - **Discovery** follows the published epochs. For each new op_log row it
//!   collects the rules the row can have made Pending on its branch: rules
//!   with a Sem incidence on a logical whose head moved (`sem_by_logical`),
//!   new and re-enabled rules, and rules given a `RecordValidation`. A row
//!   with a branch op (`CreateBranch`, `Merge`, `ArchiveBranch`) rescans
//!   every branch, as does start-up. A rule becomes a job when it is
//!   enabled and `sem` is `Pending`, keyed `(branch, rule, curvec)`.
//! - **Workers** claim a row marker-first, re-check the key against the
//!   current snapshot, build the task, call the validator within the lease
//!   and commit on the Admin channel with `client_txn_id = H("job",
//!   claim_key, generation)`. Trusted validators (principal
//!   [`TRUSTED_PRINCIPAL`]) write `RecordValidation{if_absent}`; LLM
//!   validators ([`LLM_PRINCIPAL`]) only open a non-gating PendingReview,
//!   and a batch from that principal that carries a `RecordValidation` is
//!   refused before it is submitted (T50).
use crate::actor::{ActorHandle, Message, Snapshot};
use crate::jobs::{Job, JobState, JobTable};
use contextdb_core::BranchView;
use contextdb_model::*;
use contextdb_proto::docs::{Label, ReviewDoc, Suggestion, ValidationMeta, ValidatorKind};
use contextdb_store::{Durability, Payload, StoreError, TxnStatus};
use contextdb_validate::config::{CandidateLimits, Config, Filter, Limits};
use contextdb_validate::{Item, Outcome, PremiseChange, Task, Validator};
use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[path = "judge.rs"]
mod judge;
pub use judge::pair_txn;

/// The worker's identity for trusted (Deterministic) validators ("VA").
pub const TRUSTED_PRINCIPAL: PrincipalId = PrincipalId(0x5641_0001);
/// The worker's identity for LLM validators: PendingReview batches only.
pub const LLM_PRINCIPAL: PrincipalId = PrincipalId(0x5641_0002);
/// How often discovery looks for a new epoch.
const DISCOVERY_POLL: Duration = Duration::from_millis(50);
/// How long an idle worker sleeps before looking again.
const IDLE: Duration = Duration::from_millis(500);

/// One configured validator with its job filter.
#[derive(Clone)]
pub struct Configured {
    pub filter: Filter,
    pub validator: Arc<dyn Validator>,
    /// How it runs, for `status` and `check` (isolated or user config,
    /// trusted or not).
    pub note: String,
    /// Also judges contradiction candidates (`pairs = true`).
    pub pairs: bool,
}
/// The loaded configuration (replaced whole on reload).
#[derive(Clone, Default)]
pub struct Loaded {
    pub limits: Limits,
    pub candidates: CandidateLimits,
    pub validators: Vec<Configured>,
    /// The last load or reload error; the previous configuration stays.
    pub error: Option<String>,
}
impl Loaded {
    /// Builds the adapters of a parsed `validators.toml`.
    pub fn from_config(config: &Config) -> Self {
        let validators = (config.validators.iter())
            .map(|e| {
                use contextdb_validate::config::Adapter;
                let (program, note) = match &e.adapter {
                    Adapter::Claude {
                        program,
                        isolated_login,
                        token_file,
                        ..
                    } => (
                        program,
                        format!(
                            "{}; login: {}",
                            match isolated_login {
                                Some(_) => "isolated config",
                                None =>
                                    "user config (CLAUDE.md, memory and managed settings may load)",
                            },
                            if token_file.is_some() {
                                "token file"
                            } else {
                                "claude's own"
                            }
                        ),
                    ),
                    Adapter::Command {
                        program, trusted, ..
                    } => (
                        program,
                        if *trusted {
                            "trusted command"
                        } else {
                            "untrusted command (counted as LLM)"
                        }
                        .to_owned(),
                    ),
                };
                Configured {
                    filter: e.filter.clone(),
                    validator: e.build(&contextdb_validate::program_version(program)),
                    note: if e.pairs {
                        format!("{note}; judges candidate pairs")
                    } else {
                        note
                    },
                    pairs: e.pairs,
                }
            })
            .collect();
        Self {
            limits: config.limits.clone(),
            candidates: config.candidates,
            validators,
            error: None,
        }
    }
}
/// Counters for `ctx validators status` and the G6 report.
#[derive(Debug, Default)]
pub struct Stats {
    pub calls: AtomicU64,
    pub llm_calls: AtomicU64,
    pub records: AtomicU64,
    pub reviews: AtomicU64,
    pub superseded: AtomicU64,
    pub failures: AtomicU64,
    pub tokens_in: AtomicU64,
    pub tokens_out: AtomicU64,
    pub discovery_scans: AtomicU64,
    pub discovery_rows: AtomicU64,
    /// Candidate pairs judged (calls), and how many gave a review or `none`.
    pub pair_calls: AtomicU64,
    pub pair_reviews: AtomicU64,
    pub pair_none: AtomicU64,
}
pub struct Scheduler<D: Durability> {
    actor: ActorHandle<D>,
    pub jobs: JobTable,
    /// The data directory (`validators.toml`); `None` in tests.
    dir: Option<PathBuf>,
    loaded: Mutex<Arc<Loaded>>,
    stop: AtomicBool,
    /// Bumped by discovery and reload; idle workers wait on it.
    wake: (Mutex<u64>, Condvar),
    /// Start times of LLM calls in the last hour (the call cap).
    llm_window: Mutex<VecDeque<Instant>>,
    /// The first epoch discovery has not yet looked at.
    discovered: Mutex<Option<Epoch>>,
    /// Base retry delay after a failed attempt (doubles per attempt).
    pub retry_base: Duration,
    pub stats: Stats,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Decoded validator review keys by payload hash (payloads are
    /// immutable): the open-review check reads no blob twice.
    review_keys: Mutex<std::collections::HashMap<ContentHash, Option<ReviewKey>>>,
    /// The candidate table, which takes `[candidates]` on load and reload.
    candidates: std::sync::OnceLock<Arc<crate::candidates::Candidates>>,
}
type ReviewKey = (BranchId, Option<RuleId>, HeadVector);
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}
fn unix_secs() -> u64 {
    (now_ms() / 1000) as u64
}
/// What a worker did with one claimed job (tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ran {
    /// A `RecordValidation` committed (trusted).
    Recorded(Verdict),
    /// A PendingReview opened (LLM).
    Reviewed(Suggestion),
    /// A trusted outcome that writes no record (BoundaryChange, NewRule).
    Kept(Label),
    /// The generation's commit already happened (`TxnStatus`, `TxnIdReuse`).
    AlreadyCommitted,
    /// The key was validated meanwhile (`ValidationExists`, or no longer
    /// Pending at the same vector).
    AlreadyValidated,
    /// An open validator review for the same `(branch, rule, curvec)`
    /// already exists (§4.2 dedup; a manual retry skips this check).
    AlreadyReviewed,
    /// The key is no longer Pending at its vector (`CurvecMoved`).
    Superseded,
    /// Over the LLM call cap: given back.
    Deferred,
    /// The attempt failed (requeued or `Failed`).
    Failed(String),
    /// The token no longer held the row.
    Lost,
}
impl<D: Durability + Clone + 'static> Scheduler<D> {
    pub fn new(
        actor: ActorHandle<D>,
        jobs: JobTable,
        dir: Option<PathBuf>,
        loaded: Loaded,
    ) -> Arc<Self> {
        Arc::new(Self {
            actor,
            jobs,
            dir,
            loaded: Mutex::new(Arc::new(loaded)),
            stop: AtomicBool::new(false),
            wake: (Mutex::new(0), Condvar::new()),
            llm_window: Mutex::new(VecDeque::new()),
            discovered: Mutex::new(None),
            retry_base: Duration::from_secs(10),
            stats: Stats::default(),
            threads: Mutex::new(Vec::new()),
            review_keys: Mutex::new(std::collections::HashMap::new()),
            candidates: std::sync::OnceLock::new(),
        })
    }
    /// Opens `<dir>/jobs.sqlite`, recovers it and loads `validators.toml`
    /// (a bad file is reported by `status` and no validator runs).
    pub fn open(actor: ActorHandle<D>, dir: &std::path::Path) -> Result<Arc<Self>, String> {
        let jobs = JobTable::open(&dir.join(crate::jobs::FILE))?;
        jobs.recover()?;
        let loaded = match Config::load(dir) {
            Ok(c) => Loaded::from_config(&c),
            Err(e) => Loaded {
                error: Some(e),
                ..Loaded::default()
            },
        };
        Ok(Self::new(actor, jobs, Some(dir.to_owned()), loaded))
    }
    pub fn loaded(&self) -> Arc<Loaded> {
        self.loaded.lock().expect("loaded").clone()
    }
    /// Replaces the validators (tests) and wakes the workers.
    pub fn set_loaded(&self, loaded: Loaded) {
        if let Some(c) = self.candidates.get() {
            c.set_limits(loaded.candidates);
        }
        *self.loaded.lock().expect("loaded") = Arc::new(loaded);
        self.notify();
    }
    /// Hands `[candidates]` to the candidate table, now and on each reload.
    pub fn link_candidates(&self, c: Arc<crate::candidates::Candidates>) {
        c.set_limits(self.loaded().candidates);
        let _ = self.candidates.set(c);
    }
    /// `ctx validators reload`: re-reads `validators.toml`. On an error the
    /// previous configuration stays and the error is reported.
    pub fn reload(&self) -> Result<(), String> {
        let Some(dir) = &self.dir else {
            return Err("no data directory".into());
        };
        match Config::load(dir) {
            Ok(c) => {
                self.set_loaded(Loaded::from_config(&c));
                Ok(())
            }
            Err(e) => {
                let mut l = (*self.loaded()).clone();
                l.error = Some(e.clone());
                self.set_loaded(l);
                Err(e)
            }
        }
    }
    fn notify(&self) {
        let (m, c) = &self.wake;
        *m.lock().expect("wake") += 1;
        c.notify_all();
    }
    fn wait(&self, seen: u64, timeout: Duration) {
        let (m, c) = &self.wake;
        let g = m.lock().expect("wake");
        if *g == seen && !self.stop.load(Ordering::Acquire) {
            let _ = c.wait_timeout(g, timeout);
        }
    }
    fn wake_count(&self) -> u64 {
        *self.wake.0.lock().expect("wake")
    }
    /// Starts the discovery thread and `limits.workers` workers.
    pub fn start(self: &Arc<Self>) -> std::io::Result<()> {
        let mut threads = self.threads.lock().expect("threads");
        let me = self.clone();
        threads.push(
            std::thread::Builder::new()
                .name("ctxd-discovery".into())
                .spawn(move || {
                    while !me.stop.load(Ordering::Acquire) {
                        if let Err(e) = me.discover_once() {
                            eprintln!("ctxd: job discovery: {e}");
                        }
                        let seen = me.wake_count();
                        me.wait(seen, DISCOVERY_POLL);
                    }
                })?,
        );
        for n in 0..self.loaded().limits.workers {
            let me = self.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("ctxd-validator-{n}"))
                    .spawn(move || {
                        while !me.stop.load(Ordering::Acquire) {
                            let seen = me.wake_count();
                            // Rule jobs first; candidate pairs when none waits.
                            match me.work_once().and_then(|r| match r {
                                Some(r) => Ok(Some(r)),
                                None => me.judge_once(),
                            }) {
                                Ok(Some(_)) => {}
                                Ok(None) => me.wait(seen, IDLE),
                                Err(e) => {
                                    eprintln!("ctxd: validator worker: {e}");
                                    me.wait(seen, IDLE);
                                }
                            }
                        }
                    })?,
            );
        }
        Ok(())
    }
    /// Stops the threads. Discovery is joined; a worker inside a validator
    /// call is not waited for (its row stays `Running` and is recovered at
    /// the next start, §4.4), and it commits nothing once the actor stopped.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.notify();
        let mut threads = std::mem::take(&mut *self.threads.lock().expect("threads"));
        if !threads.is_empty() {
            let _ = threads.remove(0).join();
        }
    }
    /// `ctx validators status | reload | retry | check`.
    pub fn report(
        &self,
        action: contextdb_proto::proto::ValidatorsAction,
    ) -> Result<contextdb_proto::proto::ValidatorsReport, String> {
        use contextdb_proto::proto::*;
        let mut report = ValidatorsReport::default();
        match action {
            ValidatorsAction::Status => {}
            ValidatorsAction::Reload => {
                // An error is reported in `error`; the old configuration stays.
                let _ = self.reload();
            }
            ValidatorsAction::Retry { key: Some(k), .. } => {
                if !self.jobs.retry(&k)? {
                    return Err("no such job, or it is running".into());
                }
                report.retried = 1;
            }
            ValidatorsAction::Retry { key: None, failed } => {
                if !failed {
                    return Err("retry needs a job key or --failed".into());
                }
                report.retried = self.jobs.retry_failed()? as u64;
            }
            ValidatorsAction::Check => report.check = self.check(),
        }
        if report.retried > 0 {
            self.notify();
        }
        let loaded = self.loaded();
        report.validators = (loaded.validators.iter())
            .map(|c| ValidatorInfo {
                name: c.validator.name().into(),
                kind: c.validator.kind(),
                version: format!("{} ({})", c.validator.version(), c.note),
            })
            .collect();
        let counter = |name: &str, value: u64| Counter {
            name: name.into(),
            value,
        };
        let l = &loaded.limits;
        report.limits = vec![
            counter("workers", l.workers as u64),
            counter("lease_secs", l.lease_secs),
            counter("attempts", l.attempts as u64),
            counter("llm_calls_per_hour", l.llm_calls_per_hour as u64),
            counter("candidate_per_head", loaded.candidates.per_head as u64),
            counter(
                "candidate_min_score_milli",
                (loaded.candidates.min_score * 1000.0) as u64,
            ),
        ];
        let counts = self.jobs.counts()?;
        report.jobs = (JobState::ALL.iter())
            .map(|s| counter(s.name(), counts[*s as usize]))
            .collect();
        report.failed = (self.jobs.list(JobState::Failed, 50)?.into_iter())
            .map(|j| JobInfo {
                key: j.key,
                branch: j.branch,
                rule: j.rule,
                curvec: j.curvec,
                generation: j.generation,
                attempts: j.attempts,
                last_error: j.last_error,
            })
            .collect();
        let st = &self.stats;
        let v = |a: &AtomicU64| a.load(Ordering::Relaxed);
        report.stats = vec![
            counter("calls", v(&st.calls)),
            counter("llm_calls", v(&st.llm_calls)),
            counter("records", v(&st.records)),
            counter("reviews", v(&st.reviews)),
            counter("superseded", v(&st.superseded)),
            counter("failures", v(&st.failures)),
            counter("tokens_in", v(&st.tokens_in)),
            counter("tokens_out", v(&st.tokens_out)),
            counter("discovery_scans", v(&st.discovery_scans)),
            counter("discovery_rows", v(&st.discovery_rows)),
            counter("pair_calls", v(&st.pair_calls)),
            counter("pair_reviews", v(&st.pair_reviews)),
            counter("pair_none", v(&st.pair_none)),
        ];
        if let Some(c) = self.candidates.get() {
            let cs = &c.stats;
            report.stats.extend([
                counter("candidate_heads", v(&cs.heads)),
                counter("candidate_pairs", v(&cs.pairs)),
                counter("candidate_gone", v(&cs.gone)),
                counter("candidate_us_total", v(&cs.micros)),
                counter("candidate_us_max", v(&cs.micros_max)),
            ]);
            if let Ok(n) = c.judge_counts() {
                for (name, i) in [
                    ("pairs_pending", 0),
                    ("pairs_running", 1),
                    ("pairs_judged", 2),
                    ("pairs_failed", 3),
                    ("pairs_skipped", 4),
                ] {
                    report.stats.push(counter(name, n[i]));
                }
            }
        }
        report.error = loaded.error.clone();
        Ok(report)
    }
    /// Runs every configured validator once on [`probe_task`] (an LLM call
    /// for LLM validators, within the hourly cap).
    pub fn check(&self) -> Vec<String> {
        let loaded = self.loaded();
        if loaded.validators.is_empty() {
            return vec!["no validator configured (validators.toml)".into()];
        }
        let task = probe_task();
        (loaded.validators.iter())
            .map(|c| {
                let v = c.validator.as_ref();
                let head = format!("{} [{:?}, {}; {}]", v.name(), v.kind(), v.version(), c.note);
                if v.kind() == ValidatorKind::Llm
                    && !self.llm_permit(loaded.limits.llm_calls_per_hour)
                {
                    return format!("{head}: skipped, over the hourly LLM call cap");
                }
                let lease = Duration::from_secs(loaded.limits.lease_secs);
                let probe = |task: &Task| {
                    let start = Instant::now();
                    match contextdb_validate::run(v, task, start + lease) {
                        Ok(o) => format!(
                            "ok, {} in {:.1}s (probe expects conflict)",
                            o.label.name(),
                            start.elapsed().as_secs_f64()
                        ),
                        Err(e) => format!("FAILED: {e}"),
                    }
                };
                let mut line = format!("{head}: {}", probe(&task));
                if c.pairs {
                    if v.kind() == ValidatorKind::Llm
                        && !self.llm_permit(loaded.limits.llm_calls_per_hour)
                    {
                        line.push_str("; pair probe skipped, over the hourly LLM call cap");
                    } else {
                        line.push_str(&format!("; pair probe: {}", probe(&probe_pair_task())));
                    }
                }
                line
            })
            .collect()
    }

    // Discovery.

    /// Looks at every epoch published since the last call (all branches on
    /// the first call). Returns how many keys were queued or requeued.
    pub fn discover_once(&self) -> Result<usize, String> {
        let snap = self.actor.snapshot();
        let mut from = self.discovered.lock().expect("discovered");
        let mut queued = 0;
        match *from {
            None => queued += self.scan_all(&snap)?,
            Some(e) if e > snap.epoch => {}
            Some(e) => {
                let mut rescan = false;
                let mut touched: Vec<(BranchId, BTreeSet<RuleId>)> = Vec::new();
                for n in e.0..=snap.epoch.0 {
                    if n == 0 {
                        continue;
                    }
                    self.stats.discovery_rows.fetch_add(1, Ordering::Relaxed);
                    match candidates(&snap, Epoch(n)) {
                        Some(Candidates::Rescan) | None => rescan = true,
                        Some(Candidates::Rules(b, rules)) => {
                            match touched.iter_mut().find(|(x, _)| *x == b) {
                                Some((_, s)) => s.extend(rules),
                                None => touched.push((b, rules)),
                            }
                        }
                    }
                }
                if rescan {
                    queued += self.scan_all(&snap)?;
                } else {
                    for (b, rules) in touched {
                        let Some(view) = snap.kernel.branch(b) else {
                            continue;
                        };
                        if view.info().archived {
                            continue;
                        }
                        for j in rules {
                            if let Some(curvec) = pending_at(&view, j) {
                                queued += self.queue(b, j, &curvec)?;
                            }
                        }
                    }
                }
            }
        }
        *from = Some(Epoch(snap.epoch.0 + 1));
        drop(from);
        if queued > 0 {
            self.notify();
        }
        Ok(queued)
    }
    fn scan_all(&self, snap: &Snapshot<D>) -> Result<usize, String> {
        self.stats.discovery_scans.fetch_add(1, Ordering::Relaxed);
        let mut queued = 0;
        for info in snap.kernel.branches() {
            if info.archived {
                continue;
            }
            let Some(view) = snap.kernel.branch(info.id) else {
                continue;
            };
            for (j, curvec) in view.pending_rules() {
                queued += self.queue(info.id, j, &curvec)?;
            }
        }
        Ok(queued)
    }
    fn queue(&self, b: BranchId, j: RuleId, curvec: &HeadVector) -> Result<usize, String> {
        use crate::jobs::Discovered;
        Ok(match self.jobs.discover(b, j, curvec)? {
            Discovered::New | Discovered::Requeued => 1,
            Discovered::Exists => 0,
        })
    }

    // Workers.

    /// Expires lapsed leases, then claims and runs one job. `None` when no
    /// job was claimable (or no validator is configured).
    pub fn work_once(&self) -> Result<Option<Ran>, String> {
        let loaded = self.loaded();
        self.jobs.expire(now_ms(), loaded.limits.attempts)?;
        if loaded.validators.is_empty() {
            return Ok(None);
        }
        let snap = self.actor.snapshot();
        let mut chosen = None;
        let lease = Duration::from_secs(loaded.limits.lease_secs);
        let job = self.jobs.claim(now_ms(), lease.as_millis() as i64, |job| {
            match choose(&loaded, &snap, job) {
                Choice::Run(n) => {
                    chosen = Some(n);
                    true
                }
                Choice::Stale => {
                    chosen = None;
                    true
                }
                Choice::Skip => false,
            }
        })?;
        let Some(job) = job else {
            return Ok(None);
        };
        let ran = match chosen {
            None => self.end(&job, JobState::Superseded, None, None, Ran::Superseded)?,
            Some(n) => {
                let v = loaded.validators[n].validator.clone();
                self.run_job(&loaded, &job, v.as_ref(), lease)?
            }
        };
        Ok(Some(ran))
    }
    fn end(
        &self,
        job: &Job,
        state: JobState,
        error: Option<&str>,
        meta: Option<&[u8]>,
        ran: Ran,
    ) -> Result<Ran, String> {
        if state == JobState::Superseded {
            self.stats.superseded.fetch_add(1, Ordering::Relaxed);
        }
        Ok(
            if self.jobs.finish(&job.key, job.token, state, error, meta)? {
                ran
            } else {
                Ran::Lost
            },
        )
    }
    fn fail(&self, loaded: &Loaded, job: &Job, error: String) -> Result<Ran, String> {
        self.stats.failures.fetch_add(1, Ordering::Relaxed);
        let delay = self.retry_base.as_millis() as i64 * (1i64 << job.attempts.min(10));
        let r = self.jobs.fail(
            &job.key,
            job.token,
            &error,
            loaded.limits.attempts,
            now_ms() + delay,
        )?;
        Ok(match r {
            Some(_) => Ran::Failed(error),
            None => Ran::Lost,
        })
    }
    /// Whether an open PendingReview by the LLM worker names this job's
    /// `(branch, rule, curvec)` (T84): a scan of the branch's open reviews
    /// with each payload decoded once (ADR 0015 §4.2; this cache stands in
    /// for the `validator_reviews` projection).
    fn open_review(&self, snap: &Snapshot<D>, view: &BranchView<'_>, job: &Job) -> bool {
        use contextdb_model::Wire;
        let mut keys = self.review_keys.lock().expect("review keys");
        view.artifacts().any(|(_, a)| {
            if !(a.open && a.ty == ArtifactType::PendingReview && a.author == LLM_PRINCIPAL) {
                return false;
            }
            let key = keys.entry(a.payload_hash).or_insert_with(|| {
                match snap.log.read_blob(&a.payload_hash) {
                    Payload::Available(b) => ReviewDoc::from_bytes(&b)
                        .ok()
                        .map(|d| (d.branch, d.rule, d.curvec)),
                    _ => None,
                }
            });
            key.as_ref().is_some_and(|(b, r, v)| {
                *b == job.branch && *r == Some(job.rule) && *v == job.curvec
            })
        })
    }
    /// Whether an LLM call may start now; counts it if so.
    fn llm_permit(&self, cap: u32) -> bool {
        let mut w = self.llm_window.lock().expect("llm window");
        let hour = Duration::from_secs(3600);
        while w.front().is_some_and(|t| t.elapsed() >= hour) {
            w.pop_front();
        }
        if w.len() >= cap as usize {
            return false;
        }
        w.push_back(Instant::now());
        true
    }
    fn run_job(
        &self,
        loaded: &Loaded,
        job: &Job,
        v: &dyn Validator,
        lease: Duration,
    ) -> Result<Ran, String> {
        let txn = job.client_txn_id();
        // Recovery: this generation may already have committed (§4.4 M6).
        match self
            .actor
            .request(|reply| Message::TxnStatus { id: txn, reply })
        {
            Ok(Ok(TxnStatus::Committed { .. })) => {
                return self.end(job, JobState::Done, None, None, Ran::AlreadyCommitted);
            }
            Ok(Ok(TxnStatus::NotCommitted)) => {}
            Ok(Err(e)) => return self.fail(loaded, job, format!("txn status: {e:?}")),
            Err(e) => return self.fail(loaded, job, format!("txn status: {e:?}")),
        }
        let snap = self.actor.snapshot();
        let Some(view) = snap.kernel.branch(job.branch) else {
            return self.end(job, JobState::Superseded, None, None, Ran::Superseded);
        };
        let Some(rule) = view.rule_semantic(job.rule) else {
            return self.end(job, JobState::Superseded, None, None, Ran::Superseded);
        };
        if !rule.enabled || rule.current != job.curvec {
            return self.end(job, JobState::Superseded, None, None, Ran::Superseded);
        }
        if rule.sem != SemState::Pending {
            return self.end(job, JobState::Done, None, None, Ran::AlreadyValidated);
        }
        let kind = v.kind();
        if kind == ValidatorKind::Llm && job.generation == 0 && self.open_review(&snap, &view, job)
        {
            return self.end(job, JobState::Done, None, None, Ran::AlreadyReviewed);
        }
        let task = match build_task(&snap, &view, job, &rule) {
            Ok(t) => t,
            Err(e) => return self.fail(loaded, job, e),
        };
        drop(snap);
        if kind == ValidatorKind::Llm {
            if !self.llm_permit(loaded.limits.llm_calls_per_hour) {
                let until = now_ms() + 60_000;
                return Ok(if self.jobs.release(&job.key, job.token, until)? {
                    Ran::Deferred
                } else {
                    Ran::Lost
                });
            }
            self.stats.llm_calls.fetch_add(1, Ordering::Relaxed);
        }
        self.stats.calls.fetch_add(1, Ordering::Relaxed);
        let outcome = match contextdb_validate::run(v, &task, Instant::now() + lease) {
            Ok(o) => o,
            Err(e) => return self.fail(loaded, job, e.to_string()),
        };
        if let Some((i, o)) = outcome.tokens {
            self.stats.tokens_in.fetch_add(i as u64, Ordering::Relaxed);
            self.stats.tokens_out.fetch_add(o as u64, Ordering::Relaxed);
        }
        let meta = ValidationMeta {
            v: 1,
            kind,
            validator: v.name().into(),
            version: v.version().into(),
            model: outcome.model.clone(),
            prompt_sha256: outcome.prompt_sha256,
            label: outcome.label,
            rationale: outcome.rationale.clone(),
            evidence: outcome.evidence.clone(),
            tokens_in: outcome.tokens.map(|t| t.0),
            tokens_out: outcome.tokens.map(|t| t.1),
            job: Some(txn),
            based_on: None,
            at: unix_secs(),
        };
        let (meta_hash, meta_bytes) = contextdb_proto::docs::blob(&meta)?;
        let plan = plan(kind, &outcome, job, rule.conclusion, meta_hash, v.name())?;
        let (principal, mut blobs, ops, ran) = match plan {
            Plan::Keep(label) => {
                return self.end(
                    job,
                    JobState::Done,
                    None,
                    Some(&meta_bytes),
                    Ran::Kept(label),
                );
            }
            Plan::Commit {
                principal,
                blobs,
                ops,
                ran,
            } => (principal, blobs, ops, ran),
        };
        blobs.insert(0, meta_bytes);
        let mut batch = Batch::new(ops);
        batch.branch = job.branch;
        batch.channel = Channel::Admin;
        batch.principal = principal;
        batch.client_txn_id = Some(txn);
        if let Some(e) = refuse_llm_validation(&batch) {
            return self.fail(loaded, job, e);
        }
        match self.actor.write(blobs, batch) {
            Ok(Ok(_)) => {
                match ran {
                    Ran::Recorded(_) => self.stats.records.fetch_add(1, Ordering::Relaxed),
                    _ => self.stats.reviews.fetch_add(1, Ordering::Relaxed),
                };
                self.end(job, JobState::Done, None, None, ran)
            }
            Ok(Err(StoreError::Tx(TxError::CurvecMoved { .. }))) => {
                self.end(job, JobState::Superseded, None, None, Ran::Superseded)
            }
            Ok(Err(StoreError::Tx(TxError::ValidationExists { .. }))) => {
                self.end(job, JobState::Done, None, None, Ran::AlreadyValidated)
            }
            Ok(Err(StoreError::Tx(TxError::TxnIdReuse { .. }))) => {
                self.end(job, JobState::Done, None, None, Ran::AlreadyCommitted)
            }
            Ok(Err(e)) => self.fail(loaded, job, format!("commit: {e:?}")),
            Err(e) => self.fail(loaded, job, format!("commit: {e:?}")),
        }
    }
}
/// The fixed `check` task: a database decision whose premise changed from
/// PostgreSQL to MongoDB, which a working validator labels `conflict`.
pub fn probe_task() -> Task {
    let item = |handle: &str, key: &str, kind, body: &str| Item {
        handle: handle.into(),
        key: key.into(),
        kind,
        body: body.into(),
    };
    Task {
        job: [0; 16],
        branch: MAIN,
        rule: RuleId(0),
        curvec: vec![(LogicalId(0), RevisionId(2))],
        conclusion: item(
            "probe/orders-schema@1",
            "probe/orders-schema",
            RecordKind::Decision,
            "Store orders in normalized PostgreSQL tables with foreign keys to customers.",
        ),
        premises: vec![PremiseChange {
            key: "probe/database".into(),
            kind: RecordKind::Fact,
            asserted: item(
                "probe/database@0",
                "probe/database",
                RecordKind::Fact,
                "The orders service uses PostgreSQL 16.",
            ),
            current: item(
                "probe/database@2",
                "probe/database",
                RecordKind::Fact,
                "The orders service moved to MongoDB; PostgreSQL was decommissioned.",
            ),
        }],
        pair: None,
    }
}
/// The fixed pair probe: two records that cannot both hold, which a
/// working validator labels `conflict`.
pub fn probe_pair_task() -> Task {
    let item = |handle: &str, key: &str, body: &str| Item {
        handle: handle.into(),
        key: key.into(),
        kind: RecordKind::Decision,
        body: body.into(),
    };
    Task::pair(
        [0; 16],
        MAIN,
        item(
            "probe/session-store@3",
            "probe/session-store",
            "User sessions are kept only in process memory; no external store is used.",
        ),
        item(
            "probe/session-redis@1",
            "probe/session-redis",
            "All user sessions are persisted in the shared Redis cluster.",
        ),
    )
}
/// T50's guard: a batch from the LLM principal never carries a
/// `RecordValidation`; it is refused before it is submitted.
pub fn refuse_llm_validation(batch: &Batch) -> Option<String> {
    (batch.principal == LLM_PRINCIPAL
        && (batch.ops.iter()).any(|op| matches!(op, Op::RecordValidation { .. })))
    .then(|| "an LLM validator never writes a ValidationRecord".to_owned())
}
/// The batch an outcome maps to (§4.2).
pub enum Plan {
    /// A trusted outcome that writes no record; kept in the job row.
    Keep(Label),
    Commit {
        principal: PrincipalId,
        /// Blobs besides the `ValidationMeta` one.
        blobs: Vec<Vec<u8>>,
        ops: Vec<Op>,
        ran: Ran,
    },
}
/// Maps an outcome to its batch (§4.2): trusted validators record a
/// verdict (`if_absent`), LLM validators open a PendingReview on the
/// rule's conclusion revision.
pub fn plan(
    kind: ValidatorKind,
    outcome: &Outcome,
    job: &Job,
    conclusion: RevisionId,
    meta: ContentHash,
    validator: &str,
) -> Result<Plan, String> {
    if kind != ValidatorKind::Llm {
        let verdict = match outcome.label {
            Label::Reinforcement | Label::None => Verdict::Verified,
            Label::Conflict => Verdict::Rejected,
            other => return Ok(Plan::Keep(other)),
        };
        return Ok(Plan::Commit {
            principal: TRUSTED_PRINCIPAL,
            blobs: Vec::new(),
            ops: vec![Op::RecordValidation {
                rule: job.rule.into(),
                curvec_seen: job.curvec.clone(),
                verdict,
                validator: validator.into(),
                meta: Some(meta),
                if_absent: true,
            }],
            ran: Ran::Recorded(verdict),
        });
    }
    let suggestion = match outcome.label {
        Label::Reinforcement | Label::None => Suggestion::Verify,
        Label::Conflict => Suggestion::Reject,
        Label::BoundaryChange | Label::NewRule => Suggestion::Investigate,
    };
    let review = ReviewDoc {
        v: 1,
        suggestion,
        branch: job.branch,
        rule: Some(job.rule),
        curvec: job.curvec.clone(),
        sides: Vec::new(),
        meta,
    };
    let (hash, bytes) = contextdb_proto::docs::blob(&review)?;
    Ok(Plan::Commit {
        principal: LLM_PRINCIPAL,
        blobs: vec![bytes],
        ops: vec![Op::OpenArtifact {
            ty: ArtifactType::PendingReview,
            target: conclusion.into(),
            payload_hash: hash,
        }],
        ran: Ran::Reviewed(suggestion),
    })
}
enum Choice {
    Run(usize),
    /// The key is gone (branch, rule, or no longer Pending at the vector):
    /// claimed only to be marked Superseded.
    Stale,
    /// No validator takes it.
    Skip,
}
fn choose<D: Durability>(loaded: &Loaded, snap: &Snapshot<D>, job: &Job) -> Choice {
    let Some(view) = snap.kernel.branch(job.branch) else {
        return Choice::Stale;
    };
    if view.info().archived {
        return Choice::Stale;
    }
    let Some(rule) = view.rule_semantic(job.rule) else {
        return Choice::Stale;
    };
    if !rule.enabled || rule.current != job.curvec {
        return Choice::Stale;
    }
    let Some((_, kind, scope)) =
        (view.revision_logical(rule.conclusion)).and_then(|l| view.logical_info(l))
    else {
        return Choice::Stale;
    };
    let name = &view.info().name;
    match (loaded.validators.iter()).position(|c| c.filter.matches(name, kind, &scope)) {
        Some(n) => Choice::Run(n),
        None => Choice::Skip,
    }
}
/// The current vector of rule `j` when it is enabled and Pending.
fn pending_at(view: &BranchView<'_>, j: RuleId) -> Option<HeadVector> {
    let r = view.rule_semantic(j)?;
    (r.enabled && r.sem == SemState::Pending).then_some(r.current)
}
enum Candidates {
    Rescan,
    Rules(BranchId, BTreeSet<RuleId>),
}
/// The rules op_log row `e` can have made Pending, on its branch.
fn candidates<D: Durability>(snap: &Snapshot<D>, e: Epoch) -> Option<Candidates> {
    let row = snap.log.read_row(e).ok()?;
    let batch = row.batch().ok()?;
    if (batch.ops.iter()).any(|op| {
        matches!(
            op,
            Op::CreateBranch { .. } | Op::Merge { .. } | Op::ArchiveBranch { .. }
        )
    }) {
        return Some(Candidates::Rescan);
    }
    let result = row.result().ok()?;
    let view = snap.kernel.branch(batch.branch)?;
    let mut rules = BTreeSet::new();
    for (l, _, _) in &result.net_delta.head_moves {
        rules.extend(view.sem_rules_of(*l));
    }
    rules.extend(result.net_delta.new_rules.iter().copied());
    rules.extend(result.net_delta.enable_flips.iter().copied());
    for op in &batch.ops {
        if let Op::RecordValidation { rule, .. } = op {
            match rule {
                RuleRef::Existing(j) => {
                    rules.insert(*j);
                }
                RuleRef::New(n) => {
                    rules.extend(result.allocations.new_rules.get(*n as usize).copied());
                }
            }
        }
    }
    Some(Candidates::Rules(batch.branch, rules))
}
fn body<D: Durability>(
    snap: &Snapshot<D>,
    view: &BranchView<'_>,
    r: RevisionId,
) -> Result<String, String> {
    let hash = view
        .revision_content(r)
        .map_err(|e| format!("revision {}: {e:?}", r.0))?;
    match snap.log.read_blob(&hash) {
        Payload::Available(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        _ => Err(format!("body of revision {} unavailable", r.0)),
    }
}
fn item<D: Durability>(
    snap: &Snapshot<D>,
    view: &BranchView<'_>,
    r: RevisionId,
) -> Result<Item, String> {
    let l = view
        .revision_logical(r)
        .ok_or_else(|| format!("revision {} not visible", r.0))?;
    let (key, kind, _) = view
        .logical_info(l)
        .ok_or_else(|| format!("logical {} not visible", l.0))?;
    Ok(Item {
        handle: format!("{key}@{}", r.0),
        key,
        kind,
        body: body(snap, view, r)?,
    })
}
/// The task for a job: the conclusion and each changed Sem premise with its
/// asserted and current bodies (never truncated, §4.1).
pub fn build_task<D: Durability>(
    snap: &Snapshot<D>,
    view: &BranchView<'_>,
    job: &Job,
    rule: &contextdb_core::RuleSemantic,
) -> Result<Task, String> {
    let conclusion = item(snap, view, rule.conclusion)?;
    let mut premises = Vec::new();
    for ((l, asserted), (_, current)) in rule.asserted.iter().zip(&rule.current) {
        if asserted == current {
            continue;
        }
        let asserted = item(snap, view, *asserted)?;
        let current = item(snap, view, *current)?;
        let (key, kind, _) = view
            .logical_info(*l)
            .ok_or_else(|| format!("logical {} not visible", l.0))?;
        premises.push(PremiseChange {
            key,
            kind,
            asserted,
            current,
        });
    }
    Ok(Task {
        job: job.client_txn_id(),
        branch: job.branch,
        rule: job.rule,
        curvec: job.curvec.clone(),
        conclusion,
        premises,
        pair: None,
    })
}
