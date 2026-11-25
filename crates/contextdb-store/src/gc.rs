//! Payload GC (ADR 0019 §5): the ledger, phase 1's plan and the reports.
//!
//! - **Ledger.** `gc/ledger` is append-only; each record is `"CXGL" |
//!   content 32 | key segment u32 | key offset u64 | gc_generation u64 |
//!   cutoff u64 | now u64 | check 8` (`check` is BLAKE3 of the rest,
//!   truncated). A scan resyncs at the next magic after any invalid record,
//!   as the key log does. An entry names the key-log position of the key it
//!   collected, so a key staged again later (a new position) stays live.
//! - **Phase 1** ([`gc_plan`]) runs on a published snapshot outside the
//!   actor: the keep sets K1–K3 and the candidates. Phase 2 is
//!   [`Store::gc_apply`](crate::Store::gc_apply).
use crate::{
    durability::Durability,
    keys::KeyPos,
    log::LogRow,
    store::{LogReader, Payload, StoreError},
};
use contextdb_core::Kernel;
use contextdb_model::*;
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
};

pub(crate) const DIR: &str = "gc";
const FILE: &str = "ledger";
const MAGIC: [u8; 4] = *b"CXGL";
pub(crate) const RECORD: usize = 4 + 32 + 4 + 8 + 8 + 8 + 8 + 8;

/// One collected payload (ADR 0019 §5.2 step 3a).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedgerEntry {
    pub content: ContentHash,
    /// The live key's position when it was collected.
    pub key_pos: KeyPos,
    pub gc_generation: u64,
    pub cutoff: Epoch,
    pub now: Epoch,
}
fn check(bytes: &[u8]) -> [u8; 8] {
    blake3::hash(bytes).as_bytes()[..8]
        .try_into()
        .expect("8 bytes")
}
pub(crate) fn record(e: &LedgerEntry) -> Vec<u8> {
    let mut out = Vec::with_capacity(RECORD);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&e.content);
    out.extend_from_slice(&e.key_pos.segment.to_le_bytes());
    out.extend_from_slice(&e.key_pos.offset.to_le_bytes());
    out.extend_from_slice(&e.gc_generation.to_le_bytes());
    out.extend_from_slice(&e.cutoff.0.to_le_bytes());
    out.extend_from_slice(&e.now.0.to_le_bytes());
    let c = check(&out[4..]);
    out.extend_from_slice(&c);
    out
}
/// Valid entries, in order.
pub(crate) fn parse(data: &[u8]) -> Vec<LedgerEntry> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(at) = data
        .get(pos..)
        .and_then(|d| d.windows(4).position(|w| w == MAGIC))
        .map(|p| p + pos)
    {
        match data.get(at..at + RECORD) {
            Some(r) if check(&r[4..RECORD - 8]) == r[RECORD - 8..] => {
                let word = |at: usize| u64::from_le_bytes(r[at..at + 8].try_into().expect("8"));
                out.push(LedgerEntry {
                    content: r[4..36].try_into().expect("32 bytes"),
                    key_pos: KeyPos {
                        segment: u32::from_le_bytes(r[36..40].try_into().expect("4")),
                        offset: word(40),
                    },
                    gc_generation: word(48),
                    cutoff: Epoch(word(56)),
                    now: Epoch(word(64)),
                });
                pos = at + RECORD;
            }
            _ => pos = at + 1,
        }
    }
    out
}
pub(crate) fn path(dir: &Path) -> PathBuf {
    dir.join(DIR).join(FILE)
}
/// The loaded ledger.
#[derive(Clone, Debug, Default)]
pub(crate) struct Ledger {
    pub(crate) entries: Vec<LedgerEntry>,
    /// `gc/ledger` exists: key rewrites write kind-2 headers.
    pub(crate) present: bool,
}
impl Ledger {
    /// The newest GC generation (0 without entries).
    pub(crate) fn generation(&self) -> u64 {
        (self.entries.iter())
            .map(|e| e.gc_generation)
            .max()
            .unwrap_or(0)
    }
    pub(crate) fn contents(&self) -> BTreeSet<ContentHash> {
        self.entries.iter().map(|e| e.content).collect()
    }
    /// Every collected key, as `(content, position)`.
    pub(crate) fn named(&self) -> std::collections::HashSet<(ContentHash, KeyPos)> {
        self.entries
            .iter()
            .map(|e| (e.content, e.key_pos))
            .collect()
    }
}
pub(crate) fn load(disk: &dyn Durability, dir: &Path) -> io::Result<Ledger> {
    let p = path(dir);
    if !disk.exists(&p) {
        return Ok(Ledger::default());
    }
    Ok(Ledger {
        entries: parse(&disk.read(&p)?),
        present: true,
    })
}
/// The highest cutoff any collection used: at an epoch below it, a
/// collected content may have been live (ADR 0019 §5.5). Read from the
/// ledger alone, so it works on a damaged log.
pub fn max_cutoff(disk: &dyn Durability, dir: &Path) -> Result<Option<Epoch>, StoreError> {
    let ledger = load(disk, dir).map_err(|e| StoreError::Io(e.to_string()))?;
    Ok(ledger.entries.iter().map(|e| e.cutoff).max())
}

