//! Format migrations (SPEC T68, ADR 0006 §7, §10). Migration is explicit:
//! open refuses an older format with `FormatMigrationRequired`.
//!
//! Format 0 (legacy, frozen here): identical framing, but `result_blob` lacks
//! `status_diff`. Migrating to format 1 re-derives every result through the
//! current kernel, checks the legacy fields against it, writes re-chained
//! frames with the re-derived result, and keeps the originally acked result as
//! `acked_result`. The old log is kept as `log.v0.bak`.
use crate::{
    durability::Durability,
    log::{self, FrameFault, LogRow},
    store::StoreError,
};
use contextdb_core::Db;
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
/// Rewrites a format-0 log as format 1. Frames are verified with the format-0
/// rules first; any inconsistency aborts with the old log untouched. Holds the
/// store lock, and finishes a migration interrupted between its two renames.
pub fn migrate(disk: &dyn Durability, dir: &Path) -> Result<MigrationReport, StoreError> {
    let _lock = crate::store::lock(disk, dir)?;
    if crate::store::migration_incomplete(disk, dir).map_err(io)? {
        return resume(disk, dir);
    }
    let from = match log_format(disk, dir)? {
        Some(f) if f == log::FORMAT_VERSION => {
            return Ok(MigrationReport {
                from: f,
                to: f,
                frames: 0,
                backup: String::new(),
            });
        }
        Some(0) => 0,
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
    let rows = legacy_rows(disk, &old_dir)?;
    let mut db = Db::default();
    let mut prev = log::GENESIS;
    let seg = new_dir.join(log::segment_name(Epoch(1)));
    disk.write_new(&seg, &log::segment_header(Epoch(1), prev))
        .map_err(io)?;
    for old in &rows {
        let batch = old.batch().map_err(|e| StoreError::Io(e.to_string()))?;
        let legacy = legacy::decode_result(&old.result_blob)
            .map_err(|_| StoreError::ReplayDivergence { epoch: old.epoch })?;
        let derived = db
            .commit(&batch)
            .map_err(|_| StoreError::ReplayDivergence { epoch: old.epoch })?;
        let derived_legacy = CommitResult {
            status_diff: Vec::new(),
            ..derived.clone()
        };
        if derived_legacy != legacy {
            return Err(StoreError::ReplayDivergence { epoch: old.epoch });
        }
        let row = LogRow {
            result_blob: derived.canonical_bytes(),
            acked_result: Some(legacy.canonical_bytes()),
            prev_checksum: prev,
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
    })
}
/// Second half of an interrupted swap: `log.migrating` was synced before the
/// old log was renamed to its backup, so it is the complete migrated log.
fn resume(disk: &dyn Durability, dir: &Path) -> Result<MigrationReport, StoreError> {
    let staged = dir.join("log.migrating");
    // The interrupted migration started from the newest older format, so its
    // backup carries the highest version (review N3).
    let backup = disk
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
        .max()
        .map(|(_, n)| n)
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
    let from = backup
        .trim_start_matches("log.v")
        .trim_end_matches(".bak")
        .parse()
        .unwrap_or(0);
    Ok(MigrationReport {
        from,
        to: log::FORMAT_VERSION,
        frames: scan.frames.len() as u64,
        backup: dir.join(backup).display().to_string(),
    })
}
/// Verified format-0 rows (same framing and chain rules as format 1).
fn legacy_rows(disk: &dyn Durability, dir: &Path) -> Result<Vec<LogRow>, StoreError> {
    let mut rows = Vec::new();
    let mut prev = log::GENESIS;
    for seg in log::segments(disk, dir).map_err(io)? {
        let bytes = disk.read(&seg).map_err(io)?;
        let header = log::parse_header(&bytes).ok_or(StoreError::ChainBroken {
            epoch: Epoch(rows.len() as u64 + 1),
            fault: FrameFault::SegmentHeader,
        })?;
        if header.format != 0 || header.prev != prev {
            return Err(StoreError::ChainBroken {
                epoch: Epoch(rows.len() as u64 + 1),
                fault: FrameFault::SegmentHeader,
            });
        }
        let mut pos = log::HEADER_LEN;
        while pos < bytes.len() {
            let epoch = Epoch(rows.len() as u64 + 1);
            let (row, sum, end) = log::parse_frame(&bytes, pos)
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
    let log_dir = dir.join("log");
    disk.create_dir_all(&log_dir).map_err(io)?;
    let seg = log_dir.join(log::segment_name(Epoch(1)));
    let mut header = log::segment_header(Epoch(1), log::GENESIS);
    header[8..12].copy_from_slice(&0u32.to_le_bytes());
    let check = log::header_check(&header[..52]);
    header[52..60].copy_from_slice(&check);
    disk.write_new(&seg, &header).map_err(io)?;
    let mut db = Db::default();
    let mut prev = log::GENESIS;
    let mut n = 0;
    for b in batches {
        let Ok(result) = db.commit(b) else { continue };
        let batch_blob = b.canonical_bytes();
        let row = LogRow {
            epoch: result.epoch,
            branch: b.branch,
            principal: b.principal,
            channel: b.channel,
            client_txn_id: b.client_txn_id,
            batch_hash: *blake3::hash(&batch_blob).as_bytes(),
            batch_blob,
            result_blob: legacy::encode_result(&result),
            acked_result: None,
            prev_checksum: prev,
        };
        let (bytes, sum) = log::frame(&row);
        disk.append(&seg, &bytes).map_err(io)?;
        prev = sum;
        n += 1;
    }
    disk.sync(&seg).map_err(io)?;
    disk.sync_dir(&log_dir).map_err(io)?;
    Ok(n)
}
