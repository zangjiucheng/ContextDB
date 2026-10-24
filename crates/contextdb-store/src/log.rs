//! Durable logs (ADR 0006 §2, §4; ADR 0008 §3.2, §4): segments of checksummed,
//! hash-chained frames written in groups. The op_log and the session log share
//! framing, group headers and the tail algorithm; they differ only in their
//! domain strings and record types.
use crate::durability::Durability;
use contextdb_model::*;
use std::path::{Path, PathBuf};

/// op_log format 2 adds the group header (ADR 0008 §3.2); format 3 adds
/// the branch ops (tags 14–17) and the result trailer (ADR 0013 §8). Older
/// formats migrate explicitly (`migrate`).
pub const FORMAT_VERSION: u32 = 3;
pub const SEGMENT_TARGET: usize = 64 * 1024 * 1024;
/// magic 8 + format 4 + first record number 8 + prev_checksum 32 + header_check 8.
pub const HEADER_LEN: usize = 60;
/// len 8 + len_check 8 (before the body) and checksum 32 (after it).
const FRAME_OVERHEAD: usize = 48;
pub const GENESIS: [u8; 32] = [0; 32];

/// Domain separation of one log family.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub magic: &'static [u8; 8],
    pub domain: &'static [u8],
    pub len_domain: &'static [u8],
    pub header_domain: &'static [u8],
}
pub const OP_LOG: Spec = Spec {
    magic: b"CDBLOG\0\x01",
    domain: b"contextdb.op_log.v1",
    len_domain: b"contextdb.frame.len",
    header_domain: b"contextdb.segment.header",
};
pub const SESSION_LOG: Spec = Spec {
    magic: b"CDBSES\0\x01",
    domain: b"contextdb.session_log.v1",
    len_domain: b"contextdb.session_log.frame.len",
    header_domain: b"contextdb.session_log.segment.header",
};

