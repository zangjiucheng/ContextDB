//! `ctx-ui <data dir> [--port N]`: a local, read-only viewer of a ContextDB
//! context graph in the browser.
//!
//! - The graph comes from replaying the op_log (graph.rs): exact, and safe
//!   next to a running `ctxd`.
//! - Payload bodies come from `ctxd` as advisory reads, when it runs.
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

const INDEX: &str = include_str!("../assets/index.html");
const APP: &str = include_str!("../assets/app.js");
const STYLE: &str = include_str!("../assets/style.css");

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
/// A payload body through `ctxd` (advisory read, its own harness key). Bodies
/// live in blob packs only the daemon indexes; the reason is returned when
/// it cannot answer (not running, another protocol version, missing payload).
fn body(dir: &Path, logical: u32, rev: u32) -> Result<String, String> {
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
    let env = c
        .call(&Request::Read {
            binding: Binding::Key {
                harness: "ctx-ui".into(),
                harness_session: "viewer".into(),
                agent: None,
            },
            target: Target::Pinned {
                logical: LogicalId(logical),
                rev: RevisionId(rev),
            },
            mode: Mode::Advisory,
        })
        .map_err(|e| format!("{e:?}"))?;
    match env.response {
        Response::Read {
            payload: Some(p), ..
        } => Ok(String::from_utf8_lossy(&p).into_owned()),
        Response::Read { payload: None, .. } => Err("payload unavailable".into()),
        Response::Error { code, detail } => Err(format!("{code}: {detail}")),
        other => Err(format!("unexpected {other:?}")),
    }
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
        "/api/graph" => json_response(&mut stream, state.snapshot().map(|s| s.graph())),
        "/api/timeline" => {
            let limit = param(target, "limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(200);
            json_response(&mut stream, state.snapshot().map(|s| s.timeline(limit)))
        }
        "/api/rev" => {
            let rev: Option<u32> = param(target, "id").and_then(|v| v.parse().ok());
            let result = match rev {
                None => Err("id required".into()),
                Some(r) => state.snapshot().and_then(|s| {
                    let mut v = s.explain(RevisionId(r))?;
                    let l = s.db.revision_logical(RevisionId(r)).map(|l| l.0);
                    match l.map(|l| body(&state.dir, l, r)) {
                        Some(Ok(b)) => v["body"] = json!(b),
                        Some(Err(e)) => v["body_error"] = json!(e),
                        None => {}
                    }
                    Ok(v)
                }),
            };
            json_response(&mut stream, result)
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first().map(PathBuf::from) else {
        eprintln!("usage: ctx-ui <data dir> [--port N]");
        std::process::exit(2);
    };
    let port: u16 = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(7787);
    let state = Arc::new(State {
        dir,
        port,
        cache: Mutex::new(None),
    });
    if let Err(e) = state.snapshot() {
        eprintln!("ctx-ui: {e}");
        std::process::exit(2);
    }
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|e| {
        eprintln!("ctx-ui: cannot listen on 127.0.0.1:{port}: {e}");
        std::process::exit(2);
    });
    println!("ctx-ui: http://127.0.0.1:{port}/  (read-only; Ctrl-C to stop)");
    for stream in listener.incoming().flatten() {
        let state = state.clone();
        std::thread::spawn(move || handle(&state, stream));
    }
}
