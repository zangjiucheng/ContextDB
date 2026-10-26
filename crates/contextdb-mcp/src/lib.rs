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
    /// key → logical id seen in responses (keys and ids are immutable).
    logicals: std::collections::HashMap<String, LogicalId>,
    harness: String,
    repo: String,
    head_sha: String,
    /// The data directory, for `gating.wire` (`ctx_check`'s gate decision).
    data_dir: Option<std::path::PathBuf>,
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
    // Fail rather than reuse an all-zero client_txn_id (review note).
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut id))
        .expect("/dev/urandom");
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
        // Only meaningful when the node is not valid: a CERTIFIED item
        // carries no reason (dogfood report: agents read it as stale).
        "stale_reason": (!state.l0).then(|| reason(&d.stale_reason)),
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
    let ops = json!({ "type": "array", "items": { "type": "object", "properties": {
        "op": { "enum": ["create", "revise", "set_head", "assert_base", "retract_base", "add_rule", "set_rule_enabled", "expect_heads"] } },
        "required": ["op"] } });
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
        { "name": "ctx_check", "description": "Validate this session's certificate: allow (Valid/ValidRecertified) or warn, with diffs and the reads that changed since last reported (context_update).",
          "inputSchema": { "type": "object", "properties": { "cert_id": { "type": "string" },
            "action": { "type": "object", "properties": { "tool": { "type": "string" }, "paths": { "type": "array", "items": { "type": "string" } }, "command": { "type": "string" } } } } } },
        { "name": "ctx_txn_status", "description": "Committed{epoch} or NotCommitted for a client_txn_id.",
          "inputSchema": { "type": "object", "properties": { "client_txn_id": { "type": "string" } }, "required": ["client_txn_id"] } },
        { "name": "ctx_commit", "description": "Commit several ops atomically on this session's branch. With expect_cert_valid, the session certificate must still be valid at commit time, else nothing commits (first committer wins). Ops: create, revise, set_head, assert_base, retract_base, add_rule, set_rule_enabled, expect_heads; revisions are key@rev or new:N.",
          "inputSchema": { "type": "object", "properties": { "ops": ops.clone(),
              "expect_cert_valid": { "type": "boolean" }, "client_txn_id": { "type": "string" } },
            "required": ["ops"] } },
        { "name": "ctx_what_if", "description": "What the ops would do on this session's branch (status changes, allocations); nothing is committed.",
          "inputSchema": { "type": "object", "properties": { "ops": ops }, "required": ["ops"] } },
        { "name": "ctx_branch", "description": "Branches: create (from main or `from`), list, or archive. Sessions bind to the branch named like their git branch.",
          "inputSchema": { "type": "object", "properties": {
              "action": { "enum": ["create", "list", "archive"] }, "name": { "type": "string" }, "from": { "type": "string" },
              "client_txn_id": { "type": "string" } },
            "required": ["action"] } },
        { "name": "ctx_merge", "description": "Merge branch `source` into `into` (default: this session's branch). require_certified (default true) refuses the merge unless the moved, new and conflicted records (or `certify` keys) are CERTIFIED afterwards.",
          "inputSchema": { "type": "object", "properties": {
              "source": { "type": "string" }, "into": { "type": "string" },
              "require_certified": { "type": "boolean" },
              "certify": { "type": "array", "items": { "type": "string" } },
              "client_txn_id": { "type": "string" } },
            "required": ["source"] } },
    ])
}
/// The session a `tools/call` acts for, from the harness's `_meta` (ADR 0010
/// §2): Codex sends its thread ids, Claude Code the tool-use id its
/// PreToolUse hook claimed. None: no harness binding (use this server's own
/// session, as in P3).
pub fn meta_binding(meta: &Value) -> Option<Binding> {
    let s = |k: &str| {
        meta.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
    };
    if let (Some(session), Some(thread)) = (s("sessionId"), s("threadId")) {
        // A root thread's id is its session id.
        return Some(Binding::Key {
            harness: "codex".into(),
            harness_session: session.into(),
            agent: (thread != session).then(|| thread.into()),
        });
    }
    s("claudecode/toolUseId").map(|id| Binding::Call { id: id.into() })
}
/// `ctx_check`'s `action{tool, paths, command}` (SPEC §6.3).
fn action_of(v: &Value) -> Result<Action, String> {
    let tool = match v.get("tool").and_then(Value::as_str).unwrap_or("") {
        "edit" | "Edit" | "MultiEdit" => ToolClass::Edit,
        "write" | "Write" => ToolClass::Write,
        "notebook" | "NotebookEdit" => ToolClass::Notebook,
        "patch" | "apply_patch" => ToolClass::Patch,
        "shell" | "Bash" | "PowerShell" => ToolClass::Shell,
        "" => return Err("action.tool required".into()),
        other => ToolClass::Other(other.into()),
    };
    let paths = v
        .get("paths")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(Into::into).collect())
        .unwrap_or_default();
    let command = v.get("command").and_then(Value::as_str).map(Into::into);
    Ok(Action {
        tool,
        paths,
        command,
    })
}
fn arg<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str)
}
/// `key@rev` (an existing revision) or `new:N` (the batch's N-th new one).
fn rev_ref(s: &str) -> Result<RevRef, String> {
    if let Some(n) = s.strip_prefix("new:") {
        return n
            .parse()
            .map(RevRef::New)
            .map_err(|_| format!("bad new revision {s}"));
    }
    match parse_target(s)? {
        Target::KeyRev { rev, .. } => Ok(RevRef::Existing(rev)),
        _ => Err(format!("{s}: expected key@rev or new:N")),
    }
}
/// The key a target names (`key` or `key@rev`).
fn key_of(s: &str) -> String {
    s.rsplit_once('@').map_or(s, |(k, _)| k).to_owned()
}
/// `ctx_commit` / `ctx_what_if` ops from JSON (ADR 0013 §10): logicals by
/// key, revisions as `key@rev` or `new:N`, bodies as text (stored as blobs).
fn ops_of(args: &Value) -> Result<(Vec<Op>, Vec<Vec<u8>>), String> {
    let list = args
        .get("ops")
        .and_then(Value::as_array)
        .ok_or("ops must be an array")?;
    let (mut ops, mut blobs) = (Vec::new(), Vec::new());
    for o in list {
        let s = |k: &str| -> Result<&str, String> {
            o.get(k)
                .and_then(Value::as_str)
                .ok_or(format!("{k} required"))
        };
        let b = |k: &str| o.get(k).and_then(Value::as_bool);
        let mut body = |k: &str| -> Result<ContentHash, String> {
            let bytes = s(k)?.as_bytes().to_vec();
            let hash = contextdb_proto::proto::content_hash(&bytes);
            blobs.push(bytes);
            Ok(hash)
        };
        ops.push(match s("op")? {
            "create" => Op::CreateLogical {
                key: s("key")?.into(),
                kind: kind_of(s("kind")?)?,
                scope_lane: o
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("repo")
                    .into(),
                content: body("body")?,
                adopt_as_base: b("adopt_as_base").ok_or("adopt_as_base required (SD1)")?,
            },
            "revise" => Op::AddRevision {
                logical: LogicalRef::Key(key_of(s("target")?)),
                content: body("body")?,
                adopt_as_base: b("adopt_as_base").ok_or("adopt_as_base required (SD1)")?,
            },
            "set_head" => Op::SetHead {
                logical: LogicalRef::Key(key_of(s("target")?)),
                expected: rev_ref(s("expected")?)?,
                new: rev_ref(s("new")?)?,
            },
            "assert_base" => Op::AssertBase {
                rev: rev_ref(s("rev")?)?,
            },
            "retract_base" => Op::RetractBase {
                rev: rev_ref(s("rev")?)?,
            },
            "add_rule" => Op::AddRule {
                conclusion: rev_ref(s("conclusion")?)?,
                incidences: (o.get("premises").and_then(Value::as_array))
                    .ok_or("premises required")?
                    .iter()
                    .map(|p| {
                        let f = |k: &str| {
                            p.get(k)
                                .and_then(Value::as_str)
                                .ok_or(format!("premise {k} required"))
                        };
                        Ok((rev_ref(f("target")?)?, edge_of(f("kind")?)?))
                    })
                    .collect::<Result<_, String>>()?,
                enabled: b("enabled").unwrap_or(true),
            },
            "set_rule_enabled" => Op::SetRuleEnabled {
                rule: RuleId(
                    o.get("rule")
                        .and_then(Value::as_u64)
                        .ok_or("rule required")? as u32,
                )
                .into(),
                enabled: b("enabled").ok_or("enabled required")?,
            },
            "expect_heads" => Op::ExpectHeads {
                entries: (o.get("entries").and_then(Value::as_array))
                    .ok_or("entries required")?
                    .iter()
                    .map(|e| match e.as_str().map(parse_target).transpose()? {
                        Some(Target::KeyRev { key, rev }) => Ok((LogicalRef::Key(key), rev)),
                        _ => Err("entries are key@rev".to_owned()),
                    })
                    .collect::<Result<_, String>>()?,
                source: ReadSource::Client,
            },
            other => return Err(format!("unknown op {other}")),
        });
    }
    Ok((ops, blobs))
}

