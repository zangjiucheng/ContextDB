//! SPEC Gate P3 G6: three simulated agents complete the §1.2 flow through MCP.
//! A adopts D1 = MongoDB and later the user switches it to PostgreSQL; B's
//! schema (Cur on D1) and C's deploy plan (Sem on D1) react differently:
//! B's ctx_check → Invalid with a diff; C's → Pending, then ValidRecertified
//! after a human validation (admin channel).
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_mcp::{Backend, Server};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

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
    Arc::new(Service::new(
        actor::spawn(store).unwrap(),
        disk,
        PathBuf::from(dir),
        log,
        events,
    ))
}
struct Agent {
    server: Server<InProcess>,
    next: u64,
}
impl Agent {
    fn new(s: &Arc<Service<SimDisk>>, name: &str) -> Self {
        let mut server = Server::new(InProcess(s.clone()), name, "repo", "HEAD");
        let init = json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": name } } });
        let reply: Value =
            serde_json::from_str(&server.handle_line(&init.to_string()).unwrap()).unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert!(
            server
                .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
        Self { server, next: 1 }
    }
    /// Calls a tool; returns (structured result, is_error).
    fn tool(&mut self, name: &str, args: Value) -> (Value, bool) {
        self.next += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": self.next, "method": "tools/call",
            "params": { "name": name, "arguments": args } });
        let reply: Value =
            serde_json::from_str(&self.server.handle_line(&msg.to_string()).unwrap()).unwrap();
        let result = &reply["result"];
        let error = result["isError"].as_bool().unwrap();
        let value = if error {
            json!(result["content"][0]["text"])
        } else {
            result["structuredContent"].clone()
        };
        (value, error)
    }
    fn ok(&mut self, name: &str, args: Value) -> Value {
        let (v, error) = self.tool(name, args);
        assert!(!error, "{name}: {v}");
        v
    }
}
#[test]
fn g6_three_agents_stale_plan_invalid_and_semantic_plan_recertified() {
    let s = service();
    let mut a = Agent::new(&s, "architect");
    let mut b = Agent::new(&s, "schema");
    let mut c = Agent::new(&s, "deploy");
    let list: Value = serde_json::from_str(
        &a.server
            .handle_line(r#"{"jsonrpc":"2.0","id":99,"method":"tools/list"}"#)
            .unwrap(),
    )
    .unwrap();
    let names: Vec<_> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert!(
        names.contains(&json!("ctx_check")) && !names.contains(&json!("ctx_validate")),
        "{names:?}"
    );
    // A adopts D1@v1 = MongoDB.
    a.ok(
        "ctx_record",
        json!({ "kind": "decision", "key": "d1-db", "create_only": true,
        "body": "Use MongoDB", "adopt_as_base": true }),
    );
    // B: certified recall → handle → schema D2 with Cur on D1; certified read of D2.
    let recall = b.ok(
        "ctx_recall",
        json!({ "query": "d1-db", "mode": "certified" }),
    );
    let handle = recall["items"][0]["handle"].as_str().unwrap().to_owned();
    assert_eq!(recall["items"][0]["summary"], "CERTIFIED");
    b.ok(
        "ctx_record",
        json!({ "kind": "plan", "key": "d2-schema", "create_only": true,
        "body": "documents collection schema", "adopt_as_base": false,
        "deps": [{ "target": handle, "kind": "cur" }] }),
    );
    b.ok(
        "ctx_read",
        json!({ "target": "d2-schema", "mode": "certified" }),
    );
    assert_eq!(b.ok("ctx_check", json!({}))["verdict"], "Valid");
    // C: deploy plan D3 with a Sem dependency on D1@v1 (explicit key@rev).
    c.ok(
        "ctx_record",
        json!({ "kind": "plan", "key": "d3-deploy", "create_only": true,
        "body": "deploy mongo cluster", "adopt_as_base": false,
        "deps": [{ "target": "d1-db@0", "kind": "sem" }] }),
    );
    let d3 = c.ok(
        "ctx_read",
        json!({ "target": "d3-deploy", "mode": "certified" }),
    );
    assert_eq!(d3["item"]["summary"], "CERTIFIED");
    assert_eq!(c.ok("ctx_check", json!({}))["decision"], "allow");
    // The user switches D1 to PostgreSQL.
    let sup = a.ok(
        "ctx_supersede",
        json!({ "expected_head": "d1-db@0", "body": "Use PostgreSQL",
        "adopt_as_base": true, "carry_rules": "none", "reason": "managed postgres" }),
    );
    let v2 = sup["revisions"][0].as_u64().unwrap();
    // CURRENT: D1 is v2; D2 lost support (STALE), D3 needs semantic review (PENDING).
    assert_eq!(
        a.ok(
            "ctx_read",
            json!({ "target": "d1-db", "mode": "certified" })
        )["body"],
        "Use PostgreSQL"
    );
    let d2 = b.ok(
        "ctx_read",
        json!({ "target": "d2-schema", "mode": "advisory" }),
    );
    assert!(
        d2["item"]["summary"].as_str().unwrap().starts_with("STALE"),
        "{d2}"
    );
    let d3 = c.ok(
        "ctx_read",
        json!({ "target": "d3-deploy", "mode": "advisory" }),
    );
    assert_eq!(d3["item"]["summary"], "PENDING(Semantic)");
    // B's stale plan: ctx_check warns Invalid with a diff naming D1.
    let check = b.ok("ctx_check", json!({}));
    assert_eq!(check["decision"], "warn");
    assert!(
        check["verdict"].as_str().unwrap().starts_with("Invalid"),
        "{check}"
    );
    assert!(
        check["diffs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["current"] == json!(v2)),
        "{check}"
    );
    // IMPACT / WHY_NOT answer which downstream lost support.
    let impact = a.ok("ctx_impact", json!({ "target": "d1-db@0" }));
    assert!(
        !impact["supported"].as_array().unwrap().is_empty(),
        "{impact}"
    );
    assert!(
        b.ok("ctx_why_not", json!({ "target": "d2-schema" }))["explanation"]
            .as_str()
            .unwrap()
            .contains("NotCurrent")
    );
    // C's semantic plan is Pending until a human re-validates the Sem edge.
    assert_eq!(c.ok("ctx_check", json!({}))["verdict"], "Pending");
    let snap = s.actor.snapshot();
    let d3_rev = snap
        .db
        .head(snap.db.logical_by_key("d3-deploy").unwrap())
        .unwrap();
    let (rule, ..) = snap.db.rules_concluding(d3_rev)[0].clone();
    let d1 = snap.db.logical_by_key("d1-db").unwrap();
    let admin = Identity {
        channel: Channel::Admin,
        principal: PrincipalId(0),
    };
    let validated = s.handle(
        admin,
        Request::Validate {
            rule,
            verdict: Verdict::Verified,
            curvec_seen: vec![(d1, RevisionId(v2 as u32))],
            validator: "human".into(),
        },
    );
    assert!(
        matches!(validated.response, Response::Committed { .. }),
        "{validated:?}"
    );
    let check = c.ok("ctx_check", json!({}));
    assert_eq!(check["decision"], "allow");
    assert_eq!(check["verdict"], "ValidRecertified", "{check}");
    assert!(
        check["diffs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["cert_rev"] == json!(0) && d["current"] == json!(v2))
    );
    // Agents cannot validate (no tool, and the request is refused on agent sockets).
    let (msg, error) = b.tool("ctx_validate", json!({}));
    assert!(error && msg.as_str().unwrap().contains("unknown tool"));
}
#[test]
fn json_rpc_errors_and_argument_validation() {
    let s = service();
    let mut a = Agent::new(&s, "a");
    let reply: Value = serde_json::from_str(&a.server.handle_line("{not json").unwrap()).unwrap();
    assert_eq!(reply["error"]["code"], -32700);
    let reply: Value = serde_json::from_str(
        &a.server
            .handle_line(r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(reply["error"]["code"], -32601);
    // Mandatory CAS: neither create_only nor expected_head.
    let (msg, error) = a.tool(
        "ctx_record",
        json!({ "kind": "fact", "body": "x", "adopt_as_base": true }),
    );
    assert!(
        error && msg.as_str().unwrap().contains("InvalidArgument"),
        "{msg}"
    );
    // adopt_as_base is required (SD1).
    let (msg, error) = a.tool(
        "ctx_record",
        json!({ "kind": "fact", "body": "x", "create_only": true, "key": "k" }),
    );
    assert!(
        error && msg.as_str().unwrap().contains("adopt_as_base"),
        "{msg}"
    );
    // A client_txn_id makes the write retriable and queryable.
    let txn = "0123456789abcdef0123456789abcdef";
    let w = a.ok(
        "ctx_record",
        json!({ "kind": "fact", "body": "x", "create_only": true, "key": "k",
        "adopt_as_base": true, "client_txn_id": txn }),
    );
    let again = a.ok(
        "ctx_record",
        json!({ "kind": "fact", "body": "x", "create_only": true, "key": "k",
        "adopt_as_base": true, "client_txn_id": txn }),
    );
    assert_eq!(w, again, "idempotent retry returns the original result");
    assert_eq!(
        a.ok("ctx_txn_status", json!({ "client_txn_id": txn }))["status"],
        "Committed"
    );
}
