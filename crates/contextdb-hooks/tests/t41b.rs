//! T41b (ADR 0010 §11): hook schema contract tests against the pinned
//! harness versions. Every Codex 0.163.0 input fixture validates against the
//! vendored generated schema and parses; every rendered output validates
//! against the event's output schema (Codex rejects unknown keys); events
//! with no output schema render nothing. Claude Code 2.1.306 fixtures follow
//! the public hooks reference. A harness upgrade means re-vendoring the
//! schemas and updating these fixtures and the pins.
use contextdb_hooks::{
    adapter::{self, EventName, Harness, Kind},
    render::{self, Output, REASON_BYTES},
};
use contextdb_model::{Decision, ToolClass};
use serde_json::{Value, json};
use std::path::PathBuf;

fn schema(name: &str) -> Option<Value> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex-0.163.0/schema")
        .join(name);
    std::fs::read_to_string(p)
        .ok()
        .map(|s| serde_json::from_str(&s).unwrap())
}
/// The draft-07 subset the generated schemas use: boolean schemas, `$ref`
/// to `#/definitions`, `allOf`, `type` (string or list), `const`, `enum`,
/// `properties`, `required` and `additionalProperties: false`.
fn validate(root: &Value, s: &Value, v: &Value, at: &str) -> Result<(), String> {
    match s {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return Err(format!("{at}: nothing allowed")),
        _ => {}
    }
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        let name = r
            .strip_prefix("#/definitions/")
            .ok_or(format!("{at}: ref {r}"))?;
        return validate(root, &root["definitions"][name], v, at);
    }
    for sub in s
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        validate(root, sub, v, at)?;
    }
    if let Some(t) = s.get("type") {
        let types: Vec<&str> = match t {
            Value::String(t) => vec![t],
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        let ok = types.iter().any(|t| match *t {
            "string" => v.is_string(),
            "boolean" => v.is_boolean(),
            "object" => v.is_object(),
            "array" => v.is_array(),
            "null" => v.is_null(),
            "integer" => v.is_i64() || v.is_u64(),
            "number" => v.is_number(),
            _ => false,
        });
        if !ok {
            return Err(format!("{at}: {v} is not {types:?}"));
        }
    }
    if let Some(c) = s.get("const")
        && c != v
    {
        return Err(format!("{at}: {v} != const {c}"));
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array)
        && !e.contains(v)
    {
        return Err(format!("{at}: {v} not in {e:?}"));
    }
    if let Some(obj) = v.as_object() {
        let props = s.get("properties").and_then(Value::as_object);
        for r in s
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let r = r.as_str().unwrap();
            if !obj.contains_key(r) {
                return Err(format!("{at}: missing {r}"));
            }
        }
        for (k, x) in obj {
            match props.and_then(|p| p.get(k)) {
                Some(ps) => validate(root, ps, x, &format!("{at}.{k}"))?,
                None if s.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    return Err(format!("{at}: unknown key {k}"));
                }
                None => {}
            }
        }
    }
    Ok(())
}
fn file(e: EventName) -> &'static str {
    match e {
        EventName::SessionStart => "session-start",
        EventName::UserPromptSubmit => "user-prompt-submit",
        EventName::PreToolUse => "pre-tool-use",
        EventName::PostToolUse => "post-tool-use",
        EventName::SubagentStart => "subagent-start",
        EventName::SubagentStop => "subagent-stop",
        EventName::Stop => "stop",
        EventName::PreCompact => "pre-compact",
        EventName::SessionEnd => "session-end",
    }
}
/// A Codex 0.163.0 stdin object for `e`, exactly the generated schema's keys.
fn codex_input(e: EventName, tool: Option<(&str, Value)>) -> Value {
    let mut v = json!({
        "session_id": "0199c3a1-0000-7000-8000-000000000001",
        "transcript_path": null,
        "cwd": "/repo",
        "hook_event_name": e.wire(),
    });
    let o = v.as_object_mut().unwrap();
    let turn = |o: &mut serde_json::Map<String, Value>| {
        o.insert("turn_id".into(), json!("7"));
    };
    if e != EventName::SessionEnd {
        o.insert("model".into(), json!("gpt-test"));
    }
    if !matches!(e, EventName::SessionEnd | EventName::PreCompact) {
        o.insert("permission_mode".into(), json!("default"));
    }
    match e {
        EventName::SessionStart => {
            o.insert("source".into(), json!("startup"));
        }
        EventName::SessionEnd => {
            o.insert("reason".into(), json!("other"));
        }
        EventName::UserPromptSubmit => {
            turn(o);
            o.insert("prompt".into(), json!("fix the parser"));
        }
        EventName::PreToolUse | EventName::PostToolUse => {
            turn(o);
            let (name, input) = tool.unwrap_or(("Bash", json!({ "command": "ls" })));
            o.insert("tool_name".into(), json!(name));
            o.insert("tool_input".into(), input);
            o.insert("tool_use_id".into(), json!("call_abc"));
            if e == EventName::PostToolUse {
                o.insert("tool_response".into(), json!("Success. Updated files."));
            }
        }
        EventName::PreCompact => {
            turn(o);
            o.insert("trigger".into(), json!("auto"));
        }
        EventName::Stop => {
            turn(o);
            o.insert("stop_hook_active".into(), json!(false));
            o.insert("last_assistant_message".into(), Value::Null);
        }
        EventName::SubagentStart => {
            turn(o);
            o.insert(
                "agent_id".into(),
                json!("0199c3a1-0000-7000-8000-000000000002"),
            );
            o.insert("agent_type".into(), json!("explorer"));
        }
        EventName::SubagentStop => {
            turn(o);
            o.insert(
                "agent_id".into(),
                json!("0199c3a1-0000-7000-8000-000000000002"),
            );
            o.insert("agent_type".into(), json!("explorer"));
            o.insert("agent_transcript_path".into(), Value::Null);
            o.insert("stop_hook_active".into(), json!(false));
            o.insert("last_assistant_message".into(), json!("done"));
        }
    }
    v
}
/// A Claude Code 2.1.306 stdin object for `e` (hooks reference).
fn claude_input(e: EventName, tool: Option<(&str, Value)>) -> Value {
    let mut v = json!({
        "session_id": "abc123",
        "transcript_path": "/home/u/.claude/projects/-repo/abc123.jsonl",
        "cwd": "/repo",
        "permission_mode": "default",
        "hook_event_name": e.wire(),
    });
    let o = v.as_object_mut().unwrap();
    match e {
        EventName::SessionStart => {
            o.insert("source".into(), json!("startup"));
            o.insert("model".into(), json!("claude-opus-5-5"));
        }
        EventName::SessionEnd => {
            o.insert("reason".into(), json!("clear"));
        }
        EventName::UserPromptSubmit => {
            o.insert("prompt".into(), json!("fix the parser"));
        }
        EventName::PreToolUse | EventName::PostToolUse => {
            let (name, input) = tool.unwrap_or((
                "Edit",
                json!({ "file_path": "/repo/src/lib.rs", "old_string": "a", "new_string": "b" }),
            ));
            o.insert("tool_name".into(), json!(name));
            o.insert("tool_input".into(), input);
            o.insert("tool_use_id".into(), json!("toolu_01ABC"));
            if e == EventName::PostToolUse {
                o.insert(
                    "tool_response".into(),
                    json!({ "filePath": "/repo/src/lib.rs" }),
                );
            }
        }
        EventName::PreCompact => {
            o.insert("trigger".into(), json!("manual"));
            o.insert("custom_instructions".into(), Value::Null);
        }
        EventName::Stop => {
            o.insert("stop_hook_active".into(), json!(false));
        }
        EventName::SubagentStart => {
            o.insert("agent_id".into(), json!("agent-1"));
            o.insert("agent_type".into(), json!("Explore"));
        }
        EventName::SubagentStop => {
            o.insert("agent_id".into(), json!("agent-1"));
            o.insert("agent_type".into(), json!("Explore"));
            o.insert("stop_hook_active".into(), json!(false));
            o.insert("agent_transcript_path".into(), json!("/x/agent-1.jsonl"));
        }
    }
    v
}

