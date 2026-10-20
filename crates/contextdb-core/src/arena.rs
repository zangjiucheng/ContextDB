use contextdb_model::*;
use std::collections::BTreeMap;

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
}
impl LogicalArena {
    pub fn retained_bytes(&self) -> usize {
        use crate::memory::{btree_slots, vec_bytes as b};
        use std::mem::size_of;
        b(&self.key_end)
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
    fn key(&self, n: usize) -> &str {
        let start = if n == 0 {
            0
        } else {
            self.key_end[n - 1] as usize
        };
        &self.keys[start..self.key_end[n] as usize]
    }
    pub fn find_key(&self, key: &str) -> Option<LogicalId> {
        (0..self.len())
            .position(|n| self.key(n) == key)
            .map(|n| LogicalId(n as u32))
    }
    pub fn reserve_exact(&mut self, new: &[Logical]) {
        let n = new.len();
        self.keys
            .reserve_exact(new.iter().map(|l| l.key.len()).sum());
        self.key_end.reserve_exact(n);
        self.kind.reserve_exact(n);
        self.scope.reserve_exact(n);
        self.owner.reserve_exact(n);
        self.branch.reserve_exact(n);
        self.epoch.reserve_exact(n);
    }
    pub fn append(&mut self, l: &Logical) {
        self.keys.push_str(&l.key);
        self.key_end.push(self.keys.len() as u64);
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
    pub fn len(&self) -> usize {
        self.logical.len()
    }
    pub fn reserve_exact(&mut self, n: usize) {
        self.logical.reserve_exact(n);
        self.content.reserve_exact(n);
        self.author.reserve_exact(n);
        self.branch.reserve_exact(n);
        self.epoch.reserve_exact(n);
    }
    pub fn append(&mut self, r: &Revision) {
        self.logical.push(r.logical);
        self.content.push(r.content);
        self.author.push(r.author);
        self.branch.push(r.origin_branch);
        self.epoch.push(r.origin_epoch);
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
    pub fn reserve_exact(&mut self, new: &[Rule]) {
        let n = new.len();
        let premises = new.iter().map(|j| j.premises.len()).sum();
        let refs = new.iter().map(|j| j.references.len()).sum();
        self.conclusion.reserve_exact(n);
        self.start.reserve_exact(n);
        self.premise.reserve_exact(premises);
        self.kind.reserve_exact(premises);
        self.ref_start.reserve_exact(n);
        self.refs.reserve_exact(refs);
        self.enabled.reserve_exact(n);
        self.branch.reserve_exact(n);
        self.epoch.reserve_exact(n);
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
/// Compact CSR reverse index; callers retain slice indexing by dense ID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Adjacency<T> {
    offsets: Vec<u32>,
    items: Vec<T>,
    building: Vec<Vec<T>>,
}
impl<T> Adjacency<T> {
    fn retained_bytes(&self) -> usize {
        crate::memory::vec_bytes(&self.offsets) + crate::memory::vec_bytes(&self.items)
    }
    fn rows(n: usize) -> Self {
        Self {
            offsets: Vec::new(),
            items: Vec::new(),
            building: (0..n).map(|_| Vec::new()).collect(),
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
}
impl<T> std::ops::Index<usize> for Adjacency<T> {
    type Output = [T];
    fn index(&self, n: usize) -> &[T] {
        &self.items[self.offsets[n] as usize..self.offsets[n + 1] as usize]
    }
}
impl<T: PartialEq> PartialEq<Vec<Vec<T>>> for Adjacency<T> {
    fn eq(&self, other: &Vec<Vec<T>>) -> bool {
        self.offsets.len() == other.len() + 1
            && other.iter().enumerate().all(|(n, row)| self[n] == *row)
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
    pub(crate) inc_rule: Vec<RuleId>,
    // rule_start[j]..rule_start[j + 1] are rule j's incidence IDs.
    pub(crate) rule_start: Vec<u32>,
}
impl Indexes {
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
            + b(&self.inc_rule)
            + b(&self.rule_start)
    }
    pub(crate) fn build(i: &Inputs) -> Self {
        let mut out = Self {
            rules_by_premise: Adjacency::rows(i.revisions.len()),
            rules_by_concl: Adjacency::rows(i.revisions.len()),
            exact_by_rev: Adjacency::rows(i.revisions.len()),
            pin_by_rev: Adjacency::rows(i.revisions.len()),
            sem_by_logical: Adjacency::rows(i.logicals.len()),
            refs_by_target: Adjacency::rows(i.revisions.len()),
            inc_rule: Vec::with_capacity(i.rules.iter().map(|j| j.premises.len()).sum()),
            rule_start: Vec::with_capacity(i.rules.len() + 1),
        };
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
        out.rules_by_premise.seal();
        out.rules_by_concl.seal();
        out.exact_by_rev.seal();
        out.pin_by_rev.seal();
        out.sem_by_logical.seal();
        out.refs_by_target.seal();
        out
    }
}
