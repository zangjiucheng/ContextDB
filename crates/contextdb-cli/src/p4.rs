//! P4 commands (ADR 0010 §6–§10): `ctx check`, `ctx policy`, `ctx gating`,
//! `ctx hooks`, `ctx gate-report P4` and `ctx dogfood review`. TOML is parsed
//! here only; hooks read the compiled `Wire` forms.
use contextdb_model::*;
use contextdb_proto::{
    Client,
    gate::{self, GateConfig, GateRule},
    lease,
    outbox::{Delivered, Outbox, Reply},
    proto::*,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, Write},
    path::{Path, PathBuf},
    time::Duration,
};

/// Exit codes of `ctx check` (ADR 0010 §6.4).
pub const EXIT_DENY: u8 = 1;
pub const EXIT_ERROR: u8 = 2;
pub const EXIT_UNAVAILABLE: u8 = 3;

fn connect(dir: &Path, socket: &str) -> Result<Client, String> {
    let l = lease::read_lease(&dir.join("ctxd.lease"))
        .filter(lease::Lease::is_live)
        .ok_or_else(|| format!("ctxd is not running for {}", dir.display()))?;
    let path = l
        .sockets
        .iter()
        .find(|s| s.ends_with(socket))
        .ok_or_else(|| format!("lease names no {socket}"))?;
    Client::connect_timeout(path, Duration::from_secs(30)).map_err(|e| format!("connect: {e}"))
}
fn call(c: &mut Client, r: Request) -> Result<Envelope, String> {
    let env = c.call(&r).map_err(|e| format!("{e:?}"))?;
    if let Response::Error { code, detail } = &env.response {
        return Err(format!("{code}: {detail}"));
    }
    Ok(env)
}
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}
fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex16(s: &str) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    for (k, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * k..2 * k + 2)?, 16).ok()?;
    }
    (s.len() == 32).then_some(out)
}
fn table(src: &str) -> Result<toml::Table, String> {
    src.parse::<toml::Table>().map_err(|e| format!("toml: {e}"))
}
fn only_keys(t: &toml::Table, allowed: &[&str], what: &str) -> Result<(), String> {
    match t.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!("{what}: unknown key {k}")),
        None => Ok(()),
    }
}
fn kind(name: &str) -> Result<RecordKind, String> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "fact" => RecordKind::Fact,
        "assumption" => RecordKind::Assumption,
        "decision" => RecordKind::Decision,
        "plan" => RecordKind::Plan,
        "convention" => RecordKind::Convention,
        "procedure" => RecordKind::Procedure,
        "artifact" => RecordKind::Artifact,
        "evidence" => RecordKind::Evidence,
        "codeanchor" | "code_anchor" => RecordKind::CodeAnchor,
        "summary" => RecordKind::Summary,
        other => return Err(format!("unknown record kind {other}")),
    })
}

// ---------------------------------------------------------------- policy

/// `.contextdb/policy.toml` → `PolicyDoc` (ADR 0010 §8).
pub fn policy_doc(src: &str) -> Result<PolicyDoc, String> {
    let t = table(src)?;
    only_keys(&t, &["version", "require_deps"], "policy.toml")?;
    let version = t
        .get("version")
        .and_then(toml::Value::as_integer)
        .ok_or("policy.toml: version = 1 is required")?;
    if version != 1 {
        return Err(format!("policy.toml: unsupported version {version}"));
    }
    let mut require_deps = BTreeSet::new();
    if let Some(v) = t.get("require_deps") {
        for k in v.as_array().ok_or("require_deps must be an array")? {
            require_deps.insert(kind(k.as_str().ok_or("require_deps holds kind names")?)?);
        }
    }
    Ok(PolicyDoc {
        version: 1,
        require_deps,
    })
}
pub fn policy(dir: &Path, args: &[String]) -> Result<u8, String> {
    let file = PathBuf::from(flag(args, "--file").unwrap_or(".contextdb/policy.toml"));
    let src = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let doc = policy_doc(&src)?;
    let hash = hex(blake3::hash(&doc.canonical_bytes()).as_bytes());
    match args.first().map(String::as_str) {
        Some("show") => {
            println!(
                "version={} require_deps={:?}",
                doc.version, doc.require_deps
            );
            println!("canonical hash {hash}");
        }
        Some("apply") => {
            // Takes effect only once committed, in the kernel transaction (T41).
            let mut c = connect(dir, "admin.sock")?;
            let env = call(
                &mut c,
                Request::Raw {
                    blobs: Vec::new(),
                    batch: Batch::new(vec![Op::SetPolicy { policy: doc }]),
                },
            )?;
            if let Response::Committed { result } = env.response {
                println!("policy committed at epoch {} (hash {hash})", result.epoch.0);
            }
        }
        _ => return Err("usage: ctx policy show|apply <dir> [--file F]".into()),
    }
    Ok(0)
}

