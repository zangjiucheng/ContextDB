use crate::{
    InputView, PearceKelly, StatsSink,
    stage::{Stage, rule_key},
};
use contextdb_model::*;
use std::collections::BTreeSet;

struct Resolver<'a, 'b> {
    i: &'a Stage<'b>,
    a: &'a Allocations,
}
impl Resolver<'_, '_> {
    fn r(&self, r: RevRef) -> Result<RevRef, TxError> {
        let id = match r {
            RevRef::Existing(id) => id,
            RevRef::New(n) => *self
                .a
                .new_revisions
                .get(n as usize)
                .ok_or(TxError::UnknownRef)?,
        };
        if id.index() >= self.i.revision_count() {
            return Err(TxError::UnknownRef);
        }
        Ok(id.into())
    }
    fn l(&self, l: &LogicalRef) -> Result<LogicalRef, TxError> {
        let id = match l {
            LogicalRef::Existing(id) => *id,
            LogicalRef::New(n) => *self
                .a
                .new_logicals
                .get(*n as usize)
                .ok_or(TxError::UnknownRef)?,
            LogicalRef::Key(k) => self.i.find_key(k).ok_or(TxError::UnknownRef)?,
        };
        if id.index() >= self.i.logical_count() {
            return Err(TxError::UnknownRef);
        }
        Ok(id.into())
    }
    fn j(&self, j: RuleRef) -> Result<RuleRef, TxError> {
        let id = match j {
            RuleRef::Existing(id) => id,
            RuleRef::New(n) => *self
                .a
                .new_rules
                .get(n as usize)
                .ok_or(TxError::UnknownRef)?,
        };
        if id.index() >= self.i.rule_count() {
            return Err(TxError::UnknownRef);
        }
        Ok(id.into())
    }
    fn a(&self, a: ArtifactRef) -> Result<ArtifactRef, TxError> {
        let id = match a {
            ArtifactRef::Existing(id) => id,
            ArtifactRef::New(n) => *self
                .a
                .new_artifacts
                .get(n as usize)
                .ok_or(TxError::UnknownRef)?,
        };
        if id.index() >= self.i.artifact_count() {
            return Err(TxError::UnknownRef);
        }
        Ok(id.into())
    }
    fn op(&self, op: &Op) -> Result<Op, TxError> {
        Ok(match op {
            Op::CreateLogical { .. } | Op::SetPolicy { .. } | Op::ObserverWatermark { .. } => {
                op.clone()
            }
            Op::AddRevision {
                logical,
                content,
                adopt_as_base,
            } => Op::AddRevision {
                logical: self.l(logical)?,
                content: *content,
                adopt_as_base: *adopt_as_base,
            },
            Op::SetHead {
                logical,
                expected,
                new,
            } => Op::SetHead {
                logical: self.l(logical)?,
                expected: self.r(*expected)?,
                new: self.r(*new)?,
            },
            Op::AssertBase { rev } => Op::AssertBase { rev: self.r(*rev)? },
            Op::RetractBase { rev } => Op::RetractBase { rev: self.r(*rev)? },
            Op::AddRule {
                conclusion,
                incidences,
                enabled,
            } => Op::AddRule {
                conclusion: self.r(*conclusion)?,
                incidences: incidences
                    .iter()
                    .map(|(r, k)| Ok((self.r(*r)?, *k)))
                    .collect::<Result<_, TxError>>()?,
                enabled: *enabled,
            },
            Op::SetRuleEnabled { rule, enabled } => Op::SetRuleEnabled {
                rule: self.j(*rule)?,
                enabled: *enabled,
            },
            Op::Rebind { rule, from, to } => Op::Rebind {
                rule: self.j(*rule)?,
                from: self.r(*from)?,
                to: self.r(*to)?,
            },
            Op::RecordValidation {
                rule,
                curvec_seen,
                verdict,
                validator,
                meta,
                if_absent,
            } => {
                let rule = self.j(*rule)?;
                for (l, r) in curvec_seen {
                    self.l(&(*l).into())?;
                    self.r((*r).into())?;
                }
                Op::RecordValidation {
                    rule,
                    curvec_seen: curvec_seen.clone(),
                    verdict: *verdict,
                    validator: validator.clone(),
                    meta: *meta,
                    if_absent: *if_absent,
                }
            }
            Op::SetMetadata {
                logical,
                tags,
                dirs,
            } => Op::SetMetadata {
                logical: self.l(logical)?,
                tags: tags.clone(),
                dirs: dirs.clone(),
            },
            Op::OpenArtifact {
                ty,
                target,
                payload_hash,
            } => Op::OpenArtifact {
                ty: *ty,
                target: self.r(*target)?,
                payload_hash: *payload_hash,
            },
            Op::ResolveArtifact { id, note } => Op::ResolveArtifact {
                id: self.a(*id)?,
                note: *note,
            },
            Op::ExpectHeads { entries, source } => Op::ExpectHeads {
                entries: entries
                    .iter()
                    .map(|(l, r)| {
                        let r = self.r(RevRef::Existing(*r))?;
                        Ok((self.l(l)?, super::txn::r(r)))
                    })
                    .collect::<Result<_, TxError>>()?,
                source: *source,
            },
            // Branch ops belong to the multi-branch kernel, never to one branch.
            Op::CreateBranch { .. }
            | Op::ArchiveBranch { .. }
            | Op::Merge { .. }
            | Op::SetAccess { .. }
            | Op::ShredContent { .. } => {
                return Err(TxError::UnknownBranch);
            }
        })
    }
}
fn r(r: RevRef) -> RevisionId {
    let RevRef::Existing(r) = r else {
        unreachable!()
    };
    r
}
fn l(l: &LogicalRef) -> LogicalId {
    let LogicalRef::Existing(l) = l else {
        unreachable!()
    };
    *l
}
fn j(j: RuleRef) -> RuleId {
    let RuleRef::Existing(j) = j else {
        unreachable!()
    };
    j
}
fn a(a: ArtifactRef) -> ArtifactId {
    let ArtifactRef::Existing(a) = a else {
        unreachable!()
    };
    a
}
fn kind(i: &Stage, r: RevisionId) -> RecordKind {
    i.logical_kind(i.revision_logical(r))
}
fn channel(i: &Stage, op: &Op, c: Channel) -> Result<(), TxError> {
    let anchor = match op {
        Op::CreateLogical { kind, .. } => *kind == RecordKind::CodeAnchor,
        Op::AddRevision { logical, .. } | Op::SetHead { logical, .. } => {
            i.logical_kind(l(logical)) == RecordKind::CodeAnchor
        }
        Op::AssertBase { rev } | Op::RetractBase { rev } => {
            kind(i, r(*rev)) == RecordKind::CodeAnchor
        }
        _ => false,
    };
    let broken = match op {
        Op::OpenArtifact { ty, target, .. } => Some((*ty, *target)),
        Op::ResolveArtifact { id, .. } => {
            let (ty, target, _) = i.artifact(a(*id));
            Some((ty, target.into()))
        }
        _ => None,
    };
    let is_broken = broken.is_some_and(|(t, _)| t == ArtifactType::AnchorBroken);
    let observer_allowed = matches!(op, Op::ObserverWatermark { .. } | Op::ExpectHeads { .. })
        || anchor
        || broken.is_some_and(|(t, p)| {
            t == ArtifactType::AnchorBroken && kind(i, r(p)) == RecordKind::CodeAnchor
        });
    if (c == Channel::Observer && !observer_allowed)
        || (matches!(op, Op::SetPolicy { .. }) && c != Channel::Admin)
        || ((matches!(op, Op::ObserverWatermark { .. }) || is_broken) && c != Channel::Observer)
    {
        return Err(TxError::ChannelRestricted {
            op: op.name().to_owned(),
            channel: c,
        });
    }
    if anchor && c != Channel::Observer {
        return Err(TxError::KindRestricted);
    }
    Ok(())
}
/// Write authorisation (ADR 0017 §6.1, §6.2), right after the op's refs
/// resolve: `Read` on every logical the op names, then the op's role in
/// the lane entry (`Write` or `Curate`). The store owner passes; outside
/// the kernel (`auth: None`) nothing is checked.
struct Authorise<'s, 'a> {
    i: &'s Stage<'a>,
    access: Option<&'a crate::access::AccessState>,
    p: PrincipalId,
    index: usize,
}
impl Authorise<'_, '_> {
    fn refuse(&self, reference: String, need: AccessNeed) -> TxError {
        TxError::Unauthorized {
            op_index: self.index,
            reference,
            need,
        }
    }
    fn reads(&self, l: LogicalId) -> bool {
        let (key, lane, owner) = self.i.logical_facts(l);
        let dirs = self.i.metadata_of(l).dirs;
        let facts = crate::access::LogicalFacts {
            key: &key,
            lane: &lane,
            owner,
            dirs: &dirs,
        };
        crate::access::authorized(self.access, self.p, &facts).allowed
    }
    fn role(&self, lane: &str, role: crate::access::Role) -> bool {
        crate::access::has_role(self.access, self.p, lane, role)
    }
    fn read(&self, l: LogicalId, reference: impl FnOnce() -> String) -> Result<(), TxError> {
        if self.reads(l) {
            Ok(())
        } else {
            Err(self.refuse(reference(), AccessNeed::Read))
        }
    }
    fn read_rev(&self, r: RevisionId, raw: RevRef) -> Result<(), TxError> {
        self.read(self.i.revision_logical(r), || rev_text(raw))
    }
    fn read_rule(&self, j: RuleId, raw: RuleRef) -> Result<(), TxError> {
        let named = std::iter::once(self.i.conclusion(j))
            .chain(self.i.premises(j).map(|p| p.premise))
            .chain(self.i.references(j).iter().copied());
        for r in named.collect::<Vec<_>>() {
            self.read(self.i.revision_logical(r), || rule_text(raw))?;
        }
        Ok(())
    }
    fn lane_role(
        &self,
        l: LogicalId,
        role: crate::access::Role,
        reference: impl FnOnce() -> String,
    ) -> Result<(), TxError> {
        let (_, lane, _) = self.i.logical_facts(l);
        if self.role(&lane, role) {
            Ok(())
        } else {
            let need = match role {
                crate::access::Role::Writer => AccessNeed::Write,
                crate::access::Role::Curator => AccessNeed::Curate,
            };
            Err(self.refuse(reference(), need))
        }
    }
    /// Before full resolution: a logical ref naming a hidden logical is
    /// `Unauthorized(Read)` whether or not the revisions the op also names
    /// exist (no enumeration).
    fn logical_first(&self, raw: &Op, ids: &Allocations) -> Result<(), TxError> {
        let refs: Vec<&LogicalRef> = match raw {
            Op::AddRevision { logical, .. }
            | Op::SetHead { logical, .. }
            | Op::SetMetadata { logical, .. } => vec![logical],
            Op::ExpectHeads { entries, .. } => entries.iter().map(|(l, _)| l).collect(),
            _ => Vec::new(),
        };
        for raw in refs {
            let found = match raw {
                LogicalRef::Existing(l) => Some(*l),
                LogicalRef::Key(k) => self.i.find_key(k),
                LogicalRef::New(n) => ids.new_logicals.get(*n as usize).copied(),
            };
            if let Some(l) = found.filter(|l| l.index() < self.i.logical_count()) {
                self.read(l, || logical_text(raw))?;
            }
        }
        Ok(())
    }
    /// After resolution: `Read` on everything named, then the role.
    fn op(&self, op: &Op, raw: &Op) -> Result<(), TxError> {
        use crate::access::Role::{Curator, Writer};
        match (op, raw) {
            (
                Op::CreateLogical {
                    key,
                    scope_lane,
                    adopt_as_base,
                    ..
                },
                _,
            ) => {
                if !self.role(scope_lane, Writer) {
                    return Err(self.refuse(key.clone(), AccessNeed::Write));
                }
                if *adopt_as_base && !self.role(scope_lane, Curator) {
                    return Err(self.refuse(key.clone(), AccessNeed::Curate));
                }
            }
            (
                Op::AddRevision {
                    logical,
                    adopt_as_base,
                    ..
                },
                Op::AddRevision { logical: raw, .. },
            ) => {
                let l = l(logical);
                self.read(l, || logical_text(raw))?;
                self.lane_role(l, Writer, || logical_text(raw))?;
                if *adopt_as_base {
                    self.lane_role(l, Curator, || logical_text(raw))?;
                }
            }
            (
                Op::SetHead {
                    logical,
                    expected,
                    new,
                },
                Op::SetHead {
                    logical: raw,
                    expected: raw_expected,
                    new: raw_new,
                },
            ) => {
                let l = l(logical);
                self.read(l, || logical_text(raw))?;
                self.read_rev(r(*expected), *raw_expected)?;
                self.read_rev(r(*new), *raw_new)?;
                self.lane_role(l, Writer, || logical_text(raw))?;
            }
            (Op::SetMetadata { logical, .. }, Op::SetMetadata { logical: raw, .. }) => {
                let l = l(logical);
                self.read(l, || logical_text(raw))?;
                self.lane_role(l, Writer, || logical_text(raw))?;
            }
            (
                Op::AssertBase { rev } | Op::RetractBase { rev },
                Op::AssertBase { rev: raw } | Op::RetractBase { rev: raw },
            ) => {
                let rv = r(*rev);
                self.read_rev(rv, *raw)?;
                self.lane_role(self.i.revision_logical(rv), Curator, || rev_text(*raw))?;
            }
            (
                Op::AddRule {
                    conclusion,
                    incidences,
                    ..
                },
                Op::AddRule {
                    conclusion: raw_c,
                    incidences: raw_i,
                    ..
                },
            ) => {
                let c = r(*conclusion);
                self.read_rev(c, *raw_c)?;
                for ((p, _), (raw_p, _)) in incidences.iter().zip(raw_i) {
                    self.read_rev(r(*p), *raw_p)?;
                }
                self.lane_role(self.i.revision_logical(c), Writer, || rev_text(*raw_c))?;
            }
            (
                Op::Rebind { rule, to, .. },
                Op::Rebind {
                    rule: raw,
                    to: raw_to,
                    ..
                },
            ) => {
                let j = j(*rule);
                self.read_rule(j, *raw)?;
                self.read_rev(r(*to), *raw_to)?;
                let c = self.i.revision_logical(self.i.conclusion(j));
                self.lane_role(c, Writer, || rule_text(*raw))?;
            }
            (Op::SetRuleEnabled { rule, .. }, Op::SetRuleEnabled { rule: raw, .. }) => {
                let j = j(*rule);
                self.read_rule(j, *raw)?;
                let c = self.i.revision_logical(self.i.conclusion(j));
                self.lane_role(c, Writer, || rule_text(*raw))?;
            }
            (Op::OpenArtifact { target, .. }, Op::OpenArtifact { target: raw, .. }) => {
                let t = r(*target);
                self.read_rev(t, *raw)?;
                self.lane_role(self.i.revision_logical(t), Writer, || rev_text(*raw))?;
            }
            (Op::ResolveArtifact { id, .. }, Op::ResolveArtifact { id: raw, .. }) => {
                let (_, target, _) = self.i.artifact(a(*id));
                let t = self.i.revision_logical(target);
                self.read(t, || artifact_text(*raw))?;
                self.lane_role(t, Writer, || artifact_text(*raw))?;
            }
            // Verdicts stay with the admin and the validators (§6.2 rule 5).
            (Op::RecordValidation { .. }, Op::RecordValidation { rule: raw, .. }) => {
                return Err(self.refuse(rule_text(*raw), AccessNeed::Curate));
            }
            (Op::ExpectHeads { entries, .. }, Op::ExpectHeads { entries: raw, .. }) => {
                for ((logical, _), (raw, _)) in entries.iter().zip(raw) {
                    self.read(l(logical), || logical_text(raw))?;
                }
            }
            // Admin- and observer-only ops are refused by their channel.
            _ => {}
        }
        Ok(())
    }
}
fn rev_text(r: RevRef) -> String {
    match r {
        RevRef::Existing(x) => format!("rev {}", x.0),
        RevRef::New(n) => format!("new rev {n}"),
    }
}
fn logical_text(l: &LogicalRef) -> String {
    match l {
        LogicalRef::Existing(x) => format!("logical {}", x.0),
        LogicalRef::Key(k) => k.clone(),
        LogicalRef::New(n) => format!("new logical {n}"),
    }
}
fn rule_text(j: RuleRef) -> String {
    match j {
        RuleRef::Existing(x) => format!("rule {}", x.0),
        RuleRef::New(n) => format!("new rule {n}"),
    }
}
fn artifact_text(a: ArtifactRef) -> String {
    match a {
        ArtifactRef::Existing(x) => format!("artifact {}", x.0),
        ArtifactRef::New(n) => format!("new artifact {n}"),
    }
}
fn id(n: usize, kind: CapacityKind) -> Result<u32, TxError> {
    u32::try_from(n).map_err(|_| TxError::CapacityExceeded { kind })
}

