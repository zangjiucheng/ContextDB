//! `ctx migrate` to format 5 (ADR 0017 §8.5, §10): the log headers, then
//! the packs, the projections and the modes, and success only after a
//! verification against the disk.
//!
//! 0. Pre-checks, read-only: `personal:<x>` lanes with a valid `<x>`, and
//!    free space for one copy of the plaintext packs and the projections.
//! 1. `<data>/MIGRATING-5`, synced with `<data>`. From here until step 7
//!    the store is mid-migration and every opener refuses it
//!    (`MigrationIncomplete{step}`).
//! 2. The log, through the existing `log.migrating` swap (decided from the
//!    log itself, never from the marker).
//! 3. The packs: each plaintext segment and loose blob file is re-encrypted
//!    into a new segment (keys first), synced, read back and checked, and
//!    only then unlinked. Shredded contents are not carried over.
//! 4. The body-bearing projections: `VACUUM INTO` a fresh file stamped
//!    `rebuilt_at_format = 5`, renamed over the old one, whose `-wal` and
//!    `-shm` are unlinked first (after a `TRUNCATE` checkpoint).
//! 5. The modes and ACLs, through the caller's [`Modes`].
//! 6. Verification of steps 2 to 5 against the disk.
//! 7. The marker unlinked, `<data>` synced.
//!
//! Each step redoes only what its own check finds missing, so a rerun
//! after a kill or a power cut at any point completes.
use crate::{
    durability::Durability,
    keys, log, pack, purge,
    store::{StoreError, migration_incomplete},
};
use contextdb_model::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const MARKER: &str = "MIGRATING-5";
/// The suffix of a projection being rebuilt.
const REBUILDING: &str = ".migrating";

fn io(e: std::io::Error) -> StoreError {
    StoreError::Io(e.to_string())
}
fn sql(e: rusqlite::Error) -> StoreError {
    StoreError::Io(format!("projection: {e}"))
}

/// Step 5 (ADR 0017 §3.2), supplied by the caller: the store crate does not
/// know the data directory's public entries.
pub trait Modes {
    /// Applies private modes and strips ACLs.
    fn apply(&self, dir: &Path) -> Result<(), String>;
    /// The §3.2 ownership, mode and ACL check.
    fn check(&self, dir: &Path) -> Result<(), String>;
}
/// No mode step (tests on `SimDisk`, whose files have no modes).
pub struct NoModes;
impl Modes for NoModes {
    fn apply(&self, _: &Path) -> Result<(), String> {
        Ok(())
    }
    fn check(&self, _: &Path) -> Result<(), String> {
        Ok(())
    }
}

/// Test faults (T104): stop points and a skipped step 2.
#[derive(Clone, Copy, Debug, Default)]
pub struct Faults {
    pub stop: Option<Stop>,
    /// Skips step 2, so step 6 must refuse success.
    pub skip_log: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// After step `n` (1 to 6).
    After(u8),
    /// Inside step 3, after the `n`-th source (0-based) is unlinked.
    Source(usize),
    /// Inside step 4, after the `n`-th projection (0-based) is renamed.
    Projection(usize),
}
/// The error a stop point returns.
pub const STOPPED: &str = "migration stopped (test fault)";
fn stop(faults: &Faults, at: Stop) -> Result<(), StoreError> {
    if faults.stop == Some(at) {
        return Err(StoreError::Io(STOPPED.into()));
    }
    Ok(())
}

/// What steps 3 and 4 did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Encryption {
    /// Plaintext pack segments re-encrypted.
    pub segments: u64,
    /// Loose blob files re-encrypted.
    pub loose: u64,
    /// Records written encrypted.
    pub records: u64,
    /// Plaintext records of shredded contents, dropped.
    pub dropped_shredded: u64,
    /// Projections rebuilt.
    pub projections: Vec<String>,
    /// Contents the op_log references that no pack holds (lost before the
    /// migration; nothing can make them decrypt).
    pub missing: u64,
}

