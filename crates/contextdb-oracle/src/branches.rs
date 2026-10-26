//! Independent branch model for PROP13 (ADR 0013 §11 G3). Naive by design:
//! global arenas, visibility evaluated literally by SD19 rules 1–3 from
//! origins and `branch_visibility` rows, a full overlay copy per branch and
//! per sync point, naive merge-base search and classification, and full
//! recomputation through the single-branch [`InputModel`] and [`jacobi`] on
//! a projection of the branch. It never calls the core.
use crate::{InputModel, jacobi, summarize};
use contextdb_model::*;
use std::collections::{BTreeMap, BTreeSet};

/// A global object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Obj {
    Logical(u32),
    Revision(u32),
    Rule(u32),
    Artifact(u32),
}

/// One branch's input values, keyed by global id: every visible object has
/// its entry (absent means not visible).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Overlay {
    heads: BTreeMap<u32, u32>,
    base: BTreeMap<u32, bool>,
    enabled: BTreeMap<u32, bool>,
    open: BTreeMap<u32, bool>,
    metadata: BTreeMap<u32, Metadata>,
    validations: BTreeMap<(u32, Vec<(u32, u32)>), ValidationRecord>,
    policy: PolicyDoc,
    watermarks: BTreeMap<String, Vec<u8>>,
    unobserved: BTreeSet<u32>,
}
#[derive(Clone, Debug)]
struct Branch {
    name: String,
    parent: Option<BranchId>,
    fork_epoch: Epoch,
    archived: bool,
    state: Overlay,
    /// The branch's canonical order per kind (ADR 0013 r0.5): the order in
    /// which objects became visible here. A fork inherits its parent's; a
    /// commit appends in staging order; a merge appends newly visible
    /// objects in ascending global id, then its Conflicts. SD15's "smallest
    /// id" choices (witness, failing incidence) use this order.
    order: [Vec<u32>; 4],
}
/// A stored state: `src`'s overlay at `epoch`, synced into `dst`.
#[derive(Clone, Debug)]
struct Sync {
    src: BranchId,
    dst: BranchId,
    epoch: Epoch,
    state: Overlay,
}

/// A conflict a merge opened, in global ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub kind: u8,
    pub logical: Option<u32>,
    pub rule: Option<u32>,
    pub target: u32,
    pub artifact: u32,
}
/// The merge report, in global ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeOutcome {
    pub moved_heads: Vec<(u32, u32, u32)>,
    pub conflicts: Vec<Conflict>,
    /// `(logical, 0 = tags | 1 = dirs)`.
    pub metadata_conflicts: Vec<(u32, u8)>,
    pub newly_visible: [usize; 4],
    pub convergent: usize,
}

/// A branch's whole state under global ids: what PROP13 compares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Canonical {
    pub logicals: BTreeMap<u32, (Logical, u32, Metadata)>,
    pub revisions: BTreeMap<u32, (Revision, bool)>,
    pub rules: BTreeMap<u32, Rule>,
    pub artifacts: BTreeMap<u32, Artifact>,
    pub validations: BTreeMap<(u32, Vec<(u32, u32)>), ValidationRecord>,
    pub policy: PolicyDoc,
    pub watermarks: BTreeMap<String, Vec<u8>>,
    pub unobserved: BTreeSet<u32>,
    pub nodes: BTreeMap<u32, NodeState>,
    /// Per rule: (active, layer_active, sem).
    pub rule_states: BTreeMap<u32, (bool, [bool; 2], SemState)>,
}

/// Builds the canonical form of one branch from its local inputs, derived
/// state and local → global maps (logical, revision, rule, artifact).
pub fn canonical(
    i: &Inputs,
    d: &DerivedSnapshot,
    g: [&dyn Fn(u32) -> u32; 4],
    unobserved: BTreeSet<u32>,
) -> Canonical {
    let [gl, gr, gj, ga] = g;
    let rev = |r: RevisionId| RevisionId(gr(r.0));
    let mut out = Canonical {
        logicals: BTreeMap::new(),
        revisions: BTreeMap::new(),
        rules: BTreeMap::new(),
        artifacts: BTreeMap::new(),
        validations: BTreeMap::new(),
        policy: i.policy.clone(),
        watermarks: i.watermarks.clone(),
        unobserved,
        nodes: BTreeMap::new(),
        rule_states: BTreeMap::new(),
    };
    for (n, l) in i.logicals.iter().enumerate() {
        out.logicals.insert(
            gl(n as u32),
            (l.clone(), gr(i.heads[n].0), i.metadata[n].clone()),
        );
    }
    for (n, r) in i.revisions.iter().enumerate() {
        let mut r = r.clone();
        r.logical = LogicalId(gl(r.logical.0));
        out.revisions.insert(gr(n as u32), (r, i.base[n]));
    }
    for (n, j) in i.rules.iter().enumerate() {
        let mut j = j.clone();
        j.conclusion = rev(j.conclusion);
        for p in &mut j.premises {
            p.premise = rev(p.premise);
        }
        j.premises.sort_unstable();
        for r in &mut j.references {
            *r = rev(*r);
        }
        j.references.sort_unstable();
        out.rules.insert(gj(n as u32), j);
    }
    for (n, a) in i.artifacts.iter().enumerate() {
        let mut a = a.clone();
        a.target = rev(a.target);
        out.artifacts.insert(ga(n as u32), a);
    }
    for ((j, curvec), record) in &i.validations {
        let mut curvec: Vec<_> = curvec.iter().map(|(l, r)| (gl(l.0), gr(r.0))).collect();
        curvec.sort_unstable();
        out.validations.insert((gj(j.0), curvec), record.clone());
    }
    for (n, node) in d.nodes.iter().enumerate() {
        let mut node = node.clone();
        for w in node.witness.iter_mut().flatten() {
            if let Witness::Rule(j) = w {
                *j = RuleId(gj(j.0));
            }
        }
        if let StaleReason::SupersededPinnedPremise(r) = &mut node.dimensions.stale_reason {
            *r = rev(*r);
        }
        out.nodes.insert(gr(n as u32), node);
    }
    for n in 0..i.rules.len() {
        out.rule_states
            .insert(gj(n as u32), (d.active[n], d.layer_active[n], d.sem[n]));
    }
    out
}

