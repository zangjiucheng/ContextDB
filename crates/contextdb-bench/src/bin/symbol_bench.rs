//! P5b G5 (ADR 0012 §9): P4 hook budgets with symbol anchors (the daemon's
//! live path never parses), and the observer's scan cost for 10/100/1,000
//! symbol anchors and for host files up to the size bound. Does not touch
//! dogfood.
//!
//!   symbol_bench <ctx-hook>
use contextdb_anchors::{
    Repo,
    capture::{LIVE_TIMEOUT, MAX_FILE, OBSERVER_TIMEOUT},
};
use contextdb_daemon::server::{Client, Daemon, Options};
use contextdb_model::*;
use contextdb_proto::proto::*;
use contextdb_symbols::observer::{Added, Mode as ScanMode, reconcile};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const PER_HOST: usize = 10;
const HOST_BYTES: usize = 16 << 10;

fn pct(v: &mut [Duration], p: usize) -> f64 {
    v.sort();
    v[(v.len() * p).div_ceil(100).saturating_sub(1)].as_secs_f64() * 1000.
}
fn git(root: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "user.name=B",
            "-c",
            "user.email=b@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}
/// A Rust host: `PER_HOST` anchored functions, then filler functions up to
/// `bytes`. `version` changes the first function's body.
fn host(bytes: usize, version: u32) -> String {
    let mut s = String::with_capacity(bytes + 64);
    for i in 0..PER_HOST {
        let value = if i == 0 { version } else { i as u32 };
        s += &format!(
            "/// Anchored {i}.\npub fn f{i}(x: u32) -> u32 {{\n    x.wrapping_mul({value}) + {i}\n}}\n\n"
        );
    }
    let mut j = 0;
    while s.len() < bytes {
        s += &format!(
            "fn filler{j}(v: &[u8]) -> usize {{\n    v.iter().filter(|b| **b > {}).count()\n}}\n\n",
            j % 251
        );
        j += 1;
    }
    s
}

struct Rig {
    root: std::path::PathBuf,
    repo: Repo,
    daemon: Daemon,
    observer: Client,
}
fn rig(name: &str, hosts: &[(String, String)], full_sync: bool) -> Rig {
    let root = std::env::temp_dir().join(format!("p5b-bench-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    for (path, text) in hosts {
        std::fs::write(root.join(path), text).unwrap();
    }
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "hosts"]);
    let data = root.join(".contextdb/db");
    std::fs::create_dir_all(&data).unwrap();
    let daemon = Daemon::start(
        &data,
        &Options {
            full_sync,
            ..Options::default()
        },
    )
    .unwrap();
    let repo = Repo::open(&root, Instant::now() + OBSERVER_TIMEOUT).unwrap();
    std::fs::write(
        data.join("anchors.routes"),
        vec![(repo.scope(), repo.root.to_str().unwrap().to_owned())].canonical_bytes(),
    )
    .unwrap();
    let observer = Client::connect(daemon.socket(Channel::Observer)).unwrap();
    Rig {
        root,
        repo,
        daemon,
        observer,
    }
}
impl Rig {
    fn scan(&mut self, added: &Added) -> Result<Duration, String> {
        let t = Instant::now();
        reconcile(
            &mut self.observer,
            &self.repo,
            added,
            ScanMode::Scan,
            Instant::now() + OBSERVER_TIMEOUT,
        )
        .map_err(|e| e.0)?;
        Ok(t.elapsed())
    }
}