/// Every content hash an op names (ADR 0017 §8.3's list).
pub fn op_contents(op: &Op) -> Vec<ContentHash> {
    match op {
        Op::CreateLogical { content, .. } | Op::AddRevision { content, .. } => vec![*content],
        Op::RecordValidation { meta, .. } => meta.iter().copied().collect(),
        Op::OpenArtifact { payload_hash, .. } => vec![*payload_hash],
        Op::ResolveArtifact { note, .. } => note.iter().copied().collect(),
        _ => Vec::new(),
    }
}
/// The batches of the log in `<dir>/log`, at whatever format it has.
fn batches(disk: &dyn Durability, dir: &Path) -> Result<Vec<Batch>, StoreError> {
    let Some(format) = crate::migrate::log_format(disk, dir)? else {
        return Ok(Vec::new());
    };
    crate::migrate::rows_for_test(disk, dir, format)?
        .iter()
        .map(|r| r.batch().map_err(|e| StoreError::Io(e.to_string())))
        .collect()
}
/// Contents referenced by the log, and the shredded ones.
fn referenced(
    disk: &dyn Durability,
    dir: &Path,
) -> Result<(BTreeSet<ContentHash>, BTreeSet<ContentHash>), StoreError> {
    let (mut named, mut shredded) = (BTreeSet::new(), BTreeSet::new());
    for b in batches(disk, dir)? {
        for op in &b.ops {
            named.extend(op_contents(op));
            if let Op::ShredContent { content } = op {
                shredded.insert(*content);
            }
        }
    }
    Ok((named, shredded))
}

/// Step 0's lane check (ADR 0017 §4.2): every `personal:<x>` lane names a
/// valid principal name.
fn check_lanes(disk: &dyn Durability, dir: &Path) -> Result<(), StoreError> {
    for b in batches(disk, dir)? {
        for op in &b.ops {
            if let Op::CreateLogical { scope_lane, .. } = op
                && let Some(name) = scope_lane.strip_prefix("personal:")
                && !contextdb_core::access::valid_name(name)
            {
                return Err(StoreError::Tx(TxError::InvalidAccess {
                    reason: format!(
                        "lane {scope_lane:?}: `{name}` is not a valid principal name \
                         (1-32 of a-z, 0-9, _ and -); nothing was changed"
                    ),
                }));
            }
        }
    }
    Ok(())
}
fn file_len(disk: &dyn Durability, p: &Path) -> u64 {
    disk.read(p).map_or(0, |b| b.len() as u64)
}
/// Step 0's space check (§8.5): one copy of the plaintext packs and of the
/// projections still to rebuild.
fn check_space(disk: &dyn Durability, dir: &Path) -> Result<(), StoreError> {
    let blob_dir = dir.join("blobs");
    let mut need = 0u64;
    if disk.exists(&blob_dir) {
        for (_, p) in plaintext_sources(disk, &blob_dir)? {
            need += file_len(disk, &p);
        }
    }
    for name in purge::FILES {
        let p = dir.join(name);
        if !purge::stamped(&p).unwrap_or(false) {
            need += std::fs::metadata(&p).map_or(0, |m| m.len());
        }
    }
    if let Some(free) = disk.free_space(dir).map_err(io)?
        && free < need
    {
        return Err(StoreError::Io(format!(
            "migration needs {need} bytes free for the re-encrypted packs and rebuilt \
             projections, and {free} are available; nothing was changed"
        )));
    }
    Ok(())
}
/// Plaintext pack segments (`false`) and loose blob files (`true`), oldest
/// segment first.
fn plaintext_sources(
    disk: &dyn Durability,
    blob_dir: &Path,
) -> Result<Vec<(bool, PathBuf)>, StoreError> {
    let mut names = disk.list(blob_dir).map_err(io)?;
    names.sort();
    let mut out = Vec::new();
    for n in names {
        let p = blob_dir.join(&n);
        if pack::is_pack(&n) {
            // A segment holding a valid plaintext record; a torn or garbage
            // one holds no plaintext payload.
            let data = disk.read(&p).map_err(io)?;
            if pack::parse_all(&data, &|_| None)
                .iter()
                .any(|r| r.3.is_none())
            {
                out.push((false, p));
            }
        } else if crate::store::unhex(&n).is_some() {
            out.push((true, p));
        }
    }
    Ok(out)
}

