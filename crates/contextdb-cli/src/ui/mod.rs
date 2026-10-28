//! `ctx ui <data dir> [--port N]`: a local, read-only viewer of a ContextDB
//! context graph in the browser (formerly the separate `tools/ctx-ui`; built
//! into `ctx` so it ships with every release and always matches `ctxd`).
//!
//! - The graph comes from replaying the op_log (graph.rs): exact, per branch,
//!   and safe next to a running `ctxd`.
//! - Payload bodies and the live status (with the read-time observation
//!   dimension) come from `ctxd` as advisory reads, when it runs; code anchor
//!   payloads are decoded descriptors.
//! - The server binds 127.0.0.1 only, answers GET only, and rejects any
//!   `Host` other than its own (DNS rebinding); it never writes anything.
mod graph;

use contextdb_model::*;
use contextdb_proto::{Client, lease, proto::*};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const INDEX: &str = include_str!("../../assets/ui/index.html");
const APP: &str = include_str!("../../assets/ui/app.js");
const STYLE: &str = include_str!("../../assets/ui/style.css");

struct State {
    dir: PathBuf,
    port: u16,
    cache: Mutex<Option<(graph::Fingerprint, Arc<graph::Snapshot>)>>,
}
impl State {
    /// The current snapshot, replayed again only when the op_log changed.
    fn snapshot(&self) -> Result<Arc<graph::Snapshot>, String> {
        let fp = graph::fingerprint(&self.dir);
        let mut cache = self.cache.lock().expect("cache");
        if let Some((seen, snap)) = cache.as_ref()
            && *seen == fp
        {
            return Ok(snap.clone());
        }
        let snap = Arc::new(graph::load(&self.dir)?);
        *cache = Some((fp, snap.clone()));
        Ok(snap)
    }
}
/// A revision's payload and its live item through `ctxd` (advisory read,
/// its own harness key). Bodies live in blob packs only the daemon indexes;
/// the reason is returned when it cannot answer (not running, another
/// protocol version, missing payload). Off `main`, the read runs in a viewer
/// session bound to that branch, where the revision is visible.
fn live_read(
    dir: &Path,
    branch: Option<&str>,
    logical: u32,
    rev: u32,
) -> Result<(Option<Vec<u8>>, Item), String> {
    let l = lease::read_lease(&dir.join("ctxd.lease")).ok_or("ctxd is not running")?;
    if l.protocol != PROTOCOL_VERSION {
        return Err(format!(
            "ctxd speaks protocol {}, this viewer {}: rebuild one of them",
            l.protocol, PROTOCOL_VERSION
        ));
    }
    let sock = l
        .sockets
        .iter()
        .find(|s| s.ends_with("agent.sock"))
        .ok_or("lease names no agent socket")?;
    let mut c = Client::connect_timeout(sock, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    let mut call = |r: Request| -> Result<Response, String> {
        match c.call(&r).map_err(|e| format!("{e:?}"))?.response {
            Response::Error { code, detail } => Err(format!("{code}: {detail}")),
            other => Ok(other),
        }
    };
    let binding = match branch {
        None => Binding::Key {
            harness: "ctx-ui".into(),
            harness_session: "viewer".into(),
            agent: None,
        },
        Some(name) => match call(Request::SessionAttach {
            harness: "ctx-ui".into(),
            harness_session: format!("viewer@{name}"),
            repo: dir.to_string_lossy().into_owned(),
            head_sha: String::new(),
            branch: Some(name.into()),
            git_branch: None,
        })? {
            Response::Attached { session, .. } => Binding::Session {
                session,
                agent: None,
            },
            other => return Err(format!("unexpected {other:?}")),
        },
    };
    match call(Request::Read {
        binding,
        target: Target::Pinned {
            logical: LogicalId(logical),
            rev: RevisionId(rev),
        },
        mode: Mode::Advisory,
    })? {
        Response::Read { item, payload } => Ok((payload, item)),
        other => Err(format!("unexpected {other:?}")),
    }
}
/// A code anchor's committed descriptor (ADR 0011 §2, ADR 0012 §3).
fn descriptor(bytes: &[u8]) -> Option<Value> {
    Some(
        match contextdb_anchors::AnchorDescriptor::from_bytes(bytes).ok()? {
            contextdb_anchors::AnchorDescriptor::File(d) => json!({
                "type": "file", "path": d.path, "oid": graph::hex(&d.oid),
            }),
            contextdb_anchors::AnchorDescriptor::Symbol(d) => json!({
                "type": "symbol", "path": d.path, "symbol": d.symbol,
                "language": format!("{:?}", d.language), "normaliser": d.normaliser,
                "hash": graph::hex(&d.hash),
            }),
        },
    )
}
fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'self'; style-src 'self'; script-src 'self'; img-src 'self' data:\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}
fn json_response(stream: &mut TcpStream, v: Result<Value, String>) {
    match v {
        Ok(v) => respond(
            stream,
            "200 OK",
            "application/json",
            v.to_string().as_bytes(),
        ),
        Err(e) => respond(
            stream,
            "500 Internal Server Error",
            "application/json",
            json!({ "error": e }).to_string().as_bytes(),
        ),
    }
}
/// `path?query` → (path, value of `key`).
fn param<'a>(target: &'a str, key: &str) -> Option<&'a str> {
    target
        .split_once('?')?
        .1
        .split('&')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}
