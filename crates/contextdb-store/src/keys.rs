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
pub(crate) const RECORD: usize = 4 + 32 + 32 + 8;

pub type Key = [u8; 32];

/// What a segment holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Keys appended by one process.
    Append = 0,
    /// Every key that survived a rewrite.
    Generation = 1,
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
pub(crate) fn header(kind: Kind, purged_through: Epoch) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER);
    out.extend_from_slice(&HEADER_MAGIC);
    out.push(kind as u8);
    out.extend_from_slice(&purged_through.0.to_le_bytes());
    let c = check(&out);
    out.extend_from_slice(&c);
    out
}
/// The header's kind and `purged_through`, when it verifies.
pub(crate) fn parse_header(data: &[u8]) -> Option<(Kind, Epoch)> {
    let h = data.get(..HEADER)?;
    if h[..4] != HEADER_MAGIC || check(&h[..13]) != h[13..] {
        return None;
    }
    let kind = match h[4] {
        0 => Kind::Append,
        1 => Kind::Generation,
        _ => return None,
    };
    Some((kind, Epoch(u64::from_le_bytes(h[5..13].try_into().ok()?))))
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
/// Valid records of one segment, in order.
pub(crate) fn parse(data: &[u8]) -> Vec<(ContentHash, Key)> {
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
    /// Every segment file, oldest first.
    pub(crate) segments: Vec<(u32, PathBuf)>,
    /// The newest generation's number and `purged_through`.
    pub(crate) generation: Option<(u32, Epoch)>,
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
        if let Some((Kind::Generation, through)) = parse_header(&bytes) {
            log.generation = Some((*n, through));
        }
        log.segments.push((*n, path));
        data.push((*n, bytes));
    }
    let from = log.generation.map_or(0, |(n, _)| n);
    for (n, bytes) in &data {
        if *n >= from {
            log.keys.extend(parse(bytes));
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
        let mut data = header(Kind::Append, Epoch(0));
        data.extend(record(&a, &ka));
        data.extend(record(&b, &kb));
        assert_eq!(parse_header(&data), Some((Kind::Append, Epoch(0))));
        assert_eq!(parse(&data), vec![(a, ka), (b, kb)]);
        // A damaged first record costs only that record; a torn tail none.
        let mut bad = data.clone();
        bad[HEADER + 10] ^= 1;
        bad.extend_from_slice(&record(&a, &kb)[..RECORD - 1]);
        assert_eq!(parse(&bad), vec![(b, kb)]);
        let g = header(Kind::Generation, Epoch(9));
        assert_eq!(parse_header(&g), Some((Kind::Generation, Epoch(9))));
        let mut forged = g.clone();
        forged[5] = 10;
        assert_eq!(parse_header(&forged), None);
        assert!(is_segment("keylog-00000001") && !is_segment("keylog-1.tmp"));
        assert_ne!(fresh().unwrap(), fresh().unwrap());
    }
}
