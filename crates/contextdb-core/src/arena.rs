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
/// Stable incidence IDs enumerate CSR premises only; Ref never enters support indexes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Indexes {
    pub rules_by_premise: Vec<Vec<IncId>>,
    pub rules_by_concl: Vec<Vec<RuleId>>,
    pub exact_by_rev: Vec<Vec<IncId>>,
    pub pin_by_rev: Vec<Vec<IncId>>,
    pub sem_by_logical: Vec<Vec<IncId>>,
    pub refs_by_target: Vec<Vec<RuleId>>,
}
impl Indexes {
    pub(crate) fn build(i: &Inputs) -> Self {
        let empty = vec![Vec::new(); i.revisions.len()];
        let mut out = Self {
            rules_by_premise: empty.clone(),
            rules_by_concl: vec![Vec::new(); i.revisions.len()],
            exact_by_rev: empty.clone(),
            pin_by_rev: empty.clone(),
            sem_by_logical: vec![Vec::new(); i.logicals.len()],
            refs_by_target: vec![Vec::new(); i.revisions.len()],
        };
        let mut inc = 0;
        for (n, j) in i.rules.iter().enumerate() {
            let id = RuleId(n as u32);
            out.rules_by_concl[j.conclusion.index()].push(id);
            for p in &j.premises {
                let id = IncId(inc);
                inc += 1;
                out.rules_by_premise[p.premise.index()].push(id);
                match p.kind {
                    EdgeKind::Cur => out.exact_by_rev[p.premise.index()].push(id),
                    EdgeKind::Pin => out.pin_by_rev[p.premise.index()].push(id),
                    EdgeKind::Sem => {
                        out.sem_by_logical[i.revisions[p.premise.index()].logical.index()].push(id)
                    }
                    EdgeKind::Ref => unreachable!("canonical premises"),
                }
            }
            for r in &j.references {
                out.refs_by_target[r.index()].push(id);
            }
        }
        out
    }
}
