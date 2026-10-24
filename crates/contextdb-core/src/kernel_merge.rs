//! The kernel half of `Merge` (SPEC SD19, ADR 0013 §6): sync points and the
//! merge base by containment, three-way classification in global ids, the
//! plan for the target's database, certification and the sync point.
use super::{Branch, INVISIBLE, IdMap, Import, Kernel, Kind, map_error, map_result};
use crate::{CommitContext, Db, Maintainer, StatsSink, merge::MergePlan};
use contextdb_model::*;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

/// A branch's state made available to another (ADR 0013 §6.1): at a fork,
/// the parent's state at the fork epoch; at a merge, the source's state at
/// the merge epoch. Every sync point is kept (retention, R1).
#[derive(Clone, Debug)]
pub(super) struct SyncPoint<M: Maintainer, S: StatsSink> {
    pub src: BranchId,
    pub dst: BranchId,
    pub epoch: Epoch,
    pub db: Arc<Db<M, S>>,
    pub ids: Arc<IdMap>,
}

/// A branch state: the branch as of an epoch.
type State = (BranchId, Epoch);
/// Containment answers already computed, `(x, y) → x ⊑ y`.
type Memo = HashMap<(State, State), bool>;

/// What a merge conflicts on (ADR 0013 §6.2), in canonical opening order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictKind {
    Head,
    Base,
    Enabled,
    ArtifactOpen,
    Validation,
}
/// One `Conflict` artifact a merge opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeConflictEntry {
    pub kind: ConflictKind,
    pub logical: Option<LogicalId>,
    pub rule: Option<RuleId>,
    /// The artifact's target revision.
    pub target: RevisionId,
    pub artifact: ArtifactId,
    /// The canonical payload; the artifact's `payload_hash` is its BLAKE3.
    pub payload: Vec<u8>,
}
/// A metadata field both sides changed differently: listed, never gating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataField {
    Tags,
    Dirs,
}
/// A merge's typed result (the `CommitResult` merge trailer of §8).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub source: BranchId,
    /// Heads the merge moved on existing logicals, `(logical, old, new)`.
    pub moved_heads: Vec<(LogicalId, RevisionId, RevisionId)>,
    pub conflicts: Vec<MergeConflictEntry>,
    pub metadata_conflicts: Vec<(LogicalId, MetadataField)>,
    /// Newly visible logicals, revisions, rules and artifacts.
    pub newly_visible: [usize; 4],
    /// Values both sides changed the same way.
    pub convergent: usize,
}

/// One branch state read with global ids.
struct View<'a, M: Maintainer, S: StatsSink> {
    db: &'a Db<M, S>,
    ids: &'a IdMap,
}
impl<M: Maintainer, S: StatsSink> View<'_, M, S> {
    fn local(&self, k: Kind, g: u32) -> Option<u32> {
        let l = self.ids.local(k, g);
        (l != INVISIBLE).then_some(l)
    }
    fn global(&self, k: Kind, l: u32) -> u32 {
        self.ids.global(k, l)
    }
    fn all(&self, k: Kind) -> &[u32] {
        &self.ids.to_global[k as usize]
    }
    fn head(&self, gl: u32) -> Option<u32> {
        let r = self.db.head(LogicalId(self.local(Kind::Logical, gl)?))?;
        Some(self.global(Kind::Revision, r.0))
    }
    fn base(&self, gr: u32) -> Option<bool> {
        let r = self.local(Kind::Revision, gr)?;
        self.db.is_base(RevisionId(r)).ok()
    }
    fn enabled(&self, gj: u32) -> Option<bool> {
        let j = self.local(Kind::Rule, gj)?;
        Some(self.db.rules.enabled[j as usize])
    }
    fn open(&self, ga: u32) -> Option<bool> {
        let a = self.local(Kind::Artifact, ga)?;
        Some(self.db.artifact_record(ArtifactId(a)).open)
    }
    fn metadata(&self, gl: u32) -> Option<Metadata> {
        let l = self.local(Kind::Logical, gl)?;
        Some(self.db.metadata(LogicalId(l)))
    }
    fn kind(&self, gl: u32) -> Option<RecordKind> {
        Some(self.db.kind_of(LogicalId(self.local(Kind::Logical, gl)?)))
    }
    fn logical_of(&self, gr: u32) -> u32 {
        let r = self.local(Kind::Revision, gr).expect("visible revision");
        let l = self.db.revision_record(RevisionId(r)).logical;
        self.global(Kind::Logical, l.0)
    }
    /// Validation records under global ids, curvecs sorted by logical.
    fn validations(&self) -> BTreeMap<(u32, Vec<(u32, u32)>), ValidationRecord> {
        (self.db.validations().iter())
            .map(|((j, curvec), record)| {
                let mut curvec: Vec<_> = (curvec.iter())
                    .map(|(l, r)| {
                        (
                            self.global(Kind::Logical, l.0),
                            self.global(Kind::Revision, r.0),
                        )
                    })
                    .collect();
                curvec.sort_unstable();
                ((self.global(Kind::Rule, j.0), curvec), record.clone())
            })
            .collect()
    }
}

