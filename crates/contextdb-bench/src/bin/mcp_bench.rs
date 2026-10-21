//! SPEC Gate P3 G5: MCP round trips through `ctx-mcp`'s server layer and a
//! real `ctxd` over its agent socket, for clients {1, 8, 32, 128} × fullfsync
//! {on, off}. Each client alternates a certified read and a CAS revision of
//! its own logical (no conflicts), so writes measure commit latency and
//! group-commit throughput. Usage: mcp_bench [seconds per cell] [clients...]
use contextdb_daemon::{
    proto::*,
    server::{Client, Daemon, Options},
};
use contextdb_mcp::{Backend, Server};
use serde_json::{Value, json};
use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

struct Socket(Client);
impl Backend for Socket {
    fn call(&mut self, r: Request) -> Result<Envelope, String> {
        self.0.call(&r).map_err(|e| format!("{e:?}"))
    }
}
fn tool(s: &mut Server<Socket>, id: &mut u64, name: &str, args: Value) -> Value {
    *id += 1;
    let msg = json!({ "jsonrpc": "2.0", "id": *id, "method": "tools/call", "params": { "name": name, "arguments": args } });
    let reply: Value = serde_json::from_str(&s.handle_line(&msg.to_string()).unwrap()).unwrap();
    let r = &reply["result"];
    assert!(!r["isError"].as_bool().unwrap(), "{name}: {r}");
    r["structuredContent"].clone()
}
fn pct(v: &mut [u128], p: usize) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_unstable();
    v[(v.len() * p).div_ceil(100).saturating_sub(1)] as f64 / 1e6
}
fn cell(clients: usize, full_sync: bool, seconds: u64) -> (f64, f64, f64, f64, u64, u64) {
    let dir = std::env::temp_dir().join(format!(
        "mcp-bench-{}-{clients}-{full_sync}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let d = Daemon::start(
        &dir,
        &Options {
            full_sync,
            ..Options::default()
        },
    )
    .unwrap();
    let agent = d.socket(contextdb_model::Channel::Agent).to_path_buf();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(clients + 1));
    let workers: Vec<_> = (0..clients)
        .map(|t| {
            let (agent, barrier) = (agent.clone(), barrier.clone());
            std::thread::spawn(move || {
                let mut s =
                    Server::new(Socket(Client::connect(&agent).unwrap()), "bench", "r", "h");
                let mut id = 0;
                let key = format!("w-{t}");
                let w = tool(
                    &mut s,
                    &mut id,
                    "ctx_record",
                    json!({ "kind": "fact", "key": key, "create_only": true,
                    "body": format!("{key} 0"), "adopt_as_base": true }),
                );
                let mut rev = w["revisions"][0].as_u64().unwrap();
                tool(
                    &mut s,
                    &mut id,
                    "ctx_read",
                    json!({ "target": key, "mode": "advisory" }),
                );
                barrier.wait();
                let deadline = Instant::now() + Duration::from_secs(seconds);
                let (mut reads, mut writes) = (Vec::new(), Vec::new());
                let mut n = 0u64;
                while Instant::now() < deadline {
                    n += 1;
                    let t0 = Instant::now();
                    tool(
                        &mut s,
                        &mut id,
                        "ctx_read",
                        json!({ "target": key, "mode": "certified" }),
                    );
                    reads.push(t0.elapsed().as_nanos());
                    let t0 = Instant::now();
                    let w = tool(
                        &mut s,
                        &mut id,
                        "ctx_record",
                        json!({ "kind": "fact", "expected_head": format!("{key}@{rev}"),
                        "body": format!("{key} {n}"), "adopt_as_base": true }),
                    );
                    writes.push(t0.elapsed().as_nanos());
                    rev = w["revisions"][0].as_u64().unwrap();
                }
                (reads, writes)
            })
        })
        .collect();
    barrier.wait();
    let start = Instant::now();
    let (mut reads, mut writes) = (Vec::new(), Vec::new());
    for w in workers {
        let (r, wr) = w.join().unwrap();
        reads.extend(r);
        writes.extend(wr);
    }
    let elapsed = start.elapsed().as_secs_f64();
    let stats = &d.service.actor.stats;
    let (groups, batches, clones) = (
        stats.groups.load(Ordering::Relaxed),
        stats.batches.load(Ordering::Relaxed),
        stats.clone_fallbacks.load(Ordering::Relaxed),
    );
    let throughput = writes.len() as f64 / elapsed;
    let out = (
        pct(&mut reads, 95),
        pct(&mut writes, 95),
        pct(&mut writes, 50),
        throughput,
        groups.max(1),
        clones,
    );
    eprintln!(
        "  batches/group={:.1}",
        batches as f64 / groups.max(1) as f64
    );
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
    out
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let seconds: u64 = args.first().and_then(|v| v.parse().ok()).unwrap_or(8);
    let clients: Vec<usize> = if args.len() > 1 {
        args[1..].iter().filter_map(|v| v.parse().ok()).collect()
    } else {
        vec![1, 8, 32, 128]
    };
    println!(
        "| clients | fullfsync | read p95 ms | write p50 ms | write p95 ms | write tx/s | groups | clone fallbacks |"
    );
    println!("|---:|---|---:|---:|---:|---:|---:|---:|");
    for full_sync in [true, false] {
        for &c in &clients {
            let (r95, w95, w50, tps, groups, clones) = cell(c, full_sync, seconds);
            println!(
                "| {c} | {} | {r95:.2} | {w50:.2} | {w95:.2} | {tps:.0} | {groups} | {clones} |",
                if full_sync { "on" } else { "off" }
            );
        }
    }
}
