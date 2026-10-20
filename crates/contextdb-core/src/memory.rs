//! Conservative retained-allocation accounting. Excludes payload bodies (none
//! are stored in core), allocator bookkeeping, and B-tree node overhead. A value
//! above a budget disproves that budget; a lower value cannot certify a pass.
use crate::{Db, Maintainer, StatsSink};
use std::mem::size_of;
pub(crate) fn vec_bytes<T>(v: &Vec<T>) -> usize {
    v.capacity() * size_of::<T>()
}
pub(crate) fn bool_bytes(v: &Vec<bool>) -> usize {
    v.capacity()
}
#[derive(Clone, Debug)]
pub struct MemoryFootprint {
    pub inputs: usize,
    pub indexes: usize,
    pub maintenance: usize,
    pub topology: usize,
    pub query_cache: usize,
    pub history: usize,
}
impl MemoryFootprint {
    pub fn lower_bound_bytes(&self) -> usize {
        self.inputs
            + self.indexes
            + self.maintenance
            + self.topology
            + self.query_cache
            + self.history
    }
}
impl<M: Maintainer, S: StatsSink> Db<M, S> {
    pub fn memory_footprint(&self) -> MemoryFootprint {
        let i = &self.tables;
        let inputs = self.revisions.retained_bytes()
            + self.rules.retained_bytes()
            + vec_bytes(&i.logicals)
            + i.logicals
                .iter()
                .map(|l| l.key.capacity() + l.scope_lane.capacity())
                .sum::<usize>()
            + vec_bytes(&i.heads)
            + bool_bytes(&i.base)
            + vec_bytes(&i.metadata)
            + vec_bytes(&i.artifacts)
            + i.metadata
                .iter()
                .map(|m| {
                    m.tags
                        .iter()
                        .chain(&m.dirs)
                        .map(|s| size_of::<String>() + s.capacity())
                        .sum::<usize>()
                })
                .sum::<usize>()
            + i.validations
                .iter()
                .map(|((_, v), r)| {
                    size_of::<(
                        (contextdb_model::RuleId, contextdb_model::HeadVector),
                        contextdb_model::ValidationRecord,
                    )>() + vec_bytes(v)
                        + r.validator.capacity()
                })
                .sum::<usize>();
        let history = vec_bytes(&self.history)
            + self
                .history
                .iter()
                .map(|t| vec_bytes(&t.changes))
                .sum::<usize>();
        let mut query_cache = self.memo.borrow().retained_bytes();
        if let Some(d) = self.derived.get() {
            query_cache += vec_bytes(&d.nodes)
                + vec_bytes(&d.sat)
                + d.sat.iter().map(bool_bytes).sum::<usize>()
                + vec_bytes(&d.unsat)
                + bool_bytes(&d.active)
                + vec_bytes(&d.layer_active)
                + vec_bytes(&d.sem);
        }
        MemoryFootprint {
            inputs,
            indexes: self.indexes.retained_bytes(),
            maintenance: self.maintainer.retained_bytes(),
            topology: self.pk.retained_bytes() + vec_bytes(&self.topo.order),
            query_cache,
            history,
        }
    }
}
