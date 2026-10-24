//! Sequential transactions with exact incremental L0 and maintained L1/L2 columns.
//! Inputs are exported by value; callers cannot mutate stored inputs or derived state.
//! ```compile_fail
//! let mut db = contextdb_core::Db::default();
//! db.inputs_mut().heads.clear();
//! ```
// Keep diagnostic assertions active in optimized paranoid builds as well.
macro_rules! invariant_assert { ($($arg:tt)*) => { if cfg!(any(debug_assertions, feature="paranoid")) { assert!($($arg)*); } }; }
pub(crate) use invariant_assert;
mod arena;
mod cert;
pub use cert::{HistoryUnavailable, IssueRequest, LogLink};
mod fast;
mod history;
mod kernel;
pub use kernel::{
    BranchInfo, BranchView, Import, Kernel, KernelSnapshot, KernelState, valid_branch_name,
};
mod memory;
mod merge;
pub use history::{EpochDiff, Impact, NodeChange, diff_rows};
pub use memory::MemoryFootprint;
pub mod engine;
mod layers;
mod query;
pub use query::Proof;
use std::sync::OnceLock;
mod pk;
mod stage;
mod stats;
mod topo;
pub use engine::{Counting, FullRecompute, Maintainer};
pub use pk::PearceKelly;
pub use stats::{CommitStats, NoStats, StatsSink};
mod txn;
mod view;
mod whatif;
pub use arena::{Adjacency, Indexes};
use arena::{LogicalArena, RevisionArena, RuleArena};
use contextdb_model::*;
pub use topo::NaiveTopo;
pub use view::InputView;

