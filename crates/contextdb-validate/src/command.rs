//! An external validator program (ADR 0015 §4.1): the task as one JSON
//! object on stdin, one outcome object `{label, rationale, evidence}` on
//! stdout, within the deadline, in its own process group.
//!
//! It is `Deterministic` only when configured `trusted`. That trust is
//! declared by whoever writes the data directory's `validators.toml`
//! (SPEC §10 threat row); otherwise the program counts as an LLM.
use crate::*;
use std::path::PathBuf;

pub struct CommandValidator {
    pub name: String,
    pub version: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub trusted: bool,
}
impl Validator for CommandValidator {
    fn kind(&self) -> ValidatorKind {
        if self.trusted {
            ValidatorKind::Deterministic
        } else {
            ValidatorKind::Llm
        }
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        &self.version
    }
    fn validate(&self, task: &Task, deadline: Instant) -> Result<Outcome, ValidatorError> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        let input = task.to_json().to_string();
        let out = run_process(&mut cmd, input.as_bytes(), deadline)?;
        let text = std::str::from_utf8(&out)
            .map_err(|_| ValidatorError::Malformed("stdout is not UTF-8".into()))?;
        let v: serde_json::Value = serde_json::from_str(text.trim())
            .map_err(|e| ValidatorError::Malformed(format!("json: {e}")))?;
        parse_outcome_object(&v)
    }
}
