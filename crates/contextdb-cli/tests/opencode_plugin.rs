//! The OpenCode 2.0 plugin `ctx hooks install --harness opencode` writes
//! (ADR 0014): installed with the `ctx-hook` path, refusing a foreign file,
//! uninstalled cleanly, and its hook mapping run under node against a fake
//! OpenCode context and a fake `ctx-hook` that answers like the real one.
use std::{path::Path, process::Command};

fn ctx(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(args)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}
fn temp(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("ctx-opencode-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}
/// A `ctx-hook` stand-in: logs each call and answers as ctx-hook would.
fn fake_hook(dir: &Path) -> std::path::PathBuf {
    let hook = dir.join("fake-ctx-hook");
    let log = dir.join("calls.log");
    std::fs::write(
        &hook,
        format!(
            r#"#!/bin/sh
input=$(cat)
printf '%s %s\n' "$2" "$input" >> '{log}'
case "$2:$input" in
  session-start:*) echo '{{"hookSpecificOutput":{{"hookEventName":"SessionStart","additionalContext":"BRIEFING"}}}}' ;;
  user-prompt-submit:*) echo '{{"hookSpecificOutput":{{"hookEventName":"UserPromptSubmit","additionalContext":"UPDATE"}}}}' ;;
  pre-tool-use:*deny.rs*) echo '{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"ContextDB Invalid: db"}}}}' ;;
  pre-tool-use:*warn.rs*) echo '{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","additionalContext":"ContextDB Stale: db"}}}}' ;;
  stop:*) echo '{{"systemMessage":"ContextDB drift: x.rs"}}' ;;
esac
"#,
            log = log.display()
        ),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&hook).status().unwrap();
    hook
}

#[test]
fn install_uninstall_and_foreign_files() {
    let p = temp("install");
    let hook = fake_hook(&p);
    let project = p.join("repo");
    std::fs::create_dir_all(&project).unwrap();
    let args = |sub| {
        vec![
            "hooks".to_owned(),
            sub,
            "--harness".into(),
            "opencode".into(),
            "--project".into(),
            project.display().to_string(),
            "--ctx-hook".into(),
            hook.display().to_string(),
        ]
    };
    let run = |a: Vec<String>| ctx(&a.iter().map(String::as_str).collect::<Vec<_>>());
    let (ok, out) = run(args("install".into()));
    assert!(ok, "{out}");
    let file = project.join(".opencode/plugins/contextdb/index.js");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.starts_with("// contextdb-hook-plugin:"));
    assert!(text.contains(&format!(
        "const CTX_HOOK = {:?};",
        hook.display().to_string()
    )));
    assert!(!text.contains("__CTX_HOOK__"));
    // Reinstalling overwrites its own file.
    assert!(run(args("install".into())).0);
    let (ok, out) = run(args("uninstall".into()));
    assert!(ok, "{out}");
    assert!(!project.join(".opencode/plugins/contextdb").exists());
    // Someone else's plugin of that name is never overwritten or removed.
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "export default {}\n").unwrap();
    let (ok, out) = run(args("install".into()));
    assert!(!ok && out.contains("not ContextDB's plugin"), "{out}");
    assert!(run(args("uninstall".into())).0);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "export default {}\n"
    );
    let _ = std::fs::remove_dir_all(&p);
}

