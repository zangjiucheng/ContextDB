//! P9 milestone 1, T99 (ADR 0017 §3.3): every request `ctx-mcp` sends for
//! its tools is on the non-owner allowlist, so a principal other than the
//! store owner can use every tool, and nothing it sends is refused as
//! `ChannelRestricted`.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service, non_owner_allowed},
};
use contextdb_mcp::{Backend, Server};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

const BOB: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(16),
};
struct Recording(Arc<Service<SimDisk>>, Arc<Mutex<Vec<Request>>>);
impl Backend for Recording {
    fn call(&mut self, request: Request) -> Result<Envelope, String> {
        self.1.lock().unwrap().push(request.clone());
        Ok(self.0.handle(BOB, request))
    }
}
fn service() -> Arc<Service<SimDisk>> {
    let disk = SimDisk::new();
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    Arc::new(Service::new(actor::spawn(store).unwrap(), log, events).unwrap())
}
fn tool(server: &mut Server<Recording>, name: &str, args: Value) -> Value {
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args } });
    serde_json::from_str(&server.handle_line(&msg.to_string()).unwrap()).unwrap()
}

#[test]
fn t99_every_mcp_tool_request_is_on_the_non_owner_allowlist() {
    let s = service();
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut server = Server::new(Recording(s.clone(), log.clone()), "claude", "repo", "h");
    let calls = [
        (
            "ctx_record",
            json!({ "kind": "fact", "body": "postgres", "key": "db/engine", "create_only": true, "adopt_as_base": true }),
        ),
        (
            "ctx_record",
            json!({ "kind": "fact", "body": "mongodb", "key": "db/plan", "create_only": true, "adopt_as_base": true }),
        ),
        ("ctx_recall", json!({ "query": "postgres" })),
        ("ctx_read", json!({ "target": "db/engine" })),
        (
            "ctx_supersede",
            json!({ "expected_head": "db/engine@0", "body": "postgres 16", "adopt_as_base": true, "reason": "version" }),
        ),
        ("ctx_why", json!({ "target": "db/engine" })),
        ("ctx_why_not", json!({ "target": "db/engine" })),
        ("ctx_impact", json!({ "target": "db/engine" })),
        ("ctx_check", json!({})),
        (
            "ctx_txn_status",
            json!({ "client_txn_id": "00000000000000000000000000000001" }),
        ),
        (
            "ctx_commit",
            json!({ "ops": [{ "op": "create", "key": "db/x", "kind": "fact", "body": "x", "adopt_as_base": true }] }),
        ),
        (
            "ctx_what_if",
            json!({ "ops": [{ "op": "create", "key": "db/y", "kind": "fact", "body": "y", "adopt_as_base": true }] }),
        ),
        ("ctx_branch", json!({ "action": "create", "name": "feat" })),
        ("ctx_branch", json!({ "action": "list" })),
        ("ctx_merge", json!({ "source": "feat", "into": "main" })),
        ("ctx_branch", json!({ "action": "archive", "name": "feat" })),
        (
            "ctx_conflicts",
            json!({ "open": { "a": "db/engine", "b": "db/plan", "reason": "engines" } }),
        ),
        ("ctx_conflicts", json!({ "list": { "state": "all" } })),
        ("ctx_resolve", json!({ "artifacts": [0], "note": "fixed" })),
        ("ctx_changes", json!({ "since_epoch": 0 })),
        ("ctx_check", json!({})),
        ("ctx_handoff_begin", json!({ "summary": "half done" })),
        ("ctx_handoff_accept", json!({})),
    ];
    let mut tools_seen = std::collections::BTreeSet::new();
    for (name, args) in calls {
        let before = log.lock().unwrap().len();
        let reply = tool(&mut server, name, args);
        let sent: Vec<Request> = log.lock().unwrap()[before..].to_vec();
        assert!(!sent.is_empty(), "{name} sent nothing");
        for r in &sent {
            assert!(
                non_owner_allowed(r),
                "{name} sent a store-owner request: {r:?}"
            );
        }
        let text = reply.to_string();
        assert!(
            !text.contains("ChannelRestricted"),
            "{name} was refused: {text}"
        );
        tools_seen.insert(name);
    }
    // Every tool `ctx-mcp` lists was exercised.
    let listed: Value = serde_json::from_str(
        &server
            .handle_line(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }).to_string())
            .unwrap(),
    )
    .unwrap();
    for t in listed["result"]["tools"].as_array().unwrap() {
        let n = t["name"].as_str().unwrap();
        assert!(tools_seen.contains(n), "tool {n} not exercised");
    }
    // The store owner's requests are refused for this principal.
    for r in [
        Request::Status,
        Request::Shutdown,
        Request::Briefing {
            binding: None,
            budget_chars: 100,
        },
        Request::Telemetry { since: 0, limit: 1 },
        Request::OnBranch {
            branch: BranchSel::Named("main".into()),
            request: Box::new(Request::Status),
        },
    ] {
        assert!(!non_owner_allowed(&r));
        match s.handle(BOB, r).response {
            Response::Error { code, .. } => assert_eq!(code, "ChannelRestricted"),
            other => panic!("{other:?}"),
        }
    }
}
