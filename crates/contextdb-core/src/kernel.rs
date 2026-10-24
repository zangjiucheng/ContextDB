//! The multi-branch kernel (SPEC SD19, ADR 0013; implementation per r0.5).
//!
//! Each branch is an ordinary single-branch [`Db`] holding exactly the
//! objects visible in it, under branch-local dense ids. The kernel keeps the
//! branch table, the global key namespace and the global id counters, and
//! maps ids at its boundary: batches arrive and results leave with global
//! ids. Isolation therefore holds by construction: a commit touches one
//! branch's database. A fork shares the parent's database and id map
//! (`Arc`, O(1)); the first write to either side copies it.
//!
//! Revision and rule identity is global (SPEC §6.1 contract 5, ADR 0013
//! §11a): a branch that stages a revision or rule identical to one created
//! on another branch imports that object instead of allocating a new id.
use crate::{CommitContext, Counting, Db, Maintainer, NoStats, Staged, StatsSink};
#[path = "kernel_merge.rs"]
mod merge;
use contextdb_model::*;
use merge::SyncPoint;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

/// The id kinds the kernel maps.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Logical = 0,
    Revision = 1,
    Rule = 2,
    Artifact = 3,
}
/// Local ⇄ global ids of one branch.
#[derive(Clone, Debug, Default)]
struct IdMap {
    to_global: [Vec<u32>; 4],
    to_local: [HashMap<u32, u32>; 4],
    /// Whether some local id differs from its global id.
    renumbered: bool,
}
/// A global id with no local counterpart maps here, so the branch's own
/// resolver reports `UnknownRef` at the right Op (§6.2 order).
const INVISIBLE: u32 = u32::MAX;
impl IdMap {
    fn len(&self, k: Kind) -> usize {
        self.to_global[k as usize].len()
    }
    fn local(&self, k: Kind, global: u32) -> u32 {
        self.to_local[k as usize]
            .get(&global)
            .copied()
            .unwrap_or(INVISIBLE)
    }
    fn global(&self, k: Kind, local: u32) -> u32 {
        self.to_global[k as usize][local as usize]
    }
    fn push(&mut self, k: Kind, global: u32) {
        let local = self.to_global[k as usize].len() as u32;
        self.renumbered |= local != global;
        self.to_global[k as usize].push(global);
        self.to_local[k as usize].insert(global, local);
    }
}

/// A branch's table entry (SD19 supplement).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchInfo {
    pub id: BranchId,
    pub name: String,
    pub parent: Option<BranchId>,
    pub fork_epoch: Epoch,
    pub archived: bool,
}
/// An object a branch imported from another branch by dedup; with the
/// epoch, a `branch_visibility` row (ADR 0013 §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Import {
    Logical(LogicalId),
    Revision(RevisionId),
    Rule(RuleId),
    Artifact(ArtifactId),
}
#[derive(Clone, Debug)]
struct Branch<M: Maintainer, S: StatsSink> {
    info: BranchInfo,
    db: Arc<Db<M, S>>,
    ids: Arc<IdMap>,
    /// Rows written on this branch; children inherit them through the
    /// shared database (visibility rule 3).
    imports: Vec<(Import, Epoch)>,
}
/// A rule's canonical identity under global ids.
type RuleKey = (u32, Vec<(u32, u8)>, Vec<u32>);

#[derive(Clone, Debug)]
pub struct Kernel<M: Maintainer = Counting, S: StatsSink = NoStats> {
    branches: Vec<Branch<M, S>>,
    names: BTreeMap<String, BranchId>,
    /// The global logical key namespace, SD19's one exception.
    keys: HashSet<String>,
    /// Next global id per kind.
    next: [u32; 4],
    epoch: Epoch,
    /// Every global revision (global logical) and rule origin, and the
    /// dedup indexes over them.
    revisions: Vec<Revision>,
    rule_origins: Vec<(BranchId, Epoch)>,
    revision_ids: HashMap<(u32, ContentHash), u32>,
    rule_ids: HashMap<RuleKey, u32>,
    /// Every sync point, in creation order (ADR 0013 §6.1).
    syncs: Vec<SyncPoint<M, S>>,
}
impl Default for Kernel<Counting, NoStats> {
    fn default() -> Self {
        Self::new()
    }
}

/// Branch names use the git ref character set so that every ordinary git
/// branch maps one to one (D1); `main` is reserved.
pub fn valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "main"
        && !name.starts_with(['/', '-'])
        && !name.ends_with('/')
        && !name.contains("..")
        && !name.contains("//")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
}