/// Nondeterministic wall time, excluded from CommitStats, digest and history.
#[cfg(feature = "timing")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitTiming {
    pub total_nanos: u128,
}
/// How the multi-branch kernel commits into one branch's database (ADR 0013
/// r0.5): at the global epoch, for any branch id, with the global key
/// namespace.
#[derive(Clone, Copy, Default)]
pub struct CommitContext<'a> {
    /// The global epoch of this commit; None means `epoch + 1`.
    pub epoch: Option<Epoch>,
    /// Set by the kernel: the batch's branch is this database's branch.
    pub kernel: bool,
    /// Keys taken by logicals not visible in this branch.
    pub foreign_key: Option<&'a dyn Fn(&str) -> bool>,
    /// Receives the revisions and rules the batch staged, in staging order,
    /// whether it commits or fails: the kernel gives them global ids.
    pub staged: Option<&'a std::cell::RefCell<Staged>>,
}
/// The objects a batch staged that the kernel may map to existing global
/// objects (cross-branch dedup, SPEC §6.1 contract 5 and ADR 0013 §11a).
#[derive(Clone, Debug, Default)]
pub struct Staged {
    pub revisions: Vec<Revision>,
    pub rules: Vec<Rule>,
}
#[derive(Clone, Debug)]
pub struct Db<M: Maintainer = Counting, S: StatsSink = NoStats> {
    maintainer: M,
    stats: S,
    pk: PearceKelly,
    tables: Inputs,
    logicals: LogicalArena,
    revisions: RevisionArena,
    rules: RuleArena,
    epoch: Epoch,
    derived: OnceLock<DerivedSnapshot>,
    layers: layers::LayerColumns,
    changes: cert::ChangeLog,
    indexes: Indexes,
    history: Vec<history::Transition>,
    initial_epoch: Epoch,
    /// Open only inside `what_if`.
    journal: Option<Box<whatif::Journal>>,
    /// Code anchors a merge made visible that this branch's observer has not
    /// yet seen (ADR 0013 §6.2, SD14b): they read `PENDING(ObservationLag)`
    /// until a watermark for their scope commits here.
    anchor_unobserved: std::collections::BTreeSet<LogicalId>,
    #[cfg(feature = "timing")]
    timing: Option<CommitTiming>,
}
impl<M: Maintainer, S: StatsSink> DerivedView for Db<M, S> {
    fn derived(&self) -> &DerivedSnapshot {
        self.derived.get_or_init(|| self.snapshot())
    }
}
impl Default for Db<Counting, NoStats> {
    fn default() -> Self {
        Self::new()
    }
}
impl<M: Maintainer, S: StatsSink> Db<M, S> {
    pub fn new() -> Self {
        Self {
            tables: Inputs::default(),
            logicals: LogicalArena::default(),
            revisions: RevisionArena::default(),
            rules: RuleArena::default(),
            epoch: Epoch(0),
            derived: OnceLock::new(),
            layers: layers::LayerColumns::default(),
            changes: cert::ChangeLog::default(),
            indexes: Indexes::default(),
            pk: PearceKelly::default(),
            maintainer: M::default(),
            stats: S::default(),
            history: Vec::new(),
            initial_epoch: Epoch(0),
            journal: None,
            anchor_unobserved: Default::default(),
            #[cfg(feature = "timing")]
            timing: None,
        }
    }
    pub fn stats(&self) -> &S {
        &self.stats
    }
    pub fn counting_state(&self) -> &engine::CountingState {
        self.maintainer.state()
    }
    pub fn dynamic_topo(&self) -> &PearceKelly {
        &self.pk
    }
    pub fn at_epoch(epoch: Epoch) -> Self {
        Self {
            epoch,
            initial_epoch: epoch,
            ..Self::new()
        }
    }
    /// Rebuilds a database from a durable input snapshot (ADR 0006 §6): the
    /// same maintenance as one structural commit adding every input at once.
    /// History starts empty at `epoch`; derived state must equal replay (PROP10).
    pub fn from_inputs(mut inputs: Inputs, epoch: Epoch) -> Result<Self, TxError> {
        let mut db = Self::at_epoch(epoch);
        let mut stats = S::default();
        let mut pk = PearceKelly::default();
        pk.sync(&inputs, 0, &mut stats)?;
        let delta = NetDelta {
            base_flips: (0..inputs.base.len())
                .filter(|n| inputs.base[*n])
                .map(|n| RevisionId(n as u32))
                .collect(),
            head_moves: inputs
                .heads
                .iter()
                .enumerate()
                .map(|(n, h)| (LogicalId(n as u32), None, *h))
                .collect(),
            enable_flips: Vec::new(),
            new_rules: (0..inputs.rules.len()).map(|n| RuleId(n as u32)).collect(),
        };
        let indexes = Indexes::build(&inputs);
        db.maintainer
            .update(&inputs, &indexes, &pk, &delta, &mut stats);
        db.revisions.reserve(inputs.revisions.len());
        for r in &inputs.revisions {
            db.revisions.append(r);
        }
        db.rules.reserve(&inputs.rules);
        for j in &inputs.rules {
            db.rules.append(j);
        }
        db.logicals.reserve(&inputs.logicals);
        for l in &inputs.logicals {
            db.logicals.append(l);
        }
        db.logicals.set_metadata(&inputs.metadata);
        inputs.logicals = Vec::new();
        inputs.metadata = Vec::new();
        inputs.revisions = Vec::new();
        inputs.rules = Vec::new();
        db.tables = inputs;
        db.pk = pk;
        db.indexes = indexes;
        db.layers =
            layers::LayerColumns::rebuild(&db.view(), db.maintainer.state(), &db.indexes, &db.pk);
        // Baseline post-states at `epoch`: pre-states for later diffs. DIFF
        // windows start after `epoch`, so the baseline is never reported itself.
        let baseline = db
            .derived()
            .nodes
            .iter()
            .enumerate()
            .map(|(n, state)| NodeChange {
                revision: RevisionId(n as u32),
                before: None,
                after: Some(state.clone()),
            })
            .collect();
        db.record(epoch, baseline);
        #[cfg(feature = "paranoid")]
        assert_eq!(db.derived(), &contextdb_oracle::jacobi(&db.inputs()));
        Ok(db)
    }
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
    pub fn inputs(&self) -> Inputs {
        let mut i = self.tables.clone();
        (i.logicals, i.metadata) = self.logicals.export();
        i.revisions = self.revisions.export();
        i.rules = self.rules.export();
        i
    }
    pub fn indexes(&self) -> &Indexes {
        &self.indexes
    }
    /// Reference order derived from the dynamic topology; not retained separately.
    pub fn topo(&self) -> NaiveTopo {
        self.pk.diagnostic_order()
    }
    pub fn digest(&self) -> ContentHash {
        let mut h = blake3::Hasher::new();
        h.update(&self.inputs().canonical_bytes());
        h.update(&self.derived().canonical_bytes());
        // Part of the branch overlay; absent (and hashed as nothing) unless a
        // merge set it, so single-branch digests are unchanged.
        if !self.anchor_unobserved.is_empty() {
            h.update(b"anchor_unobserved");
            for l in &self.anchor_unobserved {
                h.update(&l.0.to_le_bytes());
            }
        }
        *h.finalize().as_bytes()
    }
    pub fn derived_digest(&self) -> ContentHash {
        *blake3::hash(&self.derived().canonical_bytes()).as_bytes()
    }
    fn view(&self) -> view::StoredView<'_> {
        view::StoredView {
            tables: &self.tables,
            logicals: &self.logicals,
            revisions: &self.revisions,
            rules: &self.rules,
        }
    }
    pub fn node(&self, r: RevisionId) -> Result<NodeState, TxError> {
        if r.index() >= self.revisions.len() {
            return Err(TxError::UnknownRef);
        }
        Ok(self
            .layers
            .node_state(&self.view(), r, self.maintainer.state().row(r)))
    }
    /// The full derived snapshot from the maintained columns.
    fn snapshot(&self) -> DerivedSnapshot {
        let view = self.view();
        let c = self.maintainer.state();
        DerivedSnapshot {
            nodes: (0..self.revisions.len())
                .map(|n| {
                    let r = RevisionId(n as u32);
                    self.layers.node_state(&view, r, c.row(r))
                })
                .collect(),
            sem: (0..self.rules.len())
                .map(|n| self.layers.sem(RuleId(n as u32)))
                .collect(),
            layer_active: (0..self.rules.len())
                .map(|n| self.layers.layer_active(RuleId(n as u32)))
                .collect(),
            sat: self
                .indexes
                .rule_start
                .windows(2)
                .map(|w| c.sat[w[0] as usize..w[1] as usize].to_vec())
                .collect(),
            unsat: c.unsat.clone(),
            active: c.counted.clone(),
        }
    }
    pub fn current(&self, l: LogicalId) -> Result<(RevisionId, NodeState), TxError> {
        let r = *self
            .tables
            .heads
            .get(l.index())
            .ok_or(TxError::UnknownRef)?;
        Ok((r, self.node(r)?))
    }
    /// Recomputes the oracle's per-node L0 definition for `sample` from the
    /// inputs and the stored premise bits, and compares it with the counting
    /// state (store open self-check, ADR 0006 §4 step 7). Returns the first
    /// mismatching revision. The graph is acyclic, so each node is checked
    /// against its own premises.
    pub fn verify_counting_sample(&self, sample: &[RevisionId]) -> Result<(), RevisionId> {
        let view = self.view();
        let c = self.maintainer.state();
        for r in sample {
            let n = r.index();
            if n >= view.revision_count() {
                return Err(*r);
            }
            let holds = |j: RuleId| {
                view.enabled(j)
                    && view.premises(j).all(|p| {
                        c.s[p.premise.index()]
                            && (p.kind != EdgeKind::Cur
                                || view.head(view.revision_logical(p.premise)) == p.premise)
                    })
            };
            let rules = self.indexes.rules_by_concl.row(n);
            let support = rules.iter().filter(|j| holds(**j)).count() as u32;
            let base = view.base(*r);
            let witness = if base {
                Some(Witness::Base)
            } else {
                rules.iter().find(|j| holds(**j)).map(|j| Witness::Rule(*j))
            };
            if c.support[n] != support || c.s[n] != (base || support > 0) || c.witness[n] != witness
            {
                return Err(*r);
            }
        }
        Ok(())
    }
    pub fn revision_count(&self) -> usize {
        self.revisions.len()
    }
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
    pub fn artifact_count(&self) -> usize {
        self.tables.artifacts.len()
    }
    /// Provenance of an imported revision (kernel only; not part of any
    /// derived state).
    pub(crate) fn set_revision_origin(&mut self, r: RevisionId, origin: &Revision) {
        self.revisions
            .set_origin(r, origin.author, origin.origin_branch, origin.origin_epoch);
    }
    pub(crate) fn set_rule_origin(&mut self, j: RuleId, branch: BranchId, epoch: Epoch) {
        self.rules.set_origin(j, branch, epoch);
    }
    /// Logical by key (SPEC §6.1 contract 6).
    pub fn logical_by_key(&self, key: &str) -> Option<LogicalId> {
        self.logicals.find_key(key)
    }
    pub fn logical_count(&self) -> usize {
        self.logicals.len()
    }
    /// `(key, kind, scope_lane)` of a logical.
    pub fn logical_info(&self, l: LogicalId) -> Option<(String, RecordKind, String)> {
        (l.index() < self.logicals.len()).then(|| {
            (
                self.logicals.export_key(l),
                self.logicals.kind(l),
                self.logicals.scope_lane(l).to_owned(),
            )
        })
    }
    /// Current head of a logical.
    pub fn head(&self, l: LogicalId) -> Option<RevisionId> {
        self.tables.heads.get(l.index()).copied()
    }
    pub fn revision_logical(&self, r: RevisionId) -> Option<LogicalId> {
        (r.index() < self.revisions.len()).then(|| self.revisions.logical(r))
    }
    /// Rules concluding `r` with their enabled bit, premises and references.
    pub fn rules_concluding(
        &self,
        r: RevisionId,
    ) -> Vec<(RuleId, bool, Vec<Incidence>, Vec<RevisionId>)> {
        let view = self.view();
        self.indexes
            .rules_by_concl
            .row(r.index())
            .iter()
            .map(|j| {
                (
                    *j,
                    view.enabled(*j),
                    view.premises(*j).collect(),
                    view.references(*j).to_vec(),
                )
            })
            .collect()
    }
    /// Committed observer watermark of a scope (SD18.6).
    /// Whether a merged code anchor still awaits this branch's observer.
    pub fn anchor_unobserved(&self, l: LogicalId) -> bool {
        self.anchor_unobserved.contains(&l)
    }
    /// Every merged code anchor still awaiting this branch's observer
    /// (snapshots, ADR 0013 §8).
    pub fn anchors_unobserved(&self) -> Vec<LogicalId> {
        self.anchor_unobserved.iter().copied().collect()
    }
    pub(crate) fn set_anchors_unobserved(&mut self, all: &[LogicalId]) {
        self.anchor_unobserved = all.iter().copied().collect();
    }
    pub fn watermark(&self, scope: &str) -> Option<&[u8]> {
        self.tables.watermarks.get(scope).map(Vec::as_slice)
    }
    /// Current base bit of a revision (store projections, base_history).
    pub fn is_base(&self, r: RevisionId) -> Result<bool, TxError> {
        self.tables
            .base
            .get(r.index())
            .copied()
            .ok_or(TxError::UnknownRef)
    }
    /// Payload hash of a revision (payload bodies live in the store).
    pub fn revision_content(&self, r: RevisionId) -> Result<ContentHash, TxError> {
        (r.index() < self.revisions.len())
            .then(|| self.revisions.content(r))
            .ok_or(TxError::UnknownRef)
    }
    pub fn current_l0(&self, l: LogicalId) -> Result<(RevisionId, bool), TxError> {
        let r = *self
            .tables
            .heads
            .get(l.index())
            .ok_or(TxError::UnknownRef)?;
        Ok((r, self.maintainer.state().s[r.index()]))
    }
    pub fn status(&self, r: RevisionId) -> Result<Summary, TxError> {
        Ok(contextdb_oracle::summarize(&self.node(r)?.dimensions))
    }
    pub fn why(&self, r: RevisionId) -> Result<Explanation, TxError> {
        query::why(&self.view(), self.maintainer.state(), &self.indexes, r)
    }
    pub fn why_depth(&self, r: RevisionId, depth: usize) -> Result<Proof, TxError> {
        query::proof(
            &self.view(),
            self.maintainer.state(),
            &self.indexes,
            r,
            depth,
        )
    }
    pub fn why_not(&self, r: RevisionId, depth: usize) -> Result<WhyNot, TxError> {
        query::why_not(
            &self.view(),
            self.maintainer.state(),
            &self.indexes,
            &self.layers,
            r,
            depth,
        )
    }
    #[cfg(feature = "timing")]
    pub fn timing(&self) -> Option<&CommitTiming> {
        self.timing.as_ref()
    }
    pub fn commit(&mut self, b: &Batch) -> Result<CommitResult, TxError> {
        self.commit_with(b, &AllPresent)
    }
    /// Commit with payload availability for `MissingBlob` (P2 store; replay uses AllPresent).
    pub fn commit_with(
        &mut self,
        b: &Batch,
        blobs: &dyn BlobPresence,
    ) -> Result<CommitResult, TxError> {
        self.commit_in(b, blobs, &CommitContext::default())
    }
    /// Commit for the multi-branch kernel (see `CommitContext`).
    pub fn commit_in(
        &mut self,
        b: &Batch,
        blobs: &dyn BlobPresence,
        ctx: &CommitContext<'_>,
    ) -> Result<CommitResult, TxError> {
        #[cfg(feature = "timing")]
        let start = std::time::Instant::now();
        // Rebind mutates a RuleId's premises. Retain only its pre-change rows
        // so historical observation closures never traverse today's incidences.
        let rebound: std::collections::BTreeMap<_, _> = b
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Rebind {
                    rule: RuleRef::Existing(rule),
                    ..
                } if rule.index() < self.rules.len() => {
                    Some((*rule, self.view().premises(*rule).collect::<Vec<_>>()))
                }
                _ => None,
            })
            .collect();
        let result = self.commit_inner(b, blobs, ctx);
        if let Ok(committed) = &result {
            for (rule, old) in rebound {
                self.changes
                    .rebindings
                    .entry(rule)
                    .or_default()
                    .push((committed.epoch, old));
            }
        }
        #[cfg(feature = "timing")]
        if result.is_ok() {
            self.timing = Some(CommitTiming {
                total_nanos: start.elapsed().as_nanos(),
            });
        }
        result
    }
    fn commit_inner(
        &mut self,
        b: &Batch,
        blobs: &dyn BlobPresence,
        ctx: &CommitContext<'_>,
    ) -> Result<CommitResult, TxError> {
        if b.branch != MAIN && !ctx.kernel {
            return Err(TxError::UnknownRef);
        }
        let next = match ctx.epoch {
            Some(e) => {
                crate::invariant_assert!(e > self.epoch, "commit epochs increase");
                e
            }
            None => Epoch(
                self.epoch
                    .0
                    .checked_add(1)
                    .ok_or(TxError::CapacityExceeded {
                        kind: CapacityKind::Epoch,
                    })?,
            ),
        };
        if fast::eligible(b) {
            if let Some(staged) = ctx.staged {
                *staged.borrow_mut() = Staged::default();
            }
            return self.commit_mutable(b, next);
        }
        self.commit_structural(b, blobs, next, ctx)
    }
    /// Structural batches (ADR 0007): validate against an overlay, publish
    /// incrementally, then walk statuses from the net-delta seeds.
    fn commit_structural(
        &mut self,
        b: &Batch,
        blobs: &dyn BlobPresence,
        next: Epoch,
        ctx: &CommitContext<'_>,
    ) -> Result<CommitResult, TxError> {
        #[cfg(any(debug_assertions, feature = "paranoid"))]
        let pk_before = self.pk.clone();
        let pk_mark = self.pk.begin();
        let mut stats = S::default();
        let mut allocations = Allocations::default();
        let mut staged = stage::Stage::new(
            &self.tables,
            &self.logicals,
            &self.revisions,
            &self.rules,
            &self.indexes,
        );
        staged.foreign_key = ctx.foreign_key;
        let pk = &mut self.pk;
        let result = (|| {
            for (n, op) in b.ops.iter().enumerate() {
                txn::apply(
                    &mut staged,
                    &mut allocations,
                    b,
                    op,
                    n,
                    next,
                    pk,
                    &mut stats,
                    blobs,
                )?;
            }
            txn::final_policy(&staged)?;
            txn::final_read_set(&staged)?;
            pk.sync(&staged, b.ops.len().saturating_sub(1), &mut stats)?;
            Ok::<_, TxError>(next)
        })();
        let parts = staged.into_parts();
        if let Some(sink) = ctx.staged {
            *sink.borrow_mut() = Staged {
                revisions: parts.new_revisions.clone(),
                rules: parts.new_rules.clone(),
            };
        }
        let epoch = match result {
            Ok(epoch) => epoch,
            Err(error) => {
                self.pk.rollback(pk_mark);
                #[cfg(any(debug_assertions, feature = "paranoid"))]
                assert_eq!(
                    self.pk, pk_before,
                    "PK rollback restores the published graph"
                );
                return Err(error);
            }
        };
        Ok(self.publish(parts, pk_mark, allocations, epoch, stats))
    }
    /// Publishes a validated stage (ADR 0007): append, apply overrides while
    /// recording pre-states, then maintenance and the status walk.
    fn publish(
        &mut self,
        parts: stage::Parts,
        pk_mark: pk::PkMark,
        allocations: Allocations,
        epoch: Epoch,
        stats: S,
    ) -> CommitResult {
        let stage::Parts {
            new_logicals,
            new_heads,
            new_revisions,
            new_base,
            new_rules,
            new_artifacts,
            heads,
            base,
            enabled,
            artifact_open,
            metadata,
            validations,
            watermarks,
            policy,
        } = parts;
        match &mut self.journal {
            Some(journal) => journal.pk = Some(pk_mark),
            None => self.pk.commit(),
        }
        // Publication: append, then apply overrides while recording pre-states.
        let (old_logicals, old_revisions, old_rules) =
            (self.logicals.len(), self.revisions.len(), self.rules.len());
        let mut undo = fast::Undo::default();
        self.logicals.reserve(&new_logicals);
        memory::grow(&mut self.tables.heads, new_heads.len());
        for (l, h) in new_logicals.iter().zip(&new_heads) {
            self.logicals.append(l);
            self.tables.heads.push(*h);
        }
        for (l, h) in heads {
            let old = std::mem::replace(&mut self.tables.heads[l.index()], h);
            if old != h {
                undo.heads.insert(l, old);
            }
        }
        self.revisions.reserve(new_revisions.len());
        memory::grow(&mut self.tables.base, new_base.len());
        for (r, b) in new_revisions.iter().zip(&new_base) {
            self.revisions.append(r);
            self.tables.base.push(*b);
        }
        for (r, b) in base {
            let old = std::mem::replace(&mut self.tables.base[r.index()], b);
            if old != b {
                undo.base.insert(r, old);
            }
        }
        self.rules.reserve(&new_rules);
        for j in &new_rules {
            self.rules.append(j);
        }
        for (j, e) in enabled {
            let old = std::mem::replace(&mut self.rules.enabled[j.index()], e);
            if old != e {
                undo.enabled.insert(j, old);
            }
        }
        for (key, record) in validations {
            let old = self.tables.validations.insert(key.clone(), record);
            undo.validations.entry(key).or_insert(old);
        }
        if let Some(p) = policy {
            self.tables.policy = p;
        }
        if let Some(journal) = &mut self.journal {
            for scope in watermarks.keys() {
                let old = self.tables.watermarks.get(scope).cloned();
                journal.watermarks.entry(scope.clone()).or_insert(old);
            }
            for l in metadata.keys().filter(|l| l.index() < old_logicals) {
                journal
                    .metadata
                    .entry(*l)
                    .or_insert_with(|| self.logicals.metadata(*l));
            }
            for a in artifact_open.keys() {
                let old = self.tables.artifacts[a.index()].open;
                journal.artifact_open.entry(*a).or_insert(old);
            }
        }
        // A watermark commit means the scope was scanned: merged anchors of
        // that scope are observed now (ADR 0013 §6.2).
        if !self.anchor_unobserved.is_empty() && !watermarks.is_empty() {
            let logicals = &self.logicals;
            self.anchor_unobserved
                .retain(|l| !watermarks.contains_key(logicals.scope_lane(*l)));
        }
        // Advances only (ADR 0008 §6.3): a committed position that differs.
        let watermark_moves: Vec<String> = watermarks
            .iter()
            .filter(|(scope, position)| self.tables.watermarks.get(*scope) != Some(*position))
            .map(|(scope, _)| scope.clone())
            .collect();
        self.tables.watermarks.extend(watermarks);
        for (l, m) in metadata {
            self.logicals.put_metadata(l, m);
        }
        // The SD17 gate's input: open Conflicts per target, which seed the
        // status walk when they change (SPEC §3.4 Phase 3b).
        let mut conflict_seeds = Vec::new();
        let changed = (artifact_open.iter())
            .filter(|(a, open)| {
                let art = &self.tables.artifacts[a.index()];
                art.ty == ArtifactType::Conflict && art.open != **open
            })
            .map(|(a, open)| (self.tables.artifacts[a.index()].target, *open));
        let opened = (new_artifacts.iter())
            .filter(|a| a.ty == ArtifactType::Conflict && a.open)
            .map(|a| (a.target, true));
        for (target, open) in changed.chain(opened).collect::<Vec<_>>() {
            if let Some(journal) = &mut self.journal {
                let old = self.indexes.open_conflicts.get(&target).copied();
                journal.conflicts.entry(target).or_insert(old);
            }
            self.indexes.adjust_conflicts(target, open);
            conflict_seeds.push(target);
        }
        for (a, open) in artifact_open {
            self.tables.artifacts[a.index()].open = open;
        }
        self.tables.artifacts.extend(new_artifacts);
        let view = view::StoredView {
            tables: &self.tables,
            logicals: &self.logicals,
            revisions: &self.revisions,
            rules: &self.rules,
        };
        let compact = self.journal.is_none();
        self.indexes
            .append(&view, old_revisions, old_rules, compact);
        #[cfg(feature = "paranoid")]
        assert!(self.indexes == Indexes::build(&self.inputs()));
        let mut base_flips: Vec<RevisionId> = undo.base.keys().copied().collect();
        base_flips.extend(
            (old_revisions..self.revisions.len())
                .filter(|n| self.tables.base[*n])
                .map(|n| RevisionId(n as u32)),
        );
        let mut head_moves: Vec<_> = undo
            .heads
            .iter()
            .map(|(l, old)| (*l, Some(*old), self.tables.heads[l.index()]))
            .collect();
        head_moves.extend(
            (old_logicals..self.logicals.len())
                .map(|n| (LogicalId(n as u32), None, self.tables.heads[n])),
        );
        let delta = NetDelta {
            base_flips,
            head_moves,
            enable_flips: undo.enabled.keys().copied().collect(),
            new_rules: (old_rules..self.rules.len())
                .map(|n| RuleId(n as u32))
                .collect(),
        };
        // New revisions, new rules' conclusions and revalidated rules' conclusions.
        let mut extra: std::collections::BTreeSet<RevisionId> = (old_revisions
            ..self.revisions.len())
            .map(|n| RevisionId(n as u32))
            .collect();
        extra.extend(delta.new_rules.iter().map(|j| self.rules.conclusion(*j)));
        extra.extend(
            undo.validations
                .keys()
                .map(|(j, _)| self.rules.conclusion(*j)),
        );
        extra.extend(conflict_seeds);
        let revalidated: Vec<RuleId> = undo.validations.keys().map(|(j, _)| *j).collect();
        self.finish(
            undo,
            delta,
            allocations,
            extra,
            revalidated,
            watermark_moves,
            old_revisions,
            epoch,
            stats,
        )
    }
    fn commit_mutable(&mut self, b: &Batch, epoch: Epoch) -> Result<CommitResult, TxError> {
        let undo = fast::Undo::stage(
            &mut self.tables,
            &self.logicals,
            &self.revisions,
            &mut self.rules,
            b,
        )?;
        let delta = undo.delta(&self.tables, &self.rules);
        let old_revisions = self.revisions.len();
        Ok(self.finish(
            undo,
            delta,
            Allocations::default(),
            Default::default(),
            Vec::new(),
            Vec::new(),
            old_revisions,
            epoch,
            S::default(),
        ))
    }
    /// Counting mode: the rank-ordered status walker. Each popped node first
    /// gets its pre-state from the unchanged columns, then its dirty rules and
    /// incidences are re-evaluated (SPEC §3.4 Phase 3b). Changes in any of
    /// L0/L1/L2 or the stale reason propagate downstream (ADR 0008 §7).
    #[allow(clippy::too_many_arguments)]
    fn walk(
        &mut self,
        undo: &fast::Undo,
        update: &engine::Maintenance,
        mut seeds: std::collections::BTreeSet<RevisionId>,
        delta: &NetDelta,
        revalidated: &[RuleId],
        old_revisions: usize,
        stats: &mut S,
    ) -> (Vec<NodeChange>, Vec<RevisionId>) {
        use std::{cmp::Reverse, collections::BinaryHeap};
        let view = view::StoredView {
            tables: &self.tables,
            logicals: &self.logicals,
            revisions: &self.revisions,
            rules: &self.rules,
        };
        let before_view = fast::Before {
            view: view::StoredView {
                tables: &self.tables,
                logicals: &self.logicals,
                revisions: &self.revisions,
                rules: &self.rules,
            },
            undo,
        };
        let state = self.maintainer.state();
        self.layers.grow(&view, &self.indexes);
        seeds.extend(self.layers.seed(&view, &self.indexes, delta, revalidated));
        let mut heap = BinaryHeap::new();
        for r in &seeds {
            heap.push(Reverse((self.pk.rank(&CycleNode::Revision(*r)), *r)));
        }
        let mut changes = Vec::new();
        let mut visited = Vec::new();
        while let Some(Reverse((_, r))) = heap.pop() {
            stats.record(stats::Work::Status(r));
            visited.push(r);
            let before = (r.index() < old_revisions).then(|| {
                let old = update
                    .before
                    .get(&r)
                    .copied()
                    .unwrap_or_else(|| state.row(r));
                self.layers.node_state(&before_view, r, old)
            });
            if let Some(journal) = &mut self.journal
                && let Some(rows) = self.layers.footprint(&self.indexes, r, journal.layer_lens)
            {
                journal.layer_rows.push(rows);
            }
            self.layers.refresh(&view, state, &self.indexes, r);
            let after = self.layers.node_state(&view, r, state.row(r));
            let propagates = before.as_ref().is_none_or(|before| {
                before.l0 != after.l0
                    || before.l1 != after.l1
                    || before.l2 != after.l2
                    || before.dimensions.stale_reason != after.dimensions.stale_reason
            });
            if propagates {
                for c in self.layers.propagate(&view, &self.indexes, r) {
                    if seeds.insert(c) {
                        heap.push(Reverse((self.pk.rank(&CycleNode::Revision(c)), c)));
                    }
                }
                // Conclusions whose incidences were already dirty still need a
                // visit for inherited stale reasons.
                for id in self.indexes.rules_by_premise.row(r.index()) {
                    let c = view.conclusion(self.indexes.inc_rule[id.index()]);
                    if seeds.insert(c) {
                        heap.push(Reverse((self.pk.rank(&CycleNode::Revision(c)), c)));
                    }
                }
            }
            if before.as_ref() != Some(&after) {
                changes.push(NodeChange {
                    revision: r,
                    before,
                    after: Some(after),
                });
            }
        }
        #[cfg(any(debug_assertions, feature = "paranoid"))]
        self.layers
            .assert_matches_rebuild(&view, state, &self.indexes, &self.pk);
        changes.sort_by_key(|c| c.revision);
        (changes, visited)
    }
    /// FullRecompute mode: rebuild every L1/L2 column and compare every node.
    fn recompute_layers(
        &mut self,
        undo: &fast::Undo,
        update: &engine::Maintenance,
        old_revisions: usize,
    ) -> Vec<NodeChange> {
        let view = view::StoredView {
            tables: &self.tables,
            logicals: &self.logicals,
            revisions: &self.revisions,
            rules: &self.rules,
        };
        let before_view = fast::Before {
            view: view::StoredView {
                tables: &self.tables,
                logicals: &self.logicals,
                revisions: &self.revisions,
                rules: &self.rules,
            },
            undo,
        };
        let state = self.maintainer.state();
        let before: Vec<NodeState> = (0..old_revisions)
            .map(|n| {
                let r = RevisionId(n as u32);
                let old = update
                    .before
                    .get(&r)
                    .copied()
                    .unwrap_or_else(|| state.row(r));
                self.layers.node_state(&before_view, r, old)
            })
            .collect();
        self.layers = layers::LayerColumns::rebuild(&view, state, &self.indexes, &self.pk);
        let mut before = before.into_iter();
        (0..self.revisions.len())
            .filter_map(|n| {
                let r = RevisionId(n as u32);
                let after = self.layers.node_state(&view, r, state.row(r));
                let before = before.next();
                (before.as_ref() != Some(&after)).then_some(NodeChange {
                    revision: r,
                    before,
                    after: Some(after),
                })
            })
            .collect()
    }
    /// Shared publication tail: counting maintenance, the status walker from
    /// net-delta seeds (revisions at or above `old_revisions` are new and have
    /// no pre-state), snapshot patching, history and epoch.
    #[allow(clippy::too_many_arguments)]
    fn finish(
        &mut self,
        undo: fast::Undo,
        delta: NetDelta,
        allocations: Allocations,
        extra: std::collections::BTreeSet<RevisionId>,
        revalidated: Vec<RuleId>,
        watermark_moves: Vec<String>,
        old_revisions: usize,
        epoch: Epoch,
        mut stats: S,
    ) -> CommitResult {
        let mut seeds = fast::status_seeds(&self.view(), &self.indexes, &delta);
        seeds.extend(extra);
        let view = view::StoredView {
            tables: &self.tables,
            logicals: &self.logicals,
            revisions: &self.revisions,
            rules: &self.rules,
        };
        let update = self
            .maintainer
            .update(&view, &self.indexes, &self.pk, &delta, &mut stats);
        // Counter/witness-only changes must be reported, even without S flips.
        seeds.extend(update.before.keys().copied());
        let (changes, visited) = if M::INCREMENTAL_LAYERS {
            self.walk(
                &undo,
                &update,
                seeds,
                &delta,
                &revalidated,
                old_revisions,
                &mut stats,
            )
        } else {
            (
                self.recompute_layers(&undo, &update, old_revisions),
                Vec::new(),
            )
        };
        if M::INCREMENTAL_LAYERS {
            // Preserve an already materialized public snapshot: extend it for
            // new revisions/rules and patch changed rows. Rebuilding is O(N).
            if let Some(snapshot) = self.derived.get_mut() {
                for change in &changes {
                    let after = change.after.as_ref().expect("post-state").clone();
                    let n = change.revision.index();
                    if n < snapshot.nodes.len() {
                        snapshot.nodes[n] = after;
                    } else {
                        crate::invariant_assert!(n == snapshot.nodes.len());
                        snapshot.nodes.push(after);
                    }
                }
                let added = delta.new_rules.len();
                memory::grow(&mut snapshot.sat, added);
                memory::grow(&mut snapshot.unsat, added);
                memory::grow(&mut snapshot.active, added);
                memory::grow(&mut snapshot.layer_active, added);
                memory::grow(&mut snapshot.sem, added);
                for j in delta.new_rules.iter() {
                    crate::invariant_assert!(j.index() == snapshot.sat.len());
                    snapshot
                        .sat
                        .push(vec![false; self.indexes.rule_incidences(*j).len()]);
                    snapshot.unsat.push(0);
                    snapshot.active.push(false);
                    snapshot.layer_active.push([false; 2]);
                    snapshot.sem.push(SemState::NotRequired);
                }
                let c = self.maintainer.state();
                for r in &visited {
                    for j in self.indexes.rules_by_concl.row(r.index()) {
                        let n = j.index();
                        snapshot.sat[n].copy_from_slice(&c.sat[self.indexes.rule_incidences(*j)]);
                        snapshot.unsat[n] = c.unsat[n];
                        snapshot.active[n] = c.counted[n];
                        snapshot.layer_active[n] = self.layers.layer_active(*j);
                        snapshot.sem[n] = self.layers.sem(*j);
                    }
                }
            }
        } else {
            self.derived = OnceLock::new();
        }
        self.changes
            .record(epoch, &delta, &watermark_moves, old_revisions);
        if let Some(journal) = &mut self.journal {
            journal.record = Some((epoch, delta.clone(), watermark_moves, old_revisions));
            journal.undo = Some(undo);
        }
        let l0_flips = update.flips;
        let status_diff = changes.iter().map(|c| c.revision).collect();
        self.record(epoch, changes);
        self.epoch = epoch;
        self.stats = stats;
        #[cfg(feature = "paranoid")]
        assert_eq!(self.derived(), &contextdb_oracle::jacobi(&self.inputs()));
        CommitResult {
            epoch,
            allocations,
            net_delta: delta,
            l0_flips,
            status_diff,
        }
    }
}