#[test]
fn codex_inputs_match_the_vendored_schemas_and_parse() {
    let patch = "*** Begin Patch\n*** Update File: src/x.rs\n@@\n-a\n+b\n*** End Patch\n";
    for e in EventName::ALL {
        let tools: Vec<Option<(&str, Value)>> = match e {
            EventName::PreToolUse | EventName::PostToolUse => vec![
                Some(("Bash", json!({ "command": "cargo test" }))),
                Some(("apply_patch", json!({ "command": patch }))),
                Some(("mcp__contextdb__ctx_read", json!({ "target": "db" }))),
            ],
            _ => vec![None],
        };
        for tool in tools {
            let input = codex_input(e, tool);
            let s = schema(&format!("{}.command.input.schema.json", file(e))).unwrap();
            validate(&s, &s, &input, "$").unwrap_or_else(|err| panic!("{e:?}: {err}"));
            let ev = adapter::parse(Harness::Codex, e, &input.to_string())
                .unwrap_or_else(|u| panic!("{e:?}: {u:?}"));
            assert_eq!(ev.session, "0199c3a1-0000-7000-8000-000000000001");
            if let Kind::PreTool { action } | Kind::PostTool { action } = &ev.kind
                && action.tool == ToolClass::Patch
            {
                assert_eq!(action.paths, ["src/x.rs"]);
            }
        }
    }
}

