//! `ctxd` startup and transport (ADR 0008 §2, §5): filesystem check, store
//! lock (T37), flock probe, session log, actor, three Unix sockets whose path
//! fixes the channel, peer-credential checks, connection threads, lease.
use crate::{
    actor,
    lease::{self, Lease},
    proto::{self, Envelope, FrameError, Request, Response},
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{
    OpenOptions, ProjectionFile, StdDisk, Store, StoreError, session::SessionLog,
};
use std::{
    io,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Debug)]
pub struct Options {
    /// Filesystems allowed despite the network/virtual refusal (`--allow-fs`).
    pub allow_fs: Vec<String>,
    /// Principals bound to the channels (single-principal mode, P3–P8).
    pub principals: [PrincipalId; 3],
}
impl Default for Options {
    fn default() -> Self {
        Self {
            allow_fs: Vec::new(),
            principals: [PrincipalId(1), PrincipalId(2), PrincipalId(0)],
        }
    }
}
#[derive(Debug)]
pub enum StartError {
    UnsupportedFilesystem(String),
    /// Another `ctxd` (or store opener) holds the lock (T37).
    Locked,
    Store(StoreError),
    Io(io::Error),
}
impl From<io::Error> for StartError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<StoreError> for StartError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Locked => Self::Locked,
            other => Self::Store(other),
        }
    }
}
const CHANNELS: [(Channel, &str); 3] = [
    (Channel::Agent, "agent.sock"),
    (Channel::Observer, "observer.sock"),
    (Channel::Admin, "admin.sock"),
];
pub struct Daemon {
    pub service: Arc<Service<StdDisk>>,
    pub sockets: Vec<PathBuf>,
    pub lease: PathBuf,
    /// Set by an admin `Shutdown` (or `stop`): the daemon should exit.
    stop: Arc<AtomicBool>,
    /// Cleanup (actor, sockets, lease) has run.
    cleaned: AtomicBool,
}
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    #[cfg(target_os = "macos")]
    {
        let (uid, _) = nix::unistd::getpeereid(stream).map_err(io::Error::from)?;
        Ok(uid.as_raw())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let cred = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
            .map_err(io::Error::from)?;
        Ok(cred.uid())
    }
}
impl Daemon {
    pub fn start(dir: &Path, options: &Options) -> Result<Self, StartError> {
        std::fs::create_dir_all(dir)?;
        let dir = dir.canonicalize()?;
        lease::check_filesystem(&lease::filesystem_of(&dir)?, &options.allow_fs)
            .map_err(StartError::UnsupportedFilesystem)?;
        let open = OpenOptions {
            projections: ProjectionFile::Path(dir.join("projections.sqlite")),
            ..OpenOptions::default()
        };
        let (store, _) = Store::open(StdDisk::default(), &dir, &open)?;
        lease::probe_flock(&dir.join("LOCK"))?;
        let (log, events, _) = SessionLog::open(StdDisk::default(), &dir, false, 64 << 20)?;
        let actor = actor::spawn(store)?;
        let recall = dir.join("recall.sqlite");
        let service = Arc::new(Service::with_recall(
            actor,
            StdDisk::default(),
            dir.clone(),
            log,
            events,
            Some(&recall),
        ));
        let uid = nix::unistd::getuid().as_raw();
        let run = lease::socket_dir(&dir, uid);
        std::fs::create_dir_all(&run)?;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700))?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut sockets = Vec::new();
        for (n, (channel, name)) in CHANNELS.iter().enumerate() {
            let path = run.join(name);
            // Stale sockets are removed only now that the flock is held.
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            let identity = Identity {
                channel: *channel,
                principal: options.principals[n],
            };
            let (service, stop) = (service.clone(), stop.clone());
            std::thread::Builder::new()
                .name(format!("ctxd-accept-{name}"))
                .spawn(move || accept(listener, identity, uid, service, stop))?;
            sockets.push(path);
        }
        let lease_path = dir.join("ctxd.lease");
        let pid = std::process::id();
        lease::write_lease(
            &lease_path,
            &Lease {
                pid,
                start_time: lease::process_start_time(pid).unwrap_or_default(),
                protocol: proto::PROTOCOL_VERSION,
                sockets: sockets.clone(),
            },
        )?;
        Ok(Self {
            service,
            sockets,
            lease: lease_path,
            stop,
            cleaned: AtomicBool::new(false),
        })
    }
    /// Socket path of a channel.
    pub fn socket(&self, channel: Channel) -> &Path {
        let n = CHANNELS
            .iter()
            .position(|(c, _)| *c == channel)
            .expect("channel");
        &self.sockets[n]
    }
    /// Blocks until a `Shutdown` request or `stop` was handled.
    pub fn wait(&self) {
        while !self.stop.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    /// Stops accepting, removes sockets and the lease, stops the actor.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if self.cleaned.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self
            .service
            .actor
            .request(|reply| actor::Message::Shutdown { reply });
        for s in &self.sockets {
            let _ = UnixStream::connect(s); // wake the accept loop
            let _ = std::fs::remove_file(s);
        }
        let _ = std::fs::remove_file(&self.lease);
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}
fn accept(
    listener: UnixListener,
    identity: Identity,
    uid: u32,
    service: Arc<Service<StdDisk>>,
    stop: Arc<AtomicBool>,
) {
    for stream in listener.incoming() {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let Ok(stream) = stream else { continue };
        // Single-principal mode: only the daemon's own uid may connect.
        if peer_uid(&stream).ok() != Some(uid) {
            continue;
        }
        let (service, stop) = (service.clone(), stop.clone());
        let _ = std::thread::Builder::new()
            .name("ctxd-conn".into())
            .spawn(move || serve(stream, identity, service, stop));
    }
}
fn serve(
    mut stream: UnixStream,
    identity: Identity,
    service: Arc<Service<StdDisk>>,
    stop: Arc<AtomicBool>,
) {
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(30)));
    loop {
        let request = match proto::read_request(&mut stream) {
            Ok(r) => r,
            Err(FrameError::Version(_)) => {
                let _ = proto::write_response(
                    &mut stream,
                    &Envelope {
                        epoch: Epoch(0),
                        response: Response::Error {
                            code: "ProtocolVersion".into(),
                            detail: format!("supported {}", proto::PROTOCOL_VERSION),
                        },
                    },
                );
                return;
            }
            Err(_) => return,
        };
        let shutdown = matches!(request, Request::Shutdown) && identity.channel == Channel::Admin;
        let envelope = service.handle(identity, request);
        let ok = !matches!(envelope.response, Response::Error { .. });
        if proto::write_response(&mut stream, &envelope).is_err() {
            return;
        }
        if shutdown && ok {
            stop.store(true, Ordering::Release);
            return;
        }
    }
}
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
