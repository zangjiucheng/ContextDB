//! Request handling, independent of sockets (ADR 0008 §5–§8). Identity
//! (channel, principal) comes from the transport, never from the request.
// Internal helpers return `Err(Response)`: the error is the reply itself and
// is returned immediately, never stored, so its size does not matter.
#![allow(clippy::result_large_err)]
use crate::{
    actor::{ActorHandle, Message, Snapshot, SubmitError},
    proto::*,
};
use contextdb_core::{Db, IssueRequest, LogLink};
use contextdb_model::*;
use contextdb_store::recall::RecallIndex;
use contextdb_store::{
    Durability, Payload, StoreError,
    session::{Layer, SessionEvent, SessionLog},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

/// Transport-bound identity of a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub channel: Channel,
    pub principal: PrincipalId,
}
/// One read-ledger entry (ADR 0008 §6.2).
#[derive(Clone, Debug, PartialEq, Eq)]
struct LedgerEntry {
    logical: LogicalId,
    rev: RevisionId,
    required: bool,
    epoch: Epoch,
}
struct Session {
    principal: PrincipalId,
    head_sha: String,
    /// Live required read per logical (a newer read supersedes an older one).
    required: BTreeMap<LogicalId, LedgerEntry>,
    /// Supersessions not yet reported by `ctx_check`: (logical, old, new).
    superseded: Vec<(LogicalId, RevisionId, RevisionId)>,
    /// Certificates issued to this session (recall and session certificates).
    certs: Vec<Certificate>,
    session_cert: Option<Certificate>,
    /// Durable events of this session name epochs the log no longer holds
    /// in the same form (e.g. after `ctx doctor --truncate-before`): its
    /// ledger and handles may name reallocated ids (P3 review finding 2).
    log_broken: bool,
    /// A `LogBroken` event for this session is in the session log.
    broken_durable: bool,
}
enum Writer {
    Events(Vec<SessionEvent>),
    /// Reply once everything queued before it is durable.
    Flush(mpsc::Sender<Result<(), StoreError>>),
    /// Write what is queued, acknowledge and exit.
    Stop(mpsc::Sender<()>),
}
pub struct Service<D: Durability> {
    pub actor: ActorHandle<D>,
    sessions: Mutex<BTreeMap<SessionId, Arc<Mutex<Session>>>>,
    writer: Mutex<mpsc::Sender<Writer>>,
    writer_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    recall: Arc<Mutex<RecallIndex>>,
    stop: Arc<AtomicBool>,
}
impl<D: Durability> Drop for Service<D> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.stop_writer();
    }
}
impl<D: Durability> Service<D> {
    /// Writes every queued session event and stops the writer thread. Run
    /// before the store (and its lock) is released, so nothing appends to
    /// `sessions/` unlocked and no lazy event is lost on a clean shutdown
    /// (P3 review finding 5). Idempotent.
    pub fn stop_writer(&self) {
        // Held until the writer has exited, so a concurrent caller cannot go
        // on to stop the actor while events are still being written (N3).
        let mut slot = self.writer_thread.lock().expect("writer thread");
        let Some(thread) = slot.take() else {
            return;
        };
        let (tx, _rx) = mpsc::channel();
        let _ = self.writer.lock().expect("writer").send(Writer::Stop(tx));
        let _ = thread.join();
    }
}
/// Most items one recall returns (ADR 0008 §9).
pub const MAX_RECALL: usize = 1_000;
fn random_id() -> [u8; 16] {
    use std::io::Read;
    let mut id = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut id))
        .expect("/dev/urandom");
    id
}
/// Stable error code: the SPEC §6.2 name of the variant.
pub fn code(e: &TxError) -> String {
    let s = format!("{e:?}");
    s.split(['{', '(', ' '])
        .next()
        .unwrap_or("TxError")
        .to_owned()
}
fn store_code(e: &StoreError) -> (String, String) {
    match e {
        StoreError::Tx(t) => (code(t), format!("{t:?}")),
        // A poisoned group's outcome is unknown until restart (ADR 0008 §3.2).
        StoreError::Poisoned => (
            "OutcomeUnknown".into(),
            "query txn_status after restart".into(),
        ),
        other => {
            let s = format!("{other:?}");
            (
                s.split(['{', '(', ' '])
                    .next()
                    .unwrap_or("Store")
                    .to_owned(),
                s,
            )
        }
    }
}
fn err(code: &str, detail: impl Into<String>) -> Response {
    Response::Error {
        code: code.into(),
        detail: detail.into(),
    }
}
fn submit_err(e: SubmitError) -> Response {
    match e {
        SubmitError::Busy => err("Busy", "write queue full"),
        SubmitError::Stopped => err("Unavailable", "daemon stopping"),
        SubmitError::Timeout => err(
            "Timeout",
            "no reply within the request timeout; a write may still commit (resolve with TxnStatus)",
        ),
    }
}
fn handle_of(cert: &Certificate, index: usize) -> String {
    let hex: String = cert.cert_id[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{hex}.{index}")
}
impl<D: Durability + Clone + 'static> Service<D> {
    /// Starts the session-log writer and replays the session log into memory.
    ///
    /// Fails when a newly detected log cut cannot be made durable: serving
    /// then would let plain writes reuse the cut epochs and hide the break
    /// from the next restart.
    pub fn new(
        actor: ActorHandle<D>,
        log: SessionLog<D>,
        events: Vec<SessionEvent>,
    ) -> Result<Self, Response> {
        Self::with_recall(actor, log, events, None)
    }
    /// As `new`, with the FTS5 recall projection in `recall_path` (memory if None).
    pub fn with_recall(
        actor: ActorHandle<D>,
        log: SessionLog<D>,
        events: Vec<SessionEvent>,
        recall_path: Option<&std::path::Path>,
    ) -> Result<Self, Response> {
        let recall = Arc::new(Mutex::new(
            RecallIndex::open(recall_path)
                .or_else(|_| RecallIndex::open(None))
                .expect("recall index"),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (actor, recall, stop) = (actor.clone(), recall.clone(), stop.clone());
            std::thread::Builder::new()
                .name("ctxd-indexer".into())
                .spawn(move || indexer(actor, recall, stop))
                .expect("indexer thread");
        }
        let (tx, rx) = mpsc::channel();
        let writer_thread = std::thread::Builder::new()
            .name("ctxd-session-log".into())
            .spawn(move || writer(log, rx))
            .expect("session writer thread");
        let service = Self {
            actor,
            sessions: Mutex::new(BTreeMap::new()),
            writer: Mutex::new(tx),
            writer_thread: Mutex::new(Some(writer_thread)),
            recall,
            stop,
        };
        // Persist every newly detected log cut before serving (closure N1).
        let broken = service.replay(events);
        if !broken.is_empty() {
            service.log(
                broken
                    .into_iter()
                    .map(|session_id| SessionEvent::LogBroken { session_id })
                    .collect(),
            );
            if let Err(e) = service.flush() {
                // Release the store (and its lock) before reporting.
                service.stop_writer();
                let _ = service.actor.request(|reply| Message::Shutdown { reply });
                return Err(e);
            }
        }
        Ok(service)
    }
    /// Rebuilds in-memory sessions from durable events (sessions survive restarts).
    ///
    /// A session whose durable events name an epoch beyond the current tail,
    /// or whose start checksum no longer matches the log, is marked
    /// `log_broken`: the log was cut under it, so its ledger and handles may
    /// name reallocated ids.
    ///
    /// Returns the sessions newly found broken, to be marked durably.
    fn replay(&self, events: Vec<SessionEvent>) -> Vec<SessionId> {
        let snap = self.actor.snapshot();
        let tail = snap.epoch;
        let mut map = self.sessions.lock().expect("sessions");
        let broken = |map: &BTreeMap<SessionId, Arc<Mutex<Session>>>, id: &SessionId| {
            if let Some(s) = map.get(id) {
                s.lock().expect("session").log_broken = true;
            }
        };
        for e in events {
            match e {
                SessionEvent::Start {
                    session_id,
                    principal,
                    head_sha,
                    started_epoch,
                    log_checksum,
                    ..
                } => {
                    let log_broken = started_epoch > tail
                        || snap.log.checksum_at(started_epoch).ok() != Some(log_checksum);
                    map.insert(
                        session_id,
                        Arc::new(Mutex::new(Session {
                            principal,
                            head_sha,
                            required: BTreeMap::new(),
                            superseded: Vec::new(),
                            certs: Vec::new(),
                            session_cert: None,
                            log_broken,
                            broken_durable: false,
                        })),
                    );
                }
                SessionEvent::Read {
                    session_id,
                    logical,
                    rev,
                    epoch,
                    required,
                    ..
                } => {
                    if epoch > tail {
                        broken(&map, &session_id);
                    }
                    if let Some(s) = map.get(&session_id) {
                        s.lock().expect("session").record(LedgerEntry {
                            logical,
                            rev,
                            required,
                            epoch,
                        });
                    }
                }
                SessionEvent::CertIssued { cert } => {
                    if cert.issued_epoch > tail
                        || snap.log.checksum_at(cert.issued_epoch).ok()
                            != Some(cert.log_checksum_at_issue)
                    {
                        broken(&map, &cert.session_id);
                    }
                    if let Some(s) = map.get(&cert.session_id) {
                        let mut s = s.lock().expect("session");
                        s.certs.push((*cert).clone());
                    }
                }
                SessionEvent::LogBroken { session_id } => {
                    if let Some(s) = map.get(&session_id) {
                        let mut s = s.lock().expect("session");
                        s.log_broken = true;
                        s.broken_durable = true;
                    }
                }
                SessionEvent::SessionCert {
                    session_id,
                    cert_id,
                } => {
                    if let Some(s) = map.get(&session_id) {
                        let mut s = s.lock().expect("session");
                        s.session_cert = s.certs.iter().find(|c| c.cert_id == cert_id).cloned();
                    }
                }
                SessionEvent::End { session_id, .. } => {
                    map.remove(&session_id);
                }
                SessionEvent::Write { .. } => {}
            }
        }
        map.iter()
            .filter(|(_, s)| {
                let s = s.lock().expect("session");
                s.log_broken && !s.broken_durable
            })
            .map(|(id, _)| *id)
            .collect()
    }
    fn log(&self, events: Vec<SessionEvent>) {
        let _ = self
            .writer
            .lock()
            .expect("writer")
            .send(Writer::Events(events));
    }
    /// Waits until every event queued so far is durable, for at most the
    /// request timeout (ADR 0008 §5; P3 review finding 9).
    fn flush(&self) -> Result<(), Response> {
        let (tx, rx) = mpsc::channel();
        let _ = self.writer.lock().expect("writer").send(Writer::Flush(tx));
        match rx.recv_timeout(self.actor.timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(err("Io", format!("session log: {e:?}"))),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(err(
                "Timeout",
                "session log sync exceeded the request timeout",
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(err("Unavailable", "session writer stopped"))
            }
        }
    }
    fn session(&self, id: &SessionId) -> Option<Arc<Mutex<Session>>> {
        self.sessions.lock().expect("sessions").get(id).cloned()
    }
    pub fn session_count(&self) -> usize {
        self.sessions.lock().expect("sessions").len()
    }
    /// Handles one request on the given snapshot-consistent view.
    pub fn handle(&self, id: Identity, req: Request) -> Envelope {
        let snap = self.actor.snapshot();
        let epoch = snap.epoch;
        let response = self.dispatch(id, req, snap);
        Envelope { epoch, response }
    }
    /// Takes the snapshot by value: write paths release it before waiting for
    /// their group, so a queued writer never pins a replica the actor wants
    /// to reclaim (ADR 0008 §3.3).
    fn dispatch(&self, id: Identity, req: Request, snap: Arc<Snapshot<D>>) -> Response {
        let db = &snap.db;
        match req {
            Request::SessionStart {
                harness,
                repo,
                head_sha,
                parent,
            } => {
                let session_id = random_id();
                let log_checksum = match self.checksum(snap.epoch) {
                    Ok(c) => c,
                    Err(r) => return r,
                };
                self.sessions.lock().expect("sessions").insert(
                    session_id,
                    Arc::new(Mutex::new(Session {
                        principal: id.principal,
                        head_sha: head_sha.clone(),
                        required: BTreeMap::new(),
                        superseded: Vec::new(),
                        certs: Vec::new(),
                        session_cert: None,
                        log_broken: false,
                        broken_durable: false,
                    })),
                );
                self.log(vec![SessionEvent::Start {
                    session_id,
                    parent,
                    harness,
                    repo,
                    branch: MAIN,
                    head_sha,
                    principal: id.principal,
                    channel: id.channel,
                    started_epoch: snap.epoch,
                    log_checksum,
                }]);
                match self.flush() {
                    Ok(()) => Response::Session {
                        session: session_id,
                    },
                    Err(r) => {
                        // Never durable: the session does not exist.
                        self.sessions.lock().expect("sessions").remove(&session_id);
                        r
                    }
                }
            }
            Request::SessionEnd { session } => {
                {
                    let mut sessions = self.sessions.lock().expect("sessions");
                    match sessions.get(&session) {
                        None => return err("UnknownSession", "unknown session"),
                        Some(s) if s.lock().expect("session").principal != id.principal => {
                            return err("UnknownSession", "session of another principal");
                        }
                        Some(_) => {}
                    }
                    sessions.remove(&session);
                    self.log(vec![SessionEvent::End {
                        session_id: session,
                        epoch: snap.epoch,
                    }]);
                }
                match self.flush() {
                    Ok(()) => Response::Done,
                    Err(r) => r,
                }
            }
            Request::Recall {
                session,
                query,
                scope,
                mode,
                limit,
            } => self.recall(session, &query, &scope, mode, limit as usize, &snap),
            Request::Read {
                session,
                target,
                mode,
            } => self.read(session, &target, mode, &snap),
            Request::Record {
                session,
                kind,
                body,
                target,
                deps,
                adopt_as_base,
                client_txn_id,
            } => self.record(
                id,
                session,
                kind,
                body,
                target,
                deps,
                adopt_as_base,
                client_txn_id,
                snap,
            ),
            Request::Supersede {
                session,
                logical,
                body,
                expected_head,
                adopt_as_base,
                carry_rules,
                reason,
                client_txn_id,
            } => self.supersede(
                id,
                session,
                logical,
                body,
                expected_head,
                adopt_as_base,
                carry_rules,
                reason,
                client_txn_id,
                snap,
            ),
            Request::Why { target, depth } => match self.resolve_plain(&target, db) {
                Ok((_, r)) => match db.why_depth(r, depth as usize) {
                    Ok(proof) => Response::Why { proof },
                    Err(e) => err(&code(&e), format!("{e:?}")),
                },
                Err(r) => r,
            },
            Request::WhyNot { target, depth } => match self.resolve_plain(&target, db) {
                Ok((_, r)) => match db.why_not(r, depth as usize) {
                    Ok(why_not) => Response::WhyNot { why_not },
                    Err(e) => err(&code(&e), format!("{e:?}")),
                },
                Err(r) => r,
            },
            Request::Impact { target } => match self.resolve_plain(&target, db) {
                Ok((_, r)) => match db.impact_reachable(&[r]) {
                    Ok(impact) => Response::Impact { impact },
                    Err(e) => err(&code(&e), format!("{e:?}")),
                },
                Err(r) => r,
            },
            Request::ImpactBatch { batch } => {
                drop(snap);
                let mut batch = batch;
                batch.channel = id.channel;
                batch.principal = id.principal;
                match self
                    .actor
                    .request(|reply| Message::ImpactExact { batch, reply })
                {
                    Ok(Ok(result)) => Response::ImpactExact { result },
                    Ok(Err(e)) => {
                        let (c, d) = store_code(&e);
                        err(&c, d)
                    }
                    Err(e) => submit_err(e),
                }
            }
            Request::Check {
                session,
                cert_id,
                live_pos,
            } => self.check(session, cert_id, live_pos.into_iter().collect(), &snap),
            Request::Diff { from, to } => {
                let result = if from.min(to) >= db.initial_epoch() {
                    db.diff(from, to)
                } else {
                    match snap.log.replay_to(from.max(to)) {
                        Ok(full) => full.diff(from, to),
                        Err(e) => {
                            let (c, d) = store_code(&e);
                            return err(&c, d);
                        }
                    }
                };
                match result {
                    Ok(diff) => Response::Diff { diff },
                    Err(e) => err(&code(&e), format!("{e:?}")),
                }
            }
            Request::Status => Response::Status {
                revisions: db.revision_count() as u64,
                logicals: db.logical_count() as u64,
                sessions: self.session_count() as u64,
            },
            Request::TxnStatus { id: txn } => match self
                .actor
                .request(|reply| Message::TxnStatus { id: txn, reply })
            {
                Ok(Ok(contextdb_store::TxnStatus::Committed { epoch, result })) => {
                    Response::TxnStatus {
                        committed: Some((epoch, *result)),
                    }
                }
                Ok(Ok(contextdb_store::TxnStatus::NotCommitted)) => {
                    Response::TxnStatus { committed: None }
                }
                Ok(Err(e)) => {
                    let (c, d) = store_code(&e);
                    err(&c, d)
                }
                Err(e) => submit_err(e),
            },
            Request::Validate {
                rule,
                verdict,
                curvec_seen,
                validator,
            } => {
                drop(snap);
                // Human validation only (ADR 0008 §0.4).
                if id.channel != Channel::Admin {
                    return err(
                        "ChannelRestricted",
                        "ctx validate requires the admin socket",
                    );
                }
                let batch = Batch::new(vec![Op::RecordValidation {
                    rule: rule.into(),
                    curvec_seen,
                    verdict,
                    validator,
                }]);
                self.commit(id, Vec::new(), batch, None)
            }
            Request::Raw { blobs, batch } => {
                drop(snap);
                if id.channel == Channel::Agent {
                    return err(
                        "ChannelRestricted",
                        "raw batches require the observer or admin socket",
                    );
                }
                self.commit(id, blobs, batch, None)
            }
            Request::Shutdown => {
                if id.channel != Channel::Admin {
                    return err("ChannelRestricted", "shutdown requires the admin socket");
                }
                self.stop_writer();
                match self.actor.request(|reply| Message::Shutdown { reply }) {
                    Ok(()) => Response::Done,
                    Err(e) => submit_err(e),
                }
            }
        }
    }
    fn checksum(&self, epoch: Epoch) -> Result<ContentHash, Response> {
        self.actor.snapshot().log.checksum_at(epoch).map_err(|e| {
            let (c, d) = store_code(&e);
            err(&c, d)
        })
    }
    /// Stamps the transport identity, assigns a client_txn_id and commits.
    fn commit(
        &self,
        id: Identity,
        blobs: Vec<Vec<u8>>,
        mut batch: Batch,
        txn: Option<[u8; 16]>,
    ) -> Response {
        batch.channel = id.channel;
        batch.principal = id.principal;
        batch.client_txn_id = Some(txn.unwrap_or_else(random_id));
        match self.actor.write(blobs, batch) {
            Ok(Ok(result)) => Response::Committed { result },
            Ok(Err(e)) => {
                let (c, d) = store_code(&e);
                err(&c, d)
            }
            Err(e) => submit_err(e),
        }
    }
    fn item(&self, db: &Db, l: LogicalId, r: RevisionId, handle: Option<String>) -> Item {
        let (key, kind, _) = db.logical_info(l).expect("existing logical");
        let state = db.node(r).expect("existing revision");
        let summary = db
            .summary_at(r, &BTreeMap::new())
            .expect("existing revision");
        Item {
            logical: l,
            key,
            kind,
            rev: r,
            state,
            summary,
            handle,
        }
    }
    /// Target → (logical, revision) without session context (no handles).
    fn resolve_plain(&self, target: &Target, db: &Db) -> Result<(LogicalId, RevisionId), Response> {
        let head = |l: LogicalId| {
            db.head(l)
                .map(|r| (l, r))
                .ok_or_else(|| err("UnknownRef", "no such logical"))
        };
        match target {
            Target::Logical(l) => head(*l),
            Target::Key(k) => db
                .logical_by_key(k)
                .ok_or_else(|| err("UnknownRef", format!("no logical {k}")))
                .and_then(head),
            Target::Pinned { logical, rev } => match db.revision_logical(*rev) {
                Some(l) if l == *logical => Ok((l, *rev)),
                _ => Err(err(
                    "InvalidArgument",
                    "revision does not belong to logical",
                )),
            },
            Target::KeyRev { key, rev } => match db.logical_by_key(key) {
                Some(l) if db.revision_logical(*rev) == Some(l) => Ok((l, *rev)),
                _ => Err(err(
                    "InvalidArgument",
                    format!("{key}@{} does not exist", rev.0),
                )),
            },
            Target::Handle(_) => Err(err("InvalidArgument", "handles need a session")),
        }
    }
    /// Handle → the required entry of a certificate issued to this session.
    fn resolve_handle(
        &self,
        session: &Session,
        handle: &str,
    ) -> Option<(LogicalId, RevisionId, ContentHash)> {
        // Handles of a session whose log was cut may name reallocated ids.
        if session.log_broken {
            return None;
        }
        let (prefix, index) = handle.split_once('.')?;
        let index: usize = index.parse().ok()?;
        let cert = session
            .certs
            .iter()
            .find(|c| handle_of(c, 0).split('.').next() == Some(prefix))?;
        let e = cert.entries.get(index).filter(|e| e.required)?;
        Some((e.logical, e.rev, cert.cert_id))
    }
    fn recall(
        &self,
        session: SessionId,
        query: &str,
        scope: &str,
        mode: Mode,
        limit: usize,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        let Some(s) = self.session(&session) else {
            return err("UnknownSession", "unknown session");
        };
        let limit = limit.clamp(1, MAX_RECALL);
        let db = &snap.db;
        // Basic retrieval (ADR 0008 §9): exact key, then prefix, then substring.
        let mut found: Vec<(u8, LogicalId)> = (0..db.logical_count() as u32)
            .map(LogicalId)
            .filter_map(|l| {
                let (key, _, lane) = db.logical_info(l)?;
                if !scope.is_empty() && lane != scope {
                    return None;
                }
                let rank = if key == query {
                    0
                } else if key.starts_with(query) {
                    1
                } else if key.contains(query) {
                    2
                } else {
                    return None;
                };
                Some((rank, l))
            })
            .collect();
        // Full-text candidates after key matches; the index may lag the
        // snapshot, but every candidate's status comes from the snapshot.
        if let Ok(hits) = self
            .recall
            .lock()
            .expect("recall")
            .search(query, scope, limit)
        {
            for (n, l) in hits.into_iter().enumerate() {
                if l.index() < db.logical_count() && !found.iter().any(|(_, f)| *f == l) {
                    found.push((3 + n.min(250) as u8, l));
                }
            }
        }
        found.sort();
        found.truncate(limit);
        let mut items = Vec::new();
        let mut withheld = Vec::new();
        for (_, l) in found {
            let r = db.head(l).expect("existing logical");
            let item = self.item(db, l, r, None);
            if mode == Mode::Certified && item.summary != Summary::Certified {
                withheld.push(item); // I6: never presented as certified
            } else {
                items.push(item);
            }
        }
        let mut s = s.lock().expect("session");
        let required = mode == Mode::Certified;
        let mut events = Vec::new();
        for item in &items {
            let entry = LedgerEntry {
                logical: item.logical,
                rev: item.rev,
                required,
                epoch: snap.epoch,
            };
            events.push(read_event(session, &entry));
            s.record(entry);
        }
        let mut cert_id = None;
        if required && !items.is_empty() {
            let entries: Vec<CertItem> = items
                .iter()
                .map(|i| CertItem {
                    logical: i.logical,
                    rev: i.rev,
                    required: true,
                    via: Via::Head,
                    since_epoch: snap.epoch,
                })
                .collect();
            let request = match self.issue_request(session, &s, snap.epoch, false) {
                Ok(r) => r,
                Err(r) => return r,
            };
            if let Ok(cert) = db.issue_certificate(&request, &entries, &BTreeMap::new()) {
                for (n, item) in items.iter_mut().enumerate() {
                    item.handle = Some(handle_of(&cert, n));
                }
                cert_id = Some(cert.cert_id);
                events.push(SessionEvent::CertIssued {
                    cert: Box::new(cert.clone()),
                });
                s.certs.push(cert);
            }
        }
        // Reads and the recall certificate are queued, not awaited: recall stays
        // on the read budget; a write using a handle flushes first (ADR 0008 §8.5).
        // Queued under the session lock, so the durable order is the ledger
        // order (N17; P3 review finding 4).
        self.log(events);
        drop(s);
        Response::Recall {
            items,
            withheld,
            cert_id,
        }
    }
    fn issue_request(
        &self,
        session: SessionId,
        s: &Session,
        epoch: Epoch,
        strict: bool,
    ) -> Result<IssueRequest, Response> {
        Ok(IssueRequest {
            session_id: session,
            principal: s.principal,
            branch: MAIN,
            log_checksum: self.checksum(epoch)?,
            code_commit: s.head_sha.clone(),
            strict_no_aba: strict,
        })
    }
    fn read(
        &self,
        session: SessionId,
        target: &Target,
        mode: Mode,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        let Some(s) = self.session(&session) else {
            return err("UnknownSession", "unknown session");
        };
        let db = &snap.db;
        let resolved = match target {
            Target::Handle(h) => {
                let s = s.lock().expect("session");
                match self.resolve_handle(&s, h) {
                    Some((l, r, _)) => Ok((l, r)),
                    None => Err(err(
                        "InvalidDep",
                        "handle is not in a certificate of this session",
                    )),
                }
            }
            other => self.resolve_plain(other, db),
        };
        let (l, r) = match resolved {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let item = self.item(db, l, r, None);
        let certified = item.summary == Summary::Certified && db.head(l) == Some(r);
        let required = mode == Mode::Certified && certified;
        let entry = LedgerEntry {
            logical: l,
            rev: r,
            required,
            epoch: snap.epoch,
        };
        {
            let mut s = s.lock().expect("session");
            s.record(entry.clone());
            self.log(vec![read_event(session, &entry)]);
        }
        if mode == Mode::Certified && !certified {
            // CertifiedCurrent fails closed, without payload (T07e).
            let summary = if db.head(l) == Some(r) {
                item.summary.clone()
            } else {
                Summary::Stale(StaleReason::HeadSuperseded)
            };
            return err("NotCertified", format!("{:?} {:?}", l, summary));
        }
        let content = db.revision_content(r).expect("existing revision");
        let payload = match snap.log.read_blob(&content) {
            Payload::Available(bytes) => Some(bytes),
            Payload::Unavailable(_) => None,
        };
        Response::Read { item, payload }
    }
    /// Deps: handles of this session's certificates (required entries) or
    /// explicit `logical@rev` (ADR 0008 §6.1, T36).
    fn deps(
        &self,
        session: &Arc<Mutex<Session>>,
        deps: &[Dep],
        db: &Db,
    ) -> Result<Vec<(RevRef, EdgeKind)>, Response> {
        let mut needs_flush = false;
        let mut out = Vec::new();
        {
            let s = session.lock().expect("session");
            for d in deps {
                let rev = match &d.target {
                    Target::Handle(h) => {
                        let (_, r, _) = self.resolve_handle(&s, h).ok_or_else(|| {
                            err(
                                "InvalidDep",
                                format!(
                                    "{h} is not a required entry of this session's certificates"
                                ),
                            )
                        })?;
                        needs_flush = true;
                        r
                    }
                    Target::Pinned { logical, rev } => {
                        if db.revision_logical(*rev) != Some(*logical) {
                            return Err(err("InvalidDep", "explicit logical@rev does not exist"));
                        }
                        *rev
                    }
                    Target::KeyRev { key, rev } => match db.logical_by_key(key) {
                        Some(l) if db.revision_logical(*rev) == Some(l) => *rev,
                        _ => return Err(err("InvalidDep", "explicit key@rev does not exist")),
                    },
                    Target::Logical(_) | Target::Key(_) => {
                        return Err(err(
                            "InvalidDep",
                            "deps must be handles or explicit logical@rev",
                        ));
                    }
                };
                out.push((RevRef::Existing(rev), d.kind));
            }
        }
        // A handle is usable once its certificate is durable.
        if needs_flush {
            self.flush()?;
        }
        Ok(out)
    }
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        id: Identity,
        session: SessionId,
        kind: RecordKind,
        body: Vec<u8>,
        target: WriteTarget,
        deps: Vec<Dep>,
        adopt_as_base: bool,
        txn: Option<[u8; 16]>,
        snap: Arc<Snapshot<D>>,
    ) -> Response {
        let Some(s) = self.session(&session) else {
            return err("UnknownSession", "unknown session");
        };
        let incidences = match self.deps(&s, &deps, &snap.db) {
            Ok(v) => v,
            Err(r) => return r,
        };
        drop(snap);
        let content = *blake3::hash(&body).as_bytes();
        let mut ops = match target {
            WriteTarget::Create { key, scope_lane } => vec![Op::CreateLogical {
                key,
                kind,
                scope_lane,
                content,
                adopt_as_base,
            }],
            WriteTarget::Revise {
                logical,
                expected_head,
            } => vec![
                Op::AddRevision {
                    logical: logical.into(),
                    content,
                    adopt_as_base,
                },
                Op::SetHead {
                    logical: logical.into(),
                    expected: expected_head.into(),
                    new: RevRef::New(0),
                },
            ],
        };
        if !incidences.is_empty() {
            ops.push(Op::AddRule {
                conclusion: RevRef::New(0),
                incidences,
                enabled: true,
            });
        }
        self.commit(id, vec![body], Batch::new(ops), txn)
    }
    #[allow(clippy::too_many_arguments)]
    fn supersede(
        &self,
        id: Identity,
        session: SessionId,
        logical: LogicalId,
        body: Vec<u8>,
        expected_head: RevisionId,
        adopt_as_base: bool,
        carry_rules: bool,
        reason: String,
        txn: Option<[u8; 16]>,
        snap: Arc<Snapshot<D>>,
    ) -> Response {
        if self.session(&session).is_none() {
            return err("UnknownSession", "unknown session");
        }
        let content = *blake3::hash(&body).as_bytes();
        let mut ops = vec![
            Op::AddRevision {
                logical: logical.into(),
                content,
                adopt_as_base,
            },
            Op::SetHead {
                logical: logical.into(),
                expected: expected_head.into(),
                new: RevRef::New(0),
            },
        ];
        if carry_rules {
            // Copy the old head's own justification only; dependents are never
            // rebound (SPEC §3.5 #2, ADR 0008 §0.3).
            for (_, enabled, premises, _) in snap.db.rules_concluding(expected_head) {
                if enabled {
                    ops.push(Op::AddRule {
                        conclusion: RevRef::New(0),
                        incidences: premises
                            .iter()
                            .map(|p| (RevRef::Existing(p.premise), p.kind))
                            .collect(),
                        enabled: true,
                    });
                }
            }
        }
        drop(snap);
        let txn = txn.unwrap_or_else(random_id);
        let response = self.commit(id, vec![body], Batch::new(ops), Some(txn));
        if let Response::Committed { result } = &response {
            self.log(vec![SessionEvent::Write {
                session_id: session,
                client_txn_id: txn,
                epoch: result.epoch,
                reason,
            }]);
        }
        response
    }
    /// `ctx_check` (ADR 0008 §8.5): validate first, re-issue only when needed,
    /// failed issuance maps through the §6.4 table.
    fn check(
        &self,
        session: SessionId,
        cert_id: Option<ContentHash>,
        live: BTreeMap<String, Vec<u8>>,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        let Some(s) = self.session(&session) else {
            return err("UnknownSession", "unknown session");
        };
        let db = &snap.db;
        let link = |c: &Certificate| -> Result<LogLink, Response> {
            Ok(LogLink {
                matches: c.issued_epoch <= snap.epoch
                    && self.checksum(c.issued_epoch)? == c.log_checksum_at_issue,
                current_checksum: self.checksum(snap.epoch)?,
            })
        };
        let validate = |c: &Certificate| -> Result<Validation, Response> {
            db.validate_certificate(c, &live, link(c)?)
                .map_err(|h| err("HistoryUnavailable", format!("{h:?}")))
        };
        let decision = |v: &Validation| match v {
            Validation::Valid | Validation::ValidRecertified { .. } => Decision::Allow,
            _ => Decision::Warn, // P3 never denies (P4 configures deny)
        };
        let mut s = s.lock().expect("session");
        if let Some(id) = cert_id {
            let Some(c) = s.certs.iter().find(|c| c.cert_id == id).cloned() else {
                return err("UnknownRef", "certificate not issued to this session");
            };
            return match validate(&c) {
                Ok(v) => Response::Check {
                    decision: decision(&v),
                    validation: v,
                    cert: Some(Box::new(c)),
                    superseded: Vec::new(),
                },
                Err(r) => r,
            };
        }
        if s.log_broken {
            // The log was cut under this session: nothing it read can be
            // re-certified from its ledger (P3 review finding 2).
            return Response::Check {
                decision: Decision::Warn,
                validation: Validation::Invalid {
                    reason: InvalidReason::LogMismatch,
                    diffs: Vec::new(),
                },
                cert: s.session_cert.clone().map(Box::new),
                superseded: Vec::new(),
            };
        }
        let superseded: Vec<CertDiff> = s
            .superseded
            .drain(..)
            .map(|(logical, old, new)| CertDiff {
                logical,
                cert_rev: Some(old),
                current: Some(new),
                summary: db.summary_at(new, &live).ok(),
            })
            .collect();
        // 1. Validate the existing session certificate, skipping superseded entries.
        let mut since: BTreeMap<(LogicalId, RevisionId), Epoch> = BTreeMap::new();
        let mut strict = false;
        let mut prior: Option<Validation> = None;
        if let Some(current) = s.session_cert.clone() {
            strict = current.strict_no_aba;
            let live_entries: Vec<CertItem> = current
                .entries
                .iter()
                .filter(|e| s.required.get(&e.logical).is_some_and(|l| l.rev == e.rev))
                .cloned()
                .collect();
            for e in &live_entries {
                since.insert((e.logical, e.rev), e.since_epoch);
            }
            let mut filtered = current.clone();
            filtered.entries = live_entries;
            if !filtered.entries.is_empty() {
                let v = match validate(&filtered) {
                    Ok(v) => v,
                    Err(r) => return r,
                };
                if !matches!(v, Validation::Valid | Validation::ValidRecertified { .. }) {
                    return Response::Check {
                        decision: decision(&v),
                        validation: v,
                        cert: Some(Box::new(current)),
                        superseded,
                    };
                }
                // Adopt a recertified certificate as the session certificate.
                let mut adopted = current.clone();
                if let Validation::ValidRecertified { new_cert, .. } = &v {
                    adopted = (**new_cert).clone();
                    s.certs.push(adopted.clone());
                    s.session_cert = Some(adopted.clone());
                    self.log(vec![
                        SessionEvent::CertIssued {
                            cert: Box::new(adopted.clone()),
                        },
                        SessionEvent::SessionCert {
                            session_id: session,
                            cert_id: adopted.cert_id,
                        },
                    ]);
                }
                // Re-issue only when required reads are not covered (ADR 0008 §8.5).
                let covered = s.required.values().all(|e| {
                    adopted
                        .entries
                        .iter()
                        .any(|c| c.required && (c.logical, c.rev) == (e.logical, e.rev))
                });
                if covered {
                    drop(s);
                    if let Err(r) = self.flush() {
                        return r;
                    }
                    return Response::Check {
                        decision: decision(&v),
                        validation: v,
                        cert: Some(Box::new(adopted)),
                        superseded,
                    };
                }
                prior = Some(v);
            }
        }
        // 2. Re-issue over the live required entries.
        let entries: Vec<CertItem> = s
            .required
            .values()
            .map(|e| CertItem {
                logical: e.logical,
                rev: e.rev,
                required: true,
                via: Via::Head,
                since_epoch: since
                    .get(&(e.logical, e.rev))
                    .copied()
                    .unwrap_or(snap.epoch),
            })
            .collect();
        let request = match self.issue_request(session, &s, snap.epoch, strict) {
            Ok(r) => r,
            Err(r) => return r,
        };
        match db.issue_certificate(&request, &entries, &live) {
            Ok(cert) => {
                s.certs.push(cert.clone());
                s.session_cert = Some(cert.clone());
                self.log(vec![
                    SessionEvent::CertIssued {
                        cert: Box::new(cert.clone()),
                    },
                    SessionEvent::SessionCert {
                        session_id: session,
                        cert_id: cert.cert_id,
                    },
                ]);
                drop(s);
                if let Err(r) = self.flush() {
                    return r;
                }
                Response::Check {
                    decision: Decision::Allow,
                    // A recertification found on the way is reported, not hidden.
                    validation: prior.unwrap_or(Validation::Valid),
                    cert: Some(Box::new(cert)),
                    superseded,
                }
            }
            Err(IssueFailure::Empty) => Response::Check {
                decision: Decision::Warn,
                validation: Validation::Empty,
                cert: None,
                superseded,
            },
            Err(_) => {
                // 3. Failed issuance → §6.4 verdict over the same entries.
                let virtual_cert = Certificate {
                    cert_id: [0; 32],
                    session_id: session,
                    principal: request.principal,
                    branch: MAIN,
                    issued_epoch: snap.epoch,
                    log_checksum_at_issue: request.log_checksum,
                    code_commit: request.code_commit.clone(),
                    watermarks: Vec::new(),
                    entries,
                    vector: Vec::new(),
                    strict_no_aba: false,
                };
                match validate(&virtual_cert) {
                    Ok(v) => Response::Check {
                        decision: decision(&v),
                        validation: v,
                        cert: None,
                        superseded,
                    },
                    Err(r) => r,
                }
            }
        }
    }
}
impl Session {
    /// Ledger rule (ADR 0008 §6.2): a required read of another revision of a
    /// logical supersedes the earlier one; advisory reads never downgrade.
    fn record(&mut self, e: LedgerEntry) {
        if !e.required {
            return;
        }
        if let Some(old) = self.required.get(&e.logical)
            && old.rev != e.rev
        {
            self.superseded.push((e.logical, old.rev, e.rev));
        }
        self.required.insert(e.logical, e);
    }
}
fn read_event(session: SessionId, e: &LedgerEntry) -> SessionEvent {
    SessionEvent::Read {
        session_id: session,
        logical: e.logical,
        rev: e.rev,
        layer: Layer::L2,
        epoch: e.epoch,
        required: e.required,
    }
}
/// The session-log writer: one FIFO queue, one synced group per drain, so at
/// most one group is ever unsynced (ADR 0008 §4).
/// Session-log writer (ADR 0008 §4): syncs at once when a durable event
/// (`SessionStart`, `CertIssued`, `SessionCert`, `SessionEnd`), a flush or a
/// stop is pending, and otherwise no later than [`LAZY_SYNC`] after the
/// oldest unsynced event, however steadily events keep arriving (P3 review
/// finding 6). Read events never put a full sync in the way of the op_log.
fn writer<D: Durability>(mut log: SessionLog<D>, rx: mpsc::Receiver<Writer>) {
    let mut p = Pending::default();
    loop {
        let first = match p.since {
            None => rx.recv().ok(),
            Some(t) => match rx.recv_timeout(LAZY_SYNC.saturating_sub(t.elapsed())) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = log.append_group(&p.events);
                    return;
                }
            },
        };
        let disconnected = first.is_none() && p.since.is_none();
        if let Some(m) = first {
            p.take(m);
        }
        // Drain what is already queued, bounded so a steady stream cannot
        // postpone the sync.
        for _ in 0..DRAIN_MAX {
            match rx.try_recv() {
                Ok(m) => p.take(m),
                Err(_) => break,
            }
        }
        let due = p.since.is_some_and(|t| t.elapsed() >= LAZY_SYNC);
        if !(disconnected || due || p.urgent()) {
            continue;
        }
        let result = log.append_group(&std::mem::take(&mut p.events));
        (p.durable, p.since) = (false, None);
        for w in p.waiters.drain(..) {
            let _ = w.send(result.clone());
        }
        if let Some(s) = p.stop.take() {
            let _ = s.send(());
            return;
        }
        if disconnected {
            return;
        }
    }
}
/// What the session-log writer holds between syncs.
#[derive(Default)]
struct Pending {
    events: Vec<SessionEvent>,
    waiters: Vec<mpsc::Sender<Result<(), StoreError>>>,
    /// A durable event is among `events`.
    durable: bool,
    stop: Option<mpsc::Sender<()>>,
    /// When the oldest unsynced event arrived.
    since: Option<std::time::Instant>,
}
impl Pending {
    fn take(&mut self, m: Writer) {
        match m {
            Writer::Events(e) => {
                self.since.get_or_insert_with(std::time::Instant::now);
                self.durable |= e.iter().any(|e| {
                    matches!(
                        e,
                        SessionEvent::Start { .. }
                            | SessionEvent::CertIssued { .. }
                            | SessionEvent::SessionCert { .. }
                            | SessionEvent::LogBroken { .. }
                            | SessionEvent::End { .. }
                    )
                });
                self.events.extend(e);
            }
            Writer::Flush(w) => self.waiters.push(w),
            Writer::Stop(s) => self.stop = Some(s),
        }
    }
    fn urgent(&self) -> bool {
        self.durable || self.stop.is_some() || !self.waiters.is_empty()
    }
}
const DRAIN_MAX: usize = 4_096;
const LAZY_SYNC: std::time::Duration = std::time::Duration::from_millis(100);