// ---------------------------------------------------------------- gating

fn mode(s: &str) -> Result<GateMode, String> {
    match s {
        "off" => Ok(GateMode::Off),
        "warn" => Ok(GateMode::Warn),
        "enforce" => Ok(GateMode::Enforce),
        other => Err(format!("mode must be off|warn|enforce, not {other}")),
    }
}
fn tool_class(s: &str) -> ToolClass {
    match s {
        "edit" => ToolClass::Edit,
        "write" => ToolClass::Write,
        "notebook" => ToolClass::Notebook,
        "patch" => ToolClass::Patch,
        "shell" => ToolClass::Shell,
        other => ToolClass::Other(other.into()),
    }
}
/// `.contextdb/gating.toml` → `GateConfig` (ADR 0010 §6.2). Unknown keys
/// are errors, so a typo never silently means "warn".
pub fn gate_config(src: &str) -> Result<GateConfig, String> {
    let t = table(src)?;
    only_keys(
        &t,
        &["mode", "shell_mode", "on_unavailable", "timeout_ms", "rule"],
        "gating.toml",
    )?;
    let s = |k: &str| t.get(k).and_then(toml::Value::as_str);
    let mut cfg = GateConfig {
        source_hash: *blake3::hash(src.as_bytes()).as_bytes(),
        ..GateConfig::default()
    };
    if let Some(m) = s("mode") {
        cfg.mode = mode(m)?;
    }
    if let Some(m) = s("shell_mode") {
        cfg.shell_mode = mode(m)?;
    }
    cfg.fail_closed = match s("on_unavailable").unwrap_or("open") {
        "open" => false,
        "closed" => true,
        other => return Err(format!("on_unavailable must be open|closed, not {other}")),
    };
    if let Some(ms) = t.get("timeout_ms") {
        let ms = ms.as_integer().ok_or("timeout_ms must be an integer")?;
        cfg.timeout_ms = u32::try_from(ms)
            .ok()
            .filter(|m| (1..=10_000).contains(m))
            .ok_or("timeout_ms must be 1..=10000")?;
    }
    if let Some(rules) = t.get("rule") {
        for r in rules
            .as_array()
            .ok_or("[[rule]] must be an array of tables")?
        {
            let r = r.as_table().ok_or("[[rule]] must be a table")?;
            only_keys(r, &["tool", "path", "mode"], "[[rule]]")?;
            let tool = match r.get("tool").and_then(toml::Value::as_str) {
                None | Some("*") => None,
                Some(t) => Some(tool_class(t)),
            };
            cfg.rules.push(GateRule {
                tool,
                path: r.get("path").and_then(toml::Value::as_str).map(Into::into),
                mode: mode(
                    r.get("mode")
                        .and_then(toml::Value::as_str)
                        .ok_or("[[rule]] needs a mode")?,
                )?,
            });
        }
    }
    Ok(cfg)
}
pub fn gating(dir: &Path, args: &[String]) -> Result<u8, String> {
    if args.first().map(String::as_str) != Some("apply") {
        return Err("usage: ctx gating apply <dir> [--file F]".into());
    }
    let file = PathBuf::from(flag(args, "--file").unwrap_or(".contextdb/gating.toml"));
    let src = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let cfg = gate_config(&src)?;
    let tmp = dir.join("gating.wire.tmp");
    std::fs::write(&tmp, cfg.canonical_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join("gating.wire")).map_err(|e| e.to_string())?;
    println!(
        "gating applied: mode={:?} shell={:?} on_unavailable={} timeout={}ms rules={}",
        cfg.mode,
        cfg.shell_mode,
        if cfg.fail_closed { "closed" } else { "open" },
        cfg.timeout_ms,
        cfg.rules.len()
    );
    Ok(0)
}

// ---------------------------------------------------------------- check