impl<B: Backend> Server<B> {
    pub fn new(backend: B, harness: &str, repo: &str, head_sha: &str) -> Self {
        Self {
            backend,
            session: None,
            logicals: std::collections::HashMap::new(),
            harness: harness.into(),
            repo: repo.into(),
            head_sha: head_sha.into(),
            data_dir: None,
        }
    }
    /// `ctx_check` decides with the gating configuration in `dir`.
    pub fn with_data_dir(mut self, dir: &std::path::Path) -> Self {
        self.data_dir = Some(dir.to_path_buf());
        self
    }
    fn gate_config(&self) -> contextdb_proto::gate::GateConfig {
        self.data_dir
            .as_ref()
            .and_then(|d| std::fs::read(d.join("gating.wire")).ok())
            .and_then(|b| contextdb_proto::gate::GateConfig::from_bytes(&b).ok())
            .unwrap_or_default()
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
            // The session's branch (ADR 0013 §7, D1): `CONTEXTDB_BRANCH`, else
            // the branch named like the working directory's git branch.
            branch: std::env::var("CONTEXTDB_BRANCH")
                .ok()
                .filter(|b| !b.is_empty()),
            git_branch: contextdb_proto::git::checked_out_branch_above(std::path::Path::new(
                &self.repo,
            )),
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
                let meta = params.get("_meta").cloned().unwrap_or(Value::Null);
                Ok(match self.bound_tool(name, &args, meta_binding(&meta)) {
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
        match &env.response {
            Response::Error { code, detail } => return Err(format!("{code}: {detail}")),
            Response::Read { item, .. } => {
                self.logicals.insert(item.key.clone(), item.logical);
            }
            Response::Recall {
                items, withheld, ..
            } => {
                for i in items.iter().chain(withheld) {
                    self.logicals.insert(i.key.clone(), i.logical);
                }
            }
            _ => {}
        }
        Ok(env)
    }
    /// Runs a tool for the harness binding; a call no hook claimed runs in
    /// this server's own session and says so (ADR 0010 §2.4).
    fn bound_tool(
        &mut self,
        name: &str,
        args: &Value,
        binding: Option<Binding>,
    ) -> Result<Value, String> {
        let (label, result) = match binding {
            Some(b) => {
                let label = match &b {
                    Binding::Call { .. } => "claimed",
                    _ => "harness",
                };
                match self.tool(name, args, Some(b)) {
                    Err(e) if e.starts_with("Unclaimed") => {
                        ("unclaimed", self.tool(name, args, None))
                    }
                    other => (label, other),
                }
            }
            None => ("server", self.tool(name, args, None)),
        };
        result.map(|mut v| {
            if let Some(o) = v.as_object_mut() {
                o.insert("binding".into(), json!(label));
            }
            v
        })
    }
    fn tool(
        &mut self,
        name: &str,
        args: &Value,
        binding: Option<Binding>,
    ) -> Result<Value, String> {
        let binding = match binding {
            Some(b) => b,
            None => Binding::root(self.session()?),
        };
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
                    binding: binding.clone(),
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
                    binding: binding.clone(),
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
                    (false, Some(head)) => self.revise(&binding, head)?,
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
                    binding: binding.clone(),
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
                } = self.revise(&binding, head)?
                else {
                    unreachable!("revise returns Revise")
                };
                let env = self.call(Request::Supersede {
                    binding: binding.clone(),
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
                // On this session's branch (ADR 0013 §7). An unclaimed call
                // falls back to this server's session (`bound_tool`).
                let env = self.call(Request::OnBranch {
                    branch: BranchSel::Session(binding.clone()),
                    request: Box::new(request),
                })?;
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
                let action = args.get("action").map(action_of).transpose()?;
                // The gate decision with the same predicate and configuration
                // as the PreToolUse hook (ADR 0010 §6.5, review M4), computed
                // here so an agent's own check is not logged as a gated action.
                let cfg = self.gate_config();
                let mode = action
                    .as_ref()
                    .map_or(cfg.mode, |a| contextdb_proto::gate::resolve_mode(&cfg, a));
                let env = self.call(Request::Check {
                    binding: binding.clone(),
                    cert_id,
                    live_pos: Vec::new(),
                    action,
                    mode: None,
                    harness_version: String::new(),
                    strict: false,
                })?;
                let Response::Check {
                    validation: v,
                    cert,
                    superseded,
                    named,
                    ..
                } = env.response
                else {
                    return Err("unexpected response".into());
                };
                let (verdict, diffs) = validation(&v);
                // A peek: UserPromptSubmit acknowledges (ADR 0010 A6).
                let update = match self
                    .call(Request::Updates {
                        binding: binding.clone(),
                        ack: None,
                        max_items: 20,
                    })?
                    .response
                {
                    Response::Updates { items, .. } => items,
                    _ => Vec::new(),
                };
                Ok(json!({
                    "epoch": env.epoch.0,
                    "decision": format!("{:?}", contextdb_proto::gate::gate(mode, &v)).to_lowercase(),
                    "mode": format!("{mode:?}").to_lowercase(),
                    "verdict": verdict,
                    "changed": named.iter().map(|n| json!({
                        "key": n.key, "read": n.read.0, "current": n.current.map(|r| r.0),
                        "summary": summary_text(&n.summary), "read_by": n.agent,
                    })).collect::<Vec<_>>(),
                    "diffs": diffs,
                    "superseded": superseded.iter().map(diff).collect::<Vec<_>>(),
                    "cert_id": cert.map(|c| hex(&c.cert_id)),
                    "context_update": update.iter().map(|u| json!({
                        "key": u.key,
                        "read": u.cert_rev.0,
                        "current": u.current.map(|r| r.0),
                        "summary": summary_text(&u.summary),
                    })).collect::<Vec<_>>(),
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
            "ctx_commit" => {
                let (ops, blobs) = ops_of(args)?;
                let env = self.call(Request::Commit {
                    binding: binding.clone(),
                    ops,
                    blobs,
                    expect_cert_valid: args
                        .get("expect_cert_valid")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    client_txn_id: txn(args)?,
                })?;
                match env.response {
                    Response::Refused { validation } => {
                        Ok(json!({ "epoch": env.epoch.0, "refused": format!("{validation:?}") }))
                    }
                    _ => committed(env),
                }
            }
            "ctx_what_if" => {
                let (ops, _) = ops_of(args)?;
                let env = self.call(Request::WhatIf {
                    binding: binding.clone(),
                    ops,
                })?;
                let Response::WhatIf { result, merge } = env.response else {
                    return Err("unexpected response".into());
                };
                Ok(json!({
                    "epoch": result.epoch.0,
                    "revisions": result.allocations.new_revisions.iter().map(|r| r.0).collect::<Vec<_>>(),
                    "status_diff": result.status_diff.iter().map(|r| r.0).collect::<Vec<_>>(),
                    "merge": merge.map(|m| format!("{m:?}")),
                }))
            }
            "ctx_branch" => {
                let name = || arg(args, "name").map(str::to_owned).ok_or("name required");
                let request = match arg(args, "action") {
                    Some("create") => Request::BranchCreate {
                        name: name()?,
                        from: arg(args, "from").map(str::to_owned),
                        client_txn_id: txn(args)?,
                    },
                    Some("archive") => Request::BranchArchive {
                        name: name()?,
                        client_txn_id: txn(args)?,
                    },
                    Some("list") => Request::BranchList,
                    _ => return Err("action must be create, list or archive".into()),
                };
                let env = self.call(request)?;
                match env.response {
                    Response::Branches { branches } => Ok(json!({
                        "epoch": env.epoch.0,
                        "branches": branches.iter().map(|b| json!({
                            "name": b.name, "parent": b.parent.map(|p| p.0),
                            "fork_epoch": b.fork_epoch.0, "archived": b.archived,
                        })).collect::<Vec<_>>(),
                    })),
                    _ => committed(env),
                }
            }
            "ctx_merge" => {
                let env = self.call(Request::Merge {
                    binding: Some(binding.clone()),
                    source: arg(args, "source").ok_or("source required")?.into(),
                    into: arg(args, "into").map(str::to_owned),
                    // D2: required unless explicitly declined.
                    require_certified: args
                        .get("require_certified")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    certify: (args.get("certify").and_then(Value::as_array))
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default(),
                    client_txn_id: txn(args)?,
                })?;
                let Response::Merged { result, report } = env.response else {
                    return Err("unexpected response".into());
                };
                Ok(json!({
                    "epoch": result.epoch.0,
                    "moved_heads": report.moved_heads.len(),
                    "conflicts": report.conflicts.iter().map(|c| json!({
                        "kind": format!("{:?}", c.kind), "target": c.target.0, "artifact": c.artifact.0,
                    })).collect::<Vec<_>>(),
                    "metadata_conflicts": report.metadata_conflicts.len(),
                    "newly_visible": report.newly_visible,
                    "convergent": report.convergent,
                }))
            }
            other => Err(format!("unknown tool {other}")),
        }
    }
    /// `key@rev` of the expected head → the logical id (advisory read) and rev.
    fn revise(&mut self, binding: &Binding, head: &str) -> Result<WriteTarget, String> {
        let Target::KeyRev { key, rev } = parse_target(head)? else {
            return Err("expected_head must be key@rev".into());
        };
        // Keys are immutable: a cached id needs no lookup (the CAS still decides).
        if let Some(l) = self.logicals.get(&key) {
            return Ok(WriteTarget::Revise {
                logical: *l,
                expected_head: rev,
            });
        }
        let env = self.call(Request::Read {
            binding: binding.clone(),
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
