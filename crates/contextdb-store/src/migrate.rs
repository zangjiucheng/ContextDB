//! Format migrations (SPEC T68, ADR 0006 §7, §10). Migration is explicit:
//! open refuses an older format with `FormatMigrationRequired`.
//!
//! Format 0 (legacy, frozen here): identical framing, but `result_blob` lacks
//! `status_diff`. Migrating to format 1 re-derives every result through the
//! current kernel, checks the legacy fields against it, writes re-chained
//! frames with the re-derived result, and keeps the originally acked result as
//! `acked_result`. The old log is kept as `log.v0.bak`.
//!
//! Format 3 (P6, ADR 0013 §8) adds the branch ops and the result trailer.
//! Migrating any older format re-derives every result under the P6 kernel
//! (including the Conflict gate), appends the trailer, keeps the originally
//! acked result wherever the re-derived one differs, and reports those
//! epochs (T68 2→3).
use crate::{
    durability::Durability,
    log::{self, FrameFault, LogRow},
    store::StoreError,
};
use contextdb_core::{Db, Kernel};
use contextdb_model::*;
use std::path::Path;

/// Result encoding of format 0: CommitResult without `status_diff`.
pub mod legacy {
    use contextdb_model::*;
    pub fn encode_result(r: &CommitResult) -> Vec<u8> {
        let mut o = Vec::new();
        r.epoch.encode(&mut o);
        r.allocations.encode(&mut o);
        r.net_delta.encode(&mut o);
        r.l0_flips.encode(&mut o);
        o
    }
    /// Decodes a format-0 result; `status_diff` was never reported to clients.
    pub fn decode_result(bytes: &[u8]) -> Result<CommitResult, WireError> {
        let mut i = Decoder::new(bytes, DecodeLimits::DURABLE);
        let r = CommitResult {
            epoch: Wire::decode(&mut i)?,
            allocations: Wire::decode(&mut i)?,
            net_delta: Wire::decode(&mut i)?,
            l0_flips: Wire::decode(&mut i)?,
            status_diff: Vec::new(),
        };
        if i.remaining() != 0 {
            return Err(WireError::TrailingBytes);
        }
        Ok(r)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationReport {
    pub from: u32,
    pub to: u32,
    pub frames: u64,
    pub backup: String,
    /// Epochs whose re-derived result differs from the acked one, which is
    /// kept as `acked_result`.
    pub changed: Vec<Epoch>,
}
fn io(e: std::io::Error) -> StoreError {
    StoreError::Io(e.to_string())
}
/// Format of the first segment, if any.
pub fn log_format(disk: &dyn Durability, dir: &Path) -> Result<Option<u32>, StoreError> {
    let segs = log::segments(disk, &dir.join("log")).map_err(io)?;
    let Some(first) = segs.first() else {
        return Ok(None);
    };
    let bytes = disk.read(first).map_err(io)?;
    Ok(log::parse_header(&bytes).map(|h| h.format))
}
/// Rewrites a format-0 or format-1 log as the current format. Frames are
/// verified with their own format's rules first; any inconsistency aborts with
/// the old log untouched. Format 0 results are re-derived (they lack
/// `status_diff`); every migrated frame becomes its own group (format 2). Holds the
/// store lock, and finishes a migration interrupted between its two renames.
pub fn migrate(disk: &dyn Durability, dir: &Path) -> Result<MigrationReport, StoreError> {
    let _lock = crate::store::lock(disk, dir)?;
    if crate::store::migration_incomplete(disk, dir).map_err(io)? {
        return resume(disk, dir);
    }
    let from = match log_format(disk, dir)? {
        Some(f) if f == log::FORMAT_VERSION => {
            // A migration whose last step failed after the swap still lists
            // what it changed, from its backup (review). Best effort: a log
            // written since (a torn tail) leaves the list empty.
            let (backup, changed) = match newest_backup(disk, dir)? {
                Some((v, name)) => {
                    let changed = changed_since(disk, dir, &dir.join(&name), v).unwrap_or_default();
                    (dir.join(name).display().to_string(), changed)
                }
                None => (String::new(), Vec::new()),
            };
            return Ok(MigrationReport {
                from: f,
                to: f,
                frames: 0,
                backup,
                changed,
            });
        }
        Some(f @ (0..=2)) => f,
        Some(f) if f > log::FORMAT_VERSION => return Err(StoreError::FormatTooNew(f)),
        Some(f) => return Err(StoreError::FormatUnsupported(f)),
        None => return Err(StoreError::Io("no log to migrate".into())),
    };
    let old_dir = dir.join("log");
    let new_dir = dir.join("log.migrating");
    if disk.exists(&new_dir) {
        for n in disk.list(&new_dir).map_err(io)? {
            disk.remove(&new_dir.join(n)).map_err(io)?;
        }
    }
    disk.create_dir_all(&new_dir).map_err(io)?;
    let rows = legacy_rows(disk, &old_dir, from)?;
    let mut kernel = Kernel::default();
    let mut prev = log::GENESIS;
    let mut changed = Vec::new();
    let seg = new_dir.join(log::segment_name(Epoch(1)));
    disk.write_new(&seg, &log::segment_header(Epoch(1), prev))
        .map_err(io)?;
    for old in &rows {
        let batch = old.batch().map_err(|e| StoreError::Io(e.to_string()))?;
        let (derived, merge) = kernel
            .commit_detailed(&batch)
            .map_err(|_| StoreError::ReplayDivergence { epoch: old.epoch })?;
        if derived.epoch != old.epoch {
            return Err(StoreError::ReplayDivergence { epoch: old.epoch });
        }
        let (recorded, same) = compare(from, old, &derived)?;
        if !same {
            changed.push(old.epoch);
        }
        let acked_result = match (&old.acked_result, from, same) {
            // An earlier migration's record of what the client was told.
            (Some(acked), _, _) => Some(acked.clone()),
            // Format 0 never reported `status_diff`: keep what it did report.
            (None, 0, _) | (None, _, false) => Some(recorded.canonical_bytes()),
            (None, _, true) => None,
        };
        // Format 2 groups are real; older frames become their own group.
        let group = if from == 2 {
            old.group
        } else {
            log::GroupPos {
                seq: old.epoch.0,
                idx: 0,
                count: 1,
            }
        };
        let trailer = ResultTrailer {
            branch: batch.branch,
            merge,
        };
        let row = LogRow {
            result_blob: log::encode_result(&derived, &trailer),
            acked_result,
            prev_checksum: prev,
            group,
            client_hash: None,
            ..old.clone()
        };
        let (bytes, sum) = log::frame(&row);
        disk.append(&seg, &bytes).map_err(io)?;
        prev = sum;
    }
    disk.sync(&seg).map_err(io)?;
    disk.sync_dir(&new_dir).map_err(io)?;
    // `log.migrating`'s own entry is durable before the swap begins (N3).
    disk.sync_dir(dir).map_err(io)?;
    // Swap: keep the old log as a backup. `log.migrating` is complete and
    // durable before the first rename. A crash before it leaves the old log
    // and a stale `log.migrating` (discarded above on the next run); a crash
    // between the renames leaves no `log/`, which open refuses and `resume`
    // finishes.
    let backup = dir.join(format!("log.v{from}.bak"));
    disk.rename_dir(&old_dir, &backup).map_err(io)?;
    disk.rename_dir(&new_dir, &old_dir).map_err(io)?;
    disk.sync_dir(dir).map_err(io)?;
    Ok(MigrationReport {
        from,
        to: log::FORMAT_VERSION,
        frames: rows.len() as u64,
        backup: backup.display().to_string(),
        changed,
    })
}
/// Second half of an interrupted swap: `log.migrating` was synced before the
/// old log was renamed to its backup, so it is the complete migrated log.
fn resume(disk: &dyn Durability, dir: &Path) -> Result<MigrationReport, StoreError> {
    let staged = dir.join("log.migrating");
    // The interrupted migration started from the newest older format, so its
    // backup carries the highest version (review N3).
    let (from, backup) = newest_backup(disk, dir)?
        .ok_or_else(|| StoreError::Io("no migration backup to resume from".into()))?;
    if !disk.exists(&staged) {
        return Err(StoreError::Io(format!(
            "log/ and log.migrating are both missing; restore {backup} manually"
        )));
    }
    let scan = log::scan(disk, &staged).map_err(io)?;
    if scan.broken.is_some() || scan.torn_tail.is_some() || scan.empty_tail_segment.is_some() {
        return Err(StoreError::Io(format!(
            "log.migrating does not verify; restore {backup} manually"
        )));
    }
    disk.rename_dir(&staged, &dir.join("log")).map_err(io)?;
    disk.sync_dir(dir).map_err(io)?;
    // The interrupted run never reported its list: derive it again.
    let changed = changed_since(disk, dir, &dir.join(&backup), from)?;
    Ok(MigrationReport {
        from,
        to: log::FORMAT_VERSION,
        frames: scan.frames.len() as u64,
        backup: dir.join(backup).display().to_string(),
        changed,
    })
}
/// The newest `log.vN.bak`: an interrupted migration started from the newest
/// older format, so its backup carries the highest version (review N3).
fn newest_backup(disk: &dyn Durability, dir: &Path) -> Result<Option<(u32, String)>, StoreError> {
    Ok(disk
        .list(dir)
        .map_err(io)?
        .into_iter()
        .filter_map(|n| {
            let v: u32 = n
                .strip_prefix("log.v")?
                .strip_suffix(".bak")?
                .parse()
                .ok()?;
            Some((v, n))
        })
        .max())
}
/// The recorded result of a legacy row and whether the re-derived one
/// equals it (T68). The base is the result the old binary derived and
/// logged, not an earlier migration's `acked_result`: a format-0 store's
/// acked results lack `status_diff`, and comparing against them would list
/// every epoch.
fn compare(
    from: u32,
    old: &LogRow,
    derived: &CommitResult,
) -> Result<(CommitResult, bool), StoreError> {
    let recorded = match from {
        0 => legacy::decode_result(&old.result_blob),
        _ => CommitResult::from_bytes_with(&old.result_blob, DecodeLimits::DURABLE),
    }
    .map_err(|_| StoreError::ReplayDivergence { epoch: old.epoch })?;
    let same = match from {
        // Format 0 lacked `status_diff`.
        0 => {
            CommitResult {
                status_diff: Vec::new(),
                ..derived.clone()
            } == recorded
        }
        _ => *derived == recorded,
    };
    Ok((recorded, same))
}
/// The epochs a finished migration changed: the backup's rows against the
/// migrated log's results.
fn changed_since(
    disk: &dyn Durability,
    dir: &Path,
    backup: &Path,
    from: u32,
) -> Result<Vec<Epoch>, StoreError> {
    let old = legacy_rows(disk, backup, from)?;
    let new = legacy_rows(disk, &dir.join("log"), log::FORMAT_VERSION)?;
    let mut changed = Vec::new();
    for (o, n) in old.iter().zip(&new) {
        let derived = n
            .result()
            .map_err(|_| StoreError::ReplayDivergence { epoch: n.epoch })?;
        if !compare(from, o, &derived)?.1 {
            changed.push(o.epoch);
        }
    }
    Ok(changed)
}
/// Verified rows of a legacy format (formats 0 and 1 share framing and chain rules).
fn legacy_rows(disk: &dyn Durability, dir: &Path, format: u32) -> Result<Vec<LogRow>, StoreError> {
    let mut rows = Vec::new();
    let mut prev = log::GENESIS;
    for seg in log::segments(disk, dir).map_err(io)? {
        let bytes = disk.read(&seg).map_err(io)?;
        let header = log::parse_header(&bytes).ok_or(StoreError::ChainBroken {
            epoch: Epoch(rows.len() as u64 + 1),
            fault: FrameFault::SegmentHeader,
        })?;
        if header.format != format || header.prev != prev {
            return Err(StoreError::ChainBroken {
                epoch: Epoch(rows.len() as u64 + 1),
                fault: FrameFault::SegmentHeader,
            });
        }
        let mut pos = log::HEADER_LEN;
        while pos < bytes.len() {
            let epoch = Epoch(rows.len() as u64 + 1);
            let (row, sum, end) =
                log::parse_frame_with::<LogRow>(&log::OP_LOG, &bytes, pos, format)
                    .map_err(|fault| StoreError::ChainBroken { epoch, fault })?;
            if row.prev_checksum != prev || row.epoch != epoch {
                return Err(StoreError::ChainBroken {
                    epoch,
                    fault: FrameFault::Chain,
                });
            }
            prev = sum;
            pos = end;
            rows.push(row);
        }
    }
    Ok(rows)
}
/// Test and tooling support: writes a format-0 log for `batches` (frozen layout).
pub fn write_legacy(
    disk: &dyn Durability,
    dir: &Path,
    batches: &[Batch],
) -> Result<u64, StoreError> {
    write_format(disk, dir, batches, 0)
}
/// Test and tooling support: writes a format-2 log (P4–P5 layout, before
/// branches): one group per frame.
pub fn write_format2(
    disk: &dyn Durability,
    dir: &Path,
    batches: &[Batch],
) -> Result<u64, StoreError> {
    write_format(disk, dir, batches, 2)
}
/// Test support: a format-2 log whose recorded results pass through
/// `recorded` first, as an older kernel would have computed them.
pub fn write_format2_with(
    disk: &dyn Durability,
    dir: &Path,
    batches: &[Batch],
    recorded: &dyn Fn(&Batch, CommitResult) -> CommitResult,
) -> Result<u64, StoreError> {
    write_format_with(disk, dir, batches, 2, recorded)
}
/// Test and tooling support: writes a format-1 log (P2 layout, no group header).
pub fn write_format1(
    disk: &dyn Durability,
    dir: &Path,
    batches: &[Batch],
) -> Result<u64, StoreError> {
    write_format(disk, dir, batches, 1)
}
fn write_format(
    disk: &dyn Durability,
    dir: &Path,
    batches: &[Batch],
    format: u32,
) -> Result<u64, StoreError> {
    write_format_with(disk, dir, batches, format, &|_, r| r)
}
fn write_format_with(
    disk: &dyn Durability,
    dir: &Path,
    batches: &[Batch],
    format: u32,
    recorded: &dyn Fn(&Batch, CommitResult) -> CommitResult,
) -> Result<u64, StoreError> {
    let log_dir = dir.join("log");
    disk.create_dir_all(&log_dir).map_err(io)?;
    let seg = log_dir.join(log::segment_name(Epoch(1)));
    let header = log::segment_header_with(&log::OP_LOG, 1, log::GENESIS, format);
    disk.write_new(&seg, &header).map_err(io)?;
    let mut db = Db::default();
    let mut prev = log::GENESIS;
    let mut n = 0;
    for b in batches {
        let Ok(result) = db.commit(b) else { continue };
        let result = recorded(b, result);
        let batch_blob = b.canonical_bytes();
        let row = LogRow {
            epoch: result.epoch,
            branch: b.branch,
            principal: b.principal,
            channel: b.channel,
            client_txn_id: b.client_txn_id,
            batch_hash: *blake3::hash(&batch_blob).as_bytes(),
            batch_blob,
            result_blob: if format == 0 {
                legacy::encode_result(&result)
            } else {
                result.canonical_bytes()
            },
            acked_result: None,
            client_hash: None,
            prev_checksum: prev,
            group: if format >= 2 {
                log::GroupPos {
                    seq: result.epoch.0,
                    idx: 0,
                    count: 1,
                }
            } else {
                log::GroupPos::default()
            },
        };
        let (bytes, sum) = log::frame_format(&row, format);
        disk.append(&seg, &bytes).map_err(io)?;
        prev = sum;
        n += 1;
    }
    disk.sync(&seg).map_err(io)?;
    disk.sync_dir(&log_dir).map_err(io)?;
    Ok(n)
}
