//! MCP JSON-RPC handling (ADR 0008 §10): any input line yields either nothing
//! (notification) or one JSON-RPC response, never a panic.
#![no_main]
use contextdb_daemon::proto::{Envelope, Request, Response};
use contextdb_mcp::{Backend, Server};
use contextdb_model::Epoch;
use libfuzzer_sys::fuzz_target;

/// A backend that answers every request with an error (no daemon in the loop).
struct Refuse;
impl Backend for Refuse {
    fn call(&mut self, request: Request) -> Result<Envelope, String> {
        let response = match request {
            Request::SessionStart { .. } => Response::Session { session: [0; 16] },
            _ => Response::Error {
                code: "Fuzz".into(),
                detail: String::new(),
            },
        };
        Ok(Envelope {
            epoch: Epoch(0),
            response,
        })
    }
}
fuzz_target!(|data: &[u8]| {
    let line = String::from_utf8_lossy(data);
    let mut server = Server::new(Refuse, "fuzz", "repo", "head");
    if let Some(reply) = server.handle_line(&line) {
        assert!(reply.starts_with('{'), "responses are JSON objects");
    }
});
