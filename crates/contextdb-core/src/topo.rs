use contextdb_model::*;
use std::collections::{BTreeMap, BTreeSet};
/// Full Kahn reorder for P0. Virtual logical nodes participate in every check.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NaiveTopo {
    pub order: Vec<CycleNode>,
}
impl NaiveTopo {
    pub(crate) fn build(i: &Inputs, op_index: usize) -> Result<Self, TxError> {
        let mut edges: BTreeMap<CycleNode, BTreeSet<CycleNode>> = (0..i.revisions.len())
            .map(|n| (CycleNode::Revision(RevisionId(n as u32)), BTreeSet::new()))
            .collect();
        for rule in &i.rules {
            for p in &rule.premises {
                edges
                    .entry(CycleNode::Revision(p.premise))
                    .or_default()
                    .insert(CycleNode::Revision(rule.conclusion));
                if p.kind == EdgeKind::Sem {
                    let logical = i.revisions[p.premise.index()].logical;
                    let v = CycleNode::Logical(logical);
                    edges
                        .entry(v.clone())
                        .or_default()
                        .insert(CycleNode::Revision(rule.conclusion));
                    for (n, r) in i
                        .revisions
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| r.logical == logical)
                    {
                        let _ = r;
                        edges
                            .entry(CycleNode::Revision(RevisionId(n as u32)))
                            .or_default()
                            .insert(v.clone());
                    }
                }
            }
        }
        let mut degrees: BTreeMap<_, usize> = edges.keys().cloned().map(|n| (n, 0)).collect();
        for next in edges.values() {
            for node in next {
                *degrees.entry(node.clone()).or_default() += 1;
            }
        }
        let mut ready: BTreeSet<_> = degrees
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(n, _)| n.clone())
            .collect();
        let mut order = Vec::new();
        while let Some(n) = ready.pop_first() {
            for to in &edges[&n] {
                let degree = degrees.get_mut(to).expect("destination");
                *degree -= 1;
                if *degree == 0 {
                    ready.insert(to.clone());
                }
            }
            order.push(n);
        }
        if order.len() != edges.len() {
            // Deterministic diagnostic: sorted DFS, identical public path convention to the oracle.
            fn dfs(
                n: CycleNode,
                g: &BTreeMap<CycleNode, BTreeSet<CycleNode>>,
                done: &mut BTreeSet<CycleNode>,
                stack: &mut Vec<CycleNode>,
            ) -> Option<Vec<CycleNode>> {
                if let Some(pos) = stack.iter().position(|v| *v == n) {
                    let mut path = stack[pos..].to_vec();
                    path.push(n);
                    return Some(path);
                }
                if done.contains(&n) {
                    return None;
                }
                stack.push(n.clone());
                for next in &g[&n] {
                    if let Some(path) = dfs(next.clone(), g, done, stack) {
                        return Some(path);
                    }
                }
                stack.pop();
                done.insert(n);
                None
            }
            let mut done = BTreeSet::new();
            for n in edges.keys() {
                if let Some(path) = dfs(n.clone(), &edges, &mut done, &mut Vec::new()) {
                    return Err(TxError::CycleDetected { op_index, path });
                }
            }
            unreachable!("Kahn detected cycle")
        }
        Ok(Self { order })
    }
}
