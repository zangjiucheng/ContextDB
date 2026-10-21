//! Blob packs (ADR 0008 §17): payloads are appended to `blobs/pack-NNNNNNNN`
//! segments instead of one file each, so a group's payloads cost one append
//! and one ordered sync. A record is `MAGIC | len u32 LE | blake3 | bytes`
//! and is valid exactly when the hash matches its bytes, so a scan needs no
//! trust in lengths: after any invalid header or mismatching hash it resyncs
//! at the next `MAGIC`. Torn tails, garbage and media damage therefore cost
//! only the damaged records. A process only appends to segments it created.
use crate::durability::Durability;
use contextdb_model::*;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};

pub(crate) const MAGIC: [u8; 4] = *b"CXBP";
pub(crate) const HEADER: usize = 4 + 4 + 32;
pub(crate) const PREFIX: &str = "pack-";

#[derive(Clone, Copy, Debug)]
pub(crate) struct Loc {
    segment: u32,
    offset: u64,
    len: u32,
}
/// Shared between the commit actor and reader threads.
#[derive(Debug, Default)]
pub(crate) struct PackIndex {
    pub(crate) segments: Vec<PathBuf>,
    pub(crate) map: HashMap<ContentHash, Loc>,
}
pub(crate) fn is_pack(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .is_some_and(|n| n.len() == 8 && n.bytes().all(|b| b.is_ascii_digit()))
}
pub(crate) fn segment_name(n: usize) -> String {
    format!("{PREFIX}{n:08}")
}
/// Serialises one record.
pub(crate) fn record(bytes: &[u8]) -> Option<(ContentHash, Vec<u8>)> {
    let len = u32::try_from(bytes.len()).ok()?;
    let hash = *blake3::hash(bytes).as_bytes();
    let mut out = Vec::with_capacity(HEADER + bytes.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&hash);
    out.extend_from_slice(bytes);
    Some((hash, out))
}
/// Valid records of one segment, as `(hash, offset of bytes, len)`.
pub(crate) fn parse(data: &[u8]) -> Vec<(ContentHash, u64, u32)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(at) = find(data, pos) {
        let valid = data.get(at + 4..at + HEADER).and_then(|h| {
            let len = u32::from_le_bytes(h[..4].try_into().expect("4 bytes"));
            let hash: ContentHash = h[4..].try_into().expect("32 bytes");
            let body = data.get(at + HEADER..(at + HEADER).checked_add(len as usize)?)?;
            (*blake3::hash(body).as_bytes() == hash).then_some((hash, len))
        });
        match valid {
            Some((hash, len)) => {
                out.push((hash, (at + HEADER) as u64, len));
                pos = at + HEADER + len as usize;
            }
            None => pos = at + 1,
        }
    }
    out
}
fn find(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(MAGIC.len())
        .position(|w| w == MAGIC)
        .map(|p| p + from)
}
/// Indexes every pack segment in `dir` (names already listed).
pub(crate) fn scan(disk: &dyn Durability, dir: &Path, names: &[String]) -> io::Result<PackIndex> {
    let mut index = PackIndex::default();
    let mut packs: Vec<&String> = names.iter().filter(|n| is_pack(n)).collect();
    packs.sort();
    for name in packs {
        let path = dir.join(name);
        let segment = index.segments.len() as u32;
        for (hash, offset, len) in parse(&disk.read(&path)?) {
            index.map.insert(
                hash,
                Loc {
                    segment,
                    offset,
                    len,
                },
            );
        }
        index.segments.push(path);
    }
    Ok(index)
}
impl PackIndex {
    pub(crate) fn insert(&mut self, hash: ContentHash, segment: usize, offset: u64, len: u32) {
        self.map.insert(
            hash,
            Loc {
                segment: segment as u32,
                offset,
                len,
            },
        );
    }
    /// Path and range of a packed payload.
    pub(crate) fn locate(&self, hash: &ContentHash) -> Option<(PathBuf, u64, usize)> {
        let loc = self.map.get(hash)?;
        Some((
            self.segments[loc.segment as usize].clone(),
            loc.offset,
            loc.len as usize,
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_skips_damage_and_resyncs() {
        let (ha, a) = record(b"alpha").unwrap();
        let (hb, b) = record(b"bravo").unwrap();
        let (hc, c) = record(b"").unwrap();
        let mut data = [a.clone(), b.clone(), c.clone()].concat();
        let all = parse(&data);
        assert_eq!(
            all.iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![ha, hb, hc]
        );
        assert_eq!(&data[all[1].1 as usize..][..5], b"bravo");
        // A corrupt length in the first record loses only that record.
        data[5] = 0xFF;
        assert_eq!(
            parse(&data).iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![hb, hc]
        );
        // Flipped payload byte, leading garbage and a torn tail.
        let mut bad = b"\x00garbageCX".to_vec();
        bad.extend_from_slice(&a);
        let mut flipped = b.clone();
        *flipped.last_mut().unwrap() ^= 1;
        bad.extend_from_slice(&flipped);
        bad.extend_from_slice(&c);
        bad.extend_from_slice(&a[..a.len() - 1]);
        assert_eq!(
            parse(&bad).iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![ha, hc]
        );
        // A payload containing MAGIC does not confuse the scan.
        let (hm, m) = record(b"xxCXBPxx").unwrap();
        assert_eq!(
            parse(&[m, a].concat())
                .iter()
                .map(|r| r.0)
                .collect::<Vec<_>>(),
            vec![hm, ha]
        );
        assert!(is_pack("pack-00000001") && !is_pack("pack-1") && !is_pack("pack-0000000x"));
    }
}