/// Decodes a document payload and returns the content hashes inside it
/// (the daemon passes the `ReviewDoc.meta` decoder).
pub type Nested<'a> = dyn Fn(&[u8]) -> Vec<ContentHash> + 'a;

/// A payload phase 1 found collectable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcCandidate {
    pub content: ContentHash,
    /// Its live key's position at phase 1.
    pub key_pos: KeyPos,
    /// The bytes its pack record occupies.
    pub bytes: u64,
}
/// Phase 1's result (ADR 0019 §5.2), and the dry-run report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcPlan {
    pub e0: Epoch,
    pub cutoff: Epoch,
    pub candidates: Vec<GcCandidate>,
    /// Distinct contents in K1, K2 and K3 (the sets overlap).
    pub kept_k1: usize,
    pub kept_k2: usize,
    pub kept_k3: usize,
}
/// What [`Store::gc_apply`](crate::Store::gc_apply) collected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    /// The generation of the entries written (0: nothing collected).
    pub generation: u64,
    pub now: Epoch,
    pub collected: Vec<ContentHash>,
    pub bytes: u64,
    /// Candidates phase 2 kept: named in `(e0, now]`, pinned, or staged
    /// again since phase 1.
    pub rechecked_out: usize,
}
/// What [`compact`] reclaimed (ADR 0019 §5.5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactReport {
    pub segments_rewritten: usize,
    pub records_dropped: usize,
    pub bytes_reclaimed: u64,
    pub ledger_before: usize,
    pub ledger_after: usize,
    pub snapshots_removed: Vec<String>,
}
/// `ctx gc --compact`: the offline disk reclaim, under the store lock.
pub fn compact<D: Durability + Clone>(disk: D, dir: &Path) -> Result<CompactReport, StoreError> {
    crate::store::compact(disk, dir)
}

