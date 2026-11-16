//! The keyed team state as canonical items (ADR 0018 §4.3, §8.6; SPEC
//! SD21.5–6, SD21.13): one encoding feeds the team digest, the checkpoint's
//! chunks and the exact `ckpt_size` that `TeamStateLimit` checks. Items name
//! nothing by replica id, so a full-replay edge and a checkpoint joiner
//! produce the same bytes.

use crate::doc::{TeamDoc, kind_code};
use crate::enc::Writer;
use crate::ops::{Hash, RuleSpec};
use contextdb_model::RecordKind;
use std::collections::{BTreeMap, BTreeSet};

/// A chunk of `checkpoint/state/` holds framed items up to this size.
pub const CHUNK_BYTES: usize = 1 << 20;
/// `checkpoint/index`: fixed header, then 44 bytes per chunk.
pub const INDEX_HEADER: u64 = 4 + 4 + 16 + 4 + 8 + 32 + 8 + 4;
pub const INDEX_PER_CHUNK: u64 = 32 + 8 + 4;
/// Longest member name: the bound of the `Compact` reserve (§8.6).
pub const RESERVE_MEMBER: usize = crate::doc::MAX_NAME;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionItem {
    pub content: Hash,
    pub base: bool,
    pub shredded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordItem {
    pub path: String,
    pub lane: String,
    pub kind: RecordKind,
    pub tags: BTreeSet<String>,
    pub certified: bool,
    pub head: Hash,
    /// In creation order.
    pub revisions: Vec<RevisionItem>,
}

/// The whole keyed state, in canonical order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyedState {
    pub doc: TeamDoc,
    /// Sorted by path.
    pub records: Vec<RecordItem>,
    pub rules: BTreeMap<RuleSpec, bool>,
    pub shred: BTreeSet<Hash>,
    pub accepted: BTreeSet<(String, [u8; 16])>,
    pub seq: u64,
}

impl KeyedState {
    /// Visits every item's canonical bytes in digest order.
    pub fn for_each_item(&self, mut f: impl FnMut(&[u8])) {
        let mut w = Writer::default();
        let mut emit = |w: &mut Writer| {
            f(&w.0);
            w.0.clear();
        };
        w.u8(0).bytes(&self.doc.encode());
        emit(&mut w);
        for r in &self.records {
            w.u8(1).str(&r.path).str(&r.lane);
            w.u8(kind_code(r.kind).expect("team kind"));
            w.count(r.tags.len());
            for t in &r.tags {
                w.str(t);
            }
            w.bool(r.certified).raw(&r.head).count(r.revisions.len());
            emit(&mut w);
            for v in &r.revisions {
                w.u8(2).raw(&v.content).bool(v.base).bool(v.shredded);
                emit(&mut w);
            }
        }
        for (rule, enabled) in &self.rules {
            w.u8(3);
            rule.encode(&mut w);
            w.bool(*enabled);
            emit(&mut w);
        }
        for h in &self.shred {
            w.u8(4).raw(h);
            emit(&mut w);
        }
        for (m, id) in &self.accepted {
            w.u8(5).str(m).raw(id);
            emit(&mut w);
        }
        w.u8(6).u64(self.seq);
        emit(&mut w);
    }

    /// The team digest: BLAKE3 over `CXTS` and every framed item.
    pub fn digest(&self) -> Hash {
        let mut h = blake3::Hasher::new();
        h.update(b"CXTS");
        self.for_each_item(|item| {
            h.update(&(item.len() as u32).to_be_bytes());
            h.update(item);
        });
        *h.finalize().as_bytes()
    }

    /// The exact size of `ckpt(self, g)` for any `g`: the index, the chunks
    /// (framed items packed next-fit into ≤ 1 MiB) and the distinct contents
    /// of non-shredded revisions, given their sizes.
    pub fn ckpt_size(&self, blob_size: impl Fn(&Hash) -> u64) -> u64 {
        let mut chunks = 0u64;
        let mut current = 0usize;
        let mut total = 0u64;
        self.for_each_item(|item| {
            let framed = 4 + item.len();
            if chunks == 0 || current + framed > CHUNK_BYTES {
                chunks += 1;
                current = 0;
            }
            current += framed;
            total += framed as u64;
        });
        let mut blobs = BTreeSet::new();
        for r in &self.records {
            for v in &r.revisions {
                if !v.shredded {
                    blobs.insert(v.content);
                }
            }
        }
        let blob_bytes: u64 = blobs.iter().map(&blob_size).sum();
        INDEX_HEADER + INDEX_PER_CHUNK * chunks + total + blob_bytes
    }

    /// `ckpt_size` plus the reserve for one future `Compact` (§8.6): its
    /// accepted-id item at the longest member name, and one index entry.
    /// Next-fit packing gains at most one chunk when one item is inserted
    /// anywhere in the sequence, so whatever id and member the `Compact`
    /// has, `ckpt_size(Σc)` ≤ this bound.
    pub fn ckpt_size_reserved(&self, blob_size: impl Fn(&Hash) -> u64) -> u64 {
        self.ckpt_size(blob_size) + reserve_item_bytes() as u64 + INDEX_PER_CHUNK
    }
}

/// A framed accepted-id item for a member name of maximal length.
pub fn reserve_item_bytes() -> usize {
    4 + 1 + 4 + RESERVE_MEMBER + 16
}
