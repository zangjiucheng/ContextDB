//! Payload GC in the store (ADR 0019 §5.2–§5.5): phase 2, the ledger at
//! open and the offline compaction.
use super::{OpenOptions, Store, StoreError, StorePresence, io, read_payload};
use crate::{
    durability::Durability,
    gc::{self, CompactReport, GcPlan, GcReport, LedgerEntry, Named, Nested},
    pack,
};
use contextdb_model::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const COMPACT_DIR: &str = "compact";

/// Removes what an interrupted compaction or ledger rewrite left:
/// `compact/*.new` and `gc/ledger.tmp`. Open runs it before the sweep of
/// `blobs/` (ADR 0019 §5.5).
pub(super) fn remove_leftovers(disk: &dyn Durability, dir: &Path) -> std::io::Result<()> {
    for (sub, suffix) in [(COMPACT_DIR, ".new"), (gc::DIR, ".tmp")] {
        let d = dir.join(sub);
        if !disk.exists(&d) {
            continue;
        }
        let stale: Vec<String> = (disk.list(&d)?.into_iter())
            .filter(|n| n.ends_with(suffix))
            .collect();
        for n in &stale {
            disk.remove(&d.join(n))?;
        }
        if !stale.is_empty() {
            disk.sync_dir(&d)?;
        }
    }
    Ok(())
}