impl<M: Maintainer, S: StatsSink> Kernel<M, S> {
    pub fn new() -> Self {
        Self::from_main(Db::new())
    }
    /// Wraps an existing single-branch database as `main`; its ids are the
    /// global ids (every store written before P6).
    pub fn from_main(db: Db<M, S>) -> Self {
        let mut ids = IdMap::default();
        let inputs = db.inputs();
        let counts = [
            inputs.logicals.len(),
            inputs.revisions.len(),
            inputs.rules.len(),
            inputs.artifacts.len(),
        ];
        for (k, n) in [Kind::Logical, Kind::Revision, Kind::Rule, Kind::Artifact]
            .into_iter()
            .zip(counts)
        {
            for g in 0..n as u32 {
                ids.push(k, g);
            }
        }
        let keys = inputs.logicals.iter().map(|l| l.key.clone()).collect();
        let revision_ids = (inputs.revisions.iter().enumerate())
            .map(|(n, r)| ((r.logical.0, r.content), n as u32))
            .collect();
        let rule_ids = (inputs.rules.iter().enumerate())
            .map(|(n, j)| (rule_key(j, |r| r.0), n as u32))
            .collect();
        let rule_origins = (inputs.rules.iter())
            .map(|j| (j.origin_branch, j.origin_epoch))
            .collect();
        let revisions = inputs.revisions;
        let epoch = db.epoch();
        Self {
            branches: vec![Branch {
                info: BranchInfo {
                    id: MAIN,
                    name: "main".into(),
                    parent: None,
                    fork_epoch: Epoch(0),
                    archived: false,
                },
                db: Arc::new(db),
                ids: Arc::new(ids),
                imports: Vec::new(),
            }],
            names: [("main".to_owned(), MAIN)].into(),
            keys,
            next: counts.map(|n| n as u32),
            epoch,
            revisions,
            rule_origins,
            revision_ids,
            rule_ids,
            syncs: Vec::new(),
        }
    }
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
    pub fn branches(&self) -> impl Iterator<Item = &BranchInfo> {
        self.branches.iter().map(|b| &b.info)
    }
    /// Whether `main`'s local ids still equal the global ids, so a reader of
    /// its single-branch database sees global ids (until protocol 4 makes
    /// every reader branch-aware).
    pub fn main_is_identity(&self) -> bool {
        !self.branches[0].ids.renumbered
    }
    /// Commits with blob presence and also returns a merge's report.
    pub fn commit_detailed_with(
        &mut self,
        b: &Batch,
        blobs: &dyn BlobPresence,
    ) -> Result<(CommitResult, Option<MergeReport>), TxError> {
        self.run(b, blobs, true)
    }
    pub fn branch_by_name(&self, name: &str) -> Option<BranchId> {
        self.names.get(name).copied()
    }
    /// Read access to one branch, with global ids.
    pub fn branch(&self, id: BranchId) -> Option<BranchView<'_, M, S>> {
        self.branches.get(id.0 as usize).map(|b| BranchView { b })
    }
    pub fn commit(&mut self, b: &Batch) -> Result<CommitResult, TxError> {
        self.commit_with(b, &AllPresent)
    }
    pub fn commit_with(
        &mut self,
        b: &Batch,
        blobs: &dyn BlobPresence,
    ) -> Result<CommitResult, TxError> {
        self.run(b, blobs, true).map(|(r, _)| r)
    }
    /// Commits and also returns a merge's report (the format-3 merge
    /// trailer; ADR 0013 §8).
    pub fn commit_detailed(
        &mut self,
        b: &Batch,
    ) -> Result<(CommitResult, Option<MergeReport>), TxError> {
        self.run(b, &AllPresent, true)
    }
    pub fn what_if_detailed(
        &mut self,
        b: &Batch,
    ) -> Result<(CommitResult, Option<MergeReport>), TxError> {
        self.run(b, &AllPresent, false)
    }
    /// `WHAT_IF` on one branch (ADR 0013 §5): the result the batch would
    /// have, with the global ids it would allocate; nothing changes.
    pub fn what_if(&mut self, b: &Batch) -> Result<CommitResult, TxError> {
        self.run(b, &AllPresent, false).map(|(r, _)| r)
    }
    fn run(
        &mut self,
        b: &Batch,
        blobs: &dyn BlobPresence,
        publish: bool,
    ) -> Result<(CommitResult, Option<MergeReport>), TxError> {
        // Batch level, in the §6.2 P6 order.
        let branch = self
            .branches
            .get(b.branch.0 as usize)
            .ok_or(TxError::UnknownBranch)?;
        if branch.info.archived {
            return Err(TxError::BranchArchived {
                name: branch.info.name.clone(),
            });
        }
        let sole = |op: &Op| matches!(op, Op::CreateBranch { .. } | Op::ArchiveBranch { .. });
        if let Some(op) = b.ops.iter().find(|op| sole(op))
            && b.ops.len() != 1
        {
            return Err(TxError::SoleOpRequired {
                op: op.name().into(),
            });
        }
        let merges = b
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Merge { .. }))
            .count();
        if merges > 1
            || (merges == 1
                && b.ops
                    .iter()
                    .any(|op| !matches!(op, Op::Merge { .. } | Op::ExpectHeads { .. })))
        {
            return Err(TxError::SoleOpRequired { op: "Merge".into() });
        }
        let next = Epoch(
            self.epoch
                .0
                .checked_add(1)
                .ok_or(TxError::CapacityExceeded {
                    kind: CapacityKind::Epoch,
                })?,
        );
        let mut report = None;
        let result = match b.ops.first() {
            Some(Op::CreateBranch { name }) if b.ops.len() == 1 => {
                self.create_branch(b.branch, name, next, publish)?
            }
            Some(Op::ArchiveBranch { name }) if b.ops.len() == 1 => {
                self.archive_branch(b.branch, name, next, publish)?
            }
            _ if merges == 1 => {
                let (result, r) = self.merge(b, next, publish)?;
                report = Some(r);
                result
            }
            _ => self.commit_ops(b, blobs, next, publish)?,
        };
        if publish {
            self.epoch = next;
        }
        Ok((result, report))
    }
    fn empty_result(epoch: Epoch) -> CommitResult {
        CommitResult {
            epoch,
            allocations: Allocations::default(),
            net_delta: NetDelta::default(),
            l0_flips: Vec::new(),
            status_diff: Vec::new(),
        }
    }
    fn create_branch(
        &mut self,
        parent: BranchId,
        name: &str,
        epoch: Epoch,
        publish: bool,
    ) -> Result<CommitResult, TxError> {
        if !valid_branch_name(name) {
            return Err(TxError::InvalidBranchName { name: name.into() });
        }
        if self.names.contains_key(name) {
            return Err(TxError::BranchExists { name: name.into() });
        }
        let id = u16::try_from(self.branches.len()).map_err(|_| TxError::CapacityExceeded {
            kind: CapacityKind::Branch,
        })?;
        if !publish {
            return Ok(Self::empty_result(epoch));
        }
        let from = &self.branches[parent.0 as usize];
        let child = Branch {
            info: BranchInfo {
                id: BranchId(id),
                name: name.into(),
                parent: Some(parent),
                fork_epoch: epoch,
                archived: false,
            },
            // O(1): the child shares the parent's state until either writes.
            db: Arc::clone(&from.db),
            ids: Arc::clone(&from.ids),
            imports: Vec::new(),
        };
        self.syncs.push(SyncPoint {
            src: parent,
            dst: BranchId(id),
            epoch,
            db: Arc::clone(&child.db),
            ids: Arc::clone(&child.ids),
        });
        self.branches.push(child);
        self.names.insert(name.into(), BranchId(id));
        Ok(Self::empty_result(epoch))
    }
    fn archive_branch(
        &mut self,
        branch: BranchId,
        name: &str,
        epoch: Epoch,
        publish: bool,
    ) -> Result<CommitResult, TxError> {
        let b = &mut self.branches[branch.0 as usize];
        if branch == MAIN || b.info.name != name {
            return Err(TxError::InvalidBranchName { name: name.into() });
        }
        b.info.archived |= publish;
        Ok(Self::empty_result(epoch))
    }
    fn commit_ops(
        &mut self,
        b: &Batch,
        blobs: &dyn BlobPresence,
        epoch: Epoch,
        publish: bool,
    ) -> Result<CommitResult, TxError> {
        let Self {
            branches,
            keys,
            next,
            revisions,
            rule_origins,
            revision_ids,
            rule_ids,
            ..
        } = self;
        let branch = &mut branches[b.branch.0 as usize];
        let sink = RefCell::new(Staged::default());
        let foreign = |key: &str| keys.contains(key);
        let ctx = CommitContext {
            epoch: Some(epoch),
            kernel: true,
            foreign_key: Some(&foreign),
            staged: Some(&sink),
        };
        // Which global ids a batch's staged objects receive is known only
        // after staging, since a staged object may be an import. A batch
        // that names a global id invisible here may mean such an object; it
        // is staged again with the ids the previous attempt assigned, until
        // the two agree.
        // Imports are possible only once other branches have objects.
        let ambiguous = (next[Kind::Revision as usize] as usize > branch.ids.len(Kind::Revision)
            || next[Kind::Rule as usize] as usize > branch.ids.len(Kind::Rule))
            && names_invisible(b, &branch.ids);
        let mut plan: Option<Assignment> = None;
        let mut backup = None;
        let mut attempts = 0;
        let (result, staged, assigned) = loop {
            let local = map_batch(b, &|k, g| resolve(&branch.ids, next, plan.as_ref(), k, g));
            if ambiguous && publish && backup.is_none() {
                backup = Some(Arc::clone(&branch.db));
            }
            // Copies the database only if a fork (or the backup) shares it.
            // WHAT_IF leaves it unchanged, but needs it unshared too.
            let db = Arc::make_mut(&mut branch.db);
            let result = if publish {
                db.commit_in(&local, blobs, &ctx)
            } else {
                db.what_if_in(&local, blobs, &ctx)
            };
            let staged = sink.take();
            let assigned = assign(&staged, &branch.ids, next, revision_ids, rule_ids);
            attempts += 1;
            let settled = !ambiguous
                || attempts > b.ops.len()
                || map_batch(b, &|k, g| resolve(&branch.ids, next, Some(&assigned), k, g)) == local;
            if settled {
                break (result, staged, assigned);
            }
            if result.is_ok() && publish {
                branch.db = Arc::clone(backup.as_ref().expect("taken when ambiguous"));
            }
            plan = Some(assigned);
        };
        drop(backup);
        let staged_global = |k, x| staged_global(&branch.ids, next, &assigned, k, x);
        let result = result.map_err(|e| map_error(e, &staged_global))?;
        if !publish {
            return Ok(map_result(result, &staged_global));
        }
        let db = Arc::make_mut(&mut branch.db);
        let ids = Arc::make_mut(&mut branch.ids);
        // Logicals and artifacts are never shared: fresh ids in local order.
        for (k, n) in [
            (Kind::Logical, db.logical_count()),
            (Kind::Artifact, db.artifact_count()),
        ] {
            while ids.len(k) < n {
                ids.push(k, next[k as usize]);
                next[k as usize] += 1;
            }
        }
        crate::invariant_assert!(
            ids.len(Kind::Revision) + assigned.revisions.len() == db.revision_count()
                && ids.len(Kind::Rule) + assigned.rules.len() == db.rule_count(),
            "every staged object is assigned"
        );
        for (r, &g) in staged.revisions.iter().zip(&assigned.revisions) {
            let local = RevisionId(ids.len(Kind::Revision) as u32);
            ids.push(Kind::Revision, g);
            if g == next[Kind::Revision as usize] {
                let logical = LogicalId(ids.global(Kind::Logical, r.logical.0));
                revision_ids.insert((logical.0, r.content), g);
                revisions.push(Revision {
                    logical,
                    ..r.clone()
                });
                next[Kind::Revision as usize] += 1;
            } else {
                db.set_revision_origin(local, &revisions[g as usize]);
                branch
                    .imports
                    .push((Import::Revision(RevisionId(g)), epoch));
            }
        }
        for (j, &g) in staged.rules.iter().zip(&assigned.rules) {
            let local = RuleId(ids.len(Kind::Rule) as u32);
            ids.push(Kind::Rule, g);
            if g == next[Kind::Rule as usize] {
                let key = rule_key(j, |r| ids.global(Kind::Revision, r.0));
                rule_ids.insert(key, g);
                rule_origins.push((j.origin_branch, j.origin_epoch));
                next[Kind::Rule as usize] += 1;
            } else {
                let (origin, at) = rule_origins[g as usize];
                db.set_rule_origin(local, origin, at);
                branch.imports.push((Import::Rule(RuleId(g)), epoch));
            }
        }
        for l in &result.allocations.new_logicals {
            if let Some((key, ..)) = db.logical_info(*l) {
                keys.insert(key);
            }
        }
        Ok(map_result(result, &|k, x| ids.global(k, x)))
    }
}

