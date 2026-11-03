//! P7 validator adapters (ADR 0015 §4.1), outside the kernel and the writer.
//!
//! A [`Validator`] judges one [`Task`]: the change in a Sem rule's premises
//! since the rule was asserted, or (a pair task, ADR 0015 §5) whether two
//! records found by the lexical candidates contradict each other. Its [`Outcome`] is a label with a rationale
//! and evidence handles; mapping labels to verdicts or reviews is the
//! worker's job (§4.2), never the adapter's. Adapters:
//!
//! - [`mock::Mock`]: scripted outcomes for tests;
//! - [`command::CommandValidator`]: an external program, JSON in and out;
//! - [`claude::ClaudeCli`]: `claude -p`, sandboxed (no tools, MCP, settings
//!   or hooks), always an LLM.
use contextdb_model::*;
pub use contextdb_proto::docs::{Label, ValidatorKind};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub mod claude;
pub mod command;
pub mod config;
pub mod mock;

/// Largest total body size of a task; larger tasks fail as `TooLarge`
/// rather than being validated on partial content (§4.1, review m5).
pub const MAX_TASK_BODY_BYTES: usize = 256 * 1024;
/// Largest rationale an outcome may carry.
pub const MAX_RATIONALE_BYTES: usize = 2 * 1024;
/// Largest stdout an external validator may produce.
const MAX_STDOUT_BYTES: usize = 1 << 20;

