//! SPEC Gate P4 G6 scenario (ADR 0010 §11): two scripted agents, one per
//! harness, through the real `ctx-hook` binary and MCP-style bindings, share
//! a decision in one repository with one `ctxd`.
//! 1. Claude agent A records `db`; Codex agent B reads it (Codex `_meta` key).
//! 2. A supersedes `db`.
//! 3. B's next `apply_patch` is warned (warn mode), then denied (enforce).
//! 4. B writes a file through Bash: Stop reports it as undeclared drift.
//! 5. A's subagent reads `plan` through a claimed MCP call; when `plan`
//!    changes, A's own PreToolUse is denied: the child's read is covered.
//! 6. Both sessions' MCP calls went through one server-like path and landed
//!    in their own sessions (A never read `db`, B never read `plan`).
use contextdb_daemon::{
    proto::*,
    server::{Client, Daemon, Options},
};
use contextdb_model::*;
use contextdb_proto::gate::GateConfig;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

struct World {
    root: PathBuf,
    daemon: Daemon,
    client: Client,
}
impl World {
    fn new() -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ctx-g6-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for a in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.email=g@g",
                "-c",
                "user.name=g",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "i",
            ],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(a)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let data = root.join(".contextdb/db");
        std::fs::create_dir_all(&data).unwrap();
        let daemon = Daemon::start(&data, &Options::default()).unwrap();
        let client = Client::connect(daemon.socket(Channel::Agent)).unwrap();
        Self {
            root,
            daemon,
            client,
        }
    }
    fn gating(&self, mode: GateMode) {
        let cfg = GateConfig {
            mode,
            ..GateConfig::default()
        };
        std::fs::write(
            self.root.join(".contextdb/db/gating.wire"),
            cfg.canonical_bytes(),
        )
        .unwrap();
    }
    fn hook(&self, harness: &str, event: &str, stdin: Value) -> Option<Value> {
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
        let text = String::from_utf8(out.stdout).unwrap();
        (!text.trim().is_empty()).then(|| serde_json::from_str(&text).unwrap())
    }
    fn call(&mut self, r: Request) -> Response {
        let env = self.client.call(&r).unwrap();
        if let Response::Error { code, detail } = &env.response {
            panic!("{code}: {detail}");
        }
        env.response
    }
}
impl Drop for World {
    fn drop(&mut self) {
        self.daemon.stop();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn ev(session: &str, event: &str, root: &PathBuf, extra: Value) -> Value {
    let mut v = json!({ "session_id": session, "cwd": root, "hook_event_name": event,
        "transcript_path": null, "permission_mode": "default", "turn_id": "1", "model": "m" });
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v
}
fn verdict(w: &mut World, b: Binding) -> Validation {
    match w.call(Request::Check {
        binding: b,
        cert_id: None,
        live_pos: Vec::new(),
        action: None,
        mode: None,
        harness_version: String::new(),
        strict: false,
    }) {
        Response::Check { validation, .. } => validation,
        other => panic!("{other:?}"),
    }
}

#[test]
fn g6_two_agents_two_harnesses_one_decision() {
    let mut w = World::new();
    let root = w.root.clone();
    // Both sessions start through their hooks.
    w.hook(
        "claude",
        "session-start",
        ev("A", "SessionStart", &root, json!({ "source": "startup" })),
    );
    w.hook(
        "codex",
        "session-start",
        ev("B", "SessionStart", &root, json!({ "source": "startup" })),
    );
    let a_key = |agent: Option<&str>| Binding::Key {
        harness: "claude".into(),
        harness_session: "A".into(),
        agent: agent.map(Into::into),
    };
    let b_key = Binding::Key {
        harness: "codex".into(),
        harness_session: "B".into(),
        agent: None,
    };
    // 1. A records db and plan; B reads db (Codex `_meta` binding).
    let rec = |w: &mut World, key: &str, body: &str| match w.call(Request::Record {
        binding: a_key(None),
        kind: RecordKind::Decision,
        body: body.as_bytes().to_vec(),
        target: WriteTarget::Create {
            key: key.into(),
            scope_lane: "repo".into(),
        },
        deps: Vec::new(),
        adopt_as_base: true,
        client_txn_id: None,
    }) {
        Response::Committed { result } => result.allocations.new_revisions[0],
        other => panic!("{other:?}"),
    };
    let db = rec(&mut w, "db", "postgres");
    let plan = rec(&mut w, "plan", "migrate in May");
    w.call(Request::Read {
        binding: b_key.clone(),
        target: Target::Key("db".into()),
        mode: Mode::Certified,
    });
    let patch = ev(
        "B",
        "PreToolUse",
        &root,
        json!({ "tool_name": "apply_patch", "tool_use_id": "c1",
            "tool_input": { "command": "*** Begin Patch\n*** Update File: src/db.rs\n@@\n-a\n+b\n*** End Patch\n" } }),
    );
    w.gating(GateMode::Warn);
    assert_eq!(
        w.hook("codex", "pre-tool-use", patch.clone()),
        None,
        "B's read is current"
    );
    // 2. A supersedes db.
    w.call(Request::Supersede {
        binding: a_key(None),
        logical: LogicalId(0),
        body: b"mysql".to_vec(),
        expected_head: db,
        adopt_as_base: true,
        carry_rules: false,
        reason: "switch".into(),
        client_txn_id: None,
    });
    // 3. B is warned, then denied under enforce, naming db.
    let warn = w.hook("codex", "pre-tool-use", patch.clone()).unwrap();
    assert!(
        warn["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("db")
    );
    w.gating(GateMode::Enforce);
    let deny = w.hook("codex", "pre-tool-use", patch).unwrap();
    assert_eq!(deny["hookSpecificOutput"]["permissionDecision"], "deny");
    // 4. A file written through Bash shows up as undeclared drift for B.
    std::fs::write(root.join("bash-side-effect.txt"), "x").unwrap();
    let stop = w
        .hook(
            "codex",
            "stop",
            ev(
                "B",
                "Stop",
                &root,
                json!({ "stop_hook_active": false, "last_assistant_message": null }),
            ),
        )
        .unwrap();
    assert!(
        stop["systemMessage"]
            .as_str()
            .unwrap()
            .contains("bash-side-effect.txt")
    );
    // 5. A's subagent reads plan through a claimed Claude MCP call.
    let claim = ev(
        "A",
        "PreToolUse",
        &root,
        json!({ "tool_name": "mcp__contextdb__ctx_read", "tool_use_id": "toolu_child",
            "agent_id": "sub-1", "agent_type": "Explore", "tool_input": { "target": "plan" } }),
    );
    assert_eq!(w.hook("claude", "pre-tool-use", claim), None);
    w.call(Request::Read {
        binding: Binding::Call {
            id: "toolu_child".into(),
        },
        target: Target::Key("plan".into()),
        mode: Mode::Certified,
    });
    let a_edit = ev(
        "A",
        "PreToolUse",
        &root,
        json!({ "tool_name": "Edit", "tool_use_id": "toolu_e",
            "tool_input": { "file_path": root.join("src/plan.rs"), "old_string": "a", "new_string": "b" } }),
    );
    assert_eq!(
        w.hook("claude", "pre-tool-use", a_edit.clone()),
        None,
        "the child's read backs A"
    );
    w.call(Request::Supersede {
        binding: b_key.clone(),
        logical: LogicalId(1),
        body: b"migrate in June".to_vec(),
        expected_head: plan,
        adopt_as_base: true,
        carry_rules: false,
        reason: "slip".into(),
        client_txn_id: None,
    });
    let deny = w.hook("claude", "pre-tool-use", a_edit).unwrap();
    assert_eq!(deny["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(
        deny["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("plan")
    );
    // 6. Neither session holds the other's read: A never read db, B never plan.
    let Response::Updates { items, .. } = w.call(Request::Updates {
        binding: a_key(None),
        ack: None,
        max_items: 10,
    }) else {
        panic!()
    };
    assert!(items.iter().all(|i| i.key != "db"), "{items:?}");
    assert!(matches!(verdict(&mut w, b_key), Validation::Invalid { .. }));
}
