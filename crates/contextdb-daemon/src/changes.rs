//! `ctx_changes` (ADR 0015 §8): a branch's per-epoch status diffs after
//! `since_epoch`, paged, from the store's durable `diff_log` (read on the
//! actor, which owns the projection). No in-memory ring.
//!
//! - **Branch.** A bound caller reads its session's branch unless it names
//!   one; an unbound admin call reads the named branch or `main`. Agents
//!   give a binding.
//! - **Pages** end on epoch boundaries; `reached` is the next
//!   `since_epoch`. A scope filter drops items, never epochs.
//! - **Long-poll.** With `wait_ms` (at most 30 s) and nothing after
//!   `since_epoch` yet, the request waits for each newly published epoch
//!   and re-reads, until an item arrives or the wait ends. At most
//!   [`MAX_WAITERS`] requests wait at once; more are refused with `Busy`.
//! - **Truncated{earliest}** when the epochs after `since_epoch` cannot be
//!   served (a cut history, or a projection that does not cover the tail):
//!   the client re-reads with `ctx_check` and follows from `earliest`.
//!   Nothing is skipped silently.
use crate::{
    actor::{Message, Snapshot},
    service::{Identity, Service, err},
};
use contextdb_model::*;
use contextdb_proto::proto::*;
use contextdb_store::{ChangePage, Durability};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Longest long-poll.
pub const MAX_WAIT: Duration = Duration::from_secs(30);
/// Most requests waiting at once.
pub const MAX_WAITERS: usize = 64;
/// Most items per page, and the default.
pub const MAX_ITEMS: u32 = 1_000;
pub const DEFAULT_ITEMS: u32 = 200;

/// Releases a waiter slot when the wait ends.
struct Slot<'a>(&'a std::sync::atomic::AtomicUsize);
impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl<D: Durability + Clone + 'static> Service<D> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn changes(
        &self,
        id: Identity,
        binding: Option<Binding>,
        branch: Option<String>,
        since: Epoch,
        scope: Option<String>,
        max_items: u32,
        wait_ms: u32,
        snap: Arc<Snapshot<D>>,
    ) -> Response {
        let b = match (&binding, &branch) {
            (Some(bnd), None) => match self.bind(id, bnd, &snap) {
                Ok(bound) => bound.1.lock().expect("session").branch,
                Err(r) => return r,
            },
            (Some(bnd), Some(name)) => {
                if let Err(r) = self.bind(id, bnd, &snap) {
                    return r;
                }
                match crate::service::resolve_branch(&snap, Some(name), None) {
                    Ok(b) => b,
                    Err(r) => return r,
                }
            }
            (None, _) if id.channel == Channel::Agent => {
                return err(
                    "InvalidArgument",
                    "agents read changes through a session binding",
                );
            }
            (None, Some(name)) => match crate::service::resolve_branch(&snap, Some(name), None) {
                Ok(b) => b,
                Err(r) => return r,
            },
            (None, None) => MAIN,
        };
        crate::redact::note_branch(b);
        drop(snap);
        let max = match max_items {
            0 => DEFAULT_ITEMS,
            n => n.min(MAX_ITEMS),
        } as usize;
        let wait = Duration::from_millis(u64::from(wait_ms)).min(MAX_WAIT);
        let deadline = Instant::now() + wait;
        let mut slot = None;
        loop {
            let seen = self.actor.snapshot().epoch;
            let page = match self.actor.request(|reply| Message::Changes {
                branch: b,
                since,
                max,
                reply,
            }) {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => return err("Io", format!("changes: {e:?}")),
                Err(e) => return err("Unavailable", format!("changes: {e:?}")),
            };
            let (rows, reached, more) = match page {
                ChangePage::Truncated { earliest } => return Response::Truncated { earliest },
                ChangePage::Rows {
                    rows,
                    reached,
                    more,
                } => (rows, reached, more),
            };
            let view_snap = self.actor.snapshot();
            let view = view_snap.view(b);
            let items: Vec<ChangeItem> = (rows.into_iter())
                .filter_map(|(epoch, rev, state)| {
                    let logical = view.revision_logical(rev)?;
                    let (key, _, lane) = view.logical_info(logical)?;
                    if scope.as_deref().is_some_and(|s| s != lane) {
                        return None;
                    }
                    Some(ChangeItem {
                        epoch,
                        logical,
                        rev,
                        key,
                        summary: contextdb_core::summarize(&state.dimensions),
                    })
                })
                .collect();
            drop(view_snap);
            let now = Instant::now();
            if !items.is_empty() || more || wait.is_zero() || now >= deadline {
                return Response::Changes {
                    items,
                    reached,
                    more,
                };
            }
            // Nothing yet: wait for the next published epoch.
            if slot.is_none() {
                if self.change_waiters.fetch_add(1, Ordering::AcqRel) >= MAX_WAITERS {
                    self.change_waiters.fetch_sub(1, Ordering::AcqRel);
                    return err("Busy", "too many ctx_changes waiting; retry");
                }
                slot = Some(Slot(&self.change_waiters));
            }
            self.actor.wait_epoch(seen, deadline - now);
        }
    }
}
