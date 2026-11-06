//! Blocking client for one `ctxd` socket.
use crate::proto::{self, Envelope, FrameError, Request};
use std::{io, os::unix::net::UnixStream, path::Path, time::Duration};

/// A blocking client for one socket.
pub struct Client {
    stream: UnixStream,
}
impl Client {
    pub fn connect(path: &Path) -> io::Result<Self> {
        Ok(Self {
            stream: UnixStream::connect(path)?,
        })
    }
    /// Connects with every read and write bounded by `timeout`, so a stalled
    /// daemon surfaces as an error the caller can fail open or closed on
    /// (ADR 0010 §6.3). A local connect fails at once when nothing listens.
    pub fn connect_timeout(path: &Path, timeout: Duration) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(Self { stream })
    }
    /// Re-bounds reads and writes, e.g. to what is left of a deadline.
    pub fn set_timeout(&self, timeout: Duration) -> io::Result<()> {
        let t = Some(timeout.max(Duration::from_millis(1)));
        self.stream.set_read_timeout(t)?;
        self.stream.set_write_timeout(t)
    }
    /// Connects as a principal other than the store owner (ADR 0017 §3.2):
    /// the socket comes from `<data>/run/shared/endpoint`, and before
    /// anything is sent the server must be the store owner — the endpoint's
    /// `owner_uid` equals the owner of `<data>` and the socket's peer uid,
    /// and `<data>`'s ancestors pass the ownership rule. Otherwise
    /// `ServerNotOwner`, and nothing is sent.
    pub fn connect_shared(data: &Path, timeout: Option<Duration>) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;
        let data = data
            .canonicalize()
            .map_err(|e| format!("{}: {e}", data.display()))?;
        let shared = data.join("run").join("shared");
        let text = std::fs::read_to_string(shared.join(crate::lease::ENDPOINT))
            .map_err(|e| format!("ctxd is not running for {} ({e})", data.display()))?;
        let ep = crate::lease::Endpoint::parse(&text)
            .ok_or_else(|| "ServerNotOwner: malformed endpoint record".to_owned())?;
        if ep.protocol != proto::PROTOCOL_VERSION {
            return Err(format!(
                "ProtocolVersion: ctxd speaks {}, this client {}",
                ep.protocol,
                proto::PROTOCOL_VERSION
            ));
        }
        let owner = std::fs::metadata(&data)
            .map_err(|e| format!("{}: {e}", data.display()))?
            .uid();
        if owner != ep.owner_uid {
            return Err(format!(
                "ServerNotOwner: the endpoint names uid {}, the data directory is owned by uid {owner}",
                ep.owner_uid
            ));
        }
        crate::fsguard::check_ancestors(&data, owner)
            .map_err(|v| format!("ServerNotOwner: {v}"))?;
        let stream = UnixStream::connect(shared.join(&ep.socket))
            .map_err(|e| format!("Unavailable: {e}"))?;
        let peer = crate::fsguard::peer_uid(&stream).map_err(|e| format!("Unavailable: {e}"))?;
        if peer != owner {
            return Err(format!(
                "ServerNotOwner: the socket is served by uid {peer}, the store owner is uid {owner}"
            ));
        }
        if let Some(t) = timeout {
            stream
                .set_read_timeout(Some(t))
                .map_err(|e| e.to_string())?;
            stream
                .set_write_timeout(Some(t))
                .map_err(|e| e.to_string())?;
        }
        Ok(Self { stream })
    }
    pub fn call(&mut self, request: &Request) -> Result<Envelope, FrameError> {
        proto::write_request(&mut self.stream, request)?;
        proto::read_response(&mut self.stream)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    #[test]
    fn a_stalled_daemon_times_out() {
        let dir = std::env::temp_dir().join(format!("ctx-client-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let mut c = Client::connect_timeout(&path, Duration::from_millis(50)).unwrap();
        let held = listener.accept().unwrap(); // accepts, never answers
        let t = std::time::Instant::now();
        assert!(c.call(&Request::Status).is_err());
        assert!(t.elapsed() < Duration::from_secs(2));
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(Client::connect_timeout(&path, Duration::from_millis(50)).is_err());
    }
}
