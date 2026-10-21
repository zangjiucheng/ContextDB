//! Snapshot publication and replica reclaim (ADR 0008 §3.3), factored out of
//! the actor so loom can model the handshake. loom cannot instrument
//! arc-swap's internals, so under `cfg(loom)` the published pointer is a
//! loom-visible mutex around the `Arc` (review m6); arc-swap's own guarantees
//! (a swap settles every outstanding guard, so `Arc::get_mut` afterwards is
//! exact) are taken from its documentation and test suite.
#[cfg(loom)]
pub use loom::sync::Arc;
#[cfg(not(loom))]
pub use std::sync::Arc;

/// The pointer readers load: the latest published value.
pub struct Published<T> {
    #[cfg(not(loom))]
    inner: arc_swap::ArcSwap<T>,
    #[cfg(loom)]
    inner: loom::sync::Mutex<Arc<T>>,
}
impl<T> Published<T> {
    pub fn new(value: T) -> Self {
        Self {
            #[cfg(not(loom))]
            inner: arc_swap::ArcSwap::from_pointee(value),
            #[cfg(loom)]
            inner: loom::sync::Mutex::new(Arc::new(value)),
        }
    }
    /// The current value; a reader holds it for its whole request (I4).
    pub fn load(&self) -> Arc<T> {
        #[cfg(not(loom))]
        return self.inner.load_full();
        #[cfg(loom)]
        return self.inner.lock().expect("published").clone();
    }
    /// Publishes `value` and returns the previously published one.
    pub fn swap(&self, value: Arc<T>) -> Arc<T> {
        #[cfg(not(loom))]
        return self.inner.swap(value);
        #[cfg(loom)]
        return std::mem::replace(&mut *self.inner.lock().expect("published"), value);
    }
}
/// Waits for a value taken out of publication to become unique, retrying
/// until `give_up` says the bounded wait is over. Unique means no reader can
/// still observe it, so the caller may mutate it.
pub fn reclaim<T>(old: &mut Arc<T>, mut give_up: impl FnMut() -> bool) -> Option<&mut T> {
    while Arc::strong_count(old) > 1 {
        if give_up() {
            return None;
        }
        #[cfg(loom)]
        loom::thread::yield_now();
        #[cfg(not(loom))]
        std::thread::yield_now();
    }
    Arc::get_mut(old)
}
/// `RUSTFLAGS="--cfg loom" cargo test -p contextdb-daemon --lib --release`:
/// the actor's two-replica cycle against concurrent readers. A replica is
/// reclaimed only when unique, rewritten to the next epoch (the replay) and
/// published again; readers must only ever see whole, monotone snapshots.
#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;

    #[derive(Debug, Default)]
    struct Replica {
        epoch: u64,
        /// Always `epoch * 2` for a whole snapshot.
        check: u64,
    }
    fn whole(epoch: u64) -> Replica {
        Replica {
            epoch,
            check: epoch * 2,
        }
    }
    fn reader(p: &Published<Replica>, loads: usize) {
        let mut last = 0;
        for _ in 0..loads {
            let s = p.load();
            assert_eq!(s.check, s.epoch * 2, "torn snapshot");
            assert!(s.epoch >= last, "epoch went backwards");
            last = s.epoch;
        }
    }
    /// The actor loop: publish epochs 1..=groups, reusing the replica taken
    /// out of publication two groups ago when it is unique, else a fresh one
    /// (the clone fallback).
    fn actor(p: &Published<Replica>, groups: u64, tries: usize) -> (u64, u64) {
        let (mut reused, mut cloned) = (0, 0);
        let mut spare: Option<Arc<Replica>> = None;
        for epoch in 1..=groups {
            let mut n = 0;
            let next = match spare.as_mut().and_then(|s| {
                reclaim(s, || {
                    n += 1;
                    n > tries
                })
            }) {
                Some(r) => {
                    // Replay into the exclusively owned replica.
                    r.epoch = epoch;
                    r.check = epoch * 2;
                    reused += 1;
                    std::mem::take(r)
                }
                None => {
                    cloned += 1;
                    whole(epoch)
                }
            };
            spare = Some(p.swap(Arc::new(next)));
        }
        (reused, cloned)
    }
    #[test]
    fn publication_and_reclaim_against_one_reader() {
        loom::model(|| {
            let p = Arc::new(Published::new(whole(0)));
            let r = {
                let p = p.clone();
                loom::thread::spawn(move || reader(&p, 2))
            };
            let (reused, cloned) = actor(&p, 3, 2);
            r.join().unwrap();
            assert_eq!(reused + cloned, 3);
            assert_eq!(p.load().epoch, 3);
        });
    }
    #[test]
    fn publication_and_reclaim_against_two_readers() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(2);
        builder.check(|| {
            let p = Arc::new(Published::new(whole(0)));
            let readers: Vec<_> = (0..2)
                .map(|_| {
                    let p = p.clone();
                    loom::thread::spawn(move || reader(&p, 1))
                })
                .collect();
            actor(&p, 2, 1);
            for r in readers {
                r.join().unwrap();
            }
            assert_eq!(p.load().epoch, 2);
        });
    }
    /// A reader that still holds the spare forces the fallback; the held
    /// snapshot stays whole and unchanged.
    #[test]
    fn a_held_snapshot_is_never_reclaimed() {
        loom::model(|| {
            let p = Arc::new(Published::new(whole(0)));
            let held = p.load();
            let (reused, cloned) = actor(&p, 2, 1);
            assert_eq!((held.epoch, held.check), (0, 0));
            assert_eq!((reused, cloned), (0, 2));
        });
    }
}
