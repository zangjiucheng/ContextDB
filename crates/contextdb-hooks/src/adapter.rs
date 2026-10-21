//! Harness adapters (ADR 0010 §3): hook stdin of Claude Code 2.1.306 and
//! Codex CLI 0.163.0 → one normalised event. Parsing is lenient (unknown
//! fields are ignored, only what an event needs is required); a missing
//! required field is `Unparsed`, which the binary treats like an unavailable
//! daemon and always reports (§3.1).
use crate::patch;
use contextdb_model::{Action, ToolClass};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
}
impl Harness {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}
/// The hook events ContextDB installs (ADR 0010 §9), by CLI name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventName {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    SubagentStart,
    SubagentStop,
    Stop,
    PreCompact,
    SessionEnd,
}
impl EventName {
    pub const ALL: [Self; 9] = [
        Self::SessionStart,
        Self::UserPromptSubmit,
        Self::PreToolUse,
        Self::PostToolUse,
        Self::SubagentStart,
        Self::SubagentStop,
        Self::Stop,
        Self::PreCompact,
        Self::SessionEnd,
    ];
    /// The CLI argument (`ctx-hook <harness> <event>`).
    pub fn arg(self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::UserPromptSubmit => "user-prompt-submit",
            Self::PreToolUse => "pre-tool-use",
            Self::PostToolUse => "post-tool-use",
            Self::SubagentStart => "subagent-start",
            Self::SubagentStop => "subagent-stop",
            Self::Stop => "stop",
            Self::PreCompact => "pre-compact",
            Self::SessionEnd => "session-end",
        }
    }
    /// The harness's `hook_event_name` (same in both harnesses).
    pub fn wire(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::Stop => "Stop",
            Self::PreCompact => "PreCompact",
            Self::SessionEnd => "SessionEnd",
        }
    }
    pub fn from_arg(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.arg() == s)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    SessionStart {
        source: String,
    },
    UserPrompt,
    /// An edit, write, patch or shell action (gated, recorded).
    PreTool {
        action: Action,
    },
    PostTool {
        action: Action,
    },
    /// A call to an MCP tool (claimed when it is ContextDB's, §2.3).
    McpCall {
        tool: String,
    },
    /// Any other tool: neither gated nor recorded.
    OtherTool {
        tool: String,
    },
    SubagentStart,
    SubagentStop,
    Stop,
    PreCompact,
    SessionEnd {
        reason: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookEvent {
    pub harness: Harness,
    pub kind: Kind,
    pub session: String,
    pub agent_id: Option<String>,
    pub turn_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub cwd: String,
    pub transcript: Option<String>,
}
/// Why stdin was not usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unparsed(pub String);

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(Into::into)
}
fn need(v: &Value, k: &str) -> Result<String, Unparsed> {
    str_field(v, k).ok_or_else(|| Unparsed(format!("missing {k}")))
}
/// Classifies a tool call. Paths are as the harness gave them; the binary
/// normalises them against the repository root.
pub fn classify(harness: Harness, tool: &str, input: &Value) -> Kind {
    let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_owned);
    let action = |tool: ToolClass, paths: Vec<String>, command: Option<String>| Action {
        tool,
        paths,
        command,
    };
    if tool.starts_with("mcp__") {
        return Kind::McpCall { tool: tool.into() };
    }
    let a = match (harness, tool) {
        (Harness::Claude, "Edit" | "MultiEdit") => {
            action(ToolClass::Edit, s("file_path").into_iter().collect(), None)
        }
        (Harness::Claude, "Write") => {
            action(ToolClass::Write, s("file_path").into_iter().collect(), None)
        }
        (Harness::Claude, "NotebookEdit") => action(
            ToolClass::Notebook,
            s("notebook_path").into_iter().collect(),
            None,
        ),
        (Harness::Codex, "apply_patch") => {
            let text = s("command").unwrap_or_default();
            action(ToolClass::Patch, patch::paths(&text), None)
        }
        (Harness::Claude, "Bash" | "PowerShell" | "Monitor") | (Harness::Codex, "Bash") => {
            let command = s("command");
            // Codex intercepts `apply_patch` heredocs run through the shell.
            match command.as_deref() {
                Some(c) if harness == Harness::Codex && patch::is_patch_command(c) => {
                    action(ToolClass::Patch, patch::paths(c), command)
                }
                _ => action(ToolClass::Shell, Vec::new(), command),
            }
        }
        _ => return Kind::OtherTool { tool: tool.into() },
    };
    Kind::PreTool { action: a }
}
/// Parses one hook stdin object for `event`.
pub fn parse(harness: Harness, event: EventName, stdin: &str) -> Result<HookEvent, Unparsed> {
    let v: Value = serde_json::from_str(stdin).map_err(|e| Unparsed(format!("json: {e}")))?;
    if !v.is_object() {
        return Err(Unparsed("stdin is not a JSON object".into()));
    }
    if let Some(name) = v.get("hook_event_name").and_then(Value::as_str)
        && name != event.wire()
    {
        return Err(Unparsed(format!(
            "hook_event_name {name} for {}",
            event.wire()
        )));
    }
    let tool = || need(&v, "tool_name");
    let input = v.get("tool_input").cloned().unwrap_or(Value::Null);
    let kind = match event {
        EventName::SessionStart => Kind::SessionStart {
            source: str_field(&v, "source").unwrap_or_else(|| "startup".into()),
        },
        EventName::UserPromptSubmit => Kind::UserPrompt,
        EventName::PreToolUse => classify(harness, &tool()?, &input),
        EventName::PostToolUse => match classify(harness, &tool()?, &input) {
            Kind::PreTool { action } => Kind::PostTool { action },
            other => other,
        },
        EventName::SubagentStart => {
            need(&v, "agent_id")?;
            Kind::SubagentStart
        }
        EventName::SubagentStop => {
            need(&v, "agent_id")?;
            Kind::SubagentStop
        }
        EventName::Stop => Kind::Stop,
        EventName::PreCompact => Kind::PreCompact,
        EventName::SessionEnd => Kind::SessionEnd {
            reason: str_field(&v, "reason").unwrap_or_else(|| "other".into()),
        },
    };
    Ok(HookEvent {
        harness,
        kind,
        session: need(&v, "session_id")?,
        agent_id: str_field(&v, "agent_id"),
        turn_id: str_field(&v, "turn_id"),
        tool_use_id: str_field(&v, "tool_use_id"),
        cwd: need(&v, "cwd")?,
        transcript: str_field(&v, "transcript_path"),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn pre(h: Harness, tool: &str, input: Value) -> Kind {
        let stdin = json!({ "session_id": "s", "cwd": "/r", "hook_event_name": "PreToolUse",
            "tool_name": tool, "tool_input": input, "tool_use_id": "t1", "extra": [1, 2] });
        parse(h, EventName::PreToolUse, &stdin.to_string())
            .unwrap()
            .kind
    }
    #[test]
    fn tools_of_both_harnesses() {
        let edit = pre(
            Harness::Claude,
            "Edit",
            json!({ "file_path": "/r/a.rs", "old_string": "x" }),
        );
        assert_eq!(
            edit,
            Kind::PreTool {
                action: Action {
                    tool: ToolClass::Edit,
                    paths: vec!["/r/a.rs".into()],
                    command: None
                }
            }
        );
        assert!(matches!(
            pre(Harness::Claude, "NotebookEdit", json!({ "notebook_path": "/r/n.ipynb" })),
            Kind::PreTool { action } if action.tool == ToolClass::Notebook && action.paths == ["/r/n.ipynb"]
        ));
        assert!(matches!(
            pre(Harness::Claude, "Bash", json!({ "command": "ls" })),
            Kind::PreTool { action } if action.tool == ToolClass::Shell && action.command.as_deref() == Some("ls")
        ));
        let patch = "*** Begin Patch\n*** Update File: src/x.rs\n*** End Patch\n";
        assert!(matches!(
            pre(Harness::Codex, "apply_patch", json!({ "command": patch })),
            Kind::PreTool { action } if action.tool == ToolClass::Patch && action.paths == ["src/x.rs"]
        ));
        let heredoc = format!("apply_patch <<'EOF'\n{patch}EOF");
        assert!(matches!(
            pre(Harness::Codex, "Bash", json!({ "command": heredoc })),
            Kind::PreTool { action } if action.tool == ToolClass::Patch
        ));
        // A Claude tool name is not a Codex one, and vice versa.
        assert!(matches!(
            pre(Harness::Codex, "Edit", json!({})),
            Kind::OtherTool { .. }
        ));
        assert!(matches!(
            pre(Harness::Claude, "apply_patch", json!({})),
            Kind::OtherTool { .. }
        ));
        assert_eq!(
            pre(
                Harness::Claude,
                "mcp__contextdb__ctx_read",
                json!({ "target": "db" })
            ),
            Kind::McpCall {
                tool: "mcp__contextdb__ctx_read".into()
            }
        );
        assert!(matches!(
            pre(Harness::Claude, "Read", json!({})),
            Kind::OtherTool { .. }
        ));
    }
    #[test]
    fn unparsed_inputs() {
        let bad = |s: &str, e| parse(Harness::Claude, e, s).unwrap_err();
        bad("not json", EventName::Stop);
        bad("[1]", EventName::Stop);
        bad(r#"{"cwd":"/r"}"#, EventName::Stop);
        bad(r#"{"session_id":"s","cwd":"/r"}"#, EventName::PreToolUse);
        bad(r#"{"session_id":"s","cwd":"/r"}"#, EventName::SubagentStart);
        bad(
            r#"{"session_id":"s","cwd":"/r","hook_event_name":"Stop"}"#,
            EventName::SessionEnd,
        );
        let ok = parse(
            Harness::Codex,
            EventName::SubagentStop,
            r#"{"session_id":"s","cwd":"/r","agent_id":"t2","turn_id":"7","transcript_path":null}"#,
        )
        .unwrap();
        assert_eq!(ok.agent_id.as_deref(), Some("t2"));
        assert_eq!(ok.turn_id.as_deref(), Some("7"));
        assert_eq!(ok.transcript, None);
        for e in EventName::ALL {
            assert_eq!(EventName::from_arg(e.arg()), Some(e));
        }
    }
}