/// The canonical conflict payload (ADR 0013 §6.2): merge epoch, source,
/// kind, subject id, then each base value, the source's and the target's.
pub fn conflict_payload(
    epoch: Epoch,
    source: BranchId,
    kind: u8,
    subject: u32,
    values: &[Vec<u8>],
) -> Vec<u8> {
    let mut out = b"CTXMERGE1".to_vec();
    out.extend(epoch.0.to_le_bytes());
    out.extend(source.0.to_le_bytes());
    out.push(kind);
    out.extend(subject.to_le_bytes());
    out.extend((values.len() as u32).to_le_bytes());
    for v in values {
        out.extend((v.len() as u32).to_le_bytes());
        out.extend(v);
    }
    out
}
/// A ValidationRecord conflict's curvec, the first payload value: two
/// conflicts on one rule at different curvecs must differ.
fn enc_curvec(curvec: &[(u32, u32)]) -> Vec<u8> {
    let mut out = (curvec.len() as u32).to_le_bytes().to_vec();
    for (l, r) in curvec {
        out.extend(l.to_le_bytes());
        out.extend(r.to_le_bytes());
    }
    out
}
fn enc_rev(v: Option<u32>) -> Vec<u8> {
    match v {
        None => vec![0],
        Some(r) => [&[1u8][..], &r.to_le_bytes()].concat(),
    }
}
fn enc_bool(v: Option<bool>) -> Vec<u8> {
    vec![match v {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    }]
}
fn enc_validation(v: Option<&ValidationRecord>) -> Vec<u8> {
    match v {
        None => vec![0],
        Some(r) => [&[1u8, r.verdict as u8][..], r.validator.as_bytes()].concat(),
    }
}

/// Branch names (D1): the git ref character set, `main` reserved.
fn valid_name(name: &str) -> bool {
    let ok_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'));
    ok_chars
        && (1..=128).contains(&name.len())
        && name != "main"
        && !name.starts_with('/')
        && !name.starts_with('-')
        && !name.ends_with('/')
        && !name.contains("..")
        && !name.contains("//")
}

/// A branch projection: dense local ids in ascending global order.
struct Projection {
    inputs: Inputs,
    /// Local → global per kind.
    to_global: [Vec<u32>; 4],
    to_local: [BTreeMap<u32, u32>; 4],
}

/// A conflict before its artifact: (kind, logical, rule, target, payload).
type Found = (u8, Option<u32>, Option<u32>, u32, Vec<u8>);

/// The independent branch oracle.
#[derive(Clone, Debug)]
pub struct BranchOracle {
    /// Hashes a Conflict payload into its `payload_hash`. The caller
    /// supplies it, so the oracle keeps depending on the model only.
    hash: fn(&[u8]) -> ContentHash,
    logicals: Vec<Logical>,
    revisions: Vec<Revision>,
    rules: Vec<Rule>,
    artifacts: Vec<Artifact>,
    rows: Vec<(BranchId, Obj, Epoch)>,
    branches: Vec<Branch>,
    syncs: Vec<Sync>,
    epoch: Epoch,
}
impl BranchOracle {
    /// A store with only `main`; `hash` is the store's content hash.
    pub fn new(hash: fn(&[u8]) -> ContentHash) -> Self {
        Self {
            hash,
            logicals: Vec::new(),
            revisions: Vec::new(),
            rules: Vec::new(),
            artifacts: Vec::new(),
            rows: Vec::new(),
            branches: vec![Branch {
                name: "main".into(),
                parent: None,
                fork_epoch: Epoch(0),
                archived: false,
                state: Overlay::default(),
                order: Default::default(),
            }],
            syncs: Vec::new(),
            epoch: Epoch(0),
        }
    }
}

