//! The key log (ADR 0017 §8.2): one random 32-byte key per content hash,
//! kept in `keys/keylog-NNNNNNNN` segments next to the packs. It is truth,
//! like the packs: it cannot be rebuilt, and losing it loses every payload.
//!
//! - **Segments.** Each starts with a header `"CXKH" | kind u8 |
//!   purged_through u64 | check 8`, followed by records `"CXKY" |
//!   content_hash 32 | key 32 | check 8` (`check` is BLAKE3 of the rest,
//!   truncated). A scan resyncs at the next magic after any invalid record,
//!   as the pack scan does. A process only appends to a segment it created.
//! - **Generations.** A rewrite (a shred, milestone 6c) writes every
//!   surviving key into a segment of kind `Generation`. Keys load from the
//!   newest generation and every segment numbered after it; for a hash with
//!   several records the last wins.
//! - **Generation v2 (ADR 0019 §5.2).** Once the store has a GC ledger, a
//!   rewrite writes kind 2, whose header adds `collect_generation u64`
//!   before the check (29 bytes); kinds 0 and 1 read as generation 0.
//! - **Positions.** Each loaded key carries its [`KeyPos`], the segment
//!   number and byte offset of its record. Segment numbers only grow, so a
//!   position names one key record forever; the GC ledger matches on it.
//! - **Durability.** A blob's key record is appended, with an ordered sync,
//!   before its pack record, so the op_log group's full sync makes both
//!   durable (§8.2).
use crate::durability::Durability;
use contextdb_model::*;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};

pub(crate) const DIR: &str = "keys";
const PREFIX: &str = "keylog-";
const HEADER_MAGIC: [u8; 4] = *b"CXKH";
const RECORD_MAGIC: [u8; 4] = *b"CXKY";
pub(crate) const HEADER: usize = 4 + 1 + 8 + 8;
/// The kind-2 header: `collect_generation` before the check.
pub(crate) const HEADER_V2: usize = HEADER + 8;
pub(crate) const RECORD: usize = 4 + 32 + 32 + 8;

pub type Key = [u8; 32];
/// Where a key record sits: its key-log segment and byte offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyPos {
    pub segment: u32,
    pub offset: u64,
}

