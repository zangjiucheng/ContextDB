//! Commit actor (ADR 0008 §3): the only writer. It owns the `Store`, commits
//! queued writes as groups (natural batching, one sync per group) and
//! publishes an epoch-pinned `Snapshot` per group through `arc-swap`.
//!
//! Two replicas: after a group commits on the store's database, the replica
//! published two groups ago is reclaimed, replays the group's batches (results
//! must match) and is swapped into the store; the store's database becomes the
//! new published snapshot. A replica still held by a reader after a bounded
//! wait is replaced by a clone (counted; the fallback trigger of §3.3). While
//! the actor is idle, the spare replays the previous publication's steps, so
//! a publication replays only its own.
//!
//! Lazy maintenance (ADR 0019 §3.5, §3.7): the store records every commit
//! and drain as a step, in execution order, and the replica replays exactly
//! those. A commit that leaves a frontier ends its group; the rest of the
//! group goes back to the head of the queue. While a frontier exists and no
//! message waits, the actor drains one slice and publishes; `Settle` drains
//! on a reader's request.
use crate::publish::{self, Published};
use crate::team;
use contextdb_core::{BranchView, Budget, Db, Drain, Kernel, LAZY_BUDGET};
use contextdb_model::*;
use contextdb_store::gc::{GcPlan, GcReport, LedgerEntry};
use contextdb_store::{Durability, LogReader, Step, Store, StoreError, TxnStatus, Write};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub const QUEUE: usize = 1024;
pub const MAX_GROUP: usize = 64;
const RECLAIM_WAIT: Duration = Duration::from_millis(5);
/// A request runs for at most this long (ADR 0008 §5).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a certified read waits for a pull by default
/// (`read_settle_ms`, ADR 0019 O2).
pub const READ_SETTLE: Duration = Duration::from_millis(50);