impl BranchOracle {
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
    pub fn branch_count(&self) -> usize {
        self.branches.len()
    }
    pub fn branch_name(&self, b: BranchId) -> String {
        self.branches[b.0 as usize].name.clone()
    }
    pub fn branch_by_name(&self, name: &str) -> Option<BranchId> {
        (self.branches.iter())
            .position(|b| b.name == name)
            .map(|n| BranchId(n as u16))
    }
    fn origin(&self, o: Obj) -> (BranchId, Epoch) {
        match o {
            Obj::Logical(n) => {
                let l = &self.logicals[n as usize];
                (l.origin_branch, l.origin_epoch)
            }
            Obj::Revision(n) => {
                let r = &self.revisions[n as usize];
                (r.origin_branch, r.origin_epoch)
            }
            Obj::Rule(n) => {
                let j = &self.rules[n as usize];
                (j.origin_branch, j.origin_epoch)
            }
            Obj::Artifact(n) => {
                let a = &self.artifacts[n as usize];
                (a.origin_branch, a.origin_epoch)
            }
        }
    }
    /// SD19 visibility, literally: own origin, a row, or the parent's view
    /// at the fork (recursively).
    fn vis(&self, b: BranchId, o: Obj, e: Epoch) -> bool {
        let (origin, at) = self.origin(o);
        if origin == b && at <= e {
            return true;
        }
        if (self.rows.iter()).any(|(rb, ro, m)| *rb == b && *ro == o && *m <= e) {
            return true;
        }
        let branch = &self.branches[b.0 as usize];
        match branch.parent {
            Some(p) => self.vis(p, o, e.min(branch.fork_epoch)),
            None => false,
        }
    }
    fn all(&self) -> impl Iterator<Item = Obj> + '_ {
        (0..self.logicals.len() as u32)
            .map(Obj::Logical)
            .chain((0..self.revisions.len() as u32).map(Obj::Revision))
            .chain((0..self.rules.len() as u32).map(Obj::Rule))
            .chain((0..self.artifacts.len() as u32).map(Obj::Artifact))
    }
    /// Objects visible in `b` now, per kind, ascending.
    pub fn visible(&self, b: BranchId) -> [Vec<u32>; 4] {
        let mut out: [Vec<u32>; 4] = Default::default();
        for o in self.all() {
            if self.vis(b, o, Epoch(u64::MAX)) {
                match o {
                    Obj::Logical(n) => out[0].push(n),
                    Obj::Revision(n) => out[1].push(n),
                    Obj::Rule(n) => out[2].push(n),
                    Obj::Artifact(n) => out[3].push(n),
                }
            }
        }
        out
    }
    fn project(&self, b: BranchId) -> Projection {
        let to_global = self.branches[b.0 as usize].order.clone();
        // The order lists exactly the objects SD19 rules 1–3 make visible.
        let visible = self.visible(b);
        for k in 0..4 {
            let mut listed = to_global[k].clone();
            listed.sort_unstable();
            assert_eq!(
                listed, visible[k],
                "branch {b:?} kind {k}: order ≠ visibility"
            );
        }
        let to_local: [BTreeMap<u32, u32>; 4] = [0, 1, 2, 3].map(|k| {
            (to_global[k].iter().enumerate())
                .map(|(l, g)| (*g, l as u32))
                .collect()
        });
        let s = &self.branches[b.0 as usize].state;
        let lr = |g: u32| RevisionId(to_local[1][&g]);
        let ll = |g: u32| LogicalId(to_local[0][&g]);
        let mut i = Inputs {
            policy: s.policy.clone(),
            watermarks: s.watermarks.clone(),
            ..Inputs::default()
        };
        for g in &to_global[0] {
            i.logicals.push(self.logicals[*g as usize].clone());
            i.heads.push(lr(s.heads[g]));
            i.metadata.push(s.metadata[g].clone());
        }
        for g in &to_global[1] {
            let mut r = self.revisions[*g as usize].clone();
            r.logical = ll(r.logical.0);
            i.revisions.push(r);
            i.base.push(s.base[g]);
        }
        for g in &to_global[2] {
            let mut j = self.rules[*g as usize].clone();
            j.conclusion = lr(j.conclusion.0);
            for p in &mut j.premises {
                p.premise = lr(p.premise.0);
            }
            // Canonical order: premise ascending, then kind descending.
            j.premises
                .sort_unstable_by(|a, b| a.premise.cmp(&b.premise).then(b.kind.cmp(&a.kind)));
            for r in &mut j.references {
                *r = lr(r.0);
            }
            j.references.sort_unstable();
            j.enabled = s.enabled[g];
            i.rules.push(j);
        }
        for g in &to_global[3] {
            let mut a = self.artifacts[*g as usize].clone();
            a.target = lr(a.target.0);
            a.open = s.open[g];
            i.artifacts.push(a);
        }
        for ((j, curvec), record) in &s.validations {
            let mut curvec: Vec<_> = curvec.iter().map(|(l, r)| (ll(*l), lr(*r))).collect();
            curvec.sort_unstable();
            i.validations
                .insert((RuleId(to_local[2][j]), curvec), record.clone());
        }
        Projection {
            inputs: i,
            to_global,
            to_local,
        }
    }
    /// The canonical state of branch `b`.
    pub fn canonical(&self, b: BranchId) -> Canonical {
        let p = self.project(b);
        let d = jacobi(&p.inputs);
        let tg = &p.to_global;
        let g = |k: usize| move |x: u32| tg[k][x as usize];
        let (g0, g1, g2, g3) = (g(0), g(1), g(2), g(3));
        canonical(
            &p.inputs,
            &d,
            [&g0, &g1, &g2, &g3],
            self.branches[b.0 as usize].state.unobserved.clone(),
        )
    }
    /// Rewrites branch `b`'s overlay from projected inputs.
    fn store_overlay(&mut self, b: BranchId, i: &Inputs, to_global: &[Vec<u32>; 4]) {
        let g = |k: usize, x: u32| to_global[k][x as usize];
        let s = &mut self.branches[b.0 as usize].state;
        s.heads = (0..i.logicals.len())
            .map(|n| (g(0, n as u32), g(1, i.heads[n].0)))
            .collect();
        s.metadata = (0..i.logicals.len())
            .map(|n| (g(0, n as u32), i.metadata[n].clone()))
            .collect();
        s.base = (0..i.revisions.len())
            .map(|n| (g(1, n as u32), i.base[n]))
            .collect();
        s.enabled = (i.rules.iter().enumerate())
            .map(|(n, j)| (g(2, n as u32), j.enabled))
            .collect();
        s.open = (i.artifacts.iter().enumerate())
            .map(|(n, a)| (g(3, n as u32), a.open))
            .collect();
        s.validations = (i.validations.iter())
            .map(|((j, curvec), record)| {
                let mut curvec: Vec<_> =
                    curvec.iter().map(|(l, r)| (g(0, l.0), g(1, r.0))).collect();
                curvec.sort_unstable();
                ((g(2, j.0), curvec), record.clone())
            })
            .collect();
        s.policy = i.policy.clone();
        s.watermarks = i.watermarks.clone();
    }

    /// Applies a batch; `publish = false` is `WHAT_IF`.
    pub fn apply(
        &mut self,
        b: &Batch,
        publish: bool,
    ) -> Result<(ApplyResult, Option<MergeOutcome>), TxError> {
        if publish {
            return self.apply_now(b);
        }
        self.clone().apply_now(b)
    }
    fn apply_now(&mut self, b: &Batch) -> Result<(ApplyResult, Option<MergeOutcome>), TxError> {
        let branch = self
            .branches
            .get(b.branch.0 as usize)
            .ok_or(TxError::UnknownBranch)?;
        if branch.archived {
            return Err(TxError::BranchArchived {
                name: branch.name.clone(),
            });
        }
        for op in &b.ops {
            let sole = match op {
                Op::CreateBranch { .. } => Some("CreateBranch"),
                Op::ArchiveBranch { .. } => Some("ArchiveBranch"),
                _ => None,
            };
            if let Some(name) = sole
                && b.ops.len() > 1
            {
                return Err(TxError::SoleOpRequired { op: name.into() });
            }
        }
        let merges = (b.ops.iter())
            .filter(|op| matches!(op, Op::Merge { .. }))
            .count();
        let only_reads =
            (b.ops.iter()).all(|op| matches!(op, Op::Merge { .. } | Op::ExpectHeads { .. }));
        if merges > 1 || (merges == 1 && !only_reads) {
            return Err(TxError::SoleOpRequired { op: "Merge".into() });
        }
        let epoch = Epoch(self.epoch.0 + 1);
        let done = |s: &mut Self| {
            s.epoch = epoch;
            ApplyResult {
                epoch,
                allocations: Allocations::default(),
            }
        };
        match &b.ops[..] {
            [Op::CreateBranch { name }] => {
                if !valid_name(name) {
                    return Err(TxError::InvalidBranchName { name: name.clone() });
                }
                if self.branch_by_name(name).is_some() {
                    return Err(TxError::BranchExists { name: name.clone() });
                }
                if self.branches.len() > u16::MAX as usize {
                    return Err(TxError::CapacityExceeded {
                        kind: CapacityKind::Branch,
                    });
                }
                let state = self.branches[b.branch.0 as usize].state.clone();
                let order = self.branches[b.branch.0 as usize].order.clone();
                let child = BranchId(self.branches.len() as u16);
                self.syncs.push(Sync {
                    src: b.branch,
                    dst: child,
                    epoch,
                    state: state.clone(),
                });
                self.branches.push(Branch {
                    name: name.clone(),
                    parent: Some(b.branch),
                    fork_epoch: epoch,
                    archived: false,
                    state,
                    order,
                });
                Ok((done(self), None))
            }
            [Op::ArchiveBranch { name }] => {
                if b.branch == MAIN || self.branches[b.branch.0 as usize].name != *name {
                    return Err(TxError::InvalidBranchName { name: name.clone() });
                }
                self.branches[b.branch.0 as usize].archived = true;
                Ok((done(self), None))
            }
            _ if merges == 1 => self.merge(b, epoch).map(|(r, m)| (r, Some(m))),
            _ => self.commit(b, epoch).map(|r| (r, None)),
        }
    }

    /// An ordinary batch on one branch, through the single-branch oracle on
    /// its projection; new objects identical to invisible ones are imports.
    fn commit(&mut self, b: &Batch, epoch: Epoch) -> Result<ApplyResult, TxError> {
        let p = self.project(b.branch);
        let unknown = u32::MAX;
        let map = |k: usize, g: u32| p.to_local[k].get(&g).copied().unwrap_or(unknown);
        let local = remap(b, &map);
        let foreign = |key: &str| {
            (self.logicals.iter().enumerate())
                .any(|(n, l)| l.key == key && !p.to_local[0].contains_key(&(n as u32)))
        };
        let mut model = InputModel::from_inputs(p.inputs.clone(), self.epoch);
        let mut local_batch = local;
        local_batch.branch = MAIN;
        let result = model
            .apply_on_branch(&local_batch, &foreign)
            .map_err(|e| map_ids(e, &p))?;
        // Records name the batch's branch, not the projection's.
        let staged = model.inputs().clone();
        let lens = [0, 1, 2, 3].map(|k| p.to_global[k].len());
        let mut to_global = p.to_global.clone();
        for r in staged.logicals.iter().skip(lens[0]) {
            let mut r = r.clone();
            r.origin_branch = b.branch;
            to_global[0].push(self.logicals.len() as u32);
            self.logicals.push(r);
        }
        for r in staged.revisions.iter().skip(lens[1]) {
            let logical = to_global[0][r.logical.index()];
            let existing = (self.revisions.iter())
                .position(|x| x.logical.0 == logical && x.content == r.content);
            let g = match existing {
                Some(g) => {
                    self.rows.push((b.branch, Obj::Revision(g as u32), epoch));
                    g as u32
                }
                None => {
                    let mut r = r.clone();
                    r.logical = LogicalId(logical);
                    r.origin_branch = b.branch;
                    self.revisions.push(r);
                    (self.revisions.len() - 1) as u32
                }
            };
            to_global[1].push(g);
        }
        for j in staged.rules.iter().skip(lens[2]) {
            let gr = |r: RevisionId| RevisionId(to_global[1][r.index()]);
            let mut key = j.clone();
            key.conclusion = gr(key.conclusion);
            for p in &mut key.premises {
                p.premise = gr(p.premise);
            }
            key.premises.sort_unstable();
            for r in &mut key.references {
                *r = gr(*r);
            }
            key.references.sort_unstable();
            let existing = self.rules.iter().position(|x| {
                let mut premises = x.premises.clone();
                premises.sort_unstable();
                let mut refs = x.references.clone();
                refs.sort_unstable();
                x.conclusion == key.conclusion && premises == key.premises && refs == key.references
            });
            let g = match existing {
                Some(g) => {
                    self.rows.push((b.branch, Obj::Rule(g as u32), epoch));
                    g as u32
                }
                None => {
                    key.origin_branch = b.branch;
                    self.rules.push(key);
                    (self.rules.len() - 1) as u32
                }
            };
            to_global[2].push(g);
        }
        for a in staged.artifacts.iter().skip(lens[3]) {
            let mut a = a.clone();
            a.target = RevisionId(to_global[1][a.target.index()]);
            a.origin_branch = b.branch;
            to_global[3].push(self.artifacts.len() as u32);
            self.artifacts.push(a);
        }
        self.store_overlay(b.branch, &staged, &to_global);
        self.branches[b.branch.0 as usize].order = to_global.clone();
        // A watermark commit observes its scope's merged anchors.
        for op in &b.ops {
            if let Op::ObserverWatermark { scope, .. } = op {
                let logicals = &self.logicals;
                self.branches[b.branch.0 as usize]
                    .state
                    .unobserved
                    .retain(|l| logicals[*l as usize].scope_lane != *scope);
            }
        }
        self.epoch = epoch;
        let g = |k: usize, x: u32| to_global[k][x as usize];
        let a = result.allocations;
        Ok(ApplyResult {
            epoch,
            allocations: Allocations {
                new_logicals: a
                    .new_logicals
                    .iter()
                    .map(|x| LogicalId(g(0, x.0)))
                    .collect(),
                new_revisions: a
                    .new_revisions
                    .iter()
                    .map(|x| RevisionId(g(1, x.0)))
                    .collect(),
                new_rules: a.new_rules.iter().map(|x| RuleId(g(2, x.0))).collect(),
                new_artifacts: a
                    .new_artifacts
                    .iter()
                    .map(|x| ArtifactId(g(3, x.0)))
                    .collect(),
            },
        })
    }

    /// Whether state `x` is contained in branch state `y` (ADR 0013 §6.1).
    fn contained(&self, x: (BranchId, Epoch), y: (BranchId, Epoch)) -> bool {
        (x.0 == y.0 && x.1 <= y.1)
            || (self.syncs.iter()).any(|s| {
                s.dst == y.0
                    && s.epoch <= y.1
                    && (s.src, s.epoch) != y
                    && self.contained(x, (s.src, s.epoch))
            })
    }

    /// `Merge` (ADR 0013 §6), naively.
    fn merge(&mut self, b: &Batch, epoch: Epoch) -> Result<(ApplyResult, MergeOutcome), TxError> {
        let t = b.branch;
        let (op_index, source, require, certify) = (b.ops.iter().enumerate())
            .find_map(|(n, op)| match op {
                Op::Merge {
                    source,
                    require_certified,
                    certify,
                } => Some((n, *source, *require_certified, certify.clone())),
                _ => None,
            })
            .unwrap();
        if source.0 as usize >= self.branches.len() || source == t {
            return Err(TxError::InvalidMerge);
        }
        if b.channel == Channel::Observer {
            return Err(TxError::ChannelRestricted {
                op: "Merge".into(),
                channel: b.channel,
            });
        }
        // Read set on the target before the merge.
        let tv = self.visible(t);
        let mut reads: BTreeMap<u32, u32> = BTreeMap::new();
        let mut read_error = None;
        'reads: for op in &b.ops {
            let Op::ExpectHeads { entries, .. } = op else {
                continue;
            };
            for (l, r) in entries {
                let gl = match l {
                    LogicalRef::Existing(g) => tv[0].contains(&g.0).then_some(g.0),
                    LogicalRef::Key(k) => {
                        (tv[0].iter().copied()).find(|g| self.logicals[*g as usize].key == *k)
                    }
                    LogicalRef::New(_) => None,
                };
                let (Some(gl), true) = (gl, tv[1].contains(&r.0)) else {
                    read_error = Some(TxError::UnknownRef);
                    break 'reads;
                };
                if reads.insert(gl, r.0).is_some_and(|old| old != r.0) {
                    read_error = Some(TxError::InvalidReadSet {
                        logical: LogicalId(gl),
                    });
                    break 'reads;
                }
            }
        }
        let early = (b.ops.iter().enumerate())
            .all(|(n, op)| n < op_index || !matches!(op, Op::ExpectHeads { .. }));
        if early && let Some(e) = read_error.clone() {
            return Err(e);
        }
        // Bases: maximal stored states contained in both branches.
        let now = Epoch(u64::MAX);
        let mut candidates: Vec<usize> = Vec::new();
        for (n, s) in self.syncs.iter().enumerate() {
            let state = (s.src, s.epoch);
            let seen =
                (candidates.iter()).any(|c| (self.syncs[*c].src, self.syncs[*c].epoch) == state);
            if !seen && self.contained(state, (source, now)) && self.contained(state, (t, now)) {
                candidates.push(n);
            }
        }
        let st = |n: usize| (self.syncs[n].src, self.syncs[n].epoch);
        let bases: Vec<Overlay> = (candidates.iter())
            .filter(|c| !(candidates.iter()).any(|d| d != *c && self.contained(st(**c), st(*d))))
            .map(|c| self.syncs[*c].state.clone())
            .collect();
        let s = self.branches[source.0 as usize].state.clone();
        let tstate = self.branches[t.0 as usize].state.clone();
        let sv = self.visible(source);
        let mut out = MergeOutcome::default();
        // Three-way: None = keep, Some(Ok(v)) = take, Some(Err) = conflict.
        fn three<T: PartialEq + Clone>(
            bases: &[Option<T>],
            s: Option<T>,
            t: Option<T>,
            convergent: &mut usize,
        ) -> Option<Result<T, ()>> {
            let b = if bases.len() == 1 {
                bases[0].clone()
            } else if s == t {
                return None;
            } else if bases.iter().all(|x| *x == bases[0]) {
                bases[0].clone()
            } else {
                return Some(Err(()));
            };
            if s == b {
                None
            } else if t == b {
                Some(Ok(s.unwrap()))
            } else if s == t {
                *convergent += 1;
                None
            } else {
                Some(Err(()))
            }
        }
        let policy_bases: Vec<_> = bases.iter().map(|x| Some(x.policy.clone())).collect();
        let policy = match three(
            &policy_bases,
            Some(s.policy.clone()),
            Some(tstate.policy.clone()),
            &mut out.convergent,
        ) {
            Some(Err(())) => {
                return Err(TxError::MergeConflict {
                    kind: MergeConflictKind::Policy,
                });
            }
            Some(Ok(p)) => Some(p),
            None => None,
        };
        let anchor = |o: &Self, l: u32| o.logicals[l as usize].kind == RecordKind::CodeAnchor;
        let rev_logical = |o: &Self, r: u32| o.revisions[r as usize].logical.0;
        let mut found: Vec<Found> = Vec::new();
        let mut next = tstate.clone();
        let in_t = |kind: usize, g: u32| tv[kind].contains(&g);
        for &l in &sv[0] {
            if !in_t(0, l) {
                continue;
            }
            if !anchor(self, l) {
                let bv: Vec<_> = bases.iter().map(|x| x.heads.get(&l).copied()).collect();
                let (shead, thead) = (s.heads.get(&l).copied(), tstate.heads.get(&l).copied());
                match three(&bv, shead, thead, &mut out.convergent) {
                    None => {}
                    Some(Ok(r)) => {
                        next.heads.insert(l, r);
                        out.moved_heads.push((l, thead.unwrap(), r));
                    }
                    Some(Err(())) => {
                        let mut v: Vec<_> = bv.iter().map(|x| enc_rev(*x)).collect();
                        v.extend([enc_rev(shead), enc_rev(thead)]);
                        found.push((
                            0,
                            Some(l),
                            None,
                            thead.unwrap(),
                            conflict_payload(epoch, source, 0, l, &v),
                        ));
                    }
                }
            }
            let (sm, tm) = (s.metadata[&l].clone(), tstate.metadata[&l].clone());
            let mut merged = tm.clone();
            let bt: Vec<_> = bases
                .iter()
                .map(|x| x.metadata.get(&l).map(|m| m.tags.clone()))
                .collect();
            match three(
                &bt,
                Some(sm.tags.clone()),
                Some(tm.tags.clone()),
                &mut out.convergent,
            ) {
                None => {}
                Some(Ok(v)) => merged.tags = v,
                Some(Err(())) => out.metadata_conflicts.push((l, 0)),
            }
            let bd: Vec<_> = bases
                .iter()
                .map(|x| x.metadata.get(&l).map(|m| m.dirs.clone()))
                .collect();
            match three(
                &bd,
                Some(sm.dirs.clone()),
                Some(tm.dirs.clone()),
                &mut out.convergent,
            ) {
                None => {}
                Some(Ok(v)) => merged.dirs = v,
                Some(Err(())) => {
                    // Prefix-aware intersection; empty is never applied.
                    let within = |a: &str, b: &str| a == b || a.starts_with(&format!("{b}/"));
                    let mut both = BTreeSet::new();
                    for x in &sm.dirs {
                        for y in &tm.dirs {
                            if within(x, y) {
                                both.insert(x.clone());
                            } else if within(y, x) {
                                both.insert(y.clone());
                            }
                        }
                    }
                    if both.is_empty() {
                        out.metadata_conflicts.push((l, 1));
                    } else {
                        merged.dirs = both;
                    }
                }
            }
            next.metadata.insert(l, merged);
        }
        for &r in &sv[1] {
            let l = rev_logical(self, r);
            if in_t(0, l) && anchor(self, l) {
                if !in_t(1, r) {
                    next.base.insert(r, false);
                }
                continue;
            }
            let bv: Vec<_> = bases.iter().map(|x| x.base.get(&r).copied()).collect();
            let (sb, tb) = (s.base.get(&r).copied(), tstate.base.get(&r).copied());
            match three(&bv, sb, tb, &mut out.convergent) {
                None => {}
                Some(Ok(v)) => {
                    next.base.insert(r, v);
                }
                Some(Err(())) => {
                    let mut v: Vec<_> = bv.iter().map(|x| enc_bool(*x)).collect();
                    v.extend([enc_bool(sb), enc_bool(tb)]);
                    found.push((
                        1,
                        Some(l),
                        None,
                        r,
                        conflict_payload(epoch, source, 1, r, &v),
                    ));
                }
            }
        }
        for &j in &sv[2] {
            let bv: Vec<_> = bases.iter().map(|x| x.enabled.get(&j).copied()).collect();
            let (se, te) = (s.enabled.get(&j).copied(), tstate.enabled.get(&j).copied());
            match three(&bv, se, te, &mut out.convergent) {
                None => {}
                Some(Ok(v)) => {
                    next.enabled.insert(j, v);
                }
                Some(Err(())) => {
                    let c = self.rules[j as usize].conclusion.0;
                    let mut v: Vec<_> = bv.iter().map(|x| enc_bool(*x)).collect();
                    v.extend([enc_bool(se), enc_bool(te)]);
                    found.push((
                        2,
                        Some(rev_logical(self, c)),
                        Some(j),
                        c,
                        conflict_payload(epoch, source, 2, j, &v),
                    ));
                }
            }
        }
        for &a in &sv[3] {
            let art = &self.artifacts[a as usize];
            if art.ty == ArtifactType::AnchorBroken {
                continue;
            }
            let bv: Vec<_> = bases.iter().map(|x| x.open.get(&a).copied()).collect();
            let (so, to) = (s.open.get(&a).copied(), tstate.open.get(&a).copied());
            match three(&bv, so, to, &mut out.convergent) {
                None => {}
                Some(Ok(v)) => {
                    next.open.insert(a, v);
                }
                Some(Err(())) => {
                    let target = art.target.0;
                    let mut v: Vec<_> = bv.iter().map(|x| enc_bool(*x)).collect();
                    v.extend([enc_bool(so), enc_bool(to)]);
                    found.push((
                        3,
                        Some(rev_logical(self, target)),
                        None,
                        target,
                        conflict_payload(epoch, source, 3, a, &v),
                    ));
                }
            }
        }
        for (key, record) in &s.validations {
            let bv: Vec<_> = bases
                .iter()
                .map(|x| x.validations.get(key).cloned())
                .collect();
            let tr = tstate.validations.get(key).cloned();
            match three(&bv, Some(record.clone()), tr.clone(), &mut out.convergent) {
                None => {}
                Some(Ok(v)) => {
                    next.validations.insert(key.clone(), v);
                }
                Some(Err(())) if tr.as_ref().is_some_and(|x| x.verdict == record.verdict) => {
                    out.convergent += 1;
                }
                Some(Err(())) => {
                    let c = self.rules[key.0 as usize].conclusion.0;
                    let mut v = vec![enc_curvec(&key.1)];
                    v.extend(bv.iter().map(|x| enc_validation(x.as_ref())));
                    v.extend([enc_validation(Some(record)), enc_validation(tr.as_ref())]);
                    found.push((
                        4,
                        Some(rev_logical(self, c)),
                        Some(key.0),
                        c,
                        conflict_payload(epoch, source, 4, key.0, &v),
                    ));
                }
            }
        }
        if let Some(p) = policy {
            next.policy = p;
        }
        found.sort_by_key(|f| (f.0, f.1, f.2, f.3));
        // Newly visible objects arrive with the source's values.
        let new: [Vec<u32>; 4] = [0, 1, 2, 3].map(|k| {
            (sv[k].iter().copied())
                .filter(|g| !in_t(k, *g))
                .filter(|g| k != 3 || self.artifacts[*g as usize].ty != ArtifactType::AnchorBroken)
                .collect()
        });
        out.newly_visible = [0, 1, 2, 3].map(|k| new[k].len());
        for &l in &new[0] {
            next.heads.insert(l, s.heads[&l]);
            next.metadata.insert(l, s.metadata[&l].clone());
            if anchor(self, l) {
                next.unobserved.insert(l);
            }
        }
        for &j in &new[2] {
            next.enabled.entry(j).or_insert(s.enabled[&j]);
        }
        for &a in &new[3] {
            next.open.entry(a).or_insert(s.open[&a]);
        }
        // The merged state, tried on a copy: cycles, certification, reads.
        let mut merged = self.clone();
        for (k, list) in new.iter().enumerate() {
            for g in list {
                let o = [
                    Obj::Logical(*g),
                    Obj::Revision(*g),
                    Obj::Rule(*g),
                    Obj::Artifact(*g),
                ][k];
                merged.rows.push((t, o, epoch));
            }
        }
        for (kind, logical, rule, target, payload) in &found {
            let a = merged.artifacts.len() as u32;
            merged.artifacts.push(Artifact {
                ty: ArtifactType::Conflict,
                target: RevisionId(*target),
                payload_hash: (merged.hash)(payload),
                open: true,
                author: b.principal,
                origin_branch: t,
                origin_epoch: epoch,
            });
            next.open.insert(a, true);
            out.conflicts.push(Conflict {
                kind: *kind,
                logical: *logical,
                rule: *rule,
                target: *target,
                artifact: a,
            });
        }
        merged.branches[t.0 as usize].state = next;
        let order = &mut merged.branches[t.0 as usize].order;
        for k in 0..4 {
            order[k].extend(&new[k]);
        }
        order[3].extend(out.conflicts.iter().map(|c| c.artifact));
        let p = merged.project(t);
        if let Some(path) = crate::input::cycle_path(&p.inputs) {
            let _ = path;
            return Err(TxError::CycleDetected {
                op_index,
                path: Vec::new(),
            });
        }
        // Required logicals: `certify` resolved after the merge, or every
        // logical whose head the merge set or moved, or that conflicts.
        let mv = merged.visible(t);
        let required: Vec<u32> = if !require {
            Vec::new()
        } else if certify.is_empty() {
            let mut v: BTreeSet<u32> = out.moved_heads.iter().map(|(l, ..)| *l).collect();
            v.extend(&new[0]);
            v.extend(out.conflicts.iter().filter_map(|c| c.logical));
            v.into_iter().collect()
        } else {
            let mut v = Vec::new();
            for l in &certify {
                let g = match l {
                    LogicalRef::Existing(g) => mv[0].contains(&g.0).then_some(g.0),
                    LogicalRef::Key(k) => {
                        (mv[0].iter().copied()).find(|g| merged.logicals[*g as usize].key == *k)
                    }
                    LogicalRef::New(_) => None,
                };
                v.push(g.ok_or(TxError::UnknownRef)?);
            }
            // The first failing logical is the first by id.
            v.sort_unstable();
            v.dedup();
            v
        };
        if let Some(e) = read_error {
            return Err(e);
        }
        // A5 on the merged state, newly visible revisions counting as new,
        // in canonical order.
        let merged_policy = &p.inputs.policy;
        for (n, r) in p.inputs.revisions.iter().enumerate() {
            let logical = &p.inputs.logicals[r.logical.index()];
            if matches!(logical.kind, RecordKind::Evidence | RecordKind::CodeAnchor)
                || !merged_policy.require_deps.contains(&logical.kind)
            {
                continue;
            }
            let g = p.to_global[1][n];
            let reason = if p.inputs.base[n] {
                Some(PolicyReason::BaseNotAllowed)
            } else if new[1].contains(&g)
                && !(p.inputs.rules.iter()).any(|j| j.conclusion.index() == n && j.enabled)
            {
                Some(PolicyReason::NoEnabledRule)
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(TxError::PolicyViolation {
                    kind: logical.kind,
                    logical: LogicalId(p.to_global[0][r.logical.index()]),
                    reason,
                });
            }
        }
        for (l, r) in &reads {
            let now = tstate.heads[l];
            if now != *r {
                return Err(TxError::ReadSetConflict {
                    logical: LogicalId(*l),
                    read: RevisionId(*r),
                    now: RevisionId(now),
                });
            }
        }
        if !required.is_empty() {
            let d = jacobi(&p.inputs);
            for l in required {
                let head = merged.branches[t.0 as usize].state.heads[&l];
                let local = p.to_local[1][&head] as usize;
                // A conflict on any revision of the logical (a base, a rule's
                // conclusion, an artifact's target) makes it CONFLICTED, even
                // when its head is certified (§6.3 step 6).
                let status = if out.conflicts.iter().any(|c| c.logical == Some(l)) {
                    Summary::Conflicted
                } else {
                    summarize(&d.nodes[local].dimensions)
                };
                if status != Summary::Certified {
                    return Err(TxError::MergeNotCertified {
                        logical: LogicalId(l),
                        status,
                    });
                }
            }
        }
        merged.syncs.push(Sync {
            src: source,
            dst: t,
            epoch,
            state: s,
        });
        merged.epoch = epoch;
        let allocations = Allocations {
            new_artifacts: out
                .conflicts
                .iter()
                .map(|c| ArtifactId(c.artifact))
                .collect(),
            ..Allocations::default()
        };
        *self = merged;
        Ok((ApplyResult { epoch, allocations }, out))
    }
}

