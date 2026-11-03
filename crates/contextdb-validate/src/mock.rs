//! Scripted outcomes for tests (ADR 0015 §4.1): a default reply, per-job
//! replies, delays, failures and hangs.
use crate::*;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What the mock does for one call.
#[derive(Clone, Debug)]
pub enum Script {
    Reply(Outcome),
    Fail(ValidatorError),
    /// Sleeps this long, then replies with the default.
    Delay(Duration),
    /// Never replies before the deadline.
    Hang,
}
pub struct Mock {
    name: String,
    kind: ValidatorKind,
    default: Mutex<Script>,
    per_job: Mutex<HashMap<[u8; 16], Vec<Script>>>,
    calls: AtomicUsize,
}
impl Mock {
    pub fn new(name: &str, kind: ValidatorKind) -> Self {
        Self {
            name: name.into(),
            kind,
            default: Mutex::new(Script::Reply(Self::outcome(Label::Reinforcement))),
            per_job: Mutex::new(HashMap::new()),
            calls: AtomicUsize::new(0),
        }
    }
    pub fn outcome(label: Label) -> Outcome {
        Outcome {
            label,
            rationale: format!("mock: {}", label.name()),
            evidence: Vec::new(),
            model: None,
            prompt_sha256: None,
            tokens: None,
        }
    }
    pub fn set_default(&self, s: Script) {
        *self.default.lock().expect("mock") = s;
    }
    /// Queues replies for one job, used in order, then the default.
    pub fn script(&self, job: [u8; 16], s: Vec<Script>) {
        self.per_job.lock().expect("mock").insert(job, s);
    }
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
impl Validator for Mock {
    fn kind(&self) -> ValidatorKind {
        self.kind
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "mock"
    }
    fn validate(&self, task: &Task, deadline: Instant) -> Result<Outcome, ValidatorError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = {
            let mut jobs = self.per_job.lock().expect("mock");
            match jobs.get_mut(&task.job) {
                Some(q) if !q.is_empty() => q.remove(0),
                _ => self.default.lock().expect("mock").clone(),
            }
        };
        match next {
            Script::Reply(o) => Ok(o),
            Script::Fail(e) => Err(e),
            Script::Delay(d) => {
                let wake = Instant::now() + d;
                if wake > deadline {
                    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                    return Err(ValidatorError::Timeout);
                }
                std::thread::sleep(d);
                match self.default.lock().expect("mock").clone() {
                    Script::Reply(o) => Ok(o),
                    _ => Err(ValidatorError::Failed("mock default is not a reply".into())),
                }
            }
            Script::Hang => {
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                Err(ValidatorError::Timeout)
            }
        }
    }
}
