//! Request handling, independent of sockets (ADR 0008 §5–§8). Identity
//! (channel, principal) comes from the transport, never from the request.
// Internal helpers return `Err(Response)`: the error is the reply itself and
// is returned immediately, never stored, so its size does not matter.
#![allow(clippy::result_large_err)]
use crate::{
    actor::{ActorHandle, Message, Snapshot, SubmitError},
    packing,
    proto::*,
    ranking,
};
use contextdb_core::{BranchView, IssueRequest, LogLink};
use contextdb_model::*;
use contextdb_store::recall::RecallIndex;
use contextdb_store::{
    Durability, Payload, StoreError,
    session::{Layer, RootProof, SessionEvent, SessionLog},
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
};

/// A recall's eligibility test (ADR 0016 §3.2), shared by its rankers and
/// decided once per logical: the head in the snapshot, the scope (lane,
/// kinds, `dir`), and in certified mode a CERTIFIED `summary_at`. A
/// logical failing only certification becomes a `withheld` entry (at most
/// `limit` of them); it never takes an item slot.
struct Eligibility<'a, 'v, D: Durability> {
    service: &'a Service<D>,
    db: &'a BranchView<'v>,
    lane: &'a str,
    opts: &'a RecallOpts,
    mode: Mode,
    limit: usize,
    live: &'a BTreeMap<String, Vec<u8>>,
    /// The session's principal: a match it may not read is skipped as if
    /// it did not match, before it can be withheld or take a slot (ADR
    /// 0017 §5.3).
    viewer: crate::redact::Viewer<'a, 'v>,
    /// `Some(item)` for an eligible logical, `None` for an ineligible one.
    decided: BTreeMap<LogicalId, Option<Item>>,
    withheld: Vec<Item>,
}

impl<D: Durability + Clone + 'static> Eligibility<'_, '_, D> {
    /// Whether `l` may be shown. `hit` is the revision a text hit indexes:
    /// a hit on a revision that is not the head says nothing about `l`
    /// (another row may index the head), so it is not remembered.
    fn admit(&mut self, l: LogicalId, hit: Option<RevisionId>) -> bool {
        if !self.viewer.sees(l) {
            return false;
        }
        let Some(head) = self.db.head(l) else {
            return false;
        };
        if hit.is_some_and(|r| r != head) {
            return false;
        }
        if let Some(d) = self.decided.get(&l) {
            return d.is_some();
        }
        let item = self
            .scoped(l, head)
            .then(|| self.service.item_at(self.db, l, head, None, self.live));
        let item = match item {
            Some(item) if self.mode == Mode::Certified && item.summary != Summary::Certified => {
                // I6: never presented as certified.
                if self.withheld.len() < self.limit {
                    self.withheld.push(item);
                }
                None
            }
            other => other,
        };
        let ok = item.is_some();
        self.decided.insert(l, item);
        ok
    }
    fn scoped(&self, l: LogicalId, head: RevisionId) -> bool {
        let Some((_, kind, lane)) = self.db.logical_info(l) else {
            return false;
        };
        if !self.lane.is_empty() && lane != self.lane {
            return false;
        }
        if !self.opts.kinds.is_empty() && !self.opts.kinds.contains(&kind) {
            return false;
        }
        let Some(dir) = &self.opts.dir else {
            return true;
        };
        // `dir` = the logical's `dirs` metadata ∪ the revision's anchors.
        if (self.db.metadata(l).unwrap_or_default().dirs.iter()).any(|d| ranking::covers(dir, d)) {
            return true;
        }
        // An anchor covers its own path; any other revision, its anchor
        // premises' paths.
        let covered =
            |r: RevisionId| (self.anchor_path(r)).is_some_and(|p| ranking::covers(dir, &p));
        covered(head)
            || (self.db.rules_concluding(head).into_iter())
                .flat_map(|(_, _, premises, _)| premises)
                .any(|p| covered(p.premise))
    }
    /// The repository path a CodeAnchor revision observes, if `r` is one.
    fn anchor_path(&self, r: RevisionId) -> Option<String> {
        let l = self.db.revision_logical(r)?;
        if self.db.logical_info(l)?.1 != RecordKind::CodeAnchor {
            return None;
        }
        let content = self.db.revision_content(r).ok()?;
        Some(match self.service.anchor_descriptor(content)? {
            contextdb_anchors::AnchorDescriptor::File(d) => d.path,
            contextdb_anchors::AnchorDescriptor::Symbol(d) => d.path,
        })
    }
}