pub fn check(dir: &Path, args: &[String]) -> Result<u8, String> {
    let cfg = std::fs::read(dir.join("gating.wire"))
        .ok()
        .and_then(|b| GateConfig::from_bytes(&b).ok())
        .unwrap_or_default();
    let mut c = match connect(dir, "admin.sock") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ctx check: {e}");
            return Ok(EXIT_UNAVAILABLE);
        }
    };
    if has(args, "--all") {
        // Opt-in whole-repository mode (ADR 0010 §6.4): any STALE or
        // CONFLICTED head fails.
        let env = call(
            &mut c,
            Request::Briefing {
                binding: None,
                budget_chars: 0,
            },
        )?;
        let Response::Briefing {
            stale,
            pending,
            conflicted,
            ..
        } = env.response
        else {
            return Err("unexpected response".into());
        };
        println!(
            "epoch={} stale={stale} pending={pending} conflicted={conflicted}",
            env.epoch.0
        );
        return Ok(if stale + conflicted > 0 { EXIT_DENY } else { 0 });
    }
    let agent = flag(args, "--agent").map(String::from);
    let binding = match (
        flag(args, "--session"),
        flag(args, "--harness"),
        flag(args, "--key"),
    ) {
        (Some(s), _, _) => Binding::Session {
            session: unhex16(s).ok_or("--session takes 32 hex digits")?,
            agent,
        },
        (None, Some(h), Some(k)) => Binding::Key {
            harness: h.into(),
            harness_session: k.into(),
            agent,
        },
        _ => return Err("ctx check needs --session HEX, --harness H --key K, or --all".into()),
    };
    let paths: Vec<String> = args
        .windows(2)
        .filter(|w| w[0] == "--action-path")
        .map(|w| w[1].clone())
        .collect();
    let action = flag(args, "--action-tool").map(|t| Action {
        tool: tool_class(t),
        paths,
        command: None,
    });
    let dry = has(args, "--dry-run");
    let resolved = action
        .as_ref()
        .map_or(cfg.mode, |a| gate::resolve_mode(&cfg, a));
    let env = call(
        &mut c,
        Request::Check {
            binding,
            cert_id: None,
            live_pos: Vec::new(),
            action,
            // The CLI decides locally with the same predicate and never logs a
            // gated action (review minor 10); `--dry-run` says so explicitly.
            mode: None,
            harness_version: "ctx".into(),
            strict: has(args, "--strict"),
        },
    )?;
    let Response::Check {
        validation,
        superseded,
        named,
        ..
    } = env.response
    else {
        return Err("unexpected response".into());
    };
    let decision = gate::gate(resolved, &validation);
    println!(
        "epoch={} verdict={} mode={resolved:?} decision={decision:?}{}",
        env.epoch.0,
        gate::verdict_code(&validation),
        if dry { " (dry run)" } else { "" }
    );
    if let Validation::Invalid { diffs, .. } = &validation {
        for d in diffs {
            println!(
                "  changed: logical {} read {:?} now {:?} {:?}",
                d.logical.0, d.cert_rev, d.current, d.summary
            );
        }
    }
    for n in &named {
        println!(
            "  changed: {} read r{} now {:?} {:?}{}",
            n.key,
            n.read.0,
            n.current.map(|r| r.0),
            n.summary,
            n.agent
                .as_ref()
                .map(|a| format!(" (read by {a})"))
                .unwrap_or_default()
        );
    }
    for d in &superseded {
        println!(
            "  superseded read: logical {} {:?} → {:?}",
            d.logical.0, d.cert_rev, d.current
        );
    }
    Ok(match decision {
        Decision::Deny => EXIT_DENY,
        Decision::Warn if has(args, "--fail-on-warn") => EXIT_DENY,
        _ => 0,
    })
}

// ---------------------------------------------------------------- hooks

