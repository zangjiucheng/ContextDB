//! Durable store (ADR 0006 §3–§5): idempotency first, apply, append + sync,
//! then publish. Any durability failure poisons the store until a fresh open
//! by a new disk instance (a new process in production).
use crate::{
    durability::{Durability, LockGuard},
    log::{self, FrameFault, LogRow},
    pack::{self, PackIndex},
    projection::{EpochRows, ProjectionFile, Projections},
};
use contextdb_core::Db;
use contextdb_model::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

const SELF_CHECK_SAMPLE: usize = 1_000;
/// Disk instances that hit a durability failure in this process (ADR 0006 §3 step 5).
static POISONED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    Tx(TxError),
    Poisoned,
    RestartRequired,
    Locked,
    FormatTooNew(u32),
    /// An older durable format: run the explicit migration (`ctx migrate`).
    FormatMigrationRequired(u32),
    FormatUnsupported(u32),
    ChainBroken {
        epoch: Epoch,
        fault: FrameFault,
    },
    /// `open --strict-tail` refuses to drop an unverifiable final frame.
    TailUnverifiable {
        fault: FrameFault,
    },
    ReplayDivergence {
        epoch: Epoch,
    },
    /// Recovered counting state disagrees with the inputs (ADR 0006 §4 step 7).
    SelfCheckFailed {
        revision: RevisionId,
    },
    /// `log/` is missing while a migration's backup or staging log exists:
    /// finish it with `ctx migrate` (review finding 3).
    MigrationIncomplete,
    /// No store at this path and `OpenOptions::create` is false.
    NotAStore,
    /// The log changed between `plan_truncate_before` and `truncate_before`.
    PlanOutdated,
    Io(String),
}
impl From<TxError> for StoreError {
    fn from(e: TxError) -> Self {
        Self::Tx(e)
    }
}
fn io(e: std::io::Error) -> StoreError {
    StoreError::Io(e.to_string())
}
fn sql(e: rusqlite::Error) -> StoreError {
    StoreError::Io(format!("projection: {e}"))
}
/// The store directory's exclusive lock, shared by the store and offline tools
/// (doctor, migrate) so none of them runs against an open store (ADR 0006 §3.0).
pub(crate) fn lock(disk: &dyn Durability, dir: &Path) -> Result<Box<dyn LockGuard>, StoreError> {
    disk.lock(&dir.join("LOCK")).map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            StoreError::Locked
        } else {
            io(e)
        }
    })
}
/// True when a migration left `log/` absent but its backup or staging log present.
pub(crate) fn migration_incomplete(disk: &dyn Durability, dir: &Path) -> std::io::Result<bool> {
    if disk.exists(&dir.join("log")) || !disk.exists(dir) {
        return Ok(false);
    }
    Ok(disk
        .list(dir)?
        .iter()
        .any(|n| n == "log.migrating" || (n.starts_with("log.v") && n.ends_with(".bak"))))
}
/// Net watermark advances of a committed batch (ADR 0008 §6.3): each
/// `ObserverWatermark` is compare-and-set on `expected`, so a scope advanced
/// iff the first op's `expected` differs from the committed position.
fn watermark_moves(db: &Db, batch: &Batch) -> Vec<(String, Vec<u8>)> {
    let mut first: BTreeMap<&str, &Option<Vec<u8>>> = BTreeMap::new();
    for op in &batch.ops {
        if let Op::ObserverWatermark {
            scope, expected, ..
        } = op
        {
            first.entry(scope.as_str()).or_insert(expected);
        }
    }
    first
        .into_iter()
        .filter_map(|(scope, expected)| {
            let now = db.watermark(scope)?;
            (expected.as_deref() != Some(now)).then(|| (scope.to_owned(), now.to_vec()))
        })
        .collect()
}
/// Projection rows for the epoch just committed to `db`.
fn rows<'a>(
    db: &Db,
    batch: &Batch,
    result: &'a CommitResult,
    checksum: ContentHash,
    base: &'a mut Vec<(RevisionId, bool)>,
) -> EpochRows<'a> {
    base.clear();
    base.extend(
        result
            .net_delta
            .base_flips
            .iter()
            .map(|r| (*r, db.is_base(*r).expect("committed revision"))),
    );
    EpochRows {
        epoch: result.epoch,
        checksum,
        result,
        base,
        watermarks: watermark_moves(db, batch),
        posts: result
            .status_diff
            .iter()
            .map(|r| (*r, db.node(*r).expect("committed revision")))
            .collect(),
    }
}
#[derive(Clone, Debug)]
pub struct OpenOptions {
    pub strict_tail: bool,
    /// Projection tables; production keeps them in a file next to the log.
    pub projections: ProjectionFile,
    /// Create an empty store when `<dir>/log` is absent. Operator tools pass
    /// false so a mistyped path fails instead of creating a store.
    pub create: bool,
    /// Discard projection tables at open and rebuild them (`ctx doctor --rebuild`).
    pub rebuild_projections: bool,
    /// Segment rollover size; tests use small segments to exercise rollover.
    pub segment_target: usize,
}
impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            strict_tail: false,
            projections: ProjectionFile::Memory,
            create: true,
            rebuild_projections: false,
            segment_target: log::SEGMENT_TARGET,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruncatedTail {
    pub after_epoch: Epoch,
    pub bytes: u64,
    pub reason: FrameFault,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenReport {
    pub truncated_tail: Option<TruncatedTail>,
    pub removed_segment: Option<PathBuf>,
    pub replayed: u64,
    pub removed_temp_blobs: usize,
    /// Revisions checked by the open-time self-check.
    pub self_checked: usize,
    pub snapshot_epoch: Option<Epoch>,
    /// Projection watermark found at open (rows above it were rolled forward).
    pub projection_watermark: Epoch,
    /// Snapshot files ignored as incomplete, corrupt or not matching the log.
    pub rejected_snapshots: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxnStatus {
    Committed {
        epoch: Epoch,
        result: Box<CommitResult>,
    },
    NotCommitted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    Missing,
    Corrupt,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    Available(Vec<u8>),
    Unavailable(Unavailable),
}
/// Segment paths and frame locations, shared with `LogReader`s.
#[derive(Debug, Default)]
pub(crate) struct LogIndex {
    segments: Vec<PathBuf>,
    frames: Vec<FrameLoc>,
}
#[derive(Clone, Debug)]
struct FrameLoc {
    segment: usize,
    start: u64,
    end: u64,
}
struct Tail {
    segment: usize,
    size: u64,
    checksum: ContentHash,
}
pub struct Store<D: Durability> {
    disk: D,
    dir: PathBuf,
    db: Db,
    index: Arc<RwLock<LogIndex>>,
    ids: BTreeMap<[u8; 16], (Epoch, ContentHash)>,
    blobs: BTreeSet<ContentHash>,
    packs: Arc<RwLock<PackIndex>>,
    /// Active pack segment (created by this process) and its size.
    pack_tail: Option<(usize, u64)>,
    tail: Option<Tail>,
    proj: Projections,
    /// Set across apply/append; still true after a panic, which poisons.
    in_flight: bool,
    poisoned: bool,
    /// G4 fault injection: the next commit stops after its frame sync.
    crash_after_sync: bool,
    segment_target: usize,
    /// Sequence number of the last group written (ADR 0008 §3.2).
    group_seq: u64,
    _lock: Box<dyn LockGuard>,
}
/// Poison registry key: one disk instance and one store directory, so a failure
/// in one store does not block others in the same process (review finding 9).
fn poison_key(disk: &dyn Durability, dir: &Path) -> String {
    format!("{}:{}", disk.instance(), dir.display())
}
fn hex(h: &ContentHash) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(s: &str) -> Option<ContentHash> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0; 32];
    for (n, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * n..2 * n + 2)?, 16).ok()?;
    }
    Some(out)
}
/// Every collection in a Batch must fit its u32 Wire length (ADR 0006 §2).
fn encodable(b: &Batch) -> bool {
    let fits = |n: usize| u32::try_from(n).is_ok();
    fits(b.ops.len())
        && b.ops.iter().all(|op| match op {
            Op::CreateLogical {
                key, scope_lane, ..
            } => fits(key.len()) && fits(scope_lane.len()),
            Op::AddRule { incidences, .. } => fits(incidences.len()),
            Op::RecordValidation {
                curvec_seen,
                validator,
                ..
            } => fits(curvec_seen.len()) && fits(validator.len()),
            Op::ObserverWatermark {
                scope,
                expected,
                position,
            } => {
                fits(scope.len())
                    && fits(position.len())
                    && expected.as_ref().is_none_or(|e| fits(e.len()))
            }
            Op::SetMetadata { tags, dirs, .. } => {
                fits(tags.len())
                    && fits(dirs.len())
                    && tags.iter().chain(dirs).all(|s| fits(s.len()))
            }
            Op::SetPolicy { policy } => fits(policy.require_deps.len()),
            _ => true,
        })
}
impl<D: Durability> Store<D> {
    fn log_dir(&self) -> PathBuf {
        self.dir.join("log")
    }
    fn blob_dir(&self) -> PathBuf {
        self.dir.join("blobs")
    }
    pub fn open(
        disk: D,
        dir: &Path,
        options: &OpenOptions,
    ) -> Result<(Self, OpenReport), StoreError> {
        if POISONED
            .lock()
            .expect("poison registry")
            .contains(&poison_key(&disk, dir))
        {
            return Err(StoreError::RestartRequired);
        }
        let (log_dir, blob_dir) = (dir.join("log"), dir.join("blobs"));
        // Never create `log/` over an interrupted migration: the acked history
        // is in the backup or staging log (review finding 3).
        if migration_incomplete(&disk, dir).map_err(io)? {
            return Err(StoreError::MigrationIncomplete);
        }
        if !options.create && !disk.exists(&log_dir) {
            return Err(StoreError::NotAStore);
        }
        // Lock before creating `log/`, so the re-check below sees exactly
        // what a concurrent `migrate` left (review N2).
        disk.create_dir_all(dir).map_err(io)?;
        let lock = lock(&disk, dir)?;
        if migration_incomplete(&disk, dir).map_err(io)? {
            return Err(StoreError::MigrationIncomplete);
        }
        for d in [&log_dir, &blob_dir] {
            disk.create_dir_all(d).map_err(io)?;
        }
        disk.sync_dir(dir).map_err(io)?;
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            disk.sync_dir(parent).map_err(io)?;
        }
        let mut report = OpenReport::default();
        // Blob presence: durable names only, leftovers removed (ADR 0006 §5);
        // packed payloads are present when their record verifies on a
        // segment that is durable. A killed process can leave verifying
        // records only in the page cache, so every segment is fully synced
        // before it is trusted (ADR 0008 §17, P3 review finding 1).
        disk.sync_dir(&blob_dir).map_err(io)?;
        let mut blobs = BTreeSet::new();
        let names = disk.list(&blob_dir).map_err(io)?;
        for name in names.iter().filter(|n| pack::is_pack(n)) {
            disk.sync(&blob_dir.join(name)).map_err(io)?;
        }
        let packs = pack::scan(&disk, &blob_dir, &names).map_err(io)?;
        blobs.extend(packs.map.keys().copied());
        for name in names {
            match unhex(&name) {
                Some(h) => {
                    blobs.insert(h);
                }
                None if pack::is_pack(&name) => {}
                None => {
                    disk.remove(&blob_dir.join(&name)).map_err(io)?;
                    report.removed_temp_blobs += 1;
                }
            }
        }
        if report.removed_temp_blobs > 0 {
            disk.sync_dir(&blob_dir).map_err(io)?;
        }
        let scan = log::scan(&disk, &log_dir).map_err(io)?;
        if let Some(format) = scan.newer_format {
            return Err(StoreError::FormatTooNew(format));
        }
        if let Some(format) = scan.older_format {
            return Err(StoreError::FormatMigrationRequired(format));
        }
        if let Some((epoch, fault)) = scan.broken {
            return Err(StoreError::ChainBroken { epoch, fault });
        }
        if let Some(seg) = &scan.empty_tail_segment {
            if options.strict_tail {
                return Err(StoreError::TailUnverifiable {
                    fault: FrameFault::SegmentHeader,
                });
            }
            disk.remove(seg).map_err(io)?;
            disk.sync_dir(&log_dir).map_err(io)?;
            report.removed_segment = Some(seg.clone());
        }
        if let Some((seg, keep, bytes, fault)) = scan.torn_tail.clone() {
            if options.strict_tail {
                return Err(StoreError::TailUnverifiable { fault });
            }
            disk.truncate(&seg, keep).map_err(io)?;
            disk.sync(&seg).map_err(io)?;
            report.truncated_tail = Some(TruncatedTail {
                after_epoch: scan.frames.last().map_or(Epoch(0), |f| f.row.epoch),
                bytes,
                reason: fault,
            });
        }
        // Frames a killed process left only in the page cache are verified
        // above; sync the tail segment so the epochs this open serves (and
        // `txn_status` reports) are durable. Earlier segments were synced
        // before rollover (P3 review note).
        if let Some(last) = log::segments(&disk, &log_dir).map_err(io)?.last() {
            disk.sync(last).map_err(io)?;
        }
        let checksums: Vec<ContentHash> = scan.frames.iter().map(|f| f.checksum).collect();
        let mut proj = Projections::open(&options.projections, &checksums).map_err(sql)?;
        if options.rebuild_projections {
            proj.clear().map_err(sql)?;
        }
        report.projection_watermark = proj.watermark();
        let mut store = Self {
            dir: dir.to_path_buf(),
            db: Db::default(),
            index: Arc::new(RwLock::new(LogIndex {
                segments: log::segments(&disk, &log_dir).map_err(io)?,
                frames: Vec::new(),
            })),
            ids: BTreeMap::new(),
            blobs,
            packs: Arc::new(RwLock::new(packs)),
            pack_tail: None,
            tail: None,
            proj,
            in_flight: false,
            poisoned: false,
            crash_after_sync: false,
            segment_target: options.segment_target,
            group_seq: 0,
            _lock: lock,
            disk,
        };
        // Newest valid snapshot at or below the tail (ADR 0006 §4 step 4, §6);
        // one the core cannot rebuild is ignored, and replay starts earlier.
        let (base, rejected) =
            crate::snapshot::load_newest(&store.disk, dir, &checksums, |(epoch, inputs)| {
                Db::from_inputs(inputs, epoch).ok()
            })
            .map_err(io)?;
        report.rejected_snapshots = rejected;
        if let Some((epoch, db)) = base {
            // Projections behind the snapshot stay behind (reads fall back to
            // the log) until `catch_up_projections`; open stays proportional
            // to the tail after the snapshot.
            store.db = db;
            report.snapshot_epoch = Some(epoch);
        }
        let skip = report.snapshot_epoch.map_or(0, |e| e.0);
        store.proj.begin_bulk().map_err(sql)?;
        // Replay without idempotency, blob checks or appends (ADR 0006 §4 step 5).
        for f in scan.frames {
            let segment = store
                .index
                .read()
                .expect("log index")
                .segments
                .iter()
                .position(|s| *s == f.segment)
                .expect("scanned segment");
            if f.row.epoch.0 > skip {
                let batch = f.row.batch().expect("verified frame");
                let result = store
                    .db
                    .commit(&batch)
                    .map_err(|_| StoreError::ReplayDivergence { epoch: f.row.epoch })?;
                if result != f.row.result().expect("verified frame") {
                    return Err(StoreError::ReplayDivergence { epoch: f.row.epoch });
                }
                if f.row.epoch > store.proj.watermark() {
                    let mut base = Vec::new();
                    store
                        .proj
                        .apply(&rows(&store.db, &batch, &result, f.checksum, &mut base))
                        .map_err(sql)?;
                }
                report.replayed += 1;
            }
            if let Some(id) = f.row.client_txn_id {
                store.ids.insert(id, (f.row.epoch, f.row.batch_hash));
            }
            store
                .index
                .write()
                .expect("log index")
                .frames
                .push(FrameLoc {
                    segment,
                    start: f.start,
                    end: f.end,
                });
            store.group_seq = f.row.group.seq;
            store.tail = Some(Tail {
                segment,
                size: f.end,
                checksum: f.checksum,
            });
        }
        // Sampled self-check (ADR 0006 §4 step 7), seeded by the head checksum.
        let n = store.db.revision_count();
        if n > 0 {
            let seed = store.tail.as_ref().map_or(log::GENESIS, |t| t.checksum);
            let mut x = u64::from_le_bytes(seed[..8].try_into().expect("eight bytes")) | 1;
            let sample: Vec<RevisionId> = if n <= SELF_CHECK_SAMPLE {
                (0..n).map(|r| RevisionId(r as u32)).collect()
            } else {
                (0..SELF_CHECK_SAMPLE)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        RevisionId((x % n as u64) as u32)
                    })
                    .collect()
            };
            if let Err(r) = store.db.verify_counting_sample(&sample) {
                return Err(StoreError::SelfCheckFailed { revision: r });
            }
            report.self_checked = sample.len();
        }
        // Projection rows rolled forward are committed only after the
        // self-check accepted the state they came from (review N5).
        store.proj.end_bulk().map_err(sql)?;
        // A last segment with a valid header but no frames continues the chain.
        let last_segment = {
            let index = store.index.read().expect("log index");
            index
                .segments
                .len()
                .checked_sub(1)
                .map(|n| (n, index.segments[n].clone()))
        };
        if store.tail.is_none()
            && let Some((n, path)) = last_segment
        {
            let bytes = store.disk.read(&path).map_err(io)?;
            let header = log::parse_header(&bytes).expect("scanned header");
            store.tail = Some(Tail {
                segment: n,
                size: bytes.len() as u64,
                checksum: header.prev,
            });
        }
        Ok((store, report))
    }
    /// The live database. Unavailable once poisoned: it may hold an epoch
    /// that never became durable (review finding 5).
    pub fn db(&self) -> Result<&Db, StoreError> {
        self.readable()?;
        Ok(&self.db)
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    /// Poisons the store from outside (the daemon's replica cross-check,
    /// ADR 0008 §3.3): nothing more is served or committed.
    pub fn fail_stop(&mut self) -> StoreError {
        self.poison()
    }
    fn poison(&mut self) -> StoreError {
        self.poisoned = true;
        POISONED
            .lock()
            .expect("poison registry")
            .insert(poison_key(&self.disk, &self.dir));
        StoreError::Poisoned
    }
    fn readable(&self) -> Result<(), StoreError> {
        if self.poisoned || self.in_flight {
            return Err(StoreError::Poisoned);
        }
        Ok(())
    }
    fn usable(&mut self) -> Result<(), StoreError> {
        if self.in_flight {
            // A panic interrupted apply or append (ADR 0006 §3 step 5).
            self.in_flight = false;
            return Err(self.poison());
        }
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        Ok(())
    }
    /// Poisoned stores refuse: the answer is only known after a restart.
    pub fn txn_status(&self, id: &[u8; 16]) -> Result<TxnStatus, StoreError> {
        self.readable()?;
        match self.ids.get(id) {
            Some((epoch, _)) => Ok(TxnStatus::Committed {
                epoch: *epoch,
                result: Box::new(
                    self.read_row(*epoch)?
                        .acked()
                        .map_err(|e| StoreError::Io(e.to_string()))?,
                ),
            }),
            None => Ok(TxnStatus::NotCommitted),
        }
    }
    /// Reads and re-verifies one durable frame.
    pub fn read_row(&self, epoch: Epoch) -> Result<LogRow, StoreError> {
        self.readable()?;
        self.read_frame(epoch)
    }
    /// A durable frame, without the poison check (used inside a group commit,
    /// which only reads frames that were durable before the group began).
    fn read_frame(&self, epoch: Epoch) -> Result<LogRow, StoreError> {
        read_frame(&self.disk, &self.index, epoch)
    }
    /// A read-only view of the log up to the current durable tail, for reader
    /// threads (ADR 0008 §3.3): log-replay reads never block the commit actor.
    pub fn log_reader(&self) -> LogReader<D>
    where
        D: Clone,
    {
        LogReader {
            disk: self.disk.clone(),
            dir: self.dir.clone(),
            index: self.index.clone(),
            packs: self.packs.clone(),
            tail: self.epoch(),
        }
    }
    pub fn commit(&mut self, batch: &Batch) -> Result<CommitResult, StoreError> {
        self.commit_group(std::slice::from_ref(batch))
            .pop()
            .expect("one result per batch")
    }
    /// Group commit (ADR 0008 §3.2): every batch gets its own epoch and frame,
    /// the group shares one sync, and no result is returned before it.
    /// Idempotency covers ids staged earlier in the same group. If anything
    /// fails after the first apply, the store is poisoned and every batch of
    /// the group gets `Poisoned` (the daemon reports `OutcomeUnknown`).
    pub fn commit_group(&mut self, batches: &[Batch]) -> Vec<Result<CommitResult, StoreError>> {
        if let Err(e) = self.usable() {
            return batches.iter().map(|_| Err(e.clone())).collect();
        }
        enum Slot {
            Done(Box<Result<CommitResult, StoreError>>),
            Applied(usize),
            Duplicate(usize),
        }
        let mut slots = Vec::with_capacity(batches.len());
        let mut staged_ids: BTreeMap<[u8; 16], (usize, ContentHash)> = BTreeMap::new();
        let mut applied: Vec<(LogRow, CommitResult)> = Vec::new();
        let mut applied_batch: Vec<usize> = Vec::new();
        let prev = self.tail.as_ref().map_or(log::GENESIS, |t| t.checksum);
        self.in_flight = true;
        for (n, batch) in batches.iter().enumerate() {
            if !encodable(batch) {
                slots.push(Slot::Done(Box::new(Err(StoreError::Tx(
                    TxError::CapacityExceeded {
                        kind: CapacityKind::Encoding,
                    },
                )))));
                continue;
            }
            let batch_blob = batch.canonical_bytes();
            let batch_hash = *blake3::hash(&batch_blob).as_bytes();
            // Step 1: idempotency, durable ids first, then ids staged in this group.
            if let Some(id) = batch.client_txn_id {
                if let Some((epoch, hash)) = self.ids.get(&id).copied() {
                    slots.push(Slot::Done(Box::new(if hash != batch_hash {
                        Err(StoreError::Tx(TxError::TxnIdReuse { id }))
                    } else {
                        self.read_frame(epoch)
                            .and_then(|r| r.acked().map_err(|e| StoreError::Io(e.to_string())))
                    })));
                    continue;
                }
                if let Some((k, hash)) = staged_ids.get(&id).copied() {
                    slots.push(if hash != batch_hash {
                        Slot::Done(Box::new(Err(StoreError::Tx(TxError::TxnIdReuse { id }))))
                    } else {
                        Slot::Duplicate(k)
                    });
                    continue;
                }
            }
            // Step 2: apply with blob presence; a rejection leaves no trace.
            match self.db.commit_with(batch, &self.blobs) {
                Err(e) => slots.push(Slot::Done(Box::new(Err(StoreError::Tx(e))))),
                Ok(result) => {
                    let row = LogRow {
                        epoch: result.epoch,
                        branch: batch.branch,
                        principal: batch.principal,
                        channel: batch.channel,
                        client_txn_id: batch.client_txn_id,
                        batch_hash,
                        batch_blob,
                        result_blob: result.canonical_bytes(),
                        acked_result: None,
                        prev_checksum: prev,
                        group: log::GroupPos::default(),
                    };
                    if let Some(id) = batch.client_txn_id {
                        staged_ids.insert(id, (applied.len(), batch_hash));
                    }
                    slots.push(Slot::Applied(applied.len()));
                    applied_batch.push(n);
                    applied.push((row, result));
                }
            }
        }
        if !applied.is_empty() {
            // Step 3: frame the group, append, sync once.
            let count = u16::try_from(applied.len()).expect("group size bounded");
            let seq = self.group_seq + 1;
            let mut prev = self.tail.as_ref().map_or(log::GENESIS, |t| t.checksum);
            let mut bytes = Vec::new();
            let mut sums = Vec::with_capacity(applied.len());
            let mut ends = Vec::with_capacity(applied.len());
            for (idx, (row, _)) in applied.iter_mut().enumerate() {
                row.prev_checksum = prev;
                row.group = log::GroupPos {
                    seq,
                    idx: idx as u16,
                    count,
                };
                let (frame, sum) = log::frame(row);
                bytes.extend_from_slice(&frame);
                ends.push(bytes.len() as u64);
                sums.push(sum);
                prev = sum;
            }
            let first_epoch = applied[0].1.epoch;
            let first_prev = applied[0].0.prev_checksum;
            let (segment, start) = match self.append(first_epoch, &first_prev, &bytes) {
                Ok(v) => v,
                Err(_) => {
                    self.in_flight = false;
                    let e = self.poison();
                    return batches.iter().map(|_| Err(e.clone())).collect();
                }
            };
            if std::mem::take(&mut self.crash_after_sync) {
                // Durable but never published or acked: the process "dies" here.
                self.in_flight = false;
                let e = self.poison();
                return batches.iter().map(|_| Err(e.clone())).collect();
            }
            // Step 4: publish.
            self.group_seq = seq;
            let mut from = start;
            for (k, (row, result)) in applied.iter().enumerate() {
                let end = start + ends[k];
                self.index
                    .write()
                    .expect("log index")
                    .frames
                    .push(FrameLoc {
                        segment,
                        start: from,
                        end,
                    });
                from = end;
                if let Some(id) = row.client_txn_id {
                    self.ids.insert(id, (result.epoch, row.batch_hash));
                }
                // Projections after durability; a failure only makes them stale.
                let mut base = Vec::new();
                if self
                    .proj
                    .apply(&rows(
                        &self.db,
                        &batches[applied_batch[k]],
                        result,
                        sums[k],
                        &mut base,
                    ))
                    .is_err()
                {
                    self.proj.mark_stale();
                }
            }
            self.tail = Some(Tail {
                segment,
                size: start + bytes.len() as u64,
                checksum: *sums.last().expect("non-empty group"),
            });
        }
        self.in_flight = false;
        slots
            .into_iter()
            .map(|slot| match slot {
                Slot::Done(r) => *r,
                Slot::Applied(k) | Slot::Duplicate(k) => Ok(applied[k].1.clone()),
            })
            .collect()
    }
    /// Appends a synced group, opening a synced segment first when needed.
    /// Rollover happens only at group boundaries.
    fn append(
        &mut self,
        epoch: Epoch,
        prev: &ContentHash,
        bytes: &[u8],
    ) -> std::io::Result<(usize, u64)> {
        let log_dir = self.log_dir();
        let rollover = self
            .tail
            .as_ref()
            .is_none_or(|t| t.size as usize >= self.segment_target);
        let (segment, start) = if rollover {
            let path = log_dir.join(log::segment_name(epoch));
            self.disk
                .write_new(&path, &log::segment_header(epoch, *prev))?;
            self.disk.sync(&path)?;
            self.disk.sync_dir(&log_dir)?;
            let mut index = self.index.write().expect("log index");
            index.segments.push(path);
            (index.segments.len() - 1, log::HEADER_LEN as u64)
        } else {
            let t = self.tail.as_ref().expect("tail");
            (t.segment, t.size)
        };
        let path = self.index.read().expect("log index").segments[segment].clone();
        self.disk.append(&path, bytes)?;
        self.disk.sync(&path)?;
        Ok((segment, start))
    }
    /// Stores a payload; returns only after it is durable (ADR 0006 §5).
    pub fn put_blob(&mut self, bytes: &[u8]) -> Result<ContentHash, StoreError> {
        self.put_blobs(&[bytes])?;
        Ok(*blake3::hash(bytes).as_bytes())
    }
    /// Stores payloads; returns only after all are durable.
    pub fn put_blobs(&mut self, blobs: &[&[u8]]) -> Result<(), StoreError> {
        self.stage_blobs(blobs)?;
        let synced = match self.pack_tail {
            Some((segment, _)) => {
                let path = self.packs.read().expect("pack index").segments[segment].clone();
                self.disk.sync(&path)
            }
            None => self.disk.sync_dir(&self.blob_dir()),
        };
        if synced.is_err() {
            return Err(self.poison());
        }
        Ok(())
    }
    /// Stores payloads ordered before every later write, with no drive-cache
    /// flush: the group's new payloads are appended to the active pack with
    /// one ordered sync (plus an ordered `sync_dir` for a new segment). They
    /// become durable with the next full sync, which for the daemon is the
    /// op_log sync of the group that references them, and a frame can never
    /// reach media ahead of them (ADR 0008 §17).
    pub fn stage_blobs(&mut self, blobs: &[&[u8]]) -> Result<(), StoreError> {
        self.usable()?;
        let mut fresh: Vec<(ContentHash, Vec<u8>)> = Vec::new();
        for bytes in blobs {
            let hash = *blake3::hash(bytes).as_bytes();
            let present = fresh.iter().any(|(h, _)| *h == hash)
                || self.blobs.contains(&hash)
                    && matches!(self.get_blob(&hash), Ok(Payload::Available(_)));
            if !present {
                let (_, rec) = pack::record(bytes)
                    .ok_or_else(|| StoreError::Io("payload too large".into()))?;
                fresh.push((hash, rec));
            }
        }
        if fresh.is_empty() {
            return Ok(());
        }
        let dir = self.blob_dir();
        let size: u64 = fresh.iter().map(|(_, r)| r.len() as u64).sum();
        let (segment, start, created) = match self.pack_tail {
            Some((segment, at)) if at == 0 || at + size <= self.segment_target as u64 => {
                (segment, at, false)
            }
            _ => {
                let mut packs = self.packs.write().expect("pack index");
                let next = packs
                    .segments
                    .iter()
                    .filter_map(|p| p.file_name()?.to_str()?.get(5..)?.parse::<usize>().ok())
                    .max()
                    .map_or(1, |n| n + 1);
                packs.segments.push(dir.join(pack::segment_name(next)));
                (packs.segments.len() - 1, 0, true)
            }
        };
        let path = self.packs.read().expect("pack index").segments[segment].clone();
        let bytes: Vec<u8> = fresh.iter().flat_map(|(_, r)| r.iter().copied()).collect();
        let written = self
            .disk
            .append(&path, &bytes)
            .and_then(|()| self.disk.sync_weak(&path))
            .and_then(|()| {
                if created {
                    self.disk.sync_dir_weak(&dir)
                } else {
                    Ok(())
                }
            });
        if written.is_err() {
            return Err(self.poison());
        }
        let mut packs = self.packs.write().expect("pack index");
        let mut at = start;
        for (hash, rec) in fresh {
            packs.insert(
                hash,
                segment,
                at + pack::HEADER as u64,
                (rec.len() - pack::HEADER) as u32,
            );
            at += rec.len() as u64;
            self.blobs.insert(hash);
        }
        self.pack_tail = Some((segment, at));
        Ok(())
    }
    /// Poisoned stores refuse: a blob whose put failed may be visible now
    /// and gone after restart (review N4).
    pub fn get_blob(&self, hash: &ContentHash) -> Result<Payload, StoreError> {
        self.readable()?;
        Ok(read_payload(&self.disk, &self.dir, &self.packs, hash))
    }
    /// The last durable epoch. After poisoning, the in-memory Db may be one
    /// epoch ahead; this never is.
    pub fn epoch(&self) -> Epoch {
        Epoch(self.index.read().expect("log index").frames.len() as u64)
    }
    /// Database as of `e`: the live one when it retains `e`, otherwise a replay
    /// of the verified log prefix (ADR 0006 §8, source 3).
    fn db_at(&self, e: Epoch) -> Result<std::borrow::Cow<'_, Db>, StoreError> {
        if e > self.epoch() {
            return Err(StoreError::Tx(TxError::UnknownRef));
        }
        if e >= self.db.initial_epoch() {
            return Ok(std::borrow::Cow::Borrowed(&self.db));
        }
        let mut db = Db::default();
        for n in 1..=e.0 {
            let row = self.read_row(Epoch(n))?;
            let batch = row.batch().map_err(|e| StoreError::Io(e.to_string()))?;
            db.commit(&batch)
                .map_err(|_| StoreError::ReplayDivergence { epoch: Epoch(n) })?;
        }
        Ok(std::borrow::Cow::Owned(db))
    }
    /// `ReadMode::Historical(e)` (SPEC §6.1): status as of `e` plus payload state now.
    pub fn read_historical(
        &self,
        r: RevisionId,
        e: Epoch,
    ) -> Result<(NodeState, Payload), StoreError> {
        self.readable()?;
        if e < self.db.initial_epoch() && self.proj.covers(e) {
            let state = self
                .proj
                .state_at(r, e)
                .map_err(sql)?
                .ok_or(StoreError::Tx(TxError::UnknownRef))?;
            return Ok((state, self.get_blob(&self.db.revision_content(r)?)?));
        }
        let db = self.db_at(e)?;
        let state = db.read_historical(r, e)?;
        let payload = self.get_blob(&db.revision_content(r)?)?;
        Ok((state, payload))
    }
    /// DIFF over any committed epochs: memory when retained, else log replay.
    pub fn diff(&self, a: Epoch, b: Epoch) -> Result<contextdb_core::EpochDiff, StoreError> {
        self.readable()?;
        let low = a.min(b);
        if low >= self.db.initial_epoch() {
            return Ok(self.db.diff(a, b)?);
        }
        let high = a.max(b);
        if high <= self.epoch() && self.proj.covers(high) {
            return self.proj.diff(a, b).map_err(sql);
        }
        let mut db = Db::default();
        for n in 1..=high.0 {
            let row = self.read_row(Epoch(n))?;
            let batch = row.batch().map_err(|e| StoreError::Io(e.to_string()))?;
            db.commit(&batch)
                .map_err(|_| StoreError::ReplayDivergence { epoch: Epoch(n) })?;
        }
        Ok(db.diff(a, b)?)
    }
    /// `ReadMode::Snapshot(e)` (SPEC §6.1): head as of `e` and its status then.
    pub fn read_snapshot(
        &self,
        l: LogicalId,
        e: Epoch,
    ) -> Result<(RevisionId, NodeState), StoreError> {
        self.readable()?;
        if e < self.db.initial_epoch() && self.proj.covers(e) {
            let r = self
                .proj
                .head_at(l, e)
                .map_err(sql)?
                .ok_or(StoreError::Tx(TxError::UnknownRef))?;
            let state = self
                .proj
                .state_at(r, e)
                .map_err(sql)?
                .ok_or(StoreError::Tx(TxError::UnknownRef))?;
            return Ok((r, state));
        }
        Ok(self.db_at(e)?.read_snapshot(l, e)?)
    }
    /// Fills projection rows between the watermark and the tail by replaying
    /// the log (doctor / background maintenance; ADR 0006 §8).
    pub fn catch_up_projections(&mut self) -> Result<Epoch, StoreError> {
        self.usable()?;
        let mark = self.proj.watermark();
        if mark >= self.epoch() {
            return Ok(mark);
        }
        // One bulk transaction; any failure rolls back to the old watermark (N6).
        self.proj.begin_bulk().map_err(sql)?;
        let filled = (|| {
            let mut db = Db::default();
            let mut base = Vec::new();
            for n in 1..=self.epoch().0 {
                let row = self.read_row(Epoch(n))?;
                let batch = row.batch().map_err(|e| StoreError::Io(e.to_string()))?;
                let result = db
                    .commit(&batch)
                    .map_err(|_| StoreError::ReplayDivergence { epoch: Epoch(n) })?;
                if Epoch(n) > mark {
                    let checksum = log::checksum(&row.encode_body());
                    self.proj
                        .apply(&rows(&db, &batch, &result, checksum, &mut base))
                        .map_err(sql)?;
                }
            }
            self.proj.end_bulk().map_err(sql)
        })();
        if let Err(e) = filled {
            self.proj.abort_bulk();
            return Err(e);
        }
        Ok(self.proj.watermark())
    }
    /// Whether `scope`'s watermark advanced in `(since, until]`, from the
    /// projection; `None` when projections do not cover `until` (ADR 0008 §6.3).
    pub fn watermark_advanced(
        &self,
        scope: &str,
        since: Epoch,
        until: Epoch,
    ) -> Result<Option<bool>, StoreError> {
        self.readable()?;
        if !self.proj.covers(until) {
            return Ok(None);
        }
        self.proj
            .watermark_advanced(scope, since, until)
            .map(Some)
            .map_err(sql)
    }
    pub fn projection_watermark(&self) -> Epoch {
        self.proj.watermark()
    }
    /// Replaces the live database with `replica`, which must be at the same
    /// epoch and state (ADR 0008 §3.3 two-replica publication); returns the
    /// previous database. Debug builds compare digests.
    pub fn replace_db(&mut self, replica: Db) -> Result<Db, StoreError> {
        self.readable()?;
        if replica.epoch() != self.db.epoch() {
            return Err(StoreError::Io(format!(
                "replica at {:?}, store at {:?}",
                replica.epoch(),
                self.db.epoch()
            )));
        }
        if cfg!(debug_assertions) {
            assert_eq!(replica.digest(), self.db.digest(), "replica diverged");
        }
        Ok(std::mem::replace(&mut self.db, replica))
    }
    /// Exact IMPACT of a hypothetical batch on the live database, restored
    /// afterwards (P1 `impact_exact`; ADR 0008 §3.3 runs it on the actor).
    pub fn impact_exact(&mut self, batch: &Batch) -> Result<CommitResult, StoreError> {
        self.readable()?;
        self.db.impact_exact(batch).map_err(StoreError::Tx)
    }
    /// The op_log frame checksum at `epoch` (genesis at 0): certificate log
    /// linkage (ADR 0008 §4).
    pub fn checksum_at(&self, epoch: Epoch) -> Result<ContentHash, StoreError> {
        if epoch.0 == 0 {
            return Ok(log::GENESIS);
        }
        Ok(log::checksum(&self.read_row(epoch)?.encode_body()))
    }
    /// G4 fault injection: the next commit syncs its frame, then fails before
    /// publishing or acking (post-sync-pre-publish), poisoning the store.
    #[doc(hidden)]
    pub fn crash_after_next_sync(&mut self) {
        self.crash_after_sync = true;
    }
    /// G4 fault injection: the next projection write fails mid-transaction.
    #[doc(hidden)]
    pub fn fail_next_projection_write(&mut self) {
        self.proj.fail_next_write();
    }
    /// Writes a snapshot of the current inputs (ADR 0006 §6). A failure leaves
    /// the log untouched; snapshots are only an accelerator, so it does not poison.
    pub fn snapshot(&mut self) -> Result<Epoch, StoreError> {
        self.usable()?;
        let epoch = self.epoch();
        let checksum = self.tail.as_ref().map_or(log::GENESIS, |t| t.checksum);
        crate::snapshot::write(&self.disk, &self.dir, epoch, &checksum, &self.db.inputs())
            .map_err(io)?;
        Ok(epoch)
    }
}