/// Transport-bound identity of a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub channel: Channel,
    pub principal: PrincipalId,
}
/// The closed list of requests a principal other than the store owner may
/// send (ADR 0017 §3.3): the calls `ctx-mcp` makes. Hook, observer, admin
/// and store-wide requests (`Status`, `Briefing`, `Claim`, `Facts`, …) are
/// the store owner's.
pub fn non_owner_allowed(req: &Request) -> bool {
    match req {
        Request::SessionStart { .. }
        | Request::SessionEnd { .. }
        | Request::SessionAttach { .. }
        | Request::Updates { .. }
        | Request::Recall { .. }
        | Request::Read { .. }
        | Request::Check { .. }
        | Request::Why { .. }
        | Request::WhyNot { .. }
        | Request::Impact { .. }
        | Request::ImpactBatch { .. }
        | Request::Diff { .. }
        | Request::BranchList
        | Request::Record { .. }
        | Request::Supersede { .. }
        | Request::Commit { .. }
        | Request::WhatIf { .. }
        | Request::TxnStatus { .. }
        | Request::Changes { .. }
        | Request::Conflicts { .. }
        | Request::ConflictOpen { .. }
        | Request::Resolve { .. }
        | Request::HandoffBegin { .. }
        | Request::HandoffAccept { .. }
        | Request::BranchCreate { .. }
        | Request::BranchArchive { .. }
        | Request::Merge { .. } => true,
        Request::OnBranch { request, .. } => {
            !matches!(**request, Request::OnBranch { .. }) && non_owner_allowed(request)
        }
        _ => false,
    }
}
/// A session is bound only by its own principal (ADR 0017 §3.4); the store
/// owner's channels are one principal.
fn same_principal(id: Identity, s: &Mutex<Session>) -> Result<(), Response> {
    let owner = s.lock().expect("session").principal;
    if crate::principals::class(owner) == crate::principals::class(id.principal) {
        Ok(())
    } else {
        Err(err(
            "SessionOfAnotherPrincipal",
            "the session belongs to another principal",
        ))
    }
}
/// One read-ledger entry (ADR 0008 §6.2).
#[derive(Clone, Debug, PartialEq, Eq)]
struct LedgerEntry {
    logical: LogicalId,
    rev: RevisionId,
    required: bool,
    epoch: Epoch,
    /// The highest level of `rev` shown to this agent (ADR 0016 §4.2).
    level: levels::Level,
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
    proofs: BTreeMap<LogicalId, RootProof>,
    /// Supersessions not yet reported by `ctx_check`: (logical, old, new).
    superseded: Vec<(LogicalId, RevisionId, RevisionId)>,
    session_cert: Option<Certificate>,
    /// Last `(head, summary)` reported per logical in this agent's view.
    reported: BTreeMap<LogicalId, (Option<RevisionId>, Summary)>,
    /// Recent `Updates` peeks, until acknowledged: several, so a peek by
    /// `ctx_check` does not void the prompt hook's token (review minor 2).
    pending: VecDeque<(u64, Vec<UpdateItem>)>,
    /// Paths of this agent's edit, write, notebook and patch actions.
    declared: BTreeSet<String>,
    shell_actions: u32,
    writes: u32,
    writes_with_deps: u32,
}
pub(crate) struct Session {
    principal: PrincipalId,
    head_sha: String,
    repo: String,
    /// The branch every read, write, check and certificate of the session
    /// uses, fixed for its lifetime (ADR 0013 §7, D1).
    pub(crate) branch: BranchId,
    /// The harness session this session serves, if any.
    key: Option<(String, String)>,
    started_epoch: Epoch,
    ended_epoch: Option<Epoch>,
    agents: BTreeMap<Agent, AgentState>,
    /// Agent ids hooks have reported for this session; any other id is
    /// treated as the root (ADR 0010 §2.2).
    known_agents: BTreeSet<String>,
    /// Certificates issued to this session (recall and session certificates).
    certs: Vec<Certificate>,
    proof_history: Vec<RootProof>,
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
    /// Latest session per principal and harness session key (live or
    /// ended); keys are looked up per principal (ADR 0017 §3.4).
    keys: BTreeMap<(PrincipalId, String, String), SessionId>,
    /// Claude Code MCP tool-use id → (session, agent). Every request of
    /// that tool call resolves through it; dropped at session end, oldest
    /// first beyond [`CLAIMS_KEPT`].
    claims: BTreeMap<String, (SessionId, Agent)>,
    claim_order: VecDeque<String>,
    /// client_txn_ids already counted as writes: a retried (idempotent)
    /// commit is not counted twice (review minor 12).
    written: BTreeSet<[u8; 16]>,
    /// Outbox stream high-water marks.
    streams: BTreeMap<[u8; 16], u64>,
    /// The session each stream's facts belong to (its first applied fact);
    /// late facts and its StreamEnd go there (closure check N1).
    stream_owner: BTreeMap<[u8; 16], SessionId>,
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
    pub(crate) anchor_routes: Mutex<Option<std::path::PathBuf>>,
    /// Opened routed worktrees by root, with the identity they were opened
    /// at; a leaf lock never held across another lock.
    pub(crate) anchor_repos: Mutex<BTreeMap<String, crate::anchors::OpenedRepo>>,
    /// Decoded anchor descriptors by content hash (immutable).
    pub(crate) anchor_descriptors:
        Mutex<BTreeMap<ContentHash, contextdb_anchors::AnchorDescriptor>>,
    /// Deadline of one live anchor sample (ADR 0011 §3: 40 ms by default).
    pub(crate) anchor_live_timeout: Mutex<std::time::Duration>,
    hooks: Mutex<Hooks>,
    /// Serialises outbox deliveries (stream marks) across sweeper and hooks.
    facts_lock: Mutex<()>,
    /// Serialises session creation by key (review M1). Lock order: `facts_lock`,
    /// `attach_lock`, then `sessions` or `hooks` one at a time, then a session.
    attach_lock: Mutex<()>,
    tokens: AtomicU64,
    /// bm25 hits the text ranker examined, and items recall returned
    /// (ADR 0016 §10 G5: hits scanned per returned item, reported).
    pub text_hits_scanned: AtomicU64,
    pub recall_items: AtomicU64,
    writer: Mutex<mpsc::Sender<Writer>>,
    writer_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    recall: Arc<Mutex<RecallIndex>>,
    /// A read-only connection for recall queries when the index is a file,
    /// so queries never wait for the indexer's writes (P8 G5); else the
    /// queries share `recall`.
    recall_reader: Option<Mutex<RecallIndex>>,
    /// The retrieval projections' health and progress signal (P8, ADR 0016 §2).
    pub index_health: Arc<crate::index_health::IndexHealth>,
    /// Serialises `ctx_conflicts open` (P7 m7: one Conflict per pair).
    pub(crate) conflict_opens: Mutex<()>,
    /// Lexical contradiction candidates (P7, ADR 0015 §5).
    pub candidates: Arc<crate::candidates::Candidates>,
    stop: Arc<AtomicBool>,
    /// The P7 job scheduler, when the daemon runs one (ADR 0015 §4.4).
    pub validators: std::sync::OnceLock<Arc<crate::validators::Scheduler<D>>>,
    /// The recall and candidate indexer.
    indexer_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Embeddings, when `embeddings.toml` configures an embedder (P8, ADR
    /// 0016 §7); without one the vector projection is `Disabled`.
    pub vectors: std::sync::OnceLock<Arc<crate::embedding::Vectors>>,
    /// Why the embedder configuration could not be used: vectors report
    /// `Failed` with it.
    pub(crate) vectors_error: std::sync::OnceLock<String>,
    embedding_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The data directory, for `mirror.sqlite` (P8, ADR 0016 §8); `None`
    /// keeps the mirror cache in memory.
    pub(crate) data_dir: Mutex<Option<std::path::PathBuf>>,
    /// Serialises `ctx mirror` in this daemon (other writers of the ref are
    /// caught by its compare-and-swap).
    mirror_lock: Mutex<()>,
    /// Handoffs (P7, ADR 0015 §7), a projection of the session log.
    handoffs: Mutex<Handoffs>,
    /// `ctx_changes` requests waiting for an epoch (ADR 0015 §8).
    pub(crate) change_waiters: std::sync::atomic::AtomicUsize,
    /// The uid → principal mapping (P9, ADR 0017 §3); empty in
    /// single-user mode.
    principals: std::sync::RwLock<Arc<crate::principals::PrincipalMap>>,
    /// Set when multi-user mode may be entered (P9 milestone 2: tests only).
    multi_user: std::sync::OnceLock<crate::principals::MultiUser>,
}
impl<D: Durability> Drop for Service<D> {
    fn drop(&mut self) {
        self.stop_indexer();
        self.stop_writer();
    }
}
impl<D: Durability> Service<D> {
    /// Stops the recall and candidate indexer and waits for it, so a
    /// stopped daemon no longer writes `recall.sqlite` or
    /// `candidates.sqlite` (a successor in the same process owns them).
    pub fn stop_indexer(&self) {
        self.stop.store(true, Ordering::Release);
        for slot in [&self.indexer_thread, &self.embedding_thread] {
            let thread = slot.lock().expect("indexer").take();
            if let Some(t) = thread
                && t.thread().id() != std::thread::current().id()
            {
                let _ = t.join();
            }
        }
    }
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
pub(crate) type Bound = (SessionId, Arc<Mutex<Session>>, Agent, bool);
/// Largest `limit` a recall serves; more is clamped and reported (ADR 0016
/// §3.1).
pub const MAX_RECALL: usize = 100;
/// A session's branch (ADR 0013 §7, D1): the named branch, which must exist
/// and be open; else the branch named like the client's git branch, if it
/// exists and is open; else `main`.
pub(crate) fn resolve_branch<D: Durability>(
    snap: &Snapshot<D>,
    explicit: Option<&str>,
    git_branch: Option<&str>,
) -> Result<BranchId, Response> {
    resolve_branch_for(snap, explicit, git_branch, PrincipalId::STORE_OWNER)
}
/// As `resolve_branch`, for a session of principal `p`: the git branch's
/// namesake binds only if `p` or the store owner created it (ADR 0017 §6.2
/// rule 6); a branch another principal created under that name is skipped
/// and the session binds `main`. An explicit name binds any open branch
/// (commits there are still the creator's).
pub(crate) fn resolve_branch_for<D: Durability>(
    snap: &Snapshot<D>,
    explicit: Option<&str>,
    git_branch: Option<&str>,
    p: PrincipalId,
) -> Result<BranchId, Response> {
    let open = |name: &str| {
        let id = snap.kernel.branch_by_name(name)?;
        let archived = snap.kernel.branch(id)?.info().archived;
        Some((id, archived))
    };
    let ours = |id: BranchId| {
        snap.kernel.branch(id).is_some_and(|b| {
            let c = b.info().creator;
            c.is_store_owner() || c == contextdb_core::access_class(p)
        })
    };
    match explicit {
        Some(name) => match open(name) {
            Some((id, false)) => Ok(id),
            Some((_, true)) => Err(err("BranchArchived", format!("branch {name} is archived"))),
            None => Err(err("UnknownBranch", format!("no branch named {name}"))),
        },
        None => Ok(match git_branch.and_then(open) {
            Some((id, false)) if ours(id) => id,
            _ => MAIN,
        }),
    }
}
/// Every content-hash field a batch carries (ADR 0017 §6.3).
fn content_hashes(ops: &[Op]) -> Vec<ContentHash> {
    let mut out = Vec::new();
    for op in ops {
        match op {
            Op::CreateLogical { content, .. } | Op::AddRevision { content, .. } => {
                out.push(*content)
            }
            Op::OpenArtifact { payload_hash, .. } => out.push(*payload_hash),
            Op::ResolveArtifact { note: Some(n), .. } => out.push(*n),
            Op::RecordValidation { meta: Some(m), .. } => out.push(*m),
            _ => {}
        }
    }
    out
}
/// A target as its sender wrote it, for `Unavailable` (ADR 0017 §5.7).
pub(crate) fn target_text(t: &Target) -> String {
    match t {
        Target::Handle(h) => h.clone(),
        Target::Key(k) => k.clone(),
        Target::KeyRev { key, rev } => format!("{key}@{}", rev.0),
        Target::Logical(l) => format!("logical {}", l.0),
        Target::Pinned { logical, rev } => format!("logical {}@{}", logical.0, rev.0),
    }
}
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
/// `key@rev` and kind of each resolved dep, for rule error messages.
fn dep_names(
    db: &BranchView<'_>,
    deps: &[(RevRef, EdgeKind)],
) -> Vec<(String, RecordKind, EdgeKind)> {
    (deps.iter())
        .filter_map(|(r, k)| {
            let RevRef::Existing(r) = r else { return None };
            let (key, kind, _) = db.logical_info(db.revision_logical(*r)?)?;
            Some((format!("{key}@{}", r.0), kind, *k))
        })
        .collect()
}
/// Says which dep a rejected `ctx_record` rule names and what to send instead
/// (the kernel's error carries no dep).
fn explain_rule_error(response: Response, deps: &[(String, RecordKind, EdgeKind)]) -> Response {
    let Response::Error { code, detail } = &response else {
        return response;
    };
    if code != "InvalidRule" {
        return response;
    }
    let hint = if detail.contains("ForbiddenPremise") {
        let names: Vec<_> = (deps.iter())
            .filter(|(_, kind, k)| {
                *kind == RecordKind::Summary && matches!(k, EdgeKind::Cur | EdgeKind::Pin)
            })
            .map(|(n, _, _)| n.as_str())
            .collect();
        format!(
            "{} is a Summary: depend on a Summary with kind \"sem\" (re-validated when it \
             changes) or \"ref\" (a reference that never changes status), not cur or pin",
            names.join(", ")
        )
    } else if detail.contains("ConflictingVersions") {
        "two deps name different revisions of one record; keep one".into()
    } else if detail.contains("ForbiddenConclusion") {
        "an Evidence record cannot have deps; record it without deps".into()
    } else {
        return response;
    };
    err(code, format!("{detail}: {hint}"))
}
pub(crate) fn err(code: &str, detail: impl Into<String>) -> Response {
    Response::Error {
        code: code.into(),
        detail: detail.into(),
    }
}
/// Only the actor appends certificate read sets (ADR 0013 §4, N6).
fn certificate_reads(ops: &[Op]) -> Option<Response> {
    ops.iter()
        .any(|op| {
            matches!(
                op,
                Op::ExpectHeads {
                    source: ReadSource::Certificate(_),
                    ..
                }
            )
        })
        .then(|| {
            err(
                "ChannelRestricted",
                "certificate read sets are appended by the daemon",
            )
        })
}
/// How far the purge of shreds has got (ADR 0017 §8.4): the last shred
/// epoch, and the last one each part has purged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PurgeState {
    pub through: Epoch,
    pub keys: Epoch,
    pub recall: Epoch,
    pub vectors: Epoch,
    pub jobs: Epoch,
    pub candidates: Epoch,
}
impl PurgeState {
    /// Every part covers the last shred.
    pub fn complete(&self) -> bool {
        [
            self.keys,
            self.recall,
            self.vectors,
            self.jobs,
            self.candidates,
        ]
        .iter()
        .all(|e| *e >= self.through)
    }
}
/// The contents a validator job can hold: its rule's conclusion, every
/// premise it asserted and the revisions of its current vector.
fn job_contents<D: Durability>(
    snap: &Snapshot<D>,
    branch: BranchId,
    rule: RuleId,
    curvec: &HeadVector,
) -> Vec<ContentHash> {
    let Some(v) = snap.kernel.branch(branch) else {
        return Vec::new();
    };
    let mut revs: Vec<RevisionId> = curvec.iter().map(|(_, r)| *r).collect();
    if let Some((conclusion, incidences, refs)) = v.rule_parts(rule) {
        revs.push(conclusion);
        revs.extend(incidences.iter().map(|i| i.premise));
        revs.extend(refs);
    }
    revs.into_iter()
        .filter_map(|r| v.revision_content(r).ok())
        .collect()
}
/// What `ctx shred` names (ADR 0017 §8.3): the content of `key` at `rev`
/// (default: its head on `main`, else on the first branch that sees it),
/// every revision sharing it on every branch, and with `with_reviews` the
/// review payloads and validation metadata on those revisions and the
/// notes that resolved those reviews. CodeAnchor descriptors are refused:
/// they hold hashes, not text.
fn shred_targets<D: Durability>(
    snap: &Snapshot<D>,
    key: &str,
    rev: Option<RevisionId>,
    with_reviews: bool,
) -> Result<Vec<ShredTarget>, Response> {
    let k = &snap.kernel;
    let seen = (k.branches())
        .filter_map(|b| {
            let v = k.branch(b.id)?;
            let l = v.logical_by_key(key)?;
            Some((v, l))
        })
        .next();
    let Some((view, l)) = seen else {
        return Err(err("UnknownRef", format!("no logical {key}")));
    };
    if view
        .logical_info(l)
        .is_some_and(|(_, kind, _)| kind == RecordKind::CodeAnchor)
    {
        return Err(err(
            "InvalidRequest",
            "a CodeAnchor descriptor holds hashes and paths, not text: nothing to shred",
        ));
    }
    let main_head = k.branch(MAIN).and_then(|m| m.head(l));
    let r = match rev.or(main_head).or_else(|| view.head(l)) {
        Some(r) if view.revision_logical(r) == Some(l) => r,
        _ => return Err(err("UnknownRef", format!("no such revision of {key}"))),
    };
    let content = view
        .revision_content(r)
        .map_err(|e| err(&code(&e), format!("{e:?}")))?;
    let mut out: Vec<ShredTarget> = Vec::new();
    let mut artifacts: BTreeMap<ArtifactId, ShredTarget> = BTreeMap::new();
    for info in k.branches() {
        let Some(v) = k.branch(info.id) else { continue };
        for r in v.revisions_with_content(&content) {
            let Some(key) = (v.revision_logical(r))
                .and_then(|l| v.logical_info(l))
                .map(|i| i.0)
            else {
                continue;
            };
            let target = |content, what: &str| ShredTarget {
                content,
                what: what.into(),
                branch: info.name.clone(),
                key: key.clone(),
                rev: r,
            };
            out.push(target(content, "body"));
            if !with_reviews {
                continue;
            }
            for (a, art) in v.artifacts() {
                if art.target == r {
                    let t = target(art.payload_hash, "review");
                    artifacts.insert(a, t.clone());
                    out.push(t);
                }
            }
            for (j, ..) in v.rules_concluding(r) {
                for (_, record) in v.rule_validations(j) {
                    if let Some(meta) = record.meta {
                        out.push(target(meta, "validation"));
                    }
                }
            }
        }
    }
    // Resolution notes live only in the op_log.
    if with_reviews && !artifacts.is_empty() {
        for e in 1..=snap.epoch.0 {
            let Ok(batch) = snap.log.read_row(Epoch(e)).and_then(|r| {
                r.batch()
                    .map_err(|e| contextdb_store::StoreError::Io(e.to_string()))
            }) else {
                continue;
            };
            for op in &batch.ops {
                if let Op::ResolveArtifact {
                    id: ArtifactRef::Existing(a),
                    note: Some(note),
                } = op
                    && let Some(t) = artifacts.get(a)
                {
                    out.push(ShredTarget {
                        content: *note,
                        what: "note".into(),
                        ..t.clone()
                    });
                }
            }
        }
    }
    let mut seen = BTreeSet::new();
    out.retain(|t| seen.insert((t.content, t.what.clone(), t.branch.clone(), t.rev)));
    Ok(out)
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
    /// Starts embeddings (ADR 0016 §7) with the vector projection in
    /// `path` (memory if `None`). Once per service.
    pub fn start_vectors(
        &self,
        settings: crate::embedding::Settings,
        path: Option<&std::path::Path>,
    ) -> Result<(), String> {
        let v = Arc::new(crate::embedding::Vectors::open(settings, path)?);
        if self.vectors.set(v.clone()).is_err() {
            return Err("embeddings already started".into());
        }
        let (actor, table, stop) = (
            self.actor.clone(),
            self.candidates.clone(),
            self.stop.clone(),
        );
        let t = std::thread::Builder::new()
            .name("ctxd-embedder".into())
            .spawn(move || crate::embedding::run(actor, v, table, stop))
            .map_err(|e| format!("embedding thread: {e}"))?;
        *self.embedding_thread.lock().expect("indexer") = Some(t);
        Ok(())
    }
    /// G4 fault injection: caps `recall.sqlite` at its current size plus
    /// `extra` pages, so index writes fail as on a full disk (`None`
    /// lifts the cap).
    #[doc(hidden)]
    pub fn limit_recall(&self, extra: Option<u64>) {
        let r = self.recall.lock().expect("recall");
        let pages = extra.map(|e| r.page_count().unwrap_or(0) + e);
        let _ = r.limit_pages(pages);
    }
    /// The connection recall queries use.
    fn recall_query(&self) -> std::sync::MutexGuard<'_, RecallIndex> {
        match &self.recall_reader {
            Some(r) => r.lock().expect("recall reader"),
            None => self.recall.lock().expect("recall"),
        }
    }
    /// The purge of every committed shred (ADR 0017 §8.4), idempotent:
    /// each body-bearing projection purges the shreds above its own
    /// `purged_through`. Returns how far each part has got; the purge is
    /// complete when every part covers `through`.
    pub fn purge(&self) -> Result<PurgeState, String> {
        let snap = self.actor.snapshot();
        let shredded = snap.kernel.shredded();
        let through = shredded.values().max().copied().unwrap_or(Epoch(0));
        let mut state = PurgeState {
            through,
            keys: snap.key_purged,
            ..PurgeState::default()
        };
        // What each projection must purge: contents shredded above its mark,
        // and the revisions carrying them on every branch.
        let above = |mark: Epoch| -> (Vec<ContentHash>, Vec<(BranchId, RevisionId, LogicalId)>) {
            let contents: Vec<ContentHash> = (shredded.iter())
                .filter(|(_, e)| **e > mark)
                .map(|(c, _)| *c)
                .collect();
            let mut revs = Vec::new();
            for info in snap.kernel.branches() {
                let Some(v) = snap.kernel.branch(info.id) else {
                    continue;
                };
                for c in &contents {
                    for r in v.revisions_with_content(c) {
                        if let Some(l) = v.revision_logical(r) {
                            revs.push((info.id, r, l));
                        }
                    }
                }
            }
            (contents, revs)
        };
        let sqlerr = |what: &str, e: &dyn std::fmt::Display| format!("{what}: {e}");
        // recall.sqlite: the touched logicals re-indexed with empty text.
        {
            let mut recall = self.recall.lock().expect("recall");
            let mark = recall.purged_through().map_err(|e| sqlerr("recall", &e))?;
            if mark < through {
                let (_, revs) = above(mark);
                let logicals: BTreeSet<LogicalId> = revs.iter().map(|(_, _, l)| *l).collect();
                let mut entries: Vec<_> = (logicals.into_iter())
                    .map(|l| (l, crate::candidates::rows_of(&snap, l)))
                    .collect();
                crate::candidates::scrub(&snap, &mut entries);
                recall
                    .purge(&entries, through)
                    .map_err(|e| sqlerr("recall", &e))?;
            }
            state.recall = recall.purged_through().map_err(|e| sqlerr("recall", &e))?;
        }
        // vectors.sqlite: also when no embedder runs now but the file is left.
        let data = self.data_dir.lock().expect("data dir").clone();
        if let Some(v) = self.vectors.get() {
            let mark = v.purged_through()?;
            if mark < through {
                v.purge(&above(mark).0, through)?;
            }
            state.vectors = v.purged_through()?;
        } else if let Some(path) = (data.as_ref())
            .map(|d| d.join(contextdb_store::vectors::FILE))
            .filter(|p| p.exists())
        {
            let mut v = contextdb_store::vectors::VectorStore::open(Some(&path))
                .map_err(|e| sqlerr("vectors", &e))?;
            let mark = v.purged_through().map_err(|e| sqlerr("vectors", &e))?;
            if mark < through {
                v.purge(&above(mark).0, through)
                    .map_err(|e| sqlerr("vectors", &e))?;
            }
            state.vectors = v.purged_through().map_err(|e| sqlerr("vectors", &e))?;
        } else {
            state.vectors = through;
        }
        // jobs.sqlite: every job whose task holds a shredded payload.
        if let Some(s) = self.validators.get() {
            let mark = s.jobs.purged_through()?;
            if mark < through {
                let (contents, _) = above(mark);
                let contents: BTreeSet<ContentHash> = contents.into_iter().collect();
                let keys: Vec<_> = (s.jobs.all()?.into_iter())
                    .filter(|j| {
                        job_contents(&snap, j.branch, j.rule, &j.curvec)
                            .iter()
                            .any(|c| contents.contains(c))
                    })
                    .map(|j| j.key)
                    .collect();
                s.jobs.purge(&keys, through)?;
            }
            state.jobs = s.jobs.purged_through()?;
        } else {
            state.jobs = through;
        }
        // candidates.sqlite: every pair with a side carrying one.
        let mark = self
            .candidates
            .purged_through()
            .map_err(|e| sqlerr("candidates", &e))?;
        if mark < through {
            let revs: Vec<(BranchId, RevisionId)> =
                above(mark).1.into_iter().map(|(b, r, _)| (b, r)).collect();
            self.candidates
                .purge(&revs, through)
                .map_err(|e| sqlerr("candidates", &e))?;
        }
        state.candidates = self
            .candidates
            .purged_through()
            .map_err(|e| sqlerr("candidates", &e))?;
        Ok(state)
    }
    /// Shred epochs above the lowest mark of the key log and the
    /// projections this daemon runs (ADR 0017 §8.4); read only.
    fn purge_pending(&self, snap: &Snapshot<D>) -> Vec<Epoch> {
        let shredded = snap.kernel.shredded();
        if shredded.is_empty() {
            return Vec::new();
        }
        let mut marks = vec![snap.key_purged];
        marks.extend(self.recall.lock().expect("recall").purged_through().ok());
        marks.extend(self.vectors.get().and_then(|v| v.purged_through().ok()));
        marks.extend(
            self.validators
                .get()
                .and_then(|s| s.jobs.purged_through().ok()),
        );
        marks.extend(self.candidates.purged_through().ok());
        let low = marks.into_iter().min().unwrap_or(Epoch(0));
        let pending: BTreeSet<Epoch> = shredded.values().filter(|e| **e > low).copied().collect();
        pending.into_iter().collect()
    }
    /// The data directory, where `mirror.sqlite` lives.
    pub fn set_data_dir(&self, dir: std::path::PathBuf) {
        *self.data_dir.lock().expect("data dir") = Some(dir);
    }
    /// `ctx mirror` (ADR 0016 §8), on the admin socket.
    fn mirror(&self, action: MirrorAction, snap: &Arc<Snapshot<D>>) -> Response {
        let _one = self.mirror_lock.lock().expect("mirror");
        let data = self.data_dir.lock().expect("data dir").clone();
        let (project, branch) = match &action {
            MirrorAction::Export {
                project, branch, ..
            }
            | MirrorAction::Proposals { project, branch } => (project.clone(), branch.clone()),
            MirrorAction::Status { project } | MirrorAction::Prune { project, .. } => {
                (project.clone(), None)
            }
        };
        let branch = match branch {
            None => MAIN,
            Some(name) => match snap.kernel.branch_by_name(&name) {
                Some(b) => b,
                None => return err("UnknownBranch", format!("no branch named {name}")),
            },
        };
        let mut m =
            match crate::mirror::Mirror::new(snap, std::path::Path::new(&project), data.as_deref())
            {
                Ok(m) => m,
                Err(e) => return err(e.code(), e.detail()),
            };
        let done = match action {
            MirrorAction::Export {
                force,
                adopt,
                readable_ok,
                rebuild,
                ..
            } => {
                m.readable_ok = readable_ok;
                if rebuild {
                    m.rebuild(branch)
                } else {
                    m.export(branch, force, adopt)
                }
            }
            MirrorAction::Proposals { .. } => m.proposals(branch),
            MirrorAction::Status { .. } => m.status(),
            MirrorAction::Prune {
                older_than_secs, ..
            } => m.prune(older_than_secs),
        };
        match done {
            Ok(report) => Response::Mirror { report },
            Err(e) => err(e.code(), e.detail()),
        }
    }
    /// An `embeddings.toml` that could not be used: vectors report `Failed`.
    pub fn vectors_unavailable(&self, error: String) {
        let _ = self.vectors_error.set(error);
    }
    /// Both projections' states (ADR 0016 §2.2).
    pub fn index_report(&self, head: Epoch) -> IndexReport {
        IndexReport {
            fts: self.index_health.state(head),
            vectors: match (self.vectors.get(), self.vectors_error.get()) {
                (Some(v), _) => v.state(head),
                (None, Some(e)) => IndexState::Failed { error: e.clone() },
                (None, None) => IndexState::Disabled,
            },
        }
    }
    /// Operator-selected routing configuration. It is never accepted from an
    /// agent request or inferred from a record body.
    /// Overrides the live-sample deadline. Semantic tests use a generous
    /// one so a cold sample on a loaded machine is not Unknown; the 40 ms
    /// production budget is measured by `anchor_bench` on a real daemon.
    pub fn set_anchor_live_timeout(&self, timeout: std::time::Duration) {
        *self.anchor_live_timeout.lock().expect("timeout") = timeout;
    }
    /// Trusted repository routes; also prepares the outbox announce record
    /// (ADR 0011 §9.3). A failure there only leaves live evidence Unknown.
    pub fn set_anchor_routes(&self, path: std::path::PathBuf) {
        if let Some(dir) = path.parent() {
            let _ = contextdb_proto::outbox::Outbox::new(dir).init_announce();
        }
        *self.anchor_routes.lock().expect("routes") = Some(path);
    }
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
        let (recall, on_disk) = match recall_path.map(|p| RecallIndex::open(Some(p))) {
            Some(Ok(r)) => (r, true),
            _ => (RecallIndex::open(None).expect("recall index"), false),
        };
        let recall = Arc::new(Mutex::new(recall));
        let recall_reader = recall_path
            .filter(|_| on_disk)
            .and_then(|p| RecallIndex::open_reader(p).ok())
            .map(Mutex::new);
        // Contradiction candidates sit next to the recall projection.
        let cand_path = recall_path.map(|p| p.with_file_name(crate::candidates::FILE));
        let candidates = Arc::new(
            crate::candidates::Candidates::open(cand_path.as_deref())
                .or_else(|_| crate::candidates::Candidates::open(None))
                .expect("candidate table"),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let index_health = Arc::new(crate::index_health::IndexHealth::new(
            recall
                .lock()
                .expect("recall")
                .watermark()
                .unwrap_or(Epoch(0)),
        ));
        let indexer_thread = {
            let (actor, recall, candidates, health, stop) = (
                actor.clone(),
                recall.clone(),
                candidates.clone(),
                index_health.clone(),
                stop.clone(),
            );
            std::thread::Builder::new()
                .name("ctxd-indexer".into())
                .spawn(move || crate::candidates::indexer(actor, recall, candidates, health, stop))
                .expect("indexer thread")
        };
        let (tx, rx) = mpsc::channel();
        let writer_thread = std::thread::Builder::new()
            .name("ctxd-session-log".into())
            .spawn(move || writer(log, rx))
            .expect("session writer thread");
        let service = Self {
            actor,
            sessions: Mutex::new(BTreeMap::new()),
            anchor_routes: Mutex::new(None),
            anchor_repos: Mutex::new(BTreeMap::new()),
            anchor_descriptors: Mutex::new(BTreeMap::new()),
            anchor_live_timeout: Mutex::new(contextdb_anchors::capture::LIVE_TIMEOUT),
            hooks: Mutex::new(Hooks::default()),
            facts_lock: Mutex::new(()),
            attach_lock: Mutex::new(()),
            // Random start: a token from before a restart never matches.
            text_hits_scanned: AtomicU64::new(0),
            recall_items: AtomicU64::new(0),
            tokens: AtomicU64::new(
                u64::from_le_bytes(random_id()[..8].try_into().expect("8")) >> 1,
            ),
            writer: Mutex::new(tx),
            writer_thread: Mutex::new(Some(writer_thread)),
            recall,
            recall_reader,
            index_health,
            conflict_opens: Mutex::new(()),
            candidates,
            stop,
            indexer_thread: Mutex::new(Some(indexer_thread)),
            vectors: std::sync::OnceLock::new(),
            vectors_error: std::sync::OnceLock::new(),
            embedding_thread: Mutex::new(None),
            data_dir: Mutex::new(None),
            mirror_lock: Mutex::new(()),
            handoffs: Mutex::new(Handoffs::default()),
            change_waiters: std::sync::atomic::AtomicUsize::new(0),
            validators: std::sync::OnceLock::new(),
            principals: std::sync::RwLock::new(Arc::default()),
            multi_user: std::sync::OnceLock::new(),
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
        let mut handoffs = self.handoffs.lock().expect("handoffs");
        for e in events {
            handoffs.replay(&e);
            match e {
                SessionEvent::HandoffBegin { .. } | SessionEvent::HandoffAccept { .. } => {}
                SessionEvent::Start {
                    session_id,
                    principal,
                    head_sha,
                    repo,
                    branch,
                    started_epoch,
                    log_checksum,
                    ..
                } => {
                    let log_broken = started_epoch > tail
                        || snap.log.checksum_at(started_epoch).ok() != Some(log_checksum);
                    let mut session =
                        Session::new(principal, head_sha, repo, started_epoch, branch);
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
                    level,
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
                                level,
                            });
                    }
                }
                SessionEvent::RootProof { agent, proof } => {
                    if proof.cert.issued_epoch > tail
                        || snap.log.checksum_at(proof.cert.issued_epoch).ok()
                            != Some(proof.cert.log_checksum_at_issue)
                    {
                        broken(&map, &proof.cert.session_id);
                    }
                    if let Some(s) = map.get(&proof.cert.session_id)
                        && let Some(root) = proof.cert.entries.first()
                    {
                        let mut s = s.lock().expect("session");
                        s.proof_history.push((*proof).clone());
                        s.agent(&agent).proofs.insert(root.logical, *proof);
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
                    client_txn_id,
                    ..
                } => {
                    if !hooks.written.insert(client_txn_id) {
                        continue;
                    }
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
                    let mut principal = PrincipalId::STORE_OWNER;
                    if let Some(s) = map.get(&session_id) {
                        let mut s = s.lock().expect("session");
                        s.key = Some((harness.clone(), harness_session.clone()));
                        principal = crate::principals::class(s.principal);
                    }
                    hooks
                        .keys
                        .insert((principal, harness, harness_session), session_id);
                }
                SessionEvent::Fact {
                    session_id,
                    packet,
                    stream,
                    ..
                } => {
                    hooks.stream_owner.entry(stream).or_insert(session_id);
                    if let Some(s) = find(&map, &hooks, &session_id) {
                        s.lock().expect("session").note_fact(&packet);
                    }
                    if let Fact::Gate {
                        action,
                        mode,
                        decision,
                        harness_version,
                        at,
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
                            at,
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
                    at,
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
                    at,
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
    /// Handles one request on the given snapshot-consistent view. A
    /// principal other than the store owner gets the response through the
    /// single redaction point (ADR 0017 §5.1).
    pub fn handle(&self, id: Identity, req: Request) -> Envelope {
        let _ = crate::redact::take_branch();
        let (epoch, response) = self.handle_unredacted(id, req);
        if id.principal.is_store_owner() {
            return Envelope::new(epoch, response);
        }
        // The newest snapshot names every id the response can carry (a
        // commit's own allocations included): topology only grows.
        let snap = self.actor.snapshot();
        let branch = crate::redact::take_branch().unwrap_or(MAIN);
        let view = snap.view(branch);
        let viewer = crate::redact::Viewer::new(id.principal, &view);
        let mut redaction = Redaction::default();
        let response = viewer.redact(response, &mut redaction);
        Envelope {
            epoch,
            response,
            redaction: Some(redaction),
        }
    }
    fn handle_unredacted(&self, id: Identity, req: Request) -> (Epoch, Response) {
        // Principals other than the store owner get MCP-level requests only
        // (ADR 0017 §3.3, F8).
        if !id.principal.is_store_owner() && !non_owner_allowed(&req) {
            return (
                self.actor.snapshot().epoch,
                err(
                    "ChannelRestricted",
                    "this request is for the store owner only",
                ),
            );
        }
        // A long-poll must not hold a snapshot while it waits (the actor
        // would have to clone a replica per group, ADR 0008 §3.3).
        if let Request::Changes {
            binding,
            branch,
            since_epoch,
            scope,
            max_items,
            wait_ms,
        } = req
        {
            let response = self.changes(
                id,
                binding,
                branch,
                since_epoch,
                scope,
                max_items,
                wait_ms,
                self.actor.snapshot(),
            );
            return (self.actor.snapshot().epoch, response);
        }
        let snap = self.actor.snapshot();
        let epoch = snap.epoch;
        (epoch, self.dispatch(id, req, snap))
    }
    /// Takes the snapshot by value: write paths release it before waiting for
    /// their group, so a queued writer never pins a replica the actor wants
    /// to reclaim (ADR 0008 §3.3).
    fn dispatch(&self, id: Identity, req: Request, snap: Arc<Snapshot<D>>) -> Response {
        self.dispatch_at(id, req, snap, MAIN)
    }
    /// Unbound requests read (and `ImpactBatch`/`Validate` write) `branch`:
    /// `main`, or the branch an `OnBranch` named (protocol 4).
    fn dispatch_at(
        &self,
        id: Identity,
        req: Request,
        snap: Arc<Snapshot<D>>,
        branch: BranchId,
    ) -> Response {
        if branch != MAIN {
            crate::redact::note_branch(branch);
        }
        let db = &snap.view(branch);
        match req {
            Request::SessionStart {
                harness,
                repo,
                head_sha,
                parent,
                branch,
                git_branch,
            } => {
                let branch = match resolve_branch_for(
                    &snap,
                    branch.as_deref(),
                    git_branch.as_deref(),
                    id.principal,
                ) {
                    Ok(b) => b,
                    Err(r) => return r,
                };
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
                        branch,
                    ))),
                );
                self.log(vec![SessionEvent::Start {
                    session_id,
                    parent,
                    harness,
                    repo,
                    branch,
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
                if let Err(r) = self.end_session(id, session, snap.epoch) {
                    return r;
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
                index_wait_ms,
                opts,
            } => match self.bind(id, &binding, &snap) {
                Ok(b) => {
                    // Bounded wait for the text index to reach this snapshot
                    // (ADR 0016 §2.2); on timeout it searches anyway and the
                    // response says `Lagging`.
                    if index_wait_ms > 0 {
                        self.index_health.wait(
                            snap.epoch,
                            std::time::Duration::from_millis(index_wait_ms as u64),
                        );
                    }
                    self.recall(b, &query, &scope, &opts, mode, limit as usize, &snap)
                }
                Err(r) => r,
            },
            Request::Mirror { action } => {
                if id.channel != Channel::Admin {
                    return err("ChannelRestricted", "ctx mirror requires the admin socket");
                }
                self.mirror(action, &snap)
            }
            Request::SetAccess { doc, client_txn_id } => {
                drop(snap);
                if id.channel != Channel::Admin {
                    return err("ChannelRestricted", "ctx access requires the admin socket");
                }
                let batch = Batch::new(vec![Op::SetAccess { doc: Box::new(doc) }]);
                self.commit(id, Vec::new(), batch, client_txn_id)
            }
            Request::Shred {
                key,
                rev,
                with_reviews,
                confirm,
                client_txn_id,
            } => {
                if id.channel != Channel::Admin {
                    return err("ChannelRestricted", "ctx shred requires the admin socket");
                }
                let targets = match shred_targets(&snap, &key, rev, with_reviews) {
                    Ok(t) => t,
                    Err(r) => return r,
                };
                drop(snap);
                if !confirm {
                    return Response::Shred {
                        targets,
                        epoch: None,
                        purged: false,
                    };
                }
                let contents: BTreeSet<ContentHash> = targets.iter().map(|t| t.content).collect();
                let ops = (contents.into_iter())
                    .map(|content| Op::ShredContent { content })
                    .collect();
                match self.commit(id, Vec::new(), Batch::new(ops), client_txn_id) {
                    Response::Committed { result } => Response::Shred {
                        targets,
                        epoch: Some(result.epoch),
                        // `purged` only once the key log and every
                        // projection cover the shred (ADR 0017 §8.4).
                        purged: self
                            .purge()
                            .is_ok_and(|s| s.complete() && s.through >= result.epoch),
                    },
                    other => other,
                }
            }
            Request::AccessShow => {
                if id.channel != Channel::Admin {
                    return err("ChannelRestricted", "ctx access requires the admin socket");
                }
                let access = snap.kernel.access();
                Response::Access {
                    doc: access.map(|a| a.doc.clone()),
                    ids: access
                        .map(|a| a.ids.iter().map(|(n, p)| (n.clone(), *p)).collect())
                        .unwrap_or_default(),
                    multi_user: self.principals().is_multi_user(),
                }
            }
            Request::AccessCheck {
                principal,
                key,
                branch,
            } => {
                if id.channel != Channel::Admin {
                    return err("ChannelRestricted", "ctx access requires the admin socket");
                }
                self.access_check(&snap, &principal, &key, branch.as_deref())
            }
            Request::Reindex { target } => {
                if id.channel != Channel::Admin {
                    return err("ChannelRestricted", "ctx reindex requires the admin socket");
                }
                if matches!(target, ReindexTarget::Fts | ReindexTarget::All) {
                    self.index_health.request_reindex();
                }
                if matches!(target, ReindexTarget::Vectors | ReindexTarget::All)
                    && let Some(v) = self.vectors.get()
                {
                    v.health.request_reindex();
                }
                Response::Reindexed {
                    index: self.index_report(snap.epoch),
                }
            }
            Request::Read {
                binding,
                target,
                mode,
                level,
            } => match self.bind(id, &binding, &snap) {
                Ok(b) => self.read(b, &target, mode, level, &snap),
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
            Request::Why { target, depth } => match self.resolve_plain(id.principal, &target, db) {
                Ok((l, _)) if !db.authorizes(id.principal, l) => Response::Unavailable {
                    target: target_text(&target),
                },
                Ok((_, r)) => match db.why_depth(r, depth as usize) {
                    Ok(proof) => Response::Why { proof },
                    Err(e) => err(&code(&e), format!("{e:?}")),
                },
                Err(r) => r,
            },
            Request::WhyNot { target, depth } => {
                match self.resolve_plain(id.principal, &target, db) {
                    Ok((l, _)) if !db.authorizes(id.principal, l) => Response::Unavailable {
                        target: target_text(&target),
                    },
                    Ok((_, r)) => match db.why_not(r, depth as usize) {
                        Ok(why_not) => Response::WhyNot { why_not },
                        Err(e) => err(&code(&e), format!("{e:?}")),
                    },
                    Err(r) => r,
                }
            }
            Request::Impact { target } => match self.resolve_plain(id.principal, &target, db) {
                Ok((l, _)) if !db.authorizes(id.principal, l) => Response::Unavailable {
                    target: target_text(&target),
                },
                Ok((_, r)) => match db.impact_reachable(&[r]) {
                    Ok(impact) => Response::Impact { impact },
                    Err(e) => err(&code(&e), format!("{e:?}")),
                },
                Err(r) => r,
            },
            Request::ImpactBatch { batch, blobs } => {
                drop(snap);
                let mut batch = batch;
                if branch != MAIN {
                    batch.branch = branch;
                }
                batch.channel = id.channel;
                batch.principal = id.principal;
                if let Some(r) = certificate_reads(&batch.ops) {
                    return r;
                }
                if let Err(r) = self.possession(id.principal, batch.branch, &batch.ops, &blobs) {
                    return r;
                }
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
                strict,
            } => match self.bind(id, &binding, &snap) {
                Ok(b) => self.check_gated(
                    b,
                    cert_id,
                    live_pos.into_iter().collect(),
                    action,
                    mode,
                    harness_version,
                    strict,
                    &snap,
                ),
                Err(r) => r,
            },
            Request::HandoffBegin {
                binding,
                summary,
                to_agent,
                to,
            } => match self.bind(id, &binding, &snap) {
                Ok(b) => self.handoff_begin(b, summary, to_agent, to, &snap),
                Err(r) => r,
            },
            Request::HandoffAccept { binding, id: h } => match self.bind(id, &binding, &snap) {
                Ok(b) => self.handoff_accept(b, h, &snap),
                Err(r) => r,
            },
            // Handled by `handle`, which pins no snapshot while it waits.
            Request::Changes { .. } => err("InvalidArgument", "ctx_changes cannot be nested"),
            Request::Diff { from, to } => {
                let result = if from.min(to) >= db.initial_epoch() {
                    db.diff(from, to)
                } else {
                    match snap.log.replay_kernel_to(from.max(to)) {
                        Ok(full) => match full.branch(branch) {
                            Some(v) => v.diff(from, to),
                            None => Err(TxError::UnknownBranch),
                        },
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
            Request::AnchorState { scope, branch } => {
                self.anchor_state(id, &scope, branch.as_deref(), &snap)
            }
            Request::CheckAnchors {
                scope,
                paths,
                expected_head,
                expected_position,
            } => self.check_anchors(
                id,
                &scope,
                &paths,
                &expected_head,
                &expected_position,
                &snap,
            ),
            Request::OnBranch { branch, request } => {
                let on = match branch {
                    BranchSel::Named(name) => match snap.kernel.branch_by_name(&name) {
                        Some(b) => b,
                        None => return err("UnknownBranch", format!("no branch named {name}")),
                    },
                    BranchSel::Session(binding) => match self.bind(id, &binding, &snap) {
                        Ok((_, s, _, _)) => s.lock().expect("session").branch,
                        Err(r) => return r,
                    },
                };
                match *request {
                    inner @ (Request::Why { .. }
                    | Request::WhyNot { .. }
                    | Request::Impact { .. }
                    | Request::ImpactBatch { .. }
                    | Request::Diff { .. }
                    | Request::Status
                    | Request::Validate { .. }) => self.dispatch_at(id, inner, snap, on),
                    _ => err("InvalidRequest", "OnBranch carries only unbound requests"),
                }
            }
            Request::Commit {
                binding,
                ops,
                blobs,
                expect_cert_valid,
                client_txn_id,
            } => match self.bind(id, &binding, &snap) {
                Ok(b) => self.commit_ops(id, b, ops, blobs, expect_cert_valid, client_txn_id, snap),
                Err(r) => r,
            },
            Request::WhatIf {
                binding,
                ops,
                blobs,
            } => match self.bind(id, &binding, &snap) {
                Ok((_, s, _, _)) => {
                    if let Some(r) = certificate_reads(&ops) {
                        return r;
                    }
                    if let Some(r) = agent_validation(id, &ops) {
                        return r;
                    }
                    let mut batch = Batch::new(ops);
                    batch.branch = s.lock().expect("session").branch;
                    batch.channel = id.channel;
                    batch.principal = id.principal;
                    if let Err(r) = self.possession(id.principal, batch.branch, &batch.ops, &blobs)
                    {
                        return r;
                    }
                    drop(snap);
                    match self.actor.request(|reply| Message::WhatIf { batch, reply }) {
                        Ok(Ok((result, merge))) => Response::WhatIf { result, merge },
                        Ok(Err(e)) => {
                            let (c, d) = store_code(&e);
                            err(&c, d)
                        }
                        Err(e) => submit_err(e),
                    }
                }
                Err(r) => r,
            },
            Request::BranchCreate {
                name,
                from,
                client_txn_id,
            } => {
                let from = match from.as_deref().map(|f| snap.kernel.branch_by_name(f)) {
                    None => MAIN,
                    Some(Some(b)) => b,
                    Some(None) => return err("UnknownBranch", "no such source branch"),
                };
                drop(snap);
                let mut batch = Batch::new(vec![Op::CreateBranch { name }]);
                batch.branch = from;
                self.commit(id, Vec::new(), batch, client_txn_id)
            }
            Request::BranchList => Response::Branches {
                branches: (snap.kernel.branches())
                    .map(|b| BranchEntry {
                        id: b.id,
                        name: b.name.clone(),
                        parent: b.parent,
                        fork_epoch: b.fork_epoch,
                        archived: b.archived,
                    })
                    .collect(),
            },
            Request::BranchArchive {
                name,
                client_txn_id,
            } => {
                let Some(on) = snap.kernel.branch_by_name(&name) else {
                    return err("UnknownBranch", format!("no branch named {name}"));
                };
                drop(snap);
                let mut batch = Batch::new(vec![Op::ArchiveBranch { name }]);
                batch.branch = on;
                self.commit(id, Vec::new(), batch, client_txn_id)
            }
            Request::Merge {
                binding,
                source,
                into,
                require_certified,
                certify,
                client_txn_id,
            } => {
                let target = match (&into, &binding) {
                    (Some(name), _) => match snap.kernel.branch_by_name(name) {
                        Some(b) => b,
                        None => return err("UnknownBranch", format!("no branch named {name}")),
                    },
                    (None, Some(b)) => match self.bind(id, b, &snap) {
                        Ok((_, s, _, _)) => s.lock().expect("session").branch,
                        Err(r) => return r,
                    },
                    (None, None) => MAIN,
                };
                // Branches are not secret: an unknown source is InvalidMerge,
                // after the batch-level BranchArchived (§9). Branches are
                // never deleted, so no retry of a committed merge lands here.
                let Some(source) = snap.kernel.branch_by_name(&source) else {
                    if snap
                        .kernel
                        .branch(target)
                        .is_some_and(|b| b.info().archived)
                    {
                        return err("BranchArchived", "the target branch is archived");
                    }
                    return err("InvalidMerge", format!("no branch named {source}"));
                };
                drop(snap);
                let mut batch = Batch::new(vec![Op::Merge {
                    source,
                    require_certified,
                    certify: certify.into_iter().map(LogicalRef::Key).collect(),
                }]);
                batch.branch = target;
                match self.commit(id, Vec::new(), batch, client_txn_id) {
                    Response::Committed { result } => {
                        let report = (self.actor.snapshot().log.read_row(result.epoch).ok())
                            .and_then(|row| row.trailer().ok().flatten())
                            .and_then(|t| t.merge)
                            .unwrap_or_default();
                        Response::Merged { result, report }
                    }
                    other => other,
                }
            }
            Request::Status => Response::Status {
                revisions: db.revision_count() as u64,
                logicals: db.logical_count() as u64,
                sessions: self.session_count() as u64,
                index: self.index_report(snap.epoch),
                purge_pending: self.purge_pending(&snap),
            },
            Request::TxnStatus { id: txn } => match self
                .actor
                .request(|reply| Message::TxnStatus { id: txn, reply })
            {
                // A non-owner sees only its own rows; any other id answers
                // as an unknown one (ADR 0017 §5.5).
                Ok(Ok(contextdb_store::TxnStatus::Committed {
                    epoch,
                    result,
                    principal,
                })) if crate::principals::class(principal)
                    == crate::principals::class(id.principal) =>
                {
                    Response::TxnStatus {
                        committed: Some((epoch, *result)),
                    }
                }
                Ok(Ok(contextdb_store::TxnStatus::Committed { .. })) => {
                    Response::TxnStatus { committed: None }
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
                label,
                rationale,
                evidence,
                from_review,
            } => {
                // Admin validation only (ADR 0008 §0.4, SPEC §4.4).
                if id.channel != Channel::Admin {
                    return err(
                        "ChannelRestricted",
                        "ctx validate requires the admin socket",
                    );
                }
                // A converted suggestion must be an open PendingReview on the
                // branch (ADR 0015 §4.2).
                if let Some(a) = from_review {
                    match snap.view(branch).artifact(a) {
                        Some(r) if r.ty == ArtifactType::PendingReview && r.open => {}
                        Some(_) => {
                            return err(
                                "InvalidArgument",
                                format!("artifact {} is not an open PendingReview", a.0),
                            );
                        }
                        None => return err("UnknownRef", format!("no artifact {}", a.0)),
                    }
                }
                drop(snap);
                let meta = contextdb_proto::docs::ValidationMeta {
                    v: 1,
                    kind: contextdb_proto::docs::ValidatorKind::Admin,
                    validator: validator.clone(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    model: None,
                    prompt_sha256: None,
                    label: label.unwrap_or(match verdict {
                        Verdict::Rejected => contextdb_proto::docs::Label::Conflict,
                        _ => contextdb_proto::docs::Label::Reinforcement,
                    }),
                    rationale,
                    evidence,
                    tokens_in: None,
                    tokens_out: None,
                    job: None,
                    based_on: from_review,
                    at: unix_now(),
                };
                let (hash, bytes) = match contextdb_proto::docs::blob(&meta) {
                    Ok(b) => b,
                    Err(e) => return err("InvalidArgument", e),
                };
                let mut ops = vec![Op::RecordValidation {
                    rule: rule.into(),
                    curvec_seen,
                    verdict,
                    validator,
                    meta: Some(hash),
                    if_absent: false,
                }];
                if let Some(a) = from_review {
                    ops.push(Op::ResolveArtifact {
                        id: a.into(),
                        note: None,
                    });
                }
                let mut batch = Batch::new(ops);
                batch.branch = branch;
                self.commit(id, vec![bytes], batch, None)
            }
            Request::Raw { blobs, batch } => {
                drop(snap);
                if id.channel == Channel::Agent {
                    return err(
                        "ChannelRestricted",
                        "raw batches require the observer or admin socket",
                    );
                }
                if let Some(r) = certificate_reads(&batch.ops) {
                    return r;
                }
                self.commit(id, blobs, batch, None)
            }
            Request::Conflicts {
                binding,
                branch,
                scope,
                state,
                limit,
            } => self.conflicts_list(id, binding, branch, scope, state, limit, snap),
            Request::ConflictOpen {
                binding,
                branch,
                a,
                b,
                reason,
                evidence,
                from,
                expect_cert_valid,
                client_txn_id,
            } => self.conflict_open(
                id,
                binding,
                branch,
                a,
                b,
                reason,
                evidence,
                from,
                expect_cert_valid,
                client_txn_id,
                snap,
            ),
            Request::Resolve {
                binding,
                branch,
                artifacts,
                note,
                expect_cert_valid,
                client_txn_id,
            } => self.resolve(
                id,
                binding,
                branch,
                artifacts,
                note,
                expect_cert_valid,
                client_txn_id,
                snap,
            ),
            Request::Validators { action } => {
                drop(snap);
                if id.channel != Channel::Admin {
                    return err(
                        "ChannelRestricted",
                        "ctx validators requires the admin socket",
                    );
                }
                match self.validators.get() {
                    Some(s) => match s.report(action) {
                        Ok(report) => Response::Validators { report },
                        Err(e) => err("InvalidArgument", e),
                    },
                    None => err("Unavailable", "this daemon runs no validator jobs"),
                }
            }
            Request::Shutdown => {
                if id.channel != Channel::Admin {
                    return err("ChannelRestricted", "shutdown requires the admin socket");
                }
                if let Some(s) = self.validators.get() {
                    s.stop();
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
                branch,
                git_branch,
            } => match self.attach(
                id,
                harness,
                harness_session,
                repo,
                head_sha,
                (branch, git_branch),
                &snap,
            ) {
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
            } => match binding.map(|b| self.bind(id, &b, &snap)).transpose() {
                Ok(bound) => {
                    let branch =
                        bound.map_or(MAIN, |(_, s, _, _)| s.lock().expect("session").branch);
                    self.briefing(budget_chars as usize, branch, &snap)
                }
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
    /// Ends a live session: removed from `sessions`, then (with `sessions`
    /// released, review B1) kept among the ended for late facts.
    fn end_session(&self, id: Identity, session: SessionId, epoch: Epoch) -> Result<(), Response> {
        let s = match self.session(&session) {
            None => return Err(err("UnknownSession", "unknown session")),
            Some(s)
                if crate::principals::class(s.lock().expect("session").principal)
                    != crate::principals::class(id.principal) =>
            {
                return Err(err(
                    "SessionOfAnotherPrincipal",
                    "the session belongs to another principal",
                ));
            }
            Some(s) => s,
        };
        s.lock().expect("session").ended_epoch = Some(epoch);
        // Among the ended before leaving the live map, so a concurrent lookup
        // always finds it in one of them (closure check N4).
        self.hooks.lock().expect("hooks").end(session, s);
        self.sessions.lock().expect("sessions").remove(&session);
        self.log(vec![SessionEvent::End {
            session_id: session,
            epoch,
        }]);
        Ok(())
    }
    /// The live session of a harness session key, started (durably, with
    /// its key) if there is none (ADR 0010 §2.1).
    #[allow(clippy::too_many_arguments)]
    fn attach(
        &self,
        id: Identity,
        harness: String,
        harness_session: String,
        repo: String,
        head_sha: String,
        (branch, git_branch): (Option<String>, Option<String>),
        snap: &Arc<Snapshot<D>>,
    ) -> Result<Bound, Response> {
        let key = (
            crate::principals::class(id.principal),
            harness.clone(),
            harness_session.clone(),
        );
        // Lock order (review B1): `sessions` and `hooks` are never held
        // together here; creation is serialised by `attach_lock` (review M1).
        let live = |this: &Self| {
            let sid = this.hooks.lock().expect("hooks").keys.get(&key).copied()?;
            this.session(&sid).map(|s| (sid, s))
        };
        let found = |(sid, s): (SessionId, Arc<Mutex<Session>>)| {
            // A session an MCP call started has no repository yet; the
            // first hook supplies it (drift compares repositories).
            {
                let mut st = s.lock().expect("session");
                if st.repo.is_empty() && !repo.is_empty() {
                    st.repo = repo.clone();
                    st.head_sha = head_sha.clone();
                }
            }
            Ok((sid, s, None, false))
        };
        if let Some(hit) = live(self) {
            return found(hit);
        }
        let _create = self.attach_lock.lock().expect("attach");
        if let Some(hit) = live(self) {
            return found(hit);
        }
        let branch =
            resolve_branch_for(snap, branch.as_deref(), git_branch.as_deref(), id.principal)?;
        let session_id = random_id();
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
        let session = {
            let mut st = Session::new(
                id.principal,
                head_sha.clone(),
                repo.clone(),
                snap.epoch,
                branch,
            );
            st.key = Some((harness.clone(), harness_session.clone()));
            Arc::new(Mutex::new(st))
        };
        self.sessions
            .lock()
            .expect("sessions")
            .insert(session_id, session.clone());
        self.hooks
            .lock()
            .expect("hooks")
            .keys
            .insert(key.clone(), session_id);
        self.log(vec![
            SessionEvent::Start {
                session_id,
                parent: None,
                harness: harness.clone(),
                repo,
                branch,
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
    /// The current uid → principal mapping (ADR 0017 §3.3).
    /// Lets this service enter multi-user mode (ADR 0017 §3.2): without it
    /// a committed access document is kept but no other uid is mapped.
    pub fn enable_multi_user(&self, fs: crate::principals::MultiUser) {
        let _ = self.multi_user.set(fs);
    }
    /// The daemon's checks of a `SetAccess` before the kernel's: the owner
    /// entry carries the daemon's uid, and with multi-user mode enabled the
    /// file-system rules hold for every uid it maps.
    fn access_precheck(&self, doc: &AccessDoc) -> Result<(), Response> {
        let uid = contextdb_proto::fsguard::current_uid();
        if doc.principals.get(&doc.owner) != Some(&uid) {
            return Err(err(
                "InvalidAccess",
                format!(
                    "the owner entry {} must carry the daemon's uid {uid}",
                    doc.owner
                ),
            ));
        }
        let others: Vec<u32> = (doc.principals.iter())
            .filter(|(n, _)| **n != doc.owner)
            .map(|(_, u)| *u)
            .collect();
        if let Some(fs) = self.multi_user.get()
            && !others.is_empty()
        {
            let v = fs.check(others.into_iter());
            if !v.is_empty() {
                let list: Vec<String> = v.iter().map(|v| v.to_string()).collect();
                return Err(err(
                    "InvalidAccess",
                    format!("multi-user mode needs: {}", list.join("; ")),
                ));
            }
        }
        Ok(())
    }
    /// Applies the committed access document's mapping (at start and after
    /// every `SetAccess`). Without multi-user mode enabled nothing changes.
    /// On any failure the daemon falls back to single-user mode (fail closed).
    pub fn apply_access(&self) -> Result<(), Vec<contextdb_proto::fsguard::Violation>> {
        let Some(fs) = self.multi_user.get() else {
            return Ok(());
        };
        let snap = self.actor.snapshot();
        let result = crate::principals::map_of(snap.kernel.access(), fs.owner_uid)
            .map_err(|e| {
                vec![contextdb_proto::fsguard::Violation::Io {
                    path: fs.dir.clone(),
                    error: e,
                }]
            })
            .and_then(|map| {
                if map.is_multi_user() {
                    let v = fs.check(map.uids());
                    if !v.is_empty() {
                        return Err(v);
                    }
                }
                Ok(map)
            });
        let (map, out) = match result {
            Ok(map) => (map, Ok(())),
            Err(v) => (crate::principals::PrincipalMap::default(), Err(v)),
        };
        let multi = map.is_multi_user();
        self.set_principals(map);
        fs.chmod(multi).map_err(|v| vec![v])?;
        out
    }
    /// `ctx access check` (ADR 0017 §4.1).
    fn access_check(
        &self,
        snap: &Snapshot<D>,
        principal: &str,
        key: &str,
        branch: Option<&str>,
    ) -> Response {
        let access = snap.kernel.access();
        let p = match access.and_then(|a| a.principal_of_name(principal)) {
            Some(p) => p,
            None => return err("UnknownPrincipal", principal.to_owned()),
        };
        let b = match branch {
            None => MAIN,
            Some(name) => match snap.kernel.branch_by_name(name) {
                Some(b) => b,
                None => return err("UnknownBranch", name.to_owned()),
            },
        };
        let view = snap.view(b);
        let Some(l) = view.logical_by_key(key) else {
            return err("UnknownRef", key.to_owned());
        };
        let (Some((_, _, lane)), Some(owner)) = (view.logical_info(l), view.logical_owner(l))
        else {
            return err("UnknownRef", key.to_owned());
        };
        let dirs = view.metadata(l).unwrap_or_default().dirs;
        let d = contextdb_core::access::authorized(
            access,
            p,
            &contextdb_core::access::LogicalFacts {
                key,
                lane: &lane,
                owner,
                dirs: &dirs,
            },
        );
        Response::AccessDecision {
            principal: p,
            allowed: d.allowed,
            step: d.step.name().to_owned(),
        }
    }
    pub fn principals(&self) -> Arc<crate::principals::PrincipalMap> {
        self.principals.read().expect("principals").clone()
    }
    /// Replaces the mapping. The daemon checks the file system before it
    /// enters multi-user mode (`Daemon::set_principals`).
    pub fn set_principals(&self, map: crate::principals::PrincipalMap) {
        *self.principals.write().expect("principals") = Arc::new(map);
    }
    /// Resolves a binding to (session, agent) (ADR 0010 §2).
    pub(crate) fn bind(
        &self,
        id: Identity,
        b: &Binding,
        snap: &Arc<Snapshot<D>>,
    ) -> Result<Bound, Response> {
        match b {
            Binding::Session { session, agent } => {
                let Some(s) = self.session(session) else {
                    return Err(err("UnknownSession", "unknown session"));
                };
                same_principal(id, &s)?;
                {
                    let mut s = s.lock().expect("session");
                    crate::redact::note_branch(s.branch);
                    if let Some(a) = agent {
                        s.known_agents.insert(a.clone());
                    }
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
                    (None, None),
                    snap,
                )?;
                // Only ids a hook reported are children; any other is the
                // root (ADR 0010 §2.2).
                let agent = {
                    let s = s.lock().expect("session");
                    crate::redact::note_branch(s.branch);
                    agent.clone().filter(|a| s.known_agents.contains(a))
                };
                Ok((sid, s, agent, created))
            }
            Binding::Call { id: call } => {
                let claimed = self.hooks.lock().expect("hooks").claims.get(call).cloned();
                match claimed {
                    Some((sid, agent)) => match self.session(&sid) {
                        Some(s) => {
                            same_principal(id, &s)?;
                            crate::redact::note_branch(s.lock().expect("session").branch);
                            Ok((sid, s, agent, false))
                        }
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
    /// A `SetAccess` is checked against the daemon first (its owner entry's
    /// uid, and the file-system rules if multi-user mode may be entered),
    /// and its mapping is applied once it is durable (ADR 0017 §3.2, §4.1).
    pub(crate) fn commit(
        &self,
        id: Identity,
        blobs: Vec<Vec<u8>>,
        mut batch: Batch,
        txn: Option<[u8; 16]>,
    ) -> Response {
        let access = (batch.ops.iter()).find_map(|op| match op {
            Op::SetAccess { doc } => Some(doc),
            _ => None,
        });
        let sets_access = access.is_some();
        if let Some(doc) = access
            && let Err(r) = self.access_precheck(doc)
        {
            return r;
        }
        batch.channel = id.channel;
        batch.principal = id.principal;
        batch.client_txn_id = Some(txn.unwrap_or_else(random_id));
        match self.actor.write(blobs, batch) {
            Ok(Ok(result)) => {
                if sets_access && let Err(v) = self.apply_access() {
                    eprintln!("ctxd: access document committed but not applied: {v:?}");
                }
                Response::Committed { result }
            }
            Ok(Err(e)) => {
                let (c, d) = store_code(&e);
                err(&c, d)
            }
            Err(e) => submit_err(e),
        }
    }
    /// `ctx_commit` (ADR 0013 §4).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_ops(
        &self,
        id: Identity,
        (session, s, agent, _): Bound,
        ops: Vec<Op>,
        blobs: Vec<Vec<u8>>,
        expect_cert_valid: bool,
        client_txn_id: Option<[u8; 16]>,
        snap: Arc<Snapshot<D>>,
    ) -> Response {
        if let Some(r) = certificate_reads(&ops) {
            return r;
        }
        if let Some(r) = agent_validation(id, &ops) {
            return r;
        }
        if ops.iter().any(|op| {
            matches!(
                op,
                Op::CreateBranch { .. }
                    | Op::ArchiveBranch { .. }
                    | Op::Merge { .. }
                    | Op::SetAccess { .. }
                    | Op::ShredContent { .. }
            )
        }) {
            return err(
                "InvalidRequest",
                "branch ops, SetAccess and ShredContent have their own requests",
            );
        }
        let (branch, cert) = {
            let s = s.lock().expect("session");
            let cert = (s.agents.get(&agent)).and_then(|a| a.session_cert.clone());
            (s.branch, cert)
        };
        if let Err(r) = self.possession(id.principal, branch, &ops, &blobs) {
            return r;
        }
        let expect = match (expect_cert_valid, cert) {
            (false, _) => None,
            // A session with no certificate expects nothing valid (§4 step 2).
            (true, None) => {
                return Response::Refused {
                    validation: Validation::Empty,
                };
            }
            (true, Some(cert)) => Some(crate::actor::Expect {
                live: self.anchor_live(&snap.view(branch)),
                cert,
            }),
        };
        drop(snap);
        // The idempotency hash covers the request as sent: its ops and flag,
        // and the branch, channel and principal it commits as (the batch's
        // own fields). Not the certificate: a later ctx_check may re-issue
        // it, and a retry of a durable commit must still find its result.
        // Never the appended read set.
        let mut client = Vec::new();
        ops.encode(&mut client);
        expect_cert_valid.encode(&mut client);
        branch.encode(&mut client);
        id.channel.encode(&mut client);
        id.principal.encode(&mut client);
        let client_hash = *blake3::hash(&client).as_bytes();
        let mut batch = Batch::new(ops);
        batch.branch = branch;
        batch.channel = id.channel;
        batch.principal = id.principal;
        let txn = client_txn_id.unwrap_or_else(random_id);
        batch.client_txn_id = Some(txn);
        let with_deps = (batch.ops.iter()).any(|op| matches!(op, Op::AddRule { .. }));
        match self
            .actor
            .write_expecting(blobs, batch, client_hash, expect)
        {
            Ok(Ok(result)) => {
                // Session writes count ctx_commit like record and supersede.
                let reason = "commit".into();
                self.wrote(session, &s, &agent, txn, result.epoch, reason, with_deps);
                Response::Committed { result }
            }
            Ok(Err(StoreError::CertificateNotValid(validation))) => Response::Refused {
                validation: *validation,
            },
            Ok(Err(e)) => {
                let (c, d) = store_code(&e);
                err(&c, d)
            }
            Err(e) => submit_err(e),
        }
    }
    fn item_at(
        &self,
        db: &BranchView<'_>,
        l: LogicalId,
        r: RevisionId,
        handle: Option<String>,
        live: &BTreeMap<String, Vec<u8>>,
    ) -> Item {
        let (key, kind, _) = db.logical_info(l).expect("existing logical");
        let state = db.node(r).expect("existing revision");
        let summary = db.summary_at(r, live).expect("existing revision");
        let premises = (kind == RecordKind::Summary)
            .then(|| premise_count(db, r))
            .flatten();
        Item {
            logical: l,
            key,
            kind,
            rev: r,
            state,
            summary,
            handle,
            premises,
            body: None,
        }
    }
    /// Proof of possession for a principal other than the store owner (ADR
    /// 0017 §6.3), before the kernel and for dry runs alike: every content
    /// hash must come with matching bytes, unless it already belongs to a
    /// revision of a logical the principal may read on `branch`. Otherwise
    /// `MissingBlob`, the answer for a hash nobody has.
    fn possession(
        &self,
        p: PrincipalId,
        branch: BranchId,
        ops: &[Op],
        blobs: &[Vec<u8>],
    ) -> Result<(), Response> {
        if p.is_store_owner() {
            return Ok(());
        }
        let sent: BTreeSet<ContentHash> = blobs.iter().map(|b| content_hash(b)).collect();
        let snap = self.actor.snapshot();
        let view = snap.view(branch);
        for content in content_hashes(ops) {
            if sent.contains(&content) {
                continue;
            }
            // A shredded hash proves nothing without its bytes (T107).
            let readable = !snap.kernel.is_shredded(&content)
                && (view.revisions_with_content(&content).into_iter())
                    .filter_map(|r| view.revision_logical(r))
                    .any(|l| view.authorizes(p, l));
            if !readable {
                let e = TxError::MissingBlob { content };
                return Err(err(&code(&e), format!("{e:?}")));
            }
        }
        Ok(())
    }
    /// Target → (logical, revision) without session context (no handles).
    /// A target → `(logical, revision)` for `principal`. A revision target
    /// whose logical the principal cannot read is `Unavailable{target}`
    /// whatever the revision, so answers never map revision ids to hidden
    /// logicals (ADR 0017 §5.7; security review M3).
    pub(crate) fn resolve_plain(
        &self,
        principal: PrincipalId,
        target: &Target,
        db: &BranchView<'_>,
    ) -> Result<(LogicalId, RevisionId), Response> {
        let hidden = |l: Option<LogicalId>| {
            l.is_some_and(|l| db.logical_info(l).is_some() && !db.authorizes(principal, l))
        };
        match target {
            Target::Pinned { logical, .. } if hidden(Some(*logical)) => {
                return Err(Response::Unavailable {
                    target: target_text(target),
                });
            }
            Target::KeyRev { key, .. } if hidden(db.logical_by_key(key)) => {
                return Err(Response::Unavailable {
                    target: target_text(target),
                });
            }
            _ => {}
        }
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
    #[allow(clippy::too_many_arguments)]
    fn recall(
        &self,
        (session, s, agent, _): Bound,
        query: &str,
        scope: &str,
        opts: &RecallOpts,
        mode: Mode,
        limit: usize,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        let limit_clamped = limit > MAX_RECALL;
        let limit = limit.clamp(1, MAX_RECALL);
        let (branch, principal) = {
            let s = s.lock().expect("session");
            (s.branch, s.principal)
        };
        let db = &snap.view(branch);
        let live = self.anchor_live(db);
        let mut elig = Eligibility {
            service: self,
            db,
            lane: scope,
            opts,
            mode,
            limit,
            live: &live,
            viewer: crate::redact::Viewer::new(principal, db),
            decided: BTreeMap::new(),
            withheld: Vec::new(),
        };
        // ADR 0016 §3.2: eligibility is decided inside each ranker, before
        // any cut, and each ranker stops once it holds `limit` eligible
        // candidates or runs out.
        //
        // Key ranker: exact, then prefix, then substring, over every
        // logical on the branch.
        let mut keyed: Vec<(ranking::KeyRank, LogicalId)> = db
            .logicals()
            .filter_map(|l| {
                let (key, ..) = db.logical_info(l)?;
                Some((ranking::KeyRank::of(&key, query)?, l))
            })
            .collect();
        keyed.sort();
        let mut by_key = Vec::new();
        for (rank, l) in keyed {
            if by_key.len() == limit {
                break;
            }
            if elig.admit(l, None) {
                by_key.push((rank, l));
            }
        }
        // bm25: the AND query, then the OR query, paged. The index may lag
        // the snapshot; a hit counts only while its revision is the head
        // on the session's branch (main rows the branch shadows drop out).
        let branch = db.info().id;
        let page = limit * 4 + 16;
        let mut by_text: Vec<LogicalId> = Vec::new();
        // One read snapshot for every page (P8 M7 review M1): a write
        // between pages would shift rows past a page boundary.
        self.recall_query().read_snapshot(|index| {
            let any = contextdb_store::recall::terms(query);
            'phases: for phase in 0..2 {
                let mut offset = 0;
                loop {
                    let hits = if phase == 0 {
                        index.search_page(query, scope, branch, page, offset)
                    } else {
                        index.any_of_page(&any, scope, branch, page, offset)
                    };
                    // An index error ends this ranker; its state is in
                    // the response's `index` report.
                    let Ok(hits) = hits else { break };
                    (self.text_hits_scanned).fetch_add(hits.len() as u64, Ordering::Relaxed);
                    for h in &hits {
                        if !by_text.contains(&h.logical) && elig.admit(h.logical, Some(h.rev)) {
                            by_text.push(h.logical);
                            if by_text.len() == limit {
                                break 'phases;
                            }
                        }
                    }
                    if hits.len() < page {
                        break;
                    }
                    offset += page;
                }
            }
        });
        // Vectors (ADR 0016 §7.2), when an embedder is configured: cosine
        // order over the heads visible on the branch, paged through
        // eligibility like the others.
        let mut by_vector: Vec<LogicalId> = Vec::new();
        if let Some(v) = self.vectors.get()
            && !query.trim().is_empty()
            && let Some(q) = v.query_vector(query)
        {
            for (l, r) in crate::embedding::ranked(v, db, &q) {
                if by_vector.len() == limit {
                    break;
                }
                if elig.admit(l, Some(r)) {
                    by_vector.push(l);
                }
            }
        }
        let fused = ranking::fuse(&by_key, &[&by_text, &by_vector]);
        let Eligibility {
            decided, withheld, ..
        } = elig;
        let mut decided = decided;
        let mut items: Vec<Item> = (fused.into_iter())
            .filter_map(|l| decided.remove(&l).flatten())
            .take(limit)
            .collect();
        (self.recall_items).fetch_add(items.len() as u64, Ordering::Relaxed);
        // Packing (ADR 0016 §5.2). Without a level or a budget the
        // response keeps the header-only shape.
        let (mut omitted, mut estimate) = (0, None);
        if opts.level.is_some() || opts.budget_tokens.is_some() {
            let want = opts.level.unwrap_or(levels::Level::Outline);
            let texts: Vec<packing::Texts> = (items.iter())
                .map(|i| self.texts(snap, db, i, want))
                .collect();
            let (rendered, left_out) = packing::pack(&texts, want, opts.budget_tokens);
            items.truncate(rendered.len());
            for (item, r) in items.iter_mut().zip(rendered) {
                item.body = Some(r);
            }
            omitted = left_out as u32;
            estimate = opts.budget_tokens.map(|_| packing::ESTIMATE.to_string());
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
                level: item
                    .body
                    .as_ref()
                    .map_or(levels::Level::Header, |b| b.level),
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
            for entry in &entries {
                if let Ok(cert) = db.issue_certificate(&request, std::slice::from_ref(entry), &live)
                {
                    let proof = self.root_proof(cert, db);
                    s.proof_history.push(proof.clone());
                    s.agent(&agent).proofs.insert(entry.logical, proof.clone());
                    events.push(SessionEvent::RootProof {
                        agent: agent.clone(),
                        proof: Box::new(proof),
                    });
                }
            }
            if let Ok(cert) = db.issue_certificate(&request, &entries, &live) {
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
            index: self.index_report(snap.epoch),
            limit_clamped,
            omitted,
            estimate,
        }
    }
    /// An item's renderings for packing: abstract and outline from the
    /// recall projection when it holds this revision, else recomputed from
    /// the payload; the full text only when `full` is wanted.
    fn texts(
        &self,
        snap: &Arc<Snapshot<D>>,
        db: &BranchView<'_>,
        item: &Item,
        want: levels::Level,
    ) -> packing::Texts {
        let payload = || match db
            .revision_content(item.rev)
            .map(|c| snap.log.read_blob(&c))
        {
            Ok(Payload::Available(bytes)) => bytes,
            _ => Vec::new(),
        };
        let stored = (want < levels::Level::Full)
            .then(|| {
                (self.recall_query())
                    .levels(item.logical, item.rev)
                    .ok()
                    .flatten()
            })
            .flatten();
        let empty = levels::Rendered {
            text: String::new(),
            cut: false,
        };
        match stored {
            Some(l) => packing::Texts {
                key: item.key.clone(),
                abstract_: l.abstract_,
                outline: l.outline,
                full: empty,
            },
            None => {
                let body = payload();
                packing::Texts {
                    key: item.key.clone(),
                    abstract_: levels::abstract_of(&body),
                    outline: levels::outline_of(&body),
                    full: if want == levels::Level::Full {
                        levels::render(&body, levels::Level::Full)
                    } else {
                        empty
                    },
                }
            }
        }
    }
    /// The recall projection's stored renderings of `rev` (T87: equal to
    /// recomputing them from the payload).
    /// The level a session's root agent holds for `logical` in its ledger
    /// (T88), and the revision.
    #[doc(hidden)]
    pub fn ledger_level(
        &self,
        session: SessionId,
        logical: LogicalId,
    ) -> Option<(RevisionId, levels::Level)> {
        let s = self
            .sessions
            .lock()
            .expect("sessions")
            .get(&session)?
            .clone();
        let mut s = s.lock().expect("session");
        let e = s.agent(&None).required.get(&logical)?;
        Some((e.rev, e.level))
    }
    #[doc(hidden)]
    pub fn recall_levels(
        &self,
        logical: LogicalId,
        rev: RevisionId,
    ) -> Option<contextdb_store::recall::Levels> {
        self.recall
            .lock()
            .expect("recall")
            .levels(logical, rev)
            .ok()
            .flatten()
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
            branch: s.branch,
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
        level: Option<levels::Level>,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        let level = level.unwrap_or(levels::Level::Full);
        let db = &snap.view(s.lock().expect("session").branch);
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
            other => {
                let principal = s.lock().expect("session").principal;
                self.resolve_plain(principal, other, db)
            }
        };
        let (l, r) = match resolved {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        // A hidden logical: the target echoed, no item, no ledger entry,
        // no certificate (ADR 0017 §5.2).
        let principal = s.lock().expect("session").principal;
        if !db.authorizes(principal, l) {
            return Response::Unavailable {
                target: target_text(target),
            };
        }
        let live = self.anchor_live(db);
        let item = self.item_at(db, l, r, None, &live);
        let certified = item.summary == Summary::Certified && db.head(l) == Some(r);
        let required = mode == Mode::Certified && certified;
        let entry = LedgerEntry {
            logical: l,
            rev: r,
            required,
            epoch: snap.epoch,
            level,
        };
        {
            let mut s = s.lock().expect("session");
            s.agent(&agent).record(entry.clone());
            let mut events = vec![read_event(session, &agent, &entry)];
            if required {
                let request = match self.issue_request(session, &s, snap.epoch, false) {
                    Ok(r) => r,
                    Err(r) => return r,
                };
                let item = CertItem {
                    logical: l,
                    rev: r,
                    required: true,
                    via: Via::Head,
                    since_epoch: snap.epoch,
                };
                if let Ok(cert) = db.issue_certificate(&request, &[item], &live) {
                    let proof = self.root_proof(cert, db);
                    s.proof_history.push(proof.clone());
                    s.agent(&agent).proofs.insert(l, proof.clone());
                    events.push(SessionEvent::RootProof {
                        agent: agent.clone(),
                        proof: Box::new(proof),
                    });
                }
            }
            self.log(events);
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
        let (payload, unavailable) = match snap.log.read_blob(&content) {
            Payload::Available(bytes) => (Some(bytes), None),
            Payload::Unavailable(why) => (
                None,
                Some(
                    match why {
                        contextdb_store::Unavailable::Missing => "missing",
                        contextdb_store::Unavailable::Corrupt => "corrupt",
                        contextdb_store::Unavailable::Shredded => "shredded",
                    }
                    .to_owned(),
                ),
            ),
        };
        if level == levels::Level::Full {
            return Response::Read {
                item,
                payload,
                unavailable,
            };
        }
        // Below `full`: recomputed from the payload, so a stale projection
        // never serves a wrong rendering (ADR 0016 §4.1).
        let mut item = item;
        item.body = payload.map(|bytes| {
            let r = levels::render(&bytes, level);
            Rendering {
                level,
                cut: if level == levels::Level::Header {
                    !bytes.is_empty()
                } else {
                    r.cut
                },
                text: r.text,
            }
        });
        Response::Read {
            item,
            payload: None,
            unavailable,
        }
    }
    /// Deps: handles of this session's certificates (required entries) or
    /// explicit `logical@rev` (ADR 0008 §6.1, T36).
    fn deps(
        &self,
        session: &Arc<Mutex<Session>>,
        deps: &[Dep],
        db: &BranchView<'_>,
    ) -> Result<Vec<(RevRef, EdgeKind)>, Response> {
        let mut needs_flush = false;
        let mut out = Vec::new();
        {
            let s = session.lock().expect("session");
            for (n, d) in deps.iter().enumerate() {
                // A dep naming a logical the principal may not read is
                // `Unauthorized(Read)` before anything else is said about
                // it, whether or not the revision exists (ADR 0017 §6.1).
                let named = match &d.target {
                    Target::Pinned { logical, .. } | Target::Logical(logical) => Some(*logical),
                    Target::KeyRev { key, .. } | Target::Key(key) => db.logical_by_key(key),
                    Target::Handle(_) => None,
                };
                if let Some(l) = named
                    && db.logical_info(l).is_some()
                    && !db.authorizes(s.principal, l)
                {
                    let e = TxError::Unauthorized {
                        op_index: n,
                        reference: target_text(&d.target),
                        need: AccessNeed::Read,
                    };
                    return Err(err(&code(&e), format!("{e:?}")));
                }
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
                        let (logical, name) = match &d.target {
                            Target::Key(k) => (db.logical_by_key(k), k.clone()),
                            Target::Logical(l) => (Some(*l), format!("logical {}", l.0)),
                            _ => unreachable!(),
                        };
                        let hint = match logical.and_then(|l| db.head(l)) {
                            Some(h) => format!("; its current head is {name}@{}", h.0),
                            None => format!("; {name} does not exist"),
                        };
                        return Err(err(
                            "InvalidDep",
                            format!(
                                "a dep names one revision: give a handle from ctx_recall or \
                                 key@rev{hint}"
                            ),
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
        let branch = s.lock().expect("session").branch;
        let incidences = match self.deps(&s, &deps, &snap.view(branch)) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let named = dep_names(&snap.view(branch), &incidences);
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
        // A reference-only rule records references but supports nothing.
        let with_deps = incidences.iter().any(|(_, k)| *k != EdgeKind::Ref);
        if !incidences.is_empty() {
            ops.push(Op::AddRule {
                conclusion: RevRef::New(0),
                incidences,
                enabled: true,
            });
        }
        let txn = txn.unwrap_or_else(random_id);
        let mut batch = Batch::new(ops);
        batch.branch = branch;
        let response = explain_rule_error(self.commit(id, vec![body], batch, Some(txn)), &named);
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
        if !self.hooks.lock().expect("hooks").written.insert(txn) {
            return;
        }
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
        let branch = s.lock().expect("session").branch;
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
            let view = snap.view(branch);
            for (_, enabled, premises, refs) in view.rules_concluding(expected_head) {
                // The copy names every premise: one the principal may not
                // read refuses the request, without naming it (ADR 0017
                // §6.2 rule 4).
                let hidden = (premises
                    .iter()
                    .map(|p| p.premise)
                    .chain(refs.iter().copied()))
                .any(|r| {
                    view.revision_logical(r)
                        .is_none_or(|l| !view.authorizes(id.principal, l))
                });
                if enabled && hidden {
                    let e = TxError::Unauthorized {
                        op_index: ops.len(),
                        reference: "carry_rules".into(),
                        need: AccessNeed::Read,
                    };
                    return err(&code(&e), format!("{e:?}"));
                }
                if enabled {
                    ops.push(Op::AddRule {
                        conclusion: RevRef::New(0),
                        incidences: (premises.iter())
                            .map(|p| (RevRef::Existing(p.premise), p.kind))
                            .chain(refs.iter().map(|r| (RevRef::Existing(*r), EdgeKind::Ref)))
                            .collect(),
                        enabled: true,
                    });
                }
            }
        }
        drop(snap);
        let with_deps = ops.iter().any(|op| {
            matches!(op, Op::AddRule { incidences, .. }
                if incidences.iter().any(|(_, k)| *k != EdgeKind::Ref))
        });
        let txn = txn.unwrap_or_else(random_id);
        let mut batch = Batch::new(ops);
        batch.branch = branch;
        let response = self.commit(id, vec![body], batch, Some(txn));
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
        strict_req: bool,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        let db = &snap.view(s.lock().expect("session").branch);
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
            let Some(mut c) = s.certs.iter().find(|c| c.cert_id == id).cloned() else {
                return err("UnknownRef", "certificate not issued to this session");
            };
            let returned = c.clone();
            c.strict_no_aba |= strict_req;
            return match validate(&c) {
                Ok(v) => Response::Check {
                    decision: decision(&v),
                    validation: v,
                    cert: Some(Box::new(returned)),
                    superseded: Vec::new(),
                    gate: None,
                    updates_pending: 0,
                    named: Vec::new(),
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
                named: Vec::new(),
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
        // `ctx check --strict` validates (and re-issues) with strict_no_aba,
        // so no alternative support can hide an ABA (SPEC §6.4 step 1).
        let mut strict = strict_req;
        let mut prior: Option<Validation> = None;
        if let Some(mut current) = s.agents.get(&agent).and_then(|a| a.session_cert.clone()) {
            let strict_upgrade = strict_req && !current.strict_no_aba;
            current.strict_no_aba |= strict_req;
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
                        named: Vec::new(),
                    };
                }
                // Adopt a recertified certificate as the session certificate.
                let mut adopted = current.clone();
                if let Validation::ValidRecertified { new_cert, .. } = &v {
                    adopted = (**new_cert).clone();
                }
                if strict_upgrade {
                    adopted.cert_id = *blake3::hash(&adopted.body_bytes()).as_bytes();
                }
                if strict_upgrade || matches!(v, Validation::ValidRecertified { .. }) {
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
                    // Only an adopted recertification logged anything this
                    // check must make durable; an unchanged certificate already
                    // is (ADR 0010 G5: no session-log sync on steady checks).
                    if (strict_upgrade || matches!(v, Validation::ValidRecertified { .. }))
                        && let Err(r) = self.flush()
                    {
                        return r;
                    }
                    return Response::Check {
                        decision: decision(&v),
                        validation: v,
                        cert: Some(Box::new(adopted)),
                        superseded,
                        gate: None,
                        updates_pending: 0,
                        named: Vec::new(),
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
                since_epoch: since.get(&(e.logical, e.rev)).copied().unwrap_or(e.epoch),
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
                    named: Vec::new(),
                }
            }
            Err(IssueFailure::Empty) => Response::Check {
                decision: Decision::Warn,
                validation: Validation::Empty,
                cert: None,
                superseded,
                gate: None,
                updates_pending: 0,
                named: Vec::new(),
            },
            Err(_) => {
                // 3. Failed issuance → §6.4 verdict over the same entries.
                let virtual_cert = Certificate {
                    cert_id: [0; 32],
                    session_id: session,
                    principal: request.principal,
                    branch: request.branch,
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
                        named: Vec::new(),
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
        strict: bool,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        let (session, s, agent, _) = bound.clone();
        let view = snap.view(s.lock().expect("session").branch);
        let mut live = live;
        // A caller cannot replace failed local evidence with a fresh position.
        for scope in view
            .db()
            .inputs()
            .logicals
            .iter()
            .filter(|l| l.kind == RecordKind::CodeAnchor)
            .map(|l| &l.scope_lane)
        {
            live.remove(scope);
        }
        live.extend(self.anchor_live(&view));
        let mut response =
            match self.proof_check(&bound, cert_id, action.as_ref(), strict, &live, snap) {
                Some(response) => response,
                None => self.check(bound, cert_id, live.clone(), strict, snap),
            };
        let at = unix_now();
        if let Response::Check {
            validation,
            gate,
            updates_pending,
            named,
            ..
        } = &mut response
        {
            {
                let s = s.lock().expect("session");
                *updates_pending = update_items(&s, &agent, &view, &live).len() as u32;
                if !matches!(
                    validation,
                    Validation::Valid | Validation::ValidRecertified { .. }
                ) {
                    *named = named_reads(&s, &agent, &view, &live);
                }
            }
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
                        at,
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
                    at,
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
        let view = snap.view(s.lock().expect("session").branch);
        let live = self.anchor_live(&view);
        let mut s = s.lock().expect("session");
        if let Some(token) = ack {
            let st = s.agent(&agent);
            if let Some(at) = st.pending.iter().position(|(t, _)| *t == token) {
                let (_, items) = st.pending.remove(at).expect("pending");
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
        let mut items = update_items(&s, &agent, &view, &live);
        let max = if max_items == 0 { 50 } else { max_items };
        let more = items.len() > max;
        items.truncate(max);
        let token = self.tokens.fetch_add(1, Ordering::Relaxed);
        let pending = &mut s.agent(&agent).pending;
        pending.push_back((token, items.clone()));
        while pending.len() > 8 {
            pending.pop_front();
        }
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
        // A stream's facts belong to the session its first fact applied to
        // (live or ended, §5.3): late facts never land in a later session of
        // the same key, and a resumed harness session gets a new stream
        // (closure check N1). `hooks` and `sessions` one at a time (B1).
        let find = |sid: SessionId| {
            self.session(&sid).or_else(|| {
                self.hooks
                    .lock()
                    .expect("hooks")
                    .ended
                    .iter()
                    .find(|(e, _)| *e == sid)
                    .map(|(_, s)| s.clone())
            })
        };
        let owner = self
            .hooks
            .lock()
            .expect("hooks")
            .stream_owner
            .get(&stream)
            .copied();
        let (session, s) = match owner {
            Some(sid) => match find(sid) {
                Some(s) => (sid, s),
                None => {
                    // Evicted from the ended: record the facts against it
                    // without bringing it back to life.
                    let mut gone =
                        Session::new(id.principal, String::new(), String::new(), snap.epoch, MAIN);
                    gone.ended_epoch = Some(snap.epoch);
                    (sid, Arc::new(Mutex::new(gone)))
                }
            },
            None => match self.attach(
                id,
                harness,
                harness_session,
                String::new(),
                String::new(),
                (None, None),
                snap,
            ) {
                Ok((sid, s, _, _)) => {
                    self.hooks
                        .lock()
                        .expect("hooks")
                        .stream_owner
                        .insert(stream, sid);
                    (sid, s)
                }
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
        let mut ended = false;
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
                    at,
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
                    at: *at,
                }),
                _ => {}
            }
            s.lock().expect("session").note_fact(&p);
            ended |= p.fact == Fact::StreamEnd;
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
        {
            let mut hooks = self.hooks.lock().expect("hooks");
            hooks.streams.insert(stream, through);
            hooks.telemetry.extend(telemetry);
        }
        // A delivered StreamEnd ends the stream's own session if still live
        // (the hook does not end it separately: closure check N1).
        if ended && self.session(&session).is_some() {
            let _ = self.end_session(id, session, snap.epoch);
            let _ = self.flush();
        }
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
        // `sessions` and `hooks` one at a time (review B1).
        let mut others: Vec<Arc<Mutex<Session>>> = self
            .sessions
            .lock()
            .expect("sessions")
            .iter()
            .filter(|(id, _)| **id != session)
            .map(|(_, s)| s.clone())
            .collect();
        others.extend(
            self.hooks
                .lock()
                .expect("hooks")
                .ended
                .iter()
                .filter(|(id, _)| *id != session)
                .map(|(_, s)| s.clone()),
        );
        let mut declared = BTreeSet::new();
        let key = s.lock().expect("session").key.clone();
        for o in others {
            let o = o.lock().expect("session");
            // Active at the same time in the same repository, or an earlier
            // session of the same harness session (resumed after an end,
            // closure check N3).
            let concurrent = o.repo == repo && o.ended_epoch.is_none_or(|e| e >= started);
            if concurrent || (key.is_some() && o.key == key) {
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
    fn briefing(&self, budget: usize, branch: BranchId, snap: &Arc<Snapshot<D>>) -> Response {
        let db = &snap.view(branch);
        let live = self.anchor_live(db);
        let (mut stale, mut pending, mut conflicted) = (0u32, 0u32, 0u32);
        let mut summaries = Vec::new();
        for l in db.logicals() {
            let Some(r) = db.head(l) else { continue };
            match db.summary_at(r, &live) {
                Ok(Summary::Stale(_)) => stale += 1,
                Ok(Summary::Pending(_)) => pending += 1,
                Ok(Summary::Conflicted) => conflicted += 1,
                _ => {}
            }
            // ADR 0016 §6: Summaries whose premises moved.
            if summaries.len() < MAX_BRIEFING_SUMMARIES
                && db
                    .logical_info(l)
                    .is_some_and(|(_, k, _)| k == RecordKind::Summary)
                && premise_count(db, r).is_some_and(|c| c.moved > 0)
            {
                summaries.push(self.item_at(db, l, r, None, &live));
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
                let item = self.item_at(db, *l, r, None, &live);
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
            summaries,
        }
    }
}
/// Summaries listed in a briefing.
const MAX_BRIEFING_SUMMARIES: usize = 16;
/// A Summary revision's pending count (ADR 0016 §6), from its witness
/// rule: the first enabled rule concluding `r` with a Sem premise (rules
/// in id order, the canonical witness order). `moved` counts the Sem
/// premises whose current head differs from the nearest Verified vector
/// of that rule (fewest differences), or from the vector the rule
/// asserted when no Verified record exists. The kernel keeps validations
/// by vector, not by time, so "nearest" stands for "latest": it is 0
/// exactly when the current vector is verified.
fn premise_count(db: &BranchView<'_>, r: RevisionId) -> Option<PremiseCount> {
    let mut rules = db.rules_concluding(r);
    rules.sort_by_key(|(j, ..)| *j);
    let (j, ..) = rules.into_iter().find(|(_, enabled, premises, _)| {
        *enabled && premises.iter().any(|p| p.kind == EdgeKind::Sem)
    })?;
    let sem = db.rule_semantic(j)?;
    let verified: Vec<HeadVector> = (db.rule_validations(j).into_iter())
        .filter(|(_, rec)| rec.verdict == Verdict::Verified)
        .map(|(v, _)| v)
        .collect();
    let bases = if verified.is_empty() {
        vec![sem.asserted.clone()]
    } else {
        verified
    };
    let moved = |base: &HeadVector| {
        (sem.current.iter())
            .filter(|(l, r)| !base.iter().any(|(bl, br)| bl == l && br == r))
            .count()
    };
    Some(PremiseCount {
        moved: bases.iter().map(moved).min().unwrap_or(0) as u32,
        total: sem.current.len() as u32,
    })
}
impl AgentState {
    /// Ledger rule (ADR 0008 §6.2): a required read of another revision of a
    /// logical supersedes this agent's earlier one; advisory reads never
    /// downgrade.
    fn record(&mut self, e: LedgerEntry) {
        if !e.required {
            return;
        }
        let mut e = e;
        if let Some(old) = self.required.get(&e.logical) {
            if old.rev != e.rev {
                self.superseded.push((e.logical, old.rev, e.rev));
                // A re-read restarts what this agent was told about the logical.
                self.reported.remove(&e.logical);
            } else {
                // A re-read of the same revision never lowers its level.
                e.level = e.level.max(old.level);
            }
        }
        self.required.insert(e.logical, e);
    }
}
impl Session {
    fn new(
        principal: PrincipalId,
        head_sha: String,
        repo: String,
        started: Epoch,
        branch: BranchId,
    ) -> Self {
        Self {
            principal,
            head_sha,
            repo,
            branch,
            key: None,
            started_epoch: started,
            ended_epoch: None,
            agents: BTreeMap::new(),
            known_agents: BTreeSet::new(),
            certs: Vec::new(),
            proof_history: Vec::new(),
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
        self.view_with_agents(agent)
            .into_iter()
            .map(|(l, (_, e))| (l, e))
            .collect()
    }
    /// As [`Self::view`], with the agent whose read each entry is.
    fn view_with_agents(&self, agent: &Agent) -> BTreeMap<LogicalId, (Agent, LedgerEntry)> {
        let mut view: BTreeMap<LogicalId, (Agent, LedgerEntry)> = BTreeMap::new();
        if let Some(own) = self.agents.get(agent) {
            for e in own.required.values() {
                view.insert(e.logical, (agent.clone(), e.clone()));
            }
        }
        if let Some(root) = self.agents.get(&None) {
            for e in root.required.values() {
                view.entry(e.logical).or_insert_with(|| (None, e.clone()));
            }
        }
        let mut others: BTreeMap<LogicalId, (&Agent, &LedgerEntry)> = BTreeMap::new();
        for (a, st) in &self.agents {
            if a == agent || a.is_none() {
                continue;
            }
            for e in st.required.values() {
                let slot = others.entry(e.logical).or_insert((a, e));
                if e.epoch > slot.1.epoch {
                    *slot = (a, e);
                }
            }
        }
        for (l, (a, e)) in others {
            view.entry(l).or_insert_with(|| (a.clone(), e.clone()));
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
/// SPEC §6.1 contract 14, ADR 0015 §4.3 (T80): validation is admin-only
/// (`ctx validate`) or the in-process worker's; an agent batch carrying a
/// `RecordValidation` is refused, whatever the verdict.
fn agent_validation(id: Identity, ops: &[Op]) -> Option<Response> {
    (id.channel == Channel::Agent
        && ops
            .iter()
            .any(|op| matches!(op, Op::RecordValidation { .. })))
    .then(|| {
        err(
            "ChannelRestricted",
            "RecordValidation requires the admin socket (ctx validate)",
        )
    })
}
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
/// Reads in `agent`'s view that are no longer certified at their logical's
/// head, with the agent that read each (warn/deny reasons, ADR 0010 §6.1).
fn named_reads(
    s: &Session,
    agent: &Agent,
    db: &BranchView<'_>,
    live: &BTreeMap<String, Vec<u8>>,
) -> Vec<NamedRead> {
    s.view_with_agents(agent)
        .into_values()
        .filter_map(|(who, e)| {
            let current = db.head(e.logical);
            let summary = db
                .summary_at(current.unwrap_or(e.rev), live)
                .unwrap_or(Summary::Unavailable);
            if current == Some(e.rev) && summary == Summary::Certified {
                return None;
            }
            Some(NamedRead {
                key: db.logical_info(e.logical).map(|i| i.0).unwrap_or_default(),
                agent: who,
                read: e.rev,
                current,
                summary,
            })
        })
        .collect()
}
/// Entries of `agent`'s view whose head or status changed since they were
/// last reported to it (ADR 0010 §4.4). A required read was certified at
/// its revision when it was made.
fn update_items(
    s: &Session,
    agent: &Agent,
    db: &BranchView<'_>,
    live: &BTreeMap<String, Vec<u8>>,
) -> Vec<UpdateItem> {
    let reported = s.agents.get(agent).map(|a| &a.reported);
    let mut items: Vec<UpdateItem> = s
        .view(agent)
        .into_values()
        .filter_map(|e| {
            let current = db.head(e.logical);
            let summary = db
                .summary_at(current.unwrap_or(e.rev), live)
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
        level: e.level,
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
                            | SessionEvent::RootProof { .. }
                            | SessionEvent::CertIssued { .. }
                            | SessionEvent::SessionCert { .. }
                            | SessionEvent::LogBroken { .. }
                            | SessionEvent::End { .. }
                            | SessionEvent::HandoffBegin { .. }
                            | SessionEvent::HandoffAccept { .. }
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

include!("anchor_checks.rs");
include!("handoff.rs");
