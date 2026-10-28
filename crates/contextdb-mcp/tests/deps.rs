//! Deps through `ctx_record` (SPEC SD6 / §2.5 A2, r1.6.11): reference-only
//! deps, Summary references, and errors that name the offending dep.
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
fn summary(v: &Value) -> String {
    v["item"]["summary"].as_str().unwrap().to_owned()
}
#[test]
fn reference_only_and_summary_deps() {
    let s = service();
    let mut a = Agent::new(&s, "a");
    a.ok(
        "ctx_record",
        json!({ "kind": "summary", "key": "s", "create_only": true,
        "body": "study results", "adopt_as_base": true }),
    );
    a.ok(
        "ctx_record",
        json!({ "kind": "decision", "key": "d", "create_only": true,
        "body": "target", "adopt_as_base": true }),
    );
    // A Summary is not a cur/pin premise; the error names it and the fix.
    for kind in ["cur", "pin"] {
        let (e, error) = a.tool(
            "ctx_record",
            json!({ "kind": "plan", "key": "p", "create_only": true, "body": "x",
            "adopt_as_base": false, "deps": [{ "target": "s@0", "kind": kind }] }),
        );
        assert!(error);
        let e = e.as_str().unwrap();
        assert!(
            e.contains("ForbiddenPremise")
                && e.contains("s@0 is a Summary")
                && e.contains("\"sem\""),
            "{e}"
        );
    }
    // A bare key is refused with its current head.
    let (e, error) = a.tool(
        "ctx_record",
        json!({ "kind": "plan", "key": "p", "create_only": true, "body": "x",
        "adopt_as_base": false, "deps": [{ "target": "d", "kind": "cur" }] }),
    );
    assert!(error);
    assert!(
        e.as_str().unwrap().contains("its current head is d@1"),
        "{e}"
    );
    // Reference-only deps, Summary included, with adopt_as_base.
    let r = a.ok(
        "ctx_record",
        json!({ "kind": "decision", "key": "refs", "create_only": true, "body": "see s and d",
        "adopt_as_base": true,
        "deps": [{ "target": "s@0", "kind": "ref" }, { "target": "d@1", "kind": "ref" }] }),
    );
    assert!(r.get("warning").is_none(), "{r}");
    let refs = a.ok("ctx_read", json!({ "target": "refs", "mode": "certified" }));
    assert_eq!(summary(&refs), "CERTIFIED");
    let impact = a.ok("ctx_impact", json!({ "target": "s@0" }));
    assert_eq!(impact["reference_only"], json!([2]), "{impact}");
    // Without adopt_as_base nothing supports it, and the result says so.
    let r = a.ok(
        "ctx_record",
        json!({ "kind": "plan", "key": "loose", "create_only": true, "body": "x",
        "adopt_as_base": false, "deps": [{ "target": "d@1", "kind": "ref" }] }),
    );
    assert!(r["warning"].as_str().unwrap().contains("STALE"), "{r}");
    let loose = a.ok("ctx_read", json!({ "target": "loose", "mode": "advisory" }));
    assert_eq!(summary(&loose), "STALE(Unsupported)");
    // References never move status: the referenced Summary losing its base
    // leaves `refs` certified.
    a.ok(
        "ctx_supersede",
        json!({ "expected_head": "d@1", "body": "target v2", "adopt_as_base": true,
        "carry_rules": "none", "reason": "changed" }),
    );
    let refs = a.ok("ctx_read", json!({ "target": "refs", "mode": "certified" }));
    assert_eq!(summary(&refs), "CERTIFIED");
    // carry_rules=copy keeps a reference-only justification.
    let sup = a.ok(
        "ctx_supersede",
        json!({ "expected_head": "refs@2", "body": "see s and d (v2)", "adopt_as_base": true,
        "carry_rules": "copy", "reason": "reworded" }),
    );
    assert_eq!(sup["revisions"].as_array().unwrap().len(), 1, "{sup}");
    // A Summary as sem and ref together with a fact premise.
    a.ok(
        "ctx_record",
        json!({ "kind": "plan", "key": "mixed", "create_only": true, "body": "x",
        "adopt_as_base": false,
        "deps": [{ "target": "s@0", "kind": "sem" }, { "target": "d@4", "kind": "ref" }] }),
    );
}