/// The plugin under node: a deny throws with the reason, a warning is
/// appended to that tool's result, the briefing and the turn's update reach
/// every model request's system prompt, a drift report at the end of a turn
/// reaches the next one, and session deletion ends the session.
#[test]
fn plugin_maps_hooks_both_ways() {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node not found: skipped");
        return;
    }
    let p = temp("node");
    let hook = fake_hook(&p);
    let project = p.join("repo");
    std::fs::create_dir_all(&project).unwrap();
    let (ok, out) = ctx(&[
        "hooks",
        "install",
        "--harness",
        "opencode",
        "--project",
        project.to_str().unwrap(),
        "--ctx-hook",
        hook.to_str().unwrap(),
    ]);
    assert!(ok, "{out}");
    // node treats .js as CommonJS outside a module package.
    let plugin = p.join("plugin.mjs");
    std::fs::copy(
        project.join(".opencode/plugins/contextdb/index.js"),
        &plugin,
    )
    .unwrap();
    let driver = p.join("driver.mjs");
    std::fs::write(
        &driver,
        format!(
            r#"import assert from "node:assert/strict";
import plugin from {plugin:?};
const hooks = {{}};
const queue = [];
let wake = null;
const push = (ev) => {{ queue.push(ev); wake?.(); }};
const ctx = {{
  location: {{ directory: {project:?} }},
  session: {{ hook: (name, fn) => {{ hooks["session." + name] = fn; }} }},
  tool: {{ hook: (name, fn) => {{ hooks["tool." + name] = fn; }} }},
  event: {{ subscribe: async function* ({{ signal }}) {{
    while (!signal.aborted) {{
      if (queue.length) {{ yield queue.shift(); continue; }}
      await new Promise((r) => {{ wake = r; signal.addEventListener("abort", r); }});
    }}
  }} }},
}};
assert.equal(plugin.id, "contextdb");
const cleanup = await plugin.setup(ctx);
const sid = "ses_1";
await hooks["session.prompt"]({{ sessionID: sid, prompt: {{ text: "go" }} }});
const req = {{ sessionID: sid, system: [] }};
await hooks["session.context"](req);
assert.deepEqual(req.system.map((s) => s.text), ["BRIEFING", "UPDATE"]);
assert.ok(req.system.every((s) => s.type === "text"));
// Deny: the tool call fails with the reason.
await assert.rejects(
  hooks["tool.execute.before"]({{ tool: "edit", sessionID: sid, id: "c1", input: {{ path: "deny.rs" }} }}),
  /ContextDB Invalid: db/,
);
// Warn: allowed, and the warning follows the tool's own output.
await hooks["tool.execute.before"]({{ tool: "edit", sessionID: sid, id: "c2", input: {{ path: "warn.rs" }} }});
const done = {{ tool: "edit", sessionID: sid, id: "c2", input: {{ path: "warn.rs" }}, status: "completed",
  result: {{ content: [{{ type: "text", text: "Edited" }}] }} }};
await hooks["tool.execute.after"](done);
assert.deepEqual(done.result.content.map((c) => c.text), ["Edited", "ContextDB Stale: db"]);
// Allow: nothing appended.
await hooks["tool.execute.before"]({{ tool: "read", sessionID: sid, id: "c3", input: {{}} }});
const read = {{ tool: "read", sessionID: sid, id: "c3", input: {{}}, status: "completed", result: {{ content: [] }} }};
await hooks["tool.execute.after"](read);
assert.deepEqual(read.result.content, []);
// End of turn: the drift report reaches the next turn.
push({{ type: "session.execution.succeeded", data: {{ sessionID: sid }} }});
await new Promise((r) => setTimeout(r, 1500));
await hooks["session.prompt"]({{ sessionID: sid, prompt: {{ text: "next" }} }});
const next = {{ sessionID: sid, system: [] }};
await hooks["session.context"](next);
assert.deepEqual(next.system.map((s) => s.text), ["BRIEFING", "ContextDB drift: x.rs", "UPDATE"]);
push({{ type: "session.deleted", data: {{ sessionID: sid }} }});
await new Promise((r) => setTimeout(r, 1500));
cleanup();
console.log("ok");
"#,
            plugin = plugin.display().to_string(),
            project = project.display().to_string(),
        ),
    )
    .unwrap();
    let out = Command::new("node").arg(&driver).output().unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && text.contains("ok"), "{text}");
    // The fake hook saw Claude-shaped stdin with the plugin's session.
    let calls = std::fs::read_to_string(p.join("calls.log")).unwrap();
    let events: Vec<&str> = calls
        .lines()
        .map(|l| l.split(' ').next().unwrap())
        .collect();
    assert_eq!(
        events.iter().filter(|e| **e == "session-start").count(),
        1,
        "{calls}"
    );
    for e in [
        "user-prompt-submit",
        "pre-tool-use",
        "post-tool-use",
        "stop",
        "session-end",
    ] {
        assert!(events.contains(&e), "{e}: {calls}");
    }
    let pre: serde_json::Value = serde_json::from_str(
        calls
            .lines()
            .find(|l| l.starts_with("pre-tool-use ") && l.contains("warn.rs"))
            .unwrap()
            .split_once(' ')
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(pre["session_id"], "ses_1");
    assert_eq!(pre["hook_event_name"], "PreToolUse");
    assert_eq!(pre["tool_name"], "edit");
    assert_eq!(pre["tool_use_id"], "c2");
    assert_eq!(pre["tool_input"]["path"], "warn.rs");
    assert_eq!(pre["cwd"], project.display().to_string());
    let _ = std::fs::remove_dir_all(&p);
}