/// The global ids a batch's staged revisions and rules receive, in staging
/// order: an existing object's id for an import, else the next fresh id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Assignment {
    revisions: Vec<u32>,
    rules: Vec<u32>,
}
fn assign(
    staged: &Staged,
    ids: &IdMap,
    next: &[u32; 4],
    revision_ids: &HashMap<(u32, ContentHash), u32>,
    rule_ids: &HashMap<RuleKey, u32>,
) -> Assignment {
    let mut out = Assignment::default();
    let staged_global = |k: Kind, local: u32, fresh: &[u32]| {
        let len = ids.len(k) as u32;
        match local.checked_sub(len) {
            None => ids.global(k, local),
            Some(i) => fresh
                .get(i as usize)
                .copied()
                .unwrap_or(next[k as usize] + i),
        }
    };
    let mut fresh = next[Kind::Revision as usize];
    for r in &staged.revisions {
        let logical = staged_global(Kind::Logical, r.logical.0, &[]);
        let g = match revision_ids.get(&(logical, r.content)) {
            Some(&g) => g,
            None => {
                fresh += 1;
                fresh - 1
            }
        };
        out.revisions.push(g);
    }
    let mut fresh = next[Kind::Rule as usize];
    for j in &staged.rules {
        let key = rule_key(j, |r| staged_global(Kind::Revision, r.0, &out.revisions));
        let g = match rule_ids.get(&key) {
            Some(&g) => g,
            None => {
                fresh += 1;
                fresh - 1
            }
        };
        out.rules.push(g);
    }
    out
}
fn rule_key(j: &Rule, global: impl Fn(RevisionId) -> u32) -> RuleKey {
    let mut premises: Vec<_> = (j.premises.iter())
        .map(|p| (global(p.premise), p.kind as u8))
        .collect();
    premises.sort_unstable();
    let mut refs: Vec<_> = j.references.iter().map(|r| global(*r)).collect();
    refs.sort_unstable();
    (global(j.conclusion), premises, refs)
}

