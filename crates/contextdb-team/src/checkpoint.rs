//! The checkpoint (ADR 0018 §8.6, SPEC SD21.13): `ckpt(Σ, g)` as git tree
//! files, and its verifying decoder.
//!
//! - `checkpoint/index` (`CXTK`): accept_version, team id, generation,
//!   sequence, digest, total size, then per chunk BLAKE3, length, items;
//! - `checkpoint/state/<6 digits>`: framed canonical items packed next-fit
//!   into ≤ 1 MiB chunks, never split;
//! - `blobs/<hex>`: each content a non-shredded revision holds, once.
//!
//! Decoding checks every bound before it reads what it bounds: the index
//! size, the chunk count against the index, each chunk's length and hash,
//! every item's size and canonical order, the exact file set, and finally
//! the rebuilt digest and size against the index.

use crate::commit::hex;
use crate::doc::{MAX_STATE_BYTES, TeamDoc, kind_from_code};
use crate::enc::{DecodeError, Reader, Writer, strictly_sorted};
use crate::ops::{Hash, MAX_BLOB_BYTES, RuleSpec, valid_path};
use crate::state::{
    CHUNK_BYTES, INDEX_HEADER, INDEX_PER_CHUNK, KeyedState, RecordItem, RevisionItem,
};
use std::collections::{BTreeMap, BTreeSet};

const MAGIC: &[u8; 4] = b"CXTK";
pub const INDEX_PATH: &str = "checkpoint/index";
/// ≤ 8,193 chunks within 4 GiB (§8.6): the index stays under 512 KiB.
pub const MAX_INDEX_BYTES: usize = 512 * 1024;
pub const MAX_CHUNKS: usize = 8193;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub accept_version: u32,
    pub team: [u8; 16],
    pub generation: u32,
    pub seq: u64,
    pub digest: Hash,
    pub total: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CkptError {
    Missing(String),
    Invalid(String),
}

fn bad<T>(m: impl Into<String>) -> Result<T, CkptError> {
    Err(CkptError::Invalid(m.into()))
}

impl From<DecodeError> for CkptError {
    fn from(e: DecodeError) -> Self {
        Self::Invalid(e.0.into())
    }
}

pub fn chunk_path(i: usize) -> String {
    format!("checkpoint/state/{i:06}")
}

/// `ckpt(Σ, g)`: the files of the checkpoint tree. `blob` returns a
/// content's bytes. The total equals `KeyedState::ckpt_size`.
pub fn encode(
    state: &KeyedState,
    team: [u8; 16],
    generation: u32,
    blob: impl Fn(&Hash) -> Vec<u8>,
) -> Vec<(String, Vec<u8>)> {
    let mut chunks: Vec<(Vec<u8>, u32)> = Vec::new();
    state.for_each_item(|item| {
        let framed = 4 + item.len();
        if chunks.is_empty() || chunks.last().expect("one").0.len() + framed > CHUNK_BYTES {
            chunks.push((Vec::new(), 0));
        }
        let c = chunks.last_mut().expect("one");
        c.0.extend_from_slice(&(item.len() as u32).to_be_bytes());
        c.0.extend_from_slice(item);
        c.1 += 1;
    });
    let mut contents = BTreeSet::new();
    for r in &state.records {
        for v in &r.revisions {
            if !v.shredded {
                contents.insert(v.content);
            }
        }
    }
    let blobs: Vec<(String, Vec<u8>)> = contents
        .iter()
        .map(|h| (format!("blobs/{}", hex(h)), blob(h)))
        .collect();
    let total = INDEX_HEADER
        + INDEX_PER_CHUNK * chunks.len() as u64
        + chunks.iter().map(|c| c.0.len() as u64).sum::<u64>()
        + blobs.iter().map(|b| b.1.len() as u64).sum::<u64>();
    let mut w = Writer::default();
    w.raw(MAGIC)
        .u32(state.doc.accept_version)
        .raw(&team)
        .u32(generation)
        .u64(state.seq)
        .raw(&state.digest())
        .u64(total)
        .count(chunks.len());
    for (bytes, items) in &chunks {
        w.raw(blake3::hash(bytes).as_bytes())
            .u64(bytes.len() as u64)
            .u32(*items);
    }
    let mut files = vec![(INDEX_PATH.to_owned(), w.0)];
    files.extend(
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, (b, _))| (chunk_path(i), b)),
    );
    files.extend(blobs);
    files
}