/// The contents one frame names (ADR 0019 §5.1): revision contents (K3)
/// when `revisions` is set, and its K2 contents (artifact payloads, notes,
/// metas) always.
#[derive(Debug, Default)]
pub(crate) struct Named {
    pub(crate) revisions: BTreeSet<ContentHash>,
    pub(crate) documents: BTreeSet<ContentHash>,
}
fn decoded(e: WireError) -> StoreError {
    StoreError::Io(format!("gc: {e}"))
}
fn unknown(what: &str) -> StoreError {
    StoreError::Io(format!("gc: {what} not in the kernel"))
}
/// A global revision's content, on `branch` or any branch that sees it.
fn content_of(k: &Kernel, branch: BranchId, r: RevisionId) -> Result<ContentHash, StoreError> {
    (k.branch(branch).and_then(|v| v.revision_content(r).ok()))
        .or_else(|| {
            k.branches()
                .find_map(|b| k.branch(b.id)?.revision_content(r).ok())
        })
        .ok_or_else(|| unknown("revision"))
}
impl Named {
    pub(crate) fn frame(
        &mut self,
        k: &Kernel,
        row: &LogRow,
        revisions: bool,
    ) -> Result<(), StoreError> {
        let batch = row.batch().map_err(decoded)?;
        let result = row.result().map_err(decoded)?;
        let trailer = row.trailer().map_err(decoded)?;
        let branch = batch.branch;
        for op in &batch.ops {
            match op {
                Op::OpenArtifact { payload_hash, .. } => {
                    self.documents.insert(*payload_hash);
                }
                Op::ResolveArtifact { note: Some(n), .. } => {
                    self.documents.insert(*n);
                }
                Op::RecordValidation { meta: Some(m), .. } => {
                    self.documents.insert(*m);
                }
                _ => {}
            }
        }
        for a in &result.allocations.new_artifacts {
            let view = k.branch(branch).ok_or_else(|| unknown("branch"))?;
            let artifact = view.artifact(*a).ok_or_else(|| unknown("artifact"))?;
            self.documents.insert(artifact.payload_hash);
        }
        if let Some(m) = trailer.as_ref().and_then(|t| t.merge.as_ref()) {
            for c in &m.conflicts {
                self.documents.insert(*blake3::hash(&c.payload).as_bytes());
            }
        }
        if !revisions {
            return Ok(());
        }
        let mut named: Vec<RevisionId> = Vec::new();
        let mut existing = |r: &RevRef| {
            if let RevRef::Existing(r) = r {
                named.push(*r);
            }
        };
        for op in &batch.ops {
            match op {
                Op::CreateLogical { content, .. } | Op::AddRevision { content, .. } => {
                    self.revisions.insert(*content);
                }
                Op::MirrorAttach { head, .. } => {
                    self.revisions.insert(*head);
                }
                Op::SetHead { new, .. } => existing(new),
                Op::AssertBase { rev } | Op::RetractBase { rev } => existing(rev),
                Op::AddRule {
                    conclusion,
                    incidences,
                    ..
                } => {
                    existing(conclusion);
                    incidences.iter().for_each(|(r, _)| existing(r));
                }
                Op::Rebind { from, to, .. } => {
                    existing(from);
                    existing(to);
                }
                _ => {}
            }
        }
        let delta = &result.net_delta;
        named.extend(&result.allocations.new_revisions);
        named.extend(&delta.base_flips);
        for (_, from, to) in &delta.head_moves {
            named.extend(from.iter().chain([to]));
        }
        if let Some(m) = trailer.as_ref().and_then(|t| t.merge.as_ref()) {
            for (_, old, new) in &m.moved_heads {
                named.extend([old, new]);
            }
        }
        for r in named {
            self.revisions.insert(content_of(k, branch, r)?);
        }
        Ok(())
    }
    /// The hashes `nested` finds in each document payload that reads.
    pub(crate) fn nested(
        documents: &BTreeSet<ContentHash>,
        read: impl Fn(&ContentHash) -> Payload,
        nested: &Nested<'_>,
    ) -> BTreeSet<ContentHash> {
        let mut out = BTreeSet::new();
        for d in documents {
            if let Payload::Available(bytes) = read(d) {
                out.extend(nested(&bytes));
            }
        }
        out
    }
}