/// The local id of global `g`. Invisible ids resolve to staged objects:
/// with a plan, those it assigns; otherwise, or past its end, dense fresh
/// ids, as the single-branch engine allows (`next + i` is the batch's
/// `i`-th new object of that kind).
fn resolve(ids: &IdMap, next: &[u32; 4], plan: Option<&Assignment>, k: Kind, g: u32) -> u32 {
    let local = ids.local(k, g);
    if local != INVISIBLE {
        return local;
    }
    let len = ids.len(k) as u32;
    let staged: &[u32] = match (k, plan) {
        (Kind::Revision, Some(p)) => &p.revisions,
        (Kind::Rule, Some(p)) => &p.rules,
        _ => &[],
    };
    if let Some(i) = staged.iter().position(|x| *x == g) {
        return len + i as u32;
    }
    let fresh = staged.iter().filter(|x| **x >= next[k as usize]).count() as u32;
    match g.checked_sub(next[k as usize] + fresh) {
        Some(i) => len.saturating_add(staged.len() as u32).saturating_add(i),
        None => INVISIBLE,
    }
}
/// Whether the batch names a revision or rule not visible in the branch.
fn names_invisible(b: &Batch, ids: &IdMap) -> bool {
    let found = Cell::new(false);
    map_batch(b, &|k, g| {
        let local = ids.local(k, g);
        if local == INVISIBLE && matches!(k, Kind::Revision | Kind::Rule) {
            found.set(true);
        }
        local
    });
    found.get()
}