/// How one value merges (ADR 0013 §6.2, with the criss-cross rule of §6.1).
enum Outcome<T> {
    Keep,
    Take(T),
    Conflict,
}
fn classify<T: PartialEq + Clone>(
    bases: &[Option<T>],
    s: &Option<T>,
    t: &Option<T>,
    convergent: &mut usize,
) -> Outcome<T> {
    // Several maximal bases agree on a value or the value conflicts.
    let b = match bases {
        [b] => b,
        _ if s == t => return Outcome::Keep,
        [first, rest @ ..] if rest.iter().all(|b| b == first) => first,
        _ => return Outcome::Conflict,
    };
    if s == b {
        Outcome::Keep
    } else if t == b {
        // Visibility only grows, so a value visible in a base is in S.
        Outcome::Take(s.clone().expect("visible in the source"))
    } else if s == t {
        *convergent += 1;
        Outcome::Keep
    } else {
        Outcome::Conflict
    }
}

/// `dirs` narrowed on both sides: prefix-aware intersection (§6.2, N3).
fn intersect_dirs(a: &BTreeSet<String>, b: &BTreeSet<String>) -> BTreeSet<String> {
    let under = |x: &str, y: &str| x == y || x.starts_with(&format!("{y}/"));
    let mut out = BTreeSet::new();
    for x in a {
        for y in b {
            if under(x, y) {
                out.insert(x.clone());
            } else if under(y, x) {
                out.insert(y.clone());
            }
        }
    }
    out
}

/// Canonical conflict payload (§6.2): merge epoch, source, kind, subject,
/// then the base, source and target values.
fn payload(
    epoch: Epoch,
    source: BranchId,
    kind: ConflictKind,
    subject: u32,
    values: &[Vec<u8>],
) -> Vec<u8> {
    let mut out = b"CTXMERGE1".to_vec();
    out.extend(epoch.0.to_le_bytes());
    out.extend(source.0.to_le_bytes());
    out.push(kind as u8);
    out.extend(subject.to_le_bytes());
    out.extend((values.len() as u32).to_le_bytes());
    for v in values {
        out.extend((v.len() as u32).to_le_bytes());
        out.extend(v);
    }
    out
}
fn enc_rev(v: &Option<u32>) -> Vec<u8> {
    v.map_or(vec![0], |r| [vec![1], r.to_le_bytes().to_vec()].concat())
}
fn enc_bool(v: &Option<bool>) -> Vec<u8> {
    vec![v.map_or(0, |b| 1 + u8::from(b))]
}
fn enc_validation(v: &Option<ValidationRecord>) -> Vec<u8> {
    v.as_ref().map_or(vec![0], |r| {
        let mut out = vec![1, r.verdict as u8];
        out.extend(r.validator.as_bytes());
        out
    })
}

/// A conflict found by classification, before its artifact exists.
struct Found {
    kind: ConflictKind,
    logical: Option<u32>,
    rule: Option<u32>,
    target: u32,
    payload: Vec<u8>,
}

