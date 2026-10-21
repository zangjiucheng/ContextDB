//! Session log (ADR 0008 §4, owner decision): sessions, read ledgers, issued
//! certificates and write audit records, and from P4 session facts and
//! telemetry (ADR 0010 §4.3, A8). It is a second append-only,
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
    /// `agent` is None for the root agent (ADR 0010 §7); P3 logs (tag 1)
    /// decode as root.
    Read {
        session_id: [u8; 16],
        agent: Option<String>,
        logical: LogicalId,
        rev: RevisionId,
        layer: Layer,
        epoch: Epoch,
        required: bool,
    },
    CertIssued {
        cert: Box<Certificate>,
    },
    /// Audit and coverage (e.g. a supersede reason); `with_deps` is whether
    /// the batch added a rule with incidences (ADR 0010 §10).
    Write {
        session_id: [u8; 16],
        agent: Option<String>,
        client_txn_id: [u8; 16],
        epoch: Epoch,
        reason: String,
        with_deps: bool,
    },
    End {
        session_id: [u8; 16],
        epoch: Epoch,
    },
    /// The session's current certificate (`ctx_check`) is `cert_id`, which an
    /// earlier `CertIssued` carries; replay restores it (P3 review finding 2).
    SessionCert {
        session_id: [u8; 16],
        agent: Option<String>,
        cert_id: ContentHash,
    },
    /// The op_log was cut under this session (e.g. `ctx doctor
    /// --truncate-before`): its ledger and handles may name reallocated ids.
    /// Durable, so later restarts keep refusing them (P3 closure check N1).
    LogBroken {
        session_id: [u8; 16],
    },
    /// The harness session this session serves (ADR 0010 §2.1).
    SessionKey {
        session_id: [u8; 16],
        harness: String,
        harness_session: String,
    },
    /// A delivered outbox fact (ADR 0010 §5); durable before its reply.
    Fact {
        session_id: [u8; 16],
        stream: [u8; 16],
        packet: Packet,
        epoch: Epoch,
    },
    /// A PreToolUse gate decision checked by the daemon (ADR 0010 §6).
    Gate {
        session_id: [u8; 16],
        agent: Option<String>,
        action: Action,
        mode: GateMode,
        verdict: String,
        decision: Decision,
        harness_version: String,
        epoch: Epoch,
        /// Wall-clock seconds, for the dogfood period.
        at: u64,
    },
    /// Drift computed for one agent (ADR 0010 §5.4).
    Drift {
        session_id: [u8; 16],
        agent: Option<String>,
        report: DriftReport,
        epoch: Epoch,
    },
    /// `context_update` items acknowledged by an agent (ADR 0010 §4.4).
    Reported {
        session_id: [u8; 16],
        agent: Option<String>,
        items: Vec<(LogicalId, (Option<RevisionId>, Summary))>,
    },
    /// Outbox stream high-water mark after a delivered batch, and a range
    /// the client could not resend (ADR 0010 §5.3).
    StreamMark {
        stream: [u8; 16],
        applied_through: u64,
        skipped: Option<(u64, u64)>,
    },
    /// An MCP call arrived without a claim (ADR 0010 §2.4), for the report.
    Unclaimed {
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
                agent,
                logical,
                rev,
                layer,
                epoch,
                required,
            } => {
                15u8.encode(o);
                session_id.encode(o);
                agent.encode(o);
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
                agent,
                client_txn_id,
                epoch,
                reason,
                with_deps,
            } => {
                16u8.encode(o);
                session_id.encode(o);
                agent.encode(o);
                client_txn_id.encode(o);
                epoch.encode(o);
                reason.encode(o);
                with_deps.encode(o);
            }
            Self::End { session_id, epoch } => {
                4u8.encode(o);
                session_id.encode(o);
                epoch.encode(o);
            }
            Self::SessionCert {
                session_id,
                agent,
                cert_id,
            } => {
                17u8.encode(o);
                session_id.encode(o);
                agent.encode(o);
                cert_id.encode(o);
            }
            Self::LogBroken { session_id } => {
                6u8.encode(o);
                session_id.encode(o);
            }
            Self::SessionKey {
                session_id,
                harness,
                harness_session,
            } => {
                7u8.encode(o);
                session_id.encode(o);
                harness.encode(o);
                harness_session.encode(o);
            }
            Self::Fact {
                session_id,
                stream,
                packet,
                epoch,
            } => {
                8u8.encode(o);
                session_id.encode(o);
                stream.encode(o);
                packet.encode(o);
                epoch.encode(o);
            }
            Self::Gate {
                session_id,
                agent,
                action,
                mode,
                verdict,
                decision,
                harness_version,
                epoch,
                at,
            } => {
                9u8.encode(o);
                session_id.encode(o);
                agent.encode(o);
                action.encode(o);
                mode.encode(o);
                verdict.encode(o);
                decision.encode(o);
                harness_version.encode(o);
                epoch.encode(o);
                at.encode(o);
            }
            Self::Drift {
                session_id,
                agent,
                report,
                epoch,
            } => {
                10u8.encode(o);
                session_id.encode(o);
                agent.encode(o);
                report.encode(o);
                epoch.encode(o);
            }
            Self::Reported {
                session_id,
                agent,
                items,
            } => {
                11u8.encode(o);
                session_id.encode(o);
                agent.encode(o);
                items.encode(o);
            }
            Self::StreamMark {
                stream,
                applied_through,
                skipped,
            } => {
                14u8.encode(o);
                stream.encode(o);
                applied_through.encode(o);
                skipped.encode(o);
            }
            Self::Unclaimed { epoch } => {
                18u8.encode(o);
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
                agent: None,
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
                agent: None,
                client_txn_id: Wire::decode(i)?,
                epoch: Wire::decode(i)?,
                reason: Wire::decode(i)?,
                with_deps: false,
            },
            4 => Self::End {
                session_id: Wire::decode(i)?,
                epoch: Wire::decode(i)?,
            },
            5 => Self::SessionCert {
                session_id: Wire::decode(i)?,
                agent: None,
                cert_id: Wire::decode(i)?,
            },
            6 => Self::LogBroken {
                session_id: Wire::decode(i)?,
            },
            7 => Self::SessionKey {
                session_id: Wire::decode(i)?,
                harness: Wire::decode(i)?,
                harness_session: Wire::decode(i)?,
            },
            8 => Self::Fact {
                session_id: Wire::decode(i)?,
                stream: Wire::decode(i)?,
                packet: Wire::decode(i)?,
                epoch: Wire::decode(i)?,
            },
            9 => Self::Gate {
                session_id: Wire::decode(i)?,
                agent: Wire::decode(i)?,
                action: Wire::decode(i)?,
                mode: Wire::decode(i)?,
                verdict: Wire::decode(i)?,
                decision: Wire::decode(i)?,
                harness_version: Wire::decode(i)?,
                epoch: Wire::decode(i)?,
                at: Wire::decode(i)?,
            },
            10 => Self::Drift {
                session_id: Wire::decode(i)?,
                agent: Wire::decode(i)?,
                report: Wire::decode(i)?,
                epoch: Wire::decode(i)?,
            },
            11 => Self::Reported {
                session_id: Wire::decode(i)?,
                agent: Wire::decode(i)?,
                items: Wire::decode(i)?,
            },
            14 => Self::StreamMark {
                stream: Wire::decode(i)?,
                applied_through: Wire::decode(i)?,
                skipped: Wire::decode(i)?,
            },
            15 => Self::Read {
                session_id: Wire::decode(i)?,
                agent: Wire::decode(i)?,
                logical: Wire::decode(i)?,
                rev: Wire::decode(i)?,
                layer: Wire::decode(i)?,
                epoch: Wire::decode(i)?,
                required: Wire::decode(i)?,
            },
            16 => Self::Write {
                session_id: Wire::decode(i)?,
                agent: Wire::decode(i)?,
                client_txn_id: Wire::decode(i)?,
                epoch: Wire::decode(i)?,
                reason: Wire::decode(i)?,
                with_deps: Wire::decode(i)?,
            },
            17 => Self::SessionCert {
                session_id: Wire::decode(i)?,
                agent: Wire::decode(i)?,
                cert_id: Wire::decode(i)?,
            },
            18 => Self::Unclaimed {
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
        // Groups a killed process left only in the page cache were replayed
        // above; make them durable before anything relies on them, since a
        // later empty flush syncs nothing (P3 closure check N2).
        if let Some(last) = log.segments.last() {
            log.disk.sync(last).map_err(io)?;
        }
        // Likewise a segment whose rollover was killed before its directory
        // sync: later groups append to it, so its name must be durable.
        log.disk.sync_dir(&log.dir).map_err(io)?;
        report.events = events.len() as u64;
        Ok((log, events, report))
    }
    /// Appends `events` as one group (or several, past u16::MAX events) and
    /// syncs it. A failure poisons the log.
    pub fn append_group(&mut self, events: &[SessionEvent]) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        if events.is_empty() {
            return Ok(());
        }
        // `GroupPos` counts in u16: larger drains become several groups, each
        // synced before the next, so at most one group is ever unsynced
        // (ADR 0008 §4 R2; P3 review finding 3).
        if events.len() > usize::from(u16::MAX) {
            for chunk in events.chunks(usize::from(u16::MAX)) {
                self.append_group(chunk)?;
            }
            return Ok(());
        }
        let count = events.len() as u16;
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
