//! SPEC Gate P4 G4 (ADR 0010 §11): outbox stress. `streams` harness
//! sessions × `per_stream` concurrent workers, each repeatedly running a real
//! `ctx-hook post-tool-use` (one outbox append + a 50 ms delivery attempt)
//! against a real `ctxd`, for `seconds`. Every run appends exactly one fact,
//! so after a final flush each stream's applied seq must equal its number of
//! runs: no fact lost, none applied twice, seqs contiguous. Also reports
//! hook latency, showing appends never wait behind another hook's delivery.
//! Usage: hook_stress <ctx-hook path> [seconds] [streams] [per_stream]
use contextdb_daemon::{
    proto::*,
    server::{Client, Daemon, Options},
};
use contextdb_proto::outbox::{self, Outbox, Reply};
use serde_json::json;
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = PathBuf::from(
        args.first()
            .expect("usage: hook_stress <ctx-hook> [s] [streams] [per]"),
    )
    .canonicalize()
    .unwrap();
    let seconds: u64 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(600);
    let streams: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(8);
    let per: usize = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(16);
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("hook-stress-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for a in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.email=s@s",
            "-c",
            "user.name=s",
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
    let d = Daemon::start(&data, &Options::default()).unwrap();
    let stop = AtomicBool::new(false);
    let runs: Vec<AtomicU64> = (0..streams).map(|_| AtomicU64::new(0)).collect();
    let failures = AtomicU64::new(0);
    let lat = std::sync::Mutex::new(Vec::<Duration>::new());
    let start = Instant::now();
    std::thread::scope(|scope| {
        for s in 0..streams {
            for _ in 0..per {
                let (exe, root, stop, runs, failures, lat) =
                    (&exe, &root, &stop, &runs, &failures, &lat);
                scope.spawn(move || {
                    let stdin = json!({ "session_id": format!("stress-{s}"), "cwd": root,
                        "hook_event_name": "PostToolUse", "tool_name": "Edit", "tool_use_id": "t",
                        "tool_input": { "file_path": root.join(format!("f{s}.rs")), "old_string": "a", "new_string": "b" },
                        "tool_response": {} })
                    .to_string();
                    let mut mine = Vec::new();
                    while !stop.load(Ordering::Relaxed) {
                        let t = Instant::now();
                        let mut child = Command::new(exe)
                            .args(["claude", "post-tool-use"])
                            .current_dir(root)
                            .env_remove("CTX_DIR")
                            .stdin(Stdio::piped())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                            .unwrap();
                        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
                        if child.wait().unwrap().success() {
                            runs[s].fetch_add(1, Ordering::Relaxed);
                        } else {
                            failures.fetch_add(1, Ordering::Relaxed);
                        }
                        mine.push(t.elapsed());
                    }
                    lat.lock().unwrap().extend(mine);
                });
            }
        }
        std::thread::sleep(Duration::from_secs(seconds));
        stop.store(true, Ordering::Relaxed);
    });
    let elapsed = start.elapsed();
    // Flush what is left, then compare each stream's mark with its runs.
    let ob = Outbox::new(&data);
    let mut c = Client::connect(d.socket(contextdb_model::Channel::Agent)).unwrap();
    let mut pass = failures.load(Ordering::Relaxed) == 0;
    let mut total = 0;
    for (s, n) in runs.iter().enumerate() {
        let key = format!("stress-{s}");
        let stream = outbox::stream_id("claude", &key);
        let mut call = |packets: &[contextdb_model::Packet], skip| match c
            .call(&Request::Facts {
                harness: "claude".into(),
                harness_session: key.clone(),
                stream,
                packets: packets.to_vec(),
                skip,
            })
            .map_err(|e| format!("{e:?}"))?
            .response
        {
            Response::Facts {
                applied_through, ..
            } => Ok(Reply::Applied(applied_through)),
            Response::Gap { expected } => Ok(Reply::Gap(expected)),
            other => Err(format!("{other:?}")),
        };
        ob.deliver(&stream, Duration::from_secs(120), &mut call)
            .unwrap();
        let Reply::Applied(through) = call(&[], None).unwrap() else {
            panic!("gap on an empty batch")
        };
        let n = n.load(Ordering::Relaxed);
        total += n;
        let ok = through == n && ob.pending(&stream).unwrap().1.is_empty();
        pass &= ok;
        println!(
            "stream {s}: runs {n} applied_through {through} {}",
            if ok { "OK" } else { "MISMATCH" }
        );
    }
    let mut lat = lat.into_inner().unwrap();
    lat.sort_unstable();
    let q = |p: usize| lat[(lat.len() * p).div_ceil(100).saturating_sub(1)].as_secs_f64() * 1e3;
    println!(
        "hook runs {total} in {:.0} s ({:.0}/s), failures {}, latency p50 {:.1} p99 {:.1} max {:.1} ms",
        elapsed.as_secs_f64(),
        total as f64 / elapsed.as_secs_f64(),
        failures.load(Ordering::Relaxed),
        q(50),
        q(99),
        lat.last().unwrap().as_secs_f64() * 1e3
    );
    println!("G4 {}", if pass { "PASS" } else { "FAIL" });
    drop(c);
    drop(d);
    let _ = std::fs::remove_dir_all(&root);
    if !pass {
        std::process::exit(1);
    }
}