/// What a segment holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Keys appended by one process.
    Append = 0,
    /// Every key that survived a rewrite.
    Generation = 1,
    /// A generation that also records `collect_generation` (ADR 0019 §5.2).
    GenerationV2 = 2,
}
pub(crate) fn is_segment(name: &str) -> bool {
    number(name).is_some()
}
pub(crate) fn number(name: &str) -> Option<u32> {
    let n = name.strip_prefix(PREFIX)?;
    (n.len() == 8 && n.bytes().all(|b| b.is_ascii_digit()))
        .then(|| n.parse().ok())
        .flatten()
}
pub(crate) fn segment_name(n: u32) -> String {
    format!("{PREFIX}{n:08}")
}
fn check(bytes: &[u8]) -> [u8; 8] {
    blake3::hash(bytes).as_bytes()[..8]
        .try_into()
        .expect("8 bytes")
}
/// A segment header; `collect_generation` is written for kind 2 only.
pub(crate) fn header(kind: Kind, purged_through: Epoch, collect_generation: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_V2);
    out.extend_from_slice(&HEADER_MAGIC);
    out.push(kind as u8);
    out.extend_from_slice(&purged_through.0.to_le_bytes());
    if kind == Kind::GenerationV2 {
        out.extend_from_slice(&collect_generation.to_le_bytes());
    }
    let c = check(&out);
    out.extend_from_slice(&c);
    out
}
/// The header's kind, `purged_through` and `collect_generation` (0 before
/// kind 2), when it verifies.
pub(crate) fn parse_header(data: &[u8]) -> Option<(Kind, Epoch, u64)> {
    let (kind, len) = match *data.get(4)? {
        0 => (Kind::Append, HEADER),
        1 => (Kind::Generation, HEADER),
        2 => (Kind::GenerationV2, HEADER_V2),
        _ => return None,
    };
    let h = data.get(..len)?;
    let body = len - 8;
    if h[..4] != HEADER_MAGIC || check(&h[..body]) != h[body..] {
        return None;
    }
    let word = |at: usize| u64::from_le_bytes(h[at..at + 8].try_into().expect("8 bytes"));
    let collect = if kind == Kind::GenerationV2 {
        word(13)
    } else {
        0
    };
    Some((kind, Epoch(word(5)), collect))
}
pub(crate) fn record(hash: &ContentHash, key: &Key) -> Vec<u8> {
    let mut out = Vec::with_capacity(RECORD);
    out.extend_from_slice(&RECORD_MAGIC);
    out.extend_from_slice(hash);
    out.extend_from_slice(key);
    let c = check(&out[4..]);
    out.extend_from_slice(&c);
    out
}
/// Valid records of one segment, in order, with their byte offsets.
pub(crate) fn parse(data: &[u8]) -> Vec<(ContentHash, Key, u64)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(at) = data
        .get(pos..)
        .and_then(|d| d.windows(4).position(|w| w == RECORD_MAGIC))
        .map(|p| p + pos)
    {
        match data.get(at..at + RECORD) {
            Some(r) if check(&r[4..68]) == r[68..] => {
                out.push((
                    r[4..36].try_into().expect("32 bytes"),
                    r[36..68].try_into().expect("32 bytes"),
                    at as u64,
                ));
                pos = at + RECORD;
            }
            _ => pos = at + 1,
        }
    }
    out
}
/// A fresh random key.
pub(crate) fn fresh() -> io::Result<Key> {
    let mut k = [0u8; 32];
    getrandom::fill(&mut k).map_err(|e| io::Error::other(format!("getrandom: {e}")))?;
    Ok(k)
}
/// The loaded key log.
#[derive(Debug, Default)]
pub(crate) struct KeyLog {
    pub(crate) keys: HashMap<ContentHash, Key>,
    /// The position of each loaded key.
    pub(crate) positions: HashMap<ContentHash, KeyPos>,
    /// Every segment file, oldest first.
    pub(crate) segments: Vec<(u32, PathBuf)>,
    /// The newest generation's number, `purged_through` and
    /// `collect_generation`.
    pub(crate) generation: Option<(u32, Epoch, u64)>,
}
/// Loads keys from the newest generation and every later segment (§8.2).
pub(crate) fn load(disk: &dyn Durability, dir: &Path) -> io::Result<KeyLog> {
    let mut log = KeyLog::default();
    let mut names: Vec<(u32, String)> = (disk.list(dir)?.into_iter())
        .filter_map(|n| Some((number(&n)?, n)))
        .collect();
    names.sort();
    let mut data = Vec::with_capacity(names.len());
    for (n, name) in &names {
        let path = dir.join(name);
        let bytes = disk.read(&path)?;
        if let Some((Kind::Generation | Kind::GenerationV2, through, collect)) =
            parse_header(&bytes)
        {
            log.generation = Some((*n, through, collect));
        }
        log.segments.push((*n, path));
        data.push((*n, bytes));
    }
    let from = log.generation.map_or(0, |(n, ..)| n);
    for (n, bytes) in &data {
        if *n >= from {
            for (hash, key, offset) in parse(bytes) {
                log.keys.insert(hash, key);
                let at = KeyPos {
                    segment: *n,
                    offset,
                };
                log.positions.insert(hash, at);
            }
        }
    }
    Ok(log)
}
/// The next segment number.
pub(crate) fn next_number(log: &KeyLog) -> u32 {
    log.segments.last().map_or(1, |(n, _)| n + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_headers_round_trip_and_resync() {
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let (ka, kb) = ([7u8; 32], [8u8; 32]);
        let mut data = header(Kind::Append, Epoch(0), 0);
        data.extend(record(&a, &ka));
        data.extend(record(&b, &kb));
        assert_eq!(parse_header(&data), Some((Kind::Append, Epoch(0), 0)));
        let (at_a, at_b) = (HEADER as u64, (HEADER + RECORD) as u64);
        assert_eq!(parse(&data), vec![(a, ka, at_a), (b, kb, at_b)]);
        // A damaged first record costs only that record; a torn tail none.
        let mut bad = data.clone();
        bad[HEADER + 10] ^= 1;
        bad.extend_from_slice(&record(&a, &kb)[..RECORD - 1]);
        assert_eq!(parse(&bad), vec![(b, kb, at_b)]);
        let g = header(Kind::Generation, Epoch(9), 5);
        assert_eq!(g.len(), HEADER);
        assert_eq!(parse_header(&g), Some((Kind::Generation, Epoch(9), 0)));
        let mut forged = g.clone();
        forged[5] = 10;
        assert_eq!(parse_header(&forged), None);
        assert!(is_segment("keylog-00000001") && !is_segment("keylog-1.tmp"));
        assert_ne!(fresh().unwrap(), fresh().unwrap());
    }

    /// The kind-2 header (ADR 0019 §5.2): it round-trips; every truncation
    /// and every flipped byte fails to parse; a kind-2 byte on a 21-byte
    /// header and a kind-1 byte on a 29-byte one fail too.
    #[test]
    fn generation_v2_header_round_trips_and_refuses_damage() {
        let h = header(Kind::GenerationV2, Epoch(9), 4);
        assert_eq!(h.len(), HEADER_V2);
        assert_eq!(parse_header(&h), Some((Kind::GenerationV2, Epoch(9), 4)));
        let (a, ka) = ([1u8; 32], [7u8; 32]);
        let mut seg = h.clone();
        seg.extend(record(&a, &ka));
        assert_eq!(parse(&seg), vec![(a, ka, HEADER_V2 as u64)]);
        for n in 0..h.len() {
            assert_eq!(parse_header(&h[..n]), None, "truncated at {n}");
            for bit in [1u8, 0x80] {
                let mut bad = h.clone();
                bad[n] ^= bit;
                assert_eq!(parse_header(&bad), None, "byte {n} flipped");
            }
        }
        let mut short = header(Kind::Generation, Epoch(9), 0);
        short[4] = 2;
        assert_eq!(parse_header(&short), None);
        let mut long = h.clone();
        long[4] = 1;
        assert_eq!(parse_header(&long), None);
    }
}