/// Step 2's check: no swap in progress, and `log/` at the current format
/// (a log with no segment yet, a store with no commit, is current).
fn log_current(disk: &dyn Durability, dir: &Path) -> Result<bool, StoreError> {
    if migration_incomplete(disk, dir).map_err(io)? || !disk.exists(&dir.join("log")) {
        return Ok(false);
    }
    Ok(matches!(
        crate::migrate::log_format(disk, dir)?,
        None | Some(log::FORMAT_VERSION)
    ))
}
/// The first step of §10 whose check fails on disk (2 to 5), or `None`;
/// and how many referenced contents no pack holds.
pub fn verify(
    disk: &dyn Durability,
    dir: &Path,
    modes: &dyn Modes,
) -> Result<(Option<u8>, u64), StoreError> {
    if !log_current(disk, dir)? {
        return Ok((Some(2), 0));
    }
    let (blob_dir, key_dir) = (dir.join("blobs"), dir.join(keys::DIR));
    let mut missing = 0;
    if disk.exists(&blob_dir) {
        if !plaintext_sources(disk, &blob_dir)?.is_empty() {
            return Ok((Some(3), 0));
        }
        let keylog = if disk.exists(&key_dir) {
            keys::load(disk, &key_dir).map_err(io)?
        } else {
            keys::KeyLog::default()
        };
        let names = disk.list(&blob_dir).map_err(io)?;
        let index = pack::scan(disk, &blob_dir, &names, keylog.keys).map_err(io)?;
        let (named, shredded) = referenced(disk, dir)?;
        for c in named.difference(&shredded) {
            // A record in the index is encrypted and authenticated under
            // its key (plaintext segments were ruled out above).
            match index.locate(c) {
                Some(l) if matches!(l.sealed, Some((_, Some(_)))) => {}
                Some(_) => return Ok((Some(3), 0)),
                None => missing += 1,
            }
        }
    }
    if disk.exists(&key_dir)
        && disk
            .list(&key_dir)
            .map_err(io)?
            .iter()
            .any(|n| n.ends_with(".tmp"))
    {
        return Ok((Some(3), 0));
    }
    for name in purge::FILES {
        let p = dir.join(name);
        if !purge::stamped(&p).map_err(sql)?
            || std::fs::exists(dir.join(format!("{name}{REBUILDING}"))).unwrap_or(false)
        {
            return Ok((Some(4), 0));
        }
    }
    if modes.check(dir).is_err() {
        return Ok((Some(5), 0));
    }
    Ok((None, missing))
}

/// A store with the marker (ADR 0017 §10, "Opening a mid-migration
/// store"): the first step whose cheap check fails, 6 when none does.
/// `None` without the marker.
pub fn marked_step(disk: &dyn Durability, dir: &Path) -> Result<Option<u8>, StoreError> {
    if !disk.exists(&dir.join(MARKER)) {
        return Ok(None);
    }
    if !log_current(disk, dir)? {
        return Ok(Some(2));
    }
    let blob_dir = dir.join("blobs");
    if disk.exists(&blob_dir) && !plaintext_sources(disk, &blob_dir)?.is_empty() {
        return Ok(Some(3));
    }
    Ok(Some(unstamped(dir)?.unwrap_or(6)))
}
/// Step 4's cheap check: `Some(4)` when a projection lacks the stamp.
pub fn unstamped(dir: &Path) -> Result<Option<u8>, StoreError> {
    for name in purge::FILES {
        if !purge::stamped(&dir.join(name)).map_err(sql)? {
            return Ok(Some(4));
        }
    }
    Ok(None)
}
/// `doctor` and other offline tools: refuse a store with the marker.
pub fn refuse_marked(disk: &dyn Durability, dir: &Path) -> Result<(), StoreError> {
    match marked_step(disk, dir)? {
        Some(step) => Err(StoreError::MigrationIncomplete { step }),
        None => Ok(()),
    }
}