/// One record as the validator sees it. `handle` is `key@rev`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub handle: String,
    pub key: String,
    pub kind: RecordKind,
    pub body: String,
}
/// A Sem premise: the revision the rule asserted and the current head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PremiseChange {
    pub key: String,
    pub kind: RecordKind,
    pub asserted: Item,
    pub current: Item,
}
/// A rule task (`pair` is `None`): the rule's conclusion and its changed
/// Sem premises. A pair task (`pair` is `Some`): `conclusion` is the new
/// head and `pair` the other side; `rule`, `curvec` and `premises` are
/// unused (`RuleId(0)`, empty).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub job: [u8; 16],
    pub branch: BranchId,
    pub rule: RuleId,
    pub curvec: HeadVector,
    pub conclusion: Item,
    pub premises: Vec<PremiseChange>,
    pub pair: Option<Item>,
}
impl Task {
    /// A pair task: do `a` (the new head) and `b` contradict each other?
    pub fn pair(job: [u8; 16], branch: BranchId, a: Item, b: Item) -> Self {
        Self {
            job,
            branch,
            rule: RuleId(0),
            curvec: Vec::new(),
            conclusion: a,
            premises: Vec::new(),
            pair: Some(b),
        }
    }
    pub fn is_pair(&self) -> bool {
        self.pair.is_some()
    }
    /// Every handle the task shows: the only ones evidence may cite.
    pub fn handles(&self) -> Vec<&str> {
        let mut out = vec![self.conclusion.handle.as_str()];
        out.extend(self.pair.as_ref().map(|b| b.handle.as_str()));
        for p in &self.premises {
            out.push(&p.asserted.handle);
            out.push(&p.current.handle);
        }
        out
    }
    pub fn body_bytes(&self) -> usize {
        self.conclusion.body.len()
            + self.pair.as_ref().map_or(0, |b| b.body.len())
            + self
                .premises
                .iter()
                .map(|p| p.asserted.body.len() + p.current.body.len())
                .sum::<usize>()
    }
    /// The task as JSON (the `Command` protocol and the prompt's data).
    pub fn to_json(&self) -> serde_json::Value {
        let item = |i: &Item| {
            serde_json::json!({
                "handle": i.handle,
                "key": i.key,
                "kind": format!("{:?}", i.kind),
                "body": i.body,
            })
        };
        if let Some(b) = &self.pair {
            return serde_json::json!({
                "job": hex(&self.job),
                "branch": self.branch.0,
                "task": "pair",
                "a": item(&self.conclusion),
                "b": item(b),
            });
        }
        serde_json::json!({
            "job": hex(&self.job),
            "branch": self.branch.0,
            "rule": self.rule.0,
            "conclusion": item(&self.conclusion),
            "premises": self.premises.iter().map(|p| serde_json::json!({
                "key": p.key,
                "kind": format!("{:?}", p.kind),
                "asserted": item(&p.asserted),
                "current": item(&p.current),
            })).collect::<Vec<_>>(),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub label: Label,
    pub rationale: String,
    pub evidence: Vec<String>,
    pub model: Option<String>,
    pub prompt_sha256: Option<ContentHash>,
    pub tokens: Option<(u32, u32)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidatorError {
    /// The deadline passed; the process group was killed.
    Timeout,
    /// The task's bodies exceed [`MAX_TASK_BODY_BYTES`].
    TooLarge,
    /// The reply was not one valid outcome object.
    Malformed(String),
    /// Evidence cited a handle the task does not show.
    Disallowed(String),
    /// The validator failed (spawn error, nonzero exit, harness error).
    Failed(String),
}
impl std::fmt::Display for ValidatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "timeout"),
            Self::TooLarge => write!(f, "task too large"),
            Self::Malformed(m) => write!(f, "malformed reply: {m}"),
            Self::Disallowed(h) => write!(f, "evidence cites a handle outside the task: {h}"),
            Self::Failed(m) => write!(f, "validator failed: {m}"),
        }
    }
}
pub trait Validator: Send + Sync {
    fn kind(&self) -> ValidatorKind;
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    /// Judges one task. Implementations must not exceed `deadline`.
    fn validate(&self, task: &Task, deadline: Instant) -> Result<Outcome, ValidatorError>;
}
/// Runs a validator with the checks every outcome must pass, whatever the
/// adapter: the size cap before, the handle allowlist and rationale cap
/// after (§4.1).
pub fn run(v: &dyn Validator, task: &Task, deadline: Instant) -> Result<Outcome, ValidatorError> {
    if task.body_bytes() > MAX_TASK_BODY_BYTES {
        return Err(ValidatorError::TooLarge);
    }
    let outcome = v.validate(task, deadline)?;
    check(task, &outcome)?;
    Ok(outcome)
}
/// The handle allowlist, the rationale cap, and for a pair task the two
/// labels it allows (`conflict`, `none`).
pub fn check(task: &Task, o: &Outcome) -> Result<(), ValidatorError> {
    if task.is_pair() && !matches!(o.label, Label::Conflict | Label::None) {
        return Err(ValidatorError::Malformed(format!(
            "a pair task takes conflict or none, not {}",
            o.label.name()
        )));
    }
    let allowed = task.handles();
    if let Some(h) = o.evidence.iter().find(|h| !allowed.contains(&h.as_str())) {
        return Err(ValidatorError::Disallowed(h.clone()));
    }
    if o.rationale.len() > MAX_RATIONALE_BYTES {
        return Err(ValidatorError::Malformed("rationale over 2 KiB".into()));
    }
    Ok(())
}
/// Parses `{label, rationale, evidence}` strictly: exactly those keys, the
/// right types, a known label.
pub fn parse_outcome_object(v: &serde_json::Value) -> Result<Outcome, ValidatorError> {
    let bad = |m: &str| ValidatorError::Malformed(m.into());
    let obj = v.as_object().ok_or_else(|| bad("not an object"))?;
    if let Some(k) = obj
        .keys()
        .find(|k| !matches!(k.as_str(), "label" | "rationale" | "evidence"))
    {
        return Err(ValidatorError::Malformed(format!("unexpected key {k}")));
    }
    let label = obj
        .get("label")
        .and_then(|l| l.as_str())
        .and_then(Label::parse)
        .ok_or_else(|| bad("label"))?;
    let rationale = obj
        .get("rationale")
        .and_then(|r| r.as_str())
        .ok_or_else(|| bad("rationale"))?
        .to_owned();
    let evidence = obj
        .get("evidence")
        .and_then(|e| e.as_array())
        .ok_or_else(|| bad("evidence"))?
        .iter()
        .map(|h| {
            h.as_str()
                .map(str::to_owned)
                .ok_or_else(|| bad("evidence item"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Outcome {
        label,
        rationale,
        evidence,
        model: None,
        prompt_sha256: None,
        tokens: None,
    })
}
pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
/// Runs `cmd` in its own process group with `stdin`, returning stdout on a
/// zero exit. At the deadline the whole group is killed (§4.1).
pub(crate) fn run_process(
    cmd: &mut Command,
    stdin: &[u8],
    deadline: Instant,
) -> Result<Vec<u8>, ValidatorError> {
    let (ok, stdout, stderr) = run_process_raw(cmd, stdin, deadline)?;
    if !ok {
        return Err(ValidatorError::Failed(failure(&stdout, &stderr)));
    }
    Ok(stdout)
}
/// The tails of a failed process's output, stderr first.
pub(crate) fn failure(stdout: &[u8], stderr: &[u8]) -> String {
    let tail = |b: &[u8]| {
        let t = String::from_utf8_lossy(b);
        let t: Vec<char> = t.trim().chars().collect();
        t[t.len().saturating_sub(400)..].iter().collect::<String>()
    };
    format!(
        "exit nonzero; stderr: {}; stdout: {}",
        tail(stderr),
        tail(stdout)
    )
}
/// Like [`run_process`], but returns (zero exit, stdout, stderr) so an
/// adapter can read a structured error from stdout.
pub(crate) fn run_process_raw(
    cmd: &mut Command,
    stdin: &[u8],
    deadline: Instant,
) -> Result<(bool, Vec<u8>, Vec<u8>), ValidatorError> {
    use std::os::unix::process::CommandExt;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| ValidatorError::Failed(format!("spawn: {e}")))?;
    let pgid = nix::unistd::Pid::from_raw(child.id() as i32);
    let mut input = child.stdin.take().expect("piped");
    let data = stdin.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let mut out = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut out)
            .take(MAX_STDOUT_BYTES as u64 + 1)
            .read_to_end(&mut buf);
        buf
    });
    let mut err = child.stderr.take().expect("piped");
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut err).take(64 * 1024).read_to_end(&mut buf);
        buf
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(ValidatorError::Failed(format!("wait: {e}"))),
        }
    };
    // The group goes either way: a validator must leave nothing behind.
    let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
    let Some(status) = status else {
        let _ = child.wait();
        return Err(ValidatorError::Timeout);
    };
    let _ = writer.join();
    let stdout = reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if stdout.len() > MAX_STDOUT_BYTES {
        return Err(ValidatorError::Malformed("stdout over 1 MiB".into()));
    }
    Ok((status.success(), stdout, stderr))
}