/// Position of a frame in its sync group: `seq` numbers the groups actually
/// written (from 1, contiguous); `idx` < `count` within the group.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GroupPos {
    pub seq: u64,
    pub idx: u16,
    pub count: u16,
}
impl GroupPos {
    fn complete(&self) -> bool {
        self.idx + 1 == self.count
    }
}
impl Wire for GroupPos {
    fn encode(&self, o: &mut Vec<u8>) {
        self.seq.encode(o);
        self.idx.encode(o);
        self.count.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            seq: Wire::decode(i)?,
            idx: Wire::decode(i)?,
            count: Wire::decode(i)?,
        })
    }
}
/// A record type stored in a log family.
pub trait Record: Sized + Clone {
    /// Decodes a frame body of the given format.
    fn decode(body: &[u8], format: u32) -> Result<Self, WireError>;
    /// Encodes the current format.
    fn encode(&self) -> Vec<u8>;
    fn prev(&self) -> ContentHash;
    fn group(&self) -> GroupPos;
    /// Record-specific semantic checks; `number` is the 1-based record number.
    fn verify(&self, number: u64) -> Result<(), FrameFault>;
}

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
    /// Format 2. Rows decoded from formats 0 and 1 carry the default.
    pub group: GroupPos,
    /// Format 3: the idempotency hash of a `ctx_commit` whose batch the
    /// actor extended (ADR 0013 §4): it covers the client's ops,
    /// `expect_cert_valid` and the certificate, not the appended
    /// `ExpectHeads`. `None` means the batch hash serves.
    pub client_hash: Option<ContentHash>,
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
        self.encode_body_format(FORMAT_VERSION)
    }
    /// The body in `format`: fields a format lacks are left out.
    pub fn encode_body_format(&self, format: u32) -> Vec<u8> {
        let mut o = Vec::with_capacity(self.batch_blob.len() + self.result_blob.len() + 180);
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
        if format >= 2 {
            self.group.encode(&mut o);
        }
        if format >= 3 {
            self.client_hash.encode(&mut o);
        }
        o
    }
    /// What a retry with the same `client_txn_id` must match.
    pub fn idempotency_hash(&self) -> ContentHash {
        self.client_hash.unwrap_or(self.batch_hash)
    }
    /// Decodes the current format.
    pub fn decode_body(body: &[u8]) -> Result<Self, WireError> {
        Self::decode_format(body, FORMAT_VERSION)
    }
    /// Formats 0 and 1 share the body layout without the group header.
    pub fn decode_format(body: &[u8], format: u32) -> Result<Self, WireError> {
        let mut i = Decoder::new(body, DecodeLimits::DURABLE);
        let mut row = Self {
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
            group: GroupPos::default(),
            client_hash: None,
        };
        if format >= 2 {
            row.group = Wire::decode(&mut i)?;
        }
        if format >= 3 {
            row.client_hash = Wire::decode(&mut i)?;
        }
        if i.remaining() != 0 {
            return Err(WireError::TrailingBytes);
        }
        Ok(row)
    }
    pub fn batch(&self) -> Result<Batch, WireError> {
        Batch::from_bytes_with(&self.batch_blob, DecodeLimits::DURABLE)
    }
    pub fn result(&self) -> Result<CommitResult, WireError> {
        Ok(decode_result(&self.result_blob)?.0)
    }
    /// The format-3 trailer: the batch's branch and a merge's report.
    pub fn trailer(&self) -> Result<Option<ResultTrailer>, WireError> {
        Ok(decode_result(&self.result_blob)?.1)
    }
    /// What an idempotent retry returns (ADR 0006 §3 step 1, §7).
    pub fn acked(&self) -> Result<CommitResult, WireError> {
        match &self.acked_result {
            Some(r) => Ok(decode_result(r)?.0),
            None => self.result(),
        }
    }
}
/// A durable result: `CommitResult`, then (format 3) its trailer.
pub fn encode_result(result: &CommitResult, trailer: &ResultTrailer) -> Vec<u8> {
    let mut out = result.canonical_bytes();
    trailer.encode(&mut out);
    out
}
fn decode_result(bytes: &[u8]) -> Result<(CommitResult, Option<ResultTrailer>), WireError> {
    let mut i = Decoder::new(bytes, DecodeLimits::DURABLE);
    let result = CommitResult::decode(&mut i)?;
    let trailer = match i.remaining() {
        0 => None,
        _ => Some(ResultTrailer::decode(&mut i)?),
    };
    if i.remaining() != 0 {
        return Err(WireError::TrailingBytes);
    }
    Ok((result, trailer))
}
impl Record for LogRow {
    fn decode(body: &[u8], format: u32) -> Result<Self, WireError> {
        Self::decode_format(body, format)
    }
    fn encode(&self) -> Vec<u8> {
        self.encode_body()
    }
    fn prev(&self) -> ContentHash {
        self.prev_checksum
    }
    fn group(&self) -> GroupPos {
        self.group
    }
    fn verify(&self, number: u64) -> Result<(), FrameFault> {
        if self.epoch != Epoch(number) {
            return Err(FrameFault::EpochGap);
        }
        if *blake3::hash(&self.batch_blob).as_bytes() != self.batch_hash {
            return Err(FrameFault::BatchHash);
        }
        let batch = self.batch().map_err(|_| FrameFault::Undecodable)?;
        if (
            batch.branch,
            batch.principal,
            batch.channel,
            batch.client_txn_id,
        ) != (
            self.branch,
            self.principal,
            self.channel,
            self.client_txn_id,
        ) {
            return Err(FrameFault::Redundant);
        }
        let result = self.result().map_err(|_| FrameFault::Undecodable)?;
        if result.epoch != self.epoch {
            return Err(FrameFault::ResultEpoch);
        }
        if self.acked_result.is_some() && self.acked().is_err() {
            return Err(FrameFault::Undecodable);
        }
        Ok(())
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
/// op_log frame checksum of a body.
pub fn checksum(body: &[u8]) -> ContentHash {
    checksum_with(&OP_LOG, body)
}
pub fn checksum_with(spec: &Spec, body: &[u8]) -> ContentHash {
    let mut h = blake3::Hasher::new();
    h.update(spec.domain);
    h.update(body);
    *h.finalize().as_bytes()
}
/// Frame bytes plus the frame checksum that the next record chains to.
pub fn frame_with(spec: &Spec, body: &[u8]) -> (Vec<u8>, ContentHash) {
    let len = (body.len() as u64).to_le_bytes();
    let sum = checksum_with(spec, body);
    let mut out = Vec::with_capacity(body.len() + FRAME_OVERHEAD);
    out.extend_from_slice(&len);
    out.extend_from_slice(&check8(spec.len_domain, &len));
    out.extend_from_slice(body);
    out.extend_from_slice(&sum);
    (out, sum)
}
/// op_log frame of a row in the current format.
pub fn frame(row: &LogRow) -> (Vec<u8>, ContentHash) {
    frame_with(&OP_LOG, &row.encode_body())
}
/// op_log frame of a row in a legacy format (migration fixtures and tests).
pub fn frame_format(row: &LogRow, format: u32) -> (Vec<u8>, ContentHash) {
    frame_with(&OP_LOG, &row.encode_body_format(format))
}
/// Check bytes over the first 52 op_log header bytes.
pub fn header_check(header: &[u8]) -> [u8; 8] {
    check8(OP_LOG.header_domain, header)
}
pub fn segment_header_with(spec: &Spec, first: u64, prev: ContentHash, format: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(spec.magic);
    out.extend_from_slice(&format.to_le_bytes());
    out.extend_from_slice(&first.to_le_bytes());
    out.extend_from_slice(&prev);
    let check = check8(spec.header_domain, &out);
    out.extend_from_slice(&check);
    out
}
/// op_log segment header in the current format.
pub fn segment_header(first_epoch: Epoch, prev: ContentHash) -> Vec<u8> {
    segment_header_with(&OP_LOG, first_epoch.0, prev, FORMAT_VERSION)
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
pub fn parse_header_with(spec: &Spec, bytes: &[u8]) -> Option<Header> {
    if bytes.len() < HEADER_LEN
        || &bytes[..8] != spec.magic
        || check8(spec.header_domain, &bytes[..52]) != bytes[52..60]
    {
        return None;
    }
    Some(Header {
        format: u32::from_le_bytes(bytes[8..12].try_into().ok()?),
        first_epoch: Epoch(u64::from_le_bytes(bytes[12..20].try_into().ok()?)),
        prev: bytes[20..52].try_into().ok()?,
    })
}
pub fn parse_header(bytes: &[u8]) -> Option<Header> {
    parse_header_with(&OP_LOG, bytes)
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
    /// Group header out of sequence (ADR 0008 §3.2).
    Group,
}
impl FrameFault {
    /// Faults a crash can produce in unsynced bytes. Every other fault sits in
    /// a frame whose checksum verified, so it means a bug or tampering, never a
    /// torn write, and fails closed even on the final frame (review finding 7).
    pub fn is_torn(&self) -> bool {
        matches!(self, Self::Incomplete | Self::LengthCheck | Self::Checksum)
    }
}
/// A verified frame and where it sits.
#[derive(Clone, Debug)]
pub struct Located<R = LogRow> {
    pub row: R,
    pub checksum: ContentHash,
    pub segment: PathBuf,
    pub start: u64,
    pub end: u64,
}
/// Outcome of scanning every segment (ADR 0006 §4 steps 2–3, ADR 0008 §3.2).
#[derive(Debug)]
pub struct Scan<R = LogRow> {
    pub frames: Vec<Located<R>>,
    /// Truncatable last group: (segment, keep length, bytes dropped, fault).
    pub torn_tail: Option<(PathBuf, u64, u64, FrameFault)>,
    /// Final segment whose header is invalid and which contains no structurally
    /// valid frame anywhere: an interrupted segment creation.
    pub empty_tail_segment: Option<PathBuf>,
    /// Fail closed: (1-based record number, fault).
    pub broken: Option<(Epoch, FrameFault)>,
    pub newer_format: Option<u32>,
    /// An older format: open requires an explicit migration.
    pub older_format: Option<u32>,
}
impl<R> Default for Scan<R> {
    fn default() -> Self {
        Self {
            frames: Vec::new(),
            torn_tail: None,
            empty_tail_segment: None,
            broken: None,
            newer_format: None,
            older_format: None,
        }
    }
}
/// Parses one frame at `pos`; `Err` carries the fault.
pub fn parse_frame_with<R: Record>(
    spec: &Spec,
    bytes: &[u8],
    pos: usize,
    format: u32,
) -> Result<(R, ContentHash, usize), FrameFault> {
    let rest = &bytes[pos..];
    if rest.len() < 16 {
        return Err(FrameFault::Incomplete);
    }
    let len_bytes: [u8; 8] = rest[..8].try_into().expect("eight bytes");
    if check8(spec.len_domain, &len_bytes) != rest[8..16] {
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
    if checksum_with(spec, body) != sum {
        return Err(FrameFault::Checksum);
    }
    let row = R::decode(body, format).map_err(|_| FrameFault::Undecodable)?;
    Ok((row, sum, pos + end))
}
/// op_log frame in the current format.
pub fn parse_frame(bytes: &[u8], pos: usize) -> Result<(LogRow, ContentHash, usize), FrameFault> {
    parse_frame_with(&OP_LOG, bytes, pos, FORMAT_VERSION)
}
pub fn segments(disk: &dyn Durability, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !disk.exists(dir) {
        return Ok(Vec::new());
    }
    Ok(disk
        .list(dir)?
        .into_iter()
        .filter(|n| n.starts_with("seg-") && n.ends_with(".log"))
        .map(|n| dir.join(n))
        .collect())
}
/// op_log scan in the current format.
pub fn scan(disk: &dyn Durability, dir: &Path) -> std::io::Result<Scan> {
    scan_with::<LogRow>(&OP_LOG, disk, dir, FORMAT_VERSION)
}
/// The group currently being read.
struct Open {
    seq: u64,
    count: u16,
    next_idx: u16,
    start: u64,
    first_frame: usize,
}
/// Reads and verifies a whole log (ADR 0008 §3.2 tail algorithm):
/// 1. verify frames (structure, chain, record checks, group continuity) up to
///    the first fault F;
/// 2. a semantic fault fails closed wherever it is;
/// 3. a crash-type fault (or a short final group) truncates the last group G,
///    unless a structurally valid frame of a later group exists after F or a
///    later segment exists (G was synced before any later group or rollover).
pub fn scan_with<R: Record>(
    spec: &Spec,
    disk: &dyn Durability,
    dir: &Path,
    format: u32,
) -> std::io::Result<Scan<R>> {
    scan_with_formats(spec, disk, dir, format, format)
}
/// Explicit reader compatibility range; record decoding uses each segment's
/// actual version. The op_log keeps its existing exact-version migration path.
pub fn scan_with_formats<R: Record>(
    spec: &Spec,
    disk: &dyn Durability,
    dir: &Path,
    min_format: u32,
    format: u32,
) -> std::io::Result<Scan<R>> {
    let mut out = Scan::default();
    let segs = segments(disk, dir)?;
    let mut prev = GENESIS;
    let mut next: u64 = 1;
    let mut last_seq: u64 = 0;
    let mut open: Option<Open> = None;
    for (k, seg) in segs.iter().enumerate() {
        let last_segment = k + 1 == segs.len();
        let bytes = disk.read(seg)?;
        if open.is_some() {
            // Groups never span segments: rollover follows a synced group.
            out.broken = Some((Epoch(next), FrameFault::Group));
            return Ok(out);
        }
        let header = match parse_header_with(spec, &bytes) {
            Some(h) => h,
            // A header is synced before any frame, so a valid frame behind an
            // invalid header is corruption of acked data, never a crash.
            None if last_segment && next_frame::<R>(spec, &bytes, 0, format).is_none() => {
                out.empty_tail_segment = Some(seg.clone());
                break;
            }
            None => {
                out.broken = Some((Epoch(next), FrameFault::SegmentHeader));
                return Ok(out);
            }
        };
        if header.format > format {
            out.newer_format = Some(header.format);
            return Ok(out);
        }
        if header.format < min_format {
            out.older_format = Some(header.format);
            return Ok(out);
        }
        if header.first_epoch != Epoch(next) || header.prev != prev {
            out.broken = Some((Epoch(next), FrameFault::SegmentHeader));
            return Ok(out);
        }
        let mut pos = HEADER_LEN;
        while pos < bytes.len() {
            let parsed = parse_frame_with::<R>(spec, &bytes, pos, header.format).and_then(
                |(row, sum, end)| {
                    if row.prev() != prev {
                        return Err(FrameFault::Chain);
                    }
                    row.verify(next)?;
                    let g = row.group();
                    let continues = match &open {
                        Some(o) => g.seq == o.seq && g.idx == o.next_idx && g.count == o.count,
                        None => g.idx == 0 && g.count >= 1 && g.seq == last_seq + 1,
                    };
                    if !continues {
                        return Err(FrameFault::Group);
                    }
                    Ok((row, sum, end))
                },
            );
            match parsed {
                Ok((row, sum, end)) => {
                    let g = row.group();
                    if g.idx == 0 {
                        open = Some(Open {
                            seq: g.seq,
                            count: g.count,
                            next_idx: 0,
                            start: pos as u64,
                            first_frame: out.frames.len(),
                        });
                    }
                    let o = open.as_mut().expect("open group");
                    o.next_idx += 1;
                    if g.complete() {
                        last_seq = g.seq;
                        open = None;
                    }
                    prev = sum;
                    next += 1;
                    out.frames.push(Located {
                        row,
                        checksum: sum,
                        segment: seg.clone(),
                        start: pos as u64,
                        end: end as u64,
                    });
                    pos = end;
                }
                Err(fault) => {
                    if !fault.is_torn() || !last_segment {
                        out.broken = Some((Epoch(next), fault));
                        return Ok(out);
                    }
                    // G: the open group, or a new group starting at the fault.
                    let (g_seq, start, first_frame) = match &open {
                        Some(o) => (o.seq, o.start, o.first_frame),
                        None => (last_seq + 1, pos as u64, out.frames.len()),
                    };
                    let later = later_group::<R>(spec, &bytes, pos + 1, header.format, g_seq);
                    if later {
                        out.broken = Some((Epoch(next), fault));
                        return Ok(out);
                    }
                    out.frames.truncate(first_frame);
                    out.torn_tail = Some((seg.clone(), start, bytes.len() as u64 - start, fault));
                    return Ok(out);
                }
            }
        }
        if let Some(o) = &open
            && last_segment
        {
            // A short final group: never fully written, so never acked.
            out.frames.truncate(o.first_frame);
            out.torn_tail = Some((
                seg.clone(),
                o.start,
                bytes.len() as u64 - o.start,
                FrameFault::Incomplete,
            ));
            return Ok(out);
        }
    }
    Ok(out)
}
/// Any structurally valid frame from `from` on whose group is later than `seq`?
fn later_group<R: Record>(spec: &Spec, bytes: &[u8], from: usize, format: u32, seq: u64) -> bool {
    let mut pos = from;
    while let Some(p) = next_frame::<R>(spec, bytes, pos, format) {
        match parse_frame_with::<R>(spec, bytes, p, format) {
            Ok((row, _, end)) => {
                if row.group().seq > seq {
                    return true;
                }
                pos = end;
            }
            Err(_) => pos = p + 1,
        }
    }
    false
}
/// First position at or after `from` where a structurally valid frame starts.
fn next_frame<R: Record>(spec: &Spec, bytes: &[u8], from: usize, format: u32) -> Option<usize> {
    (from..bytes.len().saturating_sub(FRAME_OVERHEAD - 1))
        .find(|p| parse_frame_with::<R>(spec, bytes, *p, format).is_ok())
}
/// Every structurally valid op_log frame in `bytes` from `pos` on,
/// resynchronizing past unreadable regions. Returns the rows and whether any
/// bytes were unreadable. Used to list what a repair would drop.
pub fn readable_frames(bytes: &[u8], mut pos: usize) -> (Vec<LogRow>, bool) {
    let (mut rows, mut unreadable) = (Vec::new(), false);
    while pos < bytes.len() {
        match parse_frame(bytes, pos) {
            Ok((row, _, end)) => {
                rows.push(row);
                pos = end;
            }
            Err(_) => {
                unreadable = true;
                match next_frame::<LogRow>(&OP_LOG, bytes, pos + 1, FORMAT_VERSION) {
                    Some(p) => pos = p,
                    None => break,
                }
            }
        }
    }
    (rows, unreadable)
}