/// A verified checkpoint: its header, the state, and the blob bytes.
#[derive(Clone, Debug)]
pub struct Decoded {
    pub header: Header,
    pub state: KeyedState,
    pub blobs: BTreeMap<Hash, Vec<u8>>,
}

fn path_item(r: &mut Reader) -> Result<String, CkptError> {
    let p = r.str()?;
    if !valid_path(p) {
        return bad("record path");
    }
    Ok(p.to_owned())
}

/// Decodes and verifies `files` (a flattened tree). `Missing` names an
/// absent file the index requires: the caller fetches it and retries.
pub fn decode(files: &BTreeMap<String, Vec<u8>>) -> Result<Decoded, CkptError> {
    let index = files
        .get(INDEX_PATH)
        .ok_or_else(|| CkptError::Missing(INDEX_PATH.into()))?;
    if index.len() > MAX_INDEX_BYTES {
        return bad("index too large");
    }
    let mut r = Reader(index);
    if r.take(4)? != MAGIC {
        return bad("index magic");
    }
    let header = Header {
        accept_version: r.u32()?,
        team: r.array()?,
        generation: r.u32()?,
        seq: r.u64()?,
        digest: r.array()?,
        total: r.u64()?,
    };
    if header.total > MAX_STATE_BYTES {
        return bad("total over the hard limit");
    }
    let n = r.count(44)?;
    if n == 0 || n > MAX_CHUNKS {
        return bad("chunk count");
    }
    let mut chunk_meta = Vec::with_capacity(n);
    for _ in 0..n {
        let h: Hash = r.array()?;
        let len = r.u64()?;
        let items = r.u32()?;
        if len == 0 || len > CHUNK_BYTES as u64 || items == 0 {
            return bad("chunk bounds");
        }
        chunk_meta.push((h, len as usize, items));
    }
    r.end()?;
    // Items, chunk by chunk.
    let mut items: Vec<Vec<u8>> = Vec::new();
    for (i, (h, len, count)) in chunk_meta.iter().enumerate() {
        let path = chunk_path(i);
        let bytes = files.get(&path).ok_or(CkptError::Missing(path.clone()))?;
        if bytes.len() != *len || blake3::hash(bytes).as_bytes() != h {
            return bad(format!("{path}: length or hash"));
        }
        let mut cr = Reader(bytes);
        let mut seen = 0u32;
        while !cr.0.is_empty() {
            let item = cr.bytes()?;
            if item.is_empty() || item.len() + 4 > CHUNK_BYTES {
                return bad("item size");
            }
            items.push(item.to_vec());
            seen += 1;
        }
        if seen != *count {
            return bad(format!("{path}: item count"));
        }
    }
    let state = parse_items(&items)?;
    // The exact file set: index, chunks, and one blob per needed content.
    let mut needed = BTreeSet::new();
    for rec in &state.records {
        for v in &rec.revisions {
            if !v.shredded {
                needed.insert(v.content);
            }
        }
    }
    let mut blobs = BTreeMap::new();
    for h in &needed {
        let path = format!("blobs/{}", hex(h));
        let b = files.get(&path).ok_or(CkptError::Missing(path.clone()))?;
        if b.len() > MAX_BLOB_BYTES || blake3::hash(b).as_bytes() != h {
            return bad(format!("{path}: bytes"));
        }
        blobs.insert(*h, b.clone());
    }
    let expected_files = 1 + n + needed.len();
    if files.len() != expected_files {
        return bad("unexpected files in the checkpoint tree");
    }
    if state.seq != header.seq {
        return bad("sequence differs from the index");
    }
    let size = state.ckpt_size(|h| blobs.get(h).map_or(0, |b| b.len() as u64));
    if size != header.total || header.total > state.doc.state_bytes {
        return bad("size differs from the index or exceeds state_bytes");
    }
    if state.doc.accept_version != header.accept_version || state.doc.id != header.team {
        return bad("document and index disagree");
    }
    if state.digest() != header.digest {
        return bad("digest differs from the index");
    }
    Ok(Decoded {
        header,
        state,
        blobs,
    })
}

