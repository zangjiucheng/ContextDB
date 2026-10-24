use contextdb_model::*;
use std::collections::BTreeMap;

const EMPTY: u32 = u32::MAX;
fn key_hash(key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    // Fixed keys: deterministic across runs and platforms of equal word size.
    let mut h = std::hash::DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}
fn key_at<'a>(keys: &'a str, ends: &[u64], n: usize) -> &'a str {
    let start = if n == 0 { 0 } else { ends[n - 1] as usize };
    &keys[start..ends[n] as usize]
}
/// Open-addressing key → LogicalId table (ADR 0007). Slots hold ids only;
/// probes compare against the key buffer. Load factor stays at most 3/4, so a
/// probe meets an empty slot within `len + 1` steps.
#[derive(Clone, Debug, Default)]
struct KeyTable {
    slots: Vec<u32>,
    len: usize,
}
impl KeyTable {
    fn find(&self, hash: u64, eq: impl Fn(u32) -> bool) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut n = hash as usize & mask;
        for _ in 0..=self.len {
            match self.slots[n] {
                EMPTY => return None,
                id if eq(id) => return Some(id),
                _ => n = (n + 1) & mask,
            }
        }
        unreachable!("load factor keeps an empty slot")
    }
    fn insert(&mut self, hash: u64, id: u32, rehash: impl Fn(u32) -> u64) {
        if (self.len + 1) * 4 > self.slots.len() * 3 {
            let old = std::mem::take(&mut self.slots);
            self.slots = vec![EMPTY; (old.len() * 2).max(16)];
            for id in old.into_iter().filter(|id| *id != EMPTY) {
                self.place(rehash(id), id);
            }
        }
        self.place(hash, id);
        self.len += 1;
    }
    /// Removes `id` (stored under `hash`), shifting later probe-chain
    /// entries back so every remaining key stays reachable.
    fn remove(&mut self, hash: u64, id: u32, rehash: impl Fn(u32) -> u64) {
        let mask = self.slots.len() - 1;
        let mut hole = hash as usize & mask;
        while self.slots[hole] != id {
            crate::invariant_assert!(self.slots[hole] != EMPTY, "removed key is present");
            hole = (hole + 1) & mask;
        }
        self.slots[hole] = EMPTY;
        self.len -= 1;
        let mut n = (hole + 1) & mask;
        while self.slots[n] != EMPTY {
            let home = rehash(self.slots[n]) as usize & mask;
            // Move back when the hole lies cyclically within [home, n).
            if (n.wrapping_sub(home) & mask) >= (n.wrapping_sub(hole) & mask) {
                self.slots[hole] = self.slots[n];
                self.slots[n] = EMPTY;
                hole = n;
            }
            n = (n + 1) & mask;
        }
    }
    fn place(&mut self, hash: u64, id: u32) {
        let mask = self.slots.len() - 1;
        let mut n = hash as usize & mask;
        for _ in 0..=self.len {
            if self.slots[n] == EMPTY {
                self.slots[n] = id;
                return;
            }
            n = (n + 1) & mask;
        }
        unreachable!("load factor keeps an empty slot")
    }
}
/// Append-only logical columns: keys share one buffer, scope lanes are interned,
/// and only non-empty metadata is stored.
#[derive(Clone, Debug, Default)]
pub(crate) struct LogicalArena {
    // u64: total key bytes may exceed u32 (256-byte keys × u32 logicals).
    key_end: Vec<u64>,
    keys: String,
    kind: Vec<RecordKind>,
    scope: Vec<u32>,
    scopes: Vec<String>,
    scope_ids: BTreeMap<String, u32>,
    owner: Vec<PrincipalId>,
    branch: Vec<BranchId>,
    epoch: Vec<Epoch>,
    metadata: BTreeMap<LogicalId, Metadata>,
    keys_by_hash: KeyTable,
}
impl LogicalArena {
    pub fn retained_bytes(&self) -> usize {
        use crate::memory::{btree_slots, vec_bytes as b};
        use std::mem::size_of;
        b(&self.key_end)
            + b(&self.keys_by_hash.slots)
            + self.keys.capacity()
            + b(&self.kind)
            + b(&self.scope)
            + b(&self.scopes)
            + self.scopes.iter().map(String::capacity).sum::<usize>()
            + b(&self.owner)
            + b(&self.branch)
            + b(&self.epoch)
            // Interning map: key header + id per slot, plus a second copy of the lane text.
            + btree_slots(self.scope_ids.len()) * (size_of::<String>() + size_of::<u32>())
            + self.scope_ids.keys().map(String::capacity).sum::<usize>()
            // Lower bound: B-tree leaves store keys and values in separate arrays.
            + btree_slots(self.metadata.len()) * (size_of::<LogicalId>() + size_of::<Metadata>())
            + self
                .metadata
                .values()
                .map(|m| {
                    (btree_slots(m.tags.len()) + btree_slots(m.dirs.len())) * size_of::<String>()
                        + m.tags.iter().chain(&m.dirs).map(String::capacity).sum::<usize>()
                })
                .sum::<usize>()
    }
    pub fn len(&self) -> usize {
        self.kind.len()
    }
    pub fn kind(&self, l: LogicalId) -> RecordKind {
        self.kind[l.index()]
    }
    pub fn export_key(&self, l: LogicalId) -> String {
        self.key(l.index()).to_owned()
    }
    pub fn scope_lane(&self, l: LogicalId) -> &str {
        &self.scopes[self.scope[l.index()] as usize]
    }
    fn key(&self, n: usize) -> &str {
        key_at(&self.keys, &self.key_end, n)
    }
    pub fn find_key(&self, key: &str) -> Option<LogicalId> {
        self.keys_by_hash
            .find(key_hash(key), |id| self.key(id as usize) == key)
            .map(LogicalId)
    }
    pub fn reserve(&mut self, new: &[Logical]) {
        let n = new.len();
        let bytes: usize = new.iter().map(|l| l.key.len()).sum();
        if let Some(n) = crate::memory::growth(self.keys.len(), self.keys.capacity(), bytes) {
            self.keys.reserve_exact(n);
        }
        crate::memory::grow(&mut self.key_end, n);
        crate::memory::grow(&mut self.kind, n);
        crate::memory::grow(&mut self.scope, n);
        crate::memory::grow(&mut self.owner, n);
        crate::memory::grow(&mut self.branch, n);
        crate::memory::grow(&mut self.epoch, n);
    }
    pub fn append(&mut self, l: &Logical) {
        let id = u32::try_from(self.kind.len()).expect("bounded logical ids");
        self.keys.push_str(&l.key);
        self.key_end.push(self.keys.len() as u64);
        let (buffer, ends) = (&self.keys, &self.key_end);
        self.keys_by_hash.insert(key_hash(&l.key), id, |old| {
            key_hash(key_at(buffer, ends, old as usize))
        });
        self.kind.push(l.kind);
        let next = u32::try_from(self.scopes.len()).expect("lanes bounded by logicals");
        let scope = *self
            .scope_ids
            .entry(l.scope_lane.clone())
            .or_insert_with(|| {
                self.scopes.push(l.scope_lane.clone());
                next
            });
        self.scope.push(scope);
        self.owner.push(l.owner);
        self.branch.push(l.origin_branch);
        self.epoch.push(l.origin_epoch);
    }
    /// Drops logicals `n..` (WHAT_IF rollback): their keys, interned lanes
    /// first used by them, and their metadata.
    pub fn truncate(&mut self, n: usize) {
        for id in (n..self.len()).rev() {
            let (buffer, ends) = (&self.keys, &self.key_end);
            self.keys_by_hash
                .remove(key_hash(self.key(id)), id as u32, |old| {
                    key_hash(key_at(buffer, ends, old as usize))
                });
        }
        if n < self.len() {
            let start = if n == 0 {
                0
            } else {
                self.key_end[n - 1] as usize
            };
            self.keys.truncate(start);
            // Lanes are interned at first use, so logicals `..n` use exactly
            // the lanes `..=max`.
            let lanes = self.scope[..n].iter().max().map_or(0, |m| *m as usize + 1);
            for lane in self.scopes.drain(lanes..) {
                self.scope_ids.remove(&lane);
            }
        }
        self.key_end.truncate(n);
        self.kind.truncate(n);
        self.scope.truncate(n);
        self.owner.truncate(n);
        self.branch.truncate(n);
        self.epoch.truncate(n);
        self.metadata.retain(|l, _| l.index() < n);
    }
    pub fn record(&self, l: LogicalId) -> Logical {
        let n = l.index();
        Logical {
            key: self.key(n).to_owned(),
            kind: self.kind[n],
            scope_lane: self.scopes[self.scope[n] as usize].clone(),
            owner: self.owner[n],
            origin_branch: self.branch[n],
            origin_epoch: self.epoch[n],
        }
    }
    pub fn metadata(&self, l: LogicalId) -> Metadata {
        self.metadata.get(&l).cloned().unwrap_or_default()
    }
    /// Sets one logical's metadata; defaults are not stored.
    pub fn put_metadata(&mut self, l: LogicalId, m: Metadata) {
        if m == Metadata::default() {
            self.metadata.remove(&l);
        } else {
            self.metadata.insert(l, m);
        }
    }
    pub fn set_metadata(&mut self, all: &[Metadata]) {
        self.metadata = all
            .iter()
            .enumerate()
            .filter(|(_, m)| **m != Metadata::default())
            .map(|(n, m)| (LogicalId(n as u32), m.clone()))
            .collect();
    }
    pub fn export(&self) -> (Vec<Logical>, Vec<Metadata>) {
        let logicals = (0..self.len())
            .map(|n| Logical {
                key: self.key(n).to_owned(),
                kind: self.kind[n],
                scope_lane: self.scopes[self.scope[n] as usize].clone(),
                owner: self.owner[n],
                origin_branch: self.branch[n],
                origin_epoch: self.epoch[n],
            })
            .collect();
        let metadata = (0..self.len())
            .map(|n| {
                self.metadata
                    .get(&LogicalId(n as u32))
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
        (logicals, metadata)
    }
}

/// Append-only revision columns. Mutable input tables are kept separately.
#[derive(Clone, Debug, Default)]
pub(crate) struct RevisionArena {
    logical: Vec<LogicalId>,
    content: Vec<ContentHash>,
    author: Vec<PrincipalId>,
    branch: Vec<BranchId>,
    epoch: Vec<Epoch>,
}
impl RevisionArena {
    pub fn retained_bytes(&self) -> usize {
        use crate::memory::vec_bytes as b;
        b(&self.logical) + b(&self.content) + b(&self.author) + b(&self.branch) + b(&self.epoch)
    }
    pub fn logical(&self, r: RevisionId) -> LogicalId {
        self.logical[r.index()]
    }
    pub fn content(&self, r: RevisionId) -> ContentHash {
        self.content[r.index()]
    }
    pub fn len(&self) -> usize {
        self.logical.len()
    }
    pub fn reserve(&mut self, n: usize) {
        crate::memory::grow(&mut self.logical, n);
        crate::memory::grow(&mut self.content, n);
        crate::memory::grow(&mut self.author, n);
        crate::memory::grow(&mut self.branch, n);
        crate::memory::grow(&mut self.epoch, n);
    }
    pub fn append(&mut self, r: &Revision) {
        self.logical.push(r.logical);
        self.content.push(r.content);
        self.author.push(r.author);
        self.branch.push(r.origin_branch);
        self.epoch.push(r.origin_epoch);
    }
    pub fn record(&self, r: RevisionId) -> Revision {
        let n = r.index();
        Revision {
            logical: self.logical[n],
            content: self.content[n],
            author: self.author[n],
            origin_branch: self.branch[n],
            origin_epoch: self.epoch[n],
        }
    }
    /// The kernel's cross-branch dedup: a branch's copy of an imported
    /// revision carries the original's provenance.
    pub fn set_origin(
        &mut self,
        r: RevisionId,
        author: PrincipalId,
        branch: BranchId,
        epoch: Epoch,
    ) {
        self.author[r.index()] = author;
        self.branch[r.index()] = branch;
        self.epoch[r.index()] = epoch;
    }
    pub fn export(&self) -> Vec<Revision> {
        (0..self.len())
            .map(|n| Revision {
                logical: self.logical[n],
                content: self.content[n],
                author: self.author[n],
                origin_branch: self.branch[n],
                origin_epoch: self.epoch[n],
            })
            .collect()
    }
    pub fn truncate(&mut self, n: usize) {
        self.logical.truncate(n);
        self.content.truncate(n);
        self.author.truncate(n);
        self.branch.truncate(n);
        self.epoch.truncate(n);
    }
}
#[derive(Clone, Debug, Default)]
pub(crate) struct RuleArena {
    conclusion: Vec<RevisionId>,
    // Rule n owns premise[start[n]..start[n + 1]] (or ..premise.len() for the last).
    start: Vec<u32>,
    premise: Vec<RevisionId>,
    kind: Vec<EdgeKind>,
    // u64: references are not covered by the u32 incidence capacity.
    ref_start: Vec<u64>,
    refs: Vec<RevisionId>,
    pub enabled: Vec<bool>,
    branch: Vec<BranchId>,
    epoch: Vec<Epoch>,
}
fn span<T: Copy + Into<u64>>(start: &[T], n: usize, total: usize) -> std::ops::Range<usize> {
    start[n].into() as usize..start.get(n + 1).map_or(total, |e| (*e).into() as usize)
}
fn offset(n: usize) -> u32 {
    u32::try_from(n).expect("bounded index")
}
impl RuleArena {
    pub fn retained_bytes(&self) -> usize {
        use crate::memory::{bool_bytes, vec_bytes as b};
        b(&self.conclusion)
            + b(&self.start)
            + b(&self.premise)
            + b(&self.kind)
            + b(&self.ref_start)
            + b(&self.refs)
            + bool_bytes(&self.enabled)
            + b(&self.branch)
            + b(&self.epoch)
    }
    pub fn record(&self, j: RuleId) -> Rule {
        let n = j.index();
        Rule {
            conclusion: self.conclusion[n],
            premises: self.premises(j).collect(),
            references: self.references(j).to_vec(),
            enabled: self.enabled[n],
            origin_branch: self.branch[n],
            origin_epoch: self.epoch[n],
        }
    }
    pub fn set_origin(&mut self, j: RuleId, branch: BranchId, epoch: Epoch) {
        self.branch[j.index()] = branch;
        self.epoch[j.index()] = epoch;
    }
    pub fn conclusion(&self, j: RuleId) -> RevisionId {
        self.conclusion[j.index()]
    }
    pub fn premises(&self, j: RuleId) -> impl Iterator<Item = Incidence> + '_ {
        span(&self.start, j.index(), self.premise.len()).map(|p| Incidence {
            premise: self.premise[p],
            kind: self.kind[p],
        })
    }
    pub fn premise_at(&self, j: RuleId, k: usize) -> Incidence {
        let p = self.start[j.index()] as usize + k;
        Incidence {
            premise: self.premise[p],
            kind: self.kind[p],
        }
    }
    pub fn references(&self, j: RuleId) -> &[RevisionId] {
        &self.refs[span(&self.ref_start, j.index(), self.refs.len())]
    }
    pub fn len(&self) -> usize {
        self.conclusion.len()
    }
    pub fn reserve(&mut self, new: &[Rule]) {
        let n = new.len();
        let premises = new.iter().map(|j| j.premises.len()).sum();
        let refs = new.iter().map(|j| j.references.len()).sum();
        crate::memory::grow(&mut self.conclusion, n);
        crate::memory::grow(&mut self.start, n);
        crate::memory::grow(&mut self.premise, premises);
        crate::memory::grow(&mut self.kind, premises);
        crate::memory::grow(&mut self.ref_start, n);
        crate::memory::grow(&mut self.refs, refs);
        crate::memory::grow(&mut self.enabled, n);
        crate::memory::grow(&mut self.branch, n);
        crate::memory::grow(&mut self.epoch, n);
    }
    pub fn append(&mut self, j: &Rule) {
        self.conclusion.push(j.conclusion);
        self.start.push(offset(self.premise.len()));
        self.premise.extend(j.premises.iter().map(|p| p.premise));
        self.kind.extend(j.premises.iter().map(|p| p.kind));
        self.ref_start.push(self.refs.len() as u64);
        self.refs.extend_from_slice(&j.references);
        self.enabled.push(j.enabled);
        self.branch.push(j.origin_branch);
        self.epoch.push(j.origin_epoch);
    }
    pub fn export(&self) -> Vec<Rule> {
        (0..self.len())
            .map(|n| Rule {
                conclusion: self.conclusion[n],
                premises: self.premises(RuleId(n as u32)).collect(),
                references: self.references(RuleId(n as u32)).to_vec(),
                enabled: self.enabled[n],
                origin_branch: self.branch[n],
                origin_epoch: self.epoch[n],
            })
            .collect()
    }
    pub fn truncate(&mut self, n: usize) {
        if n < self.len() {
            self.premise.truncate(self.start[n] as usize);
            self.kind.truncate(self.start[n] as usize);
            self.refs.truncate(self.ref_start[n] as usize);
        }
        self.conclusion.truncate(n);
        self.start.truncate(n);
        self.ref_start.truncate(n);
        self.enabled.truncate(n);
        self.branch.truncate(n);
        self.epoch.truncate(n);
    }
}
/// Reverse index: a compact CSR base plus per-row overflow for entries appended
/// since the last compaction (ADR 0007). A row reads as base then overflow, so
/// entries stay in ascending insertion order (canonical witnesses depend on it).
#[derive(Clone, Debug, Default)]
pub struct Adjacency<T> {
    offsets: Vec<u32>,
    items: Vec<T>,
    building: Vec<Vec<T>>,
    /// Rows beyond the CSR base or with appended entries.
    extra: BTreeMap<u32, Vec<T>>,
    extra_len: usize,
    rows: usize,
}
/// One adjacency row: CSR part followed by appended entries.
#[derive(Clone, Copy, Debug)]
pub struct Row<'a, T> {
    base: &'a [T],
    extra: &'a [T],
}
impl<'a, T> Row<'a, T> {
    pub fn iter(&self) -> std::iter::Chain<std::slice::Iter<'a, T>, std::slice::Iter<'a, T>> {
        self.base.iter().chain(self.extra.iter())
    }
    pub fn len(&self) -> usize {
        self.base.len() + self.extra.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn first(&self) -> Option<&'a T> {
        self.base.first().or(self.extra.first())
    }
}
impl<'a, T> IntoIterator for Row<'a, T> {
    type Item = &'a T;
    type IntoIter = std::iter::Chain<std::slice::Iter<'a, T>, std::slice::Iter<'a, T>>;
    fn into_iter(self) -> Self::IntoIter {
        self.base.iter().chain(self.extra.iter())
    }
}
impl<T: PartialEq> PartialEq<Vec<T>> for Row<'_, T> {
    fn eq(&self, other: &Vec<T>) -> bool {
        self.len() == other.len() && self.iter().zip(other).all(|(a, b)| a == b)
    }
}
impl<T> Adjacency<T> {
    fn retained_bytes(&self) -> usize {
        use crate::memory::{btree_slots, vec_bytes as b};
        b(&self.offsets)
            + b(&self.items)
            + btree_slots(self.extra.len())
                * (std::mem::size_of::<u32>() + std::mem::size_of::<Vec<T>>())
            + self.extra.values().map(b).sum::<usize>()
    }
    fn with_rows(n: usize) -> Self {
        Self {
            offsets: Vec::new(),
            items: Vec::new(),
            building: (0..n).map(|_| Vec::new()).collect(),
            extra: BTreeMap::new(),
            extra_len: 0,
            rows: n,
        }
    }
    fn push(&mut self, row: usize, value: T) {
        self.building[row].push(value);
    }
    fn seal(&mut self) {
        let n = self.building.iter().map(Vec::len).sum();
        self.items = Vec::with_capacity(n);
        self.offsets = Vec::with_capacity(self.building.len() + 1);
        self.offsets.push(0);
        for row in self.building.drain(..) {
            self.items.extend(row);
            self.offsets
                .push(u32::try_from(self.items.len()).expect("bounded index"));
        }
        self.building = Vec::new();
    }
    pub fn rows(&self) -> usize {
        self.rows
    }
    pub fn row(&self, n: usize) -> Row<'_, T> {
        let base = if n + 1 < self.offsets.len() {
            &self.items[self.offsets[n] as usize..self.offsets[n + 1] as usize]
        } else {
            &[]
        };
        let extra = if self.extra.is_empty() {
            &[][..]
        } else {
            self.extra.get(&(n as u32)).map_or(&[][..], Vec::as_slice)
        };
        Row { base, extra }
    }
    /// Grows the row count; new rows start empty.
    pub(crate) fn ensure_rows(&mut self, n: usize) {
        self.rows = self.rows.max(n);
    }
    /// Removes the last appended entry of `row` (WHAT_IF rollback; nothing is
    /// compacted while a journal is open).
    pub(crate) fn pop(&mut self, row: usize) -> Option<T> {
        let extra = self.extra.get_mut(&(row as u32))?;
        let value = extra.pop();
        if extra.is_empty() {
            self.extra.remove(&(row as u32));
        }
        self.extra_len -= 1;
        value
    }
    pub(crate) fn truncate_rows(&mut self, n: usize) {
        crate::invariant_assert!(
            self.extra.range(n as u32..).next().is_none() && self.offsets.len() <= n + 1
        );
        self.rows = n;
    }
    /// Appends after every existing entry of `row`.
    pub(crate) fn append(&mut self, row: usize, value: T) {
        debug_assert!(row < self.rows);
        self.extra.entry(row as u32).or_default().push(value);
        self.extra_len += 1;
    }
}
impl<T: Clone> Adjacency<T> {
    /// Folds overflow into the CSR once it exceeds a quarter of the base:
    /// amortized O(1) per appended entry.
    pub(crate) fn maybe_compact(&mut self) {
        if self.extra_len <= (self.items.len() / 4).max(1024) {
            return;
        }
        let mut items = Vec::with_capacity(self.items.len() + self.extra_len);
        let mut offsets = Vec::with_capacity(self.rows + 1);
        offsets.push(0);
        for n in 0..self.rows {
            items.extend(self.row(n).iter().cloned());
            offsets.push(u32::try_from(items.len()).expect("bounded index"));
        }
        self.items = items;
        self.offsets = offsets;
        self.extra = BTreeMap::new();
        self.extra_len = 0;
    }
}
impl<T: PartialEq> PartialEq for Adjacency<T> {
    /// Logical equality: same rows with the same entries, however stored.
    fn eq(&self, other: &Self) -> bool {
        self.rows == other.rows
            && (0..self.rows).all(|n| {
                let (a, b) = (self.row(n), other.row(n));
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
            })
    }
}
impl<T: Eq> Eq for Adjacency<T> {}
impl<T: PartialEq> PartialEq<Vec<Vec<T>>> for Adjacency<T> {
    fn eq(&self, other: &Vec<Vec<T>>) -> bool {
        self.rows == other.len() && other.iter().enumerate().all(|(n, row)| self.row(n) == *row)
    }
}
/// Stable incidence IDs enumerate CSR premises only; Ref never enters support indexes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Indexes {
    pub rules_by_premise: Adjacency<IncId>,
    pub rules_by_concl: Adjacency<RuleId>,
    pub exact_by_rev: Adjacency<IncId>,
    pub pin_by_rev: Adjacency<IncId>,
    pub sem_by_logical: Adjacency<IncId>,
    pub refs_by_target: Adjacency<RuleId>,
    /// Revisions of each logical in creation order (Sem virtual-node edges).
    pub revisions_by_logical: Adjacency<RevisionId>,
    pub(crate) inc_rule: Vec<RuleId>,
    // rule_start[j]..rule_start[j + 1] are rule j's incidence IDs.
    pub(crate) rule_start: Vec<u32>,
    /// Open `Conflict` artifacts per target revision, the SD17 gate's input
    /// (ADR 0013 §6.4); revisions with none are absent.
    pub(crate) open_conflicts: BTreeMap<RevisionId, u32>,
}
impl Indexes {
    /// Whether an open `Conflict` targets `r` (SD11's `Conflicted(r)`).
    pub(crate) fn conflicted(&self, r: RevisionId) -> bool {
        self.open_conflicts.contains_key(&r)
    }
    /// One more (`open`) or one fewer open Conflict on `r`.
    pub(crate) fn adjust_conflicts(&mut self, r: RevisionId, open: bool) {
        let n = self.open_conflicts.entry(r).or_default();
        *n = if open {
            *n + 1
        } else {
            n.checked_sub(1).expect("open conflict")
        };
        if *n == 0 {
            self.open_conflicts.remove(&r);
        }
    }
    pub(crate) fn rule_incidences(&self, j: RuleId) -> std::ops::Range<usize> {
        self.rule_start[j.index()] as usize..self.rule_start[j.index() + 1] as usize
    }
    /// Premise and kind live in the input columns; IDs only locate them.
    pub(crate) fn incidence(&self, i: &impl crate::InputView, id: usize) -> Incidence {
        let j = self.inc_rule[id];
        i.premise_at(j, id - self.rule_start[j.index()] as usize)
    }
    pub fn retained_bytes(&self) -> usize {
        use crate::memory::vec_bytes as b;
        self.rules_by_premise.retained_bytes()
            + self.rules_by_concl.retained_bytes()
            + self.exact_by_rev.retained_bytes()
            + self.pin_by_rev.retained_bytes()
            + self.sem_by_logical.retained_bytes()
            + self.refs_by_target.retained_bytes()
            + self.revisions_by_logical.retained_bytes()
            + b(&self.inc_rule)
            + b(&self.rule_start)
    }
    pub(crate) fn build(i: &Inputs) -> Self {
        let mut out = Self {
            rules_by_premise: Adjacency::with_rows(i.revisions.len()),
            rules_by_concl: Adjacency::with_rows(i.revisions.len()),
            exact_by_rev: Adjacency::with_rows(i.revisions.len()),
            pin_by_rev: Adjacency::with_rows(i.revisions.len()),
            sem_by_logical: Adjacency::with_rows(i.logicals.len()),
            refs_by_target: Adjacency::with_rows(i.revisions.len()),
            revisions_by_logical: Adjacency::with_rows(i.logicals.len()),
            inc_rule: Vec::with_capacity(i.rules.iter().map(|j| j.premises.len()).sum()),
            rule_start: Vec::with_capacity(i.rules.len() + 1),
            open_conflicts: BTreeMap::new(),
        };
        for (n, r) in i.revisions.iter().enumerate() {
            out.revisions_by_logical
                .push(r.logical.index(), RevisionId(n as u32));
        }
        let mut inc = 0;
        for (n, j) in i.rules.iter().enumerate() {
            let id = RuleId(n as u32);
            out.rule_start
                .push(u32::try_from(out.inc_rule.len()).expect("bounded index"));
            out.rules_by_concl.push(j.conclusion.index(), id);
            for p in &j.premises {
                out.inc_rule.push(id);
                let id = IncId(inc);
                inc += 1;
                out.rules_by_premise.push(p.premise.index(), id);
                match p.kind {
                    EdgeKind::Cur => out.exact_by_rev.push(p.premise.index(), id),
                    EdgeKind::Pin => out.pin_by_rev.push(p.premise.index(), id),
                    EdgeKind::Sem => out
                        .sem_by_logical
                        .push(i.revisions[p.premise.index()].logical.index(), id),
                    EdgeKind::Ref => unreachable!("canonical premises"),
                }
            }
            for r in &j.references {
                out.refs_by_target.push(r.index(), id);
            }
        }
        out.rule_start
            .push(u32::try_from(out.inc_rule.len()).expect("bounded index"));
        for a in &i.artifacts {
            if a.ty == ArtifactType::Conflict && a.open {
                out.adjust_conflicts(a.target, true);
            }
        }
        out.rules_by_premise.seal();
        out.rules_by_concl.seal();
        out.exact_by_rev.seal();
        out.pin_by_rev.seal();
        out.sem_by_logical.seal();
        out.refs_by_target.seal();
        out.revisions_by_logical.seal();
        out
    }
    /// Incremental counterpart of `build` for appended inputs (ADR 0007):
    /// revisions from `old_revisions` and rules from `old_rules` onward.
    pub(crate) fn append(
        &mut self,
        i: &impl crate::InputView,
        old_revisions: usize,
        old_rules: usize,
        compact: bool,
    ) {
        let (revisions, logicals) = (i.revision_count(), i.logical_count());
        for a in [
            &mut self.rules_by_premise,
            &mut self.exact_by_rev,
            &mut self.pin_by_rev,
        ] {
            a.ensure_rows(revisions);
        }
        self.rules_by_concl.ensure_rows(revisions);
        self.refs_by_target.ensure_rows(revisions);
        self.sem_by_logical.ensure_rows(logicals);
        self.revisions_by_logical.ensure_rows(logicals);
        for n in old_revisions..revisions {
            let r = RevisionId(n as u32);
            self.revisions_by_logical
                .append(i.revision_logical(r).index(), r);
        }
        if self.rule_start.is_empty() {
            self.rule_start.push(0);
        }
        let new_rules = i.rule_count().saturating_sub(old_rules);
        crate::memory::grow(&mut self.rule_start, new_rules);
        crate::memory::grow(
            &mut self.inc_rule,
            (old_rules..i.rule_count())
                .map(|n| i.premises(RuleId(n as u32)).count())
                .sum(),
        );
        for n in old_rules..i.rule_count() {
            let j = RuleId(n as u32);
            self.rules_by_concl.append(i.conclusion(j).index(), j);
            for p in i.premises(j) {
                let id = IncId(u32::try_from(self.inc_rule.len()).expect("bounded index"));
                self.inc_rule.push(j);
                self.rules_by_premise.append(p.premise.index(), id);
                match p.kind {
                    EdgeKind::Cur => self.exact_by_rev.append(p.premise.index(), id),
                    EdgeKind::Pin => self.pin_by_rev.append(p.premise.index(), id),
                    EdgeKind::Sem => self
                        .sem_by_logical
                        .append(i.revision_logical(p.premise).index(), id),
                    EdgeKind::Ref => unreachable!("canonical premises"),
                }
            }
            for r in i.references(j) {
                self.refs_by_target.append(r.index(), j);
            }
            self.rule_start
                .push(u32::try_from(self.inc_rule.len()).expect("bounded index"));
        }
        if !compact {
            return;
        }
        self.rules_by_premise.maybe_compact();
        self.rules_by_concl.maybe_compact();
        self.exact_by_rev.maybe_compact();
        self.pin_by_rev.maybe_compact();
        self.sem_by_logical.maybe_compact();
        self.refs_by_target.maybe_compact();
        self.revisions_by_logical.maybe_compact();
    }
}
impl Indexes {
    /// Lengths that [`Self::truncate`] restores.
    pub(crate) fn mark(&self) -> (usize, usize) {
        (self.inc_rule.len(), self.rule_start.len())
    }
    /// Undoes [`Self::append`] (made without compaction): `i` still holds the
    /// appended revisions and rules.
    pub(crate) fn truncate(
        &mut self,
        i: &impl crate::InputView,
        (old_logicals, old_revisions, old_rules): (usize, usize, usize),
        (incs, starts): (usize, usize),
    ) {
        fn pop<T: PartialEq + std::fmt::Debug>(a: &mut Adjacency<T>, row: usize, want: T) {
            let got = a.pop(row);
            crate::invariant_assert!(got.as_ref() == Some(&want), "{got:?} != {want:?}");
        }
        for n in (old_rules..i.rule_count()).rev() {
            let j = RuleId(n as u32);
            for r in i.references(j).iter().rev() {
                pop(&mut self.refs_by_target, r.index(), j);
            }
            let premises: Vec<_> = i.premises(j).collect();
            for (k, p) in premises.iter().enumerate().rev() {
                let id = IncId(self.rule_start[n] + k as u32);
                pop(&mut self.rules_by_premise, p.premise.index(), id);
                match p.kind {
                    EdgeKind::Cur => pop(&mut self.exact_by_rev, p.premise.index(), id),
                    EdgeKind::Pin => pop(&mut self.pin_by_rev, p.premise.index(), id),
                    EdgeKind::Sem => pop(
                        &mut self.sem_by_logical,
                        i.revision_logical(p.premise).index(),
                        id,
                    ),
                    EdgeKind::Ref => unreachable!("canonical premises"),
                }
            }
            pop(&mut self.rules_by_concl, i.conclusion(j).index(), j);
        }
        for n in (old_revisions..i.revision_count()).rev() {
            let r = RevisionId(n as u32);
            pop(
                &mut self.revisions_by_logical,
                i.revision_logical(r).index(),
                r,
            );
        }
        self.inc_rule.truncate(incs);
        self.rule_start.truncate(starts);
        for a in [
            &mut self.rules_by_premise,
            &mut self.exact_by_rev,
            &mut self.pin_by_rev,
        ] {
            a.truncate_rows(old_revisions);
        }
        self.rules_by_concl.truncate_rows(old_revisions);
        self.refs_by_target.truncate_rows(old_revisions);
        self.sem_by_logical.truncate_rows(old_logicals);
        self.revisions_by_logical.truncate_rows(old_logicals);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    #[test]
    fn key_table_probes_wrap_and_grow_at_three_quarters() {
        // Pairs of ids share a home slot near the end, so probes wrap to slot 0.
        let hash = |id: u32| 13 + u64::from(id / 2);
        let mut t = KeyTable::default();
        assert_eq!(t.find(0, |_| true), None);
        for id in 0..12 {
            t.insert(hash(id), id, hash);
        }
        assert_eq!(t.slots.len(), 16, "12 of 16 slots is exactly 3/4");
        for id in 0..12 {
            assert_eq!(t.find(hash(id), |x| x == id), Some(id));
        }
        assert_eq!(t.find(15, |_| false), None);
        t.insert(hash(12), 12, hash);
        assert_eq!((t.slots.len(), t.len), (32, 13));
        for id in 0..13 {
            assert_eq!(t.find(hash(id), |x| x == id), Some(id));
        }
        assert_ne!(key_hash("a"), key_hash("b"));
        assert_eq!(key_hash("a"), key_hash("a"));
    }
    fn logical(key: String) -> Logical {
        Logical {
            key,
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            owner: PrincipalId::default(),
            origin_branch: BranchId::default(),
            origin_epoch: Epoch(0),
        }
    }
    #[test]
    fn logical_arena_finds_keys_across_rehashes_and_keeps_sparse_metadata() {
        let mut a = LogicalArena::default();
        let keys: Vec<String> = (0..100).map(|n| format!("k{n}")).collect();
        for k in &keys {
            a.append(&logical(k.clone()));
        }
        for (n, k) in keys.iter().enumerate() {
            assert_eq!(a.find_key(k), Some(LogicalId(n as u32)));
        }
        assert_eq!(a.find_key("k100"), None);
        // Distinct keys spread over the table instead of sharing one probe chain.
        let homes: BTreeSet<usize> = keys
            .iter()
            .map(|k| key_hash(k) as usize & (a.keys_by_hash.slots.len() - 1))
            .collect();
        assert!(homes.len() > keys.len() / 2);
        let tagged = Metadata {
            tags: ["t".to_owned()].into(),
            dirs: Default::default(),
        };
        let mut all = vec![Metadata::default(); 100];
        all[3] = tagged.clone();
        a.set_metadata(&all);
        assert_eq!(a.metadata.len(), 1, "defaults are not stored");
        assert_eq!(a.metadata(LogicalId(3)), tagged);
        assert_eq!(a.export().1, all);
    }
    #[test]
    fn revision_arena_returns_content() {
        let mut a = RevisionArena::default();
        a.append(&Revision {
            logical: LogicalId(0),
            content: [7; 32],
            author: PrincipalId::default(),
            origin_branch: BranchId::default(),
            origin_epoch: Epoch(1),
        });
        assert_eq!(a.content(RevisionId(0)), [7; 32]);
    }
    fn sealed(rows: &[&[u32]]) -> Adjacency<u32> {
        let mut a = Adjacency::with_rows(rows.len());
        for (n, row) in rows.iter().enumerate() {
            for v in *row {
                a.push(n, *v);
            }
        }
        a.seal();
        a
    }
    #[test]
    fn adjacency_rows_combine_csr_and_overflow() {
        let mut a = sealed(&[&[1, 2], &[], &[3]]);
        assert_eq!(a.rows(), 3);
        assert_eq!(a, vec![vec![1, 2], vec![], vec![3]]);
        assert!(a.row(1).is_empty() && !a.row(0).is_empty());
        assert_eq!(a.row(0).first(), Some(&1));
        assert_eq!(a.row(1).first(), None);
        a.append(1, 9);
        assert_eq!(a.row(1).first(), Some(&9), "overflow-only row");
        a.ensure_rows(5);
        assert_eq!(a.rows(), 5);
        assert!(a.row(3).is_empty() && a.row(4).is_empty());
        a.append(4, 8);
        assert_eq!(a, vec![vec![1, 2], vec![9], vec![3], vec![], vec![8]]);
    }
    #[test]
    fn adjacency_equality_is_logical_and_strict() {
        let a = sealed(&[&[1, 2], &[3]]);
        let mut b = sealed(&[&[1], &[]]);
        b.append(0, 2);
        b.append(1, 3);
        assert_eq!(a, b, "same rows, different storage");
        assert_ne!(a, sealed(&[&[1, 2], &[3], &[]]), "row count");
        assert_ne!(a, sealed(&[&[1, 2], &[3, 4]]), "row length");
        assert_ne!(a, sealed(&[&[1, 2], &[4]]), "entries");
        assert_ne!(a, sealed(&[&[2, 1], &[3]]), "order");
    }
    #[test]
    fn adjacency_compacts_past_a_quarter_of_the_base() {
        let base: Vec<u32> = (0..8192).collect();
        let fresh = || sealed(&[&base, &[]]);
        // Threshold: max(8192 / 4, 1024) = 2048 overflow entries.
        let mut a = fresh();
        for v in 0..2048 {
            a.append(1, v);
        }
        a.maybe_compact();
        assert_eq!(a.extra_len, 2048, "at the threshold: no compaction");
        a.append(1, 2048);
        a.maybe_compact();
        assert_eq!((a.extra_len, a.items.len()), (0, 8192 + 2049));
        let mut expected = fresh();
        for v in 0..2049 {
            expected.append(1, v);
        }
        assert_eq!(a, expected);
        // Small bases use the 1024 floor.
        let mut small = sealed(&[&[1]]);
        for v in 0..1025 {
            small.append(0, v);
        }
        small.maybe_compact();
        assert_eq!(small.extra_len, 0);
    }
}
#[cfg(test)]
mod reserve_tests {
    use super::*;
    fn logical(key: &str) -> Logical {
        Logical {
            key: key.into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            owner: PrincipalId::default(),
            origin_branch: BranchId::default(),
            origin_epoch: Epoch(0),
        }
    }
    #[test]
    fn arenas_reserve_with_the_shared_growth_policy() {
        let mut a = LogicalArena::default();
        let first: Vec<_> = ["abc", "de", "f"].map(logical).into();
        a.reserve(&first);
        assert_eq!((a.keys.capacity(), a.key_end.capacity()), (6, 3));
        assert_eq!(
            [a.kind.capacity(), a.scope.capacity(), a.owner.capacity()],
            [3, 3, 3]
        );
        assert_eq!([a.branch.capacity(), a.epoch.capacity()], [3, 3]);
        for l in &first {
            a.append(l);
        }
        // 6 + 1 key bytes: grows to max(7, 6 + 6 / 4) = 7.
        a.reserve(&[logical("g")]);
        assert_eq!(a.keys.capacity(), 7);
        let mut r = RevisionArena::default();
        r.reserve(4);
        assert_eq!(
            [
                r.logical.capacity(),
                r.content.capacity(),
                r.author.capacity(),
                r.branch.capacity(),
                r.epoch.capacity()
            ],
            [4; 5]
        );
        let mut j = RuleArena::default();
        let rule = Rule {
            conclusion: RevisionId(1),
            premises: vec![
                Incidence {
                    premise: RevisionId(0),
                    kind: EdgeKind::Pin,
                };
                3
            ],
            references: vec![RevisionId(2); 2],
            enabled: true,
            origin_branch: BranchId::default(),
            origin_epoch: Epoch(0),
        };
        j.reserve(&[rule.clone(), rule]);
        assert_eq!(
            [
                j.conclusion.capacity(),
                j.start.capacity(),
                j.ref_start.capacity(),
                j.enabled.capacity(),
                j.branch.capacity(),
                j.epoch.capacity()
            ],
            [2; 6]
        );
        assert_eq!(
            [j.premise.capacity(), j.kind.capacity(), j.refs.capacity()],
            [6, 6, 4]
        );
    }
}
