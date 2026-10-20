use contextdb_model::RevisionId;

/// Deterministic work counters. Wall-clock timing is deliberately separate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitStats {
    pub seeds: u64,
    pub nodes_popped: u64,
    pub node_flips: u64,
    pub incidences_visited: u64,
    pub rules_reevaluated: u64,
    pub rule_flips: u64,
    pub heap_pushes: u64,
    pub pk_edges: u64,
    pub pk_slow_paths: u64,
    pub pk_forward: u64,
    pub pk_backward: u64,
    pub pk_reordered: u64,
    pub transient_resets: u64,
    pub status_nodes: Vec<RevisionId>,
    pub seed_nodes: Vec<RevisionId>,
    pub popped_nodes: Vec<RevisionId>,
}
#[derive(Clone, Copy, Debug)]
pub enum Work {
    Status(RevisionId),
    Seed(RevisionId),
    Pop(RevisionId),
    NodeFlip,
    Incidence,
    Rule,
    RuleFlip,
    Push,
    PkEdge,
    PkSlow,
    PkForward,
    PkBackward,
    PkReorder,
}
pub trait StatsSink: Clone + std::fmt::Debug + Default {
    fn reset(&mut self);
    fn record(&mut self, work: Work);
}
impl StatsSink for CommitStats {
    fn reset(&mut self) {
        *self = Self::default();
    }
    fn record(&mut self, work: Work) {
        match work {
            Work::Status(r) => self.status_nodes.push(r),
            Work::Seed(r) => {
                self.seeds += 1;
                self.seed_nodes.push(r);
            }
            Work::Pop(r) => {
                self.nodes_popped += 1;
                self.popped_nodes.push(r);
            }
            Work::NodeFlip => self.node_flips += 1,
            Work::Incidence => self.incidences_visited += 1,
            Work::Rule => self.rules_reevaluated += 1,
            Work::RuleFlip => self.rule_flips += 1,
            Work::Push => self.heap_pushes += 1,
            Work::PkEdge => self.pk_edges += 1,
            Work::PkSlow => self.pk_slow_paths += 1,
            Work::PkForward => self.pk_forward += 1,
            Work::PkBackward => self.pk_backward += 1,
            Work::PkReorder => self.pk_reordered += 1,
        }
    }
}
/// Zero-sized, monomorphized no-op sink; no counters or traces are allocated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoStats;
impl StatsSink for NoStats {
    #[inline]
    fn reset(&mut self) {}
    #[inline]
    fn record(&mut self, _: Work) {}
}