/// The inverse of `KeyedState::for_each_item`, with canonical order checks.
fn parse_items(items: &[Vec<u8>]) -> Result<KeyedState, CkptError> {
    let mut it = items.iter().peekable();
    let tag = |b: &Vec<u8>| b.first().copied();
    let doc = match it.next() {
        Some(b) if tag(b) == Some(0) => {
            let mut r = Reader(&b[1..]);
            let d =
                TeamDoc::decode(r.bytes()?).map_err(|e| CkptError::Invalid(format!("{e:?}")))?;
            r.end()?;
            d
        }
        _ => return bad("first item must be the document"),
    };
    let mut records: Vec<RecordItem> = Vec::new();
    while let Some(b) = it.peek().filter(|b| tag(b) == Some(1)) {
        let mut r = Reader(&b[1..]);
        let path = path_item(&mut r)?;
        let lane = r.str()?.to_owned();
        let kind = kind_from_code(r.u8()?).ok_or(CkptError::Invalid("kind".into()))?;
        let n = r.count(4)?;
        let tags: Vec<String> = (0..n)
            .map(|_| r.str().map(str::to_owned))
            .collect::<Result<_, _>>()?;
        if !strictly_sorted(&tags) {
            return bad("tags order");
        }
        let certified = r.bool()?;
        let head: Hash = r.array()?;
        let nrev = r.u32()? as usize;
        r.end()?;
        it.next();
        if nrev == 0 || records.last().is_some_and(|l| l.path >= path) {
            return bad("record order or revisions");
        }
        let mut revisions = Vec::with_capacity(nrev.min(items.len()));
        for _ in 0..nrev {
            match it.next() {
                Some(b) if tag(b) == Some(2) => {
                    let mut r = Reader(&b[1..]);
                    revisions.push(RevisionItem {
                        content: r.array()?,
                        base: r.bool()?,
                        shredded: r.bool()?,
                    });
                    r.end()?;
                }
                _ => return bad("revision item expected"),
            }
        }
        let contents: BTreeSet<Hash> = revisions.iter().map(|v| v.content).collect();
        if contents.len() != revisions.len() || !contents.contains(&head) {
            return bad("revisions must be distinct and hold the head");
        }
        records.push(RecordItem {
            path,
            lane,
            kind,
            tags: tags.into_iter().collect(),
            certified,
            head,
            revisions,
        });
    }
    let mut rules: BTreeMap<RuleSpec, bool> = BTreeMap::new();
    while let Some(b) = it.peek().filter(|b| tag(b) == Some(3)) {
        let mut r = Reader(&b[1..]);
        let spec = RuleSpec::decode_item(&mut r)?;
        let enabled = r.bool()?;
        r.end()?;
        it.next();
        if rules.keys().next_back().is_some_and(|l| *l >= spec) {
            return bad("rule order");
        }
        rules.insert(spec, enabled);
    }
    let mut shred = BTreeSet::new();
    while let Some(b) = it.peek().filter(|b| tag(b) == Some(4)) {
        let mut r = Reader(&b[1..]);
        let h: Hash = r.array()?;
        r.end()?;
        it.next();
        if shred.last().is_some_and(|l| *l >= h) {
            return bad("shred order");
        }
        shred.insert(h);
    }
    let mut accepted = BTreeSet::new();
    while let Some(b) = it.peek().filter(|b| tag(b) == Some(5)) {
        let mut r = Reader(&b[1..]);
        let pair = (r.str()?.to_owned(), r.array()?);
        r.end()?;
        it.next();
        if accepted.last().is_some_and(|l| *l >= pair) {
            return bad("accepted order");
        }
        accepted.insert(pair);
    }
    let seq = match it.next() {
        Some(b) if tag(b) == Some(6) => {
            let mut r = Reader(&b[1..]);
            let s = r.u64()?;
            r.end()?;
            s
        }
        _ => return bad("sequence item expected"),
    };
    if it.next().is_some() {
        return bad("items after the sequence");
    }
    for rec in &records {
        if !doc.lanes.contains_key(&rec.lane) {
            return bad("record in an unknown lane");
        }
        for v in &rec.revisions {
            if v.shredded != shred.contains(&v.content) {
                return bad("shredded bit disagrees with the shred set");
            }
        }
    }
    Ok(KeyedState {
        doc,
        records,
        rules,
        shred,
        accepted,
        seq,
    })
}