/// A database and id map shared by branches and sync points.
type Shared<M, S> = (Arc<Db<M, S>>, Arc<IdMap>);
/// One branch database state in a snapshot: its inputs under local ids,
/// local → global ids per kind, and its unobserved anchors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelState {
    pub inputs: Inputs,
    pub to_global: [Vec<u32>; 4],
    pub unobserved: Vec<LogicalId>,
}
/// A branch in a snapshot: its table entry, its state's index and its
/// `branch_visibility` rows.
pub type SnapshotBranch = (BranchInfo, u32, Vec<(Import, Epoch)>);
/// The whole kernel as inputs (ADR 0013 §8): distinct states, the branch
/// table (each branch naming its state) and the sync points. Global tables
/// are derived on import: every global object stays visible on the branch
/// that created it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelSnapshot {
    pub epoch: Epoch,
    pub states: Vec<KernelState>,
    pub branches: Vec<SnapshotBranch>,
    /// `(src, dst, epoch, state)`.
    pub syncs: Vec<(BranchId, BranchId, Epoch, u32)>,
}

impl<M: Maintainer, S: StatsSink> Kernel<M, S> {
    /// Exports every branch and sync point; states shared in memory are
    /// written once.
    pub fn export(&self) -> KernelSnapshot {
        let mut states: Vec<KernelState> = Vec::new();
        let mut seen: Vec<(*const Db<M, S>, *const IdMap)> = Vec::new();
        let mut state_of = |db: &Arc<Db<M, S>>, ids: &Arc<IdMap>| -> u32 {
            let key = (Arc::as_ptr(db), Arc::as_ptr(ids));
            if let Some(n) = seen.iter().position(|k| *k == key) {
                return n as u32;
            }
            seen.push(key);
            states.push(KernelState {
                inputs: db.inputs(),
                to_global: ids.to_global.clone(),
                unobserved: db.anchors_unobserved(),
            });
            (states.len() - 1) as u32
        };
        let branches = (self.branches.iter())
            .map(|b| (b.info.clone(), state_of(&b.db, &b.ids), b.imports.clone()))
            .collect();
        let syncs = (self.syncs.iter())
            .map(|s| (s.src, s.dst, s.epoch, state_of(&s.db, &s.ids)))
            .collect();
        KernelSnapshot {
            epoch: self.epoch,
            states,
            branches,
            syncs,
        }
    }
    /// Rebuilds a kernel from [`Self::export`]. Each state is rebuilt from
    /// its inputs (history starts at the snapshot epoch, as for one
    /// database); the global tables are derived from the branches.
    pub fn import(s: KernelSnapshot) -> Result<Self, TxError> {
        let bad = || TxError::CapacityExceeded {
            kind: CapacityKind::Encoding,
        };
        let mut built: Vec<Shared<M, S>> = Vec::new();
        for state in s.states {
            let mut ids = IdMap::default();
            for (k, list) in [Kind::Logical, Kind::Revision, Kind::Rule, Kind::Artifact]
                .into_iter()
                .zip(&state.to_global)
            {
                for g in list {
                    ids.push(k, *g);
                }
            }
            let counts = [
                state.inputs.logicals.len(),
                state.inputs.revisions.len(),
                state.inputs.rules.len(),
                state.inputs.artifacts.len(),
            ];
            if counts != [0, 1, 2, 3].map(|k| state.to_global[k].len()) {
                return Err(bad());
            }
            let mut db = Db::from_inputs(state.inputs, s.epoch)?;
            db.set_anchors_unobserved(&state.unobserved);
            built.push((Arc::new(db), Arc::new(ids)));
        }
        let pick = |n: u32| built.get(n as usize).cloned().ok_or_else(bad);
        let mut branches = Vec::new();
        let mut names = BTreeMap::new();
        for (info, state, imports) in s.branches {
            if info.id.0 as usize != branches.len() {
                return Err(bad());
            }
            let (db, ids) = pick(state)?;
            names.insert(info.name.clone(), info.id);
            branches.push(Branch {
                info,
                db,
                ids,
                imports,
            });
        }
        if branches.is_empty() {
            return Err(bad());
        }
        let mut syncs = Vec::new();
        for (src, dst, epoch, state) in s.syncs {
            let (db, ids) = pick(state)?;
            syncs.push(SyncPoint {
                src,
                dst,
                epoch,
                db,
                ids,
            });
        }
        // Global tables from the branches.
        let mut next = [0u32; 4];
        let mut keys = HashSet::new();
        let mut revisions: BTreeMap<u32, Revision> = BTreeMap::new();
        let mut rules: BTreeMap<u32, (RuleKey, (BranchId, Epoch))> = BTreeMap::new();
        for b in &branches {
            let inputs = b.db.inputs();
            let g = |k: Kind, l: u32| b.ids.global(k, l);
            for (k, n) in [Kind::Logical, Kind::Revision, Kind::Rule, Kind::Artifact]
                .into_iter()
                .zip([
                    inputs.logicals.len(),
                    inputs.revisions.len(),
                    inputs.rules.len(),
                    inputs.artifacts.len(),
                ])
            {
                for l in 0..n as u32 {
                    next[k as usize] = next[k as usize].max(g(k, l) + 1);
                }
            }
            keys.extend(inputs.logicals.iter().map(|l| l.key.clone()));
            for (n, r) in inputs.revisions.iter().enumerate() {
                let logical = LogicalId(g(Kind::Logical, r.logical.0));
                revisions
                    .entry(g(Kind::Revision, n as u32))
                    .or_insert(Revision {
                        logical,
                        ..r.clone()
                    });
            }
            for (n, j) in inputs.rules.iter().enumerate() {
                rules.entry(g(Kind::Rule, n as u32)).or_insert_with(|| {
                    (
                        rule_key(j, |r| g(Kind::Revision, r.0)),
                        (j.origin_branch, j.origin_epoch),
                    )
                });
            }
        }
        if revisions.len() != next[Kind::Revision as usize] as usize
            || rules.len() != next[Kind::Rule as usize] as usize
        {
            return Err(bad());
        }
        let revision_ids = (revisions.iter())
            .map(|(g, r)| ((r.logical.0, r.content), *g))
            .collect();
        let rule_ids = rules
            .iter()
            .map(|(g, (key, _))| (key.clone(), *g))
            .collect();
        let rule_origins = rules.values().map(|(_, origin)| *origin).collect();
        Ok(Self {
            branches,
            names,
            keys,
            next,
            epoch: s.epoch,
            revisions: revisions.into_values().collect(),
            rule_origins,
            revision_ids,
            rule_ids,
            syncs,
        })
    }
}