/// Rewrites every `Existing` id in a batch.
fn remap(b: &Batch, map: &dyn Fn(usize, u32) -> u32) -> Batch {
    let l = |x: &LogicalRef| match x {
        LogicalRef::Existing(g) => LogicalRef::Existing(LogicalId(map(0, g.0))),
        other => other.clone(),
    };
    let r = |x: &RevRef| match x {
        RevRef::Existing(g) => RevRef::Existing(RevisionId(map(1, g.0))),
        other => *other,
    };
    let j = |x: &RuleRef| match x {
        RuleRef::Existing(g) => RuleRef::Existing(RuleId(map(2, g.0))),
        other => *other,
    };
    let a = |x: &ArtifactRef| match x {
        ArtifactRef::Existing(g) => ArtifactRef::Existing(ArtifactId(map(3, g.0))),
        other => *other,
    };
    let mut out = b.clone();
    for op in &mut out.ops {
        match op {
            Op::AddRevision { logical, .. } | Op::SetMetadata { logical, .. } => {
                *logical = l(logical)
            }
            Op::SetHead {
                logical,
                expected,
                new,
            } => {
                *logical = l(logical);
                *expected = r(expected);
                *new = r(new);
            }
            Op::AssertBase { rev } | Op::RetractBase { rev } => *rev = r(rev),
            Op::AddRule {
                conclusion,
                incidences,
                ..
            } => {
                *conclusion = r(conclusion);
                for (p, _) in incidences {
                    *p = r(p);
                }
            }
            Op::SetRuleEnabled { rule, .. } => *rule = j(rule),
            Op::Rebind { rule, from, to } => {
                *rule = j(rule);
                *from = r(from);
                *to = r(to);
            }
            Op::RecordValidation {
                rule, curvec_seen, ..
            } => {
                *rule = j(rule);
                for (x, y) in curvec_seen {
                    *x = LogicalId(map(0, x.0));
                    *y = RevisionId(map(1, y.0));
                }
            }
            Op::OpenArtifact { target, .. } => *target = r(target),
            Op::ResolveArtifact { id } => *id = a(id),
            Op::ExpectHeads { entries, .. } => {
                for (x, y) in entries {
                    *x = l(x);
                    *y = RevisionId(map(1, y.0));
                }
            }
            _ => {}
        }
    }
    out
}

/// Errors naming pre-batch objects get their global ids; staged ids stay.
fn map_ids(e: TxError, p: &Projection) -> TxError {
    let g = |k: usize, x: u32| p.to_global[k].get(x as usize).copied().unwrap_or(x);
    match e {
        TxError::ReadSetConflict { logical, read, now } => TxError::ReadSetConflict {
            logical: LogicalId(g(0, logical.0)),
            read: RevisionId(g(1, read.0)),
            now: RevisionId(g(1, now.0)),
        },
        TxError::InvalidReadSet { logical } => TxError::InvalidReadSet {
            logical: LogicalId(g(0, logical.0)),
        },
        other => other,
    }
}