/// Phase 1 (ADR 0019 §5.2): on `kernel`, a published snapshot at `e0`,
/// and the log through `e0`, the keep sets K1 (live now, every branch and
/// retained sync point), K2 (artifact payloads, current and replaced metas
/// and notes, and what `nested` finds in those documents) and K3 (contents
/// frames in `(cutoff, e0]` name). Candidates are the hashes under a live
/// key in the pack index that none keeps and that are not shredded.
pub fn gc_plan<D: Durability>(
    kernel: &Kernel,
    log: &LogReader<D>,
    e0: Epoch,
    cutoff: Epoch,
    nested: &Nested<'_>,
) -> Result<GcPlan, StoreError> {
    if e0 > log.tail() || kernel.epoch() != e0 || cutoff > e0 {
        return Err(StoreError::PlanOutdated);
    }
    let mut k1 = BTreeSet::new();
    let mut k2 = BTreeSet::new();
    for state in kernel.export().states {
        let i = &state.inputs;
        let content = |r: &RevisionId| i.revisions[r.0 as usize].content;
        k1.extend(i.heads.iter().map(content));
        k1.extend(
            (i.revisions.iter().zip(&i.base))
                .filter(|(_, b)| **b)
                .map(|(r, _)| r.content),
        );
        for rule in &i.rules {
            k1.insert(content(&rule.conclusion));
            k1.extend(rule.premises.iter().map(|p| content(&p.premise)));
            k1.extend(rule.references.iter().map(content));
        }
        k2.extend(i.artifacts.iter().map(|a| a.payload_hash));
        k2.extend(i.validations.values().filter_map(|v| v.meta));
    }
    let mut named = Named::default();
    for e in 1..=e0.0 {
        named.frame(kernel, &log.read_row(Epoch(e))?, e > cutoff.0)?;
    }
    k2.extend(named.documents);
    let k3 = named.revisions;
    let inner = Named::nested(&k2, |h| log.read_blob(h), nested);
    k2.extend(inner);
    let packs = log.packs().read().expect("pack index");
    let mut candidates: Vec<GcCandidate> = (packs.positions.iter())
        .filter(|(h, _)| packs.map.contains_key(*h) && !packs.shredded.contains(*h))
        .filter(|(h, _)| !k1.contains(*h) && !k2.contains(*h) && !k3.contains(*h))
        .map(|(h, at)| GcCandidate {
            content: *h,
            key_pos: *at,
            bytes: packs.record_size(h).unwrap_or(0),
        })
        .collect();
    candidates.sort_by_key(|c| c.content);
    Ok(GcPlan {
        e0,
        cutoff,
        candidates,
        kept_k1: k1.len(),
        kept_k2: k2.len(),
        kept_k3: k3.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: u8) -> LedgerEntry {
        LedgerEntry {
            content: [n; 32],
            key_pos: KeyPos {
                segment: n as u32,
                offset: 21 + 76 * n as u64,
            },
            gc_generation: 3,
            cutoff: Epoch(100),
            now: Epoch(250),
        }
    }
    /// The ledger's decoding (fuzz-style): entries round-trip; every
    /// truncation of the last record and every flipped byte of a record
    /// costs exactly that record; garbage before, between and after
    /// records costs nothing.
    #[test]
    fn ledger_records_round_trip_and_resync() {
        let (a, b) = (entry(1), entry(2));
        let data = [record(&a), record(&b)].concat();
        assert_eq!(record(&a).len(), RECORD);
        assert_eq!(parse(&data), vec![a, b]);
        for n in 0..RECORD {
            let torn = &data[..RECORD + n];
            assert_eq!(parse(torn), vec![a], "torn at {n}");
            let mut bad = data.clone();
            bad[n] ^= 0x40;
            assert_eq!(parse(&bad), vec![b], "byte {n} flipped");
        }
        let mut noisy = b"\0CXGLjunkCXG".to_vec();
        noisy.extend(record(&a));
        noisy.extend(b"CXGL");
        noisy.extend(record(&b));
        noisy.extend(&record(&a)[..40]);
        assert_eq!(parse(&noisy), vec![a, b]);
        // A pseudo-random stream holds no entry.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let junk: Vec<u8> = (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        assert!(parse(&junk).is_empty());
        let ledger = Ledger {
            entries: vec![a, b],
            present: true,
        };
        assert!(ledger.named().contains(&(a.content, a.key_pos)));
        assert!(!ledger.named().contains(&(a.content, b.key_pos)));
        assert_eq!(ledger.generation(), 3);
    }
}