/// A payload from the packs, else from a legacy per-file blob (format ≤ 2
/// stores written before ADR 0008 §17).
fn read_payload(
    disk: &dyn Durability,
    dir: &Path,
    packs: &RwLock<PackIndex>,
    hash: &ContentHash,
) -> Payload {
    let located = packs.read().expect("pack index").locate(hash);
    let read = match located {
        Some((path, offset, len)) => disk.read_range(&path, offset, len),
        None => disk.read(&dir.join("blobs").join(hex(hash))),
    };
    match read {
        Ok(bytes) if *blake3::hash(&bytes).as_bytes() == *hash => Payload::Available(bytes),
        Ok(_) => Payload::Unavailable(Unavailable::Corrupt),
        Err(_) => Payload::Unavailable(Unavailable::Missing),
    }
}
/// Reads and re-verifies one durable frame through the shared index.
fn read_frame(
    disk: &dyn Durability,
    index: &RwLock<LogIndex>,
    epoch: Epoch,
) -> Result<LogRow, StoreError> {
    let (path, loc) = {
        let index = index.read().expect("log index");
        let loc = epoch
            .0
            .checked_sub(1)
            .and_then(|n| index.frames.get(n as usize))
            .cloned()
            .ok_or(StoreError::Tx(TxError::UnknownRef))?;
        (index.segments[loc.segment].clone(), loc)
    };
    // One frame, not the whole segment (review finding 6).
    let len =
        usize::try_from(loc.end - loc.start).map_err(|_| StoreError::Io("frame size".into()))?;
    let bytes = disk.read_range(&path, loc.start, len).map_err(io)?;
    let frame = &bytes[16..len - 32];
    if log::checksum(frame) != bytes[len - 32..] {
        return Err(StoreError::ChainBroken {
            epoch,
            fault: FrameFault::Checksum,
        });
    }
    LogRow::decode_body(frame).map_err(|e| StoreError::Io(e.to_string()))
}
/// Read-only log access bounded by the tail it was created at (ADR 0008 §3.3).
#[derive(Clone)]
pub struct LogReader<D: Durability> {
    disk: D,
    dir: PathBuf,
    index: Arc<RwLock<LogIndex>>,
    packs: Arc<RwLock<PackIndex>>,
    tail: Epoch,
}
impl<D: Durability> LogReader<D> {
    /// Reads a payload without the actor (ADR 0008 §3.3): payloads are
    /// immutable once staged, and content-addressed.
    pub fn read_blob(&self, hash: &ContentHash) -> Payload {
        read_payload(&self.disk, &self.dir, &self.packs, hash)
    }
    pub fn tail(&self) -> Epoch {
        self.tail
    }
    /// The op_log frame checksum at `epoch` (genesis at 0).
    pub fn checksum_at(&self, epoch: Epoch) -> Result<ContentHash, StoreError> {
        if epoch.0 == 0 {
            return Ok(log::GENESIS);
        }
        Ok(log::checksum(&self.read_row(epoch)?.encode_body()))
    }
    pub fn read_row(&self, epoch: Epoch) -> Result<LogRow, StoreError> {
        if epoch > self.tail {
            return Err(StoreError::Tx(TxError::UnknownRef));
        }
        read_frame(&self.disk, &self.index, epoch)
    }
    /// The database as of `e` by replaying the verified prefix (ADR 0006 §8 source 3).
    pub fn replay_to(&self, e: Epoch) -> Result<Db, StoreError> {
        if e > self.tail {
            return Err(StoreError::Tx(TxError::UnknownRef));
        }
        let mut db = Db::default();
        for n in 1..=e.0 {
            let row = self.read_row(Epoch(n))?;
            let batch = row.batch().map_err(|e| StoreError::Io(e.to_string()))?;
            db.commit(&batch)
                .map_err(|_| StoreError::ReplayDivergence { epoch: Epoch(n) })?;
        }
        Ok(db)
    }
}