/// The ContextDB MCP tools the Claude claim entry matches (ADR 0010 §2.3).
const CLAIMED: &str = "ctx_recall|ctx_read|ctx_record|ctx_supersede|ctx_check";
/// The harness tool-name form of an MCP server name: characters outside
/// `[A-Za-z0-9_-]` become `_`, then regex-escaped (`-` needs no escape).
pub fn mcp_server_name(name: &str) -> Result<String, String> {
    let n: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if n.trim_matches('_').is_empty() {
        return Err(format!("--mcp-name {name:?} normalises to nothing"));
    }
    Ok(n)
}
/// (event, matcher, timeout s, extra handler fields) per harness (§9).
fn entries(
    harness: &str,
    mcp: &str,
    fail_closed: bool,
) -> Vec<(&'static str, Option<String>, u32, Value)> {
    match harness {
        "claude" => {
            let tools = "Edit|Write|MultiEdit|NotebookEdit|Bash|PowerShell|Monitor".to_string();
            let pre = if fail_closed {
                json!({ "onFailure": "block" })
            } else {
                json!({})
            };
            vec![
                ("SessionStart", None, 5, json!({})),
                ("UserPromptSubmit", None, 2, json!({})),
                ("PreToolUse", Some(tools.clone()), 2, pre),
                (
                    "PreToolUse",
                    Some(format!("^mcp__{mcp}__({CLAIMED})$")),
                    2,
                    json!({}),
                ),
                ("PostToolUse", Some(tools), 5, json!({ "async": true })),
                ("SubagentStart", None, 5, json!({})),
                ("SubagentStop", None, 5, json!({})),
                ("Stop", None, 5, json!({})),
                ("PreCompact", None, 5, json!({})),
                ("SessionEnd", None, 3, json!({})),
            ]
        }
        _ => {
            let tools = "apply_patch|Bash".to_string();
            vec![
                ("SessionStart", None, 5, json!({})),
                ("UserPromptSubmit", None, 2, json!({})),
                ("PreToolUse", Some(tools.clone()), 2, json!({})),
                ("PostToolUse", Some(tools), 5, json!({ "async": true })),
                ("SubagentStart", None, 5, json!({})),
                ("SubagentStop", None, 5, json!({ "async": true })),
                ("Stop", None, 5, json!({ "async": true })),
                ("PreCompact", None, 5, json!({ "async": true })),
                ("SessionEnd", None, 2, json!({})),
            ]
        }
    }
}
fn event_arg(event: &str) -> &'static str {
    match event {
        "SessionStart" => "session-start",
        "UserPromptSubmit" => "user-prompt-submit",
        "PreToolUse" => "pre-tool-use",
        "PostToolUse" => "post-tool-use",
        "SubagentStart" => "subagent-start",
        "SubagentStop" => "subagent-stop",
        "Stop" => "stop",
        "PreCompact" => "pre-compact",
        _ => "session-end",
    }
}
/// A shell word for Codex's session shell, double-quoted.
pub fn shell_quote(path: &str) -> Result<String, String> {
    if path.chars().any(|c| c == '\n' || c == '\0') {
        return Err(format!("cannot quote {path:?} for a shell"));
    }
    let mut q = String::from('"');
    for c in path.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    Ok(q)
}
/// Whether a handler is one of ours (`ctx-hook` at `exe`).
fn ours(handler: &Value, exe: &str) -> bool {
    let cmd = handler.get("command").and_then(Value::as_str).unwrap_or("");
    cmd == exe || cmd.starts_with(&format!("{} ", shell_quote(exe).unwrap_or_default()))
}
/// Removes our handlers (and groups left empty) from a hooks object.
fn strip(hooks: &mut serde_json::Map<String, Value>, exe: &str) {
    for groups in hooks.values_mut() {
        if let Some(groups) = groups.as_array_mut() {
            for g in groups.iter_mut() {
                if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                    hs.retain(|h| !ours(h, exe));
                }
            }
            groups.retain(|g| {
                g.get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|h| !h.is_empty())
            });
        }
    }
    hooks.retain(|_, g| g.as_array().is_none_or(|a| !a.is_empty()));
}
/// Merges ContextDB's hooks into a harness config (idempotent: ours are
/// replaced, everyone else's are kept).
pub fn merge_hooks(
    config: &mut Value,
    harness: &str,
    exe: &str,
    mcp: &str,
    fail_closed: bool,
) -> Result<(), String> {
    if !config.is_object() {
        *config = json!({});
    }
    let hooks = config
        .as_object_mut()
        .expect("object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().ok_or("\"hooks\" is not an object")?;
    strip(hooks, exe);
    for (event, matcher, timeout, extra) in entries(harness, mcp, fail_closed) {
        let mut handler = match harness {
            "claude" => json!({ "type": "command", "command": exe,
                "args": ["claude", event_arg(event)], "timeout": timeout }),
            _ => json!({ "type": "command",
                "command": format!("{} codex {}", shell_quote(exe)?, event_arg(event)),
                "timeout": timeout }),
        };
        handler
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        let mut group = json!({ "hooks": [handler] });
        if let Some(m) = matcher {
            group["matcher"] = json!(m);
        }
        hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or(format!("hooks.{event} is not an array"))?
            .push(group);
    }
    Ok(())
}
fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}
pub fn hooks(args: &[String]) -> Result<u8, String> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    if sub == "flush" {
        let dir = Path::new(args.get(1).ok_or("usage: ctx hooks flush <dir>")?);
        return flush(dir);
    }
    let harness = flag(args, "--harness").ok_or("--harness claude|codex is required")?;
    if !matches!(harness, "claude" | "codex") {
        return Err("--harness must be claude or codex".into());
    }
    let exe = match flag(args, "--ctx-hook") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_exe()
            .map_err(|e| e.to_string())?
            .with_file_name("ctx-hook"),
    };
    let exe = exe
        .canonicalize()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    let exe = exe.to_string_lossy().into_owned();
    let project = flag(args, "--project").map(PathBuf::from);
    let base = match &project {
        Some(p) => p.clone(),
        None => home()?,
    };
    let file = match harness {
        "claude" => base.join(".claude/settings.json"),
        _ => base.join(".codex/hooks.json"),
    };
    let mut config: Value = match std::fs::read_to_string(&file) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: {e}", file.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    match sub {
        "install" => {
            let mcp = mcp_server_name(flag(args, "--mcp-name").unwrap_or("contextdb"))?;
            let data = flag(args, "--dir").map(PathBuf::from);
            let fail_closed = data
                .as_ref()
                .and_then(|d| std::fs::read(d.join("gating.wire")).ok())
                .and_then(|b| GateConfig::from_bytes(&b).ok())
                .is_some_and(|c| c.fail_closed);
            merge_hooks(&mut config, harness, &exe, &mcp, fail_closed)?;
            if let (Some(root), Some(d)) = (&project, &data) {
                let dot = root.join(".contextdb");
                std::fs::create_dir_all(&dot).map_err(|e| e.to_string())?;
                std::fs::write(dot.join("dir.wire"), d.to_string_lossy().into_owned().canonical_bytes())
                    .map_err(|e| e.to_string())?;
            }
            if harness == "codex"
                && let Some(d) = &data
                && let Ok(out) = std::process::Command::new("codex").arg("--version").output()
            {
                let v = String::from_utf8_lossy(&out.stdout);
                let v = v.split_whitespace().last().unwrap_or("").to_owned();
                let _ = std::fs::write(d.join("codex.version"), v);
            }
        }
        "uninstall" => {
            if let Some(h) = config.get_mut("hooks").and_then(Value::as_object_mut) {
                strip(h, &exe);
            }
        }
        _ => return Err("usage: ctx hooks install|uninstall --harness claude|codex [--project ROOT] [--dir DATA] [--ctx-hook PATH] [--mcp-name NAME]; ctx hooks flush <dir>".into()),
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    let tmp = file.with_extension("json.ctx-tmp");
    std::fs::write(&tmp, text + "\n").map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &file).map_err(|e| e.to_string())?;
    println!(
        "{sub}ed ContextDB hooks for {harness} in {}",
        file.display()
    );
    if harness == "codex" && sub == "install" {
        println!(
            "Codex runs new or edited hooks only after you trust them: open /hooks in Codex (or accept the startup review)."
        );
    }
    Ok(0)
}
/// Delivers every dirty outbox stream now (ADR 0010 §5.2).
fn flush(dir: &Path) -> Result<u8, String> {
    let ob = Outbox::new(dir);
    let mut c = connect(dir, "agent.sock")?;
    let mut n = 0;
    for s in ob.dirty() {
        let st = ob.state(&s);
        if st.harness.is_empty() {
            continue;
        }
        let r = ob
            .deliver(&s, Duration::from_secs(60), |packets, skip| {
                match call(
                    &mut c,
                    Request::Facts {
                        harness: st.harness.clone(),
                        harness_session: st.harness_session.clone(),
                        stream: s,
                        packets: packets.to_vec(),
                        skip,
                    },
                )?
                .response
                {
                    Response::Facts {
                        applied_through, ..
                    } => Ok(Reply::Applied(applied_through)),
                    Response::Gap { expected } => Ok(Reply::Gap(expected)),
                    other => Err(format!("unexpected {other:?}")),
                }
            })
            .map_err(|e| e.to_string())?;
        println!("stream {}: {r:?}", hex(&s));
        if matches!(r, Delivered::Done { remaining: 0, .. }) {
            n += 1;
        }
    }
    println!("{n} streams delivered");
    Ok(0)
}

