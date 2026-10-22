//! The details of what `ctx-hook` says and sends (ADR 0010 §3–§6): the real
//! binary against a scripted fake `ctxd` that records every request and
//! answers what the test wants, in a temporary git repository. Reason and
//! notice wording, agent attribution, the Empty rate limit, harness
//! versions, path normalisation, claims, briefings, `context_update` tokens,
//! drift messages, SessionStart baselines and multi-stream delivery.
use contextdb_hooks::outbox::{self, Outbox};
use contextdb_model::*;
use contextdb_proto::{
    gate::{self, GateConfig},
    lease::{self, Lease},
    proto::*,
};
use serde_json::{Value, json};
use std::{
    io::Write,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

/// What the fake daemon answers; the test changes it between hook runs.
struct Script {
    check: Response,
    briefing: Response,
    updates: Response,
    /// Returned with the `Facts` reply of a batch that carries a Drift fact.
    drift: Option<DriftReport>,
}
impl Default for Script {
    fn default() -> Self {
        Self {
            check: check(Validation::Valid, Decision::Allow, Vec::new()),
            briefing: Response::Briefing {
                stale: 0,
                pending: 0,
                conflicted: 0,
                items: Vec::new(),
            },
            updates: Response::Updates {
                token: 1,
                items: Vec::new(),
                more: false,
            },
            drift: None,
        }
    }
}
fn check(validation: Validation, gate: Decision, named: Vec<NamedRead>) -> Response {
    Response::Check {
        decision: gate,
        validation,
        cert: None,
        superseded: Vec::new(),
        gate: Some(gate),
        updates_pending: 0,
        named,
    }
}
fn respond(script: &Script, r: &Request) -> Response {
    match r {
        Request::SessionAttach { .. } => Response::Attached {
            session: [1; 16],
            created: false,
        },
        Request::Check { .. } => script.check.clone(),
        Request::Briefing { .. } => script.briefing.clone(),
        Request::Updates { .. } => script.updates.clone(),
        Request::Facts { packets, skip, .. } => {
            let drifted = packets.iter().any(|p| matches!(p.fact, Fact::Drift { .. }));
            Response::Facts {
                applied_through: packets
                    .last()
                    .map(|p| p.seq)
                    .or(skip.map(|s| s.1))
                    .unwrap_or(0),
                drift: script
                    .drift
                    .clone()
                    .filter(|_| drifted)
                    .into_iter()
                    .collect(),
            }
        }
        _ => Response::Done,
    }
}

/// A fake `ctxd`: an agent socket named by a lease in the data directory.
struct Fake {
    dir: PathBuf,
    seen: Arc<Mutex<Vec<Request>>>,
    script: Arc<Mutex<Script>>,
}
impl Fake {
    fn start(data: &Path) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        // Short: sun_path is 104 bytes on macOS.
        let dir = PathBuf::from(format!(
            "/tmp/ctxfk-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("agent.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        lease::write_lease(
            &data.join("ctxd.lease"),
            &Lease {
                pid: std::process::id(),
                start_time: String::new(),
                protocol: PROTOCOL_VERSION,
                sockets: vec![sock],
            },
        )
        .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(Script::default()));
        let (s, sc) = (seen.clone(), script.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { break };
                let (s, sc) = (s.clone(), sc.clone());
                std::thread::spawn(move || {
                    while let Ok(r) = read_request(&mut conn) {
                        let response = respond(&sc.lock().unwrap(), &r);
                        s.lock().unwrap().push(r);
                        let env = Envelope {
                            epoch: Epoch(7),
                            response,
                        };
                        if write_response(&mut conn, &env).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        Self { dir, seen, script }
    }
    fn set(&self, f: impl FnOnce(&mut Script)) {
        f(&mut self.script.lock().unwrap());
    }
    /// Takes the requests seen so far.
    fn take(&self) -> Vec<Request> {
        std::mem::take(&mut self.seen.lock().unwrap())
    }
}
impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn git_in(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
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
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    out.stdout
}
struct Repo {
    root: PathBuf,
}
impl Repo {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ctx-hook-det-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git_in(&root, &["init", "-q"]);
        std::fs::write(root.join("README"), "x").unwrap();
        std::fs::write(root.join("tracked.txt"), "t1").unwrap();
        git_in(&root, &["add", "README", "tracked.txt"]);
        git_in(&root, &["commit", "-q", "-m", "init"]);
        let repo = Self { root };
        std::fs::create_dir_all(repo.data()).unwrap();
        repo
    }
    fn data(&self) -> PathBuf {
        self.root.join(".contextdb/db")
    }
    fn gating(&self, cfg: GateConfig) {
        std::fs::write(self.data().join("gating.wire"), cfg.canonical_bytes()).unwrap();
    }
    fn hook(&self, harness: &str, event: &str, stdin: &Value) -> Option<Value> {
        self.hook_with(harness, event, stdin, &self.root, &[])
    }
    /// Runs the binary with `dir` as its process cwd and extra env.
    fn hook_with(
        &self,
        harness: &str,
        event: &str,
        stdin: &Value,
        dir: &Path,
        env: &[(&str, &str)],
    ) -> Option<Value> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ctx-hook"));
        cmd.args([harness, event])
            .current_dir(dir)
            .env_remove("CTX_DIR")
            .env_remove("CLAUDE_CODE_EXECPATH");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
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
    fn input(&self, event: &str, session: &str, agent: Option<&str>, extra: Value) -> Value {
        let mut v = json!({ "session_id": session, "cwd": self.root, "hook_event_name": event,
            "transcript_path": null, "permission_mode": "default" });
        if let Some(a) = agent {
            v["agent_id"] = json!(a);
        }
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    }
    fn edit(&self, session: &str, agent: Option<&str>, path: &str) -> Value {
        self.input(
            "PreToolUse",
            session,
            agent,
            json!({ "tool_name": "Edit", "tool_use_id": "toolu_e",
                "tool_input": { "file_path": path, "old_string": "a", "new_string": "b" } }),
        )
    }
    fn edit_lib(&self, session: &str, agent: Option<&str>) -> Value {
        self.edit(
            session,
            agent,
            &self.root.join("src/lib.rs").to_string_lossy(),
        )
    }
    fn stop(&self, session: &str) -> Value {
        self.input("Stop", session, None, json!({ "stop_hook_active": false }))
    }
}
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn system(o: &Value) -> &str {
    o["systemMessage"].as_str().unwrap_or("")
}
fn context(o: &Value) -> &str {
    o["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("")
}
fn deny_reason(o: &Value) -> &str {
    assert_eq!(o["hookSpecificOutput"]["permissionDecision"], "deny", "{o}");
    o["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
}
fn checks(seen: &[Request]) -> Vec<(Option<String>, Action, String)> {
    seen.iter()
        .filter_map(|r| match r {
            Request::Check {
                binding: Binding::Session { agent, .. },
                action: Some(action),
                harness_version,
                ..
            } => Some((agent.clone(), action.clone(), harness_version.clone())),
            _ => None,
        })
        .collect()
}
fn facts(seen: &[Request]) -> Vec<Fact> {
    seen.iter()
        .filter_map(|r| match r {
            Request::Facts { packets, .. } => Some(packets.iter().map(|p| p.fact.clone())),
            _ => None,
        })
        .flatten()
        .collect()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
const REREAD: &str = ". Re-read them (ctx_read/ctx_recall, certified) or supersede before editing.";

/// ADR 0010 §6.1 / review M3: the reason names each changed read, "read rX,
/// now rY" only when the head moved, and whose read it was relative to the
/// agent being checked; warn and deny differ in their notice.
#[test]
fn reasons_name_reads_revisions_and_the_reading_agent() {
    let repo = Repo::new("reasons");
    let fake = Fake::start(&repo.data());
    let named = vec![
        NamedRead {
            key: "db".into(),
            agent: None,
            read: RevisionId(2),
            current: Some(RevisionId(3)),
            summary: Summary::Stale(StaleReason::HeadSuperseded),
        },
        NamedRead {
            key: "api".into(),
            agent: Some("a1".into()),
            read: RevisionId(4),
            current: Some(RevisionId(4)),
            summary: Summary::Conflicted,
        },
        NamedRead {
            key: "q".into(),
            agent: Some("a2".into()),
            read: RevisionId(5),
            current: None,
            summary: Summary::Pending(PendingReason::Semantic),
        },
    ];
    let invalid = Validation::Invalid {
        reason: InvalidReason::Changed,
        diffs: Vec::new(),
    };
    let verdict = gate::verdict_code(&invalid);
    fake.set(|s| s.check = check(invalid.clone(), Decision::Warn, named.clone()));
    // The main agent: subagents' reads are attributed, its own are not.
    let o = repo
        .hook("claude", "pre-tool-use", &repo.edit_lib("s", None))
        .unwrap();
    assert_eq!(
        context(&o),
        format!(
            "ContextDB {verdict}: db: read r2, now r3 STALE(HeadSuperseded); \
             api: r4 CONFLICTED (read by subagent a1); \
             q: r5 PENDING(Semantic) (read by subagent a2){REREAD}"
        )
    );
    assert_eq!(system(&o), format!("ContextDB warning: {verdict}"));
    assert!(o["hookSpecificOutput"].get("permissionDecision").is_none());
    let seen = checks(&fake.take());
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, None);
    assert_eq!(seen[0].1.paths, ["src/lib.rs"]);
    // Subagent a1: the main agent's read and a2's are attributed, its own not.
    let o = repo
        .hook("claude", "pre-tool-use", &repo.edit_lib("s", Some("a1")))
        .unwrap();
    assert_eq!(
        context(&o),
        format!(
            "ContextDB {verdict}: db: read r2, now r3 STALE(HeadSuperseded) (read by the main agent); \
             api: r4 CONFLICTED; \
             q: r5 PENDING(Semantic) (read by subagent a2){REREAD}"
        )
    );
    assert_eq!(checks(&fake.take())[0].0.as_deref(), Some("a1"));
    // Enforce: a deny with the same reason and a deny notice.
    fake.set(|s| s.check = check(invalid.clone(), Decision::Deny, named[..1].to_vec()));
    let o = repo
        .hook("claude", "pre-tool-use", &repo.edit_lib("s", None))
        .unwrap();
    assert_eq!(
        deny_reason(&o),
        format!("ContextDB {verdict}: db: read r2, now r3 STALE(HeadSuperseded){REREAD}")
    );
    assert_eq!(system(&o), format!("ContextDB denied Edit ({verdict})"));
    // A cut session log asks for a new session instead of re-reads.
    let cut = Validation::Invalid {
        reason: InvalidReason::LogMismatch,
        diffs: Vec::new(),
    };
    fake.set(|s| s.check = check(cut.clone(), Decision::Deny, Vec::new()));
    let o = repo
        .hook("claude", "pre-tool-use", &repo.edit_lib("s", None))
        .unwrap();
    assert_eq!(
        deny_reason(&o),
        format!(
            "ContextDB {}. The session log was cut under this session: start a new session.",
            gate::verdict_code(&cut)
        )
    );
}

/// A daemon error reply is unavailability, reported with its code and
/// detail; fail-closed denies with a reason saying so.
#[test]
fn daemon_errors_are_reported_with_code_and_detail() {
    let repo = Repo::new("errors");
    let fake = Fake::start(&repo.data());
    fake.set(|s| {
        s.check = Response::Error {
            code: "E_TEST".into(),
            detail: "boom".into(),
        }
    });
    let o = repo
        .hook("claude", "pre-tool-use", &repo.edit_lib("s", None))
        .unwrap();
    assert_eq!(
        system(&o),
        "ContextDB unavailable (E_TEST: boom): action not checked"
    );
    assert!(o.get("hookSpecificOutput").is_none());
    repo.gating(GateConfig {
        fail_closed: true,
        ..GateConfig::default()
    });
    let o = repo
        .hook("claude", "pre-tool-use", &repo.edit_lib("s", None))
        .unwrap();
    assert_eq!(
        deny_reason(&o),
        "ContextDB unavailable (E_TEST: boom); gating is fail-closed"
    );
}

/// The Empty warning is rendered at most every 600 s per stream and agent;
/// one agent's warning does not silence or reset another's.
#[test]
fn empty_warnings_are_rate_limited_per_agent() {
    let repo = Repo::new("empty");
    let fake = Fake::start(&repo.data());
    fake.set(|s| s.check = check(Validation::Empty, Decision::Warn, Vec::new()));
    let warned = |agent: Option<&str>| {
        repo.hook("claude", "pre-tool-use", &repo.edit_lib("s", agent))
            .map(|o| {
                assert_eq!(system(&o), "ContextDB: no certified reads back this edit");
                assert!(context(&o).contains("(Empty)"), "{o}");
            })
            .is_some()
    };
    assert!(warned(None), "first Empty warns");
    assert!(!warned(None), "then it is rate-limited");
    assert!(warned(Some("a1")), "another agent's first warning");
    assert!(!warned(Some("a1")));
    assert!(!warned(None), "a1's warning kept the main agent's");
    // Exactly 600 s after the last warning it warns again.
    let ob = Outbox::new(&repo.data());
    let stream = outbox::stream_id("claude", "s");
    ob.update_state(&stream, |st| {
        st.empty_warned.retain(|(a, _)| a.is_some());
        st.empty_warned.push((None, now() - 600));
    })
    .unwrap();
    assert!(warned(None), "the limit has passed");
    assert!(!warned(None));
    let st = ob.state(&stream);
    assert_eq!(st.empty_warned.len(), 2, "{:?}", st.empty_warned);
    let main = st.empty_warned.iter().find(|(a, _)| a.is_none()).unwrap().1;
    assert!(main + 5 >= now(), "{main}");
}

/// The harness version goes with each check (ADR 0010 §10).
#[test]
fn harness_versions_go_with_each_check() {
    let repo = Repo::new("version");
    let fake = Fake::start(&repo.data());
    repo.hook_with(
        "claude",
        "pre-tool-use",
        &repo.edit_lib("s", None),
        &repo.root,
        &[("CLAUDE_CODE_EXECPATH", "/opt/claude/versions/2.1.306")],
    );
    assert_eq!(checks(&fake.take())[0].2, "2.1.306");
    std::fs::write(repo.data().join("codex.version"), "0.163.0\n").unwrap();
    let patch = repo.input(
        "PreToolUse",
        "c",
        None,
        json!({ "tool_name": "apply_patch", "tool_use_id": "call_p", "turn_id": "1",
            "tool_input": { "command": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n*** End Patch\n" } }),
    );
    repo.hook("codex", "pre-tool-use", &patch);
    assert_eq!(checks(&fake.take())[0].2, "0.163.0");
}

/// Paths given through a symlink are retried through their canonical
/// parent; relative paths are the harness cwd's, not the process's.
#[test]
fn action_paths_are_normalised_against_the_checkout() {
    let repo = Repo::new("paths");
    let fake = Fake::start(&repo.data());
    std::fs::create_dir_all(repo.root.join("src")).unwrap();
    let link = repo.root.with_file_name(format!(
        "{}-link",
        repo.root.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&repo.root, &link).unwrap();
    let via_link = link.join("src/a.rs").to_string_lossy().into_owned();
    repo.hook("claude", "pre-tool-use", &repo.edit("s", None, &via_link));
    assert_eq!(checks(&fake.take())[0].1.paths, ["src/a.rs"]);
    // The process runs elsewhere, where a `src` also exists.
    let elsewhere = repo.root.with_file_name(format!(
        "{}-else",
        repo.root.file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(elsewhere.join("src")).unwrap();
    repo.hook_with(
        "claude",
        "pre-tool-use",
        &repo.edit("s", None, "src/b.rs"),
        &elsewhere,
        &[],
    );
    assert_eq!(checks(&fake.take())[0].1.paths, ["src/b.rs"]);
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&elsewhere);
}

/// ADR 0010 §2.3: only Claude's calls to ContextDB's own tools are claimed,
/// for the calling agent.
#[test]
fn only_claudes_contextdb_tool_calls_are_claimed() {
    let repo = Repo::new("claim");
    let fake = Fake::start(&repo.data());
    let call = |tool: &str, id: &str| {
        repo.input(
            "PreToolUse",
            "s",
            Some("a1"),
            json!({ "tool_name": tool, "tool_use_id": id, "tool_input": {} }),
        )
    };
    let claims = |seen: Vec<Request>| -> Vec<(Option<String>, String)> {
        seen.into_iter()
            .filter_map(|r| match r {
                Request::Claim { agent, call, .. } => Some((agent, call)),
                _ => None,
            })
            .collect()
    };
    repo.hook(
        "claude",
        "pre-tool-use",
        &call("mcp__contextdb__ctx_read", "toolu_1"),
    );
    assert_eq!(
        claims(fake.take()),
        [(Some("a1".to_owned()), "toolu_1".to_owned())]
    );
    repo.hook(
        "codex",
        "pre-tool-use",
        &call("mcp__contextdb__ctx_read", "call_2"),
    );
    assert_eq!(claims(fake.take()), []);
    repo.hook(
        "claude",
        "pre-tool-use",
        &call("mcp__other__ctx_nope", "toolu_3"),
    );
    assert_eq!(claims(fake.take()), []);
}

/// The briefing lists recently changed items, and SessionStart flags a
/// `gating.toml` that differs from what `gating.wire` was compiled from.
#[test]
fn briefing_items_and_unapplied_gating_file() {
    let repo = Repo::new("brief");
    let fake = Fake::start(&repo.data());
    let item = Item {
        logical: LogicalId(1),
        key: "db".into(),
        kind: RecordKind::Decision,
        rev: RevisionId(3),
        state: NodeState {
            l0: true,
            l1: true,
            l2: true,
            support: 1,
            witness: [Some(Witness::Base), None, None],
            dimensions: Dimensions {
                authorized: true,
                conflicted: false,
                currency: Currency::Current,
                structural: Structural::Supported,
                l1: true,
                l2: true,
                semantic: SemState::NotRequired,
                stale_reason: StaleReason::HeadSuperseded,
                observation: Observation::NotApplicable,
            },
        },
        summary: Summary::Certified,
        handle: None,
    };
    fake.set(|s| {
        s.briefing = Response::Briefing {
            stale: 1,
            pending: 2,
            conflicted: 0,
            items: vec![item],
        }
    });
    let start = repo.input("SessionStart", "s", None, json!({ "source": "startup" }));
    let tail = "Read what you rely on with ctx_recall/ctx_read (certified) before acting; edits are checked against those reads.\n</contextdb_briefing>";
    let o = repo.hook("claude", "session-start", &start).unwrap();
    assert_eq!(
        context(&o),
        format!(
            "<contextdb_briefing epoch=7>\nHeads: 1 STALE, 2 PENDING, 0 CONFLICTED.\nRecently changed:\n- db (Decision) r3 CERTIFIED\n{tail}"
        )
    );
    assert_eq!(system(&o), "", "no gating.toml: nothing to apply");
    fake.set(|s| s.briefing = Script::default().briefing);
    let o = repo.hook("claude", "session-start", &start).unwrap();
    assert_eq!(
        context(&o),
        format!("<contextdb_briefing epoch=7>\nHeads: 0 STALE, 0 PENDING, 0 CONFLICTED.\n{tail}")
    );
    // gating.toml applied: its hash is the compiled source hash.
    let toml = repo.root.join(".contextdb/gating.toml");
    std::fs::write(&toml, "mode = \"warn\"\n").unwrap();
    repo.gating(GateConfig {
        source_hash: *blake3::hash(b"mode = \"warn\"\n").as_bytes(),
        ..GateConfig::default()
    });
    let o = repo.hook("claude", "session-start", &start).unwrap();
    assert_eq!(system(&o), "");
    std::fs::write(&toml, "mode = \"enforce\"\n").unwrap();
    let o = repo.hook("claude", "session-start", &start).unwrap();
    assert_eq!(
        system(&o),
        "ContextDB: .contextdb/gating.toml changed; run `ctx gating apply`"
    );
}

/// `context_update` lines, and each agent's acknowledgement token survives
/// another agent's prompt.
#[test]
fn context_updates_keep_each_agents_token() {
    let repo = Repo::new("updates");
    let fake = Fake::start(&repo.data());
    let item = |current| UpdateItem {
        logical: LogicalId(1),
        key: "db".into(),
        cert_rev: RevisionId(2),
        current,
        summary: Summary::Stale(StaleReason::HeadSuperseded),
    };
    let prompt = |agent| repo.input("UserPromptSubmit", "s", agent, json!({ "prompt": "go" }));
    let acks = |seen: Vec<Request>| -> Vec<(Option<String>, Option<u64>)> {
        seen.into_iter()
            .filter_map(|r| match r {
                Request::Updates {
                    binding: Binding::Session { agent, .. },
                    ack,
                    ..
                } => Some((agent, ack)),
                _ => None,
            })
            .collect()
    };
    fake.set(|s| {
        s.updates = Response::Updates {
            token: 5,
            items: vec![item(Some(RevisionId(3))), item(None)],
            more: true,
        }
    });
    let o = repo
        .hook("claude", "user-prompt-submit", &prompt(None))
        .unwrap();
    assert_eq!(
        context(&o),
        "<context_update epoch=7>\ndb: read r2, now r3 STALE(HeadSuperseded)\ndb: r2 STALE(HeadSuperseded)\n(more changes: run ctx_check)\n</context_update>"
    );
    assert_eq!(acks(fake.take()), [(None, None)]);
    fake.set(|s| {
        s.updates = Response::Updates {
            token: 9,
            items: vec![item(None)],
            more: false,
        }
    });
    repo.hook("claude", "user-prompt-submit", &prompt(Some("a1")))
        .unwrap();
    assert_eq!(acks(fake.take()), [(Some("a1".into()), None)]);
    fake.set(|s| s.updates = Script::default().updates);
    assert_eq!(
        repo.hook("claude", "user-prompt-submit", &prompt(None)),
        None
    );
    assert_eq!(acks(fake.take()), [(None, Some(5))]);
    assert_eq!(
        repo.hook("claude", "user-prompt-submit", &prompt(Some("a1"))),
        None
    );
    assert_eq!(acks(fake.take()), [(Some("a1".into()), Some(9))]);
}

/// The Stop notice lists at most eight paths per kind, then "(+k more)",
/// and names unrealised edits only when there are some.
#[test]
fn drift_notices_list_truncate_and_name_unrealised_edits() {
    let repo = Repo::new("driftmsg");
    let fake = Fake::start(&repo.data());
    let files = |n: usize| (0..n).map(|i| format!("f{i}")).collect::<Vec<_>>();
    let report = |undeclared, unrealised| DriftReport {
        undeclared,
        unrealised,
        ..DriftReport::default()
    };
    fake.set(|s| s.drift = Some(report(files(9), Vec::new())));
    let o = repo.hook("claude", "stop", &repo.stop("s")).unwrap();
    assert_eq!(
        system(&o),
        "ContextDB drift: changed without a recorded edit: f0, f1, f2, f3, f4, f5, f6, f7 (+1 more);"
    );
    fake.set(|s| s.drift = Some(report(files(8), Vec::new())));
    let o = repo.hook("claude", "stop", &repo.stop("s")).unwrap();
    assert_eq!(
        system(&o),
        "ContextDB drift: changed without a recorded edit: f0, f1, f2, f3, f4, f5, f6, f7;"
    );
    fake.set(|s| s.drift = Some(report(Vec::new(), vec!["u.rs".into()])));
    let o = repo.hook("claude", "stop", &repo.stop("s")).unwrap();
    assert_eq!(
        system(&o),
        "ContextDB drift: edited but no longer changed: u.rs;"
    );
    fake.set(|s| s.drift = Some(report(Vec::new(), Vec::new())));
    assert_eq!(repo.hook("claude", "stop", &repo.stop("s")), None);
}

/// The SessionStart baseline is the dirty paths with their git content
/// hashes; Stop reports what changed since HEAD at start (tracked and
/// untracked), minus baseline paths whose content is unchanged.
#[test]
fn baseline_and_drift_facts_follow_git() {
    let repo = Repo::new("baseline");
    let fake = Fake::start(&repo.data());
    std::fs::write(repo.root.join("pre.txt"), "p").unwrap();
    std::fs::write(repo.root.join("tracked.txt"), "t2").unwrap();
    let hash = |p: &str| {
        let out = git_in(&repo.root, &["hash-object", p]);
        *blake3::hash(String::from_utf8(out).unwrap().trim().as_bytes()).as_bytes()
    };
    let start = repo.input("SessionStart", "s", None, json!({ "source": "startup" }));
    repo.hook("claude", "session-start", &start);
    let baseline: Vec<_> = facts(&fake.take())
        .into_iter()
        .filter_map(|f| match f {
            Fact::Baseline { dirty } => Some(dirty),
            _ => None,
        })
        .collect();
    assert_eq!(
        baseline,
        [vec![
            ("pre.txt".to_owned(), hash("pre.txt")),
            ("tracked.txt".to_owned(), hash("tracked.txt")),
        ]]
    );
    // README (tracked, clean at start) and tracked.txt change; pre.txt not.
    std::fs::write(repo.root.join("README"), "y").unwrap();
    std::fs::write(repo.root.join("tracked.txt"), "t3").unwrap();
    std::fs::write(repo.root.join("new.txt"), "n").unwrap();
    repo.hook("claude", "stop", &repo.stop("s"));
    let done: Vec<_> = facts(&fake.take())
        .into_iter()
        .filter_map(|f| match f {
            Fact::Drift { done } => Some(done),
            _ => None,
        })
        .collect();
    assert_eq!(
        done,
        [Some(vec![
            "README".to_owned(),
            "new.txt".to_owned(),
            "tracked.txt".to_owned()
        ])]
    );
}

/// Stop delivers this stream and then every other dirty one, each under its
/// own harness session.
#[test]
fn stop_delivers_other_dirty_streams_under_their_own_sessions() {
    let repo = Repo::new("streams");
    // No daemon: session A's action waits in the outbox.
    std::fs::create_dir_all(repo.root.join("src")).unwrap();
    std::fs::write(repo.root.join("src/lib.rs"), "a").unwrap();
    let post = repo.input(
        "PostToolUse",
        "A",
        None,
        json!({ "tool_name": "Write", "tool_use_id": "w",
            "tool_input": { "file_path": repo.root.join("src/lib.rs"), "content": "a" },
            "tool_response": {} }),
    );
    repo.hook("claude", "post-tool-use", &post);
    let fake = Fake::start(&repo.data());
    repo.hook("claude", "stop", &repo.stop("B"));
    let delivered: Vec<(String, [u8; 16], usize)> = fake
        .take()
        .into_iter()
        .filter_map(|r| match r {
            Request::Facts {
                harness_session,
                stream,
                packets,
                ..
            } => Some((harness_session, stream, packets.len())),
            _ => None,
        })
        .collect();
    let a = outbox::stream_id("claude", "A");
    let b = outbox::stream_id("claude", "B");
    assert_eq!(delivered.first().map(|d| (&*d.0, d.1)), Some(("B", b)));
    assert!(
        delivered.iter().any(|d| d.0 == "A" && d.1 == a && d.2 > 0),
        "{delivered:?}"
    );
    assert!(
        !delivered.iter().any(|d| d.1 == a && d.0 != "A"),
        "{delivered:?}"
    );
    assert!(Outbox::new(&repo.data()).dirty().is_empty());
}