/// Read access to one branch with global ids.
pub struct BranchView<'a, M: Maintainer = Counting, S: StatsSink = NoStats> {
    b: &'a Branch<M, S>,
}
impl<'a, M: Maintainer, S: StatsSink> BranchView<'a, M, S> {
    pub fn info(&self) -> &BranchInfo {
        &self.b.info
    }
    /// Objects this branch imported by dedup, with the import epoch.
    pub fn imports(&self) -> &[(Import, Epoch)] {
        &self.b.imports
    }
    /// The branch's single-branch database (local ids).
    pub fn db(&self) -> &'a Db<M, S> {
        &self.b.db
    }
    /// Whether the branch still shares its state with the branch it forked
    /// from (or a fork of it): neither has written since.
    pub fn shares_state_with(&self, other: &BranchView<'_, M, S>) -> bool {
        Arc::ptr_eq(&self.b.db, &other.b.db)
    }
    fn local(&self, k: Kind, global: u32) -> Option<u32> {
        let l = self.b.ids.local(k, global);
        (l != INVISIBLE).then_some(l)
    }
    /// Global ids of the branch database's local ids.
    pub fn global_logical(&self, local: LogicalId) -> LogicalId {
        LogicalId(self.b.ids.global(Kind::Logical, local.0))
    }
    pub fn global_revision(&self, local: RevisionId) -> RevisionId {
        RevisionId(self.b.ids.global(Kind::Revision, local.0))
    }
    pub fn global_rule(&self, local: RuleId) -> RuleId {
        RuleId(self.b.ids.global(Kind::Rule, local.0))
    }
    pub fn global_artifact(&self, local: ArtifactId) -> ArtifactId {
        ArtifactId(self.b.ids.global(Kind::Artifact, local.0))
    }
    pub fn logical_by_key(&self, key: &str) -> Option<LogicalId> {
        let l = self.b.db.logical_by_key(key)?;
        Some(LogicalId(self.b.ids.global(Kind::Logical, l.0)))
    }
    pub fn head(&self, l: LogicalId) -> Option<RevisionId> {
        let local = self.local(Kind::Logical, l.0)?;
        let r = self.b.db.head(LogicalId(local))?;
        Some(RevisionId(self.b.ids.global(Kind::Revision, r.0)))
    }
    pub fn node(&self, r: RevisionId) -> Result<NodeState, TxError> {
        let local = self.local(Kind::Revision, r.0).ok_or(TxError::UnknownRef)?;
        self.b.db.node(RevisionId(local))
    }
    pub fn status(&self, r: RevisionId) -> Result<Summary, TxError> {
        let local = self.local(Kind::Revision, r.0).ok_or(TxError::UnknownRef)?;
        self.b.db.status(RevisionId(local))
    }
    pub fn is_base(&self, r: RevisionId) -> Result<bool, TxError> {
        let local = self.local(Kind::Revision, r.0).ok_or(TxError::UnknownRef)?;
        self.b.db.is_base(RevisionId(local))
    }
    pub fn rule_enabled(&self, j: RuleId) -> Option<bool> {
        let local = self.local(Kind::Rule, j.0)?;
        Some(self.b.db.rules.enabled[local as usize])
    }
    pub fn is_visible_revision(&self, r: RevisionId) -> bool {
        self.local(Kind::Revision, r.0).is_some()
    }
    /// The branch digest: its visible inputs and derived state under its
    /// local (canonical) ids.
    pub fn digest(&self) -> ContentHash {
        self.b.db.digest()
    }
}

