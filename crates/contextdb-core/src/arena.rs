use contextdb_model::*;

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
    offset: Vec<usize>,
    len: Vec<usize>,
    premise: Vec<RevisionId>,
    kind: Vec<EdgeKind>,
    refs: Vec<Vec<RevisionId>>,
    pub enabled: Vec<bool>,
    flags: Vec<u8>,
    branch: Vec<BranchId>,
    epoch: Vec<Epoch>,
}
impl RuleArena {
    pub fn retained_bytes(&self) -> usize {
        use crate::memory::{bool_bytes, vec_bytes as b};
        b(&self.conclusion)
            + b(&self.offset)
            + b(&self.len)
            + b(&self.premise)
            + b(&self.kind)
            + b(&self.refs)
            + self.refs.iter().map(b).sum::<usize>()
            + bool_bytes(&self.enabled)
            + b(&self.flags)
            + b(&self.branch)
            + b(&self.epoch)
    }
    pub fn conclusion(&self, j: RuleId) -> RevisionId {
        self.conclusion[j.index()]
    }
    pub fn premises(&self, j: RuleId) -> impl Iterator<Item = Incidence> + '_ {
        let n = j.index();
        (self.offset[n]..self.offset[n] + self.len[n]).map(|p| Incidence {
            premise: self.premise[p],
            kind: self.kind[p],
        })
    }
    pub fn references(&self, j: RuleId) -> &[RevisionId] {
        &self.refs[j.index()]
    }
    pub fn len(&self) -> usize {
        self.conclusion.len()
    }
    pub fn append(&mut self, j: &Rule) {
        self.conclusion.push(j.conclusion);
        self.offset.push(self.premise.len());
        self.len.push(j.premises.len());
        self.premise.extend(j.premises.iter().map(|p| p.premise));
        self.kind.extend(j.premises.iter().map(|p| p.kind));
        self.refs.push(j.references.clone());
        self.enabled.push(j.enabled);
        self.flags
            .push(u8::from(j.premises.iter().any(|p| p.kind == EdgeKind::Sem)));
        self.branch.push(j.origin_branch);
        self.epoch.push(j.origin_epoch);
    }
    pub fn export(&self) -> Vec<Rule> {
        (0..self.len())
            .map(|n| Rule {
                conclusion: self.conclusion[n],
                premises: (self.offset[n]..self.offset[n] + self.len[n])
                    .map(|p| Incidence {
                        premise: self.premise[p],
                        kind: self.kind[p],
                    })
                    .collect(),
                references: self.refs[n].clone(),
                enabled: self.enabled[n],
                origin_branch: self.branch[n],
                origin_epoch: self.epoch[n],
            })
            .collect()
    }
    pub fn truncate(&mut self, n: usize) {
        let p = if n == self.len() {
            self.premise.len()
        } else {
            self.offset[n]
        };
        self.premise.truncate(p);
        self.kind.truncate(p);
        self.conclusion.truncate(n);
        self.offset.truncate(n);
        self.len.truncate(n);
        self.refs.truncate(n);
        self.enabled.truncate(n);
        self.flags.truncate(n);
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
    pub(crate) inc_premise: Vec<RevisionId>,
    pub(crate) inc_kind: Vec<EdgeKind>,
    pub(crate) rule_offset: Vec<usize>,
    pub(crate) rule_len: Vec<usize>,
}
impl Indexes {
    pub fn retained_bytes(&self) -> usize {
        use crate::memory::vec_bytes as b;
        self.rules_by_premise.retained_bytes()
            + self.rules_by_concl.retained_bytes()
            + self.exact_by_rev.retained_bytes()
            + self.pin_by_rev.retained_bytes()
            + self.sem_by_logical.retained_bytes()
            + self.refs_by_target.retained_bytes()
            + b(&self.inc_rule)
            + b(&self.inc_premise)
            + b(&self.inc_kind)
            + b(&self.rule_offset)
            + b(&self.rule_len)
    }
    pub(crate) fn build(i: &Inputs) -> Self {
        let mut out = Self {
            rules_by_premise: Adjacency::rows(i.revisions.len()),
            rules_by_concl: Adjacency::rows(i.revisions.len()),
            exact_by_rev: Adjacency::rows(i.revisions.len()),
            pin_by_rev: Adjacency::rows(i.revisions.len()),
            sem_by_logical: Adjacency::rows(i.logicals.len()),
            refs_by_target: Adjacency::rows(i.revisions.len()),
            inc_rule: Vec::new(),
            inc_premise: Vec::new(),
            inc_kind: Vec::new(),
            rule_offset: Vec::new(),
            rule_len: Vec::new(),
        };
        let mut inc = 0;
        for (n, j) in i.rules.iter().enumerate() {
            let id = RuleId(n as u32);
            out.rule_offset.push(out.inc_rule.len());
            out.rule_len.push(j.premises.len());
            out.rules_by_concl.push(j.conclusion.index(), id);
            for p in &j.premises {
                out.inc_rule.push(id);
                out.inc_premise.push(p.premise);
                out.inc_kind.push(p.kind);
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
        out.rules_by_premise.seal();
        out.rules_by_concl.seal();
        out.exact_by_rev.seal();
        out.pin_by_rev.seal();
        out.sem_by_logical.seal();
        out.refs_by_target.seal();
        out
    }
}
