use crate::{
    NaiveTopo,
    stats::{StatsSink, Work},
};
use contextdb_model::*;
use std::collections::BTreeSet;

const NONE: u32 = u32::MAX;

/// Inputs Pearce–Kelly reads while syncing (published or staged, ADR 0007).
pub(crate) trait PkSource: crate::InputView {
    /// Revisions of a logical, for Sem virtual-node edges.
    fn revisions_of(&self, l: LogicalId) -> Vec<RevisionId>;
    /// Full inputs for the deterministic cycle diagnostic (error path only).
    fn export(&self) -> Inputs;
}
impl PkSource for Inputs {
    fn revisions_of(&self, l: LogicalId) -> Vec<RevisionId> {
        (0..self.revisions.len())
            .filter(|n| self.revisions[*n].logical == l)
            .map(|n| RevisionId(n as u32))
            .collect()
    }
    fn export(&self) -> Inputs {
        self.clone()
    }
}
/// Reversible change recorded while a staging journal is active.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PkUndo {
    Edge(u32, u32),
    Rank(u32, u32),
    Order(u32, u32),
    LogicalSlot(u32),
}
/// State to restore on rejection: lengths plus the journal position.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PkMark {
    nodes: usize,
    rev_node: usize,
    logical_node: usize,
    seen_revisions: usize,
    seen_rules: usize,
}

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
    /// Active only during staging; empty otherwise.
    journal: Option<Vec<PkUndo>>,
}
impl PearceKelly {
    /// Starts journalling changes so a rejected batch can be reverted.
    pub(crate) fn begin(&mut self) -> PkMark {
        self.journal = Some(Vec::new());
        PkMark {
            nodes: self.nodes.len(),
            rev_node: self.rev_node.len(),
            logical_node: self.logical_node.len(),
            seen_revisions: self.seen_revisions,
            seen_rules: self.seen_rules,
        }
    }
    pub(crate) fn commit(&mut self) {
        self.journal = None;
    }
    /// Reverts every change since `begin`: edges and reorders in reverse, then
    /// truncation of appended nodes (which reorders may have moved).
    pub(crate) fn rollback(&mut self, mark: PkMark) {
        for undo in self.journal.take().unwrap_or_default().into_iter().rev() {
            match undo {
                PkUndo::Edge(u, v) => {
                    let (u, v) = (u as usize, v as usize);
                    if let Ok(pos) = self.out[u].binary_search(&(v as u32)) {
                        self.out[u].remove(pos);
                    }
                    if let Ok(pos) = self.incoming[v].binary_search(&(u as u32)) {
                        self.incoming[v].remove(pos);
                    }
                }
                PkUndo::Rank(n, old) => self.rank[n as usize] = old,
                PkUndo::Order(pos, old) => self.order[pos as usize] = old,
                PkUndo::LogicalSlot(l) => self.logical_node[l as usize] = NONE,
            }
        }
        self.nodes.truncate(mark.nodes);
        self.out.truncate(mark.nodes);
        self.incoming.truncate(mark.nodes);
        self.rank.truncate(mark.nodes);
        self.order.truncate(mark.nodes);
        self.rev_node.truncate(mark.rev_node);
        self.logical_node.truncate(mark.logical_node);
        self.seen_revisions = mark.seen_revisions;
        self.seen_rules = mark.seen_rules;
    }
    fn record(&mut self, undo: PkUndo) {
        if let Some(j) = &mut self.journal {
            j.push(undo);
        }
    }
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
            crate::memory::resize(column, index + 1, NONE);
        }
        column[index] = handle;
        if let CycleNode::Logical(l) = n {
            self.record(PkUndo::LogicalSlot(l.0));
        }
        crate::memory::grow(&mut self.nodes, 1);
        crate::memory::grow(&mut self.out, 1);
        crate::memory::grow(&mut self.incoming, 1);
        crate::memory::grow(&mut self.rank, 1);
        crate::memory::grow(&mut self.order, 1);
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
                self.record(PkUndo::Order(pos, self.order[pos as usize]));
                self.record(PkUndo::Rank(n as u32, self.rank[n]));
                self.order[pos as usize] = n as u32;
                self.rank[n] = pos;
                stats.record(Work::PkReorder);
            }
        }
        // Edge lists grow exactly while small (most nodes have few edges), 1.5x after.
        fn insert_sorted(list: &mut Vec<u32>, x: u32) {
            if list.len() == list.capacity() {
                list.reserve_exact((list.len() / 2).max(1));
            }
            let pos = list.binary_search(&x).unwrap_err();
            list.insert(pos, x);
        }
        insert_sorted(&mut self.out[u], vh);
        insert_sorted(&mut self.incoming[v], uh);
        self.record(PkUndo::Edge(uh, vh));
        crate::invariant_assert!(self.rank[u] < self.rank[v]);
        Ok(())
    }
    /// Only entry for structural rule insertion. Prefixes may contain new revisions.
    pub(crate) fn sync<S: StatsSink>(
        &mut self,
        i: &impl PkSource,
        op_index: usize,
        stats: &mut S,
    ) -> Result<(), TxError> {
        let result: Result<(), ()> = (|| {
            for n in self.seen_revisions..i.revision_count() {
                let r = RevisionId(n as u32);
                self.node(CycleNode::Revision(r));
                let logical = i.revision_logical(r);
                if self.is_sem_logical(logical) {
                    self.edge(CycleNode::Revision(r), CycleNode::Logical(logical), stats)?;
                }
            }
            self.seen_revisions = i.revision_count();
            for n in self.seen_rules..i.rule_count() {
                let j = RuleId(n as u32);
                let conclusion = i.conclusion(j);
                let premises: Vec<_> = i.premises(j).collect();
                for p in premises {
                    self.edge(
                        CycleNode::Revision(p.premise),
                        CycleNode::Revision(conclusion),
                        stats,
                    )?;
                    if p.kind == EdgeKind::Sem {
                        let logical = i.revision_logical(p.premise);
                        if !self.is_sem_logical(logical) {
                            self.node(CycleNode::Logical(logical));
                            for r in i.revisions_of(logical) {
                                self.edge(
                                    CycleNode::Revision(r),
                                    CycleNode::Logical(logical),
                                    stats,
                                )?;
                            }
                        }
                        self.edge(
                            CycleNode::Logical(logical),
                            CycleNode::Revision(conclusion),
                            stats,
                        )?;
                    }
                }
            }
            self.seen_rules = i.rule_count();
            Ok(())
        })();
        if result.is_err() {
            return match NaiveTopo::build(&i.export(), op_index) {
                Err(error) => Err(error),
                Ok(_) => panic!("PK rejected a DAG"),
            };
        }
        crate::invariant_assert!(self.valid());
        Ok(())
    }
}