fn map_batch(b: &Batch, local: &dyn Fn(Kind, u32) -> u32) -> Batch {
    let l = |l: &LogicalRef| match l {
        LogicalRef::Existing(g) => LogicalRef::Existing(LogicalId(local(Kind::Logical, g.0))),
        other => other.clone(),
    };
    let r = |r: &RevRef| match r {
        RevRef::Existing(g) => RevRef::Existing(RevisionId(local(Kind::Revision, g.0))),
        other => *other,
    };
    let rid = |g: &RevisionId| RevisionId(local(Kind::Revision, g.0));
    let lid = |g: &LogicalId| LogicalId(local(Kind::Logical, g.0));
    let j = |j: &RuleRef| match j {
        RuleRef::Existing(g) => RuleRef::Existing(RuleId(local(Kind::Rule, g.0))),
        other => *other,
    };
    let a = |a: &ArtifactRef| match a {
        ArtifactRef::Existing(g) => ArtifactRef::Existing(ArtifactId(local(Kind::Artifact, g.0))),
        other => *other,
    };
    let ops = b
        .ops
        .iter()
        .map(|op| match op {
            Op::AddRevision {
                logical,
                content,
                adopt_as_base,
            } => Op::AddRevision {
                logical: l(logical),
                content: *content,
                adopt_as_base: *adopt_as_base,
            },
            Op::SetHead {
                logical,
                expected,
                new,
            } => Op::SetHead {
                logical: l(logical),
                expected: r(expected),
                new: r(new),
            },
            Op::AssertBase { rev } => Op::AssertBase { rev: r(rev) },
            Op::RetractBase { rev } => Op::RetractBase { rev: r(rev) },
            Op::AddRule {
                conclusion,
                incidences,
                enabled,
            } => Op::AddRule {
                conclusion: r(conclusion),
                incidences: incidences.iter().map(|(p, k)| (r(p), *k)).collect(),
                enabled: *enabled,
            },
            Op::SetRuleEnabled { rule, enabled } => Op::SetRuleEnabled {
                rule: j(rule),
                enabled: *enabled,
            },
            Op::Rebind { rule, from, to } => Op::Rebind {
                rule: j(rule),
                from: r(from),
                to: r(to),
            },
            Op::RecordValidation {
                rule,
                curvec_seen,
                verdict,
                validator,
            } => Op::RecordValidation {
                rule: j(rule),
                curvec_seen: curvec_seen.iter().map(|(x, y)| (lid(x), rid(y))).collect(),
                verdict: *verdict,
                validator: validator.clone(),
            },
            Op::SetMetadata {
                logical,
                tags,
                dirs,
            } => Op::SetMetadata {
                logical: l(logical),
                tags: tags.clone(),
                dirs: dirs.clone(),
            },
            Op::OpenArtifact {
                ty,
                target,
                payload_hash,
            } => Op::OpenArtifact {
                ty: *ty,
                target: r(target),
                payload_hash: *payload_hash,
            },
            Op::ResolveArtifact { id } => Op::ResolveArtifact { id: a(id) },
            Op::ExpectHeads { entries, source } => Op::ExpectHeads {
                entries: entries.iter().map(|(x, y)| (l(x), rid(y))).collect(),
                source: *source,
            },
            Op::Merge {
                source,
                require_certified,
                certify,
            } => Op::Merge {
                source: *source,
                require_certified: *require_certified,
                certify: certify.iter().map(l).collect(),
            },
            Op::CreateLogical { .. }
            | Op::SetPolicy { .. }
            | Op::ObserverWatermark { .. }
            | Op::CreateBranch { .. }
            | Op::ArchiveBranch { .. } => op.clone(),
        })
        .collect();
    Batch {
        branch: b.branch,
        principal: b.principal,
        channel: b.channel,
        client_txn_id: b.client_txn_id,
        ops,
    }
}