/// Hashes the SPEC's canonical policy JSON, independent of enum discriminants.
pub fn policy_hash(p: &PolicyDoc) -> ContentHash {
    let mut names: Vec<_> = p
        .require_deps
        .iter()
        .map(|k| format!("\"{k:?}\""))
        .collect();
    names.sort();
    let json = format!(
        "{{\"require_deps\":[{}],\"version\":{}}}",
        names.join(","),
        p.version
    );
    *blake3::hash(json.as_bytes()).as_bytes()
}

/// Snapshot readers share a published `Db` across threads (ADR 0008 §3.3).
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<Db<Counting, NoStats>>();
    shared::<Db<FullRecompute, NoStats>>();
    shared::<Db<Counting, CommitStats>>();
};
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maintenance_journal_has_old_rows_without_inventing_new_prestate() {
        let create = |key: &str, content, base| Op::CreateLogical {
            key: key.into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: [content; 32],
            adopt_as_base: base,
        };
        let mut db = Db::default();
        db.commit(&Batch::new(vec![create("source", 0, true)]))
            .unwrap();
        let mut previous = db.maintainer.clone();
        let result = db
            .commit(&Batch::new(vec![
                create("conclusion", 1, false),
                Op::AddRule {
                    conclusion: RevRef::New(0),
                    incidences: vec![(RevisionId(0).into(), EdgeKind::Pin)],
                    enabled: true,
                },
                Op::RetractBase {
                    rev: RevisionId(0).into(),
                },
            ]))
            .unwrap();
        let journal = previous.update(
            &db.inputs(),
            &db.indexes,
            &db.pk,
            &result.net_delta,
            &mut NoStats,
        );
        assert_eq!(journal.flips, vec![RevisionId(0)]);
        assert_eq!(journal.before.len(), 1);
        let source = journal.before.get(&RevisionId(0)).unwrap();
        assert!(source.s);
        assert_eq!(source.support, 0);
        assert_eq!(source.witness, Some(Witness::Base));
        assert!(!journal.before.contains_key(&RevisionId(1)));
        assert_eq!(previous.state(), db.maintainer.state());
    }
    /// `logical_info` reports each logical's exact key, kind and lane, and
    /// nothing past the last logical (mutation closure for `export_key`).
    #[test]
    fn logical_info_reports_key_kind_and_lane() {
        let mut db = Db::default();
        db.commit(&Batch::new(vec![
            logical("db.engine", "repo", 1),
            logical("plan", "team", 2),
        ]))
        .unwrap();
        assert_eq!(
            db.logical_info(LogicalId(0)),
            Some(("db.engine".into(), RecordKind::Fact, "repo".into()))
        );
        assert_eq!(
            db.logical_info(LogicalId(1)),
            Some(("plan".into(), RecordKind::Fact, "team".into()))
        );
        assert_eq!(db.logical_info(LogicalId(2)), None);
    }
    fn logical(key: &str, lane: &str, content: u8) -> Op {
        Op::CreateLogical {
            key: key.into(),
            kind: RecordKind::Fact,
            scope_lane: lane.into(),
            content: [content; 32],
            adopt_as_base: true,
        }
    }
    #[test]
    fn fast_path_resolves_logical_keys_exactly() {
        let mut db = Db::default();
        db.commit(&Batch::new(vec![
            logical("a", "x", 0),
            logical("b", "y", 1),
            logical("c", "x", 2),
        ]))
        .unwrap();
        db.commit(&Batch::new(vec![Op::AddRevision {
            logical: LogicalRef::Existing(LogicalId(1)),
            content: [9; 32],
            adopt_as_base: false,
        }]))
        .unwrap();
        // SetHead is fast-path eligible; the key must select logical 1, not 0 or 2.
        db.commit(&Batch::new(vec![Op::SetHead {
            logical: LogicalRef::Key("b".into()),
            expected: RevisionId(1).into(),
            new: RevisionId(3).into(),
        }]))
        .unwrap();
        assert_eq!(db.current_l0(LogicalId(1)).unwrap().0, RevisionId(3));
        assert_eq!(db.current_l0(LogicalId(0)).unwrap().0, RevisionId(0));
        assert_eq!(
            db.commit(&Batch::new(vec![Op::SetHead {
                logical: LogicalRef::Key("missing".into()),
                expected: RevisionId(0).into(),
                new: RevisionId(0).into(),
            }])),
            Err(TxError::UnknownRef)
        );
        // Lanes are interned, but each logical exports its own lane.
        let lanes: Vec<_> = db
            .inputs()
            .logicals
            .into_iter()
            .map(|l| l.scope_lane)
            .collect();
        assert_eq!(lanes, ["x", "y", "x"]);
    }
    #[test]
    fn exact_impact_restores_structural_state_completely() {
        let mut db = Db::default();
        db.commit(&Batch::new(vec![logical("a", "x", 0)])).unwrap();
        let (inputs, digest) = (db.inputs(), db.digest());
        let (pk, topo, indexes) = (db.pk.clone(), db.topo(), db.indexes.clone());
        let preview = db
            .impact_exact(&Batch::new(vec![
                logical("b", "z", 1),
                Op::AddRule {
                    conclusion: RevRef::New(0),
                    incidences: vec![(RevisionId(0).into(), EdgeKind::Pin)],
                    enabled: true,
                },
                Op::SetMetadata {
                    logical: LogicalRef::Existing(LogicalId(0)),
                    tags: ["t".to_string()].into(),
                    dirs: Default::default(),
                },
            ]))
            .unwrap();
        assert_eq!(preview.allocations.new_revisions, vec![RevisionId(1)]);
        assert_eq!(db.inputs(), inputs);
        assert_eq!(db.digest(), digest);
        assert_eq!(
            (db.pk.clone(), db.topo(), db.indexes.clone()),
            (pk, topo, indexes)
        );
        assert_eq!(db.revisions.len(), 1);
        assert_eq!(db.rules.len(), 0);
    }
    #[test]
    fn policy_json_is_canonical_by_kind_name() {
        let policy = PolicyDoc {
            version: 1,
            require_deps: [
                RecordKind::Plan,
                RecordKind::CodeAnchor,
                RecordKind::Artifact,
            ]
            .into_iter()
            .collect(),
        };
        assert_eq!(
            policy_hash(&policy),
            *blake3::hash(br#"{"require_deps":["Artifact","CodeAnchor","Plan"],"version":1}"#)
                .as_bytes()
        );
    }
}