#[test]
fn claude_inputs_parse() {
    for e in EventName::ALL {
        let input = claude_input(e, None);
        let ev = adapter::parse(Harness::Claude, e, &input.to_string())
            .unwrap_or_else(|u| panic!("{e:?}: {u:?}"));
        assert_eq!(ev.session, "abc123");
    }
    let nb = claude_input(
        EventName::PreToolUse,
        Some((
            "NotebookEdit",
            json!({ "notebook_path": "/repo/a.ipynb", "new_source": "x" }),
        )),
    );
    assert!(matches!(
        adapter::parse(Harness::Claude, EventName::PreToolUse, &nb.to_string()).unwrap().kind,
        Kind::PreTool { action } if action.paths == ["/repo/a.ipynb"]
    ));
}

#[test]
fn every_rendered_output_validates_against_the_codex_output_schema() {
    let outputs = [
        Output::default(),
        Output {
            decision: Some(Decision::Allow),
            system: "note".into(),
            ..Output::default()
        },
        Output {
            decision: Some(Decision::Warn),
            reason: "ContextDB Invalid: db changed".into(),
            context: "ContextDB Invalid: db changed".into(),
            system: "ContextDB warning".into(),
        },
        Output {
            decision: Some(Decision::Deny),
            reason: "x".repeat(2_000),
            context: "c".into(),
            system: "ContextDB denied".into(),
        },
        Output {
            context: "<context_update epoch=3>\ndb: read r1, now r2\n</context_update>".into(),
            ..Output::default()
        },
        Output {
            system: "ContextDB drift: changed without a recorded edit: a.rs;".into(),
            context: "never shown on stop".into(),
            ..Output::default()
        },
    ];
    for e in EventName::ALL {
        let s = schema(&format!("{}.command.output.schema.json", file(e)));
        for out in &outputs {
            let Some(text) = render::render(e, out) else {
                continue;
            };
            let Some(s) = &s else {
                panic!("{e:?} has no Codex output schema but rendered {text}");
            };
            let v: Value = serde_json::from_str(&text).unwrap();
            validate(s, s, &v, "$").unwrap_or_else(|err| panic!("{e:?} {text}: {err}"));
            // Never a bare allow (Claude would skip the permission prompt,
            // Codex rejects allow without updatedInput).
            assert_ne!(v["hookSpecificOutput"]["permissionDecision"], "allow");
            if let Some(r) = v["hookSpecificOutput"]["permissionDecisionReason"].as_str() {
                assert!(!r.is_empty() && r.len() <= REASON_BYTES);
            }
            // Stop-like events only ever carry a systemMessage (§3.4).
            if matches!(
                e,
                EventName::Stop | EventName::SubagentStop | EventName::PreCompact
            ) {
                assert!(v.get("hookSpecificOutput").is_none(), "{e:?}");
            }
        }
    }
    assert!(schema("session-end.command.output.schema.json").is_none());
}
/// The validator has teeth: outputs Codex would reject are rejected here.
#[test]
fn the_validator_rejects_what_codex_rejects() {
    let stop = schema("stop.command.output.schema.json").unwrap();
    let pre = schema("pre-tool-use.command.output.schema.json").unwrap();
    let bad = [
        (
            &stop,
            json!({ "hookSpecificOutput": { "hookEventName": "Stop", "additionalContext": "x" } }),
        ),
        (&pre, json!({ "unknown": 1 })),
        (
            &pre,
            json!({ "hookSpecificOutput": { "hookEventName": "Stop" } }),
        ),
        (
            &pre,
            json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "maybe" } }),
        ),
        (
            &pre,
            json!({ "hookSpecificOutput": { "permissionDecision": "deny" } }),
        ),
        (&pre, json!({ "systemMessage": 3 })),
    ];
    for (s, v) in bad {
        assert!(validate(s, s, &v, "$").is_err(), "{v}");
    }
    let input = schema("pre-tool-use.command.input.schema.json").unwrap();
    let mut extra = codex_input(EventName::PreToolUse, None);
    extra["surprise"] = json!(1);
    assert!(validate(&input, &input, &extra, "$").is_err());
}