fn map_result(r: CommitResult, map: &dyn Fn(Kind, u32) -> u32) -> CommitResult {
    let g_l = |_: (), x: LogicalId| LogicalId(map(Kind::Logical, x.0));
    let g_r = |_: (), x: RevisionId| RevisionId(map(Kind::Revision, x.0));
    let g_j = |_: (), x: RuleId| RuleId(map(Kind::Rule, x.0));
    CommitResult {
        epoch: r.epoch,
        allocations: Allocations {
            new_logicals: r
                .allocations
                .new_logicals
                .iter()
                .map(|x| g_l((), *x))
                .collect(),
            new_revisions: r
                .allocations
                .new_revisions
                .iter()
                .map(|x| g_r((), *x))
                .collect(),
            new_rules: r
                .allocations
                .new_rules
                .iter()
                .map(|x| g_j((), *x))
                .collect(),
            new_artifacts: r
                .allocations
                .new_artifacts
                .iter()
                .map(|x| ArtifactId(map(Kind::Artifact, x.0)))
                .collect(),
        },
        net_delta: NetDelta {
            base_flips: r.net_delta.base_flips.iter().map(|x| g_r((), *x)).collect(),
            head_moves: r
                .net_delta
                .head_moves
                .iter()
                .map(|(l, old, new)| (g_l((), *l), old.map(|o| g_r((), o)), g_r((), *new)))
                .collect(),
            enable_flips: r
                .net_delta
                .enable_flips
                .iter()
                .map(|x| g_j((), *x))
                .collect(),
            new_rules: r.net_delta.new_rules.iter().map(|x| g_j((), *x)).collect(),
        },
        l0_flips: r.l0_flips.iter().map(|x| g_r((), *x)).collect(),
        status_diff: r.status_diff.iter().map(|x| g_r((), *x)).collect(),
    }
}
/// Errors carry ids of objects visible in the branch, or of objects the
/// failed batch staged: map the former back, and give the latter the global
/// id they would have received.
/// The global id of local `x`: mapped, or staged by the batch (the id it
/// was assigned, or would receive).
fn staged_global(ids: &IdMap, next: &[u32; 4], assigned: &Assignment, k: Kind, x: u32) -> u32 {
    let len = ids.len(k) as u32;
    let staged: &[u32] = match k {
        Kind::Revision => &assigned.revisions,
        Kind::Rule => &assigned.rules,
        _ => &[],
    };
    match x.checked_sub(len) {
        None => ids.global(k, x),
        Some(i) => staged.get(i as usize).copied().unwrap_or_else(|| {
            let fresh = staged.iter().filter(|g| **g >= next[k as usize]).count();
            next[k as usize] + fresh as u32 + (i - staged.len() as u32)
        }),
    }
}
fn map_error(e: TxError, map: &dyn Fn(Kind, u32) -> u32) -> TxError {
    let l = |x: LogicalId| LogicalId(map(Kind::Logical, x.0));
    let r = |x: RevisionId| RevisionId(map(Kind::Revision, x.0));
    let j = |x: RuleId| RuleId(map(Kind::Rule, x.0));
    match e {
        TxError::DuplicateRule { existing, enabled } => TxError::DuplicateRule {
            existing: j(existing),
            enabled,
        },
        TxError::NoSemIncidence { rule } => TxError::NoSemIncidence { rule: j(rule) },
        TxError::HeadConflict {
            logical,
            expected,
            actual,
        } => TxError::HeadConflict {
            logical: l(logical),
            expected: r(expected),
            actual: r(actual),
        },
        TxError::CurvecMoved {
            rule,
            seen,
            current,
        } => TxError::CurvecMoved {
            rule: j(rule),
            seen: seen.into_iter().map(|(x, y)| (l(x), r(y))).collect(),
            current: current.into_iter().map(|(x, y)| (l(x), r(y))).collect(),
        },
        TxError::ArtifactNotOpen { id } => TxError::ArtifactNotOpen {
            id: ArtifactId(map(Kind::Artifact, id.0)),
        },
        TxError::CycleDetected { op_index, path } => TxError::CycleDetected {
            op_index,
            path: path
                .into_iter()
                .map(|n| match n {
                    CycleNode::Revision(x) => CycleNode::Revision(r(x)),
                    CycleNode::Logical(x) => CycleNode::Logical(l(x)),
                })
                .collect(),
        },
        TxError::PolicyViolation {
            kind,
            logical,
            reason,
        } => TxError::PolicyViolation {
            kind,
            logical: l(logical),
            reason,
        },
        TxError::ReadSetConflict { logical, read, now } => TxError::ReadSetConflict {
            logical: l(logical),
            read: r(read),
            now: r(now),
        },
        TxError::InvalidReadSet { logical } => TxError::InvalidReadSet {
            logical: l(logical),
        },
        TxError::MergeNotCertified { logical, status } => TxError::MergeNotCertified {
            logical: l(logical),
            status,
        },
        other => other,
    }
}
