//! `ctx-mcp [--data] <data dir>` (or `CTXDB_DATA`): MCP over stdio, connected
//! to the daemon's agent socket. The store owner finds it through
//! `<dir>/ctxd.lease`; another principal (the data directory is not its own)
//! through `<dir>/run/shared/endpoint`, after checking that the server is the
//! store owner (ADR 0017 §3.2, `ServerNotOwner`). One JSON-RPC message per
//! line.
use contextdb_daemon::{lease, proto::*, server::Client};
use contextdb_mcp::{Backend, Server};
use std::{
    io::{BufRead, Write},
    path::PathBuf,
    process::ExitCode,
};

struct Socket(Client);
impl Backend for Socket {
    fn call(&mut self, request: Request) -> Result<Envelope, String> {
        self.0
            .call(&request)
            .map_err(|e| format!("Unavailable: {e:?}"))
    }
}
/// The store owner connects through its lease; any other user through the
/// public endpoint, which first authenticates the server.
fn connect(dir: &std::path::Path) -> Result<Client, String> {
    use std::os::unix::fs::MetadataExt;
    let owner = std::fs::metadata(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .uid();
    if owner != contextdb_proto::fsguard::current_uid() {
        return Client::connect_shared(dir, None);
    }
    let l = lease::read_lease(&dir.join("ctxd.lease"))
        .filter(lease::Lease::is_live)
        .ok_or_else(|| format!("ctxd is not running for {}", dir.display()))?;
    let agent = (l.sockets.iter())
        .find(|s| s.ends_with("agent.sock"))
        .ok_or("lease names no agent socket")?;
    Client::connect(agent).map_err(|e| e.to_string())
}
fn main() -> ExitCode {
    contextdb_proto::fsguard::private_umask();
    match std::env::args().nth(1).as_deref() {
        Some("-V" | "--version") => {
            println!("ctx-mcp {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("-h" | "--help") => {
            println!("usage: ctx-mcp [--data] <data dir>   (or CTXDB_DATA)");
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = match args.as_slice() {
        [d] if d != "--data" => Some(PathBuf::from(d)),
        [flag, d] if flag == "--data" => Some(PathBuf::from(d)),
        [] => std::env::var_os("CTXDB_DATA").map(PathBuf::from),
        _ => None,
    };
    let Some(dir) = dir else {
        eprintln!("usage: ctx-mcp [--data] <data dir>   (or CTXDB_DATA)");
        return ExitCode::from(2);
    };
    let client = match connect(&dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ctx-mcp: {e}");
            return ExitCode::from(3);
        }
    };
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    let repo = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let mut server = Server::new(Socket(client), "mcp", &repo, &head).with_data_dir(&dir);
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle_line(&line)
            && writeln!(out, "{reply}").and_then(|()| out.flush()).is_err()
        {
            break;
        }
    }
    ExitCode::SUCCESS
}
