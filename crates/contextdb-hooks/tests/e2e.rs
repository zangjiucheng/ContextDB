//! End to end (ADR 0010 §11): the real `ctx-hook` binary, run as each
//! harness would run it, against a real `ctxd` in a temporary git
//! repository. T38 (stale plan → warn/deny with a short reason), T39 (fail
//! open/closed), claims, `context_update`, drift and outbox recovery.
use contextdb_daemon::{
    proto::*,
    server::{Client, Daemon, Options},
};
use contextdb_hooks::render::REASON_BYTES;
use contextdb_model::*;
use contextdb_proto::gate::GateConfig;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

struct Repo {
    root: PathBuf,
}
impl Repo {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ctx-hook-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&base)
                .args([
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(base.join("README"), "x").unwrap();
        git(&["add", "README"]);
        git(&["commit", "-q", "-m", "init"]);
        Self { root: base }
    }
    fn data(&self) -> PathBuf {
        self.root.join(".contextdb/db")
    }
    fn daemon(&self) -> Daemon {
        std::fs::create_dir_all(self.data()).unwrap();
        Daemon::start(
            &self.data(),
            &Options {
                full_sync: false,
                ..Options::default()
            },
        )
        .unwrap()
    }
    fn gating(&self, cfg: GateConfig) {
        std::fs::create_dir_all(self.data()).unwrap();
        std::fs::write(self.data().join("gating.wire"), cfg.canonical_bytes()).unwrap();
    }
    /// Runs the hook as the harness would: stdin JSON, cwd in the repo.
    fn hook(&self, harness: &str, event: &str, stdin: &Value) -> Option<Value> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ctx-hook"))
            .args([harness, event])
            .current_dir(&self.root)
            .env_remove("CTX_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "ctx-hook must always exit 0");
        let text = String::from_utf8(out.stdout).unwrap();
        (!text.trim().is_empty()).then(|| serde_json::from_str(&text).unwrap())
    }
}
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn input(event: &str, session: &str, cwd: &Path, extra: Value) -> Value {
    let mut v = json!({ "session_id": session, "cwd": cwd, "hook_event_name": event,
        "transcript_path": null, "permission_mode": "default" });
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v
}
fn edit(repo: &Repo, session: &str) -> Value {
    input(
        "PreToolUse",
        session,
        &repo.root,
        json!({ "tool_name": "Edit", "tool_use_id": "toolu_e",
            "tool_input": { "file_path": repo.root.join("src/lib.rs"), "old_string": "a", "new_string": "b" } }),
    )
}
fn patch(repo: &Repo, session: &str) -> Value {
    input(
        "PreToolUse",
        session,
        &repo.root,
        json!({ "tool_name": "apply_patch", "tool_use_id": "call_p", "turn_id": "1", "model": "m",
            "tool_input": { "command": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n*** End Patch\n" } }),
    )
}
fn agent(d: &Daemon) -> Client {
    Client::connect(d.socket(Channel::Agent)).unwrap()
}
fn attach(c: &mut Client, harness: &str, key: &str) -> SessionId {
    match c
        .call(&Request::SessionAttach {
            harness: harness.into(),
            harness_session: key.into(),
            repo: String::new(),
            head_sha: String::new(),
            branch: None,
            git_branch: None,
        })
        .unwrap()
        .response
    {
        Response::Attached { session, .. } => session,
        other => panic!("{other:?}"),
    }
}
fn record(c: &mut Client, s: SessionId, key: &str, body: &str) {
    let r = c
        .call(&Request::Record {
            binding: Binding::root(s),
            kind: RecordKind::Decision,
            body: body.as_bytes().to_vec(),
            target: WriteTarget::Create {
                key: key.into(),
                scope_lane: "repo".into(),
            },
            deps: Vec::new(),
            adopt_as_base: true,
            client_txn_id: None,
        })
        .unwrap();
    assert!(matches!(r.response, Response::Committed { .. }), "{r:?}");
}
fn read(c: &mut Client, s: SessionId, key: &str) -> (LogicalId, RevisionId) {
    match c
        .call(&Request::Read {
            binding: Binding::root(s),
            target: Target::Key(key.into()),
            mode: Mode::Certified,
        })
        .unwrap()
        .response
    {
        Response::Read { item, .. } => (item.logical, item.rev),
        other => panic!("{other:?}"),
    }
}
fn supersede(c: &mut Client, s: SessionId, l: LogicalId, r: RevisionId) {
    let resp = c
        .call(&Request::Supersede {
            binding: Binding::root(s),
            logical: l,
            body: format!("changed from r{}", r.0).into_bytes(),
            expected_head: r,
            adopt_as_base: true,
            carry_rules: false,
            reason: "switch".into(),
            client_txn_id: None,
        })
        .unwrap();
    assert!(
        matches!(resp.response, Response::Committed { .. }),
        "{resp:?}"
    );
}
fn enforce() -> GateConfig {
    GateConfig {
        mode: GateMode::Enforce,
        ..GateConfig::default()
    }
}

/// T38: a stale read makes PreToolUse warn (warn mode) or deny (enforce),
/// for both harnesses, with a reason of at most 480 bytes naming the read.
#[test]
fn t38_stale_plan_warns_or_denies_with_a_short_reason() {
    let repo = Repo::new("t38");
    let d = repo.daemon();
    let mut c = agent(&d);
    let other = attach(&mut c, "claude", "other");
    record(&mut c, other, "db", "postgres");
    for (harness, session, stdin) in [
        ("claude", "s-claude", edit(&repo, "s-claude")),
        ("codex", "s-codex", patch(&repo, "s-codex")),
    ] {
        repo.gating(GateConfig {
            timeout_ms: 5_000,
            ..GateConfig::default()
        });
        let s = attach(&mut c, harness, session);
        let (l, r) = read(&mut c, s, "db");
        // Fresh read: nothing printed (allow never renders).
        assert_eq!(
            repo.hook(harness, "pre-tool-use", &stdin),
            None,
            "{harness}"
        );
        supersede(&mut c, other, l, r);
        let warn = repo.hook(harness, "pre-tool-use", &stdin).expect(harness);
        let ctx = warn["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("db"), "{ctx}");
        assert!(
            warn["hookSpecificOutput"]
                .get("permissionDecision")
                .is_none()
        );
        repo.gating(enforce());
        let deny = repo.hook(harness, "pre-tool-use", &stdin).unwrap();
        assert_eq!(deny["hookSpecificOutput"]["permissionDecision"], "deny");
        let reason = deny["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert!(
            reason.contains("db") && reason.len() <= REASON_BYTES,
            "{reason}"
        );
        // Re-reading the new head clears it.
        read(&mut c, s, "db");
        assert_eq!(repo.hook(harness, "pre-tool-use", &stdin), None);
    }
}

/// T39: daemon unreachable, stalled or speaking another protocol, and
/// unparsed input: fail-open notices, fail-closed denies; no data directory
/// means no output at all.
#[test]
fn t39_unavailable_daemon_fails_open_or_closed() {
    // No ContextDB data directory: nothing, whatever happens.
    let plain = Repo::new("t39-plain");
    assert_eq!(
        plain.hook("claude", "pre-tool-use", &edit(&plain, "s")),
        None
    );
    let repo = Repo::new("t39");
    let open = GateConfig {
        timeout_ms: 100,
        ..GateConfig::default()
    };
    let closed = GateConfig {
        fail_closed: true,
        ..open.clone()
    };
    // Down: a data directory but no daemon.
    repo.gating(open.clone());
    for (h, stdin) in [("claude", edit(&repo, "s")), ("codex", patch(&repo, "s"))] {
        let o = repo.hook(h, "pre-tool-use", &stdin).unwrap();
        assert!(
            o["systemMessage"].as_str().unwrap().contains("unavailable"),
            "{o}"
        );
        assert!(o.get("hookSpecificOutput").is_none());
    }
    repo.gating(closed.clone());
    let o = repo
        .hook("claude", "pre-tool-use", &edit(&repo, "s"))
        .unwrap();
    assert_eq!(o["hookSpecificOutput"]["permissionDecision"], "deny");
    // Unparsed input follows the same rule and is reported.
    let o = repo
        .hook("codex", "pre-tool-use", &json!({ "cwd": repo.root }))
        .unwrap();
    assert_eq!(o["hookSpecificOutput"]["permissionDecision"], "deny");
    repo.gating(open.clone());
    let o = repo
        .hook("codex", "pre-tool-use", &json!({ "cwd": repo.root }))
        .unwrap();
    assert!(
        o["systemMessage"]
            .as_str()
            .unwrap()
            .contains("could not read")
    );
    // Stalled: a socket that accepts and never answers; bounded by timeout_ms.
    let short = PathBuf::from(format!("/tmp/ctxstall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&short);
    std::fs::create_dir_all(&short).unwrap();
    let sock = short.join("stall.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let lease = contextdb_proto::lease::Lease {
        pid: std::process::id(),
        start_time: String::new(),
        protocol: PROTOCOL_VERSION,
        sockets: vec![sock.with_file_name("agent.sock")],
    };
    std::fs::rename(&sock, sock.with_file_name("agent.sock")).unwrap();
    contextdb_proto::lease::write_lease(&repo.data().join("ctxd.lease"), &lease).unwrap();
    let hold = std::thread::spawn(move || {
        let conns: Vec<_> = listener.incoming().take(2).collect();
        std::thread::sleep(Duration::from_secs(3));
        drop(conns);
    });
    repo.gating(closed);
    let t = Instant::now();
    let o = repo
        .hook("claude", "pre-tool-use", &edit(&repo, "s"))
        .unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(o["hookSpecificOutput"]["permissionDecision"], "deny");
    repo.gating(open);
    let o = repo
        .hook("claude", "pre-tool-use", &edit(&repo, "s"))
        .unwrap();
    assert!(o["systemMessage"].as_str().unwrap().contains("unavailable"));
    hold.join().unwrap();
    let _ = std::fs::remove_dir_all(&short);
    // Unavailable checks were kept for the report.
    let outbox = repo.data().join("outbox");
    assert!(std::fs::read_dir(outbox).unwrap().count() > 0);
}

#[test]
fn claude_claims_bind_mcp_reads_and_updates_are_injected_once_acked() {
    let repo = Repo::new("claims");
    let d = repo.daemon();
    let mut c = agent(&d);
    let other = attach(&mut c, "claude", "other");
    record(&mut c, other, "db", "postgres");
    // The PreToolUse hook claims a ContextDB MCP call for its session.
    let claim = input(
        "PreToolUse",
        "sess-1",
        &repo.root,
        json!({ "tool_name": "mcp__contextdb__ctx_read", "tool_use_id": "toolu_77",
            "tool_input": { "target": "db" } }),
    );
    assert_eq!(repo.hook("claude", "pre-tool-use", &claim), None);
    let r = c
        .call(&Request::Read {
            binding: Binding::Call {
                id: "toolu_77".into(),
            },
            target: Target::Key("db".into()),
            mode: Mode::Certified,
        })
        .unwrap();
    let Response::Read { item, .. } = r.response else {
        panic!("{r:?}")
    };
    // The hook's own session now has the read: an edit is allowed.
    repo.gating(enforce());
    assert_eq!(
        repo.hook("claude", "pre-tool-use", &edit(&repo, "sess-1")),
        None
    );
    // Superseded elsewhere: the next prompt carries a context_update, then
    // (once acknowledged by the following prompt) nothing.
    supersede(&mut c, other, item.logical, item.rev);
    let prompt = input(
        "UserPromptSubmit",
        "sess-1",
        &repo.root,
        json!({ "prompt": "go" }),
    );
    let first = repo.hook("claude", "user-prompt-submit", &prompt).unwrap();
    let ctx = first["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        ctx.starts_with("<context_update epoch=") && ctx.contains("db"),
        "{ctx}"
    );
    assert_eq!(repo.hook("claude", "user-prompt-submit", &prompt), None);
}

#[test]
fn drift_and_outbox_recovery_after_the_daemon_was_down() {
    let repo = Repo::new("drift");
    let start = input(
        "SessionStart",
        "sess-d",
        &repo.root,
        json!({ "source": "startup" }),
    );
    // The daemon is down for the whole session start and the edit.
    std::fs::create_dir_all(repo.data()).unwrap();
    let o = repo.hook("claude", "session-start", &start).unwrap();
    assert!(o["systemMessage"].as_str().unwrap().contains("unavailable"));
    std::fs::create_dir_all(repo.root.join("src")).unwrap();
    std::fs::write(repo.root.join("src/lib.rs"), "a").unwrap();
    let post = input(
        "PostToolUse",
        "sess-d",
        &repo.root,
        json!({ "tool_name": "Write", "tool_use_id": "toolu_w",
            "tool_input": { "file_path": repo.root.join("src/lib.rs"), "content": "a" },
            "tool_response": {} }),
    );
    assert_eq!(repo.hook("claude", "post-tool-use", &post), None);
    // A shell side effect no hook recorded.
    std::fs::write(repo.root.join("side.txt"), "b").unwrap();
    // The daemon comes back; Stop delivers everything and reports drift.
    let d = repo.daemon();
    let stop = input(
        "Stop",
        "sess-d",
        &repo.root,
        json!({ "stop_hook_active": false }),
    );
    let o = repo.hook("claude", "stop", &stop).unwrap();
    let msg = o["systemMessage"].as_str().unwrap();
    assert!(
        msg.contains("side.txt") && !msg.contains("src/lib.rs"),
        "{msg}"
    );
    assert!(o.get("hookSpecificOutput").is_none());
    // Delivering again changes nothing (idempotent per stream seq).
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    let count = |admin: &mut Client| match admin
        .call(&Request::Telemetry {
            since: 0,
            limit: 1_000,
        })
        .unwrap()
        .response
    {
        Response::Telemetry { entries, .. } => entries
            .iter()
            .filter(|e| matches!(e, TelemetryEntry::Drift { .. }))
            .count(),
        other => panic!("{other:?}"),
    };
    assert_eq!(count(&mut admin), 1);
    let end = input(
        "SessionEnd",
        "sess-d",
        &repo.root,
        json!({ "reason": "other" }),
    );
    assert_eq!(repo.hook("claude", "session-end", &end), None);
    assert_eq!(count(&mut admin), 1);
}

/// Review M3: after a UserPromptSubmit acknowledged the update, a deny still
/// names the stale read (from the check itself, not the update peek).
#[test]
fn deny_names_the_stale_read_after_the_update_was_acknowledged() {
    let repo = Repo::new("named");
    let d = repo.daemon();
    let mut c = agent(&d);
    let other = attach(&mut c, "claude", "other");
    record(&mut c, other, "db", "postgres");
    let s = attach(&mut c, "claude", "sess-n");
    let (l, r) = read(&mut c, s, "db");
    supersede(&mut c, other, l, r);
    let prompt = input(
        "UserPromptSubmit",
        "sess-n",
        &repo.root,
        json!({ "prompt": "go" }),
    );
    assert!(repo.hook("claude", "user-prompt-submit", &prompt).is_some());
    assert_eq!(
        repo.hook("claude", "user-prompt-submit", &prompt),
        None,
        "acknowledged"
    );
    repo.gating(enforce());
    let deny = repo
        .hook("claude", "pre-tool-use", &edit(&repo, "sess-n"))
        .unwrap();
    let reason = deny["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(reason.contains("db: read r"), "{reason}");
}

/// Review M2: from a linked worktree, paths are relative to that worktree,
/// git runs there, and the data directory is the main worktree's.
#[test]
fn linked_worktrees_share_the_data_directory_and_use_their_own_paths() {
    let repo = Repo::new("wt");
    let d = repo.daemon();
    let wt = repo.root.with_file_name(format!(
        "{}-wt",
        repo.root.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&wt);
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "add", "-q", "--detach"])
            .arg(&wt)
            .output()
            .unwrap()
            .status
            .success()
    );
    repo.gating(GateConfig {
        rules: vec![contextdb_proto::gate::GateRule {
            tool: None,
            path: Some("src/**".into()),
            mode: GateMode::Enforce,
        }],
        ..GateConfig::default()
    });
    let run = |event: &str, stdin: &Value| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ctx-hook"))
            .args(["claude", event])
            .current_dir(&wt)
            .env_remove("CTX_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        (!text.trim().is_empty()).then(|| serde_json::from_str::<Value>(&text).unwrap())
    };
    let start = input(
        "SessionStart",
        "sess-wt",
        &wt,
        json!({ "source": "startup" }),
    );
    run("session-start", &start);
    // An Edit in the worktree is `src/a.rs`: the enforce rule applies (Empty
    // warns), and the daemon logged it with the relative path.
    let e = input(
        "PreToolUse",
        "sess-wt",
        &wt,
        json!({ "tool_name": "Edit", "tool_use_id": "t",
            "tool_input": { "file_path": wt.join("src/a.rs"), "old_string": "a", "new_string": "b" } }),
    );
    run("pre-tool-use", &e);
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    let Response::Telemetry { entries, .. } = admin
        .call(&Request::Telemetry {
            since: 0,
            limit: 100,
        })
        .unwrap()
        .response
    else {
        panic!()
    };
    assert!(
        entries.iter().any(|x| matches!(x,
            TelemetryEntry::Gate { action, mode: GateMode::Enforce, .. } if action.paths == ["src/a.rs"])),
        "{entries:?}"
    );
    // Drift runs in the worktree: a file written there is seen.
    std::fs::write(wt.join("wt-side.txt"), "x").unwrap();
    let stop = input("Stop", "sess-wt", &wt, json!({ "stop_hook_active": false }));
    let o = run("stop", &stop).unwrap();
    assert!(
        o["systemMessage"].as_str().unwrap().contains("wt-side.txt"),
        "{o}"
    );
    let _ = Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["worktree", "remove", "--force"])
        .arg(&wt)
        .output();
}

/// Review M5: a SessionStart after compaction keeps the first baseline, so
/// the session's own earlier edit is not reported as unrealised; and
/// minor 1: facts appended with the daemon down and no SessionStart are
/// swept by `ctxd` (the stream names its harness session).
#[test]
fn compaction_keeps_the_baseline_and_streams_without_session_start_are_swept() {
    let repo = Repo::new("compact");
    std::fs::create_dir_all(repo.data()).unwrap();
    std::fs::create_dir_all(repo.root.join("src")).unwrap();
    // No SessionStart, daemon down: the edit fact waits in the outbox.
    std::fs::write(repo.root.join("src/lib.rs"), "a").unwrap();
    let post = input(
        "PostToolUse",
        "sess-c",
        &repo.root,
        json!({ "tool_name": "Write", "tool_use_id": "w",
            "tool_input": { "file_path": repo.root.join("src/lib.rs"), "content": "a" },
            "tool_response": {} }),
    );
    repo.hook("claude", "post-tool-use", &post);
    let d = repo.daemon();
    let ob = contextdb_proto::outbox::Outbox::new(&repo.data());
    let t = Instant::now();
    while !ob.dirty().is_empty() && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ob.dirty().is_empty(), "the sweeper delivered the stream");
    // SessionStart (startup), then again (compact): the edit made before the
    // compaction stays the session's own.
    let start = |source: &str| {
        input(
            "SessionStart",
            "sess-c",
            &repo.root,
            json!({ "source": source }),
        )
    };
    repo.hook("claude", "session-start", &start("startup"));
    std::fs::write(repo.root.join("src/lib.rs"), "b").unwrap();
    repo.hook("claude", "post-tool-use", &post);
    repo.hook("claude", "session-start", &start("compact"));
    let stop = input(
        "Stop",
        "sess-c",
        &repo.root,
        json!({ "stop_hook_active": false }),
    );
    if let Some(o) = repo.hook("claude", "stop", &stop) {
        let msg = o["systemMessage"].as_str().unwrap_or("");
        assert!(!msg.contains("src/lib.rs"), "{msg}");
    }
    drop(d);
}

/// Closure check N1 and N3: a clean SessionEnd ends the session without
/// creating another; resuming the same harness session id starts a new
/// session on a new stream, and an edit made before the end is not reported
/// as undeclared drift.
#[test]
fn session_end_and_resume_make_no_phantom_and_keep_earlier_edits_declared() {
    let repo = Repo::new("resume");
    let d = repo.daemon();
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    let live = |admin: &mut Client| match admin.call(&Request::Status).unwrap().response {
        Response::Status { sessions, .. } => sessions,
        other => panic!("{other:?}"),
    };
    let start = input(
        "SessionStart",
        "sess-r",
        &repo.root,
        json!({ "source": "startup" }),
    );
    repo.hook("claude", "session-start", &start);
    assert_eq!(live(&mut admin), 1);
    std::fs::write(repo.root.join("early.txt"), "x").unwrap();
    let post = input(
        "PostToolUse",
        "sess-r",
        &repo.root,
        json!({ "tool_name": "Write", "tool_use_id": "w",
            "tool_input": { "file_path": repo.root.join("early.txt"), "content": "x" },
            "tool_response": {} }),
    );
    repo.hook("claude", "post-tool-use", &post);
    let end = input(
        "SessionEnd",
        "sess-r",
        &repo.root,
        json!({ "reason": "other" }),
    );
    repo.hook("claude", "session-end", &end);
    assert_eq!(live(&mut admin), 0, "ended, and no phantom session left");
    // Resume with the same id: a new session, and the earlier edit is the
    // same harness session's own.
    let resume = input(
        "SessionStart",
        "sess-r",
        &repo.root,
        json!({ "source": "resume" }),
    );
    repo.hook("claude", "session-start", &resume);
    assert_eq!(live(&mut admin), 1);
    // Changed again after the resume (so the new baseline does not hide
    // it): still declared, by the same harness session's earlier session.
    std::fs::write(repo.root.join("early.txt"), "y").unwrap();
    let stop = input(
        "Stop",
        "sess-r",
        &repo.root,
        json!({ "stop_hook_active": false }),
    );
    if let Some(o) = repo.hook("claude", "stop", &stop) {
        let msg = o["systemMessage"].as_str().unwrap_or("");
        assert!(!msg.contains("early.txt"), "{msg}");
    }
}

/// Drift compares against HEAD at SessionStart (mutation closure): a file
/// committed during the session, by a Bash side effect, is still reported.
#[test]
fn drift_includes_files_committed_after_session_start() {
    let repo = Repo::new("commit");
    let d = repo.daemon();
    let start = input(
        "SessionStart",
        "sess-g",
        &repo.root,
        json!({ "source": "startup" }),
    );
    repo.hook("claude", "session-start", &start);
    std::fs::write(repo.root.join("committed.txt"), "x").unwrap();
    for a in [
        vec!["add", "committed.txt"],
        vec![
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "side",
        ],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo.root)
                .args(a)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let stop = input(
        "Stop",
        "sess-g",
        &repo.root,
        json!({ "stop_hook_active": false }),
    );
    let o = repo.hook("claude", "stop", &stop).expect("drift notice");
    assert!(
        o["systemMessage"]
            .as_str()
            .unwrap()
            .contains("committed.txt"),
        "{o}"
    );
    drop(d);
}