/// Step 0 and step 1, when the store is not yet marked.
pub(crate) fn begin(disk: &dyn Durability, dir: &Path, swap: bool) -> Result<(), StoreError> {
    if !swap {
        check_lanes(disk, dir)?;
    }
    check_space(disk, dir)?;
    let marker = dir.join(MARKER);
    disk.write_new(&marker, b"5\n").map_err(io)?;
    disk.sync(&marker).map_err(io)?;
    disk.sync_dir(dir).map_err(io)
}

/// Step 3: re-encrypts every plaintext segment and loose blob.
pub(crate) fn packs(
    disk: &dyn Durability,
    dir: &Path,
    faults: &Faults,
    out: &mut Encryption,
) -> Result<(), StoreError> {
    let (blob_dir, key_dir) = (dir.join("blobs"), dir.join(keys::DIR));
    if !disk.exists(&blob_dir) {
        return Ok(());
    }
    disk.create_dir_all(&key_dir).map_err(io)?;
    disk.sync_dir(dir).map_err(io)?;
    let (_, shredded) = referenced(disk, dir)?;
    let mut keylog = keys::load(disk, &key_dir).map_err(io)?;
    let sources = plaintext_sources(disk, &blob_dir)?;
    let mut next_pack = disk
        .list(&blob_dir)
        .map_err(io)?
        .iter()
        .filter(|n| pack::is_pack(n))
        .filter_map(|n| n.get(5..)?.parse::<usize>().ok())
        .max()
        .map_or(1, |n| n + 1);
    for (k, (loose, path)) in sources.into_iter().enumerate() {
        let data = disk.read(&path).map_err(io)?;
        let mut records: BTreeMap<ContentHash, Vec<u8>> = BTreeMap::new();
        if loose {
            let hash = *blake3::hash(&data).as_bytes();
            // A loose file whose bytes do not match its name was unreadable.
            if Some(hash)
                == path
                    .file_name()
                    .and_then(|n| crate::store::unhex(n.to_str()?))
            {
                records.insert(hash, data.clone());
            }
            out.loose += 1;
        } else {
            for (hash, off, len, nonce) in pack::parse_all(&data, &|_| None) {
                if nonce.is_none() {
                    let at = off as usize;
                    records.insert(hash, data[at..at + len as usize].to_vec());
                }
            }
            out.segments += 1;
        }
        let before = records.len();
        records.retain(|h, _| !shredded.contains(h));
        out.dropped_shredded += (before - records.len()) as u64;
        if !records.is_empty() {
            // Keys first, synced, as the store does (§8.2).
            let new: Vec<(ContentHash, keys::Key)> = records
                .keys()
                .filter(|h| !keylog.keys.contains_key(*h))
                .map(|h| keys::fresh().map(|key| (*h, key)))
                .collect::<std::io::Result<_>>()
                .map_err(io)?;
            if !new.is_empty() {
                let n = keys::next_number(&keylog);
                let kp = key_dir.join(keys::segment_name(n));
                let mut bytes = keys::header(keys::Kind::Append, Epoch(0));
                for (h, key) in &new {
                    bytes.extend(keys::record(h, key));
                }
                disk.write_new(&kp, &bytes).map_err(io)?;
                disk.sync(&kp).map_err(io)?;
                disk.sync_dir(&key_dir).map_err(io)?;
                keylog.keys.extend(new);
                keylog.segments.push((n, kp));
            }
            let mut bytes = pack::SEGMENT_HEADER.to_vec();
            for (h, b) in &records {
                let (_, rec) = pack::sealed(b, &keylog.keys[h])
                    .ok_or_else(|| StoreError::Io("encrypting a payload failed".into()))?;
                bytes.extend(rec);
            }
            let np = blob_dir.join(pack::segment_name(next_pack));
            next_pack += 1;
            disk.write_new(&np, &bytes).map_err(io)?;
            disk.sync(&np).map_err(io)?;
            disk.sync_dir(&blob_dir).map_err(io)?;
            // Read back: every record authenticates and decrypts to its hash.
            let back = disk.read(&np).map_err(io)?;
            let found: BTreeMap<ContentHash, (u64, u32, [u8; pack::NONCE])> =
                pack::parse_all(&back, &|h| keylog.keys.get(h).copied())
                    .into_iter()
                    .filter_map(|(h, off, len, n)| Some((h, (off, len, n?))))
                    .collect();
            for (h, b) in &records {
                let ok = found.get(h).and_then(|(off, len, nonce)| {
                    let ct = back.get(*off as usize..(*off + *len as u64) as usize)?;
                    pack::open_sealed(&keylog.keys[h], h, nonce, ct)
                });
                if ok.as_deref() != Some(b.as_slice()) {
                    return Err(StoreError::Io(format!(
                        "the re-encrypted copy of {} does not verify; {} is kept",
                        np.display(),
                        path.display()
                    )));
                }
            }
            out.records += records.len() as u64;
        }
        disk.remove(&path).map_err(io)?;
        disk.sync_dir(&blob_dir).map_err(io)?;
        stop(faults, Stop::Source(k))?;
    }
    Ok(())
}