// ---------------------------------------------------------------- report

fn telemetry(dir: &Path) -> Result<Vec<TelemetryEntry>, String> {
    let mut c = connect(dir, "admin.sock")?;
    let mut all = Vec::new();
    let mut since = 0;
    loop {
        let env = call(
            &mut c,
            Request::Telemetry {
                since,
                limit: 10_000,
            },
        )?;
        let Response::Telemetry { entries, next } = env.response else {
            return Err("unexpected response".into());
        };
        if entries.is_empty() {
            return Ok(all);
        }
        all.extend(entries);
        since = next;
    }
}
/// A stable identity of a telemetry entry: the hash of its encoding. Its
/// position is not stable across daemon restarts (review M6).
pub fn entry_id(e: &TelemetryEntry) -> String {
    hex(&blake3::hash(&e.canonical_bytes()).as_bytes()[..12])
}
/// Human labels (`ctx dogfood review`): entry id → "ok" | "false".
fn labels(dir: &Path) -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(dir.join("reports/review.jsonl")).unwrap_or_default();
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            Some((
                v["id"].as_str()?.to_owned(),
                v["label"].as_str()?.to_owned(),
            ))
        })
        .collect()
}
/// Allowed actions sampled for the legitimate-allow rate (1 in 20, by id so
/// the sample is the same after a restart).
fn sampled(id: &str) -> bool {
    u8::from_str_radix(&id[..2], 16).is_ok_and(|b| b % 20 == 0)
}
#[derive(Debug, Default)]
pub struct Report {
    pub gated: usize,
    pub unavailable: usize,
    pub by_verdict: BTreeMap<(String, String), usize>,
    pub flagged: usize,
    pub flagged_labeled: usize,
    pub false_alarms: usize,
    pub stale_intercepts: usize,
    pub stale_true: usize,
    pub stale_labeled: usize,
    pub allows_sampled: usize,
    pub allows_ok: usize,
    pub writes: usize,
    pub writes_with_deps: usize,
    pub drift_reports: usize,
    pub undeclared: usize,
    pub unrealised: usize,
    pub unclaimed: usize,
    pub skipped: u64,
    pub sessions: usize,
    pub max_parallel: usize,
    pub first: u64,
    pub last: u64,
    pub harness_versions: BTreeMap<String, usize>,
}
/// The G6 dogfood numbers (ADR 0010 §10) from telemetry and labels.
pub fn report(entries: &[TelemetryEntry], labels: &BTreeMap<String, String>) -> Report {
    let mut r = Report::default();
    let mut spans: BTreeMap<[u8; 16], (u64, u64)> = BTreeMap::new();
    for e in entries {
        let id = entry_id(e);
        match e {
            TelemetryEntry::Gate {
                session,
                mode,
                verdict,
                decision,
                available,
                harness_version,
                at,
                ..
            } => {
                if *mode == GateMode::Off {
                    continue;
                }
                r.gated += 1;
                if !available {
                    r.unavailable += 1;
                    continue;
                }
                *r.by_verdict
                    .entry((verdict.clone(), format!("{decision:?}")))
                    .or_default() += 1;
                *r.harness_versions
                    .entry(harness_version.clone())
                    .or_default() += 1;
                if *at > 0 {
                    let span = spans.entry(*session).or_insert((*at, *at));
                    span.0 = span.0.min(*at);
                    span.1 = span.1.max(*at);
                    r.first = if r.first == 0 { *at } else { r.first.min(*at) };
                    r.last = r.last.max(*at);
                }
                let label = labels.get(&id).map(String::as_str);
                if *decision == Decision::Allow {
                    if sampled(&id) && label.is_some() {
                        r.allows_sampled += 1;
                        r.allows_ok += usize::from(label == Some("ok"));
                    }
                    continue;
                }
                r.flagged += 1;
                if label.is_some() {
                    r.flagged_labeled += 1;
                }
                r.false_alarms += usize::from(label == Some("false"));
                if verdict.starts_with("Invalid") || verdict == "Conflicted" {
                    r.stale_intercepts += 1;
                    if label.is_some() {
                        r.stale_labeled += 1;
                        r.stale_true += usize::from(label == Some("ok"));
                    }
                }
            }
            TelemetryEntry::Write { with_deps, .. } => {
                r.writes += 1;
                r.writes_with_deps += usize::from(*with_deps);
            }
            TelemetryEntry::Drift { report, .. } => {
                r.drift_reports += 1;
                r.undeclared += report.undeclared.len();
                r.unrealised += report.unrealised.len();
            }
            TelemetryEntry::Skip { from, to, .. } => r.skipped += to - from + 1,
            TelemetryEntry::Unclaimed { .. } => r.unclaimed += 1,
        }
    }
    r.sessions = spans.len();
    // Most sessions active at once (by first and last gated action).
    let mut edges: Vec<(u64, i32)> = spans
        .values()
        .flat_map(|(a, b)| [(*a, 1), (*b + 1, -1)])
        .collect();
    edges.sort();
    let mut cur = 0i32;
    for (_, d) in edges {
        cur += d;
        r.max_parallel = r.max_parallel.max(cur as usize);
    }
    r
}
fn pct(n: usize, d: usize) -> String {
    if d == 0 {
        "n/a".into()
    } else {
        format!("{:.1}% ({n}/{d})", 100.0 * n as f64 / d as f64)
    }
}
pub fn render_report(r: &Report) -> String {
    let days = (r.last.saturating_sub(r.first)) as f64 / 86_400.0;
    let false_rate = (r.gated > 0).then(|| r.false_alarms as f64 / r.gated as f64);
    let coverage = (r.writes > 0).then(|| r.writes_with_deps as f64 / r.writes as f64);
    let ok = |b: Option<bool>| match b {
        Some(true) => "PASS",
        Some(false) => "FAIL",
        None => "n/a",
    };
    let mut s = String::from(
        "# P4 dogfood report (G6)\n\nGenerated by `ctx gate-report P4` from daemon telemetry and `ctx dogfood review` labels (ADR 0010 §10).\n\n",
    );
    s.push_str("| Gate | Threshold | Measured | Result |\n|---|---|---|---|\n");
    s.push_str(&format!(
        "| Period | ≥ 14 days | {days:.1} days | {} |\n",
        ok((r.first > 0).then_some(days >= 14.0))
    ));
    s.push_str(&format!(
        "| Parallel agents | ≥ 2 | {} sessions, at most {} at once | {} |\n",
        r.sessions,
        r.max_parallel,
        ok((r.sessions > 0).then_some(r.max_parallel >= 2))
    ));
    s.push_str(&format!(
        "| False-deny rate | ≤ 2% (enforce recommendation) | {} | {} |\n",
        pct(r.false_alarms, r.gated),
        ok(false_rate
            .filter(|_| r.flagged_labeled == r.flagged)
            .map(|f| f <= 0.02))
    ));
    s.push_str(&format!(
        "| Dependency coverage | ≥ 60% | {} | {} |\n\n",
        pct(r.writes_with_deps, r.writes),
        ok(coverage.map(|c| c >= 0.60))
    ));
    s.push_str(&format!(
        "Warn/deny labelled: {}/{} (the false-deny rate needs all of them labelled).\n\n",
        r.flagged_labeled, r.flagged
    ));
    s.push_str("Reported only:\n\n");
    s.push_str(&format!(
        "- gated actions {}, unavailable {}\n- stale-action intercepts {}, true positives {}\n- legitimate-action allow rate (sampled allows) {}\n- drift reports {}: {} undeclared, {} unrealised paths\n- MCP calls without a claim {}; outbox facts skipped {}\n",
        r.gated,
        r.unavailable,
        r.stale_intercepts,
        pct(r.stale_true, r.stale_labeled),
        pct(r.allows_ok, r.allows_sampled),
        r.drift_reports,
        r.undeclared,
        r.unrealised,
        r.unclaimed,
        r.skipped
    ));
    s.push_str("\n| Verdict | Decision | Count |\n|---|---|---|\n");
    for ((v, d), n) in &r.by_verdict {
        s.push_str(&format!("| {v} | {d} | {n} |\n"));
    }
    s.push_str("\n| Harness version | Gated |\n|---|---|\n");
    for (v, n) in &r.harness_versions {
        s.push_str(&format!(
            "| {} | {n} |\n",
            if v.is_empty() { "(unknown)" } else { v }
        ));
    }
    s
}
pub fn gate_report(args: &[String]) -> Result<u8, String> {
    if args.first().map(String::as_str) != Some("P4") {
        return Err("usage: ctx gate-report P4 <dir> [--out FILE]".into());
    }
    let dir = Path::new(args.get(1).ok_or("missing <dir>")?);
    let entries = telemetry(dir)?;
    let r = report(&entries, &labels(dir));
    let md = render_report(&r);
    match flag(args, "--out") {
        Some(out) => {
            std::fs::write(out, &md).map_err(|e| e.to_string())?;
            let json = json!({
                "gated": r.gated, "unavailable": r.unavailable, "flagged": r.flagged,
                "flagged_labeled": r.flagged_labeled, "false_alarms": r.false_alarms,
                "writes": r.writes, "writes_with_deps": r.writes_with_deps,
                "sessions": r.sessions, "max_parallel": r.max_parallel,
                "first": r.first, "last": r.last, "unclaimed": r.unclaimed,
            });
            std::fs::write(
                Path::new(out).with_extension("json"),
                serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            println!("wrote {out}");
        }
        None => print!("{md}"),
    }
    Ok(0)
}
/// Labels warn/deny decisions and sampled allows (ADR 0010 §10).
pub fn dogfood(args: &[String]) -> Result<u8, String> {
    if args.first().map(String::as_str) != Some("review") {
        return Err("usage: ctx dogfood review <dir>".into());
    }
    let dir = Path::new(args.get(1).ok_or("missing <dir>")?);
    let entries = telemetry(dir)?;
    let done = labels(dir);
    std::fs::create_dir_all(dir.join("reports")).map_err(|e| e.to_string())?;
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("reports/review.jsonl"))
        .map_err(|e| e.to_string())?;
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    for e in &entries {
        let id = entry_id(e);
        let TelemetryEntry::Gate {
            action,
            verdict,
            decision,
            available: true,
            ..
        } = e
        else {
            continue;
        };
        if done.contains_key(&id) || (*decision == Decision::Allow && !sampled(&id)) {
            continue;
        }
        let question = if *decision == Decision::Allow {
            "Was allowing it right?"
        } else {
            "Should ContextDB have flagged it?"
        };
        println!(
            "{id} {decision:?} ({verdict}) {:?} {:?} — {question} [y/n/s/q]",
            action.tool, action.paths
        );
        let Some(Ok(answer)) = lines.next() else {
            break;
        };
        let label = match answer.trim() {
            "y" => "ok",
            "n" => "false",
            "q" => break,
            _ => continue,
        };
        writeln!(out, "{}", json!({ "id": id, "label": label })).map_err(|e| e.to_string())?;
    }
    Ok(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_and_gating_files_parse_strictly() {
        let p = policy_doc("version = 1\nrequire_deps = [\"Decision\", \"plan\"]\n").unwrap();
        assert_eq!(p.require_deps.len(), 2);
        assert!(policy_doc("version = 2").is_err());
        assert!(policy_doc("version = 1\nrequire = []").is_err());
        assert!(policy_doc("version = 1\nrequire_deps = [\"Nope\"]").is_err());
        let g = gate_config(
            "mode = \"enforce\"\non_unavailable = \"closed\"\ntimeout_ms = 200\n[[rule]]\ntool = \"edit\"\npath = \"src/gen/**\"\nmode = \"off\"\n",
        )
        .unwrap();
        assert_eq!(
            (g.mode, g.fail_closed, g.timeout_ms),
            (GateMode::Enforce, true, 200)
        );
        assert_eq!(g.rules[0].tool, Some(ToolClass::Edit));
        assert!(gate_config("mdoe = \"warn\"").is_err(), "typos are errors");
        assert!(gate_config("mode = \"strict\"").is_err());
        assert!(gate_config("timeout_ms = 0").is_err());
        assert_eq!(gate_config("").unwrap().mode, GateMode::Warn);
    }
    #[test]
    fn hooks_merge_idempotently_and_keep_other_hooks() {
        let mut cfg = json!({ "hooks": { "PreToolUse": [
            { "matcher": "Bash", "hooks": [{ "type": "command", "command": "rtk hook claude" }] }
        ] }, "model": "x" });
        merge_hooks(&mut cfg, "claude", "/bin/ctx-hook", "contextdb", false).unwrap();
        let once = cfg.clone();
        merge_hooks(&mut cfg, "claude", "/bin/ctx-hook", "contextdb", false).unwrap();
        assert_eq!(cfg, once, "idempotent");
        let pre = cfg["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 3, "theirs + edits + claim");
        assert_eq!(pre[0]["hooks"][0]["command"], "rtk hook claude");
        assert!(pre.iter().any(|g| {
            g["matcher"]
                .as_str()
                .unwrap()
                .starts_with("^mcp__contextdb__(")
        }));
        assert_eq!(cfg["model"], "x");
        strip(cfg["hooks"].as_object_mut().unwrap(), "/bin/ctx-hook");
        assert_eq!(
            cfg["hooks"],
            json!({ "PreToolUse": [
            { "matcher": "Bash", "hooks": [{ "type": "command", "command": "rtk hook claude" }] }
        ] })
        );
        let mut codex = json!({});
        merge_hooks(
            &mut codex,
            "codex",
            "/opt/my tools/ctx-hook",
            "contextdb",
            true,
        )
        .unwrap();
        let cmd = codex["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(cmd, "\"/opt/my tools/ctx-hook\" codex pre-tool-use");
        assert!(
            codex["hooks"]["SessionEnd"][0]["hooks"][0]
                .get("async")
                .is_none()
        );
        assert_eq!(shell_quote("a\"$b`c\\").unwrap(), "\"a\\\"\\$b\\`c\\\\\"");
        assert_eq!(mcp_server_name("my.ctx").unwrap(), "my_ctx");
        assert!(mcp_server_name("...").is_err());
    }
    #[test]
    fn report_counts_and_thresholds() {
        let gate = |session: u8, decision, verdict: &str, at| TelemetryEntry::Gate {
            session: [session; 16],
            agent: None,
            action: Action {
                tool: ToolClass::Edit,
                paths: vec![],
                command: None,
            },
            mode: GateMode::Warn,
            verdict: verdict.into(),
            decision,
            available: true,
            harness_version: "2.1.306".into(),
            epoch: Epoch(1),
            at,
        };
        let day = 86_400;
        let mut entries = vec![
            gate(1, Decision::Allow, "Valid", 1_000),
            gate(1, Decision::Warn, "Invalid", 1_000 + day),
            gate(2, Decision::Warn, "Empty", 1_000 + day),
            gate(2, Decision::Allow, "Valid", 1_000 + 15 * day),
        ];
        entries.extend((0..3).map(|n| TelemetryEntry::Write {
            session: [1; 16],
            agent: None,
            with_deps: n < 2,
            epoch: Epoch(2),
        }));
        let id = |i: usize| entry_id(&entries[i]);
        let labels: BTreeMap<String, String> = [
            (id(0), "ok".into()),
            (id(1), "ok".into()),
            (id(2), "false".into()),
        ]
        .into();
        let r = report(&entries, &labels);
        assert_eq!((r.gated, r.flagged, r.false_alarms), (4, 2, 1));
        assert_eq!((r.stale_intercepts, r.stale_true), (1, 1));
        // Allows count only when sampled (by id).
        assert_eq!(r.allows_sampled, usize::from(sampled(&id(0))));
        // Labels survive reordering (a restart rebuilds telemetry in log order).
        let mut shuffled = entries.clone();
        shuffled.reverse();
        let r2 = report(&shuffled, &labels);
        assert_eq!((r2.flagged, r2.false_alarms, r2.stale_true), (2, 1, 1));
        assert_eq!((r.writes, r.writes_with_deps), (3, 2));
        assert_eq!((r.sessions, r.max_parallel), (2, 2));
        let md = render_report(&r);
        assert!(md.contains("15.0 days | PASS"), "{md}");
        assert!(
            md.contains("False-deny rate | ≤ 2% (enforce recommendation) | 25.0% (1/4) | FAIL"),
            "{md}"
        );
        assert!(md.contains("66.7% (2/3) | PASS"), "{md}");
    }
}