/// `?branch=N`, `main` when absent.
fn branch_param(target: &str) -> BranchId {
    param(target, "branch")
        .and_then(|v| v.parse().ok())
        .map_or(MAIN, BranchId)
}
fn handle(state: &State, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    let mut host = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) if line == "\r\n" || line == "\n" => break,
            Ok(_) => {
                if let Some((k, v)) = line.split_once(':')
                    && k.eq_ignore_ascii_case("host")
                {
                    host = Some(v.trim().to_owned());
                }
            }
        }
    }
    let mut parts = request.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let allowed = [
        format!("127.0.0.1:{}", state.port),
        format!("localhost:{}", state.port),
    ];
    if !host.as_ref().is_some_and(|h| allowed.contains(h)) {
        respond(
            &mut stream,
            "421 Misdirected Request",
            "text/plain",
            b"bad host",
        );
        return;
    }
    if method != "GET" {
        respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            b"read-only",
        );
        return;
    }
    let path = target.split('?').next().unwrap_or("");
    match path {
        "/" => respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            INDEX.as_bytes(),
        ),
        "/app.js" => respond(
            &mut stream,
            "200 OK",
            "text/javascript; charset=utf-8",
            APP.as_bytes(),
        ),
        "/style.css" => respond(
            &mut stream,
            "200 OK",
            "text/css; charset=utf-8",
            STYLE.as_bytes(),
        ),
        "/favicon.ico" => respond(&mut stream, "204 No Content", "image/x-icon", b""),
        "/api/epoch" => json_response(
            &mut stream,
            Ok(json!({ "fingerprint": format!("{:?}", graph::fingerprint(&state.dir)) })),
        ),
        "/api/graph" => json_response(
            &mut stream,
            state.snapshot().and_then(|s| s.graph(branch_param(target))),
        ),
        "/api/timeline" => {
            let limit = param(target, "limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(200);
            json_response(&mut stream, state.snapshot().map(|s| s.timeline(limit)))
        }
        "/api/rev" => {
            let rev: Option<u32> = param(target, "id").and_then(|v| v.parse().ok());
            let branch = branch_param(target);
            let result = match rev {
                None => Err("id required".into()),
                Some(r) => state.snapshot().and_then(|s| {
                    let mut v = s.explain(branch, RevisionId(r))?;
                    let name = (branch != MAIN)
                        .then(|| s.view(branch).map(|b| b.info().name.clone()))
                        .transpose()?;
                    let logical = v["logical"].as_u64().unwrap_or_default() as u32;
                    match live_read(&state.dir, name.as_deref(), logical, r) {
                        Ok((payload, item)) => {
                            let (label, class) = graph::summary(&item.summary);
                            v["live"] = json!({ "summary": label, "class": class });
                            match payload {
                                Some(p) if item.kind == RecordKind::CodeAnchor => {
                                    v["anchor"] = descriptor(&p).unwrap_or(Value::Null);
                                }
                                Some(p) => v["body"] = json!(String::from_utf8_lossy(&p)),
                                None => v["body_error"] = json!("payload unavailable"),
                            }
                        }
                        Err(e) => v["body_error"] = json!(e),
                    }
                    Ok(v)
                }),
            };
            json_response(&mut stream, result)
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}
pub fn run(args: &[String]) -> Result<u8, String> {
    let Some(dir) = args
        .first()
        .filter(|a| !a.starts_with('-'))
        .map(PathBuf::from)
    else {
        return Err("usage: ctx ui <data dir> [--port N]".into());
    };
    if !dir.join("log").is_dir() {
        let hint = if dir.join("db/log").is_dir() {
            format!(": did you mean {}?", dir.join("db").display())
        } else {
            String::new()
        };
        return Err(format!(
            "{} is not a ContextDB data dir (no log/){hint}",
            dir.display()
        ));
    }
    // An explicit --port is the only port tried; the default moves on to the
    // next free one, so a second viewer still starts.
    let ports = match args.iter().position(|a| a == "--port") {
        Some(i) => {
            let port: u16 = args
                .get(i + 1)
                .and_then(|p| p.parse().ok())
                .ok_or("--port needs a number")?;
            port..=port
        }
        None => 7787..=7796,
    };
    let mut bound = Err(String::new());
    for port in ports.clone() {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => {
                bound = Ok((l, port));
                break;
            }
            Err(e) => bound = Err(format!("{e}")),
        }
    }
    let (listener, port) = bound.map_err(|e| {
        let (lo, hi) = (ports.start(), ports.end());
        if lo == hi {
            format!("cannot listen on 127.0.0.1:{lo}: {e}")
        } else {
            format!("cannot listen on 127.0.0.1:{lo}-{hi}: {e} (try --port N)")
        }
    })?;
    let state = Arc::new(State {
        dir,
        port,
        cache: Mutex::new(None),
    });
    state.snapshot()?;
    println!("ctx ui: http://127.0.0.1:{port}/  (read-only; Ctrl-C to stop)");
    for stream in listener.incoming().flatten() {
        let state = state.clone();
        std::thread::spawn(move || handle(&state, stream));
    }
    Ok(0)
}