/// K5 of payload GC (ADR 0019 §5.1): the epochs of every published
/// snapshot still alive, as a multiset (the actor's own published and spare
/// ones included). Its lock is a leaf: guards drop on reader threads, so
/// nothing else is taken while it is held. No `Weak` is kept, so
/// `publish::reclaim`'s `Arc::get_mut` still works.
#[derive(Debug, Default)]
pub struct HeldEpochs(Mutex<std::collections::BTreeMap<Epoch, usize>>);
impl HeldEpochs {
    fn hold(self: &Arc<Self>, epoch: Epoch) -> Hold {
        *self.lock().entry(epoch).or_default() += 1;
        Hold {
            set: self.clone(),
            epoch,
        }
    }
    /// The lowest epoch a live snapshot holds.
    pub fn held(&self) -> Option<Epoch> {
        self.lock().keys().next().copied()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<Epoch, usize>> {
        // A count is never left half-updated, so a poisoned lock is usable.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
/// A snapshot's registration in [`HeldEpochs`], from its creation until it
/// drops.
#[derive(Debug)]
pub struct Hold {
    set: Arc<HeldEpochs>,
    epoch: Epoch,
}
impl Drop for Hold {
    fn drop(&mut self) {
        let mut held = self.set.lock();
        if let Some(n) = held.get_mut(&self.epoch) {
            *n -= 1;
            if *n == 0 {
                held.remove(&self.epoch);
            }
        }
    }
}
/// What readers see: one epoch, its database and a log reader bounded by it.
pub struct Snapshot<D: Durability> {
    pub epoch: Epoch,
    /// Every branch (ADR 0013); readers use `main` until protocol 4.
    pub kernel: Kernel,
    pub log: LogReader<D>,
    /// The highest shred epoch whose key-log purge completed (ADR 0017
    /// §8.4), when this snapshot was published.
    pub key_purged: Epoch,
    /// Bumped on every publication, settling ones included (ADR 0019
    /// §3.7): a pull waits for a newer one.
    pub generation: u64,
    /// Registers `epoch` with K5 while the snapshot lives.
    _hold: Hold,
}
impl<D: Durability> Snapshot<D> {
    /// `main`'s database. Protocol-3 readers use ids as global ids, which
    /// holds while `main` is not renumbered (no other branch allocated ids).
    pub fn db(&self) -> &Db {
        debug_assert!(
            self.kernel.main_is_identity(),
            "a protocol-4 reader is required"
        );
        self.kernel.branch(MAIN).expect("main").db()
    }
    /// Branch `b` read with global ids (protocol 4); `main` when `b` is
    /// unknown.
    pub fn view(&self, b: BranchId) -> BranchView<'_> {
        self.kernel
            .branch(b)
            .unwrap_or_else(|| self.kernel.branch(MAIN).expect("main"))
    }
}
pub struct WriteRequest {
    /// Payload bodies stored (durably) before the batch is applied.
    pub blobs: Vec<Vec<u8>>,
    pub batch: Batch,
    /// Idempotency hash when the actor extends the batch (`expect`).
    pub client_hash: Option<ContentHash>,
    /// `expect_cert_valid` (ADR 0013 §4).
    pub expect: Option<Box<Expect>>,
    pub reply: mpsc::Sender<Result<Acked, StoreError>>,
}
/// A write's acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acked {
    pub result: CommitResult,
    /// The commit's maintenance was pending at ack (ADR 0019 §3.9), so the
    /// result's diffs are empty; for an idempotent retry, as its frame
    /// recorded.
    pub settling: bool,
}
/// A certificate the batch's commit depends on: validated by the actor on
/// the live state just before staging; a valid one's vector becomes an
/// `ExpectHeads{source: Certificate}` the kernel checks in the same step.
pub struct Expect {
    pub cert: Certificate,
    pub live: std::collections::BTreeMap<String, Vec<u8>>,
}
pub enum Message {
    Write(WriteRequest),
    ImpactExact {
        batch: Batch,
        reply: mpsc::Sender<Result<CommitResult, StoreError>>,
    },
    /// `WHAT_IF` on the batch's branch, with a merge's report.
    WhatIf {
        batch: Batch,
        reply: mpsc::Sender<Result<(CommitResult, Option<MergeReport>), StoreError>>,
    },
    TxnStatus {
        id: [u8; 16],
        reply: mpsc::Sender<Result<TxnStatus, StoreError>>,
    },
    Checksum {
        epoch: Epoch,
        reply: mpsc::Sender<Result<ContentHash, StoreError>>,
    },
    /// The store's `key_log_purged_through`; with `retry`, an unfinished
    /// key-log rewrite runs again first (ADR 0017 §8.4).
    KeyPurge {
        retry: bool,
        reply: mpsc::Sender<Result<Epoch, StoreError>>,
    },
    /// One `ctx_changes` page from the durable `diff_log` (ADR 0015 §8).
    Changes {
        branch: BranchId,
        since: Epoch,
        max: usize,
        reply: mpsc::Sender<Result<contextdb_store::ChangePage, StoreError>>,
    },
    /// P10: import every joined team (or one), then catch every open
    /// branch up (ADR 0018 §8.1). One line per team.
    TeamImport {
        team: Option<TeamId>,
        reply: mpsc::Sender<Vec<String>>,
    },
    /// P10: a write the kernel refused as a team write (ADR 0018 §6.1):
    /// `Ok(Some((team, proposal id)))` when it is now queued, `Ok(None)` when
    /// it is not a team write (the refusal stands).
    TeamRoute {
        batch: Batch,
        blobs: Vec<Vec<u8>>,
        refused: TxError,
        reply: mpsc::Sender<team::Routed>,
    },
    /// P10: a team joined while the daemon runs, opened from its directory.
    TeamOpen {
        dir: std::path::PathBuf,
        reply: mpsc::Sender<Result<TeamId, String>>,
    },
    /// P10: `ctx team leave` (ADR 0018 §8.5).
    TeamLeave {
        team: TeamId,
        reply: mpsc::Sender<Result<usize, String>>,
    },
    /// P10: `ctx team status` and the publish steps (ADR 0018 §7.2).
    TeamCall {
        team: Option<TeamId>,
        call: team::Call,
        reply: mpsc::Sender<team::Answer>,
    },
    /// A reader's pull (ADR 0019 §3.7): drain the pending commit of `epoch`
    /// until every node of rank ≤ `through` is settled (`None`: fully),
    /// then publish. Nothing when `epoch` is no longer pending. No reply:
    /// the reader waits for the publication generation.
    Settle {
        epoch: Epoch,
        through: Option<usize>,
    },
    /// Payload GC phase 2 (ADR 0019 §5.2): one exclusive step, never in a
    /// commit group, with the K5 bound taken now.
    Gc {
        plan: Box<GcPlan>,
        pinned: std::collections::BTreeSet<ContentHash>,
        reply: mpsc::Sender<Result<GcReport, StoreError>>,
    },
    /// The GC ledger's entries, for the projection purge.
    GcLedger {
        reply: mpsc::Sender<Vec<LedgerEntry>>,
    },
    Shutdown {
        reply: mpsc::Sender<()>,
    },
}
/// Counters reported by `Status` and the G5 bench.
#[derive(Debug, Default)]
pub struct ActorStats {
    pub groups: AtomicU64,
    pub batches: AtomicU64,
    pub clone_fallbacks: AtomicU64,
    /// Replica replays that disagreed with the store (each poisons it).
    pub replay_mismatches: AtomicU64,
    pub reclaim_wait_micros: AtomicU64,
    /// Writes a settling commit ended the group of, submitted again (ADR
    /// 0019 §3.5 item 3).
    pub requeued: AtomicU64,
    /// Times the idle actor brought the spare up to date.
    pub idle_catchups: AtomicU64,
    /// `Drain` steps replayed by publications (idle catch-ups excluded).
    pub replayed_drains: AtomicU64,
    /// Whether the replica is ready: caught up to the published snapshot,
    /// so the next publication replays only its own steps.
    pub spare_ready: std::sync::atomic::AtomicBool,
}
#[derive(Clone)]
pub struct ActorHandle<D: Durability> {
    tx: mpsc::SyncSender<Message>,
    published: Arc<Published<Snapshot<D>>>,
    /// The last published epoch and publication generation, signalled on
    /// every publication (the `ctx_changes` long-poll, ADR 0015 §8; pulls,
    /// ADR 0019 §3.7).
    epochs: Arc<(Mutex<(Epoch, u64)>, Condvar)>,
    pub stats: Arc<ActorStats>,
    /// How long a caller waits for its reply ([`REQUEST_TIMEOUT`]).
    pub timeout: Duration,
    /// How long a certified read waits for a pull ([`READ_SETTLE`];
    /// `[maintenance] read_settle_ms` in `ctxd.toml`).
    pub read_settle: Duration,
    /// P10: per team, what reads decide `ReplicationLag` from.
    pub team_live: team::LiveMap,
    /// K5: the epochs live snapshots hold.
    pub held: Arc<HeldEpochs>,
    /// The store's commit budget (`None`: eager).
    pub lazy: Option<Budget>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// The queue is full (`Busy`, ADR 0008 §3.1).
    Busy,
    /// The actor has stopped.
    Stopped,
    /// No reply within the request timeout. A write may still commit: its
    /// outcome is resolved through `TxnStatus` with its client_txn_id.
    Timeout,
}
impl<D: Durability> ActorHandle<D> {
    /// The current snapshot; readers hold it for the whole request (I4).
    pub fn snapshot(&self) -> Arc<Snapshot<D>> {
        self.published.load()
    }
    /// K5 (ADR 0019 §5.1): the lowest epoch a live snapshot holds.
    pub fn held(&self) -> Epoch {
        (self.held.held()).unwrap_or_else(|| self.published.load().epoch)
    }
    /// Waits until an epoch after `after` is published, or `timeout`
    /// passes; returns the last published epoch.
    pub fn wait_epoch(&self, after: Epoch, timeout: Duration) -> Epoch {
        self.wait_published(|(e, _)| e > after, timeout).0
    }
    /// Waits until a publication generation after `after`, or `timeout`
    /// passes; returns the last published generation.
    pub fn wait_generation(&self, after: u64, timeout: Duration) -> u64 {
        self.wait_published(|(_, g)| g > after, timeout).1
    }
    fn wait_published(
        &self,
        done: impl Fn((Epoch, u64)) -> bool,
        timeout: Duration,
    ) -> (Epoch, u64) {
        let (m, c) = &*self.epochs;
        let deadline = Instant::now() + timeout;
        let mut p = m.lock().expect("epochs");
        while !done(*p) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            p = c.wait_timeout(p, left).expect("epochs").0;
        }
        *p
    }
    /// The pull (ADR 0019 §3.7): asks the actor to settle the pending
    /// commit of `epoch` through rank `through` (`None`: fully), then waits
    /// up to `wait` for a publication newer than `generation`. True when one
    /// came. A full queue (`Busy`) or a stopped actor answers false at once.
    ///
    /// The caller drops its snapshot before calling (a held snapshot makes
    /// the actor clone its replica) and afterwards restarts its whole
    /// request on a fresh snapshot (I4).
    pub fn pull(
        &self,
        epoch: Epoch,
        through: Option<usize>,
        generation: u64,
        wait: Duration,
    ) -> bool {
        if self.submit(Message::Settle { epoch, through }).is_err() {
            return false;
        }
        self.wait_generation(generation, wait) > generation
    }
    pub fn submit(&self, m: Message) -> Result<(), SubmitError> {
        self.tx.try_send(m).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => SubmitError::Busy,
            mpsc::TrySendError::Disconnected(_) => SubmitError::Stopped,
        })
    }
    /// Submits a write and waits for its (post-sync) result.
    pub fn write(
        &self,
        blobs: Vec<Vec<u8>>,
        batch: Batch,
    ) -> Result<Result<Acked, StoreError>, SubmitError> {
        let (reply, rx) = mpsc::channel();
        self.submit(Message::Write(WriteRequest {
            blobs,
            batch,
            client_hash: None,
            expect: None,
            reply,
        }))?;
        self.wait(rx)
    }
    /// A write whose commit is conditional on a certificate (`ctx_commit`
    /// with `expect_cert_valid`).
    pub fn write_expecting(
        &self,
        blobs: Vec<Vec<u8>>,
        batch: Batch,
        client_hash: ContentHash,
        expect: Option<Expect>,
    ) -> Result<Result<Acked, StoreError>, SubmitError> {
        let (reply, rx) = mpsc::channel();
        self.submit(Message::Write(WriteRequest {
            blobs,
            batch,
            client_hash: Some(client_hash),
            expect: expect.map(Box::new),
            reply,
        }))?;
        self.wait(rx)
    }
    fn wait<T>(&self, rx: mpsc::Receiver<T>) -> Result<T, SubmitError> {
        rx.recv_timeout(self.timeout).map_err(|e| match e {
            mpsc::RecvTimeoutError::Timeout => SubmitError::Timeout,
            mpsc::RecvTimeoutError::Disconnected => SubmitError::Stopped,
        })
    }
    pub fn request<T>(
        &self,
        make: impl FnOnce(mpsc::Sender<T>) -> Message,
    ) -> Result<T, SubmitError> {
        let (reply, rx) = mpsc::channel();
        self.submit(make(reply))?;
        self.wait(rx)
    }
}
/// The kernel the next publication swaps into the store, replayed to the
/// store's state first.
enum Replica<D: Durability> {
    /// The snapshot published before the current one: at the state before
    /// `behind`, the steps of the current publication.
    Spare {
        snapshot: Arc<Snapshot<D>>,
        behind: Vec<Step>,
    },
    /// Caught up while idle: at the published snapshot's state.
    Ready(Box<Kernel>),
}
struct Actor<D: Durability> {
    store: Store<D>,
    published: Arc<Published<Snapshot<D>>>,
    /// None before the first publication and after a replay mismatch.
    replica: Option<Replica<D>>,
    stats: Arc<ActorStats>,
    epochs: Arc<(Mutex<(Epoch, u64)>, Condvar)>,
    /// P10: joined teams; only the actor reads or writes them.
    teams: std::collections::BTreeMap<TeamId, team::Joined>,
    team_live: team::LiveMap,
    /// Team-shredded contents a local holder keeps (§11): checked once per
    /// process, since what holds them (records, artifacts, validation
    /// metadata, notes) is never removed.
    shred_kept: std::collections::BTreeSet<ContentHash>,
    held: Arc<HeldEpochs>,
}
/// A catch-up writer over the store's single writer: each batch carries
/// the mirror payloads, read from the replica.
struct ActorWriter<'a, D: Durability + Clone> {
    actor: &'a mut Actor<D>,
    replica: &'a contextdb_team::replica::Replica,
}
impl<D: Durability + Clone> contextdb_team::catchup::Writer for ActorWriter<'_, D> {
    type Error = StoreError;
    fn kernel(&self) -> &Kernel {
        // Checked readable before the catch-up starts; a failed commit ends
        // it before the next read.
        self.actor.store.kernel().expect("a readable store")
    }
    fn commit(&mut self, b: &Batch) -> Result<CommitResult, StoreError> {
        // Bytes shredded here are never stored again: the kernel creates
        // such a mirror revision unavailable instead (§5.1, §11).
        let blobs: Vec<Vec<u8>> = {
            let k = self.actor.store.kernel().expect("a readable store");
            let presence = self.actor.store.presence();
            let collected = |h: &ContentHash| presence.collected(h);
            team::contents(k, b, &collected)
                .filter(|h| !k.is_shredded(h))
                .filter_map(|h| self.replica.blob(&h).map(<[u8]>::to_vec))
                .collect()
        };
        let (reply, rx) = mpsc::channel();
        let rest = self.actor.commit(vec![WriteRequest {
            blobs,
            batch: b.clone(),
            client_hash: None,
            expect: None,
            reply,
        }]);
        // A group's first write always gets a result.
        debug_assert!(rest.is_empty());
        drop(rest);
        (rx.recv().unwrap_or(Err(StoreError::Poisoned))).map(|a| a.result)
    }
}
impl<D: Durability + Clone> Actor<D> {
    /// The rest of a leave whose `leaving` is recorded: every open branch
    /// retracted with the watermark `left`, then the team's directory
    /// deleted. Rerun until it completes (a crash in between).
    fn finish_leave(&mut self, team: TeamId) -> Result<usize, String> {
        let Some(j) = self.teams.remove(&team) else {
            return Err("not joined".into());
        };
        if let Err(e) = self.store.kernel() {
            self.teams.insert(team, j);
            return Err(format!("{e:?}"));
        }
        let mut w = ActorWriter {
            actor: self,
            replica: j.store.replica(),
        };
        let written = match contextdb_team::catchup::leave_all(&mut w, team) {
            Ok(n) => n,
            Err((b, e)) => {
                let msg = format!("branch {}: {e:?}", b.0);
                self.teams.insert(team, j);
                return Err(msg);
            }
        };
        let dir = j.store.dir().to_owned();
        drop(j);
        self.team_live.lock().expect("team live").remove(&team);
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
        Ok(written)
    }
}
/// Starts the actor thread over an opened store.
pub fn spawn<D: Durability + Clone + 'static>(
    store: Store<D>,
) -> Result<ActorHandle<D>, StoreError> {
    spawn_with_teams(store, Default::default())
}
/// [`spawn`] with the joined teams (P10).
pub fn spawn_with_teams<D: Durability + Clone + 'static>(
    mut store: Store<D>,
    teams: std::collections::BTreeMap<TeamId, team::Joined>,
) -> Result<ActorHandle<D>, StoreError> {
    let kernel = store.kernel()?.clone();
    let lazy = store.lazy();
    store.record_steps();
    let held = Arc::new(HeldEpochs::default());
    let snapshot = Snapshot {
        epoch: store.epoch(),
        kernel,
        log: store.log_reader(),
        key_purged: store.key_log_purged_through(),
        generation: 0,
        _hold: held.hold(store.epoch()),
    };
    let epochs = Arc::new((Mutex::new((snapshot.epoch, 0)), Condvar::new()));
    let published = Arc::new(Published::new(snapshot));
    let stats = Arc::new(ActorStats::default());
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    // Every joined team is known from the start: routing sees it, and its
    // reads lag (no position) until its first import.
    let team_live = team::LiveMap::default();
    {
        let mut l = team_live.lock().expect("team live");
        for t in teams.keys() {
            l.entry(*t).or_default();
        }
    }
    let mut actor = Actor {
        store,
        published: published.clone(),
        replica: None,
        stats: stats.clone(),
        epochs: epochs.clone(),
        teams,
        team_live: team_live.clone(),
        shred_kept: Default::default(),
        held: held.clone(),
    };
    std::thread::Builder::new()
        .name("ctxd-actor".into())
        .spawn(move || {
            let reply = actor.run(rx);
            // Release the store (and its lock) before confirming the shutdown.
            drop(actor);
            if let Some(reply) = reply {
                let _ = reply.send(());
            }
        })
        .map_err(|e| StoreError::Io(e.to_string()))?;
    Ok(ActorHandle {
        tx,
        published,
        epochs,
        stats,
        timeout: REQUEST_TIMEOUT,
        read_settle: READ_SETTLE,
        team_live,
        held,
        lazy,
    })
}
impl<D: Durability + Clone> Actor<D> {
    /// Returns the shutdown reply, sent after the store is dropped.
    fn run(&mut self, rx: mpsc::Receiver<Message>) -> Option<mpsc::Sender<()>> {
        // Exact IMPACT clones the inputs (O(N)), so at most one runs per group
        // interval; the rest wait their turn (ADR 0008 §3.3, review finding 8).
        let mut impacts = std::collections::VecDeque::new();
        // Writes a settling commit ended the group of: they commit next,
        // ahead of newer messages (ADR 0019 §3.5 item 3).
        let mut requeued: std::collections::VecDeque<WriteRequest> = Default::default();
        loop {
            let busy = !impacts.is_empty() || !requeued.is_empty() || self.pending();
            let first = match rx.try_recv() {
                Ok(m) => Some(m),
                Err(_) if busy => None,
                Err(mpsc::TryRecvError::Empty) => {
                    // Idle: the spare catches up now, off the next ack's path.
                    self.catch_up();
                    match rx.recv() {
                        Ok(m) => Some(m),
                        Err(_) => return None,
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => return None,
            };
            let idle = first.is_none() && requeued.is_empty();
            let mut writes: Vec<WriteRequest> = requeued.drain(..).collect();
            let mut others = Vec::new();
            let take =
                |m: Message, writes: &mut Vec<WriteRequest>, others: &mut Vec<Message>| match m {
                    Message::Write(w) => writes.push(w),
                    other => others.push(other),
                };
            if let Some(first) = first {
                take(first, &mut writes, &mut others);
                // Natural batching: whatever is already queued, no waiting.
                while writes.len() < MAX_GROUP {
                    match rx.try_recv() {
                        Ok(m) => take(m, &mut writes, &mut others),
                        Err(_) => break,
                    }
                }
            }
            if !writes.is_empty() {
                requeued.extend(self.commit(writes));
            }
            for m in others {
                match m {
                    Message::Shutdown { reply } => return Some(reply),
                    m @ (Message::ImpactExact { .. } | Message::WhatIf { .. }) => {
                        impacts.push_back(m)
                    }
                    m => self.handle(m),
                }
            }
            if let Some(m) = impacts.pop_front() {
                self.handle(m);
            }
            // Idle drain (ADR 0019 §3.7): no message waits, so one slice.
            if idle && impacts.is_empty() && self.pending() {
                let units = self.store.lazy().map_or(LAZY_BUDGET, |b: Budget| b.units);
                let _ = self.store.drain(Drain::Units(units));
                self.publish_changes();
            }
        }
    }
    /// Whether the store's kernel has a frontier to drain (never once
    /// poisoned: nothing can drain it then).
    fn pending(&self) -> bool {
        (self.store.kernel()).is_ok_and(|k| k.frontier().is_some())
    }
    /// After any message or slice (ADR 0019 §3.5 item 6): publishes when the
    /// store's epoch advanced or it took a step, so a settle a dry run, a
    /// pull or an idempotent retry caused is published too.
    fn publish_changes(&mut self) {
        let steps = self.store.take_steps();
        if steps.is_empty() && self.store.epoch() <= self.published.load().epoch {
            return;
        }
        self.publish(steps);
    }
    fn handle(&mut self, m: Message) {
        self.answer(m);
        self.publish_changes();
    }
    fn answer(&mut self, m: Message) {
        match m {
            Message::Write(_) | Message::Shutdown { .. } => unreachable!("handled by run"),
            Message::Settle { epoch, through } => {
                if self.store.frontier_epoch() == Some(epoch) {
                    let _ = self.store.drain(through.map_or(Drain::Full, Drain::Rank));
                }
            }
            // A dry run settles first; that is published before the reply.
            Message::ImpactExact { batch, reply } => {
                let r = self.store.impact_exact(&batch);
                self.publish_changes();
                let _ = reply.send(r);
            }
            Message::WhatIf { batch, reply } => {
                let r = self.store.what_if_detailed(&batch);
                self.publish_changes();
                let _ = reply.send(r);
            }
            Message::TxnStatus { id, reply } => {
                let _ = reply.send(self.store.txn_status(&id));
            }
            Message::Gc {
                plan,
                pinned,
                reply,
            } => {
                let held = self.held.held().unwrap_or(self.published.load().epoch);
                let r = (self.store).gc_apply(
                    &plan,
                    &pinned,
                    held,
                    &contextdb_proto::docs::nested_hashes,
                );
                // Readers see `Collected` through the shared pack index
                // already; a publication tells waiters something changed.
                if r.as_ref().is_ok_and(|r| r.generation > 0) {
                    let steps = self.store.take_steps();
                    self.publish(steps);
                }
                let _ = reply.send(r);
            }
            Message::GcLedger { reply } => {
                let _ = reply.send(self.store.gc_ledger().to_vec());
            }
            Message::TeamLeave { team, reply } => {
                let r = match self.teams.get_mut(&team) {
                    None => Err("not joined".to_owned()),
                    Some(j) => team::mark_leaving(j.store.dir())
                        .map(|()| j.leaving = true)
                        .map_err(|e| e.to_string()),
                }
                .and_then(|()| self.finish_leave(team));
                let _ = reply.send(r);
            }
            Message::TeamImport { team, reply } => {
                let leaving: Vec<TeamId> = (self.teams.iter())
                    .filter(|(t, j)| j.leaving && team.is_none_or(|x| x == **t))
                    .map(|(t, _)| *t)
                    .collect();
                let mut lines = Vec::new();
                for t in leaving {
                    lines.push(match self.finish_leave(t) {
                        Ok(n) => format!(
                            "{} left ({n} branches written)",
                            contextdb_team::commit::hex(&t)
                        ),
                        Err(e) => format!(
                            "{} leave not finished: {e}",
                            contextdb_team::commit::hex(&t)
                        ),
                    });
                }
                let mut teams = std::mem::take(&mut self.teams);
                let live = self.team_live.clone();
                for (t, j) in teams.iter_mut() {
                    // A leave not finished is never caught up (Q12).
                    if team.is_some_and(|x| x != *t) || j.leaving {
                        continue;
                    }
                    let mut catch_up = |team: TeamId,
                                        replica: &contextdb_team::replica::Replica,
                                        h: &contextdb_team::catchup::History|
                     -> Result<(), (BranchId, StoreError)> {
                        // Notes live only in the op log: read (only when
                        // there is something to shred) before the writer
                        // borrows the actor.
                        let mut todo = match self.store.kernel() {
                            Ok(k) => team::shred_todo(k, replica),
                            Err(e) => return Err((MAIN, e)),
                        };
                        todo.retain(|h| !self.shred_kept.contains(h));
                        let notes = if todo.is_empty() {
                            Default::default()
                        } else {
                            let store = &self.store;
                            team::notes_in_log(store.epoch(), |e| store.read_row(e), &todo)
                                .map_err(|e| (MAIN, e))?
                        };
                        let mut w = ActorWriter {
                            actor: self,
                            replica,
                        };
                        // §11 (Q6): the local shred step precedes any mirror.
                        let kept =
                            team::shred_step(&mut w, &todo, &notes).map_err(|e| (MAIN, e))?;
                        let r = contextdb_team::catchup::catch_up_all(&mut w, team, h).map(|_| ());
                        if !kept.is_empty() {
                            eprintln!(
                                "ctxd: team {}: {} shredded content(s) kept: local data holds them",
                                contextdb_team::commit::hex(&team),
                                kept.len()
                            );
                            self.shred_kept.extend(kept);
                        }
                        r
                    };
                    lines.push(team::import(j, &live, &mut catch_up));
                }
                self.teams = teams;
                let _ = reply.send(lines);
            }
            Message::TeamOpen { dir, reply } => {
                let r = team::open_one(&dir).and_then(|j| {
                    let t = j.store.team();
                    if self.teams.contains_key(&t) {
                        return Err("already joined".into());
                    }
                    // A rejoin whose chain gives a mirror key this store
                    // holds another kind or lane (§8.7, Q14).
                    let k = self.store.kernel().map_err(|e| format!("{e:?}"))?;
                    let want = contextdb_team::catchup::projection(j.store.replica());
                    if let Some(TxError::Team { code, detail }) =
                        contextdb_team::catchup::key_mismatch_error(k, t, &want)
                    {
                        return Err(format!("{code}: {detail}"));
                    }
                    self.teams.insert(t, j);
                    self.team_live
                        .lock()
                        .expect("team live")
                        .entry(t)
                        .or_default();
                    Ok(t)
                });
                let _ = reply.send(r);
            }
            Message::TeamCall { team, call, reply } => {
                let a = match self.store.kernel() {
                    Ok(k) => team::call(&mut self.teams, k, team, call),
                    Err(e) => team::Answer::Error {
                        code: "OutcomeUnknown".into(),
                        detail: format!("{e:?}"),
                    },
                };
                for (t, j) in &self.teams {
                    if team.is_none_or(|x| x == *t) {
                        team::refresh_proposed(j, &self.team_live);
                    }
                }
                let _ = reply.send(a);
            }
            Message::TeamRoute {
                batch,
                blobs,
                refused,
                reply,
            } => {
                let committed = (batch.client_txn_id).is_some_and(|t| {
                    matches!(self.store.txn_status(&t), Ok(TxnStatus::Committed { .. }))
                });
                let r = match self.store.kernel() {
                    Ok(k) => team::propose(&mut self.teams, k, &batch, &blobs, refused, committed),
                    Err(_) => Err(refused),
                };
                if let Ok(Some((t, _))) = &r
                    && let Some(j) = self.teams.get(t)
                {
                    team::refresh_proposed(j, &self.team_live);
                }
                let _ = reply.send(r);
            }
            Message::KeyPurge { retry, reply } => {
                let r = if retry {
                    self.store.retry_key_purge()
                } else {
                    Ok(self.store.key_log_purged_through())
                };
                let _ = reply.send(r);
            }
            Message::Checksum { epoch, reply } => {
                let _ = reply.send(self.store.checksum_at(epoch));
            }
            Message::Changes {
                branch,
                since,
                max,
                reply,
            } => {
                let _ = reply.send(self.store.changes(branch, since, max));
            }
        }
    }
    /// Commits one group and replies to each write it processed. Returns
    /// the writes after a settling commit, which ended the group (ADR 0019
    /// §3.5 item 3): the caller submits them again, first.
    fn commit(&mut self, writes: Vec<WriteRequest>) -> Vec<WriteRequest> {
        // A client transaction id a team proposal holds is not committed
        // locally (SPEC §6.1): `TxnIdReuse`, which `team_route` resolves
        // to the proposal for a retry of the same request.
        let (writes, held): (Vec<_>, Vec<_>) = writes.into_iter().partition(|w| {
            (w.batch.client_txn_id).is_none_or(|t| team::proposal_by_txn(&self.teams, &t).is_none())
        });
        for w in held {
            let id = w.batch.client_txn_id.expect("held by its id");
            let _ = w
                .reply
                .send(Err(StoreError::Tx(TxError::TxnIdReuse { id })));
        }
        if writes.is_empty() {
            return Vec::new();
        }
        // Payloads first: a blob failure poisons the store, which fails the group.
        // Ordered, not flushed: the group's op_log sync makes them durable
        // (ADR 0008 §17).
        let blobs: Vec<&[u8]> = writes
            .iter()
            .flat_map(|w| w.blobs.iter().map(Vec::as_slice))
            .collect();
        let blob_error = self.store.stage_blobs(&blobs).err();
        let group: Vec<Write> = (writes.iter())
            .map(|w| Write {
                batch: w.batch.clone(),
                client_hash: w.client_hash,
            })
            .collect();
        let before = self.store.epoch();
        let results = match blob_error {
            Some(e) => writes.iter().map(|_| Err(e.clone())).collect(),
            None => {
                // Certificate-conditional batches gain their `ExpectHeads`
                // here; the store's step log records what committed.
                let expects: Vec<Option<&Expect>> =
                    writes.iter().map(|w| w.expect.as_deref()).collect();
                let mut prepare = |n: usize,
                                   kernel: &Kernel,
                                   checksum: &dyn Fn(Epoch) -> Option<ContentHash>,
                                   batch: &Batch|
                 -> Result<Batch, StoreError> {
                    match expects[n] {
                        Some(expect) => expect_certificate(kernel, checksum, batch, expect),
                        None => Ok(batch.clone()),
                    }
                };
                self.store.commit_group_with(&group, &mut prepare)
            }
        };
        self.stats.groups.fetch_add(1, Ordering::Relaxed);
        self.stats
            .batches
            .fetch_add(results.len() as u64, Ordering::Relaxed);
        self.publish_changes();
        // Settling at ack (ADR 0019 §3.9): a new epoch is while it is still
        // the frontier's; a retry of an earlier one is as its frame says.
        let frontier = self.store.frontier_epoch();
        let settling = |r: &CommitResult| match r.epoch > before {
            true => frontier == Some(r.epoch),
            false => (self.store.read_row(r.epoch)).is_ok_and(|row| row.settling),
        };
        let mut writes = writes;
        let rest = writes.split_off(results.len());
        (self.stats.requeued).fetch_add(rest.len() as u64, Ordering::Relaxed);
        for (w, r) in writes.into_iter().zip(results) {
            let r = r.map(|result| Acked {
                settling: settling(&result),
                result,
            });
            let _ = w.reply.send(r);
        }
        rest
    }
    /// Publishes the store's database and keeps a caught-up replica in the
    /// store: the replica replays the steps it is behind and `current`, the
    /// store's steps since the last publication.
    fn publish(&mut self, current: Vec<Step>) {
        let reclaimed = match self.replica.take() {
            Some(Replica::Ready(db)) => Some((*db, Vec::new())),
            Some(Replica::Spare {
                mut snapshot,
                behind,
            }) => (self.reclaim(&mut snapshot, RECLAIM_WAIT)).map(|db| (db, behind)),
            None => None,
        };
        let replica = match reclaimed {
            Some((mut db, behind)) => {
                let drains = (behind.iter().chain(&current))
                    .filter(|s| matches!(s, Step::Drain(_)))
                    .count();
                (self.stats.replayed_drains).fetch_add(drains as u64, Ordering::Relaxed);
                if !self.replay(&mut db, behind.iter().chain(&current)) {
                    return;
                }
                db
            }
            None => {
                self.stats.clone_fallbacks.fetch_add(1, Ordering::Relaxed);
                match self.store.kernel() {
                    Ok(kernel) => kernel.clone(),
                    Err(_) => return, // poisoned: nothing more is published
                }
            }
        };
        let Ok(newest) = self.store.replace_kernel(replica) else {
            return;
        };
        let snapshot = Arc::new(Snapshot {
            epoch: newest.epoch(),
            _hold: self.held.hold(newest.epoch()),
            kernel: newest,
            log: self.store.log_reader(),
            key_purged: self.store.key_log_purged_through(),
            generation: self.published.load().generation + 1,
        });
        let published = (snapshot.epoch, snapshot.generation);
        self.replica = Some(Replica::Spare {
            snapshot: self.published.swap(snapshot),
            behind: current,
        });
        self.stats.spare_ready.store(false, Ordering::Relaxed);
        let (m, c) = &*self.epochs;
        *m.lock().expect("epochs") = published;
        c.notify_all();
    }
    /// While idle: the spare, when no reader holds it, replays the steps it
    /// is behind and becomes ready. Its snapshot drops, so its epoch is no
    /// longer held (K5); the ready kernel is at the published epoch, which
    /// the published snapshot holds.
    fn catch_up(&mut self) {
        self.replica = match self.replica.take() {
            Some(Replica::Spare {
                mut snapshot,
                behind,
            }) => match self.reclaim(&mut snapshot, Duration::ZERO) {
                None => Some(Replica::Spare { snapshot, behind }),
                Some(mut db) => {
                    if !self.replay(&mut db, &behind) {
                        return;
                    }
                    self.stats.idle_catchups.fetch_add(1, Ordering::Relaxed);
                    self.stats.spare_ready.store(true, Ordering::Relaxed);
                    Some(Replica::Ready(Box::new(db)))
                }
            },
            other => other,
        };
    }
    /// Replays `steps` on `db`. A replica that replays differently is a
    /// kernel bug: fail stop, never publish either state (§3.3).
    fn replay<'a>(&mut self, db: &mut Kernel, steps: impl IntoIterator<Item = &'a Step>) -> bool {
        let lazy = self.store.lazy();
        if steps.into_iter().all(|step| replay(db, step, lazy)) {
            return true;
        }
        self.stats.replay_mismatches.fetch_add(1, Ordering::Relaxed);
        let _ = self.store.fail_stop();
        false
    }
    /// The spare's kernel once no reader holds it, waiting at most `wait`.
    fn reclaim(&self, spare: &mut Arc<Snapshot<D>>, wait: Duration) -> Option<Kernel> {
        let start = Instant::now();
        let s = publish::reclaim(spare, || start.elapsed() >= wait)?;
        let waited = start.elapsed().as_micros() as u64;
        self.stats
            .reclaim_wait_micros
            .fetch_add(waited, Ordering::Relaxed);
        Some(std::mem::take(&mut s.kernel))
    }
}
/// Replays one of the store's steps on a replica, with the store's budget;
/// false when a commit's result differs from the acknowledged one, or a
/// drain finds no frontier (the store records one only when it has one).
fn replay(db: &mut Kernel, step: &Step, lazy: Option<Budget>) -> bool {
    match step {
        Step::Commit { batch, result } => {
            (db.commit_detailed_lazy(batch, &AllPresent, lazy))
                .map(|(r, _)| r)
                .as_ref()
                == Ok(result)
        }
        Step::Drain(how) => {
            let pending = db.frontier().is_some();
            db.drain(*how);
            pending
        }
    }
}
/// Validates `expect`'s certificate on the live kernel (every earlier batch
/// of the group applied) and appends its vector as
/// `ExpectHeads{source: Certificate}`, so validation and the kernel's read
/// check run on one state (ADR 0013 §4, §3.5 #13).
fn expect_certificate(
    kernel: &Kernel,
    checksum: &dyn Fn(Epoch) -> Option<ContentHash>,
    batch: &Batch,
    expect: &Expect,
) -> Result<Batch, StoreError> {
    let cert = &expect.cert;
    let view = kernel
        .branch(batch.branch)
        .ok_or(StoreError::Tx(TxError::UnknownBranch))?;
    let link = contextdb_core::LogLink {
        matches: checksum(cert.issued_epoch) == Some(cert.log_checksum_at_issue),
        current_checksum: checksum(kernel.epoch()).unwrap_or([0; 32]),
    };
    let verdict = view
        .validate_certificate(cert, &expect.live, link)
        .unwrap_or(Validation::Unknown {
            lag: vec!["history".into()],
        });
    let vector = match verdict {
        Validation::Valid => cert.vector.clone(),
        Validation::ValidRecertified { new_cert, .. } => new_cert.vector,
        other => return Err(StoreError::CertificateNotValid(Box::new(other))),
    };
    let mut extended = batch.clone();
    extended.ops.push(Op::ExpectHeads {
        entries: (vector.into_iter())
            .map(|v| (LogicalRef::Existing(v.logical), v.rev))
            .collect(),
        source: ReadSource::Certificate(cert.cert_id),
    });
    Ok(extended)
}
