//! Durable op_log (ADR 0006 §2, §4): segments of checksummed, hash-chained frames.
use crate::durability::Durability;
use contextdb_model::*;
use std::path::{Path, PathBuf};

pub const FORMAT_VERSION: u32 = 1;
pub const SEGMENT_TARGET: usize = 64 * 1024 * 1024;
const MAGIC: &[u8; 8] = b"CDBLOG\0\x01";
const DOMAIN: &[u8] = b"contextdb.op_log.v1";
const LEN_DOMAIN: &[u8] = b"contextdb.frame.len";
const HEADER_DOMAIN: &[u8] = b"contextdb.segment.header";
/// magic 8 + format 4 + first_epoch 8 + prev_checksum 32 + header_check 8.
pub const HEADER_LEN: usize = 60;
/// len 8 + len_check 8 (before the body) and checksum 32 (after it).
const FRAME_OVERHEAD: usize = 48;
pub const GENESIS: [u8; 32] = [0; 32];

/// One committed batch (ADR 0006 §2). Blob fields carry u64 lengths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRow {
    pub epoch: Epoch,
    pub branch: BranchId,
    pub principal: PrincipalId,
    pub channel: Channel,
    pub client_txn_id: Option<[u8; 16]>,
    pub batch_hash: ContentHash,
    pub batch_blob: Vec<u8>,
    pub result_blob: Vec<u8>,
    /// Originally acked result kept across result-changing migrations (§7).
    pub acked_result: Option<Vec<u8>>,
    pub prev_checksum: ContentHash,
}
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    (bytes.len() as u64).encode(out);
    out.extend_from_slice(bytes);
}
fn get_bytes(input: &mut Decoder<'_>) -> Result<Vec<u8>, WireError> {
    let n = u64::decode(input)?;
    let n = usize::try_from(n).map_err(|_| WireError::LimitExceeded)?;
    Ok(input.take_bytes(n)?.to_vec())
}
impl LogRow {
    pub fn encode_body(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(self.batch_blob.len() + self.result_blob.len() + 128);
        self.epoch.encode(&mut o);
        self.branch.encode(&mut o);
        self.principal.encode(&mut o);
        self.channel.encode(&mut o);
        self.client_txn_id.encode(&mut o);
        self.batch_hash.encode(&mut o);
        put_bytes(&mut o, &self.batch_blob);
        put_bytes(&mut o, &self.result_blob);
        self.acked_result.is_some().encode(&mut o);
        if let Some(r) = &self.acked_result {
            put_bytes(&mut o, r);
        }
        self.prev_checksum.encode(&mut o);
        o
    }
    pub fn decode_body(body: &[u8]) -> Result<Self, WireError> {
        let mut i = Decoder::new(body, DecodeLimits::DURABLE);
        let row = Self {
            epoch: Wire::decode(&mut i)?,
            branch: Wire::decode(&mut i)?,
            principal: Wire::decode(&mut i)?,
            channel: Wire::decode(&mut i)?,
            client_txn_id: Wire::decode(&mut i)?,
            batch_hash: Wire::decode(&mut i)?,
            batch_blob: get_bytes(&mut i)?,
            result_blob: get_bytes(&mut i)?,
            acked_result: if bool::decode(&mut i)? {
                Some(get_bytes(&mut i)?)
            } else {
                None
            },
            prev_checksum: Wire::decode(&mut i)?,
        };
        if i.remaining() != 0 {
            return Err(WireError::TrailingBytes);
        }
        Ok(row)
    }
    pub fn batch(&self) -> Result<Batch, WireError> {
        Batch::from_bytes_with(&self.batch_blob, DecodeLimits::DURABLE)
    }
    pub fn result(&self) -> Result<CommitResult, WireError> {
        CommitResult::from_bytes_with(&self.result_blob, DecodeLimits::DURABLE)
    }
    /// What an idempotent retry returns (ADR 0006 §3 step 1, §7).
    pub fn acked(&self) -> Result<CommitResult, WireError> {
        match &self.acked_result {
            Some(r) => CommitResult::from_bytes_with(r, DecodeLimits::DURABLE),
            None => self.result(),
        }
    }
}
fn check8(domain: &[u8], data: &[u8]) -> [u8; 8] {
    let mut h = blake3::Hasher::new();
    h.update(domain);
    h.update(data);
    h.finalize().as_bytes()[..8]
        .try_into()
        .expect("eight bytes")
}
pub fn checksum(body: &[u8]) -> ContentHash {
    let mut h = blake3::Hasher::new();
    h.update(DOMAIN);
    h.update(body);
    *h.finalize().as_bytes()
}
/// Frame bytes plus the frame checksum that the next row chains to.
pub fn frame(row: &LogRow) -> (Vec<u8>, ContentHash) {
    let body = row.encode_body();
    let len = (body.len() as u64).to_le_bytes();
    let sum = checksum(&body);
    let mut out = Vec::with_capacity(body.len() + FRAME_OVERHEAD);
    out.extend_from_slice(&len);
    out.extend_from_slice(&check8(LEN_DOMAIN, &len));
    out.extend_from_slice(&body);
    out.extend_from_slice(&sum);
    (out, sum)
}
/// Check bytes over the first 52 header bytes.
pub fn header_check(header: &[u8]) -> [u8; 8] {
    check8(HEADER_DOMAIN, header)
}
pub fn segment_header(first_epoch: Epoch, prev: ContentHash) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&first_epoch.0.to_le_bytes());
    out.extend_from_slice(&prev);
    let check = check8(HEADER_DOMAIN, &out);
    out.extend_from_slice(&check);
    out
}
pub fn segment_name(first_epoch: Epoch) -> String {
    format!("seg-{:020}.log", first_epoch.0)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub format: u32,
    pub first_epoch: Epoch,
    pub prev: ContentHash,
}
pub fn parse_header(bytes: &[u8]) -> Option<Header> {
    if bytes.len() < HEADER_LEN
        || &bytes[..8] != MAGIC
        || check8(HEADER_DOMAIN, &bytes[..52]) != bytes[52..60]
    {
        return None;
    }
    Some(Header {
        format: u32::from_le_bytes(bytes[8..12].try_into().ok()?),
        first_epoch: Epoch(u64::from_le_bytes(bytes[12..20].try_into().ok()?)),
        prev: bytes[20..52].try_into().ok()?,
    })
}

