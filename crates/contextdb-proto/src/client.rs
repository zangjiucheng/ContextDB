//! Blocking client for one `ctxd` socket.
use crate::proto::{self, Envelope, FrameError, Request};
use std::{io, os::unix::net::UnixStream, path::Path};

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
    pub fn call(&mut self, request: &Request) -> Result<Envelope, FrameError> {
        proto::write_request(&mut self.stream, request)?;
        proto::read_response(&mut self.stream)
    }
}