impl<M: Maintainer, S: StatsSink> Kernel<M, S> {
    /// Whether the state `x` is contained in branch state `y` (§6.1, N1):
    /// it is `y`'s own earlier state, or contained in a state synced into
    /// `y` at or before `y`'s epoch.
    fn contained(&self, x: (BranchId, Epoch), y: (BranchId, Epoch), memo: &mut Memo) -> bool {
        if x.0 == y.0 && x.1 <= y.1 {
            return true;
        }
        if let Some(known) = memo.get(&(x, y)) {
            return *known;
        }
        // Sync epochs strictly decrease along the recursion: one commit per
        // epoch, so no sync into `src` shares the epoch of one out of it.
        let found = (self.syncs.iter())
            .filter(|s| s.dst == y.0 && s.epoch <= y.1 && (s.src, s.epoch) != y)
            .any(|s| self.contained(x, (s.src, s.epoch), memo));
        memo.insert((x, y), found);
        found
    }
    /// The maximal stored states contained in both branches now (§6.1).
    fn merge_bases(&self, source: BranchId, target: BranchId) -> Vec<usize> {
        let now = Epoch(u64::MAX);
        let mut memo = HashMap::new();
        let mut candidates: Vec<usize> = Vec::new();
        for (n, s) in self.syncs.iter().enumerate() {
            let state = (s.src, s.epoch);
            if candidates
                .iter()
                .any(|c| (self.syncs[*c].src, self.syncs[*c].epoch) == state)
            {
                continue;
            }
            if self.contained(state, (source, now), &mut memo)
                && self.contained(state, (target, now), &mut memo)
            {
                candidates.push(n);
            }
        }
        let state = |n: usize| (self.syncs[n].src, self.syncs[n].epoch);
        (candidates.iter().copied())
            .filter(|c| {
                !candidates
                    .iter()
                    .any(|d| d != c && self.contained(state(*c), state(*d), &mut memo))
            })
            .collect()
    }

