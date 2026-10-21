use crate::InputView;
mod counting;
mod full;
use crate::{Indexes, PearceKelly, StatsSink};
use contextdb_model::*;
pub use counting::Counting;
pub use full::FullRecompute;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CountingState {
    pub s: Vec<bool>,
    pub support: Vec<u32>,
    pub unsat: Vec<u32>,
    pub counted: Vec<bool>,
    pub sat: Vec<bool>,
    pub witness: Vec<Option<Witness>>,
}
impl CountingState {
    pub(crate) fn retained_bytes(&self) -> usize {
        use crate::memory::{bool_bytes, vec_bytes as b};
        bool_bytes(&self.s)
            + b(&self.support)
            + b(&self.unsat)
            + bool_bytes(&self.counted)
            + bool_bytes(&self.sat)
            + b(&self.witness)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct CountingRow {
    pub s: bool,
    pub support: u32,
    pub witness: Option<Witness>,
}
impl CountingState {
    pub fn row(&self, r: RevisionId) -> CountingRow {
        CountingRow {
            s: self.s[r.index()],
            support: self.support[r.index()],
            witness: self.witness[r.index()],
        }
    }
}
#[derive(Debug, Default)]
pub struct Maintenance {
    pub flips: Vec<RevisionId>,
    /// Rows at entry for visited pre-existing revisions. The structural commit
    /// path computes its own snapshot diff and does not consume this journal.
    pub before: std::collections::BTreeMap<RevisionId, CountingRow>,
}
pub trait Maintainer: Clone + std::fmt::Debug + Default + Send + Sync {
    /// Whether L1/L2 columns are maintained incrementally (Counting) or rebuilt
    /// from scratch after every commit (FullRecompute, an independent exact mode).
    const INCREMENTAL_LAYERS: bool;
    fn update<S: StatsSink, V: InputView>(
        &mut self,
        i: &V,
        index: &Indexes,
        topo: &PearceKelly,
        delta: &NetDelta,
        stats: &mut S,
    ) -> Maintenance;
    fn state(&self) -> &CountingState;
    fn retained_bytes(&self) -> usize;
}
