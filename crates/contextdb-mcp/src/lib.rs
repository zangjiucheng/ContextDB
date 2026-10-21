//! `ctx-mcp`: a thin MCP server over stdio (ADR 0008 §10, owner decision:
//! hand-rolled JSON-RPC 2.0). Each tool maps to one `ctxd` request on the
//! agent socket; results carry per-dimension status and the SD13 summary.
use contextdb_daemon::proto::*;
use contextdb_model::*;
use serde_json::{Value, json};

/// Protocol revisions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Where requests go: the daemon's agent socket, or an in-process service in tests.
pub trait Backend {
    fn call(&mut self, request: Request) -> Result<Envelope, String>;
}
pub struct Server<B: Backend> {
    backend: B,
    session: Option<SessionId>,
    harness: String,
    repo: String,
    head_sha: String,
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != 2 * N {
        return None;
    }
    let mut out = [0u8; N];
    for (n, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * n..2 * n + 2)?, 16).ok()?;
    }
    Some(out)
}
fn random_id() -> [u8; 16] {
    use std::io::Read;
    let mut id = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut id);
    }
    id
}
/// SD13 summary as one word plus reason.
pub fn summary_text(s: &Summary) -> String {
    match s {
        Summary::Unavailable => "UNAVAILABLE".into(),
        Summary::Conflicted => "CONFLICTED".into(),
        Summary::Stale(r) => format!("STALE({})", reason(r)),
        Summary::Pending(r) => format!("PENDING({r:?})"),
        Summary::Certified => "CERTIFIED".into(),
    }
}
fn reason(r: &StaleReason) -> String {
    match r {
        StaleReason::SupersededPinnedPremise(p) => format!("SupersededPinnedPremise(r{})", p.0),
        other => format!("{other:?}"),
    }
}
/// Per-dimension status (SPEC §6.3: "所有工具返回逐维度状态").
fn status(state: &NodeState) -> Value {
    let d = &state.dimensions;
    json!({
        "l0": state.l0, "l1": state.l1, "l2": state.l2,
        "currency": format!("{:?}", d.currency),
        "structural": format!("{:?}", d.structural),
        "semantic": format!("{:?}", d.semantic),
        "conflicted": d.conflicted,
        "authorized": d.authorized,
        "stale_reason": reason(&d.stale_reason),
        "support": state.support,
    })
}
fn item(i: &Item) -> Value {
    json!({
        "handle": i.handle,
        "key": i.key,
        "logical": i.logical.0,
        "rev": i.rev.0,
        "ref": format!("{}@{}", i.key, i.rev.0),
        "kind": format!("{:?}", i.kind),
        "summary": summary_text(&i.summary),
        "status": status(&i.state),
    })
}
fn diff(d: &CertDiff) -> Value {
    json!({
        "logical": d.logical.0,
        "cert_rev": d.cert_rev.map(|r| r.0),
        "current": d.current.map(|r| r.0),
        "summary": d.summary.as_ref().map(summary_text),
    })
}
fn validation(v: &Validation) -> (String, Vec<Value>) {
    match v {
        Validation::Invalid { reason, diffs } => (
            format!("Invalid({reason:?})"),
            diffs.iter().map(diff).collect(),
        ),
        Validation::ValidRecertified { diffs, .. } => {
            ("ValidRecertified".into(), diffs.iter().map(diff).collect())
        }
        Validation::Conflicted { entries } => (
            "Conflicted".into(),
            entries.iter().map(|l| json!({ "logical": l.0 })).collect(),
        ),
        Validation::Ambiguous { logical } => {
            ("Ambiguous".into(), vec![json!({ "logical": logical.0 })])
        }
        Validation::Unknown { lag } => (
            "Unknown".into(),
            lag.iter().map(|s| json!({ "scope": s })).collect(),
        ),
        Validation::Pending { rules } => (
            "Pending".into(),
            rules.iter().map(|j| json!({ "rule": j.0 })).collect(),
        ),
        other => (format!("{other:?}"), Vec::new()),
    }
}
/// `handle`, `key@rev` or `key` (current head).
fn parse_target(s: &str) -> Result<Target, String> {
    if let Some((prefix, index)) = s.split_once('.')
        && prefix.len() == 12
        && prefix.bytes().all(|b| b.is_ascii_hexdigit())
        && index.parse::<usize>().is_ok()
    {
        return Ok(Target::Handle(s.to_owned()));
    }
    match s.rsplit_once('@') {
        Some((key, rev)) => {
            let rev = rev
                .trim_start_matches('r')
                .parse()
                .map_err(|_| format!("bad revision in {s}"))?;
            Ok(Target::KeyRev {
                key: key.to_owned(),
                rev: RevisionId(rev),
            })
        }
        None if !s.is_empty() => Ok(Target::Key(s.to_owned())),
        None => Err("empty target".into()),
    }
}
fn kind_of(s: &str) -> Result<RecordKind, String> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "decision" => RecordKind::Decision,
        "plan" => RecordKind::Plan,
        "fact" => RecordKind::Fact,
        "evidence" => RecordKind::Evidence,
        "summary" => RecordKind::Summary,
        "codeanchor" | "code_anchor" => RecordKind::CodeAnchor,
        other => return Err(format!("unknown kind {other}")),
    })
}
fn edge_of(s: &str) -> Result<EdgeKind, String> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "cur" => EdgeKind::Cur,
        "pin" => EdgeKind::Pin,
        "sem" => EdgeKind::Sem,
        "ref" => EdgeKind::Ref,
        other => return Err(format!("unknown edge kind {other}")),
    })
}
fn tools() -> Value {
    let target = json!({ "type": "string", "description": "handle (from ctx_recall), key@rev, or key (current head)" });
    json!([
        { "name": "ctx_recall", "description": "Find context records by key. certified mode returns only CERTIFIED items (others are withheld) and handles usable as deps.",
          "inputSchema": { "type": "object", "properties": {
              "query": { "type": "string" }, "scope": { "type": "string" },
              "mode": { "enum": ["certified", "advisory"] }, "limit": { "type": "integer" } },
            "required": ["query"] } },
        { "name": "ctx_read", "description": "Read one record. certified mode fails closed unless the current head is CERTIFIED.",
          "inputSchema": { "type": "object", "properties": { "target": target, "mode": { "enum": ["certified", "advisory"] } }, "required": ["target"] } },
        { "name": "ctx_record", "description": "Write a record. Exactly one of create_only (with key) or expected_head (key@rev) is required; deps are handles or key@rev.",
          "inputSchema": { "type": "object", "properties": {
              "kind": { "enum": ["decision", "plan", "fact", "evidence", "summary"] },
              "body": { "type": "string" }, "key": { "type": "string" }, "scope": { "type": "string" },
              "create_only": { "type": "boolean" }, "expected_head": { "type": "string" },
              "deps": { "type": "array", "items": { "type": "object", "properties": {
                  "target": { "type": "string" }, "kind": { "enum": ["cur", "pin", "sem", "ref"] } }, "required": ["target", "kind"] } },
              "adopt_as_base": { "type": "boolean" }, "client_txn_id": { "type": "string" } },
            "required": ["kind", "body", "adopt_as_base"] } },
        { "name": "ctx_supersede", "description": "Replace a record's head. carry_rules=copy copies the old head's own justification only; dependents are never rebound.",
          "inputSchema": { "type": "object", "properties": {
              "expected_head": { "type": "string", "description": "key@rev of the current head" },
              "body": { "type": "string" }, "adopt_as_base": { "type": "boolean" },
              "carry_rules": { "enum": ["copy", "none"] }, "reason": { "type": "string" },
              "client_txn_id": { "type": "string" } },
            "required": ["expected_head", "body", "adopt_as_base", "reason"] } },
        { "name": "ctx_why", "description": "Why a record holds: base, active rules and premises.",
          "inputSchema": { "type": "object", "properties": { "target": target, "depth": { "type": "integer" } }, "required": ["target"] } },
        { "name": "ctx_why_not", "description": "Why a record does not hold: each rule's failing incidences.",
          "inputSchema": { "type": "object", "properties": { "target": target, "depth": { "type": "integer" } }, "required": ["target"] } },
        { "name": "ctx_impact", "description": "Records that depend on the target (reachable impact).",
          "inputSchema": { "type": "object", "properties": { "target": target }, "required": ["target"] } },
        { "name": "ctx_check", "description": "Validate this session's certificate: allow (Valid/ValidRecertified) or warn, with diffs.",
          "inputSchema": { "type": "object", "properties": { "cert_id": { "type": "string" } } } },
        { "name": "ctx_txn_status", "description": "Committed{epoch} or NotCommitted for a client_txn_id.",
          "inputSchema": { "type": "object", "properties": { "client_txn_id": { "type": "string" } }, "required": ["client_txn_id"] } },
    ])
}
fn arg<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str)
}
impl<B: Backend> Server<B> {
    pub fn new(backend: B, harness: &str, repo: &str, head_sha: &str) -> Self {
        Self {
            backend,
            session: None,
            harness: harness.into(),
            repo: repo.into(),
            head_sha: head_sha.into(),
        }
    }
    fn session(&mut self) -> Result<SessionId, String> {
        if let Some(s) = self.session {
            return Ok(s);
        }
        let env = self.backend.call(Request::SessionStart {
            harness: self.harness.clone(),
            repo: self.repo.clone(),
            head_sha: self.head_sha.clone(),
            parent: None,
        })?;
        match env.response {
            Response::Session { session } => {
                self.session = Some(session);
                Ok(session)
            }
            other => Err(format!("{other:?}")),
        }
    }
    /// Handles one JSON-RPC line; `None` for notifications.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(
                    json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": e.to_string() } })
                        .to_string(),
                );
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => {
                let asked = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let version = PROTOCOL_VERSIONS
                    .iter()
                    .find(|v| **v == asked)
                    .copied()
                    .unwrap_or(PROTOCOL_VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "ctx-mcp", "version": env!("CARGO_PKG_VERSION") },
                }))
            }
            "notifications/initialized" | "notifications/cancelled" => return None,
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match self.tool(name, &args) {
                    Ok(value) => json!({
                        "content": [{ "type": "text", "text": value.to_string() }],
                        "structuredContent": value,
                        "isError": false,
                    }),
                    Err(e) => {
                        json!({ "content": [{ "type": "text", "text": e }], "isError": true })
                    }
                })
            }
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        id.as_ref()?;
        Some(
            match result {
                Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            }
            .to_string(),
        )
    }
    fn call(&mut self, request: Request) -> Result<Envelope, String> {
        let env = self.backend.call(request)?;
        if let Response::Error { code, detail } = &env.response {
            return Err(format!("{code}: {detail}"));
        }
        Ok(env)
    }
    fn tool(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        let session = self.session()?;
        let txn = |args: &Value| -> Result<Option<[u8; 16]>, String> {
            match arg(args, "client_txn_id") {
                Some(t) => unhex(t)
                    .map(Some)
                    .ok_or_else(|| "client_txn_id must be 32 hex digits".into()),
                // Always set, so OutcomeUnknown can be resolved (ADR 0008 §8.1).
                None => Ok(Some(random_id())),
            }
        };
        let mode = |args: &Value| match arg(args, "mode") {
            Some("advisory") => Mode::Advisory,
            _ => Mode::Certified,
        };
        match name {
            "ctx_recall" => {
                let env = self.call(Request::Recall {
                    session,
                    query: arg(args, "query").unwrap_or("").into(),
                    scope: arg(args, "scope").unwrap_or("").into(),
                    mode: mode(args),
                    limit: args.get("limit").and_then(Value::as_u64).unwrap_or(10) as u32,
                })?;
                let Response::Recall {
                    items,
                    withheld,
                    cert_id,
                } = env.response
                else {
                    return Err("unexpected response".into());
                };
                Ok(json!({
                    "epoch": env.epoch.0,
                    "items": items.iter().map(item).collect::<Vec<_>>(),
                    "withheld": withheld.iter().map(item).collect::<Vec<_>>(),
                    "cert_id": cert_id.map(|c| hex(&c)),
                }))
            }
            "ctx_read" => {
                let target = parse_target(arg(args, "target").ok_or("target required")?)?;
                let env = self.call(Request::Read {
                    session,
                    target,
                    mode: mode(args),
                })?;
                let Response::Read { item: i, payload } = env.response else {
                    return Err("unexpected response".into());
                };
                Ok(json!({
                    "epoch": env.epoch.0,
                    "item": item(&i),
                    "body": payload.map(|p| String::from_utf8_lossy(&p).into_owned()),
                }))
            }
            "ctx_record" => {
                let kind = kind_of(arg(args, "kind").ok_or("kind required")?)?;
                let body = arg(args, "body")
                    .ok_or("body required")?
                    .as_bytes()
                    .to_vec();
                let adopt_as_base = args
                    .get("adopt_as_base")
                    .and_then(Value::as_bool)
                    .ok_or("adopt_as_base is required (SD1)")?;
                let create = args
                    .get("create_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let target = match (create, arg(args, "expected_head")) {
                    (true, None) => WriteTarget::Create {
                        key: arg(args, "key").ok_or("create_only needs key")?.into(),
                        scope_lane: arg(args, "scope").unwrap_or("repo").into(),
                    },
                    (false, Some(head)) => self.revise(session, head)?,
                    _ => {
                        return Err(
                            "InvalidArgument: give exactly one of create_only or expected_head"
                                .into(),
                        );
                    }
                };
                let deps = args
                    .get("deps")
                    .and_then(Value::as_array)
                    .map(|deps| {
                        deps.iter()
                            .map(|d| {
                                Ok(Dep {
                                    target: parse_target(
                                        arg(d, "target").ok_or("dep target required")?,
                                    )?,
                                    kind: edge_of(arg(d, "kind").ok_or("dep kind required")?)?,
                                })
                            })
                            .collect::<Result<Vec<_>, String>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                let env = self.call(Request::Record {
                    session,
                    kind,
                    body,
                    target,
                    deps,
                    adopt_as_base,
                    client_txn_id: txn(args)?,
                })?;
                committed(env)
            }
            "ctx_supersede" => {
                let head = arg(args, "expected_head").ok_or("expected_head required")?;
                let WriteTarget::Revise {
                    logical,
                    expected_head,
                } = self.revise(session, head)?
                else {
                    unreachable!("revise returns Revise")
                };
                let env = self.call(Request::Supersede {
                    session,
                    logical,
                    body: arg(args, "body")
                        .ok_or("body required")?
                        .as_bytes()
                        .to_vec(),
                    expected_head,
                    adopt_as_base: args
                        .get("adopt_as_base")
                        .and_then(Value::as_bool)
                        .ok_or("adopt_as_base is required (SD1)")?,
                    carry_rules: arg(args, "carry_rules") == Some("copy"),
                    reason: arg(args, "reason").unwrap_or("").into(),
                    client_txn_id: txn(args)?,
                })?;
                committed(env)
            }
            "ctx_why" | "ctx_why_not" | "ctx_impact" => {
                let target = parse_target(arg(args, "target").ok_or("target required")?)?;
                let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(3) as u32;
                let request = match name {
                    "ctx_why" => Request::Why { target, depth },
                    "ctx_why_not" => Request::WhyNot { target, depth },
                    _ => Request::Impact { target },
                };
                let env = self.call(request)?;
                let detail = match env.response {
                    Response::Why { proof } => format!("{proof:?}"),
                    Response::WhyNot { why_not } => format!("{why_not:?}"),
                    Response::Impact { impact } => {
                        return Ok(json!({
                            "epoch": env.epoch.0,
                            "supported": impact.supported.iter().map(|r| r.0).collect::<Vec<_>>(),
                            "reference_only": impact.reference_only.iter().map(|r| r.0).collect::<Vec<_>>(),
                        }));
                    }
                    other => return Err(format!("unexpected response {other:?}")),
                };
                Ok(json!({ "epoch": env.epoch.0, "explanation": detail }))
            }
            "ctx_check" => {
                let cert_id = match arg(args, "cert_id") {
                    Some(c) => Some(unhex(c).ok_or("cert_id must be 64 hex digits")?),
                    None => None,
                };
                let env = self.call(Request::Check {
                    session,
                    cert_id,
                    live_pos: Vec::new(),
                })?;
                let Response::Check {
                    decision,
                    validation: v,
                    cert,
                    superseded,
                } = env.response
                else {
                    return Err("unexpected response".into());
                };
                let (verdict, diffs) = validation(&v);
                Ok(json!({
                    "epoch": env.epoch.0,
                    "decision": format!("{decision:?}").to_lowercase(),
                    "verdict": verdict,
                    "diffs": diffs,
                    "superseded": superseded.iter().map(diff).collect::<Vec<_>>(),
                    "cert_id": cert.map(|c| hex(&c.cert_id)),
                }))
            }
            "ctx_txn_status" => {
                let id = unhex(arg(args, "client_txn_id").ok_or("client_txn_id required")?)
                    .ok_or("client_txn_id must be 32 hex digits")?;
                let env = self.call(Request::TxnStatus { id })?;
                let Response::TxnStatus { committed } = env.response else {
                    return Err("unexpected response".into());
                };
                Ok(match committed {
                    Some((epoch, _)) => json!({ "status": "Committed", "epoch": epoch.0 }),
                    None => json!({ "status": "NotCommitted" }),
                })
            }
            other => Err(format!("unknown tool {other}")),
        }
    }
    /// `key@rev` of the expected head → the logical id (advisory read) and rev.
    fn revise(&mut self, session: SessionId, head: &str) -> Result<WriteTarget, String> {
        let Target::KeyRev { key, rev } = parse_target(head)? else {
            return Err("expected_head must be key@rev".into());
        };
        let env = self.call(Request::Read {
            session,
            target: Target::KeyRev { key, rev },
            mode: Mode::Advisory,
        })?;
        let Response::Read { item, .. } = env.response else {
            return Err("unexpected response".into());
        };
        Ok(WriteTarget::Revise {
            logical: item.logical,
            expected_head: rev,
        })
    }
}
fn committed(env: Envelope) -> Result<Value, String> {
    let Response::Committed { result } = env.response else {
        return Err("unexpected response".into());
    };
    Ok(json!({
        "epoch": result.epoch.0,
        "revisions": result.allocations.new_revisions.iter().map(|r| r.0).collect::<Vec<_>>(),
        "status_diff": result.status_diff.iter().map(|r| r.0).collect::<Vec<_>>(),
    }))
}
