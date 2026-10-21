//! P4 CLI end to end (ADR 0010 §6.4, §8): T41 (policy is enforced in the
//! kernel transaction on every channel; hooks play no part) and `ctx check`
//! exit codes, dry run and unavailability.
use contextdb_daemon::{
    proto::*,
    server::{Client, Daemon, Options},
};
use contextdb_model::*;
use std::process::{Command, Stdio};

fn ctx(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.code().unwrap_or(-1), text)
}
fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("ctx-p4-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
fn decision(binding: Binding, key: &str) -> Request {
    record(binding, key, RecordKind::Decision, false)
}
fn record(binding: Binding, key: &str, kind: RecordKind, adopt_as_base: bool) -> Request {
    Request::Record {
        binding,
        kind,
        body: key.as_bytes().to_vec(),
        target: WriteTarget::Create {
            key: key.into(),
            scope_lane: "repo".into(),
        },
        deps: Vec::new(),
        adopt_as_base,
        client_txn_id: None,
    }
}
fn code(r: &Response) -> &str {
    match r {
        Response::Error { code, .. } => code,
        _ => "ok",
    }
}

/// T41: a policy committed by `ctx policy apply` is enforced in the kernel
/// transaction, on the agent channel and on admin `Raw` alike, with no hook
/// installed anywhere (so git `--no-verify` or removing hooks cannot bypass it).
#[test]
fn t41_policy_violations_are_rejected_on_every_channel() {
    let dir = tmp("t41");
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let path = dir.to_str().unwrap();
    let file = dir.join("policy.toml");
    std::fs::write(&file, "version = 1\nrequire_deps = [\"Decision\"]\n").unwrap();
    let (rc, out) = ctx(&["policy", "show", path, "--file", file.to_str().unwrap()]);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("canonical hash"), "{out}");
    let (rc, out) = ctx(&["policy", "apply", path, "--file", file.to_str().unwrap()]);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("policy committed at epoch 1"), "{out}");
    // Agent channel (what ctx-mcp uses).
    let mut agent = Client::connect(d.socket(Channel::Agent)).unwrap();
    let Response::Attached { session, .. } = agent
        .call(&Request::SessionAttach {
            harness: "t".into(),
            harness_session: "k".into(),
            repo: String::new(),
            head_sha: String::new(),
        })
        .unwrap()
        .response
    else {
        panic!()
    };
    let r = agent.call(&decision(Binding::root(session), "d1")).unwrap();
    assert_eq!(code(&r.response), "PolicyViolation", "{r:?}");
    // The most privileged channel, as a raw batch.
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    let raw = Request::Raw {
        blobs: vec![b"d2".to_vec()],
        batch: Batch::new(vec![Op::CreateLogical {
            key: "d2".into(),
            kind: RecordKind::Decision,
            scope_lane: "repo".into(),
            content: *blake3::hash(b"d2").as_bytes(),
            adopt_as_base: false,
        }]),
    };
    assert_eq!(code(&admin.call(&raw).unwrap().response), "PolicyViolation");
    // Kinds the policy does not name are unaffected.
    let fact = record(Binding::root(session), "f1", RecordKind::Fact, false);
    assert_eq!(code(&agent.call(&fact).unwrap().response), "ok");
    // A bad file never reaches the daemon.
    std::fs::write(&file, "version = 1\nrequire_deps = [\"Nope\"]\n").unwrap();
    let (rc, out) = ctx(&["policy", "apply", path, "--file", file.to_str().unwrap()]);
    assert_eq!(rc, 2, "{out}");
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ctx_check_exit_codes_dry_run_and_unavailable() {
    let dir = tmp("check");
    let path = dir.to_str().unwrap().to_owned();
    // No daemon: unavailable.
    let (rc, _) = ctx(&["check", &path, "--harness", "t", "--key", "k"]);
    assert_eq!(rc, 3);
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let mut agent = Client::connect(d.socket(Channel::Agent)).unwrap();
    let attach = |agent: &mut Client, key: &str| match agent
        .call(&Request::SessionAttach {
            harness: "t".into(),
            harness_session: key.into(),
            repo: String::new(),
            head_sha: String::new(),
        })
        .unwrap()
        .response
    {
        Response::Attached { session, .. } => session,
        other => panic!("{other:?}"),
    };
    let (me, other) = (attach(&mut agent, "k"), attach(&mut agent, "o"));
    let rec = record(Binding::root(other), "db", RecordKind::Decision, true);
    agent.call(&rec).unwrap();
    let read = |agent: &mut Client| match agent
        .call(&Request::Read {
            binding: Binding::root(me),
            target: Target::Key("db".into()),
            mode: Mode::Certified,
        })
        .unwrap()
        .response
    {
        Response::Read { item, .. } => item,
        other => panic!("{other:?}"),
    };
    let item = read(&mut agent);
    // A plan built on db@r: superseding db makes this head STALE (for --all).
    let plan = Request::Record {
        binding: Binding::root(other),
        kind: RecordKind::Plan,
        body: b"plan".to_vec(),
        target: WriteTarget::Create {
            key: "plan".into(),
            scope_lane: "repo".into(),
        },
        deps: vec![Dep {
            target: Target::Pinned {
                logical: item.logical,
                rev: item.rev,
            },
            kind: EdgeKind::Cur,
        }],
        adopt_as_base: false,
        client_txn_id: None,
    };
    assert_eq!(code(&agent.call(&plan).unwrap().response), "ok");
    let key = ["check", path.as_str(), "--harness", "t", "--key", "k"];
    let (rc, out) = ctx(&key);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("verdict=Valid"), "{out}");
    agent
        .call(&Request::Supersede {
            binding: Binding::root(other),
            logical: item.logical,
            body: b"mysql".to_vec(),
            expected_head: item.rev,
            adopt_as_base: true,
            carry_rules: false,
            reason: "switch".into(),
            client_txn_id: None,
        })
        .unwrap();
    // Warn mode: exit 0, or 1 with --fail-on-warn.
    let (rc, out) = ctx(&key);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("decision=Warn"), "{out}");
    let mut fail = key.to_vec();
    fail.push("--fail-on-warn");
    assert_eq!(ctx(&fail).0, 1);
    // Enforce: deny, exit 1; a dry run decides the same without logging.
    let gating = dir.join("gating.toml");
    std::fs::write(&gating, "mode = \"enforce\"\n").unwrap();
    let (rc, out) = ctx(&["gating", "apply", &path, "--file", gating.to_str().unwrap()]);
    assert_eq!(rc, 0, "{out}");
    let (rc, out) = ctx(&key);
    assert_eq!(rc, 1, "{out}");
    let mut dry = key.to_vec();
    dry.extend([
        "--dry-run",
        "--action-tool",
        "edit",
        "--action-path",
        "src/a.rs",
    ]);
    let (rc, out) = ctx(&dry);
    assert_eq!(rc, 1, "{out}");
    assert!(out.contains("(dry run)"), "{out}");
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
    let gates = entries
        .iter()
        .filter(|e| matches!(e, TelemetryEntry::Gate { .. }))
        .count();
    assert_eq!(
        gates, 0,
        "ctx check decides locally and never logs a gated action"
    );
    // --all: a stale head somewhere fails the whole-repository mode.
    let (rc, out) = ctx(&["check", &path, "--all"]);
    assert_eq!(rc, 1, "{out}");
    // The report runs on the same telemetry.
    let (rc, out) = ctx(&["gate-report", "P4", &path]);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("False-deny rate"), "{out}");
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0010 §9: install merges into the project's harness config without
/// touching other hooks, twice is a no-op, uninstall removes only ours, and
/// the data directory is recorded for the hooks.
#[test]
fn hooks_install_is_idempotent_and_uninstall_removes_only_ours() {
    let root = tmp("install");
    let exe = env!("CARGO_BIN_EXE_ctx"); // stands in for ctx-hook
    let settings = root.join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(
        &settings,
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"theirs"}]}]},"model":"m"}"#,
    )
    .unwrap();
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let root_s = root.to_str().unwrap();
    let install = [
        "hooks",
        "install",
        "--harness",
        "claude",
        "--project",
        root_s,
        "--ctx-hook",
        exe,
        "--dir",
        data.to_str().unwrap(),
    ];
    assert_eq!(ctx(&install).0, 0);
    let once = std::fs::read_to_string(&settings).unwrap();
    assert_eq!(ctx(&install).0, 0);
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        once,
        "idempotent"
    );
    let v: serde_json::Value = serde_json::from_str(&once).unwrap();
    assert_eq!(v["model"], "m");
    assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 2);
    assert!(root.join(".contextdb/dir.wire").exists());
    let (rc, out) = ctx(&[
        "hooks",
        "install",
        "--harness",
        "codex",
        "--project",
        root_s,
        "--ctx-hook",
        exe,
    ]);
    assert_eq!(rc, 0);
    assert!(out.contains("trust"), "{out}");
    assert!(root.join(".codex/hooks.json").exists());
    let uninstall = [
        "hooks",
        "uninstall",
        "--harness",
        "claude",
        "--project",
        root_s,
        "--ctx-hook",
        exe,
    ];
    assert_eq!(ctx(&uninstall).0, 0);
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(
        v["hooks"],
        serde_json::json!({"Stop":[{"hooks":[{"type":"command","command":"theirs"}]}]})
    );
    let _ = std::fs::remove_dir_all(&root);
}
