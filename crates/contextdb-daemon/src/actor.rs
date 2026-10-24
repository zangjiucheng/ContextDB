//! Commit actor (ADR 0008 §3): the only writer. It owns the `Store`, commits
//! queued writes as groups (natural batching, one sync per group) and
//! publishes an epoch-pinned `Snapshot` per group through `arc-swap`.
//!
//! Two replicas: after a group commits on the store's database, the replica
//! published two groups ago is reclaimed, replays the group's batches (results
//! must match) and is swapped into the store; the store's database becomes the
//! new published snapshot. A replica still held by a reader after a bounded
//! wait is replaced by a clone (counted; the fallback trigger of §3.3).
use crate::publish::{self, Published};
use contextdb_core::{Db, Kernel};
use contextdb_model::*;
use contextdb_store::{Durability, LogReader, Store, StoreError, TxnStatus};
use std::{
    sync::{
        Arc,
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

/// What readers see: one epoch, its database and a log reader bounded by it.
pub struct Snapshot<D: Durability> {
    pub epoch: Epoch,
    /// Every branch (ADR 0013); readers use `main` until protocol 4.
    pub kernel: Kernel,
    pub log: LogReader<D>,
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
}
pub struct WriteRequest {
    /// Payload bodies stored (durably) before the batch is applied.
    pub blobs: Vec<Vec<u8>>,
    pub batch: Batch,
    pub reply: mpsc::Sender<Result<CommitResult, StoreError>>,
}
pub enum Message {
    Write(WriteRequest),
    ImpactExact {
        batch: Batch,
        reply: mpsc::Sender<Result<CommitResult, StoreError>>,
    },
    TxnStatus {
        id: [u8; 16],
        reply: mpsc::Sender<Result<TxnStatus, StoreError>>,
    },
    Checksum {
        epoch: Epoch,
        reply: mpsc::Sender<Result<ContentHash, StoreError>>,
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
}
#[derive(Clone)]
pub struct ActorHandle<D: Durability> {
    tx: mpsc::SyncSender<Message>,
    published: Arc<Published<Snapshot<D>>>,
    pub stats: Arc<ActorStats>,
    /// How long a caller waits for its reply ([`REQUEST_TIMEOUT`]).
    pub timeout: Duration,
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
    ) -> Result<Result<CommitResult, StoreError>, SubmitError> {
        let (reply, rx) = mpsc::channel();
        self.submit(Message::Write(WriteRequest {
            blobs,
            batch,
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
struct Actor<D: Durability> {
    store: Store<D>,
    published: Arc<Published<Snapshot<D>>>,
    spare: Option<Arc<Snapshot<D>>>,
    /// Newly applied batches of the previous group, with their results: the
    /// spare is two groups behind the store and must replay them too.
    previous: Vec<(Batch, CommitResult)>,
    stats: Arc<ActorStats>,
}
/// Starts the actor thread over an opened store.
pub fn spawn<D: Durability + Clone + 'static>(
    store: Store<D>,
) -> Result<ActorHandle<D>, StoreError> {
    let kernel = store.kernel()?.clone();
    let snapshot = Snapshot {
        epoch: store.epoch(),
        kernel,
        log: store.log_reader(),
    };
    let published = Arc::new(Published::new(snapshot));
    let stats = Arc::new(ActorStats::default());
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let mut actor = Actor {
        store,
        published: published.clone(),
        spare: None,
        previous: Vec::new(),
        stats: stats.clone(),
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
        stats,
        timeout: REQUEST_TIMEOUT,
    })
}
impl<D: Durability + Clone> Actor<D> {
    /// Returns the shutdown reply, sent after the store is dropped.
    fn run(&mut self, rx: mpsc::Receiver<Message>) -> Option<mpsc::Sender<()>> {
        // Exact IMPACT clones the inputs (O(N)), so at most one runs per group
        // interval; the rest wait their turn (ADR 0008 §3.3, review finding 8).
        let mut impacts = std::collections::VecDeque::new();
        loop {
            let first = if impacts.is_empty() {
                match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return None,
                }
            } else {
                rx.try_recv().ok()
            };
            let mut writes = Vec::new();
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
                self.commit(writes);
            }
            for m in others {
                match m {
                    Message::Shutdown { reply } => return Some(reply),
                    m @ Message::ImpactExact { .. } => impacts.push_back(m),
                    m => self.handle(m),
                }
            }
            if let Some(m) = impacts.pop_front() {
                self.handle(m);
            }
        }
    }
    fn handle(&mut self, m: Message) {
        match m {
            Message::Write(_) | Message::Shutdown { .. } => unreachable!("handled by run"),
            Message::ImpactExact { batch, reply } => {
                let _ = reply.send(self.store.impact_exact(&batch));
            }
            Message::TxnStatus { id, reply } => {
                let _ = reply.send(self.store.txn_status(&id));
            }
            Message::Checksum { epoch, reply } => {
                let _ = reply.send(self.store.checksum_at(epoch));
            }
        }
    }
    fn commit(&mut self, writes: Vec<WriteRequest>) {
        // Payloads first: a blob failure poisons the store, which fails the group.
        // Ordered, not flushed: the group's op_log sync makes them durable
        // (ADR 0008 §17).
        let blobs: Vec<&[u8]> = writes
            .iter()
            .flat_map(|w| w.blobs.iter().map(Vec::as_slice))
            .collect();
        let blob_error = self.store.stage_blobs(&blobs).err();
        let batches: Vec<Batch> = writes.iter().map(|w| w.batch.clone()).collect();
        let before = self.store.epoch();
        let results = match blob_error {
            Some(e) => batches.iter().map(|_| Err(e.clone())).collect(),
            None => self.store.commit_group(&batches),
        };
        self.stats.groups.fetch_add(1, Ordering::Relaxed);
        self.stats
            .batches
            .fetch_add(batches.len() as u64, Ordering::Relaxed);
        if self.store.epoch() > before {
            self.publish(&batches, &results, before);
        }
        for (w, r) in writes.into_iter().zip(results) {
            let _ = w.reply.send(r);
        }
    }
    /// Publishes the store's database and keeps a caught-up replica in the store.
    fn publish(
        &mut self,
        batches: &[Batch],
        results: &[Result<CommitResult, StoreError>],
        before: Epoch,
    ) {
        // Newly applied batches in epoch order (idempotent hits and staged
        // duplicates repeat an epoch or predate the group).
        let mut fresh: Vec<(Epoch, &Batch, &CommitResult)> = batches
            .iter()
            .zip(results)
            .filter_map(|(b, r)| r.as_ref().ok().map(|r| (r.epoch, b, r)))
            .filter(|(e, _, _)| *e > before)
            .collect();
        fresh.sort_by_key(|(e, _, _)| *e);
        fresh.dedup_by_key(|(e, _, _)| *e);
        let current: Vec<(Batch, CommitResult)> = fresh
            .iter()
            .map(|(_, b, r)| ((*b).clone(), (*r).clone()))
            .collect();
        // The spare was published one group ago, so it is at the state before
        // the previous group: replay the previous group, then this one.
        let previous = std::mem::take(&mut self.previous);
        let replica = match self.reclaim() {
            Some(mut db) => {
                let agrees = previous
                    .iter()
                    .chain(&current)
                    .all(|(b, r)| db.commit_with(b, &AllPresent).as_ref() == Ok(r));
                if !agrees {
                    // A replica that replays differently is a kernel bug:
                    // fail stop, never publish either state (§3.3).
                    self.stats.replay_mismatches.fetch_add(1, Ordering::Relaxed);
                    let _ = self.store.fail_stop();
                    return;
                }
                Some(db)
            }
            None => None,
        };
        self.previous = current;
        let replica = match replica {
            Some(db) => db,
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
            kernel: newest,
            log: self.store.log_reader(),
        });
        self.spare = Some(self.published.swap(snapshot));
    }
    /// The replica published two groups ago, once no reader holds it.
    fn reclaim(&mut self) -> Option<Kernel> {
        let mut spare = self.spare.take()?;
        let start = Instant::now();
        let s = publish::reclaim(&mut spare, || start.elapsed() >= RECLAIM_WAIT)?;
        let waited = start.elapsed().as_micros() as u64;
        self.stats
            .reclaim_wait_micros
            .fetch_add(waited, Ordering::Relaxed);
        Some(std::mem::take(&mut s.kernel))
    }
}
