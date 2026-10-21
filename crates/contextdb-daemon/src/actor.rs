//! Commit actor (ADR 0008 §3): the only writer. It owns the `Store`, commits
//! queued writes as groups (natural batching, one sync per group) and
//! publishes an epoch-pinned `Snapshot` per group through `arc-swap`.
//!
//! Two replicas: after a group commits on the store's database, the replica
//! published two groups ago is reclaimed, replays the group's batches (results
//! must match) and is swapped into the store; the store's database becomes the
//! new published snapshot. A replica still held by a reader after a bounded
//! wait is replaced by a clone (counted; the fallback trigger of §3.3).
use arc_swap::ArcSwap;
use contextdb_core::Db;
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

/// What readers see: one epoch, its database and a log reader bounded by it.
pub struct Snapshot<D: Durability> {
    pub epoch: Epoch,
    pub db: Db,
    pub log: LogReader<D>,
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
    pub reclaim_wait_micros: AtomicU64,
}
#[derive(Clone)]
pub struct ActorHandle<D: Durability> {
    tx: mpsc::SyncSender<Message>,
    published: Arc<ArcSwap<Snapshot<D>>>,
    pub stats: Arc<ActorStats>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// The queue is full (`Busy`, ADR 0008 §3.1).
    Busy,
    /// The actor has stopped.
    Stopped,
}
impl<D: Durability> ActorHandle<D> {
    /// The current snapshot; readers hold it for the whole request (I4).
    pub fn snapshot(&self) -> Arc<Snapshot<D>> {
        self.published.load_full()
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
        rx.recv().map_err(|_| SubmitError::Stopped)
    }
    pub fn request<T>(
        &self,
        make: impl FnOnce(mpsc::Sender<T>) -> Message,
    ) -> Result<T, SubmitError> {
        let (reply, rx) = mpsc::channel();
        self.submit(make(reply))?;
        rx.recv().map_err(|_| SubmitError::Stopped)
    }
}
struct Actor<D: Durability> {
    store: Store<D>,
    published: Arc<ArcSwap<Snapshot<D>>>,
    spare: Option<Arc<Snapshot<D>>>,
    stats: Arc<ActorStats>,
}
/// Starts the actor thread over an opened store.
pub fn spawn<D: Durability + Clone + 'static>(
    store: Store<D>,
) -> Result<ActorHandle<D>, StoreError> {
    let db = store.db()?.clone();
    let snapshot = Snapshot {
        epoch: store.epoch(),
        db,
        log: store.log_reader(),
    };
    let published = Arc::new(ArcSwap::from_pointee(snapshot));
    let stats = Arc::new(ActorStats::default());
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let mut actor = Actor {
        store,
        published: published.clone(),
        spare: None,
        stats: stats.clone(),
    };
    std::thread::Builder::new()
        .name("ctxd-actor".into())
        .spawn(move || actor.run(rx))
        .map_err(|e| StoreError::Io(e.to_string()))?;
    Ok(ActorHandle {
        tx,
        published,
        stats,
    })
}
impl<D: Durability + Clone> Actor<D> {
    fn run(&mut self, rx: mpsc::Receiver<Message>) {
        while let Ok(first) = rx.recv() {
            let mut writes = Vec::new();
            let mut others = Vec::new();
            let take =
                |m: Message, writes: &mut Vec<WriteRequest>, others: &mut Vec<Message>| match m {
                    Message::Write(w) => writes.push(w),
                    other => others.push(other),
                };
            take(first, &mut writes, &mut others);
            // Natural batching: whatever is already queued, no waiting.
            while writes.len() < MAX_GROUP {
                match rx.try_recv() {
                    Ok(m) => take(m, &mut writes, &mut others),
                    Err(_) => break,
                }
            }
            if !writes.is_empty() {
                self.commit(writes);
            }
            for m in others {
                if self.handle(m) {
                    return;
                }
            }
        }
    }
    /// Returns true on shutdown.
    fn handle(&mut self, m: Message) -> bool {
        match m {
            Message::Write(_) => unreachable!("writes are grouped"),
            Message::ImpactExact { batch, reply } => {
                let _ = reply.send(self.store.impact_exact(&batch));
            }
            Message::TxnStatus { id, reply } => {
                let _ = reply.send(self.store.txn_status(&id));
            }
            Message::Checksum { epoch, reply } => {
                let _ = reply.send(self.store.checksum_at(epoch));
            }
            Message::Shutdown { reply } => {
                let _ = reply.send(());
                return true;
            }
        }
        false
    }
    fn commit(&mut self, writes: Vec<WriteRequest>) {
        // Payloads first: a blob failure poisons the store, which fails the group.
        let mut blob_error = None;
        for w in &writes {
            for b in &w.blobs {
                if let Err(e) = self.store.put_blob(b) {
                    blob_error = Some(e);
                }
            }
        }
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
        let replica = self.reclaim().and_then(|mut db| {
            for (_, b, r) in &fresh {
                let replayed = db.commit_with(b, &AllPresent).ok()?;
                if replayed != **r {
                    return None;
                }
            }
            Some(db)
        });
        let replica = match replica {
            Some(db) => db,
            None => {
                self.stats.clone_fallbacks.fetch_add(1, Ordering::Relaxed);
                match self.store.db() {
                    Ok(db) => db.clone(),
                    Err(_) => return, // poisoned: nothing more is published
                }
            }
        };
        let Ok(newest) = self.store.replace_db(replica) else {
            return;
        };
        let snapshot = Arc::new(Snapshot {
            epoch: newest.epoch(),
            db: newest,
            log: self.store.log_reader(),
        });
        self.spare = Some(self.published.swap(snapshot));
    }
    /// The replica published two groups ago, once no reader holds it.
    fn reclaim(&mut self) -> Option<Db> {
        let mut spare = self.spare.take()?;
        let start = Instant::now();
        loop {
            // arc-swap settles outstanding guards on swap, so a unique Arc
            // means no reader can still observe this snapshot.
            if let Some(s) = Arc::get_mut(&mut spare) {
                let waited = start.elapsed().as_micros() as u64;
                self.stats
                    .reclaim_wait_micros
                    .fetch_add(waited, Ordering::Relaxed);
                return Some(std::mem::take(&mut s.db));
            }
            if start.elapsed() >= RECLAIM_WAIT {
                return None;
            }
            std::thread::yield_now();
        }
    }
}
