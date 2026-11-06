//! Health of the retrieval projections (ADR 0016 §2.2): what the indexer
//! has applied, whether it is rebuilding or failing, and a progress signal
//! that a recall's bounded wait wakes on. Status never comes from here: lag
//! only ever means a text search may miss records newer than the watermark.
use contextdb_model::Epoch;
use contextdb_proto::proto::{IndexReport, IndexState};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Longest bounded wait a recall may ask for.
pub const MAX_WAIT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Inner {
    watermark: Epoch,
    rebuilding: Option<(u64, u64)>,
    failed: Option<String>,
    reindex: bool,
}

#[derive(Default)]
pub struct IndexHealth {
    inner: Mutex<Inner>,
    progress: Condvar,
}

impl IndexHealth {
    pub fn new(watermark: Epoch) -> Self {
        Self {
            inner: Mutex::new(Inner {
                watermark,
                ..Inner::default()
            }),
            progress: Condvar::new(),
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("index health")
    }
    /// The index is complete up to `epoch`.
    pub fn applied(&self, epoch: Epoch) {
        let mut g = self.lock();
        g.watermark = epoch;
        g.rebuilding = None;
        g.failed = None;
        drop(g);
        self.progress.notify_all();
    }
    /// A rebuild started or advanced: `done` of `total` logicals.
    pub fn rebuilding(&self, done: u64, total: u64) {
        let mut g = self.lock();
        g.rebuilding = Some((done, total));
        g.failed = None;
        g.watermark = Epoch(0);
        drop(g);
        self.progress.notify_all();
    }
    /// The last update failed; the indexer retries.
    pub fn failed(&self, error: String) {
        self.lock().failed = Some(error);
        self.progress.notify_all();
    }
    /// `ctx reindex`: the indexer clears and rebuilds on its next pass.
    pub fn request_reindex(&self) {
        let mut g = self.lock();
        g.reindex = true;
        g.rebuilding = Some((0, 0));
        drop(g);
        self.progress.notify_all();
    }
    /// Whether a reindex is requested (the request stays).
    pub fn reindex_requested(&self) -> bool {
        self.lock().reindex
    }
    /// Whether a reindex was requested (and clears the request).
    pub fn take_reindex(&self) -> bool {
        std::mem::take(&mut self.lock().reindex)
    }
    /// The text index's state relative to a snapshot at `head`.
    pub fn state(&self, head: Epoch) -> IndexState {
        let g = self.lock();
        if let Some(error) = &g.failed {
            IndexState::Failed {
                error: error.clone(),
            }
        } else if let Some((done, total)) = g.rebuilding {
            IndexState::Rebuilding { done, total }
        } else if g.watermark < head {
            IndexState::Lagging {
                watermark: g.watermark,
                head_epoch: head,
            }
        } else {
            IndexState::Ready {
                watermark: g.watermark,
            }
        }
    }
    /// Both projections; vectors stay `Disabled` until an embedder exists
    /// (milestone 5).
    pub fn report(&self, head: Epoch) -> IndexReport {
        IndexReport {
            fts: self.state(head),
            vectors: IndexState::Disabled,
        }
    }
    /// Waits up to `timeout` (capped at [`MAX_WAIT`]) for the watermark to
    /// reach `head`; returns whether it did. Wakes on every progress signal.
    pub fn wait(&self, head: Epoch, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout.min(MAX_WAIT);
        let mut g = self.lock();
        loop {
            if g.watermark >= head && g.rebuilding.is_none() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            g = self
                .progress
                .wait_timeout(g, deadline - now)
                .expect("index health")
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn states_follow_the_indexer() {
        let h = IndexHealth::new(Epoch(3));
        assert_eq!(
            h.state(Epoch(3)),
            IndexState::Ready {
                watermark: Epoch(3)
            }
        );
        assert_eq!(
            h.state(Epoch(5)),
            IndexState::Lagging {
                watermark: Epoch(3),
                head_epoch: Epoch(5)
            }
        );
        h.rebuilding(2, 9);
        assert_eq!(
            h.state(Epoch(5)),
            IndexState::Rebuilding { done: 2, total: 9 }
        );
        h.failed("disk full".into());
        assert!(matches!(h.state(Epoch(5)), IndexState::Failed { .. }));
        h.applied(Epoch(5));
        assert_eq!(
            h.state(Epoch(5)),
            IndexState::Ready {
                watermark: Epoch(5)
            }
        );
        h.request_reindex();
        assert!(h.take_reindex() && !h.take_reindex());
    }

    /// T86: the bounded wait wakes on progress, well before its timeout.
    #[test]
    fn the_wait_wakes_on_progress_and_times_out_otherwise() {
        let h = Arc::new(IndexHealth::new(Epoch(1)));
        let t = Instant::now();
        assert!(!h.wait(Epoch(2), Duration::from_millis(30)));
        assert!(t.elapsed() >= Duration::from_millis(30));
        let h2 = h.clone();
        let bump = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            h2.applied(Epoch(2));
        });
        let t = Instant::now();
        assert!(h.wait(Epoch(2), Duration::from_secs(1)));
        assert!(t.elapsed() < Duration::from_millis(500));
        bump.join().unwrap();
    }
}