/// Step 4: rebuilds every body-bearing projection without the stamp.
pub(crate) fn projections(
    dir: &Path,
    faults: &Faults,
    out: &mut Encryption,
) -> Result<(), StoreError> {
    let mut k = 0;
    for name in purge::FILES {
        let path = dir.join(name);
        let tmp = dir.join(format!("{name}{REBUILDING}"));
        if purge::stamped(&path).map_err(sql)? {
            let _ = std::fs::remove_file(&tmp);
            continue;
        }
        let _ = std::fs::remove_file(&tmp);
        {
            let old = rusqlite::Connection::open(&path).map_err(sql)?;
            // Everything in the WAL goes into the main file first, so the
            // copy is complete and the `-wal` can go.
            old.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .map_err(sql)?;
            old.execute("VACUUM INTO ?1", [tmp.display().to_string()])
                .map_err(sql)?;
        }
        {
            let new = rusqlite::Connection::open(&tmp).map_err(sql)?;
            new.execute_batch(purge::PRAGMAS).map_err(sql)?;
            purge::stamp(&new).map_err(sql)?;
        }
        std::fs::File::open(&tmp)
            .and_then(|f| f.sync_all())
            .map_err(io)?;
        for side in ["-wal", "-shm"] {
            match std::fs::remove_file(dir.join(format!("{name}{side}"))) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io(e)),
                _ => {}
            }
        }
        std::fs::rename(&tmp, &path).map_err(io)?;
        std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(io)?;
        out.projections.push(name.to_owned());
        stop(faults, Stop::Projection(k))?;
        k += 1;
    }
    Ok(())
}

/// Steps 1 to 7 on a store holding the lock. `swap`: a log swap was found
/// in progress.
pub(crate) fn run(
    disk: &dyn Durability,
    dir: &Path,
    modes: &dyn Modes,
    faults: &Faults,
    log_step: &mut dyn FnMut(&Faults) -> Result<crate::migrate::MigrationReport, StoreError>,
) -> Result<crate::migrate::MigrationReport, StoreError> {
    if !disk.exists(&dir.join(MARKER)) {
        let swap = migration_incomplete(disk, dir).map_err(io)?;
        begin(disk, dir, swap)?;
    }
    stop(faults, Stop::After(1))?;
    let mut report = log_step(faults)?;
    stop(faults, Stop::After(2))?;
    let mut enc = Encryption::default();
    packs(disk, dir, faults, &mut enc)?;
    stop(faults, Stop::After(3))?;
    projections(dir, faults, &mut enc)?;
    stop(faults, Stop::After(4))?;
    modes.apply(dir).map_err(StoreError::Io)?;
    stop(faults, Stop::After(5))?;
    let (failed, missing) = verify(disk, dir, modes)?;
    if let Some(step) = failed {
        return Err(StoreError::MigrationIncomplete { step });
    }
    enc.missing = missing;
    stop(faults, Stop::After(6))?;
    disk.remove(&dir.join(MARKER)).map_err(io)?;
    disk.sync_dir(dir).map_err(io)?;
    report.encryption = Some(enc);
    Ok(report)
}
