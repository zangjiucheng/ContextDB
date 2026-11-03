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
