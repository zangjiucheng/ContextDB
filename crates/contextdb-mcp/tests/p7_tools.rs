//! P7 milestone 5: `ctx_conflicts` and `ctx_resolve` through `ctx-mcp`, on
//! the server's own session (ADR 0015 §6).
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_mcp::{Backend, Server};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
struct InProcess(Arc<Service<SimDisk>>);
impl Backend for InProcess {
    fn call(&mut self, request: Request) -> Result<Envelope, String> {
        Ok(self.0.handle(AGENT, request))
    }
}
fn service() -> Arc<Service<SimDisk>> {
    let disk = SimDisk::new();
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    Arc::new(Service::new(actor::spawn(store).unwrap(), log, events).unwrap())
}
fn tool(server: &mut Server<InProcess>, name: &str, args: Value) -> Result<Value, Value> {
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args } });
    let reply: Value =
        serde_json::from_str(&server.handle_line(&msg.to_string()).unwrap()).unwrap();
    if reply["result"]["isError"] == true {
        Err(reply)
    } else {
        Ok(reply["result"]["structuredContent"].clone())
    }
}

#[test]
fn conflicts_open_list_and_resolve_through_mcp() {
    let s = service();
    let mut server = Server::new(InProcess(s.clone()), "claude", "repo", "h");
    for (key, body) in [("db/engine", "postgres"), ("db/plan", "mongodb only")] {
        tool(
            &mut server,
            "ctx_record",
            json!({ "kind": "fact", "body": body, "key": key, "create_only": true, "adopt_as_base": true }),
        )
        .unwrap();
    }
    let opened = tool(
        &mut server,
        "ctx_conflicts",
        json!({ "open": { "a": "db/engine@0", "b": "db/plan@1", "reason": "different engines",
                          "evidence": ["db/engine@0", "db/plan@1"] } }),
    )
    .unwrap();
    let ids: Vec<u64> = opened["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    let listed = tool(
        &mut server,
        "ctx_conflicts",
        json!({ "list": { "state": "open" } }),
    )
    .unwrap();
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["origin"], "agent");
    assert_eq!(items[0]["sides"][0]["handle"], "db/engine@0");
    assert_eq!(items[0]["sides"][1]["current"], true);
    // Both sides gate certified reads.
    assert!(
        tool(
            &mut server,
            "ctx_read",
            json!({ "target": "db/plan", "mode": "certified" })
        )
        .is_err()
    );
    // Refused: the same pair again.
    assert!(
        tool(
            &mut server,
            "ctx_conflicts",
            json!({ "open": { "a": "db/engine@0", "b": "db/plan@1", "reason": "different engines",
                              "evidence": ["db/engine@0", "db/plan@1"] } }),
        )
        .is_err()
    );
    tool(
        &mut server,
        "ctx_resolve",
        json!({ "artifacts": ids, "note": "engine is postgres; plan revised" }),
    )
    .unwrap();
    let listed = tool(&mut server, "ctx_conflicts", json!({})).unwrap();
    assert!(listed["items"].as_array().unwrap().is_empty());
    tool(
        &mut server,
        "ctx_read",
        json!({ "target": "db/plan", "mode": "certified" }),
    )
    .unwrap();
}

/// Milestone 7: `ctx_handoff_begin` on one MCP session and
/// `ctx_handoff_accept` (list, then accept) on another; a second accept is
/// refused.
#[test]
fn handoff_through_mcp() {
    let s = service();
    let mut owner = Server::new(InProcess(s.clone()), "claude", "repo", "h");
    tool(
        &mut owner,
        "ctx_record",
        json!({ "kind": "decision", "body": "use postgres", "key": "db", "create_only": true, "adopt_as_base": true }),
    )
    .unwrap();
    tool(
        &mut owner,
        "ctx_read",
        json!({ "target": "db", "mode": "certified" }),
    )
    .unwrap();
    let begun = tool(
        &mut owner,
        "ctx_handoff_begin",
        json!({ "summary": "db chosen; write the schema" }),
    )
    .unwrap();
    let id = begun["id"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 32);
    let mut next = Server::new(InProcess(s.clone()), "claude", "repo", "h");
    let listed = tool(&mut next, "ctx_handoff_accept", json!({})).unwrap();
    assert_eq!(listed["handoffs"][0]["id"], id.as_str());
    assert_eq!(listed["handoffs"][0]["reads"], 1);
    let accepted = tool(&mut next, "ctx_handoff_accept", json!({ "id": id })).unwrap();
    assert_eq!(accepted["validation"], "Valid");
    assert_eq!(accepted["summary"], "db chosen; write the schema");
    assert_eq!(accepted["reads_copied"], 1);
    let mut third = Server::new(InProcess(s), "claude", "repo", "h");
    let taken = tool(&mut third, "ctx_handoff_accept", json!({ "id": id })).unwrap_err();
    assert!(taken.to_string().contains("HandoffTaken"), "{taken}");
}

/// Milestone 8: `ctx_changes` pages through MCP on the session's branch.
#[test]
fn changes_through_mcp() {
    let s = service();
    let mut server = Server::new(InProcess(s.clone()), "claude", "repo", "h");
    for key in ["a", "b", "c"] {
        tool(
            &mut server,
            "ctx_record",
            json!({ "kind": "fact", "body": key, "key": key, "create_only": true, "adopt_as_base": true }),
        )
        .unwrap();
    }
    let first = tool(
        &mut server,
        "ctx_changes",
        json!({ "since_epoch": 0, "max_items": 2 }),
    )
    .unwrap();
    assert_eq!(first["more"], true);
    let mut refs: Vec<String> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["ref"].as_str().unwrap().to_owned())
        .collect();
    let rest = tool(
        &mut server,
        "ctx_changes",
        json!({ "since_epoch": first["reached"], "wait_ms": 10 }),
    )
    .unwrap();
    assert_eq!(rest["more"], false);
    refs.extend(
        rest["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["ref"].as_str().unwrap().to_owned()),
    );
    assert_eq!(refs, vec!["a@0", "b@1", "c@2"]);
    let ahead = tool(&mut server, "ctx_changes", json!({ "since_epoch": 99 })).unwrap();
    assert_eq!(ahead["truncated"], true);
}
