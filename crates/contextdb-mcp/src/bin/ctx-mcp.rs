//! `ctx-mcp <data dir>`: MCP over stdio, connected to the daemon's agent socket
//! (discovered from `<dir>/ctxd.lease`). One JSON-RPC message per line.
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
fn main() -> ExitCode {
    let Some(dir) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: ctx-mcp <data dir>");
        return ExitCode::from(2);
    };
    let Some(l) = lease::read_lease(&dir.join("ctxd.lease")).filter(lease::Lease::is_live) else {
        eprintln!("ctx-mcp: ctxd is not running for {}", dir.display());
        return ExitCode::from(3);
    };
    let Some(agent) = l.sockets.iter().find(|s| s.ends_with("agent.sock")) else {
        eprintln!("ctx-mcp: lease names no agent socket");
        return ExitCode::from(3);
    };
    let client = match Client::connect(agent) {
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
    let mut server = Server::new(Socket(client), "mcp", &repo, &head);
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
