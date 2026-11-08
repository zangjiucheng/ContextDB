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
    /// F_FULLFSYNC on macOS (default); `false` is for G5 measurement only.
    pub full_sync: bool,
    /// Deadline of one live anchor sample; `None` is the production budget
    /// (`contextdb_anchors::capture::LIVE_TIMEOUT`). Tests that assert
    /// certified reads on a loaded machine set a longer one.
    pub anchor_live_timeout: Option<std::time::Duration>,
    /// Test hook (ADR 0017 §11 T98): the peer uid a connection reports. Only
    /// debug builds honour it; release builds always ask the kernel.
    pub peer_uid: Option<PeerUidHook>,
    /// Whether the committed access document may map other uids (multi-user
    /// mode). Off by default; `ctxd --multi-user` (or `ctx service install
    /// --multi-user`) turns it on. Starting in it requires the file-system
    /// rules of ADR 0017 §3.2.
    pub multi_user: bool,
}
/// A replacement for `getpeereid` in tests (debug builds only).
#[derive(Clone)]
pub struct PeerUidHook(pub Arc<PeerUidFn>);
/// The peer uid of an accepted connection, or `None` to close it.
pub type PeerUidFn = dyn Fn(&UnixStream) -> Option<u32> + Send + Sync;
impl std::fmt::Debug for PeerUidHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PeerUidHook")
    }
}
impl Default for Options {
    fn default() -> Self {
        Self {
            allow_fs: Vec::new(),
            principals: [PrincipalId(1), PrincipalId(2), PrincipalId(0)],
            full_sync: true,
            anchor_live_timeout: None,
            peer_uid: None,
            multi_user: false,
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
    /// Multi-user mode was asked for and the file-system rules do not hold
    /// (ADR 0017 §3.2).
    MultiUser(Vec<contextdb_proto::fsguard::Violation>),
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
/// Socket file names. The agent socket lives in `run/shared` (0711, others
/// may traverse it); the observer and admin sockets in `run/private` (0700)
/// (ADR 0017 §3.2).
const CHANNELS: [(Channel, &str); 3] = [
    (Channel::Agent, "agent.sock"),
    (Channel::Observer, "observer.sock"),
    (Channel::Admin, "admin.sock"),
];
fn channel_dir(run: &Path, channel: Channel) -> PathBuf {
    run.join(if channel == Channel::Agent {
        "shared"
    } else {
        "private"
    })
}
pub struct Daemon {
    pub service: Arc<Service<StdDisk>>,
    pub sockets: Vec<PathBuf>,
    pub lease: PathBuf,
    /// The canonical data directory and the store owner's uid.
    pub dir: PathBuf,
    pub owner_uid: u32,
    /// The socket directory (`<data>/run`, or the long-path fallback).
    run: PathBuf,
    /// Set by an admin `Shutdown` (or `stop`): the daemon should exit.
    stop: Arc<AtomicBool>,
    /// Cleanup (actor, sockets, lease) has run.
    cleaned: AtomicBool,
}
fn peer_uid(stream: &UnixStream, hook: Option<&PeerUidHook>) -> Option<u32> {
    #[cfg(debug_assertions)]
    if let Some(h) = hook {
        return (h.0)(stream);
    }
    #[cfg(not(debug_assertions))]
    let _ = hook;
    contextdb_proto::fsguard::peer_uid(stream).ok()
}
impl Daemon {
    pub fn start(dir: &Path, options: &Options) -> Result<Self, StartError> {
        std::fs::create_dir_all(dir)?;
        let dir = dir.canonicalize()?;
        lease::check_filesystem(&lease::filesystem_of(&dir)?, &options.allow_fs)
            .map_err(StartError::UnsupportedFilesystem)?;
        let disk = StdDisk {
            full_sync: options.full_sync,
        };
        let open = OpenOptions {
            projections: ProjectionFile::Path(dir.join(contextdb_store::PROJECTIONS_FILE)),
            ..OpenOptions::default()
        };
        let (store, _) = Store::open(disk.clone(), &dir, &open)?;
        lease::probe_flock(&dir.join("LOCK"))?;
        let (log, events, _) = SessionLog::open(disk.clone(), &dir, false, 64 << 20)?;
        let actor = actor::spawn(store)?;
        let recall = dir.join("recall.sqlite");
        let service = Arc::new(
            Service::with_recall(actor, log, events, Some(&recall))
                .map_err(|e| io::Error::other(format!("session log: {e:?}")))?,
        );
        service.set_anchor_routes(dir.join("anchors.routes"));
        service.set_data_dir(dir.clone());
        if let Some(timeout) = options.anchor_live_timeout {
            service.set_anchor_live_timeout(timeout);
        }
        // P7 jobs (ADR 0015 §4.4): discovery always runs; no validator is
        // called unless validators.toml configures one.
        let scheduler = crate::validators::Scheduler::open(service.actor.clone(), &dir)
            .map_err(io::Error::other)?;
        scheduler.link_candidates(service.candidates.clone());
        scheduler.start()?;
        let _ = service.validators.set(scheduler);
        // Embeddings (ADR 0016 §7): only when embeddings.toml exists. A bad
        // file leaves vectors `Failed` with the reason; recall still runs
        // on keys and text.
        match contextdb_validate::embed::Config::load(&dir) {
            Ok(None) => {}
            Ok(Some(c)) => {
                let settings = crate::embedding::Settings::from_config(&c);
                if let Err(e) =
                    service.start_vectors(settings, Some(&dir.join(contextdb_store::vectors::FILE)))
                {
                    service.vectors_unavailable(e);
                }
            }
            Err(e) => service.vectors_unavailable(e),
        }
        // Shreds whose purge did not complete before the last stop are
        // purged again, before any request is served (ADR 0017 §8.4).
        match service.purge() {
            Ok(s) if !s.complete() => eprintln!("ctxd: shred purge pending: {s:?}"),
            Ok(_) => {}
            Err(e) => eprintln!("ctxd: shred purge failed (retried at the next start): {e}"),
        }
        let uid = nix::unistd::getuid().as_raw();
        let run = lease::socket_dir(&dir, uid);
        // `run` and `run/shared` are traversable by others (0711) only inside
        // the data directory; `run/private` and the temp fallback's
        // directories stay 0700 (ADR 0017 §3.2).
        let inside = run.starts_with(&dir);
        let mut dirs = Vec::new();
        if !inside {
            dirs.extend(run.parent().map(|p| (p.to_path_buf(), 0o700)));
        }
        let open = if inside { 0o711 } else { 0o700 };
        dirs.push((run.clone(), open));
        dirs.push((run.join("shared"), open));
        dirs.push((run.join("private"), 0o700));
        for (d, mode) in &dirs {
            use std::os::unix::fs::{DirBuilderExt, MetadataExt};
            match std::fs::DirBuilder::new().mode(0o700).create(d) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(d)?;
                }
                Err(e) => return Err(e.into()),
            }
            // Every directory this daemon may have created for its sockets
            // must be a real directory owned by this user: another user must
            // not be able to swap the socket path (P3 review note).
            let m = std::fs::symlink_metadata(d)?;
            if !m.is_dir() || m.uid() != uid {
                return Err(StartError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} is not a directory owned by uid {uid}", d.display()),
                )));
            }
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(*mode))?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let mut sockets = Vec::new();
        for (n, (channel, name)) in CHANNELS.iter().enumerate() {
            let path = channel_dir(&run, *channel).join(name);
            // Stale sockets are removed only now that the flock is held,
            // including those of the P3–P8 layout directly in `run`.
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(run.join(name));
            let listener = UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            let identity = Identity {
                channel: *channel,
                principal: options.principals[n],
            };
            let (service, stop, hook) = (service.clone(), stop.clone(), options.peer_uid.clone());
            std::thread::Builder::new()
                .name(format!("ctxd-accept-{name}"))
                .spawn(move || accept(listener, identity, uid, service, stop, hook))?;
            sockets.push(path);
        }
        // The public discovery record (ADR 0017 §3.2): no pid, no paths.
        lease::write_endpoint(
            &run.join("shared").join(lease::ENDPOINT),
            &lease::Endpoint {
                protocol: proto::PROTOCOL_VERSION,
                socket: "agent.sock".into(),
                owner_uid: uid,
            },
        )?;
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
        {
            // Leftover outbox streams (ended sessions, hooks that ran out of
            // budget) are delivered at start and every 30 s (ADR 0010 §5.2).
            let (service, stop, dir) = (service.clone(), stop.clone(), dir.clone());
            let identity = Identity {
                channel: Channel::Agent,
                principal: options.principals[0],
            };
            std::thread::Builder::new()
                .name("ctxd-outbox-sweeper".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        sweep_outbox(&dir, &service, identity);
                        for _ in 0..600 {
                            if stop.load(Ordering::Acquire) {
                                return;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                    }
                })?;
        }
        let daemon = Self {
            service,
            sockets,
            lease: lease_path,
            dir,
            owner_uid: uid,
            run,
            stop,
            cleaned: AtomicBool::new(false),
        };
        if options.multi_user {
            daemon.service.enable_multi_user(daemon.multi_user());
            if let Err(v) = daemon.service.apply_access() {
                daemon.stop();
                return Err(StartError::MultiUser(v));
            }
        }
        Ok(daemon)
    }
    /// Enters (or leaves) multi-user mode with `map` (ADR 0017 §3.2–§3.3).
    /// Mapping another principal first requires the file-system rules: the
    /// sockets inside `<data>`, every entry owned by this uid with no ACL
    /// and private modes, the ancestors owned by root or the owner, and each
    /// mapped uid able to search down to the shared socket. Then the agent
    /// socket opens to others (0666; unmapped uids are still refused on
    /// accept). Without other principals it is 0600 again.
    pub fn set_principals(
        &self,
        map: crate::principals::PrincipalMap,
    ) -> Result<(), Vec<contextdb_proto::fsguard::Violation>> {
        let fs = self.multi_user();
        if map.is_multi_user() {
            let v = fs.check(map.uids());
            if !v.is_empty() {
                return Err(v);
            }
        }
        let multi = map.is_multi_user();
        self.service.set_principals(map);
        fs.chmod(multi).map_err(|v| vec![v])
    }
    /// This daemon's file-system gate for multi-user mode.
    pub fn multi_user(&self) -> crate::principals::MultiUser {
        crate::principals::MultiUser {
            dir: self.dir.clone(),
            run: self.run.clone(),
            agent_socket: self.socket(Channel::Agent).to_path_buf(),
            owner_uid: self.owner_uid,
        }
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
        // Validator jobs, then session events, then the store and its lock
        // (review finding 5).
        if let Some(s) = self.service.validators.get() {
            s.stop();
        }
        self.service.stop_indexer();
        self.service.stop_writer();
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
/// Delivers every dirty outbox stream in `dir` to `service`, in process,
/// with the hooks' own protocol (stream marks make it idempotent). Returns
/// the number of streams fully delivered.
pub fn sweep_outbox<D: contextdb_store::Durability + Clone + 'static>(
    dir: &Path,
    service: &Service<D>,
    identity: Identity,
) -> usize {
    use contextdb_proto::outbox::{Delivered, Outbox, Reply};
    let ob = Outbox::new(dir);
    let mut done = 0;
    for s in ob.dirty() {
        let st = ob.state(&s);
        if st.harness.is_empty() {
            continue;
        }
        let result = ob.deliver(
            &s,
            std::time::Duration::from_secs(5),
            |packets, skip| match service
                .handle(
                    identity,
                    Request::Facts {
                        harness: st.harness.clone(),
                        harness_session: st.harness_session.clone(),
                        stream: s,
                        packets: packets.to_vec(),
                        skip,
                    },
                )
                .response
            {
                Response::Facts {
                    applied_through, ..
                } => Ok(Reply::Applied(applied_through)),
                Response::Gap { expected } => Ok(Reply::Gap(expected)),
                other => Err(format!("{other:?}")),
            },
        );
        if matches!(result, Ok(Delivered::Done { remaining: 0, .. })) {
            done += 1;
        }
    }
    done
}
impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}
/// Accepts on one socket (ADR 0017 §3.3): the store owner's uid gets the
/// socket's channel principal; on the agent socket a uid the current access
/// document maps gets its principal; any other uid is closed before a byte
/// is read.
fn accept(
    listener: UnixListener,
    identity: Identity,
    uid: u32,
    service: Arc<Service<StdDisk>>,
    stop: Arc<AtomicBool>,
    hook: Option<PeerUidHook>,
) {
    for stream in listener.incoming() {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let Some(peer) = peer_uid(&stream, hook.as_ref()) else {
            continue;
        };
        let identity = if peer == uid {
            identity
        } else if identity.channel == Channel::Agent
            && let Some(p) = service.principals().principal_of_uid(peer)
        {
            Identity {
                channel: Channel::Agent,
                principal: p,
            }
        } else {
            continue;
        };
        let (service, stop) = (service.clone(), stop.clone());
        let _ = std::thread::Builder::new()
            .name("ctxd-conn".into())
            .spawn(move || serve(stream, identity, peer, service, stop));
    }
}
fn serve(
    mut stream: UnixStream,
    identity: Identity,
    peer: u32,
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
                        redaction: None,
                        response: Response::Error {
                            code: "ProtocolVersion".into(),
                            detail: format!("supported {}", proto::PROTOCOL_VERSION),
                        },
                    },
                );
                close_after_reply(&mut stream);
                return;
            }
            Err(_) => return,
        };
        // The mapping is checked again on every request: a uid the access
        // document no longer maps (to this principal) is revoked, and the
        // connection closes (ADR 0017 §3.3).
        if !identity.principal.is_store_owner()
            && service.principals().principal_of_uid(peer) != Some(identity.principal)
        {
            let _ = proto::write_response(
                &mut stream,
                &Envelope {
                    epoch: service.actor.snapshot().epoch,
                    redaction: None,
                    response: Response::Error {
                        code: "AccessRevoked".into(),
                        detail: "this uid is no longer mapped to the principal".into(),
                    },
                },
            );
            close_after_reply(&mut stream);
            return;
        }
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
/// Closes after an error reply without resetting the peer: the rest of the
/// rejected frame (its length is not trusted across versions) is drained,
/// bounded in bytes and time, since closing with unread input makes Linux
/// report `ECONNRESET` to the client instead of a clean end of stream.
fn close_after_reply(stream: &mut UnixStream) {
    use std::io::Read;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));
    let mut buf = [0u8; 64 * 1024];
    let mut left = proto::MAX_FRAME as usize + 8;
    while left > 0 {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => left = left.saturating_sub(n),
        }
    }
}
pub use contextdb_proto::Client;
