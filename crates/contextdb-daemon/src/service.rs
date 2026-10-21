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
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
/// The root agent is `None`; subagents are keyed by their harness agent id
/// (ADR 0010 §7).
type Agent = Option<String>;
/// One agent's part of a session (ADR 0010 §7): its own reads, its session
/// certificate and what was last reported to it.
#[derive(Default)]
struct AgentState {
    /// Live required read per logical (a newer read by this agent
    /// supersedes its older one).
    required: BTreeMap<LogicalId, LedgerEntry>,
    /// Supersessions not yet reported by `ctx_check`: (logical, old, new).
    superseded: Vec<(LogicalId, RevisionId, RevisionId)>,
    session_cert: Option<Certificate>,
    /// Last `(head, summary)` reported per logical in this agent's view.
    reported: BTreeMap<LogicalId, (Option<RevisionId>, Summary)>,
    /// The batch of the last `Updates` peek, until acknowledged.
    pending: Option<(u64, Vec<UpdateItem>)>,
    /// Paths of this agent's edit, write, notebook and patch actions.
    declared: BTreeSet<String>,
    shell_actions: u32,
    writes: u32,
    writes_with_deps: u32,
}
struct Session {
    principal: PrincipalId,
    head_sha: String,
    repo: String,
    started_epoch: Epoch,
    ended_epoch: Option<Epoch>,
    agents: BTreeMap<Agent, AgentState>,
    /// Agent ids hooks have reported for this session; any other id is
    /// treated as the root (ADR 0010 §2.2).
    known_agents: BTreeSet<String>,
    /// Certificates issued to this session (recall and session certificates).
    certs: Vec<Certificate>,
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
/// Hook-related daemon state (ADR 0010 §2, §5, §10).
#[derive(Default)]
struct Hooks {
    /// Latest session per harness session key (live or ended).
    keys: BTreeMap<(String, String), SessionId>,
    /// Claude Code MCP tool-use id → (session, agent). Every request of
    /// that tool call resolves through it; dropped at session end, oldest
    /// first beyond [`CLAIMS_KEPT`].
    claims: BTreeMap<String, (SessionId, Agent)>,
    claim_order: VecDeque<String>,
    /// Outbox stream high-water marks.
    streams: BTreeMap<[u8; 16], u64>,
    /// Recently ended sessions, for late facts and concurrent-drift.
    ended: VecDeque<(SessionId, Arc<Mutex<Session>>)>,
    telemetry: Vec<TelemetryEntry>,
}
/// Claims kept across all sessions (ADR 0010 §2.3).
const CLAIMS_KEPT: usize = 65_536;
/// Ended sessions kept for late outbox facts (ADR 0010 §5.3).
const ENDED_KEPT: usize = 256;
pub struct Service<D: Durability> {
    pub actor: ActorHandle<D>,
    sessions: Mutex<BTreeMap<SessionId, Arc<Mutex<Session>>>>,
    hooks: Mutex<Hooks>,
    /// Serialises outbox deliveries (stream marks) across sweeper and hooks.
    facts_lock: Mutex<()>,
    tokens: AtomicU64,
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
/// A resolved binding: session id, session, agent, and whether the session
/// was just started.
type Bound = (SessionId, Arc<Mutex<Session>>, Agent, bool);
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
            hooks: Mutex::new(Hooks::default()),
            facts_lock: Mutex::new(()),
            tokens: AtomicU64::new(1),
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
        let mut hooks = self.hooks.lock().expect("hooks");
        let broken = |map: &BTreeMap<SessionId, Arc<Mutex<Session>>>, id: &SessionId| {
            if let Some(s) = map.get(id) {
                s.lock().expect("session").log_broken = true;
            }
        };
        // Live or recently ended: late facts and drift reach ended sessions.
        let find =
            |map: &BTreeMap<SessionId, Arc<Mutex<Session>>>, hooks: &Hooks, id: &SessionId| {
                map.get(id).cloned().or_else(|| {
                    hooks
                        .ended
                        .iter()
                        .find(|(e, _)| e == id)
                        .map(|(_, s)| s.clone())
                })
            };
        for e in events {
            match e {
                SessionEvent::Start {
                    session_id,
                    principal,
                    head_sha,
                    repo,
                    started_epoch,
                    log_checksum,
                    ..
                } => {
                    let log_broken = started_epoch > tail
                        || snap.log.checksum_at(started_epoch).ok() != Some(log_checksum);
                    let mut session = Session::new(principal, head_sha, repo, started_epoch);
                    session.log_broken = log_broken;
                    map.insert(session_id, Arc::new(Mutex::new(session)));
                }
                SessionEvent::Read {
                    session_id,
                    agent,
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
                        s.lock()
                            .expect("session")
                            .agent(&agent)
                            .record(LedgerEntry {
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
                    agent,
                    cert_id,
                } => {
                    if let Some(s) = map.get(&session_id) {
                        let mut s = s.lock().expect("session");
                        let cert = s.certs.iter().find(|c| c.cert_id == cert_id).cloned();
                        s.agent(&agent).session_cert = cert;
                    }
                }
                SessionEvent::End { session_id, epoch } => {
                    if let Some(s) = map.remove(&session_id) {
                        s.lock().expect("session").ended_epoch = Some(epoch);
                        hooks.end(session_id, s);
                    }
                }
                SessionEvent::Write {
                    session_id,
                    agent,
                    epoch,
                    with_deps,
                    ..
                } => {
                    if let Some(s) = find(&map, &hooks, &session_id) {
                        s.lock().expect("session").count_write(&agent, with_deps);
                    }
                    hooks.telemetry.push(TelemetryEntry::Write {
                        session: session_id,
                        agent,
                        with_deps,
                        epoch,
                    });
                }
                SessionEvent::SessionKey {
                    session_id,
                    harness,
                    harness_session,
                } => {
                    hooks.keys.insert((harness, harness_session), session_id);
                }
                SessionEvent::Fact {
                    session_id, packet, ..
                } => {
                    if let Some(s) = find(&map, &hooks, &session_id) {
                        s.lock().expect("session").note_fact(&packet);
                    }
                    if let Fact::Gate {
                        action,
                        mode,
                        decision,
                        harness_version,
                    } = packet.fact
                    {
                        hooks.telemetry.push(TelemetryEntry::Gate {
                            session: session_id,
                            agent: packet.agent,
                            action,
                            mode,
                            verdict: "Unavailable".into(),
                            decision,
                            available: false,
                            harness_version,
                            epoch: tail,
                        });
                    }
                }
                SessionEvent::Gate {
                    session_id,
                    agent,
                    action,
                    mode,
                    verdict,
                    decision,
                    harness_version,
                    epoch,
                } => hooks.telemetry.push(TelemetryEntry::Gate {
                    session: session_id,
                    agent,
                    action,
                    mode,
                    verdict,
                    decision,
                    available: true,
                    harness_version,
                    epoch,
                }),
                SessionEvent::Drift {
                    session_id,
                    agent,
                    report,
                    ..
                } => hooks.telemetry.push(TelemetryEntry::Drift {
                    session: session_id,
                    agent,
                    report,
                }),
                SessionEvent::Reported {
                    session_id,
                    agent,
                    items,
                } => {
                    if let Some(s) = map.get(&session_id) {
                        let mut s = s.lock().expect("session");
                        s.agent(&agent).reported.extend(items);
                    }
                }
                SessionEvent::StreamMark {
                    stream,
                    applied_through,
                    skipped,
                } => {
                    hooks.streams.insert(stream, applied_through);
                    if let Some((from, to)) = skipped {
                        hooks
                            .telemetry
                            .push(TelemetryEntry::Skip { stream, from, to });
                    }
                }
                SessionEvent::Unclaimed { epoch } => {
                    hooks.telemetry.push(TelemetryEntry::Unclaimed { epoch })
                }
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
                    Arc::new(Mutex::new(Session::new(
                        id.principal,
                        head_sha.clone(),
                        repo.clone(),
                        snap.epoch,
                    ))),
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
                    if let Some(s) = sessions.remove(&session) {
                        s.lock().expect("session").ended_epoch = Some(snap.epoch);
                        self.hooks.lock().expect("hooks").end(session, s);
                    }
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
                binding,
                query,
                scope,
                mode,
                limit,
            } => match self.bind(id, &binding, &snap) {
                Ok(b) => self.recall(b, &query, &scope, mode, limit as usize, &snap),
                Err(r) => r,
            },
            Request::Read {
                binding,
                target,
                mode,
            } => match self.bind(id, &binding, &snap) {
                Ok(b) => self.read(b, &target, mode, &snap),
                Err(r) => r,
            },
            Request::Record {
                binding,
                kind,
                body,
                target,
                deps,
                adopt_as_base,
                client_txn_id,
            } => match self.bind(id, &binding, &snap) {
                Ok(b) => self.record(
                    id,
                    b,
                    kind,
                    body,
                    target,
                    deps,
                    adopt_as_base,
                    client_txn_id,
                    snap,
                ),
                Err(r) => r,
            },
            Request::Supersede {
                binding,
                logical,
                body,
                expected_head,
                adopt_as_base,
                carry_rules,
                reason,
                client_txn_id,
            } => match self.bind(id, &binding, &snap) {
                Ok(b) => self.supersede(
                    id,
                    b,
                    logical,
                    body,
                    expected_head,
                    adopt_as_base,
                    carry_rules,
                    reason,
                    client_txn_id,
                    snap,
                ),
                Err(r) => r,
            },
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
                binding,
                cert_id,
                live_pos,
                action,
                mode,
                harness_version,
            } => match self.bind(id, &binding, &snap) {
                Ok(b) => self.check_gated(
                    b,
                    cert_id,
                    live_pos.into_iter().collect(),
                    action,
                    mode,
                    harness_version,
                    &snap,
                ),
                Err(r) => r,
            },
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
            Request::SessionAttach {
                harness,
                harness_session,
                repo,
                head_sha,
            } => match self.attach(id, harness, harness_session, repo, head_sha, &snap) {
                Ok((session, _, _, created)) => Response::Attached { session, created },
                Err(r) => r,
            },
            Request::Claim {
                session,
                agent,
                call,
            } => {
                let Some(s) = self.session(&session) else {
                    return err("UnknownSession", "unknown session");
                };
                if let Some(a) = &agent {
                    s.lock().expect("session").known_agents.insert(a.clone());
                }
                let mut hooks = self.hooks.lock().expect("hooks");
                if hooks
                    .claims
                    .insert(call.clone(), (session, agent))
                    .is_none()
                {
                    hooks.claim_order.push_back(call);
                }
                while hooks.claim_order.len() > CLAIMS_KEPT {
                    if let Some(old) = hooks.claim_order.pop_front() {
                        hooks.claims.remove(&old);
                    }
                }
                Response::Done
            }
            Request::Updates {
                binding,
                ack,
                max_items,
            } => match self.bind(id, &binding, &snap) {
                Ok(b) => self.updates(b, ack, max_items as usize, &snap),
                Err(r) => r,
            },
            Request::Facts {
                harness,
                harness_session,
                stream,
                packets,
                skip,
            } => self.facts(id, harness, harness_session, stream, packets, skip, &snap),
            Request::Briefing {
                binding,
                budget_chars,
            } => match self.bind(id, &binding, &snap) {
                Ok(_) => self.briefing(budget_chars as usize, &snap),
                Err(r) => r,
            },
            Request::Telemetry { since, limit } => {
                if id.channel != Channel::Admin {
                    return err("ChannelRestricted", "telemetry requires the admin socket");
                }
                let hooks = self.hooks.lock().expect("hooks");
                let from = (since as usize).min(hooks.telemetry.len());
                let to = from
                    .saturating_add(limit as usize)
                    .min(hooks.telemetry.len());
                Response::Telemetry {
                    entries: hooks.telemetry[from..to].to_vec(),
                    next: to as u64,
                }
            }
        }
    }
    /// The live session of a harness session key, started (durably, with
    /// its key) if there is none (ADR 0010 §2.1).
    fn attach(
        &self,
        id: Identity,
        harness: String,
        harness_session: String,
        repo: String,
        head_sha: String,
        snap: &Arc<Snapshot<D>>,
    ) -> Result<Bound, Response> {
        let key = (harness.clone(), harness_session.clone());
        let session_id = {
            // Held across lookup and insert so concurrent attaches of one key
            // start one session.
            let mut hooks = self.hooks.lock().expect("hooks");
            if let Some(sid) = hooks.keys.get(&key).copied()
                && let Some(s) = self.session(&sid)
            {
                // A session an MCP call started has no repository yet; the
                // first hook supplies it (drift compares repositories).
                {
                    let mut st = s.lock().expect("session");
                    if st.repo.is_empty() && !repo.is_empty() {
                        st.repo = repo;
                        st.head_sha = head_sha;
                    }
                }
                return Ok((sid, s, None, false));
            }
            let session_id = random_id();
            hooks.keys.insert(key.clone(), session_id);
            session_id
        };
        let undo = |this: &Self| {
            this.sessions.lock().expect("sessions").remove(&session_id);
            let mut hooks = this.hooks.lock().expect("hooks");
            if hooks.keys.get(&key) == Some(&session_id) {
                hooks.keys.remove(&key);
            }
        };
        let log_checksum = match self.checksum(snap.epoch) {
            Ok(c) => c,
            Err(r) => {
                undo(self);
                return Err(r);
            }
        };
        let session = Arc::new(Mutex::new(Session::new(
            id.principal,
            head_sha.clone(),
            repo.clone(),
            snap.epoch,
        )));
        self.sessions
            .lock()
            .expect("sessions")
            .insert(session_id, session.clone());
        self.log(vec![
            SessionEvent::Start {
                session_id,
                parent: None,
                harness: harness.clone(),
                repo,
                branch: MAIN,
                head_sha,
                principal: id.principal,
                channel: id.channel,
                started_epoch: snap.epoch,
                log_checksum,
            },
            SessionEvent::SessionKey {
                session_id,
                harness,
                harness_session,
            },
        ]);
        if let Err(r) = self.flush() {
            undo(self);
            return Err(r);
        }
        Ok((session_id, session, None, true))
    }
    /// Resolves a binding to (session, agent) (ADR 0010 §2).
    fn bind(&self, id: Identity, b: &Binding, snap: &Arc<Snapshot<D>>) -> Result<Bound, Response> {
        match b {
            Binding::Session { session, agent } => {
                let Some(s) = self.session(session) else {
                    return Err(err("UnknownSession", "unknown session"));
                };
                if let Some(a) = agent {
                    s.lock().expect("session").known_agents.insert(a.clone());
                }
                Ok((*session, s, agent.clone(), false))
            }
            Binding::Key {
                harness,
                harness_session,
                agent,
            } => {
                let (sid, s, _, created) = self.attach(
                    id,
                    harness.clone(),
                    harness_session.clone(),
                    String::new(),
                    String::new(),
                    snap,
                )?;
                // Only ids a hook reported are children; any other is the
                // root (ADR 0010 §2.2).
                let agent = agent
                    .clone()
                    .filter(|a| s.lock().expect("session").known_agents.contains(a));
                Ok((sid, s, agent, created))
            }
            Binding::Call { id: call } => {
                let claimed = self.hooks.lock().expect("hooks").claims.get(call).cloned();
                match claimed {
                    Some((sid, agent)) => match self.session(&sid) {
                        Some(s) => Ok((sid, s, agent, false)),
                        None => Err(err("UnknownSession", "the claiming session ended")),
                    },
                    None => {
                        self.hooks
                            .lock()
                            .expect("hooks")
                            .telemetry
                            .push(TelemetryEntry::Unclaimed { epoch: snap.epoch });
                        self.log(vec![SessionEvent::Unclaimed { epoch: snap.epoch }]);
                        Err(err("Unclaimed", "no hook claimed this tool call"))
                    }
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
        (session, s, agent, _): Bound,
        query: &str,
        scope: &str,
        mode: Mode,
        limit: usize,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
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
            events.push(read_event(session, &agent, &entry));
            s.agent(&agent).record(entry);
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
        (session, s, agent, _): Bound,
        target: &Target,
        mode: Mode,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
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
            s.agent(&agent).record(entry.clone());
            self.log(vec![read_event(session, &agent, &entry)]);
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
        (session, s, agent, _): Bound,
        kind: RecordKind,
        body: Vec<u8>,
        target: WriteTarget,
        deps: Vec<Dep>,
        adopt_as_base: bool,
        txn: Option<[u8; 16]>,
        snap: Arc<Snapshot<D>>,
    ) -> Response {
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
        let with_deps = !incidences.is_empty();
        if with_deps {
            ops.push(Op::AddRule {
                conclusion: RevRef::New(0),
                incidences,
                enabled: true,
            });
        }
        let txn = txn.unwrap_or_else(random_id);
        let response = self.commit(id, vec![body], Batch::new(ops), Some(txn));
        if let Response::Committed { result } = &response {
            self.wrote(
                session,
                &s,
                &agent,
                txn,
                result.epoch,
                "record".into(),
                with_deps,
            );
        }
        response
    }
    /// Ties a committed write to its session and agent (ADR 0010 §4.3, §10).
    #[allow(clippy::too_many_arguments)]
    fn wrote(
        &self,
        session: SessionId,
        s: &Arc<Mutex<Session>>,
        agent: &Agent,
        txn: [u8; 16],
        epoch: Epoch,
        reason: String,
        with_deps: bool,
    ) {
        s.lock().expect("session").count_write(agent, with_deps);
        self.hooks
            .lock()
            .expect("hooks")
            .telemetry
            .push(TelemetryEntry::Write {
                session,
                agent: agent.clone(),
                with_deps,
                epoch,
            });
        self.log(vec![SessionEvent::Write {
            session_id: session,
            agent: agent.clone(),
            client_txn_id: txn,
            epoch,
            reason,
            with_deps,
        }]);
    }
    #[allow(clippy::too_many_arguments)]
    fn supersede(
        &self,
        id: Identity,
        (session, s, agent, _): Bound,
        logical: LogicalId,
        body: Vec<u8>,
        expected_head: RevisionId,
        adopt_as_base: bool,
        carry_rules: bool,
        reason: String,
        txn: Option<[u8; 16]>,
        snap: Arc<Snapshot<D>>,
    ) -> Response {
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
        let with_deps = ops
            .iter()
            .any(|op| matches!(op, Op::AddRule { incidences, .. } if !incidences.is_empty()));
        let txn = txn.unwrap_or_else(random_id);
        let response = self.commit(id, vec![body], Batch::new(ops), Some(txn));
        if let Response::Committed { result } = &response {
            self.wrote(session, &s, &agent, txn, result.epoch, reason, with_deps);
        }
        response
    }
    /// `ctx_check` (ADR 0008 §8.5): validate first, re-issue only when needed,
    /// failed issuance maps through the §6.4 table.
    fn check(
        &self,
        (session, s, agent, _): Bound,
        cert_id: Option<ContentHash>,
        live: BTreeMap<String, Vec<u8>>,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
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
                    gate: None,
                    updates_pending: 0,
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
                cert: s
                    .agents
                    .get(&agent)
                    .and_then(|a| a.session_cert.clone())
                    .map(Box::new),
                superseded: Vec::new(),
                gate: None,
                updates_pending: 0,
            };
        }
        let drained: Vec<_> = s.agent(&agent).superseded.drain(..).collect();
        let view = s.view(&agent);
        let superseded: Vec<CertDiff> = drained
            .into_iter()
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
        if let Some(current) = s.agents.get(&agent).and_then(|a| a.session_cert.clone()) {
            strict = current.strict_no_aba;
            let live_entries: Vec<CertItem> = current
                .entries
                .iter()
                .filter(|e| view.get(&e.logical).is_some_and(|l| l.rev == e.rev))
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
                        gate: None,
                        updates_pending: 0,
                    };
                }
                // Adopt a recertified certificate as the session certificate.
                let mut adopted = current.clone();
                if let Validation::ValidRecertified { new_cert, .. } = &v {
                    adopted = (**new_cert).clone();
                    s.certs.push(adopted.clone());
                    s.agent(&agent).session_cert = Some(adopted.clone());
                    self.log(vec![
                        SessionEvent::CertIssued {
                            cert: Box::new(adopted.clone()),
                        },
                        SessionEvent::SessionCert {
                            session_id: session,
                            agent: agent.clone(),
                            cert_id: adopted.cert_id,
                        },
                    ]);
                }
                // Re-issue only when required reads are not covered (ADR 0008 §8.5).
                let covered = view.values().all(|e| {
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
                        gate: None,
                        updates_pending: 0,
                    };
                }
                prior = Some(v);
            }
        }
        // 2. Re-issue over the live required entries.
        let entries: Vec<CertItem> = view
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
                s.agent(&agent).session_cert = Some(cert.clone());
                self.log(vec![
                    SessionEvent::CertIssued {
                        cert: Box::new(cert.clone()),
                    },
                    SessionEvent::SessionCert {
                        session_id: session,
                        agent: agent.clone(),
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
                    gate: None,
                    updates_pending: 0,
                }
            }
            Err(IssueFailure::Empty) => Response::Check {
                decision: Decision::Warn,
                validation: Validation::Empty,
                cert: None,
                superseded,
                gate: None,
                updates_pending: 0,
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
                        gate: None,
                        updates_pending: 0,
                    },
                    Err(r) => r,
                }
            }
        }
    }
    /// `ctx_check` plus the gate (ADR 0010 §6): with a mode the daemon applies
    /// the shared predicate and logs the decision; the pending update count
    /// is reported without consuming anything (§4.4).
    #[allow(clippy::too_many_arguments)]
    fn check_gated(
        &self,
        bound: Bound,
        cert_id: Option<ContentHash>,
        live: BTreeMap<String, Vec<u8>>,
        action: Option<Action>,
        mode: Option<GateMode>,
        harness_version: String,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        let (session, s, agent, _) = bound.clone();
        let mut response = self.check(bound, cert_id, live, snap);
        if let Response::Check {
            validation,
            gate,
            updates_pending,
            ..
        } = &mut response
        {
            *updates_pending = {
                let s = s.lock().expect("session");
                update_items(&s, &agent, &snap.db).len() as u32
            };
            if let Some(mode) = mode {
                let decision = contextdb_proto::gate::gate(mode, validation);
                *gate = Some(decision);
                let action = action.unwrap_or(Action {
                    tool: ToolClass::Other("unknown".into()),
                    paths: Vec::new(),
                    command: None,
                });
                let verdict = contextdb_proto::gate::verdict_code(validation).to_owned();
                self.hooks
                    .lock()
                    .expect("hooks")
                    .telemetry
                    .push(TelemetryEntry::Gate {
                        session,
                        agent: agent.clone(),
                        action: action.clone(),
                        mode,
                        verdict: verdict.clone(),
                        decision,
                        available: true,
                        harness_version: harness_version.clone(),
                        epoch: snap.epoch,
                    });
                self.log(vec![SessionEvent::Gate {
                    session_id: session,
                    agent,
                    action,
                    mode,
                    verdict,
                    decision,
                    harness_version,
                    epoch: snap.epoch,
                }]);
            }
        }
        response
    }
    /// `context_update` peek/ack (ADR 0010 §4.4).
    fn updates(
        &self,
        (session, s, agent, _): Bound,
        ack: Option<u64>,
        max_items: usize,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        let mut s = s.lock().expect("session");
        if let Some(token) = ack {
            let st = s.agent(&agent);
            if st.pending.as_ref().is_some_and(|(t, _)| *t == token) {
                let (_, items) = st.pending.take().expect("pending");
                let items: Vec<_> = items
                    .into_iter()
                    .map(|i| (i.logical, (i.current, i.summary)))
                    .collect();
                st.reported.extend(items.iter().cloned());
                self.log(vec![SessionEvent::Reported {
                    session_id: session,
                    agent: agent.clone(),
                    items,
                }]);
            }
        }
        let mut items = update_items(&s, &agent, &snap.db);
        let max = if max_items == 0 { 50 } else { max_items };
        let more = items.len() > max;
        items.truncate(max);
        let token = self.tokens.fetch_add(1, Ordering::Relaxed);
        s.agent(&agent).pending = Some((token, items.clone()));
        Response::Updates { token, items, more }
    }
    /// Outbox delivery (ADR 0010 §5.3): in seq order, idempotent per stream,
    /// durable before the reply.
    #[allow(clippy::too_many_arguments)]
    fn facts(
        &self,
        id: Identity,
        harness: String,
        harness_session: String,
        stream: [u8; 16],
        packets: Vec<Packet>,
        skip: Option<(u64, u64)>,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        // Facts of an ended session still apply to it (§5.3).
        let known = {
            let hooks = self.hooks.lock().expect("hooks");
            hooks
                .keys
                .get(&(harness.clone(), harness_session.clone()))
                .copied()
                .and_then(|sid| {
                    self.session(&sid)
                        .or_else(|| {
                            hooks
                                .ended
                                .iter()
                                .find(|(e, _)| *e == sid)
                                .map(|(_, s)| s.clone())
                        })
                        .map(|s| (sid, s))
                })
        };
        let (session, s) = match known {
            Some(v) => v,
            None => match self.attach(
                id,
                harness,
                harness_session,
                String::new(),
                String::new(),
                snap,
            ) {
                Ok((sid, s, _, _)) => (sid, s),
                Err(r) => return r,
            },
        };
        let _serial = self.facts_lock.lock().expect("facts");
        let mut through = self
            .hooks
            .lock()
            .expect("hooks")
            .streams
            .get(&stream)
            .copied()
            .unwrap_or(0);
        let mut skipped = None;
        if let Some((from, to)) = skip {
            if from > through + 1 {
                return Response::Gap {
                    expected: through + 1,
                };
            }
            if to > through {
                skipped = Some((through + 1, to));
                through = to;
            }
        }
        let mut events = Vec::new();
        let mut telemetry = Vec::new();
        let mut drift = Vec::new();
        for p in packets {
            if p.seq <= through {
                continue;
            }
            if p.seq != through + 1 {
                if events.is_empty() && skipped.is_none() {
                    return Response::Gap {
                        expected: through + 1,
                    };
                }
                break;
            }
            match &p.fact {
                Fact::Drift { done } => {
                    let report = self.drift(session, &s, &p.agent, done.as_deref());
                    drift.push(report.clone());
                    telemetry.push(TelemetryEntry::Drift {
                        session,
                        agent: p.agent.clone(),
                        report: report.clone(),
                    });
                    events.push(SessionEvent::Drift {
                        session_id: session,
                        agent: p.agent.clone(),
                        report,
                        epoch: snap.epoch,
                    });
                }
                Fact::Gate {
                    action,
                    mode,
                    decision,
                    harness_version,
                } => telemetry.push(TelemetryEntry::Gate {
                    session,
                    agent: p.agent.clone(),
                    action: action.clone(),
                    mode: *mode,
                    verdict: "Unavailable".into(),
                    decision: *decision,
                    available: false,
                    harness_version: harness_version.clone(),
                    epoch: snap.epoch,
                }),
                _ => {}
            }
            s.lock().expect("session").note_fact(&p);
            through = p.seq;
            events.push(SessionEvent::Fact {
                session_id: session,
                stream,
                packet: p,
                epoch: snap.epoch,
            });
        }
        if let Some((from, to)) = skipped {
            telemetry.push(TelemetryEntry::Skip { stream, from, to });
        }
        events.push(SessionEvent::StreamMark {
            stream,
            applied_through: through,
            skipped,
        });
        self.log(events);
        if let Err(r) = self.flush() {
            return r;
        }
        let mut hooks = self.hooks.lock().expect("hooks");
        hooks.streams.insert(stream, through);
        hooks.telemetry.extend(telemetry);
        Response::Facts {
            applied_through: through,
            drift,
        }
    }
    /// Declared-vs-done drift of one agent (ADR 0010 §5.4). Paths declared by
    /// any agent of this session or by another session of the same repository
    /// active at the same time are not undeclared.
    fn drift(
        &self,
        session: SessionId,
        s: &Arc<Mutex<Session>>,
        agent: &Agent,
        done: Option<&[String]>,
    ) -> DriftReport {
        let (repo, started) = {
            let s = s.lock().expect("session");
            (s.repo.clone(), s.started_epoch)
        };
        let others: Vec<Arc<Mutex<Session>>> = {
            let live = self.sessions.lock().expect("sessions");
            let hooks = self.hooks.lock().expect("hooks");
            live.iter()
                .chain(hooks.ended.iter().map(|(id, s)| (id, s)))
                .filter(|(id, _)| **id != session)
                .map(|(_, s)| s.clone())
                .collect()
        };
        let mut declared = BTreeSet::new();
        for o in others {
            let o = o.lock().expect("session");
            if o.repo == repo && o.ended_epoch.is_none_or(|e| e >= started) {
                declared.extend(o.declared());
            }
        }
        let s = s.lock().expect("session");
        declared.extend(s.declared());
        let own = s.agents.get(agent);
        let mut report = DriftReport {
            shell_actions: own.map_or(0, |a| a.shell_actions),
            writes: own.map_or(0, |a| a.writes),
            writes_with_deps: own.map_or(0, |a| a.writes_with_deps),
            ..DriftReport::default()
        };
        match done {
            None => report.done_unknown = true,
            Some(done) => {
                let done: BTreeSet<&String> = done.iter().collect();
                report.undeclared = done
                    .iter()
                    .filter(|p| !declared.contains(**p))
                    .map(|p| (*p).clone())
                    .collect();
                report.unrealised = own
                    .into_iter()
                    .flat_map(|a| a.declared.iter())
                    .filter(|p| !done.contains(p))
                    .cloned()
                    .collect();
            }
        }
        report
    }
    /// Advisory SessionStart briefing (ADR 0010 §4.5): adds no ledger entry.
    fn briefing(&self, budget: usize, snap: &Arc<Snapshot<D>>) -> Response {
        let db = &snap.db;
        let (mut stale, mut pending, mut conflicted) = (0u32, 0u32, 0u32);
        for l in (0..db.logical_count() as u32).map(LogicalId) {
            let Some(r) = db.head(l) else { continue };
            match db.summary_at(r, &BTreeMap::new()) {
                Ok(Summary::Stale(_)) => stale += 1,
                Ok(Summary::Pending(_)) => pending += 1,
                Ok(Summary::Conflicted) => conflicted += 1,
                _ => {}
            }
        }
        // The most recently moved heads first.
        let mut seen = BTreeSet::new();
        let mut items = Vec::new();
        let mut used = 0usize;
        let floor = db.initial_epoch().0.max(1);
        'rows: for e in (floor..=snap.epoch.0).rev().take(256) {
            let Ok(row) = snap.log.read_row(Epoch(e)) else {
                continue;
            };
            let Ok(result) = row.result() else { continue };
            for (l, _, _) in &result.net_delta.head_moves {
                if !seen.insert(*l) {
                    continue;
                }
                let Some(r) = db.head(*l) else { continue };
                let item = self.item(db, *l, r, None);
                used += item.key.len() + 48;
                if used > budget {
                    break 'rows;
                }
                items.push(item);
            }
        }
        Response::Briefing {
            stale,
            pending,
            conflicted,
            items,
        }
    }
}
impl AgentState {
    /// Ledger rule (ADR 0008 §6.2): a required read of another revision of a
    /// logical supersedes this agent's earlier one; advisory reads never
    /// downgrade.
    fn record(&mut self, e: LedgerEntry) {
        if !e.required {
            return;
        }
        if let Some(old) = self.required.get(&e.logical)
            && old.rev != e.rev
        {
            self.superseded.push((e.logical, old.rev, e.rev));
            // A re-read restarts what this agent was told about the logical.
            self.reported.remove(&e.logical);
        }
        self.required.insert(e.logical, e);
    }
}
impl Session {
    fn new(principal: PrincipalId, head_sha: String, repo: String, started: Epoch) -> Self {
        Self {
            principal,
            head_sha,
            repo,
            started_epoch: started,
            ended_epoch: None,
            agents: BTreeMap::new(),
            known_agents: BTreeSet::new(),
            certs: Vec::new(),
            log_broken: false,
            broken_durable: false,
        }
    }
    /// The state of `agent`, created on first use; a named agent becomes
    /// known to the session.
    fn agent(&mut self, agent: &Agent) -> &mut AgentState {
        if let Some(a) = agent {
            self.known_agents.insert(a.clone());
        }
        self.agents.entry(agent.clone()).or_default()
    }
    fn count_write(&mut self, agent: &Agent, with_deps: bool) {
        let a = self.agent(agent);
        a.writes += 1;
        a.writes_with_deps += u32::from(with_deps);
    }
    /// Effects of a delivered outbox fact on the session (write set).
    fn note_fact(&mut self, p: &Packet) {
        if let Fact::Action { action, ok: true } = &p.fact {
            let a = self.agent(&p.agent);
            match action.tool {
                ToolClass::Shell => a.shell_actions += 1,
                ToolClass::Edit | ToolClass::Write | ToolClass::Notebook | ToolClass::Patch => {
                    a.declared.extend(action.paths.iter().cloned())
                }
                ToolClass::Other(_) => {}
            }
        }
    }
    /// Every path any agent of this session declared.
    fn declared(&self) -> BTreeSet<String> {
        self.agents
            .values()
            .flat_map(|a| a.declared.iter().cloned())
            .collect()
    }
    /// The ledger `agent` is checked against (ADR 0010 §7): its own entries;
    /// for other logicals the root's; else the newest read by any agent.
    fn view(&self, agent: &Agent) -> BTreeMap<LogicalId, LedgerEntry> {
        let mut view: BTreeMap<LogicalId, LedgerEntry> = BTreeMap::new();
        let own = self.agents.get(agent);
        let root = self.agents.get(&None);
        for e in own.into_iter().flat_map(|a| a.required.values()) {
            view.insert(e.logical, e.clone());
        }
        for e in root.into_iter().flat_map(|a| a.required.values()) {
            view.entry(e.logical).or_insert_with(|| e.clone());
        }
        let mut others: BTreeMap<LogicalId, &LedgerEntry> = BTreeMap::new();
        for (a, st) in &self.agents {
            if a == agent || a.is_none() {
                continue;
            }
            for e in st.required.values() {
                let slot = others.entry(e.logical).or_insert(e);
                if e.epoch > slot.epoch {
                    *slot = e;
                }
            }
        }
        for (l, e) in others {
            view.entry(l).or_insert_with(|| e.clone());
        }
        view
    }
}
impl Hooks {
    fn end(&mut self, id: SessionId, s: Arc<Mutex<Session>>) {
        self.claims.retain(|_, (sid, _)| *sid != id);
        let claims = &self.claims;
        self.claim_order.retain(|c| claims.contains_key(c));
        self.ended.push_back((id, s));
        while self.ended.len() > ENDED_KEPT {
            self.ended.pop_front();
        }
    }
}
/// Entries of `agent`'s view whose head or status changed since they were
/// last reported to it (ADR 0010 §4.4). A required read was certified at
/// its revision when it was made.
fn update_items(s: &Session, agent: &Agent, db: &Db) -> Vec<UpdateItem> {
    let reported = s.agents.get(agent).map(|a| &a.reported);
    let mut items: Vec<UpdateItem> = s
        .view(agent)
        .into_values()
        .filter_map(|e| {
            let current = db.head(e.logical);
            let summary = db
                .summary_at(current.unwrap_or(e.rev), &BTreeMap::new())
                .unwrap_or(Summary::Unavailable);
            let last = reported
                .and_then(|r| r.get(&e.logical).cloned())
                .unwrap_or((Some(e.rev), Summary::Certified));
            if (current, summary.clone()) == last {
                return None;
            }
            let key = db.logical_info(e.logical).map(|i| i.0).unwrap_or_default();
            Some(UpdateItem {
                logical: e.logical,
                key,
                cert_rev: e.rev,
                current,
                summary,
            })
        })
        .collect();
    items.sort_by(|a, b| a.key.cmp(&b.key));
    items
}
fn read_event(session: SessionId, agent: &Agent, e: &LedgerEntry) -> SessionEvent {
    SessionEvent::Read {
        session_id: session,
        agent: agent.clone(),
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
