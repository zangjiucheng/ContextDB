//! ADR 0010 §2: `ctx-mcp` binds each tool call to the harness session (and
//! agent) the call belongs to, from the `tools/call` `_meta`: Codex thread
//! ids directly, Claude Code tool-use ids through the PreToolUse claim. One
//! server process serving several sessions keeps their reads apart.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_mcp::{Backend, Server, meta_binding};
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
fn tool(server: &mut Server<InProcess>, name: &str, args: Value, meta: Value) -> Value {
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args, "_meta": meta } });
    let reply: Value =
        serde_json::from_str(&server.handle_line(&msg.to_string()).unwrap()).unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    reply["result"]["structuredContent"].clone()
}
fn attach(s: &Service<SimDisk>, harness: &str, key: &str) -> SessionId {
    match s
        .handle(
            AGENT,
            Request::SessionAttach {
                harness: harness.into(),
                harness_session: key.into(),
                repo: "repo".into(),
                head_sha: "h".into(),
            },
        )
        .response
    {
        Response::Attached { session, .. } => session,
        other => panic!("{other:?}"),
    }
}
fn verdict(s: &Service<SimDisk>, session: SessionId) -> Validation {
    match s
        .handle(
            AGENT,
            Request::Check {
                binding: Binding::root(session),
                cert_id: None,
                live_pos: Vec::new(),
                action: None,
                mode: None,
                harness_version: String::new(),
            },
        )
        .response
    {
        Response::Check { validation, .. } => validation,
        other => panic!("{other:?}"),
    }
}

#[test]
fn meta_keys_of_both_harnesses_map_to_bindings() {
    assert_eq!(
        meta_binding(&json!({ "sessionId": "t1", "threadId": "t1", "callId": "c" })),
        Some(Binding::Key {
            harness: "codex".into(),
            harness_session: "t1".into(),
            agent: None
        })
    );
    assert_eq!(
        meta_binding(&json!({ "sessionId": "t1", "threadId": "t2" })),
        Some(Binding::Key {
            harness: "codex".into(),
            harness_session: "t1".into(),
            agent: Some("t2".into())
        })
    );
    assert_eq!(
        meta_binding(&json!({ "claudecode/toolUseId": "toolu_1" })),
        Some(Binding::Call {
            id: "toolu_1".into()
        })
    );
    assert_eq!(meta_binding(&json!({})), None);
    assert_eq!(meta_binding(&Value::Null), None);
}

#[test]
fn one_server_keeps_claimed_sessions_apart_and_falls_back_when_unclaimed() {
    let s = service();
    let a = attach(&s, "claude", "A");
    let b = attach(&s, "claude", "B");
    let mut server = Server::new(InProcess(s.clone()), "claude", "repo", "h");
    let rec = tool(
        &mut server,
        "ctx_record",
        json!({ "kind": "decision", "key": "db", "body": "pg", "create_only": true, "adopt_as_base": true }),
        json!({}),
    );
    assert_eq!(rec["binding"], "server");
    // B's PreToolUse hook claims its call; the read lands in B only.
    let claim = Request::Claim {
        session: b,
        agent: None,
        call: "toolu_b1".into(),
    };
    assert_eq!(s.handle(AGENT, claim).response, Response::Done);
    let read = tool(
        &mut server,
        "ctx_read",
        json!({ "target": "db" }),
        json!({ "claudecode/toolUseId": "toolu_b1" }),
    );
    assert_eq!(read["binding"], "claimed");
    assert_eq!(verdict(&s, a), Validation::Empty);
    assert!(matches!(
        verdict(&s, b),
        Validation::Valid | Validation::ValidRecertified { .. }
    ));
    // An unclaimed call never lands in another session's ledger.
    let unclaimed = tool(
        &mut server,
        "ctx_read",
        json!({ "target": "db" }),
        json!({ "claudecode/toolUseId": "toolu_nobody" }),
    );
    assert_eq!(unclaimed["binding"], "unclaimed");
    assert_eq!(verdict(&s, a), Validation::Empty);
}

#[test]
fn codex_thread_ids_bind_directly_and_ctx_check_peeks_updates() {
    let s = service();
    let mut server = Server::new(InProcess(s.clone()), "codex", "repo", "h");
    let meta = json!({ "sessionId": "th-1", "threadId": "th-1", "callId": "call_1" });
    tool(
        &mut server,
        "ctx_record",
        json!({ "kind": "decision", "key": "db", "body": "pg", "create_only": true, "adopt_as_base": true }),
        meta.clone(),
    );
    let read = tool(
        &mut server,
        "ctx_read",
        json!({ "target": "db" }),
        meta.clone(),
    );
    assert_eq!(read["binding"], "harness");
    let session = attach(&s, "codex", "th-1");
    assert!(matches!(
        verdict(&s, session),
        Validation::Valid | Validation::ValidRecertified { .. }
    ));
    // Someone else supersedes db: ctx_check reports it without consuming it.
    let head = format!("db@{}", read["item"]["rev"]);
    let other = Server::new(InProcess(s.clone()), "claude", "repo", "h");
    let mut other = other;
    tool(
        &mut other,
        "ctx_supersede",
        json!({ "expected_head": head, "body": "mysql", "adopt_as_base": true }),
        json!({}),
    );
    for _ in 0..2 {
        let check = tool(
            &mut server,
            "ctx_check",
            json!({ "action": { "tool": "Edit", "paths": ["a.rs"] } }),
            meta.clone(),
        );
        assert_eq!(check["verdict"], "Invalid(Changed)");
        assert_eq!(check["context_update"].as_array().unwrap().len(), 1);
    }
}
