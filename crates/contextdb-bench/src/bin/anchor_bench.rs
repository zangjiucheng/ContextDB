//! P5a capture/reconcile and real PreToolUse budgets. Does not touch dogfood.
use contextdb_anchors::{
    Repo,
    capture::{LIVE_TIMEOUT, OBSERVER_TIMEOUT},
    observer,
};
use contextdb_daemon::server::{Client, Daemon, Options};
use contextdb_model::*;
use contextdb_proto::proto::*;
use std::{
    collections::BTreeSet,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
fn pct(v: &mut [Duration], p: usize) -> f64 {
    v.sort();
    v[(v.len() * p).div_ceil(100).saturating_sub(1)].as_secs_f64() * 1000.
}
fn main() {
    let exe = std::env::args().nth(1).expect("anchor_bench <ctx-hook>");
    let mut pass = true;
    println!(
        "P5a anchor benchmark; os={} arch={} timeout_live_ms={} observer_ms={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        LIVE_TIMEOUT.as_millis(),
        OBSERVER_TIMEOUT.as_millis()
    );
    for full_sync in [false, true] {
        for count in [10, 100, 1000] {
            let root = std::env::temp_dir().join(format!(
                "p5a-bench-{}-{count}-{full_sync}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            assert!(
                Command::new("git")
                    .current_dir(&root)
                    .args(["init", "-q"])
                    .status()
                    .unwrap()
                    .success()
            );
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
            let repo = Repo::open(&root, Instant::now() + OBSERVER_TIMEOUT).unwrap();
            let paths: BTreeSet<String> = (0..count).map(|i| format!("file-{i:04}.rs")).collect();
            for path in &paths {
                std::fs::write(root.join(path), "// raw fixture\r\nfn a() {}\n").unwrap();
            }
            std::fs::write(
                data.join("anchors.routes"),
                vec![(repo.scope(), repo.root.to_str().unwrap().to_owned())].canonical_bytes(),
            )
            .unwrap();
            let mut observer = Client::connect(d.socket(Channel::Observer)).unwrap();
            let t = Instant::now();
            observer::reconcile(
                &mut observer,
                &repo,
                &paths,
                Instant::now() + OBSERVER_TIMEOUT,
            )
            .unwrap();
            let initial = t.elapsed();
            let mut client = Client::connect(d.socket(Channel::Agent)).unwrap();
            let response = client
                .call(&Request::SessionAttach {
                    harness: "claude".into(),
                    harness_session: "bench".into(),
                    repo: repo.root.to_str().unwrap().into(),
                    head_sha: String::new(),
                    branch: None,
                    git_branch: None,
                })
                .unwrap()
                .response;
            let Response::Attached { session, .. } = response else {
                panic!()
            };
            let mut reads = 0;
            for path in &paths {
                let r = client
                    .call(&Request::Read {
                        binding: Binding::root(session),
                        target: Target::Key(format!("{}/{path}", repo.scope())),
                        mode: Mode::Certified,
                    })
                    .unwrap()
                    .response;
                if matches!(r, Response::Read { .. }) {
                    reads += 1;
                }
            }
            let mut captures = Vec::new();
            let mut reconciles = Vec::new();
            let mut hooks = Vec::new();
            let mut failures = 0;
            let mut hook_failures = 0;
            for _ in 0..100 {
                let t = Instant::now();
                if repo.capture(&paths, Instant::now() + LIVE_TIMEOUT).is_err() {
                    failures += 1;
                }
                captures.push(t.elapsed());
                let t = Instant::now();
                observer::reconcile(
                    &mut observer,
                    &repo,
                    &BTreeSet::new(),
                    Instant::now() + OBSERVER_TIMEOUT,
                )
                .unwrap();
                reconciles.push(t.elapsed());
                let input = serde_json::json!({"session_id":"bench","cwd":root,"hook_event_name":"PreToolUse","tool_name":"Edit","tool_use_id":"bench-edit","tool_input":{"file_path":root.join("file-0000.rs"),"old_string":"a","new_string":"b"}});
                let t = Instant::now();
                let mut child = Command::new(Path::new(&exe))
                    .args(["claude", "pre-tool-use"])
                    .current_dir(&root)
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
                    .write_all(input.to_string().as_bytes())
                    .unwrap();
                let out = child.wait_with_output().unwrap();
                assert!(out.status.success());
                // Warnings/empty output must not masquerade as successful certification.
                if !out.stdout.is_empty() {
                    hook_failures += 1;
                }
                hooks.push(t.elapsed());
            }
            let p95 = pct(&mut hooks, 95);
            let p99 = pct(&mut hooks, 99);
            // ADR 0011 §7 G5 (owner decision 2026-10-22): P4 hook budgets at
            // every size; 10/100 anchors must also certify every read without
            // capture failures; at the 1,000 bound misses are reported (they
            // are Unknown, fail closed), not a failure.
            pass &= p95 <= 50. && p99 <= 100.;
            if count < 1000 {
                pass &= failures == 0 && reads == count && hook_failures == 0;
            }
            println!(
                "anchors={count} full_sync={full_sync} initial_reconcile_ms={:.3} certified_reads={reads}/{count} capture_p95_ms={:.3} capture_p99_ms={:.3} capture_failures={failures}/100 steady_reconcile_p95_ms={:.3} hook_p95_ms={p95:.3} hook_p99_ms={p99:.3} hook_warnings={hook_failures}/100",
                initial.as_secs_f64() * 1000.,
                pct(&mut captures, 95),
                pct(&mut captures, 99),
                pct(&mut reconciles, 95)
            );
            drop(d);
            let _ = std::fs::remove_dir_all(root);
        }
    }
    println!("p5a_g5_pass={pass}");
    if !pass {
        std::process::exit(1);
    }
}
