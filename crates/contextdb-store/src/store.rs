//! Durable store (ADR 0006 §3–§5): idempotency first, apply, append + sync,
//! then publish. Any durability failure poisons the store until a fresh open
//! by a new disk instance (a new process in production).
use crate::{
    durability::{Durability, LockGuard},
    log::{self, FrameFault, LogRow},
};
use contextdb_core::Db;
use contextdb_model::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Disk instances that hit a durability failure in this process (ADR 0006 §3 step 5).
static POISONED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    Tx(TxError),
    Poisoned,
    RestartRequired,
    Locked,
    FormatTooNew(u32),
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
#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    pub strict_tail: bool,
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
    segments: Vec<PathBuf>,
    frames: Vec<FrameLoc>,
    ids: BTreeMap<[u8; 16], (Epoch, ContentHash)>,
    blobs: BTreeSet<ContentHash>,
    tail: Option<Tail>,
    /// Set across apply/append; still true after a panic, which poisons.
    in_flight: bool,
    poisoned: bool,
    _lock: Box<dyn LockGuard>,
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
            .contains(&disk.instance())
        {
            return Err(StoreError::RestartRequired);
        }
        let (log_dir, blob_dir) = (dir.join("log"), dir.join("blobs"));
        for d in [&log_dir, &blob_dir] {
            disk.create_dir_all(d).map_err(io)?;
        }
        disk.sync_dir(dir).map_err(io)?;
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            disk.sync_dir(parent).map_err(io)?;
        }
        let lock = disk.lock(&dir.join("LOCK")).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                StoreError::Locked
            } else {
                io(e)
            }
        })?;
        let mut report = OpenReport::default();
        // Blob presence: durable names only, leftovers removed (ADR 0006 §5).
        disk.sync_dir(&blob_dir).map_err(io)?;
        let mut blobs = BTreeSet::new();
        for name in disk.list(&blob_dir).map_err(io)? {
            match unhex(&name) {
                Some(h) => {
                    blobs.insert(h);
                }
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
        if let Some((epoch, fault)) = scan.broken {
            return Err(StoreError::ChainBroken { epoch, fault });
        }
        if let Some(seg) = &scan.empty_tail_segment {
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
        let mut store = Self {
            dir: dir.to_path_buf(),
            db: Db::default(),
            segments: log::segments(&disk, &log_dir).map_err(io)?,
            frames: Vec::new(),
            ids: BTreeMap::new(),
            blobs,
            tail: None,
            in_flight: false,
            poisoned: false,
            _lock: lock,
            disk,
        };
        // Replay without idempotency, blob checks or appends (ADR 0006 §4 step 5).
        let mut start = log::HEADER_LEN as u64;
        let mut current = None;
        for f in scan.frames {
            let segment = store
                .segments
                .iter()
                .position(|s| *s == f.segment)
                .expect("scanned segment");
            if current != Some(segment) {
                current = Some(segment);
                start = log::HEADER_LEN as u64;
            }
            let batch = f.row.batch().expect("verified frame");
            let result = store
                .db
                .commit(&batch)
                .map_err(|_| StoreError::ReplayDivergence { epoch: f.row.epoch })?;
            if result != f.row.result().expect("verified frame") {
                return Err(StoreError::ReplayDivergence { epoch: f.row.epoch });
            }
            if let Some(id) = f.row.client_txn_id {
                store.ids.insert(id, (f.row.epoch, f.row.batch_hash));
            }
            store.frames.push(FrameLoc {
                segment,
                start,
                end: f.end,
            });
            start = f.end;
            store.tail = Some(Tail {
                segment,
                size: f.end,
                checksum: f.checksum,
            });
            report.replayed += 1;
        }
        // A last segment with a valid header but no frames continues the chain.
        if store.tail.is_none() && !store.segments.is_empty() {
            let n = store.segments.len() - 1;
            let bytes = store.disk.read(&store.segments[n]).map_err(io)?;
            let header = log::parse_header(&bytes).expect("scanned header");
            store.tail = Some(Tail {
                segment: n,
                size: bytes.len() as u64,
                checksum: header.prev,
            });
        }
        Ok((store, report))
    }
    pub fn db(&self) -> &Db {
        &self.db
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    fn poison(&mut self) -> StoreError {
        self.poisoned = true;
        POISONED
            .lock()
            .expect("poison registry")
            .insert(self.disk.instance());
        StoreError::Poisoned
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
    pub fn txn_status(&self, id: &[u8; 16]) -> Result<TxnStatus, StoreError> {
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
        let loc = epoch
            .0
            .checked_sub(1)
            .and_then(|n| self.frames.get(n as usize))
            .ok_or(StoreError::Tx(TxError::UnknownRef))?;
        let bytes = self.disk.read(&self.segments[loc.segment]).map_err(io)?;
        let frame = &bytes[loc.start as usize + 16..loc.end as usize - 32];
        if log::checksum(frame) != bytes[loc.end as usize - 32..loc.end as usize] {
            return Err(StoreError::ChainBroken {
                epoch,
                fault: FrameFault::Checksum,
            });
        }
        LogRow::decode_body(frame).map_err(|e| StoreError::Io(e.to_string()))
    }
    pub fn commit(&mut self, batch: &Batch) -> Result<CommitResult, StoreError> {
        self.usable()?;
        let batch_blob = if encodable(batch) {
            batch.canonical_bytes()
        } else {
            return Err(StoreError::Tx(TxError::CapacityExceeded {
                kind: CapacityKind::Encoding,
            }));
        };
        let batch_hash = *blake3::hash(&batch_blob).as_bytes();
        // Step 1: idempotency precedes every other check (ADR 0006 §3).
        if let Some(id) = batch.client_txn_id
            && let Some((epoch, hash)) = self.ids.get(&id).copied()
        {
            if hash != batch_hash {
                return Err(StoreError::Tx(TxError::TxnIdReuse { id }));
            }
            return self
                .read_row(epoch)?
                .acked()
                .map_err(|e| StoreError::Io(e.to_string()));
        }
        self.in_flight = true;
        // Step 2: apply with blob presence; a rejection leaves no trace.
        let result = match self.db.commit_with(batch, &self.blobs) {
            Ok(r) => r,
            Err(e) => {
                self.in_flight = false;
                return Err(StoreError::Tx(e));
            }
        };
        // Step 3: append one frame and sync before anything is published.
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
            prev_checksum: self.tail.as_ref().map_or(log::GENESIS, |t| t.checksum),
        };
        let (bytes, sum) = log::frame(&row);
        let (segment, start) = match self.append(result.epoch, &row.prev_checksum, &bytes) {
            Ok(v) => v,
            Err(_) => {
                self.in_flight = false;
                return Err(self.poison());
            }
        };
        // Step 4: publish.
        self.in_flight = false;
        let end = start + bytes.len() as u64;
        self.frames.push(FrameLoc {
            segment,
            start,
            end,
        });
        self.tail = Some(Tail {
            segment,
            size: end,
            checksum: sum,
        });
        if let Some(id) = batch.client_txn_id {
            self.ids.insert(id, (result.epoch, batch_hash));
        }
        Ok(result)
    }
    /// Appends a synced frame, opening a synced segment first when needed.
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
            .is_none_or(|t| t.size as usize >= log::SEGMENT_TARGET);
        let (segment, start) = if rollover {
            let path = log_dir.join(log::segment_name(epoch));
            self.disk
                .write_new(&path, &log::segment_header(epoch, *prev))?;
            self.disk.sync(&path)?;
            self.disk.sync_dir(&log_dir)?;
            self.segments.push(path);
            (self.segments.len() - 1, log::HEADER_LEN as u64)
        } else {
            let t = self.tail.as_ref().expect("tail");
            (t.segment, t.size)
        };
        let path = &self.segments[segment];
        self.disk.append(path, bytes)?;
        self.disk.sync(path)?;
        Ok((segment, start))
    }
    /// Stores a payload; returns only after it is durable (ADR 0006 §5).
    pub fn put_blob(&mut self, bytes: &[u8]) -> Result<ContentHash, StoreError> {
        self.usable()?;
        let hash = *blake3::hash(bytes).as_bytes();
        let dir = self.blob_dir();
        let path = dir.join(hex(&hash));
        if self.disk.exists(&path)
            && self
                .disk
                .read(&path)
                .is_ok_and(|b| *blake3::hash(&b).as_bytes() == hash)
        {
            self.blobs.insert(hash);
            return Ok(hash);
        }
        let tmp = dir.join(format!("{}.tmp", hex(&hash)));
        let written = self
            .disk
            .write_new(&tmp, bytes)
            .and_then(|()| self.disk.sync(&tmp))
            .and_then(|()| self.disk.rename(&tmp, &path))
            .and_then(|()| self.disk.sync_dir(&dir));
        if written.is_err() {
            return Err(self.poison());
        }
        self.blobs.insert(hash);
        Ok(hash)
    }
    pub fn get_blob(&self, hash: &ContentHash) -> Payload {
        match self.disk.read(&self.blob_dir().join(hex(hash))) {
            Ok(bytes) if *blake3::hash(&bytes).as_bytes() == *hash => Payload::Available(bytes),
            Ok(_) => Payload::Unavailable(Unavailable::Corrupt),
            Err(_) => Payload::Unavailable(Unavailable::Missing),
        }
    }
    pub fn epoch(&self) -> Epoch {
        self.db.epoch()
    }
}
