//! Blob packs (ADR 0008 §17): payloads are appended to `blobs/pack-NNNNNNNN`
//! segments instead of one file each, so a group's payloads cost one append
//! and one ordered sync. A record is `MAGIC | len u32 LE | blake3 | bytes`
//! and is valid exactly when the hash matches its bytes, so a scan needs no
//! trust in lengths: after any invalid header or mismatching hash it resyncs
//! at the next `MAGIC`. Torn tails, garbage and media damage therefore cost
//! only the damaged records. A process only appends to segments it created.
//!
//! **Encrypted records (ADR 0017 §8.1, P9).** Segments written since P9
//! start with [`SEGMENT_HEADER`] and hold `"CXBE" | len u32 | content_hash
//! 32 | nonce 24 | XChaCha20-Poly1305(key, nonce, plaintext, aad =
//! content_hash)`, with one random key per content hash in the key log
//! (`keys.rs`). The content hash stays BLAKE3 of the plaintext. A record is
//! valid when it authenticates under its hash's key; one whose key is gone
//! (shredded) is unreadable ciphertext. Plaintext `CXBP` records are still
//! read until `ctx migrate` re-encrypts them.
use crate::durability::Durability;
use crate::keys::Key;
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use contextdb_model::*;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};

pub(crate) const MAGIC: [u8; 4] = *b"CXBP";
pub(crate) const HEADER: usize = 4 + 4 + 32;
pub(crate) const PREFIX: &str = "pack-";
pub(crate) const ENC_MAGIC: [u8; 4] = *b"CXBE";
pub(crate) const NONCE: usize = 24;
pub(crate) const ENC_HEADER: usize = 4 + 4 + 32 + NONCE;
/// Starts every segment written since P9: its records are encrypted.
pub(crate) const SEGMENT_HEADER: [u8; 8] = *b"CXPH\x01\0\0\0";