/// Keeps the recall projection up to date with published snapshots: the
/// logicals whose heads moved since the indexed epoch are reindexed (full
/// rebuild when the projection is ahead of the log or empty).
fn indexer<D: Durability>(
    actor: ActorHandle<D>,
    recall: Arc<Mutex<RecallIndex>>,
    stop: Arc<AtomicBool>,
) {
    let text = |snap: &Snapshot<D>,
                l: LogicalId|
     -> Option<(LogicalId, String, RecordKind, String, String)> {
        let db = &snap.db;
        let (key, kind, scope) = db.logical_info(l)?;
        let content = db.revision_content(db.head(l)?).ok()?;
        let body = match snap.log.read_blob(&content) {
            Payload::Available(b) => {
                let mut s = String::from_utf8_lossy(&b).into_owned();
                s.truncate(64 * 1024);
                s
            }
            Payload::Unavailable(_) => String::new(),
        };
        Some((l, key, kind, scope, body))
    };
    while !stop.load(Ordering::Acquire) {
        let snap = actor.snapshot();
        let indexed = recall
            .lock()
            .expect("recall")
            .watermark()
            .unwrap_or(Epoch(0));
        if indexed == snap.epoch {
            // Never hold a snapshot while idle: it would keep the actor from
            // reclaiming that replica (forcing a clone per group, ADR 0008 §3.3).
            drop(snap);
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
        let db = &snap.db;
        let touched: Vec<LogicalId> = if indexed > snap.epoch || indexed.0 == 0 {
            let _ = recall.lock().expect("recall").clear();
            (0..db.logical_count() as u32).map(LogicalId).collect()
        } else {
            let mut t = std::collections::BTreeSet::new();
            for e in indexed.0 + 1..=snap.epoch.0 {
                if let Ok(row) = snap.log.read_row(Epoch(e))
                    && let Ok(result) = row.result()
                {
                    t.extend(result.net_delta.head_moves.iter().map(|(l, _, _)| *l));
                }
            }
            t.into_iter().collect()
        };
        let entries: Vec<_> = touched.into_iter().filter_map(|l| text(&snap, l)).collect();
        let epoch = snap.epoch;
        drop(snap);
        let _ = recall.lock().expect("recall").update(epoch, &entries);
    }
}