/// Why a frame failed verification (ADR 0006 §2 "Verification at open").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameFault {
    Incomplete,
    LengthCheck,
    Checksum,
    Undecodable,
    Chain,
    EpochGap,
    BatchHash,
    Redundant,
    ResultEpoch,
    SegmentHeader,
}
/// A verified frame and where it sits.
#[derive(Clone, Debug)]
pub struct Located {
    pub row: LogRow,
    pub checksum: ContentHash,
    pub segment: PathBuf,
    pub end: u64,
}
/// Outcome of scanning every segment (ADR 0006 §4 steps 2–3).
#[derive(Debug, Default)]
pub struct Scan {
    pub frames: Vec<Located>,
    /// Final-frame or trailing-byte fault: (segment, verified length, bytes dropped, fault).
    pub torn_tail: Option<(PathBuf, u64, u64, FrameFault)>,
    /// Final segment without a valid header and without verified frames.
    pub empty_tail_segment: Option<PathBuf>,
    /// An earlier frame failed: fail closed.
    pub broken: Option<(Epoch, FrameFault)>,
    pub newer_format: Option<u32>,
    /// An older format: open requires an explicit migration.
    pub older_format: Option<u32>,
}
fn verify(row: &LogRow, prev: &ContentHash, expected: Epoch) -> Result<(), FrameFault> {
    if row.prev_checksum != *prev {
        return Err(FrameFault::Chain);
    }
    if row.epoch != expected {
        return Err(FrameFault::EpochGap);
    }
    if *blake3::hash(&row.batch_blob).as_bytes() != row.batch_hash {
        return Err(FrameFault::BatchHash);
    }
    let batch = row.batch().map_err(|_| FrameFault::Undecodable)?;
    if (
        batch.branch,
        batch.principal,
        batch.channel,
        batch.client_txn_id,
    ) != (row.branch, row.principal, row.channel, row.client_txn_id)
    {
        return Err(FrameFault::Redundant);
    }
    let result = row.result().map_err(|_| FrameFault::Undecodable)?;
    if result.epoch != row.epoch {
        return Err(FrameFault::ResultEpoch);
    }
    if row.acked_result.is_some() && row.acked().is_err() {
        return Err(FrameFault::Undecodable);
    }
    Ok(())
}
/// Parses one frame at `pos`; `Err` carries the fault.
pub fn parse_frame(bytes: &[u8], pos: usize) -> Result<(LogRow, ContentHash, usize), FrameFault> {
    let rest = &bytes[pos..];
    if rest.len() < 16 {
        return Err(FrameFault::Incomplete);
    }
    let len_bytes: [u8; 8] = rest[..8].try_into().expect("eight bytes");
    if check8(LEN_DOMAIN, &len_bytes) != rest[8..16] {
        return Err(FrameFault::LengthCheck);
    }
    let len = usize::try_from(u64::from_le_bytes(len_bytes)).map_err(|_| FrameFault::Incomplete)?;
    let end = 16usize
        .checked_add(len)
        .and_then(|n| n.checked_add(32))
        .ok_or(FrameFault::Incomplete)?;
    if end > rest.len() {
        return Err(FrameFault::Incomplete);
    }
    let body = &rest[16..16 + len];
    let sum: ContentHash = rest[16 + len..end].try_into().expect("32 bytes");
    if checksum(body) != sum {
        return Err(FrameFault::Checksum);
    }
    let row = LogRow::decode_body(body).map_err(|_| FrameFault::Undecodable)?;
    Ok((row, sum, pos + end))
}
pub fn segments(disk: &dyn Durability, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    Ok(disk
        .list(dir)?
        .into_iter()
        .filter(|n| n.starts_with("seg-") && n.ends_with(".log"))
        .map(|n| dir.join(n))
        .collect())
}
/// Reads and verifies the whole log. Only the final frame of the final segment
/// (or bytes after it) may be classified as a torn tail; anything earlier is broken.
pub fn scan(disk: &dyn Durability, dir: &Path) -> std::io::Result<Scan> {
    let mut out = Scan::default();
    let segs = segments(disk, dir)?;
    let mut prev = GENESIS;
    let mut next_epoch = Epoch(1);
    for (k, seg) in segs.iter().enumerate() {
        let last_segment = k + 1 == segs.len();
        let bytes = disk.read(seg)?;
        let header = match parse_header(&bytes) {
            Some(h) => h,
            None if last_segment => {
                out.empty_tail_segment = Some(seg.clone());
                break;
            }
            None => {
                out.broken = Some((next_epoch, FrameFault::SegmentHeader));
                return Ok(out);
            }
        };
        if header.format > FORMAT_VERSION {
            out.newer_format = Some(header.format);
            return Ok(out);
        }
        if header.format < FORMAT_VERSION {
            out.older_format = Some(header.format);
            return Ok(out);
        }
        if header.first_epoch != next_epoch || header.prev != prev {
            out.broken = Some((next_epoch, FrameFault::SegmentHeader));
            return Ok(out);
        }
        let mut pos = HEADER_LEN;
        while pos < bytes.len() {
            let parsed = parse_frame(&bytes, pos).and_then(|(row, sum, end)| {
                verify(&row, &prev, next_epoch).map(|()| (row, sum, end))
            });
            match parsed {
                Ok((row, sum, end)) => {
                    prev = sum;
                    next_epoch = Epoch(next_epoch.0 + 1);
                    out.frames.push(Located {
                        row,
                        checksum: sum,
                        segment: seg.clone(),
                        end: end as u64,
                    });
                    pos = end;
                }
                Err(fault) => {
                    // A later verified frame proves this one is not the tail.
                    let later = last_segment && !any_frame_after(&bytes, pos);
                    if later {
                        out.torn_tail =
                            Some((seg.clone(), pos as u64, (bytes.len() - pos) as u64, fault));
                    } else {
                        out.broken = Some((next_epoch, fault));
                    }
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}
/// True when a structurally valid frame (length check + checksum) starts after `pos`.
fn any_frame_after(bytes: &[u8], pos: usize) -> bool {
    (pos + 1..bytes.len().saturating_sub(FRAME_OVERHEAD - 1)).any(|p| parse_frame(bytes, p).is_ok())
}