    /// `Merge` with its `ExpectHeads` (ADR 0013 §6.3, §9 order). Returns the
    /// target's commit result and the merge report in global ids.
    pub(super) fn merge(
        &mut self,
        b: &Batch,
        epoch: Epoch,
        publish: bool,
    ) -> Result<(CommitResult, MergeReport), TxError> {
        let target = b.branch;
        let (op_index, source, require_certified, certify) = (b.ops.iter().enumerate())
            .find_map(|(n, op)| match op {
                Op::Merge {
                    source,
                    require_certified,
                    certify,
                } => Some((n, *source, *require_certified, certify)),
                _ => None,
            })
            .expect("a merge batch");
        if source.0 as usize >= self.branches.len() || source == target {
            return Err(TxError::InvalidMerge);
        }
        if b.channel == Channel::Observer {
            return Err(TxError::ChannelRestricted {
                op: "Merge".into(),
                channel: b.channel,
            });
        }
        // Read sets resolve against the target before the merge. Their Op
        // errors come in Op order: before the merge's own when every
        // `ExpectHeads` precedes it, else after them (§9).
        let early = (b.ops.iter().enumerate())
            .all(|(n, op)| n < op_index || !matches!(op, Op::ExpectHeads { .. }));
        let (reads, late) = match self.read_set(b) {
            Ok(reads) => (reads, None),
            Err(e) if early => return Err(e),
            Err(e) => (Vec::new(), Some(e)),
        };
        let bases = self.merge_bases(source, target);
        crate::invariant_assert!(!bases.is_empty(), "fork bases are common ancestors");
        let (sb, tb) = (
            &self.branches[source.0 as usize],
            &self.branches[target.0 as usize],
        );
        let s = View {
            db: &sb.db,
            ids: &sb.ids,
        };
        let t = View {
            db: &tb.db,
            ids: &tb.ids,
        };
        let bs: Vec<_> = (bases.iter())
            .map(|n| View {
                db: &self.syncs[*n].db,
                ids: &self.syncs[*n].ids,
            })
            .collect();
        let mut report = MergeReport {
            source,
            ..MergeReport::default()
        };
        let mut found: Vec<Found> = Vec::new();
        let mut convergent = 0;
        // Policy first: an unmergeable policy refuses the merge.
        let policy = {
            let values: Vec<_> = bs.iter().map(|v| Some(v.db.policy().clone())).collect();
            match classify(
                &values,
                &Some(s.db.policy().clone()),
                &Some(t.db.policy().clone()),
                &mut convergent,
            ) {
                Outcome::Conflict => {
                    return Err(TxError::MergeConflict {
                        kind: MergeConflictKind::Policy,
                    });
                }
                Outcome::Take(p) => Some(p),
                Outcome::Keep => None,
            }
        };
        // Newly visible objects, by kind, in ascending global id.
        let new = |k: Kind| -> Vec<u32> {
            let mut v: Vec<u32> = (s.all(k).iter().copied())
                .filter(|g| t.local(k, *g).is_none())
                .filter(|g| {
                    // AnchorBroken is observer state; the target's scan decides it.
                    k != Kind::Artifact
                        || s.db.artifact_record(ArtifactId(s.local(k, *g).unwrap())).ty
                            != ArtifactType::AnchorBroken
                })
                .collect();
            v.sort_unstable();
            v
        };
        let new_ids = [
            new(Kind::Logical),
            new(Kind::Revision),
            new(Kind::Rule),
            new(Kind::Artifact),
        ];
        let observed_in_t = |gl: u32| t.kind(gl) == Some(RecordKind::CodeAnchor);
        // Heads and metadata of logicals the target already sees.
        let mut heads: BTreeMap<u32, u32> = BTreeMap::new();
        let mut metadata: BTreeMap<u32, Metadata> = BTreeMap::new();
        for &gl in s.all(Kind::Logical) {
            if t.local(Kind::Logical, gl).is_none() {
                continue;
            }
            if !observed_in_t(gl) {
                let values: Vec<_> = bs.iter().map(|v| v.head(gl)).collect();
                let (sv, tv) = (s.head(gl), t.head(gl));
                match classify(&values, &sv, &tv, &mut convergent) {
                    Outcome::Keep => {}
                    Outcome::Take(r) => {
                        heads.insert(gl, r);
                    }
                    Outcome::Conflict => {
                        let mut v: Vec<_> = values.iter().map(enc_rev).collect();
                        v.extend([enc_rev(&sv), enc_rev(&tv)]);
                        found.push(Found {
                            kind: ConflictKind::Head,
                            logical: Some(gl),
                            rule: None,
                            target: tv.expect("visible logicals have heads"),
                            payload: payload(epoch, source, ConflictKind::Head, gl, &v),
                        });
                    }
                }
            }
            let (sm, tm) = (s.metadata(gl).unwrap(), t.metadata(gl).unwrap());
            let bm: Vec<_> = bs.iter().map(|v| v.metadata(gl)).collect();
            let mut merged = tm.clone();
            let tags: Vec<_> = bm
                .iter()
                .map(|m| m.as_ref().map(|m| m.tags.clone()))
                .collect();
            match classify(
                &tags,
                &Some(sm.tags.clone()),
                &Some(tm.tags.clone()),
                &mut convergent,
            ) {
                Outcome::Keep => {}
                Outcome::Take(v) => merged.tags = v,
                Outcome::Conflict => report
                    .metadata_conflicts
                    .push((LogicalId(gl), MetadataField::Tags)),
            }
            let dirs: Vec<_> = bm
                .iter()
                .map(|m| m.as_ref().map(|m| m.dirs.clone()))
                .collect();
            match classify(
                &dirs,
                &Some(sm.dirs.clone()),
                &Some(tm.dirs.clone()),
                &mut convergent,
            ) {
                Outcome::Keep => {}
                Outcome::Take(v) => merged.dirs = v,
                Outcome::Conflict => {
                    let both = intersect_dirs(&sm.dirs, &tm.dirs);
                    // Empty would mean "not narrowed": never applied.
                    if both.is_empty() || sm.dirs.is_empty() || tm.dirs.is_empty() {
                        report
                            .metadata_conflicts
                            .push((LogicalId(gl), MetadataField::Dirs));
                    } else {
                        merged.dirs = both;
                    }
                }
            }
            if merged != tm {
                metadata.insert(gl, merged);
            }
        }
        // Base bits per revision the source sees.
        let mut base: BTreeMap<u32, bool> = BTreeMap::new();
        for &gr in s.all(Kind::Revision) {
            let gl = s.logical_of(gr);
            let existing_logical = t.local(Kind::Logical, gl).is_some();
            if existing_logical && observed_in_t(gl) {
                // Observer domain: new revisions arrive unobserved, not base.
                if t.local(Kind::Revision, gr).is_none() {
                    base.insert(gr, false);
                }
                continue;
            }
            let values: Vec<_> = bs.iter().map(|v| v.base(gr)).collect();
            let (sv, tv) = (s.base(gr), t.base(gr));
            match classify(&values, &sv, &tv, &mut convergent) {
                Outcome::Keep => {}
                Outcome::Take(v) => {
                    base.insert(gr, v);
                }
                Outcome::Conflict => {
                    let mut v: Vec<_> = values.iter().map(enc_bool).collect();
                    v.extend([enc_bool(&sv), enc_bool(&tv)]);
                    found.push(Found {
                        kind: ConflictKind::Base,
                        logical: Some(gl),
                        rule: None,
                        target: gr,
                        payload: payload(epoch, source, ConflictKind::Base, gr, &v),
                    });
                }
            }
        }
        // Enabled per rule.
        let mut enabled: BTreeMap<u32, bool> = BTreeMap::new();
        for &gj in s.all(Kind::Rule) {
            let values: Vec<_> = bs.iter().map(|v| v.enabled(gj)).collect();
            let (sv, tv) = (s.enabled(gj), t.enabled(gj));
            match classify(&values, &sv, &tv, &mut convergent) {
                Outcome::Keep => {}
                Outcome::Take(v) => {
                    enabled.insert(gj, v);
                }
                Outcome::Conflict => {
                    let j = s.local(Kind::Rule, gj).unwrap();
                    let conclusion = s.db.rule_record(RuleId(j)).conclusion;
                    let target = s.global(Kind::Revision, conclusion.0);
                    let mut v: Vec<_> = values.iter().map(enc_bool).collect();
                    v.extend([enc_bool(&sv), enc_bool(&tv)]);
                    found.push(Found {
                        kind: ConflictKind::Enabled,
                        logical: Some(s.logical_of(target)),
                        rule: Some(gj),
                        target,
                        payload: payload(epoch, source, ConflictKind::Enabled, gj, &v),
                    });
                }
            }
        }
        // Open or resolved per artifact.
        let mut open: BTreeMap<u32, bool> = BTreeMap::new();
        for &ga in s.all(Kind::Artifact) {
            let a =
                s.db.artifact_record(ArtifactId(s.local(Kind::Artifact, ga).unwrap()));
            if a.ty == ArtifactType::AnchorBroken {
                continue;
            }
            let values: Vec<_> = bs.iter().map(|v| v.open(ga)).collect();
            let (sv, tv) = (s.open(ga), t.open(ga));
            match classify(&values, &sv, &tv, &mut convergent) {
                Outcome::Keep => {}
                Outcome::Take(v) => {
                    open.insert(ga, v);
                }
                Outcome::Conflict => {
                    let target = s.global(Kind::Revision, a.target.0);
                    let mut v: Vec<_> = values.iter().map(enc_bool).collect();
                    v.extend([enc_bool(&sv), enc_bool(&tv)]);
                    found.push(Found {
                        kind: ConflictKind::ArtifactOpen,
                        logical: Some(s.logical_of(target)),
                        rule: None,
                        target,
                        payload: payload(epoch, source, ConflictKind::ArtifactOpen, ga, &v),
                    });
                }
            }
        }
        // Validation records per (rule, curvec).
        let mut validations = BTreeMap::new();
        let (sval, tval) = (s.validations(), t.validations());
        let bval: Vec<_> = bs.iter().map(|v| v.validations()).collect();
        for (key, record) in &sval {
            let values: Vec<_> = bval.iter().map(|m| m.get(key).cloned()).collect();
            let (sv, tv) = (Some(record.clone()), tval.get(key).cloned());
            match classify(&values, &sv, &tv, &mut convergent) {
                Outcome::Keep => {}
                Outcome::Take(v) => {
                    validations.insert(key.clone(), v);
                }
                // Only different verdicts conflict.
                Outcome::Conflict if tv.as_ref().is_some_and(|t| t.verdict == record.verdict) => {
                    convergent += 1;
                }
                Outcome::Conflict => {
                    let j = s.local(Kind::Rule, key.0).unwrap();
                    let conclusion = s.db.rule_record(RuleId(j)).conclusion;
                    let target = s.global(Kind::Revision, conclusion.0);
                    let mut v: Vec<_> = values.iter().map(enc_validation).collect();
                    v.extend([enc_validation(&sv), enc_validation(&tv)]);
                    found.push(Found {
                        kind: ConflictKind::Validation,
                        logical: Some(s.logical_of(target)),
                        rule: Some(key.0),
                        target,
                        payload: payload(epoch, source, ConflictKind::Validation, key.0, &v),
                    });
                }
            }
        }
        found.sort_by_key(|f| (f.kind, f.logical, f.rule, f.target));
        report.convergent = convergent;
        report.newly_visible = [0, 1, 2, 3].map(|k| new_ids[k].len());

        // The plan in the target's local ids: new objects take the next
        // local ids in ascending global order.
        let lens = [Kind::Logical, Kind::Revision, Kind::Rule, Kind::Artifact]
            .map(|k| t.ids.len(k) as u32);
        let new_local: [HashMap<u32, u32>; 4] = [0, 1, 2, 3].map(|k| {
            (new_ids[k].iter().enumerate())
                .map(|(i, g)| (*g, lens[k] + i as u32))
                .collect()
        });
        let local = |k: Kind, g: u32| -> u32 {
            t.local(k, g)
                .or_else(|| new_local[k as usize].get(&g).copied())
                .expect("visible after the merge")
        };
        let sl = |k: Kind, g: u32| s.local(k, g).expect("visible in the source");
        let mut plan = MergePlan::default();
        for &gl in &new_ids[0] {
            let l = LogicalId(sl(Kind::Logical, gl));
            let head = s.head(gl).expect("visible logicals have heads");
            plan.logicals.push((
                s.db.logical_record(l),
                RevisionId(local(Kind::Revision, head)),
                s.db.metadata(l),
            ));
        }
        for &gr in &new_ids[1] {
            let mut r = s.db.revision_record(RevisionId(sl(Kind::Revision, gr)));
            r.logical = LogicalId(local(Kind::Logical, s.global(Kind::Logical, r.logical.0)));
            let is_base = base.remove(&gr).unwrap_or(false);
            plan.revisions.push((r, is_base));
        }
        for &gj in &new_ids[2] {
            let mut j = s.db.rule_record(RuleId(sl(Kind::Rule, gj)));
            let rev =
                |r: RevisionId| RevisionId(local(Kind::Revision, s.global(Kind::Revision, r.0)));
            j.conclusion = rev(j.conclusion);
            for p in &mut j.premises {
                p.premise = rev(p.premise);
            }
            j.premises
                .sort_unstable_by(|a, b| a.premise.cmp(&b.premise).then(b.kind.cmp(&a.kind)));
            for r in &mut j.references {
                *r = rev(*r);
            }
            j.references.sort_unstable();
            j.enabled = enabled.remove(&gj).unwrap_or(j.enabled);
            plan.rules.push(j);
        }
        for &ga in &new_ids[3] {
            let mut a =
                s.db.artifact_record(ArtifactId(sl(Kind::Artifact, ga)))
                    .clone();
            a.target = RevisionId(local(Kind::Revision, s.global(Kind::Revision, a.target.0)));
            a.open = open.remove(&ga).unwrap_or(a.open);
            plan.artifacts.push(a);
        }
        for f in &found {
            plan.artifacts.push(Artifact {
                ty: ArtifactType::Conflict,
                target: RevisionId(local(Kind::Revision, f.target)),
                payload_hash: *blake3::hash(&f.payload).as_bytes(),
                open: true,
                author: b.principal,
                origin_branch: target,
                origin_epoch: epoch,
            });
        }
        plan.conflicts = found.len();
        for (gl, gr) in &heads {
            let l = LogicalId(local(Kind::Logical, *gl));
            plan.heads.insert(l, RevisionId(local(Kind::Revision, *gr)));
            let old = t.head(*gl).expect("visible");
            report
                .moved_heads
                .push((LogicalId(*gl), RevisionId(old), RevisionId(*gr)));
        }
        for (gr, v) in base {
            plan.base.insert(RevisionId(local(Kind::Revision, gr)), v);
        }
        for (gj, v) in enabled {
            plan.enabled.insert(RuleId(local(Kind::Rule, gj)), v);
        }
        for (ga, v) in open {
            plan.artifact_open
                .insert(ArtifactId(local(Kind::Artifact, ga)), v);
        }
        for (gl, m) in metadata {
            plan.metadata.insert(LogicalId(local(Kind::Logical, gl)), m);
        }
        for ((gj, curvec), record) in validations {
            let mut curvec: Vec<_> = (curvec.iter())
                .map(|(l, r)| {
                    (
                        LogicalId(local(Kind::Logical, *l)),
                        RevisionId(local(Kind::Revision, *r)),
                    )
                })
                .collect();
            curvec.sort_unstable();
            plan.validations
                .insert((RuleId(local(Kind::Rule, gj)), curvec), record);
        }
        plan.policy = policy;
        // New anchors arrive with the source's head and base as an initial
        // observation, unobserved here (§6.2, N9, R2).
        plan.unobserved = (new_ids[0].iter())
            .filter(|g| s.kind(**g) == Some(RecordKind::CodeAnchor))
            .map(|g| LogicalId(local(Kind::Logical, *g)))
            .collect();

        // Local ⇄ global for the merged target: new objects, then the
        // Conflicts' fresh artifact ids.
        let mut new_globals = new_ids.clone();
        new_globals[3]
            .extend((0..found.len() as u32).map(|i| self.next[Kind::Artifact as usize] + i));
        let t_ids = Arc::clone(&tb.ids);
        let to_global = |k: Kind, x: u32| -> u32 {
            match x.checked_sub(lens[k as usize]) {
                None => t_ids.global(k, x),
                Some(i) => new_globals[k as usize][i as usize],
            }
        };
        // Required logicals (step 6): `certify`, resolved on the merged
        // target (so `UnknownRef` follows step 3), or every logical whose
        // head the merge set or moved, or that has a conflict.
        let required: Result<Option<Vec<LogicalId>>, TxError> = if !require_certified {
            Ok(None)
        } else if certify.is_empty() {
            // The merge sets the heads of newly visible logicals too (T20b).
            let mut v: BTreeSet<u32> = heads.keys().copied().collect();
            v.extend(&new_ids[0]);
            v.extend(found.iter().filter_map(|f| f.logical));
            Ok(Some(
                v.into_iter()
                    .map(|g| LogicalId(local(Kind::Logical, g)))
                    .collect(),
            ))
        } else {
            (certify.iter())
                .map(|l| {
                    let g = match l {
                        LogicalRef::Existing(g) => Some(g.0),
                        LogicalRef::Key(k) => {
                            (t.db.logical_by_key(k).map(|l| t.global(Kind::Logical, l.0))).or_else(
                                || s.db.logical_by_key(k).map(|l| s.global(Kind::Logical, l.0)),
                            )
                        }
                        LogicalRef::New(_) => None,
                    };
                    g.and_then(|g| {
                        t.local(Kind::Logical, g)
                            .or_else(|| new_local[0].get(&g).copied())
                    })
                    .map(LogicalId)
                    .ok_or(TxError::UnknownRef)
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some)
        };
        // Read sets compare against the pre-batch heads.
        let stale = reads.iter().find_map(|(l, read)| {
            let now = t.db.head(*l).expect("visible");
            // Local ids: `map_error` maps them.
            (now != *read).then_some(TxError::ReadSetConflict {
                logical: *l,
                read: *read,
                now,
            })
        });
        let found: Vec<_> = found
            .into_iter()
            .map(|f| (f.kind, f.logical, f.rule, f.target, f.payload))
            .collect();
        let ctx = CommitContext {
            epoch: Some(epoch),
            kernel: true,
            ..CommitContext::default()
        };
        let branch = &mut self.branches[target.0 as usize];
        let db = Arc::make_mut(&mut branch.db);
        // Steps 2–6, then the end-of-batch checks in the §9 order.
        let checked = |db: &mut Db<M, S>| -> Result<CommitResult, TxError> {
            let result = db.commit_merge(&plan, op_index, &ctx)?;
            let required = required.clone()?;
            if let Some(e) = late.clone() {
                return Err(e);
            }
            if let Some(e) = stale.clone() {
                return Err(e);
            }
            for l in required.iter().flatten() {
                let status = db.status(db.head(*l).expect("visible"))?;
                if status != Summary::Certified {
                    return Err(TxError::MergeNotCertified {
                        logical: *l,
                        status,
                    });
                }
            }
            Ok(result)
        };
        // A merge that can fail only in steps 2–3 commits directly; one
        // that checks the merged state is tried first.
        let direct = publish && stale.is_none() && late.is_none() && matches!(required, Ok(None));
        let result = if direct {
            db.commit_merge(&plan, op_index, &ctx)
        } else {
            db.dry(checked).and_then(|r| match publish {
                true => db.commit_merge(&plan, op_index, &ctx),
                false => Ok(r),
            })
        };
        let result = result.map_err(|e| map_error(e, &to_global))?;
        let result = map_result(result, &to_global);
        report.conflicts = (found.into_iter().zip(&result.allocations.new_artifacts))
            .map(
                |((kind, logical, rule, target, payload), artifact)| MergeConflictEntry {
                    kind,
                    logical: logical.map(LogicalId),
                    rule: rule.map(RuleId),
                    target: RevisionId(target),
                    artifact: *artifact,
                    payload,
                },
            )
            .collect();
        if !publish {
            return Ok((result, report));
        }
        drop(t_ids);
        // Publish ids, visibility rows and the sync point.
        let ids = Arc::make_mut(&mut branch.ids);
        for (k, list) in [Kind::Logical, Kind::Revision, Kind::Rule, Kind::Artifact]
            .into_iter()
            .zip(&new_globals)
        {
            for g in list {
                ids.push(k, *g);
            }
        }
        self.next[Kind::Artifact as usize] += report.conflicts.len() as u32;
        let rows = (new_ids[0].iter().map(|g| Import::Logical(LogicalId(*g))))
            .chain(new_ids[1].iter().map(|g| Import::Revision(RevisionId(*g))))
            .chain(new_ids[2].iter().map(|g| Import::Rule(RuleId(*g))))
            .chain(new_ids[3].iter().map(|g| Import::Artifact(ArtifactId(*g))));
        branch.imports.extend(rows.map(|i| (i, epoch)));
        let src = &self.branches[source.0 as usize];
        let point = SyncPoint {
            src: source,
            dst: target,
            epoch,
            db: Arc::clone(&src.db),
            ids: Arc::clone(&src.ids),
        };
        self.syncs.push(point);
        Ok((result, report))
    }

    /// The batch's `ExpectHeads` entries on the target, as local
    /// `(logical, read)` pairs: `UnknownRef` and `InvalidReadSet` here; the
    /// comparison happens at batch end.
    fn read_set(&self, b: &Batch) -> Result<Vec<(LogicalId, RevisionId)>, TxError> {
        let branch: &Branch<M, S> = &self.branches[b.branch.0 as usize];
        let t = View {
            db: &branch.db,
            ids: &branch.ids,
        };
        let mut out: BTreeMap<LogicalId, RevisionId> = BTreeMap::new();
        for op in &b.ops {
            let Op::ExpectHeads { entries, .. } = op else {
                continue;
            };
            for (logical, read) in entries {
                let l = match logical {
                    LogicalRef::Existing(g) => t.local(Kind::Logical, g.0).map(LogicalId),
                    LogicalRef::Key(k) => t.db.logical_by_key(k),
                    LogicalRef::New(_) => None,
                }
                .ok_or(TxError::UnknownRef)?;
                let r = t
                    .local(Kind::Revision, read.0)
                    .map(RevisionId)
                    .ok_or(TxError::UnknownRef)?;
                if out.insert(l, r).is_some_and(|old| old != r) {
                    return Err(TxError::InvalidReadSet {
                        logical: LogicalId(t.global(Kind::Logical, l.0)),
                    });
                }
            }
        }
        Ok(out.into_iter().collect())
    }
}