fn canonical(
    i: &Stage,
    conclusion: RevisionId,
    incidences: &[(RevRef, EdgeKind)],
) -> Result<(Vec<Incidence>, Vec<RevisionId>), TxError> {
    // Sort/group rather than the reference model's map-based normalization.
    let mut raw: Vec<_> = incidences.iter().map(|(p, k)| (r(*p), *k)).collect();
    raw.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    raw.dedup_by_key(|v| v.0);
    // SD6: a rule names at least one revision; reference-only rules never fire.
    if raw.is_empty() {
        return Err(TxError::InvalidRule(InvalidRule::EmptyPremises));
    }
    if raw
        .iter()
        .any(|(p, k)| *p == conclusion && *k != EdgeKind::Ref)
    {
        return Err(TxError::InvalidRule(InvalidRule::SelfPremise));
    }
    let mut seen = BTreeSet::new();
    for (p, _) in &raw {
        let logical = i.revision_logical(*p);
        if !seen.insert(logical) {
            return Err(TxError::InvalidRule(InvalidRule::ConflictingVersions(
                logical,
            )));
        }
    }
    if raw.iter().filter(|(_, k)| *k != EdgeKind::Ref).count() > 1024 {
        return Err(TxError::InvalidRule(InvalidRule::TooManyPremises));
    }
    if matches!(
        kind(i, conclusion),
        RecordKind::Evidence | RecordKind::CodeAnchor
    ) {
        return Err(TxError::InvalidRule(InvalidRule::ForbiddenConclusion));
    }
    if raw.iter().any(|(p, k)| {
        kind(i, *p) == RecordKind::Summary && !matches!(k, EdgeKind::Sem | EdgeKind::Ref)
    }) {
        return Err(TxError::InvalidRule(InvalidRule::ForbiddenPremise));
    }
    let mut premises = Vec::new();
    let mut refs = Vec::new();
    for (p, k) in raw {
        if k == EdgeKind::Ref {
            refs.push(p)
        } else {
            premises.push(Incidence {
                premise: p,
                kind: k,
            })
        }
    }
    Ok((premises, refs))
}
#[allow(clippy::too_many_arguments)]
fn add<S: StatsSink>(
    i: &mut Stage,
    ids: &mut Allocations,
    b: &Batch,
    epoch: Epoch,
    conclusion: RevisionId,
    raw: &[(RevRef, EdgeKind)],
    enabled: bool,
    op_index: usize,
    topo: &mut PearceKelly,
    stats: &mut S,
) -> Result<(), TxError> {
    let (premises, references) = canonical(i, conclusion, raw)?;
    let key = rule_key(conclusion, &premises, &references);
    if let Some(existing) = i.find_rule(&key) {
        return Err(TxError::DuplicateRule {
            existing,
            enabled: i.enabled(existing),
        });
    }
    id(i.rule_count(), CapacityKind::Rule)?;
    i.incidences = incidence_count(i.incidences, premises.len())?;
    let rule = i.push_rule(Rule {
        conclusion,
        premises,
        references,
        enabled,
        origin_branch: b.branch,
        origin_epoch: epoch,
    });
    topo.sync(i, op_index, stats)?;
    ids.new_rules.push(rule);
    Ok(())
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply<S: StatsSink>(
    i: &mut Stage,
    ids: &mut Allocations,
    b: &Batch,
    op: &Op,
    index: usize,
    epoch: Epoch,
    topo: &mut PearceKelly,
    stats: &mut S,
    blobs: &dyn BlobPresence,
) -> Result<(), TxError> {
    let original = op;
    let authorise = i.auth.map(|access| Authorise {
        i,
        access,
        p: b.principal,
        index,
    });
    let authorise = authorise.filter(|_| !b.principal.is_store_owner());
    // `personal:<name>` for an unlisted name: refused for everyone, the
    // store owner included (ADR 0017 §6.2 rule 1).
    if let (Some(Some(access)), Op::CreateLogical { scope_lane, .. }) = (i.auth, op)
        && access.unknown_personal(scope_lane)
    {
        return Err(TxError::InvalidLane {
            lane: scope_lane.clone(),
        });
    }
    if let Some(check) = &authorise {
        check.logical_first(op, ids)?;
    }
    let op = Resolver { i, a: ids }.op(op)?;
    if let Some(check) = &authorise {
        check.op(&op, original)?;
    }
    channel(i, &op, b.channel)?;
    match op {
        Op::CreateLogical {
            key,
            kind,
            scope_lane,
            content,
            adopt_as_base,
        } => {
            let mut chars = key.bytes();
            let start = chars
                .next()
                .is_some_and(|x| x.is_ascii_lowercase() || x.is_ascii_digit());
            if !start
                || key.len() > 256
                || !chars
                    .all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || b"._:/#-".contains(&x))
                || (kind == RecordKind::CodeAnchor) != key.starts_with("code:")
            {
                return Err(TxError::InvalidKey { key });
            }
            shredded(i, &content)?;
            if !blobs.contains(&content) {
                return Err(TxError::MissingBlob { content });
            }
            if i.find_key(&key).is_some() || i.key_taken_elsewhere(&key) {
                return Err(TxError::LogicalExists { key });
            }
            id(i.logical_count(), CapacityKind::Logical)?;
            let revision = RevisionId(id(i.revision_count(), CapacityKind::Revision)?);
            let logical = i.push_logical(
                Logical {
                    key,
                    kind,
                    scope_lane,
                    owner: b.principal,
                    origin_branch: b.branch,
                    origin_epoch: epoch,
                },
                revision,
            );
            i.push_revision(
                Revision {
                    logical,
                    content,
                    author: b.principal,
                    origin_branch: b.branch,
                    origin_epoch: epoch,
                },
                adopt_as_base,
            );
            ids.new_logicals.push(logical);
            ids.new_revisions.push(revision);
        }
        Op::AddRevision {
            logical,
            content,
            adopt_as_base,
        } => {
            let logical = l(&logical);
            if i.logical_kind(logical) == RecordKind::Evidence {
                return Err(TxError::ImmutableKind);
            }
            shredded(i, &content)?;
            if !blobs.contains(&content) {
                return Err(TxError::MissingBlob { content });
            }
            let revision = match i.find_revision(logical, &content) {
                Some(v) => v,
                None => {
                    id(i.revision_count(), CapacityKind::Revision)?;
                    i.push_revision(
                        Revision {
                            logical,
                            content,
                            author: b.principal,
                            origin_branch: b.branch,
                            origin_epoch: epoch,
                        },
                        false,
                    )
                }
            };
            if adopt_as_base {
                i.set_base(revision, true);
            }
            ids.new_revisions.push(revision);
        }
        Op::SetHead {
            logical,
            expected,
            new,
        } => {
            let logical = l(&logical);
            let expected = r(expected);
            let new = r(new);
            if i.logical_kind(logical) == RecordKind::Evidence {
                return Err(TxError::ImmutableKind);
            }
            if [new, expected]
                .iter()
                .any(|r| i.revision_logical(*r) != logical)
            {
                return Err(TxError::LogicalMismatch);
            }
            let actual = i.head(logical);
            if expected != actual {
                return Err(TxError::HeadConflict {
                    logical,
                    expected,
                    actual,
                });
            }
            i.set_head(logical, new);
        }
        Op::AssertBase { rev } => i.set_base(r(rev), true),
        Op::RetractBase { rev } => i.set_base(r(rev), false),
        Op::AddRule {
            conclusion,
            incidences,
            enabled,
        } => add(
            i,
            ids,
            b,
            epoch,
            r(conclusion),
            &incidences,
            enabled,
            index,
            topo,
            stats,
        )?,
        Op::SetRuleEnabled { rule, enabled } => i.set_enabled(j(rule), enabled),
        Op::Rebind { rule, from, to } => {
            let rule = j(rule);
            let from = r(from);
            let to = r(to);
            if i.revision_logical(from) != i.revision_logical(to) {
                return Err(TxError::LogicalMismatch);
            }
            let old = Rule {
                conclusion: i.conclusion(rule),
                premises: i.premises(rule).collect(),
                references: i.references(rule).to_vec(),
                enabled: i.enabled(rule),
                origin_branch: b.branch,
                origin_epoch: epoch,
            };
            if !old.premises.iter().any(|p| p.premise == from) {
                return Err(TxError::InvalidRebind {
                    reason: RebindReason::NotPremise,
                });
            }
            if from == to {
                return Err(TxError::InvalidRebind {
                    reason: RebindReason::SameRevision,
                });
            }
            let raw: Vec<_> = old
                .premises
                .iter()
                .map(|p| {
                    (
                        (if p.premise == from { to } else { p.premise }).into(),
                        p.kind,
                    )
                })
                .chain(old.references.iter().map(|p| ((*p).into(), EdgeKind::Ref)))
                .collect();
            add(
                i,
                ids,
                b,
                epoch,
                old.conclusion,
                &raw,
                old.enabled,
                index,
                topo,
                stats,
            )?;
            i.set_enabled(rule, false);
        }
        Op::RecordValidation {
            rule,
            curvec_seen,
            verdict,
            validator,
            meta,
            if_absent,
        } => {
            // §6.2 order: MissingBlob before NoSemIncidence and the CAS class.
            if let Some(content) = &meta {
                shredded(i, content)?;
            }
            if let Some(content) = meta
                && !blobs.contains(&content)
            {
                return Err(TxError::MissingBlob { content });
            }
            let rule = j(rule);
            let mut current: Vec<_> = i
                .premises(rule)
                .filter(|p| p.kind == EdgeKind::Sem)
                .map(|p| {
                    let l = i.revision_logical(p.premise);
                    (l, i.head(l))
                })
                .collect();
            current.sort_unstable();
            if current.is_empty() {
                return Err(TxError::NoSemIncidence { rule });
            }
            if current != curvec_seen {
                return Err(TxError::CurvecMoved {
                    rule,
                    seen: curvec_seen,
                    current,
                });
            }
            // ADR 0015 §3.1: right after CurvecMoved in the CAS class.
            if if_absent && let Some(old) = i.validation(rule, &current) {
                return Err(TxError::ValidationExists {
                    rule,
                    curvec: current,
                    verdict: old.verdict,
                });
            }
            i.validations.insert(
                (rule, current),
                ValidationRecord {
                    verdict,
                    validator,
                    meta,
                },
            );
        }
        Op::SetPolicy { policy } => {
            if policy.version != 1 {
                return Err(TxError::InvalidPolicy);
            }
            i.policy = Some(policy);
        }
        Op::ObserverWatermark {
            scope,
            expected,
            position,
        } => {
            let actual = i.watermark(&scope);
            if actual != expected {
                return Err(TxError::WatermarkConflict {
                    scope,
                    expected,
                    actual,
                });
            }
            if scope.len() > 256 {
                return Err(TxError::CapacityExceeded {
                    kind: CapacityKind::WatermarkScope,
                });
            }
            if position.len() > 64 {
                return Err(TxError::CapacityExceeded {
                    kind: CapacityKind::WatermarkPosition,
                });
            }
            i.watermarks.insert(scope, position);
        }
        Op::SetMetadata {
            logical,
            tags,
            dirs,
        } => {
            let logical = l(&logical);
            let before = i.metadata_of(logical).dirs;
            for path in &dirs {
                if path.is_empty()
                    || path
                        .split('/')
                        .any(|v| v.is_empty() || matches!(v, "." | "..") || v.contains('*'))
                {
                    return Err(TxError::InvalidMetadata);
                }
                if !before.is_empty()
                    && !before.iter().any(|old| {
                        path == old || path.strip_prefix(old).is_some_and(|s| s.starts_with('/'))
                    })
                {
                    return Err(TxError::InvalidMetadata);
                }
            }
            if !before.is_empty() && dirs.is_empty() {
                return Err(TxError::InvalidMetadata);
            }
            i.metadata.insert(logical, Metadata { tags, dirs });
        }
        Op::OpenArtifact {
            ty,
            target,
            payload_hash,
        } => {
            shredded(i, &payload_hash)?;
            let artifact = ArtifactId(id(i.artifact_count(), CapacityKind::Artifact)?);
            i.new_artifacts.push(Artifact {
                ty,
                target: r(target),
                payload_hash,
                open: true,
                author: b.principal,
                origin_branch: b.branch,
                origin_epoch: epoch,
            });
            ids.new_artifacts.push(artifact);
        }
        Op::ResolveArtifact { id, note } => {
            if let Some(content) = &note {
                shredded(i, content)?;
            }
            if let Some(content) = note
                && !blobs.contains(&content)
            {
                return Err(TxError::MissingBlob { content });
            }
            let id = a(id);
            if !i.artifact(id).2 {
                return Err(TxError::ArtifactNotOpen { id });
            }
            i.set_artifact_open(id, false);
        }
        Op::ExpectHeads { entries, .. } => {
            let Op::ExpectHeads { entries: raw, .. } = original else {
                unreachable!()
            };
            for ((logical, read), (as_written, _)) in entries.iter().zip(raw) {
                let logical = l(logical);
                // Only pre-batch heads can be read; a New logical has none.
                if matches!(as_written, LogicalRef::New(_))
                    || logical.index() >= i.published_logicals()
                    || i.read_set
                        .insert(logical, *read)
                        .is_some_and(|r| r != *read)
                {
                    return Err(TxError::InvalidReadSet { logical });
                }
            }
        }
        Op::CreateBranch { .. }
        | Op::ArchiveBranch { .. }
        | Op::Merge { .. }
        | Op::SetAccess { .. }
        | Op::ShredContent { .. } => {
            unreachable!("rejected by the resolver")
        }
    }

    Ok(())
}
/// `ContentShredded` for an op naming a shredded payload (ADR 0017 §8.3),
/// before `MissingBlob`.
fn shredded(i: &Stage, content: &ContentHash) -> Result<(), TxError> {
    match i.shredded {
        Some(set) if set.contains_key(content) => {
            Err(TxError::ContentShredded { content: *content })
        }
        _ => Ok(()),
    }
}
/// `ExpectHeads` at batch end, after A5 (SPEC §6.1 contract 12): every read
/// head must still be the published one.
pub(crate) fn final_read_set(i: &Stage) -> Result<(), TxError> {
    for (logical, read) in &i.read_set {
        let now = i
            .published_head(*logical)
            .expect("read logicals are published");
        if now != *read {
            return Err(TxError::ReadSetConflict {
                logical: *logical,
                read: *read,
                now,
            });
        }
    }
    Ok(())
}
/// A5 over the staged batch (ADR 0007 §5). Published revisions were valid at
/// the previous commit, so only revisions whose base became true and new
/// revisions can violate it, unless the policy itself changed (full scan).
/// Candidates are scanned in ascending RevisionId order, as the full scan is.
pub(crate) fn final_policy(i: &Stage) -> Result<(), TxError> {
    let old_revisions = i.published_revisions();
    let candidates: BTreeSet<RevisionId> = if i.policy.is_some() {
        (0..i.revision_count())
            .map(|n| RevisionId(n as u32))
            .collect()
    } else {
        i.base_candidates()
            .chain((old_revisions..i.revision_count()).map(|n| RevisionId(n as u32)))
            .collect()
    };
    let policy = i.policy();
    for r in candidates {
        let logical = i.revision_logical(r);
        let k = i.logical_kind(logical);
        if matches!(k, RecordKind::CodeAnchor | RecordKind::Evidence)
            || !policy.require_deps.contains(&k)
        {
            continue;
        }
        let reason = if i.base(r) {
            Some(PolicyReason::BaseNotAllowed)
        } else if r.index() >= old_revisions && !i.staged_rule_enabled(r) {
            Some(PolicyReason::NoEnabledRule)
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(TxError::PolicyViolation {
                kind: k,
                logical,
                reason,
            });
        }
    }
    Ok(())
}

fn incidence_count(old: u64, extra: usize) -> Result<u64, TxError> {
    let count = old + extra as u64;
    if count > u32::MAX as u64 {
        return Err(TxError::CapacityExceeded {
            kind: CapacityKind::Incidence,
        });
    }
    Ok(count)
}
#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn incidence_capacity_inclusive_limit() {
        let max = u32::MAX as u64;
        assert_eq!(incidence_count(0, 3), Ok(3));
        assert_eq!(incidence_count(max - 1, 1), Ok(max));
        assert_eq!(
            incidence_count(max, 2),
            Err(TxError::CapacityExceeded {
                kind: CapacityKind::Incidence
            })
        );
    }
}
