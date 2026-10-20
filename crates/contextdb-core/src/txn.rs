use crate::topo::NaiveTopo;
use contextdb_model::*;
use std::collections::BTreeSet;

struct Resolver<'a> {
    i: &'a Inputs,
    a: &'a Allocations,
}
impl Resolver<'_> {
    fn r(&self, r: RevRef) -> Result<RevRef, TxError> {
        let id = match r {
            RevRef::Existing(id) => id,
            RevRef::New(n) => *self
                .a
                .new_revisions
                .get(n as usize)
                .ok_or(TxError::UnknownRef)?,
        };
        if id.index() >= self.i.revisions.len() {
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
            LogicalRef::Key(k) => LogicalId(
                self.i
                    .logicals
                    .iter()
                    .position(|v| v.key == *k)
                    .ok_or(TxError::UnknownRef)? as u32,
            ),
        };
        if id.index() >= self.i.logicals.len() {
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
        if id.index() >= self.i.rules.len() {
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
        if id.index() >= self.i.artifacts.len() {
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
            Op::ResolveArtifact { id } => Op::ResolveArtifact { id: self.a(*id)? },
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
fn kind(i: &Inputs, r: RevisionId) -> RecordKind {
    i.logicals[i.revisions[r.index()].logical.index()].kind
}
fn channel(i: &Inputs, op: &Op, c: Channel) -> Result<(), TxError> {
    let anchor = match op {
        Op::CreateLogical { kind, .. } => *kind == RecordKind::CodeAnchor,
        Op::AddRevision { logical, .. } | Op::SetHead { logical, .. } => {
            i.logicals[l(logical).index()].kind == RecordKind::CodeAnchor
        }
        Op::AssertBase { rev } | Op::RetractBase { rev } => {
            kind(i, r(*rev)) == RecordKind::CodeAnchor
        }
        _ => false,
    };
    let broken = match op {
        Op::OpenArtifact { ty, target, .. } => Some((*ty, *target)),
        Op::ResolveArtifact { id } => {
            let x = &i.artifacts[a(*id).index()];
            Some((x.ty, x.target.into()))
        }
        _ => None,
    };
    let is_broken = broken.is_some_and(|(t, _)| t == ArtifactType::AnchorBroken);
    let observer_allowed = matches!(op, Op::ObserverWatermark { .. })
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
fn id(n: usize, kind: CapacityKind) -> Result<u32, TxError> {
    u32::try_from(n).map_err(|_| TxError::CapacityExceeded { kind })
}

fn canonical(
    i: &Inputs,
    conclusion: RevisionId,
    incidences: &[(RevRef, EdgeKind)],
) -> Result<(Vec<Incidence>, Vec<RevisionId>), TxError> {
    // Sort/group rather than the reference model's map-based normalization.
    let mut raw: Vec<_> = incidences.iter().map(|(p, k)| (r(*p), *k)).collect();
    raw.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    raw.dedup_by_key(|v| v.0);
    if raw.iter().all(|(_, k)| *k == EdgeKind::Ref) {
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
        let logical = i.revisions[p.index()].logical;
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
    if raw
        .iter()
        .any(|(p, k)| kind(i, *p) == RecordKind::Summary && *k != EdgeKind::Sem)
    {
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
fn add(
    i: &mut Inputs,
    ids: &mut Allocations,
    b: &Batch,
    epoch: Epoch,
    conclusion: RevisionId,
    raw: &[(RevRef, EdgeKind)],
    enabled: bool,
    op_index: usize,
) -> Result<(), TxError> {
    let (premises, references) = canonical(i, conclusion, raw)?;
    for (n, old) in i.rules.iter().enumerate() {
        if old.conclusion == conclusion && old.premises == premises && old.references == references
        {
            return Err(TxError::DuplicateRule {
                existing: RuleId(n as u32),
                enabled: old.enabled,
            });
        }
    }
    let n = id(i.rules.len(), CapacityKind::Rule)?;
    let count =
        i.rules.iter().map(|j| j.premises.len() as u64).sum::<u64>() + premises.len() as u64;
    if count > u32::MAX as u64 {
        return Err(TxError::CapacityExceeded {
            kind: CapacityKind::Incidence,
        });
    }
    i.rules.push(Rule {
        conclusion,
        premises,
        references,
        enabled,
        origin_branch: b.branch,
        origin_epoch: epoch,
    });
    NaiveTopo::build(i, op_index)?;
    ids.new_rules.push(RuleId(n));
    Ok(())
}
pub(crate) fn apply(
    i: &mut Inputs,
    ids: &mut Allocations,
    b: &Batch,
    op: &Op,
    index: usize,
    epoch: Epoch,
) -> Result<(), TxError> {
    let op = Resolver { i, a: ids }.op(op)?;
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
            if i.logicals.iter().any(|v| v.key == key) {
                return Err(TxError::LogicalExists { key });
            }
            let logical = LogicalId(id(i.logicals.len(), CapacityKind::Logical)?);
            let revision = RevisionId(id(i.revisions.len(), CapacityKind::Revision)?);
            i.logicals.push(Logical {
                key,
                kind,
                scope_lane,
                owner: b.principal,
                origin_branch: b.branch,
                origin_epoch: epoch,
            });
            i.heads.push(revision);
            i.metadata.push(Metadata::default());
            i.revisions.push(Revision {
                logical,
                content,
                author: b.principal,
                origin_branch: b.branch,
                origin_epoch: epoch,
            });
            i.base.push(adopt_as_base);
            ids.new_logicals.push(logical);
            ids.new_revisions.push(revision);
        }
        Op::AddRevision {
            logical,
            content,
            adopt_as_base,
        } => {
            let logical = l(&logical);
            if i.logicals[logical.index()].kind == RecordKind::Evidence {
                return Err(TxError::ImmutableKind);
            }
            let existing = i
                .revisions
                .iter()
                .enumerate()
                .find(|(_, v)| v.logical == logical && v.content == content)
                .map(|(n, _)| RevisionId(n as u32));
            let revision = match existing {
                Some(v) => v,
                None => {
                    let v = RevisionId(id(i.revisions.len(), CapacityKind::Revision)?);
                    i.revisions.push(Revision {
                        logical,
                        content,
                        author: b.principal,
                        origin_branch: b.branch,
                        origin_epoch: epoch,
                    });
                    i.base.push(false);
                    v
                }
            };
            i.base[revision.index()] |= adopt_as_base;
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
            if i.logicals[logical.index()].kind == RecordKind::Evidence {
                return Err(TxError::ImmutableKind);
            }
            if [new, expected]
                .iter()
                .any(|r| i.revisions[r.index()].logical != logical)
            {
                return Err(TxError::LogicalMismatch);
            }
            let actual = i.heads[logical.index()];
            if expected != actual {
                return Err(TxError::HeadConflict {
                    logical,
                    expected,
                    actual,
                });
            }
            i.heads[logical.index()] = new;
        }
        Op::AssertBase { rev } => i.base[r(rev).index()] = true,
        Op::RetractBase { rev } => i.base[r(rev).index()] = false,
        Op::AddRule {
            conclusion,
            incidences,
            enabled,
        } => add(i, ids, b, epoch, r(conclusion), &incidences, enabled, index)?,
        Op::SetRuleEnabled { rule, enabled } => i.rules[j(rule).index()].enabled = enabled,
        Op::Rebind { rule, from, to } => {
            let rule = j(rule);
            let from = r(from);
            let to = r(to);
            if i.revisions[from.index()].logical != i.revisions[to.index()].logical {
                return Err(TxError::LogicalMismatch);
            }
            let old = i.rules[rule.index()].clone();
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
            add(i, ids, b, epoch, old.conclusion, &raw, old.enabled, index)?;
            i.rules[rule.index()].enabled = false;
        }
        Op::RecordValidation {
            rule,
            curvec_seen,
            verdict,
            validator,
        } => {
            let rule = j(rule);
            let mut current: Vec<_> = i.rules[rule.index()]
                .premises
                .iter()
                .filter(|p| p.kind == EdgeKind::Sem)
                .map(|p| {
                    let l = i.revisions[p.premise.index()].logical;
                    (l, i.heads[l.index()])
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
            i.validations
                .insert((rule, current), ValidationRecord { verdict, validator });
        }
        Op::SetPolicy { policy } => {
            if policy.version != 1 {
                return Err(TxError::InvalidPolicy);
            }
            i.policy = policy;
        }
        Op::ObserverWatermark {
            scope,
            expected,
            position,
        } => {
            let actual = i.watermarks.get(&scope).cloned();
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
            let before = &i.metadata[logical.index()].dirs;
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
            i.metadata[logical.index()] = Metadata { tags, dirs };
        }
        Op::OpenArtifact {
            ty,
            target,
            payload_hash,
        } => {
            let artifact = ArtifactId(id(i.artifacts.len(), CapacityKind::Artifact)?);
            i.artifacts.push(Artifact {
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
        Op::ResolveArtifact { id } => {
            let id = a(id);
            if !i.artifacts[id.index()].open {
                return Err(TxError::ArtifactNotOpen { id });
            }
            i.artifacts[id.index()].open = false;
        }
    }

    Ok(())
}
pub(crate) fn final_policy(i: &Inputs, old_revisions: usize) -> Result<(), TxError> {
    for (n, r) in i.revisions.iter().enumerate() {
        let k = i.logicals[r.logical.index()].kind;
        if matches!(k, RecordKind::CodeAnchor | RecordKind::Evidence)
            || !i.policy.require_deps.contains(&k)
        {
            continue;
        }
        let reason = if i.base[n] {
            Some(PolicyReason::BaseNotAllowed)
        } else if n >= old_revisions
            && i.rules
                .iter()
                .all(|j| j.conclusion.index() != n || !j.enabled)
        {
            Some(PolicyReason::NoEnabledRule)
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(TxError::PolicyViolation {
                kind: k,
                logical: r.logical,
                reason,
            });
        }
    }
    Ok(())
}
