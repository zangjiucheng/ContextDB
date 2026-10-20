//! Operator repair (SPEC T33/T34, ADR 0006 §4 step 3). `truncate_before` is the
//! only way to open a log with a corrupt non-final frame: it drops every frame
//! from `epoch` on, and the plan lists the acked epochs and client_txn_ids lost.
use crate::{
    durability::Durability,
    log::{self, LogRow},
    store::StoreError,
};
use contextdb_model::*;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruncationPlan {
    /// New tail: every frame after it is dropped.
    pub keep_through: Epoch,
    /// Dropped frames that were still readable (acked data being lost).
    pub dropped_epochs: Vec<Epoch>,
    pub dropped_txn_ids: Vec<[u8; 16]>,
    /// Unreadable bytes after the last readable dropped frame.
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
        if plan.cut.is_some() {
            plan.later_segments.push(seg);
            continue;
        }
        let bytes = disk.read(&seg).map_err(io)?;
        let mut pos = log::HEADER_LEN;
        if next >= epoch {
            // The cut falls on a segment boundary: drop this whole segment.
            plan.cut = Some((seg.clone(), 0));
            plan.later_segments.push(seg);
            continue;
        }
        while pos < bytes.len() {
            if next == epoch {
                plan.cut = Some((seg.clone(), pos as u64));
            }
            match log::parse_frame(&bytes, pos) {
                Ok((row, sum, end)) if row.epoch == next && row.prev_checksum == prev => {
                    if next >= epoch {
                        dropped(&mut plan, &row);
                    }
                    prev = sum;
                    next = Epoch(next.0 + 1);
                    pos = end;
                }
                _ if next < epoch => {
                    return Err(StoreError::ChainBroken {
                        epoch: next,
                        fault: log::FrameFault::Checksum,
                    });
                }
                _ => {
                    plan.unreadable_tail = true;
                    break;
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
fn dropped(plan: &mut TruncationPlan, row: &LogRow) {
    plan.dropped_epochs.push(row.epoch);
    if let Some(id) = row.client_txn_id {
        plan.dropped_txn_ids.push(id);
    }
}
/// Executes a plan: truncates the cut segment, removes later segments and
/// snapshots beyond the new tail, and syncs. Projections are rebuilt at open.
pub fn truncate_before(
    disk: &dyn Durability,
    dir: &Path,
    plan: &TruncationPlan,
) -> Result<(), StoreError> {
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