#[derive(Clone, Copy, Debug)]
pub(crate) struct Loc {
    segment: u32,
    offset: u64,
    len: u32,
    /// Encrypted records: the record's nonce (`len` covers the tag).
    nonce: Option<[u8; NONCE]>,
}
/// Shared between the commit actor and reader threads.
#[derive(Debug, Default)]
pub(crate) struct PackIndex {
    pub(crate) segments: Vec<PathBuf>,
    pub(crate) map: HashMap<ContentHash, Loc>,
    /// The key of every content hash (ADR 0017 §8.2).
    pub(crate) keys: HashMap<ContentHash, Key>,
}
/// Whether a segment's bytes start with the encrypted-segment header.
pub(crate) fn is_encrypted_segment(data: &[u8]) -> bool {
    data.starts_with(&SEGMENT_HEADER)
}
fn cipher(key: &Key) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(&(*key).into())
}
/// Serialises one encrypted record of `bytes` under `key`.
pub(crate) fn sealed(bytes: &[u8], key: &Key) -> Option<(ContentHash, Vec<u8>)> {
    let hash = *blake3::hash(bytes).as_bytes();
    let mut nonce = [0u8; NONCE];
    getrandom::fill(&mut nonce).ok()?;
    let ct = cipher(key)
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: bytes,
                aad: &hash,
            },
        )
        .ok()?;
    let len = u32::try_from(ct.len()).ok()?;
    let mut out = Vec::with_capacity(ENC_HEADER + ct.len());
    out.extend_from_slice(&ENC_MAGIC);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&hash);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Some((hash, out))
}
/// Decrypts a record's ciphertext; `None` when it does not authenticate.
pub(crate) fn open_sealed(
    key: &Key,
    hash: &ContentHash,
    nonce: &[u8; NONCE],
    ct: &[u8],
) -> Option<Vec<u8>> {
    cipher(key)
        .decrypt(&XNonce::from(*nonce), Payload { msg: ct, aad: hash })
        .ok()
}
/// Valid records of one segment, plaintext and encrypted, as `(hash,
/// offset of the bytes or ciphertext, len, nonce)`. An encrypted record is
/// valid when it authenticates under `key(hash)`.
pub(crate) fn parse_all(
    data: &[u8],
    key: &dyn Fn(&ContentHash) -> Option<Key>,
) -> Vec<(ContentHash, u64, u32, Option<[u8; NONCE]>)> {
    let mut out = Vec::new();
    let mut pos = if is_encrypted_segment(data) {
        SEGMENT_HEADER.len()
    } else {
        0
    };
    while let Some((at, enc)) = find_any(data, pos) {
        if !enc {
            let valid = data.get(at + 4..at + HEADER).and_then(|h| {
                let len = u32::from_le_bytes(h[..4].try_into().expect("4 bytes"));
                let hash: ContentHash = h[4..].try_into().expect("32 bytes");
                let body = data.get(at + HEADER..(at + HEADER).checked_add(len as usize)?)?;
                (*blake3::hash(body).as_bytes() == hash).then_some((hash, len))
            });
            match valid {
                Some((hash, len)) => {
                    out.push((hash, (at + HEADER) as u64, len, None));
                    pos = at + HEADER + len as usize;
                }
                None => pos = at + 1,
            }
            continue;
        }
        let valid = data.get(at + 4..at + ENC_HEADER).and_then(|h| {
            let len = u32::from_le_bytes(h[..4].try_into().expect("4 bytes"));
            let hash: ContentHash = h[4..36].try_into().expect("32 bytes");
            let nonce: [u8; NONCE] = h[36..].try_into().expect("24 bytes");
            let ct = data.get(at + ENC_HEADER..(at + ENC_HEADER).checked_add(len as usize)?)?;
            open_sealed(&key(&hash)?, &hash, &nonce, ct)?;
            Some((hash, len, nonce))
        });
        match valid {
            Some((hash, len, nonce)) => {
                out.push((hash, (at + ENC_HEADER) as u64, len, Some(nonce)));
                pos = at + ENC_HEADER + len as usize;
            }
            None => pos = at + 1,
        }
    }
    out
}
/// The next plaintext or encrypted record magic at or after `from`.
fn find_any(data: &[u8], from: usize) -> Option<(usize, bool)> {
    data.get(from..)?
        .windows(4)
        .position(|w| w == MAGIC || w == ENC_MAGIC)
        .map(|p| (p + from, data[p + from..p + from + 4] == ENC_MAGIC))
}
/// Where a payload is, and how to open it.
pub(crate) struct Located {
    pub(crate) path: PathBuf,
    pub(crate) offset: u64,
    pub(crate) len: usize,
    pub(crate) sealed: Option<([u8; NONCE], Option<Key>)>,
}
pub(crate) fn is_pack(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .is_some_and(|n| n.len() == 8 && n.bytes().all(|b| b.is_ascii_digit()))
}
pub(crate) fn segment_name(n: usize) -> String {
    format!("{PREFIX}{n:08}")
}
/// Serialises one record.
#[cfg(test)]
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
/// Valid plaintext records of one segment, as `(hash, offset of bytes,
/// len)`.
#[cfg(test)]
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
#[cfg(test)]
fn find(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(MAGIC.len())
        .position(|w| w == MAGIC)
        .map(|p| p + from)
}
/// Indexes every pack segment in `dir` (names already listed), with the
/// key log's keys.
pub(crate) fn scan(
    disk: &dyn Durability,
    dir: &Path,
    names: &[String],
    keys: HashMap<ContentHash, Key>,
) -> io::Result<PackIndex> {
    let mut index = PackIndex {
        keys,
        ..PackIndex::default()
    };
    let mut packs: Vec<&String> = names.iter().filter(|n| is_pack(n)).collect();
    packs.sort();
    for name in packs {
        let path = dir.join(name);
        let segment = index.segments.len() as u32;
        let keys = &index.keys;
        for (hash, offset, len, nonce) in parse_all(&disk.read(&path)?, &|h| keys.get(h).copied()) {
            index.map.insert(
                hash,
                Loc {
                    segment,
                    offset,
                    len,
                    nonce,
                },
            );
        }
        index.segments.push(path);
    }
    Ok(index)
}
impl PackIndex {
    pub(crate) fn insert(
        &mut self,
        hash: ContentHash,
        segment: usize,
        offset: u64,
        len: u32,
        nonce: Option<[u8; NONCE]>,
    ) {
        self.map.insert(
            hash,
            Loc {
                segment: segment as u32,
                offset,
                len,
                nonce,
            },
        );
    }
    /// Path and range of a packed payload, with the nonce and key of an
    /// encrypted record (`None` key: the key is gone).
    pub(crate) fn locate(&self, hash: &ContentHash) -> Option<Located> {
        let loc = self.map.get(hash)?;
        Some(Located {
            path: self.segments[loc.segment as usize].clone(),
            offset: loc.offset,
            len: loc.len as usize,
            sealed: loc.nonce.map(|n| (n, self.keys.get(hash).copied())),
        })
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