fn main() {
    let exe = std::env::args().nth(1).expect("symbol_bench <ctx-hook>");
    let mut pass = true;
    println!(
        "P5b symbol benchmark; os={} arch={} timeout_live_ms={} observer_ms={} per_host={PER_HOST} host_bytes={HOST_BYTES}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        LIVE_TIMEOUT.as_millis(),
        OBSERVER_TIMEOUT.as_millis()
    );
    for full_sync in [false, true] {
        for count in [10, 100, 1000] {
            let hosts: Vec<(String, String)> = (0..count / PER_HOST)
                .map(|h| (format!("host_{h:03}.rs"), host(HOST_BYTES, 1)))
                .collect();
            let mut rig = rig(&format!("{count}-{full_sync}"), &hosts, full_sync);
            let added = Added {
                files: Default::default(),
                symbols: hosts
                    .iter()
                    .flat_map(|(p, _)| (0..PER_HOST).map(move |i| (p.clone(), format!("f{i}"))))
                    .collect(),
            };
            let initial = rig.scan(&added).expect("initial scan");

            let mut client = Client::connect(rig.daemon.socket(Channel::Agent)).unwrap();
            let Response::Attached { session, .. } = client
                .call(&Request::SessionAttach {
                    harness: "claude".into(),
                    harness_session: "bench".into(),
                    repo: rig.repo.root.to_str().unwrap().into(),
                    head_sha: String::new(),
                    branch: None,
                    git_branch: None,
                })
                .unwrap()
                .response
            else {
                panic!("attach")
            };
            let mut reads = 0;
            for (path, symbol) in &added.symbols {
                let r = client
                    .call(&Request::Read {
                        binding: Binding::root(session),
                        target: Target::Key(format!("{}/{path}#{symbol}", rig.repo.scope())),
                        mode: Mode::Certified,
                    })
                    .unwrap()
                    .response;
                if matches!(r, Response::Read { .. }) {
                    reads += 1;
                }
            }

            let none = Added::default();
            let (mut steady, mut edits, mut hooks) = (Vec::new(), Vec::new(), Vec::new());
            let mut hook_warnings = 0;
            let edited = rig.root.join(&hosts[0].0);
            for round in 0..100 {
                if round < 20 {
                    steady.push(rig.scan(&none).expect("steady scan"));
                    // One symbol body changes: a new revision, then back
                    // (identical syntax restores the same revision).
                    std::fs::write(&edited, host(HOST_BYTES, 2 + (round as u32 % 2))).unwrap();
                    edits.push(rig.scan(&none).expect("edit scan"));
                    // As an agent would: re-read the changed symbol before
                    // editing its host (a stale read is rightly Invalid).
                    let key = format!("{}/{}#f0", rig.repo.scope(), hosts[0].0);
                    let r = client
                        .call(&Request::Read {
                            binding: Binding::root(session),
                            target: Target::Key(key),
                            mode: Mode::Certified,
                        })
                        .unwrap()
                        .response;
                    assert!(matches!(r, Response::Read { .. }), "{r:?}");
                }
                let input = serde_json::json!({"session_id":"bench","cwd":rig.root,"hook_event_name":"PreToolUse","tool_name":"Edit","tool_use_id":"bench-edit","tool_input":{"file_path":rig.root.join(&hosts[hosts.len() - 1].0),"old_string":"a","new_string":"b"}});
                let t = Instant::now();
                let mut child = Command::new(Path::new(&exe))
                    .args(["claude", "pre-tool-use"])
                    .current_dir(&rig.root)
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
                // A warning is not a successful certification.
                if !out.stdout.is_empty() {
                    if hook_warnings == 0 {
                        eprintln!(
                            "first hook warning (symbols={count}, round {round}): {}",
                            String::from_utf8_lossy(&out.stdout)
                        );
                    }
                    hook_warnings += 1;
                }
                hooks.push(t.elapsed());
            }
            let (p95, p99) = (pct(&mut hooks, 95), pct(&mut hooks, 99));
            // ADR 0012 §9 G5: P4 hook budgets at every size; 10/100 symbol
            // anchors certify every read without warnings (as P5a).
            pass &= p95 <= 50. && p99 <= 100.;
            if count < 1000 {
                pass &= reads == count && hook_warnings == 0;
            }
            println!(
                "symbols={count} hosts={} full_sync={full_sync} initial_scan_ms={:.3} certified_reads={reads}/{count} steady_scan_p95_ms={:.3} edit_scan_p95_ms={:.3} hook_p95_ms={p95:.3} hook_p99_ms={p99:.3} hook_warnings={hook_warnings}/100",
                hosts.len(),
                initial.as_secs_f64() * 1000.,
                pct(&mut steady, 95),
                pct(&mut edits, 95),
            );
            drop(rig.daemon);
            let _ = std::fs::remove_dir_all(&rig.root);
        }
    }

    // Host size up to the capture bound: one host, ten anchors. Reported
    // only: past what the 5 s deadline allows the scan is unavailable.
    for bytes in [64 << 10, 256 << 10, 1 << 20, 4 << 20, MAX_FILE] {
        let hosts = vec![("big.rs".to_owned(), host(bytes - 256, 1))];
        let mut rig = rig(&format!("size-{bytes}"), &hosts, false);
        let added = Added {
            files: Default::default(),
            symbols: (0..PER_HOST)
                .map(|i| ("big.rs".to_owned(), format!("f{i}")))
                .collect(),
        };
        let mut outcome = match rig.scan(&added) {
            Ok(t) => format!("initial_scan_ms={:.3}", t.as_secs_f64() * 1000.),
            Err(e) => format!("initial_scan=unavailable({e})"),
        };
        let mut steady = Vec::new();
        // Nothing is registered after an unavailable first scan, so later
        // scans would observe nothing: not measured.
        let registered = !outcome.contains("unavailable");
        for _ in 0..if registered { 5 } else { 0 } {
            match rig.scan(&Added::default()) {
                Ok(t) => steady.push(t),
                Err(e) => {
                    outcome += &format!(" steady_scan=unavailable({e})");
                    break;
                }
            }
        }
        if steady.len() == 5 {
            outcome += &format!(" steady_scan_p95_ms={:.3}", pct(&mut steady, 95));
        }
        println!("host_bytes={} {outcome}", bytes - 256);
        drop(rig.daemon);
        let _ = std::fs::remove_dir_all(&rig.root);
    }
    println!("p5b_g5_pass={pass}");
    if !pass {
        std::process::exit(1);
    }
}
