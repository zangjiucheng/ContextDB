//! SPEC Gate P4 G5 (ADR 0010 §11): PreToolUse end to end, from spawning a
//! real `ctx-hook` to its exit, against a real `ctxd` in a git repository.
//! Cells: fullfsync {on, off} × concurrent sessions {1, 8} × ledger
//! {10, 100, 1000} reads; every 10th check follows a new read, so the
//! session certificate is re-issued (and the session log synced). Also
//! reported: the Claude MCP claim hook, PostToolUse, UserPromptSubmit and
//! the Codex launch overhead (`$SHELL -lc`). Exits non-zero when a PreToolUse
//! cell misses p95 ≤ 50 ms or p99 ≤ 100 ms (initial budget).
//! Usage: hook_bench <ctx-hook path> [iterations per session]
use contextdb_daemon::{
    proto::*,
    server::{Client, Daemon, Options},
};
use contextdb_model::*;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn pct(v: &mut [Duration], p: usize) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_unstable();
    v[(v.len() * p).div_ceil(100).saturating_sub(1)].as_secs_f64() * 1e3
}
fn run_hook(exe: &Path, root: &Path, harness: &str, event: &str, stdin: &Value) -> Duration {
    let t = Instant::now();
    let mut child = Command::new(exe)
        .args([harness, event])
        .current_dir(root)
        .env_remove("CTX_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ctx-hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    t.elapsed()
}
fn repo(name: &str) -> PathBuf {
    let base = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("hook-bench-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&base)
                .args([
                    "-c",
                    "user.email=b@b",
                    "-c",
                    "user.name=b",
                    "-c",
                    "commit.gpgsign=false"
                ])
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        )
    };
    git(&["init", "-q"]);
    std::fs::write(base.join("README"), "x").unwrap();
    git(&["add", "README"]);
    git(&["commit", "-q", "-m", "init"]);
    base
}
fn call(c: &mut Client, r: Request) -> Response {
    let env = c.call(&r).unwrap();
    if let Response::Error { code, detail } = &env.response {
        panic!("{code}: {detail}");
    }
    env.response
}
fn attach(c: &mut Client, key: &str) -> SessionId {
    match call(
        c,
        Request::SessionAttach {
            harness: "claude".into(),
            harness_session: key.into(),
            repo: String::new(),
            head_sha: String::new(),
            branch: None,
            git_branch: None,
        },
    ) {
        Response::Attached { session, .. } => session,
        other => panic!("{other:?}"),
    }
}
fn read(c: &mut Client, s: SessionId, key: &str) {
    call(
        c,
        Request::Read {
            binding: Binding::root(s),
            target: Target::Key(key.into()),
            mode: Mode::Certified,
        },
    );
}
fn record(c: &mut Client, s: SessionId, key: &str) {
    call(
        c,
        Request::Record {
            binding: Binding::root(s),
            kind: RecordKind::Fact,
            body: key.as_bytes().to_vec(),
            target: WriteTarget::Create {
                key: key.into(),
                scope_lane: "repo".into(),
            },
            deps: Vec::new(),
            adopt_as_base: true,
            client_txn_id: None,
        },
    );
}
fn edit(root: &Path, session: &str) -> Value {
    json!({ "session_id": session, "cwd": root, "hook_event_name": "PreToolUse",
        "transcript_path": null, "tool_name": "Edit", "tool_use_id": "toolu_b",
        "tool_input": { "file_path": root.join("src/lib.rs"), "old_string": "a", "new_string": "b" } })
}
struct Cell {
    full_sync: bool,
    sessions: usize,
    ledger: usize,
    p50: f64,
    p95: f64,
    p99: f64,
    reissue_p95: f64,
    n: usize,
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = PathBuf::from(
        args.first()
            .expect("usage: hook_bench <ctx-hook> [iterations]"),
    )
    .canonicalize()
    .expect("ctx-hook path");
    let iterations: usize = args.get(1).and_then(|n| n.parse().ok()).unwrap_or(200);
    let mut cells = Vec::new();
    let mut extra: Vec<(String, f64, f64)> = Vec::new();
    for full_sync in [true, false] {
        let root = repo(if full_sync { "on" } else { "off" });
        let data = root.join(".contextdb/db");
        std::fs::create_dir_all(&data).unwrap();
        let d = Daemon::start(
            &data,
            &Options {
                full_sync,
                ..Options::default()
            },
        )
        .unwrap();
        let mut c = Client::connect(d.socket(Channel::Agent)).unwrap();
        let writer = attach(&mut c, "writer");
        let max_ledger = 1_000;
        for k in 0..max_ledger + 64 * iterations / 10 + 64 {
            record(&mut c, writer, &format!("k{k}"));
        }
        let mut next_fresh = max_ledger;
        for ledger in [10usize, 100, 1_000] {
            for sessions in [1usize, 8] {
                let keys: Vec<String> = (0..sessions)
                    .map(|s| format!("bench-{full_sync}-{ledger}-{sessions}-{s}"))
                    .collect();
                // Each session reads `ledger` records, then checks once so
                // the session certificate exists.
                let mut fresh: Vec<Vec<String>> = Vec::new();
                for key in &keys {
                    let s = attach(&mut c, key);
                    for k in 0..ledger {
                        read(&mut c, s, &format!("k{k}"));
                    }
                    run_hook(&exe, &root, "claude", "pre-tool-use", &edit(&root, key));
                    let mine: Vec<String> = (0..iterations / 10 + 1)
                        .map(|_| {
                            next_fresh += 1;
                            format!("k{next_fresh}")
                        })
                        .collect();
                    fresh.push(mine);
                }
                let socket = d.socket(Channel::Agent).to_path_buf();
                let results: Vec<(Vec<Duration>, Vec<Duration>)> = std::thread::scope(|scope| {
                    let handles: Vec<_> = keys
                        .iter()
                        .zip(fresh)
                        .map(|(key, fresh)| {
                            let (exe, root, socket) = (&exe, &root, &socket);
                            scope.spawn(move || {
                                let mut c = Client::connect(socket).unwrap();
                                let s = attach(&mut c, key);
                                let (mut steady, mut reissue) = (Vec::new(), Vec::new());
                                for i in 0..iterations {
                                    let stdin = edit(root, key);
                                    if i % 10 == 9 {
                                        // A new read: the check re-issues.
                                        read(&mut c, s, &fresh[i / 10]);
                                        reissue.push(run_hook(
                                            exe,
                                            root,
                                            "claude",
                                            "pre-tool-use",
                                            &stdin,
                                        ));
                                    } else {
                                        steady.push(run_hook(
                                            exe,
                                            root,
                                            "claude",
                                            "pre-tool-use",
                                            &stdin,
                                        ));
                                    }
                                }
                                (steady, reissue)
                            })
                        })
                        .collect();
                    handles.into_iter().map(|h| h.join().unwrap()).collect()
                });
                let mut all: Vec<Duration> = results
                    .iter()
                    .flat_map(|(a, b)| a.iter().chain(b))
                    .copied()
                    .collect();
                let mut re: Vec<Duration> = results
                    .iter()
                    .flat_map(|(_, b)| b.iter())
                    .copied()
                    .collect();
                cells.push(Cell {
                    full_sync,
                    sessions,
                    ledger,
                    n: all.len(),
                    p50: pct(&mut all, 50),
                    p95: pct(&mut all, 95),
                    p99: pct(&mut all, 99),
                    reissue_p95: pct(&mut re, 95),
                });
            }
        }
        if full_sync {
            // Other hooks, one session, steady state.
            let key = "others";
            let s = attach(&mut c, key);
            read(&mut c, s, "k0");
            let samples = |event: &str, stdin: Value| {
                let mut v: Vec<Duration> = (0..iterations)
                    .map(|_| run_hook(&exe, &root, "claude", event, &stdin))
                    .collect();
                (pct(&mut v, 50), pct(&mut v, 95))
            };
            let claim = |n: usize| {
                json!({ "session_id": key, "cwd": root, "hook_event_name": "PreToolUse",
                    "tool_name": "mcp__contextdb__ctx_read", "tool_use_id": format!("toolu_{n}"),
                    "tool_input": { "target": "k0" } })
            };
            let (a, b) = samples("pre-tool-use", claim(0));
            extra.push(("PreToolUse claim (Claude MCP call)".into(), a, b));
            let post = json!({ "session_id": key, "cwd": root, "hook_event_name": "PostToolUse",
                "tool_name": "Edit", "tool_use_id": "toolu_p", "tool_response": {},
                "tool_input": { "file_path": root.join("src/lib.rs"), "old_string": "a", "new_string": "b" } });
            let (a, b) = samples("post-tool-use", post);
            extra.push(("PostToolUse (outbox append + delivery)".into(), a, b));
            let prompt = json!({ "session_id": key, "cwd": root, "hook_event_name": "UserPromptSubmit", "prompt": "go" });
            let (a, b) = samples("user-prompt-submit", prompt);
            extra.push(("UserPromptSubmit (context_update)".into(), a, b));
        }
        drop(c);
        drop(d);
        let _ = std::fs::remove_dir_all(&root);
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut v: Vec<Duration> = (0..20)
        .map(|_| {
            let t = Instant::now();
            Command::new(&shell).args(["-lc", "true"]).output().unwrap();
            t.elapsed()
        })
        .collect();
    let shell_p50 = pct(&mut v, 50);
    let shell_p95 = pct(&mut v, 95);
    println!("# P4 G5 hook latency (ctx-hook spawn → exit, ms)");
    println!("fullfsync sessions ledger n p50 p95 p99 reissue_p95 budget");
    let mut pass = true;
    for c in &cells {
        let ok = c.p95 <= 50.0 && c.p99 <= 100.0;
        pass &= ok;
        println!(
            "{} {} {} {} {:.2} {:.2} {:.2} {:.2} {}",
            if c.full_sync { "on" } else { "off" },
            c.sessions,
            c.ledger,
            c.n,
            c.p50,
            c.p95,
            c.p99,
            c.reissue_p95,
            if ok { "PASS" } else { "MISS" }
        );
    }
    println!("# other hooks (fullfsync on, 1 session): name p50 p95");
    for (name, a, b) in &extra {
        println!("{name}: {a:.2} {b:.2}");
    }
    println!(
        "# Codex launch overhead: {shell} -lc true: p50 {shell_p50:.2} p95 {shell_p95:.2} (harness cost, not gated)"
    );
    println!("g5_pass={pass}");
    if !pass {
        std::process::exit(1);
    }
}
