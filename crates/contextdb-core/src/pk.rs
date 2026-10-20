use crate::{
    NaiveTopo,
    stats::{StatsSink, Work},
};
use contextdb_model::*;
use std::collections::BTreeSet;

const NONE: u32 = u32::MAX;

/// Pearce–Kelly (2006): bounded bidirectional discovery, stable reassignment.
/// Graph edges include disabled rules and Sem virtual logical nodes.
/// Node handles are dense u32; revision/logical lookups are direct columns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PearceKelly {
    rev_node: Vec<u32>,
    // NONE until the logical first gains a Sem incidence.
    logical_node: Vec<u32>,
    nodes: Vec<CycleNode>,
    out: Vec<Vec<u32>>,
    incoming: Vec<Vec<u32>>,
    order: Vec<u32>,
    rank: Vec<u32>,
    seen_revisions: usize,
    seen_rules: usize,
}
impl PearceKelly {
    pub(crate) fn retained_bytes(&self) -> usize {
        use crate::memory::vec_bytes as b;
        b(&self.rev_node)
            + b(&self.logical_node)
            + b(&self.nodes)
            + b(&self.out)
            + b(&self.incoming)
            + self.out.iter().chain(&self.incoming).map(b).sum::<usize>()
            + b(&self.order)
            + b(&self.rank)
    }
    fn lookup(&self, n: &CycleNode) -> Option<usize> {
        let id = match n {
            CycleNode::Revision(r) => self.rev_node.get(r.index()),
            CycleNode::Logical(l) => self.logical_node.get(l.index()),
        };
        id.copied().filter(|id| *id != NONE).map(|id| id as usize)
    }
    fn node(&mut self, n: CycleNode) -> usize {
        if let Some(id) = self.lookup(&n) {
            return id;
        }
        let id = self.nodes.len();
        let handle = u32::try_from(id)
            .ok()
            .filter(|h| *h < NONE)
            .expect("bounded node count");
        let (column, index) = match n {
            CycleNode::Revision(r) => (&mut self.rev_node, r.index()),
            CycleNode::Logical(l) => (&mut self.logical_node, l.index()),
        };
        if column.len() <= index {
            column.resize(index + 1, NONE);
        }
        column[index] = handle;
        self.nodes.push(n);
        self.out.push(Vec::new());
        self.incoming.push(Vec::new());
        self.rank.push(handle);
        self.order.push(handle);
        id
    }
    fn is_sem_logical(&self, l: LogicalId) -> bool {
        self.lookup(&CycleNode::Logical(l)).is_some()
    }
    pub fn rank(&self, n: &CycleNode) -> usize {
        self.rank[self.lookup(n).expect("known cycle node")] as usize
    }
    pub fn order(&self) -> impl Iterator<Item = &CycleNode> {
        self.order.iter().map(|n| &self.nodes[*n as usize])
    }
    pub fn valid(&self) -> bool {
        self.out
            .iter()
            .enumerate()
            .all(|(n, edges)| edges.iter().all(|v| self.rank[n] < self.rank[*v as usize]))
    }
    pub(crate) fn diagnostic_order(&self) -> NaiveTopo {
        NaiveTopo {
            order: self.order().cloned().collect(),
        }
    }
    fn edge<S: StatsSink>(
        &mut self,
        from: CycleNode,
        to: CycleNode,
        stats: &mut S,
    ) -> Result<(), ()> {
        let u = self.node(from);
        let v = self.node(to);
        let (uh, vh) = (u as u32, v as u32);
        if self.out[u].binary_search(&vh).is_ok() {
            return Ok(());
        }
        stats.record(Work::PkEdge);
        if self.rank[u] >= self.rank[v] {
            stats.record(Work::PkSlow);
            let low = self.rank[v];
            let high = self.rank[u];
            let mut forward = BTreeSet::new();
            let mut stack = vec![v];
            while let Some(n) = stack.pop() {
                if !forward.insert(n) {
                    continue;
                }
                stats.record(Work::PkForward);
                if n == u {
                    return Err(());
                }
                for next in &self.out[n] {
                    if self.rank[*next as usize] <= high {
                        stack.push(*next as usize);
                    }
                }
            }
            let mut backward = BTreeSet::new();
            stack.push(u);
            while let Some(n) = stack.pop() {
                if !backward.insert(n) {
                    continue;
                }
                stats.record(Work::PkBackward);
                for prev in &self.incoming[n] {
                    if self.rank[*prev as usize] >= low {
                        stack.push(*prev as usize);
                    }
                }
            }
            crate::invariant_assert!(forward.is_disjoint(&backward));
            let mut b: Vec<_> = backward.into_iter().collect();
            b.sort_unstable_by_key(|n| self.rank[*n]);
            let mut f: Vec<_> = forward.into_iter().collect();
            f.sort_unstable_by_key(|n| self.rank[*n]);
            let mut positions: Vec<_> = b.iter().chain(&f).map(|n| self.rank[*n]).collect();
            positions.sort_unstable();
            for (pos, n) in positions.into_iter().zip(b.into_iter().chain(f)) {
                self.order[pos as usize] = n as u32;
                self.rank[n] = pos;
                stats.record(Work::PkReorder);
            }
        }
        let pos = self.out[u].binary_search(&vh).unwrap_err();
        self.out[u].insert(pos, vh);
        let pos = self.incoming[v].binary_search(&uh).unwrap_err();
        self.incoming[v].insert(pos, uh);
        crate::invariant_assert!(self.rank[u] < self.rank[v]);
        Ok(())
    }
    /// Only entry for structural rule insertion. Prefixes may contain new revisions.
    pub(crate) fn sync<S: StatsSink>(
        &mut self,
        i: &Inputs,
        op_index: usize,
        stats: &mut S,
    ) -> Result<(), TxError> {
        let result: Result<(), ()> = (|| {
            for n in self.seen_revisions..i.revisions.len() {
                let r = RevisionId(n as u32);
                self.node(CycleNode::Revision(r));
                let logical = i.revisions[n].logical;
                if self.is_sem_logical(logical) {
                    self.edge(CycleNode::Revision(r), CycleNode::Logical(logical), stats)?;
                }
            }
            self.seen_revisions = i.revisions.len();
            for rule in &i.rules[self.seen_rules..] {
                for p in &rule.premises {
                    self.edge(
                        CycleNode::Revision(p.premise),
                        CycleNode::Revision(rule.conclusion),
                        stats,
                    )?;
                    if p.kind == EdgeKind::Sem {
                        let logical = i.revisions[p.premise.index()].logical;
                        if !self.is_sem_logical(logical) {
                            self.node(CycleNode::Logical(logical));
                            for (n, r) in i
                                .revisions
                                .iter()
                                .enumerate()
                                .filter(|(_, r)| r.logical == logical)
                            {
                                let _ = r;
                                self.edge(
                                    CycleNode::Revision(RevisionId(n as u32)),
                                    CycleNode::Logical(logical),
                                    stats,
                                )?;
                            }
                        }
                        self.edge(
                            CycleNode::Logical(logical),
                            CycleNode::Revision(rule.conclusion),
                            stats,
                        )?;
                    }
                }
            }
            self.seen_rules = i.rules.len();
            Ok(())
        })();
        if result.is_err() {
            return match NaiveTopo::build(i, op_index) {
                Err(error) => Err(error),
                Ok(_) => panic!("PK rejected a DAG"),
            };
        }
        crate::invariant_assert!(self.valid());
        Ok(())
    }
}
