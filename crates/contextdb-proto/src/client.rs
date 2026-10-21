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
