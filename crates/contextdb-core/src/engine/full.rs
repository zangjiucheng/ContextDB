use super::{CountingState, Maintainer, Maintenance};
use crate::InputView;
use crate::{Indexes, PearceKelly, StatsSink, stats::Work};
use contextdb_model::*;

#[derive(Clone, Debug, Default)]
pub struct FullRecompute {
    state: CountingState,
}
impl Maintainer for FullRecompute {
    fn retained_bytes(&self) -> usize {
        self.state.retained_bytes()
    }
    fn state(&self) -> &CountingState {
        &self.state
    }
    fn update<S: StatsSink, V: InputView>(
        &mut self,
        i: &V,
        index: &Indexes,
        topo: &PearceKelly,
        _: &NetDelta,
        stats: &mut S,
    ) -> Maintenance {
        let before = (0..self.state.s.len())
            .map(|n| {
                let r = RevisionId(n as u32);
                (r, self.state.row(r))
            })
            .collect();
        let mut next = CountingState {
            s: vec![false; i.revision_count()],
            support: vec![0; i.revision_count()],
            unsat: vec![0; i.rule_count()],
            counted: vec![false; i.rule_count()],
            sat: vec![false; index.inc_rule.len()],
            witness: vec![None; i.revision_count()],
        };
        let mut flips = Vec::new();
        for node in topo.order() {
            let CycleNode::Revision(r) = node else {
                continue;
            };
            stats.record(Work::Seed(*r));
            stats.record(Work::Pop(*r));
            for j in index.rules_by_concl.row(r.index()) {
                stats.record(Work::Rule);

                for n in index.rule_incidences(*j) {
                    stats.record(Work::Incidence);
                    let Incidence { premise: p, kind } = index.incidence(i, n);
                    let yes = next.s[p.index()]
                        && (kind != EdgeKind::Cur || i.head(i.revision_logical(p)) == p);
                    next.sat[n] = yes;
                    next.unsat[j.index()] += u32::from(!yes);
                }
                let active = i.enabled(*j) && next.unsat[j.index()] == 0;
                next.counted[j.index()] = active;
                next.support[r.index()] += u32::from(active);
                if active && next.witness[r.index()].is_none() {
                    next.witness[r.index()] = Some(Witness::Rule(*j));
                }
                if active != self.state.counted.get(j.index()).copied().unwrap_or(false) {
                    stats.record(Work::RuleFlip);
                }
            }
            if i.base(*r) {
                next.witness[r.index()] = Some(Witness::Base);
            }
            next.s[r.index()] = i.base(*r) || next.support[r.index()] > 0;
            if next.s[r.index()] != self.state.s.get(r.index()).copied().unwrap_or(false) {
                flips.push(*r);
                stats.record(Work::NodeFlip);
            }
        }
        self.state = next;
        flips.sort_unstable();
        Maintenance { flips, before }
    }
}
