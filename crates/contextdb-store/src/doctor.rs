//! Operator repair (SPEC T33/T34, ADR 0006 §4 step 3). `truncate_before` is the
//! only way to open a log with a corrupt non-final frame: it drops every frame
//! from `epoch` on. The plan lists every readable frame that would be dropped
//! (including frames after an unreadable region and in later segments) and
//! whether unreadable bytes are dropped too. Execution holds the store lock and
//! re-plans, so it never races a running store or a stale plan.
use crate::{durability::Durability, log, store::StoreError};
use contextdb_model::*;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruncationPlan {
    /// New tail: every frame after it is dropped.
    pub keep_through: Epoch,
    /// Dropped frames that are still readable (acked data being lost), in log order.
    pub dropped_epochs: Vec<Epoch>,
    pub dropped_txn_ids: Vec<[u8; 16]>,
    /// Some dropped bytes could not be read as frames: more acked epochs may be lost.
    pub unreadable_tail: bool,
    cut: Option<(PathBuf, u64)>,
    later_segments: Vec<PathBuf>,
}
fn io(e: std::io::Error) -> StoreError {
    StoreError::Io(e.to_string())
}
/// Dry run: verifies frames before `epoch` and lists what would be dropped.
pub fn plan_truncate_before(
    disk: &dyn Durability,
    dir: &Path,
    epoch: Epoch,
) -> Result<TruncationPlan, StoreError> {
    if epoch.0 == 0 {
        return Err(StoreError::Tx(TxError::UnknownRef));
    }
    let log_dir = dir.join("log");
    let mut plan = TruncationPlan {
        keep_through: Epoch(epoch.0 - 1),
        dropped_epochs: Vec::new(),
        dropped_txn_ids: Vec::new(),
        unreadable_tail: false,
        cut: None,
        later_segments: Vec::new(),
    };
    let mut prev = log::GENESIS;
    let mut next = Epoch(1);
    for seg in log::segments(disk, &log_dir).map_err(io)? {
        let bytes = disk.read(&seg).map_err(io)?;
        if plan.cut.is_some() || next >= epoch {
            // At or after the cut: the whole segment is dropped. A segment
            // boundary cut keeps nothing of this segment.
            if plan.cut.is_none() {
                plan.cut = Some((seg.clone(), 0));
            }
            let start = if log::parse_header(&bytes).is_some() {
                log::HEADER_LEN
            } else {
                plan.unreadable_tail |= !bytes.is_empty();
                0
            };
            dropped(&mut plan, &bytes, start);
            plan.later_segments.push(seg);
            continue;
        }
        if log::parse_header(&bytes).is_none_or(|h| h.first_epoch != next || h.prev != prev) {
            return Err(StoreError::ChainBroken {
                epoch: next,
                fault: log::FrameFault::SegmentHeader,
            });
        }
        let mut pos = log::HEADER_LEN;
        // Start of the group being read, if it is not complete yet.
        let mut group: Option<(usize, Epoch)> = None;
        while pos < bytes.len() {
            if next == epoch {
                // Cut at a group boundary (ADR 0008 §3.2): a partial group
                // would be truncated as a torn tail at the next open, so the
                // whole group is dropped and listed.
                let (cut, first) = group.unwrap_or((pos, epoch));
                plan.keep_through = Epoch(first.0 - 1);
                plan.cut = Some((seg.clone(), cut as u64));
                dropped(&mut plan, &bytes, cut);
                break;
            }
            match log::parse_frame(&bytes, pos) {
                Ok((row, sum, end)) if row.epoch == next && row.prev_checksum == prev => {
                    if row.group.idx == 0 {
                        group = Some((pos, row.epoch));
                    }
                    if row.group.idx + 1 == row.group.count {
                        group = None;
                    }
                    prev = sum;
                    next = Epoch(next.0 + 1);
                    pos = end;
                }
                _ => {
                    return Err(StoreError::ChainBroken {
                        epoch: next,
                        fault: log::FrameFault::Checksum,
                    });
                }
            }
        }
    }
    if next < epoch {
        // Nothing at or after `epoch`: the log already ends before it.
        plan.keep_through = Epoch(next.0 - 1);
    }
    Ok(plan)
}
/// Records every readable frame from `pos` on as dropped.
fn dropped(plan: &mut TruncationPlan, bytes: &[u8], pos: usize) {
    let (rows, unreadable) = log::readable_frames(bytes, pos);
    plan.unreadable_tail |= unreadable;
    for row in rows {
        plan.dropped_epochs.push(row.epoch);
        if let Some(id) = row.client_txn_id {
            plan.dropped_txn_ids.push(id);
        }
    }
}
/// Executes a plan: truncates the cut segment, removes later segments and
/// snapshots beyond the new tail, and syncs. Projections are rebuilt at open.
pub fn truncate_before(
    disk: &dyn Durability,
    dir: &Path,
    plan: &TruncationPlan,
) -> Result<(), StoreError> {
    let _lock = crate::store::lock(disk, dir)?;
    // The log must still be exactly what the operator reviewed.
    if plan_truncate_before(disk, dir, Epoch(plan.keep_through.0 + 1))? != *plan {
        return Err(StoreError::PlanOutdated);
    }
    let log_dir = dir.join("log");
    if let Some((seg, pos)) = &plan.cut
        && *pos > 0
    {
        disk.truncate(seg, *pos).map_err(io)?;
        disk.sync(seg).map_err(io)?;
    }
    for seg in &plan.later_segments {
        if plan
            .cut
            .as_ref()
            .is_none_or(|(s, pos)| s != seg || *pos == 0)
        {
            disk.remove(seg).map_err(io)?;
        }
    }
    disk.sync_dir(&log_dir).map_err(io)?;
    let snap = dir.join("snap");
    if disk.exists(&snap) {
        for name in disk.list(&snap).map_err(io)? {
            let epoch = name
                .strip_prefix("snap-")
                .and_then(|n| n.strip_suffix(".bin"))
                .and_then(|n| n.parse::<u64>().ok());
            if epoch.is_none_or(|e| e > plan.keep_through.0) {
                disk.remove(&snap.join(name)).map_err(io)?;
            }
        }
        disk.sync_dir(&snap).map_err(io)?;
    }
    Ok(())
}