/// `program --version`'s first line within 10 s, for the metadata's
/// `version` (`unknown` when it cannot be read).
pub fn program_version(program: &std::path::Path) -> String {
    let mut cmd = Command::new(program);
    cmd.arg("--version");
    match run_process(&mut cmd, &[], Instant::now() + Duration::from_secs(10)) {
        Ok(out) => String::from_utf8_lossy(&out)
            .lines()
            .next()
            .map(|l| l.trim().chars().take(80).collect())
            .filter(|l: &String| !l.is_empty())
            .unwrap_or_else(|| "unknown".into()),
        Err(_) => "unknown".into(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn task() -> Task {
        let item = |h: &str, body: &str| Item {
            handle: h.into(),
            key: h.split('@').next().unwrap().into(),
            kind: RecordKind::Decision,
            body: body.into(),
        };
        Task {
            job: [1; 16],
            branch: MAIN,
            rule: RuleId(0),
            curvec: vec![(LogicalId(0), RevisionId(2))],
            conclusion: item("plan@1", "use postgres driver"),
            premises: vec![PremiseChange {
                key: "db".into(),
                kind: RecordKind::Decision,
                asserted: item("db@0", "postgres"),
                current: item("db@2", "postgres 16"),
            }],
            pair: None,
        }
    }
    #[test]
    fn pair_tasks_show_both_sides_and_take_two_labels() {
        let item = |h: &str, body: &str| Item {
            handle: h.into(),
            key: h.split('@').next().unwrap().into(),
            kind: RecordKind::Fact,
            body: body.into(),
        };
        let t = Task::pair([2; 16], MAIN, item("a@3", "x"), item("b@1", "yy"));
        assert_eq!(t.handles(), vec!["a@3", "b@1"]);
        assert_eq!(t.body_bytes(), 3);
        let j = t.to_json();
        assert_eq!(j["task"], "pair");
        assert_eq!(j["b"]["handle"], "b@1");
        assert!(j.get("premises").is_none());
        let mut o = mock::Mock::outcome(Label::Conflict);
        o.evidence = vec!["b@1".into()];
        assert_eq!(check(&t, &o), Ok(()));
        assert_eq!(check(&t, &mock::Mock::outcome(Label::None)), Ok(()));
        for l in [Label::Reinforcement, Label::BoundaryChange, Label::NewRule] {
            assert!(matches!(
                check(&t, &mock::Mock::outcome(l)),
                Err(ValidatorError::Malformed(_))
            ));
        }
        // A rule task does not get the pair's handles.
        assert!(!task().handles().contains(&"b@1"));
    }
    #[test]
    fn outcome_parsing_is_strict_and_evidence_is_allowlisted() {
        let ok = serde_json::json!({"label": "reinforcement", "rationale": "same db", "evidence": ["db@2"]});
        let o = parse_outcome_object(&ok).unwrap();
        assert_eq!(o.label, Label::Reinforcement);
        assert_eq!(check(&task(), &o), Ok(()));
        for bad in [
            serde_json::json!({"label": "approve", "rationale": "", "evidence": []}),
            serde_json::json!({"label": "none", "rationale": "", "evidence": [], "verdict": "Verified"}),
            serde_json::json!({"label": "none", "rationale": 3, "evidence": []}),
            serde_json::json!(["none"]),
        ] {
            assert!(
                matches!(
                    parse_outcome_object(&bad),
                    Err(ValidatorError::Malformed(_))
                ),
                "{bad}"
            );
        }
        let outside = Outcome {
            evidence: vec!["secrets@9".into()],
            ..o.clone()
        };
        assert_eq!(
            check(&task(), &outside),
            Err(ValidatorError::Disallowed("secrets@9".into()))
        );
        let long = Outcome {
            rationale: "x".repeat(MAX_RATIONALE_BYTES + 1),
            ..o
        };
        assert!(matches!(
            check(&task(), &long),
            Err(ValidatorError::Malformed(_))
        ));
    }
    #[test]
    fn oversize_tasks_fail_before_the_validator_runs() {
        let mut t = task();
        t.conclusion.body = "x".repeat(MAX_TASK_BODY_BYTES + 1);
        let m = mock::Mock::new("m", ValidatorKind::Deterministic);
        assert_eq!(
            run(&m, &t, Instant::now() + Duration::from_secs(1)),
            Err(ValidatorError::TooLarge)
        );
        assert_eq!(m.calls(), 0);
    }
}
