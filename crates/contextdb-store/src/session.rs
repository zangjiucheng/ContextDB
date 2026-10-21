//! Session log (ADR 0008 §4, owner decision): sessions, read ledgers, issued
//! certificates and write audit records. It is a second append-only,
//! hash-chained log family under `<dir>/sessions/`, sharing op_log framing and
//! the tail algorithm (`log::scan_with`). It never feeds kernel state.
//!
//! Every `append_group` writes one group and syncs it before returning, so at
//! most one group is ever unsynced (review R2); the daemon's writer thread
//! batches queued events into groups.
use crate::{
    durability::Durability,
    log::{self, FrameFault, GroupPos, Record},
    store::StoreError,
};
use contextdb_model::*;
use std::path::{Path, PathBuf};

pub const SESSION_FORMAT: u32 = 1;

/// Read layer recorded in the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    L0,
    L1,
    L2,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    Start {
        session_id: [u8; 16],
        parent: Option<[u8; 16]>,
        harness: String,
        repo: String,
        branch: BranchId,
        head_sha: String,
        principal: PrincipalId,
        channel: Channel,
        started_epoch: Epoch,
        log_checksum: ContentHash,
    },
    Read {
        session_id: [u8; 16],
        logical: LogicalId,
        rev: RevisionId,
        layer: Layer,
        epoch: Epoch,
        required: bool,
    },
    CertIssued {
        cert: Box<Certificate>,
    },
    /// Audit only (e.g. a supersede reason).
    Write {
        session_id: [u8; 16],
        client_txn_id: [u8; 16],
        epoch: Epoch,
        reason: String,
    },
    End {
        session_id: [u8; 16],
        epoch: Epoch,
    },
}
impl Wire for Layer {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::L0),
            1 => Ok(Self::L1),
            2 => Ok(Self::L2),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for SessionEvent {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Start {
                session_id,
                parent,
                harness,
                repo,
                branch,
                head_sha,
                principal,
                channel,
                started_epoch,
                log_checksum,
            } => {
                0u8.encode(o);
                session_id.encode(o);
                parent.encode(o);
                harness.encode(o);
                repo.encode(o);
                branch.encode(o);
                head_sha.encode(o);
                principal.encode(o);
                channel.encode(o);
                started_epoch.encode(o);
                log_checksum.encode(o);
            }
            Self::Read {
                session_id,
                logical,
                rev,
                layer,
                epoch,
                required,
            } => {
                1u8.encode(o);
                session_id.encode(o);
                logical.encode(o);
                rev.encode(o);
                layer.encode(o);
                epoch.encode(o);
                required.encode(o);
            }
            Self::CertIssued { cert } => {
                2u8.encode(o);
                cert.encode(o);
            }
            Self::Write {
                session_id,
                client_txn_id,
                epoch,
                reason,
            } => {
                3u8.encode(o);
                session_id.encode(o);
                client_txn_id.encode(o);
                epoch.encode(o);
                reason.encode(o);
            }
            Self::End { session_id, epoch } => {
                4u8.encode(o);
                session_id.encode(o);
                epoch.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Start {
                session_id: Wire::decode(i)?,
                parent: Wire::decode(i)?,
                harness: Wire::decode(i)?,
                repo: Wire::decode(i)?,
                branch: Wire::decode(i)?,
                head_sha: Wire::decode(i)?,
                principal: Wire::decode(i)?,
                channel: Wire::decode(i)?,
                started_epoch: Wire::decode(i)?,
                log_checksum: Wire::decode(i)?,
            },
            1 => Self::Read {
                session_id: Wire::decode(i)?,
                logical: Wire::decode(i)?,
                rev: Wire::decode(i)?,
                layer: Wire::decode(i)?,
                epoch: Wire::decode(i)?,
                required: Wire::decode(i)?,
            },
            2 => Self::CertIssued {
                cert: Box::new(Wire::decode(i)?),
            },
            3 => Self::Write {
                session_id: Wire::decode(i)?,
                client_txn_id: Wire::decode(i)?,
                epoch: Wire::decode(i)?,
                reason: Wire::decode(i)?,
            },
            4 => Self::End {
                session_id: Wire::decode(i)?,
                epoch: Wire::decode(i)?,
            },
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
/// One framed session event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecord {
    pub event: SessionEvent,
    pub prev: ContentHash,
    pub group: GroupPos,
}
impl Record for SessionRecord {
    fn decode(body: &[u8], _format: u32) -> Result<Self, WireError> {
        let mut i = Decoder::new(body, DecodeLimits::DURABLE);
        let r = Self {
            event: Wire::decode(&mut i)?,
            prev: Wire::decode(&mut i)?,
            group: Wire::decode(&mut i)?,
        };
        if i.remaining() != 0 {
            return Err(WireError::TrailingBytes);
        }
        Ok(r)
    }
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.event.encode(&mut o);
        self.prev.encode(&mut o);
        self.group.encode(&mut o);
        o
    }
    fn prev(&self) -> ContentHash {
        self.prev
    }
    fn group(&self) -> GroupPos {
        self.group
    }
    fn verify(&self, _number: u64) -> Result<(), FrameFault> {
        Ok(())
    }
}
/// What open found in the session log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionOpenReport {
    pub events: u64,
    pub truncated_bytes: Option<u64>,
    pub removed_segment: Option<PathBuf>,
}
pub struct SessionLog<D: Durability> {
    disk: D,
    dir: PathBuf,
    segments: Vec<PathBuf>,
    tail: Option<(usize, u64)>,
    prev: ContentHash,
    next: u64,
    group_seq: u64,
    segment_target: usize,
    poisoned: bool,
}
fn io(e: std::io::Error) -> StoreError {
    StoreError::Io(e.to_string())
}
impl<D: Durability> SessionLog<D> {
    /// Opens (creating if needed) the session log under `store_dir/sessions`
    /// and returns every durable event in order. The caller holds the store lock.
    pub fn open(
        disk: D,
        store_dir: &Path,
        strict_tail: bool,
        segment_target: usize,
    ) -> Result<(Self, Vec<SessionEvent>, SessionOpenReport), StoreError> {
        let dir = store_dir.join("sessions");
        disk.create_dir_all(&dir).map_err(io)?;
        disk.sync_dir(store_dir).map_err(io)?;
        let scan = log::scan_with::<SessionRecord>(&log::SESSION_LOG, &disk, &dir, SESSION_FORMAT)
            .map_err(io)?;
        if let Some(f) = scan.newer_format.or(scan.older_format) {
            return Err(StoreError::FormatUnsupported(f));
        }
        if let Some((epoch, fault)) = scan.broken {
            return Err(StoreError::ChainBroken { epoch, fault });
        }
        let mut report = SessionOpenReport::default();
        if let Some(seg) = &scan.empty_tail_segment {
            if strict_tail {
                return Err(StoreError::TailUnverifiable {
                    fault: FrameFault::SegmentHeader,
                });
            }
            disk.remove(seg).map_err(io)?;
            disk.sync_dir(&dir).map_err(io)?;
            report.removed_segment = Some(seg.clone());
        }
        if let Some((seg, keep, bytes, fault)) = scan.torn_tail.clone() {
            if strict_tail {
                return Err(StoreError::TailUnverifiable { fault });
            }
            disk.truncate(&seg, keep).map_err(io)?;
            disk.sync(&seg).map_err(io)?;
            report.truncated_bytes = Some(bytes);
        }
        let segments = log::segments(&disk, &dir).map_err(io)?;
        let mut log = Self {
            disk,
            dir,
            segments,
            tail: None,
            prev: log::GENESIS,
            next: 1,
            group_seq: 0,
            segment_target,
            poisoned: false,
        };
        let mut events = Vec::with_capacity(scan.frames.len());
        for f in scan.frames {
            let k = log
                .segments
                .iter()
                .position(|s| *s == f.segment)
                .expect("scanned segment");
            log.tail = Some((k, f.end));
            log.prev = f.checksum;
            log.next += 1;
            log.group_seq = f.row.group.seq;
            events.push(f.row.event);
        }
        if log.tail.is_none() && !log.segments.is_empty() {
            // A header-only last segment continues the chain.
            let n = log.segments.len() - 1;
            let bytes = log.disk.read(&log.segments[n]).map_err(io)?;
            let header = log::parse_header_with(&log::SESSION_LOG, &bytes).expect("scanned header");
            log.tail = Some((n, bytes.len() as u64));
            log.prev = header.prev;
        }
        report.events = events.len() as u64;
        Ok((log, events, report))
    }
    /// Appends `events` as one group and syncs it. A failure poisons the log.
    pub fn append_group(&mut self, events: &[SessionEvent]) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        if events.is_empty() {
            return Ok(());
        }
        let count = u16::try_from(events.len())
            .map_err(|_| StoreError::Io("session group too large".into()))?;
        let seq = self.group_seq + 1;
        let first = self.next;
        let first_prev = self.prev;
        let mut prev = self.prev;
        let mut bytes = Vec::new();
        for (idx, event) in events.iter().enumerate() {
            let record = SessionRecord {
                event: event.clone(),
                prev,
                group: GroupPos {
                    seq,
                    idx: idx as u16,
                    count,
                },
            };
            let (frame, sum) = log::frame_with(&log::SESSION_LOG, &record.encode());
            bytes.extend_from_slice(&frame);
            prev = sum;
        }
        let written = (|| -> std::io::Result<(usize, u64)> {
            let rollover = self
                .tail
                .is_none_or(|(_, size)| size as usize >= self.segment_target);
            let (k, start) = if rollover {
                let path = self.dir.join(log::segment_name(Epoch(first)));
                self.disk.write_new(
                    &path,
                    &log::segment_header_with(&log::SESSION_LOG, first, first_prev, SESSION_FORMAT),
                )?;
                self.disk.sync(&path)?;
                self.disk.sync_dir(&self.dir)?;
                self.segments.push(path);
                (self.segments.len() - 1, log::HEADER_LEN as u64)
            } else {
                self.tail.expect("tail")
            };
            self.disk.append(&self.segments[k], &bytes)?;
            self.disk.sync(&self.segments[k])?;
            Ok((k, start))
        })();
        match written {
            Ok((k, start)) => {
                self.tail = Some((k, start + bytes.len() as u64));
                self.prev = prev;
                self.next += events.len() as u64;
                self.group_seq = seq;
                Ok(())
            }
            Err(e) => {
                self.poisoned = true;
                Err(io(e))
            }
        }
    }
}