impl<D: Durability> Store<D> {
    /// Commit-time blob presence with `collected` (ADR 0019 §5.4).
    pub fn presence(&self) -> StorePresence<'_> {
        StorePresence {
            blobs: &self.blobs,
            packs: self.packs.read().expect("pack index"),
        }
    }
    /// The GC ledger's entries, oldest first (for projection purges and
    /// reports).
    pub fn gc_ledger(&self) -> &[LedgerEntry] {
        &self.ledger.entries
    }
    /// Creates `sub` under the store directory, durably, behind the version
    /// barrier (ADR 0019 §4).
    fn format7_dir(&mut self, sub: &str) -> std::io::Result<std::path::PathBuf> {
        self.version_barrier()?;
        let d = self.dir.join(sub);
        if !self.disk.exists(&d) {
            self.disk.create_dir_all(&d)?;
            self.disk.sync_dir(&self.dir)?;
        }
        Ok(d)
    }
    /// Phase 2 of payload GC (ADR 0019 §5.2), one exclusive step:
    /// - settles, then refuses a stale plan (`held`, the oldest snapshot a
    ///   reader holds, is below its cutoff) with `PlanOutdated`;
    /// - drops every candidate named in `(e0, now]` (K3 and every K2 kind,
    ///   with what `nested` finds), `pinned` (K4), or staged again since
    ///   phase 1 (its live key moved);
    /// - behind the version barrier: (a) appends the ledger entries and
    ///   syncs them, (b) marks them collected for every reader, (c)
    ///   rewrites the key log without their keys.
    ///
    /// An error before the first ledger byte changes nothing; any error
    /// from then on poisons the store, and reopen applies whatever complete
    /// entries are on disk. Projections purge after it from the report
    /// (`VectorStore::collect`).
    pub fn gc_apply(
        &mut self,
        plan: &GcPlan,
        pinned: &BTreeSet<ContentHash>,
        held: Epoch,
        nested: &Nested<'_>,
    ) -> Result<GcReport, StoreError> {
        self.usable()?;
        self.settle()?;
        let now = self.epoch();
        if held < plan.cutoff || plan.e0 > now {
            return Err(StoreError::PlanOutdated);
        }
        let mut named = Named::default();
        for e in plan.e0.0 + 1..=now.0 {
            named.frame(&self.db, &self.read_frame(Epoch(e))?.0, true)?;
        }
        let (disk, dir, packs) = (&self.disk, &self.dir, &self.packs);
        let inner = Named::nested(
            &named.documents,
            |h| read_payload(disk, dir, packs, h),
            nested,
        );
        let kept = |h: &ContentHash| {
            pinned.contains(h)
                || named.revisions.contains(h)
                || named.documents.contains(h)
                || inner.contains(h)
        };
        let collect: Vec<gc::GcCandidate> = {
            let packs = self.packs.read().expect("pack index");
            (plan.candidates.iter())
                .filter(|c| !kept(&c.content) && !packs.shredded.contains(&c.content))
                .filter(|c| {
                    packs.map.contains_key(&c.content)
                        && packs.positions.get(&c.content) == Some(&c.key_pos)
                })
                .copied()
                .collect()
        };
        let mut report = GcReport {
            now,
            rechecked_out: plan.candidates.len() - collect.len(),
            ..GcReport::default()
        };
        if collect.is_empty() {
            return Ok(report);
        }
        let gc_dir = self.format7_dir(gc::DIR).map_err(io)?;
        let generation = self.ledger.generation().max(self.key_collect) + 1;
        let entries: Vec<LedgerEntry> = (collect.iter())
            .map(|c| LedgerEntry {
                content: c.content,
                key_pos: c.key_pos,
                gc_generation: generation,
                cutoff: plan.cutoff,
                now,
            })
            .collect();
        // (a) From the first ledger byte on, any error poisons.
        let path = gc::path(&self.dir);
        let bytes: Vec<u8> = entries.iter().flat_map(gc::record).collect();
        self.in_flight = true;
        let appended = (self.disk.append(&path, &bytes))
            .and_then(|()| self.disk.sync(&path))
            .and_then(|()| self.disk.sync_dir(&gc_dir));
        self.ledger.present = true;
        self.ledger.entries.extend(&entries);
        if appended.is_err() {
            self.in_flight = false;
            return Err(self.poison());
        }
        // (b) Readers see `Collected` from here on.
        {
            let mut packs = self.packs.write().expect("pack index");
            for e in &entries {
                packs.collected.insert(e.content);
                packs.map.remove(&e.content);
                packs.keys.remove(&e.content);
                packs.positions.remove(&e.content);
                self.blobs.remove(&e.content);
            }
        }
        // (c)
        let rewritten = self.rewrite_keys();
        self.in_flight = false;
        if rewritten.is_err() {
            return Err(self.poison());
        }
        report.generation = generation;
        report.bytes = collect.iter().map(|c| c.bytes).sum();
        report.collected = collect.iter().map(|c| c.content).collect();
        Ok(report)
    }
    /// The compaction of an open store (see [`compact`]).
    fn compact_packs(&mut self) -> Result<CompactReport, StoreError> {
        self.usable()?;
        self.settle()?;
        let compact_dir = self.format7_dir(COMPACT_DIR).map_err(io)?;
        let blob_dir = self.blob_dir();
        let mut report = CompactReport {
            ledger_before: self.ledger.entries.len(),
            ..CompactReport::default()
        };
        let mut dead: BTreeSet<ContentHash> = self.ledger.contents();
        dead.extend(self.db.shredded().keys());
        let segments = self.packs.read().expect("pack index").segments.clone();
        for path in segments {
            let data = self.disk.read(&path).map_err(io)?;
            let (ranges, valid) = {
                let packs = self.packs.read().expect("pack index");
                let key = |h: &ContentHash| packs.keys.get(h).copied();
                let ranges = pack::dead_records(&data, &|h| dead.contains(h), &key);
                let valid: Vec<ContentHash> =
                    (pack::parse_all(&data, &key).iter()).map(|r| r.0).collect();
                (ranges, valid)
            };
            if ranges.is_empty() {
                continue;
            }
            let mut kept = Vec::with_capacity(data.len());
            let mut from = 0;
            for (start, end) in &ranges {
                kept.extend_from_slice(&data[from..*start]);
                from = *end;
            }
            kept.extend_from_slice(&data[from..]);
            let name = path.file_name().expect("segment name").to_string_lossy();
            let tmp = compact_dir.join(format!("{name}.new"));
            self.disk.write_new(&tmp, &kept).map_err(io)?;
            let back = self.disk.read(&tmp).map_err(io)?;
            let still: Vec<ContentHash> = {
                let packs = self.packs.read().expect("pack index");
                (pack::parse_all(&back, &|h| packs.keys.get(h).copied()).iter())
                    .map(|r| r.0)
                    .collect()
            };
            if back != kept || still != valid {
                return Err(StoreError::Io(format!("compact: {name} did not verify")));
            }
            self.disk.sync(&tmp).map_err(io)?;
            self.disk.rename(&tmp, &path).map_err(io)?;
            self.disk.sync_dir(&blob_dir).map_err(io)?;
            self.disk.sync_dir(&compact_dir).map_err(io)?;
            report.segments_rewritten += 1;
            report.records_dropped += ranges.len();
            report.bytes_reclaimed += (data.len() - kept.len()) as u64;
        }
        if self.ledger.present {
            self.rewrite_ledger().map_err(io)?;
        }
        report.ledger_after = self.ledger.entries.len();
        let checksums = (1..=self.epoch().0)
            .map(|e| self.checksum_at(Epoch(e)))
            .collect::<Result<Vec<_>, _>>()?;
        report.snapshots_removed =
            crate::snapshot::prune(&self.disk, &self.dir, &checksums, 2, |(_, k)| {
                contextdb_core::Kernel::<contextdb_core::Counting>::import(k).is_ok()
            })
            .map_err(io)?;
        Ok(report)
    }
    /// A new ledger generation (ADR 0019 §5.5): the newest entry of each
    /// hash that still has no live record; hashes staged again drop out.
    fn rewrite_ledger(&mut self) -> std::io::Result<()> {
        let mut newest: BTreeMap<ContentHash, LedgerEntry> = BTreeMap::new();
        {
            let packs = self.packs.read().expect("pack index");
            for e in &self.ledger.entries {
                if !packs.map.contains_key(&e.content) {
                    newest.insert(e.content, *e);
                }
            }
        }
        let entries: Vec<LedgerEntry> = newest.into_values().collect();
        let path = gc::path(&self.dir);
        let tmp = path.with_extension("tmp");
        let bytes: Vec<u8> = entries.iter().flat_map(gc::record).collect();
        self.disk.write_new(&tmp, &bytes)?;
        self.disk.sync(&tmp)?;
        self.disk.rename(&tmp, &path)?;
        self.disk.sync_dir(&self.dir.join(gc::DIR))?;
        self.ledger.entries = entries;
        Ok(())
    }
}

/// `ctx gc --compact` (ADR 0019 §5.5), offline under the store lock:
/// - rewrites each pack segment holding a record that does not
///   authenticate under its hash's live key and whose hash is shredded or
///   collected, through `compact/pack-N.new` (written, read back and
///   verified, synced, renamed over `blobs/pack-N`, both directories
///   synced);
/// - rewrites `gc/ledger` as a new generation;
/// - removes snapshots that no longer load, keeping the newest two valid;
/// - then checks PROP10: the reopened kernel equals a full replay.
pub(crate) fn compact<D: Durability + Clone>(
    disk: D,
    dir: &Path,
) -> Result<CompactReport, StoreError> {
    let options = OpenOptions {
        create: false,
        ..OpenOptions::default()
    };
    let report = Store::open(disk.clone(), dir, &options)?
        .0
        .compact_packs()?;
    let (store, _) = Store::open(disk, dir, &options)?;
    let replayed = store.log_reader().replay_kernel_to(store.epoch())?;
    let digests = |k: &contextdb_core::Kernel| -> Vec<ContentHash> {
        k.branches()
            .map(|b| k.branch(b.id).expect("listed").digest())
            .collect()
    };
    if digests(store.kernel()?) != digests(&replayed) {
        return Err(StoreError::ReplayDivergence {
            epoch: store.epoch(),
        });
    }
    Ok(report)
}
