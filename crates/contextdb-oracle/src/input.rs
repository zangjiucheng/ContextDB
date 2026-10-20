use contextdb_model::*;
use std::collections::{BTreeMap, BTreeSet};

/// Independent clone-and-stage reference input model. Never calls the core front-end.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputModel {
    inputs: Inputs,
    epoch: Epoch,
}
impl InputModel {
    pub fn inputs(&self) -> &Inputs {
        &self.inputs
    }
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
    pub fn at_epoch(epoch: Epoch) -> Self {
        Self {
            epoch,
            ..Self::default()
        }
    }
    pub fn apply(&mut self, batch: &Batch) -> Result<ApplyResult, TxError> {
        self.apply_with(batch, &AllPresent)
    }
    /// Same contract as core `commit_with`: `MissingBlob` at its §6.2 per-Op slot.
    pub fn apply_with(
        &mut self,
        batch: &Batch,
        blobs: &dyn BlobPresence,
    ) -> Result<ApplyResult, TxError> {
        if batch.branch != MAIN {
            return Err(TxError::UnknownRef);
        }
        let mut staged = self.inputs.clone();
        let mut ids = Allocations::default();
        let next = Epoch(self.epoch.0.saturating_add(1));
        let old_revisions = staged.revisions.len();
        for (op_index, op) in batch.ops.iter().enumerate() {
            apply_op(&mut staged, &mut ids, batch, op, op_index, next, blobs)?;
        }
        for (r, revision) in staged.revisions.iter().enumerate() {
            let logical = &staged.logicals[revision.logical.index()];
            if matches!(logical.kind, RecordKind::Evidence | RecordKind::CodeAnchor)
                || !staged.policy.require_deps.contains(&logical.kind)
            {
                continue;
            }
            let reason = if staged.base[r] {
                Some(PolicyReason::BaseNotAllowed)
            } else if r >= old_revisions
                && !staged
                    .rules
                    .iter()
                    .any(|j| j.conclusion.index() == r && j.enabled)
            {
                Some(PolicyReason::NoEnabledRule)
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(TxError::PolicyViolation {
                    kind: logical.kind,
                    logical: revision.logical,
                    reason,
                });
            }
        }
        let epoch = Epoch(
            self.epoch
                .0
                .checked_add(1)
                .ok_or(TxError::CapacityExceeded {
                    kind: CapacityKind::Epoch,
                })?,
        );
        self.inputs = staged;
        self.epoch = epoch;
        Ok(ApplyResult {
            epoch,
            allocations: ids,
        })
    }
}
fn rev(i: &Inputs, a: &Allocations, r: RevRef) -> Result<RevisionId, TxError> {
    let id = match r {
        RevRef::Existing(r) => r,
        RevRef::New(k) => *a.new_revisions.get(k as usize).ok_or(TxError::UnknownRef)?,
    };
    i.revisions.get(id.index()).ok_or(TxError::UnknownRef)?;
    Ok(id)
}
fn logical(i: &Inputs, a: &Allocations, l: &LogicalRef) -> Result<LogicalId, TxError> {
    let id = match l {
        LogicalRef::Existing(l) => *l,
        LogicalRef::Key(key) => LogicalId(
            i.logicals
                .iter()
                .position(|l| &l.key == key)
                .ok_or(TxError::UnknownRef)? as u32,
        ),
        LogicalRef::New(k) => *a.new_logicals.get(*k as usize).ok_or(TxError::UnknownRef)?,
    };
    i.logicals.get(id.index()).ok_or(TxError::UnknownRef)?;
    Ok(id)
}
fn rule(i: &Inputs, a: &Allocations, r: RuleRef) -> Result<RuleId, TxError> {
    let id = match r {
        RuleRef::Existing(r) => r,
        RuleRef::New(k) => *a.new_rules.get(k as usize).ok_or(TxError::UnknownRef)?,
    };
    i.rules.get(id.index()).ok_or(TxError::UnknownRef)?;
    Ok(id)
}
fn artifact(i: &Inputs, a: &Allocations, r: ArtifactRef) -> Result<ArtifactId, TxError> {
    let id = match r {
        ArtifactRef::Existing(r) => r,
        ArtifactRef::New(k) => *a.new_artifacts.get(k as usize).ok_or(TxError::UnknownRef)?,
    };
    i.artifacts.get(id.index()).ok_or(TxError::UnknownRef)?;
    Ok(id)
}
fn resolve(i: &Inputs, a: &Allocations, op: &Op) -> Result<Op, TxError> {
    let rr = |r| rev(i, a, r).map(RevRef::Existing);
    let lr = |l: &LogicalRef| logical(i, a, l).map(LogicalRef::Existing);
    let jr = |j| rule(i, a, j).map(RuleRef::Existing);
    Ok(match op {
        Op::CreateLogical { .. } | Op::SetPolicy { .. } | Op::ObserverWatermark { .. } => {
            op.clone()
        }
        Op::AddRevision {
            logical,
            content,
            adopt_as_base,
        } => Op::AddRevision {
            logical: lr(logical)?,
            content: *content,
            adopt_as_base: *adopt_as_base,
        },
        Op::SetHead {
            logical,
            expected,
            new,
        } => Op::SetHead {
            logical: lr(logical)?,
            expected: rr(*expected)?,
            new: rr(*new)?,
        },
        Op::AssertBase { rev } => Op::AssertBase { rev: rr(*rev)? },
        Op::RetractBase { rev } => Op::RetractBase { rev: rr(*rev)? },
        Op::AddRule {
            conclusion,
            incidences,
            enabled,
        } => Op::AddRule {
            conclusion: rr(*conclusion)?,
            incidences: incidences
                .iter()
                .map(|(r, k)| Ok((rr(*r)?, *k)))
                .collect::<Result<_, TxError>>()?,
            enabled: *enabled,
        },
        Op::SetRuleEnabled { rule, enabled } => Op::SetRuleEnabled {
            rule: jr(*rule)?,
            enabled: *enabled,
        },
        Op::Rebind { rule, from, to } => Op::Rebind {
            rule: jr(*rule)?,
            from: rr(*from)?,
            to: rr(*to)?,
        },
        Op::RecordValidation {
            rule,
            curvec_seen,
            verdict,
            validator,
        } => {
            for (l, r) in curvec_seen {
                lr(&LogicalRef::Existing(*l))?;
                rr(RevRef::Existing(*r))?;
            }
            Op::RecordValidation {
                rule: jr(*rule)?,
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
            logical: lr(logical)?,
            tags: tags.clone(),
            dirs: dirs.clone(),
        },
        Op::OpenArtifact {
            ty,
            target,
            payload_hash,
        } => Op::OpenArtifact {
            ty: *ty,
            target: rr(*target)?,
            payload_hash: *payload_hash,
        },
        Op::ResolveArtifact { id } => Op::ResolveArtifact {
            id: ArtifactRef::Existing(artifact(i, a, *id)?),
        },
    })
}
fn rid(r: RevRef) -> RevisionId {
    match r {
        RevRef::Existing(id) => id,
        _ => unreachable!("resolved"),
    }
}
fn lid(l: &LogicalRef) -> LogicalId {
    match l {
        LogicalRef::Existing(id) => *id,
        _ => unreachable!("resolved"),
    }
}
fn jid(j: RuleRef) -> RuleId {
    match j {
        RuleRef::Existing(id) => id,
        _ => unreachable!("resolved"),
    }
}
fn aid(a: ArtifactRef) -> ArtifactId {
    match a {
        ArtifactRef::Existing(id) => id,
        _ => unreachable!("resolved"),
    }
}
fn kind(i: &Inputs, r: RevisionId) -> RecordKind {
    i.logicals[i.revisions[r.index()].logical.index()].kind
}
fn anchor_write(i: &Inputs, op: &Op) -> bool {
    match op {
        Op::CreateLogical { kind, .. } => *kind == RecordKind::CodeAnchor,
        Op::AddRevision { logical, .. } | Op::SetHead { logical, .. } => {
            i.logicals[lid(logical).index()].kind == RecordKind::CodeAnchor
        }
        Op::AssertBase { rev } | Op::RetractBase { rev } => {
            kind(i, rid(*rev)) == RecordKind::CodeAnchor
        }
        _ => false,
    }
}
fn check_channel(i: &Inputs, c: Channel, op: &Op) -> Result<(), TxError> {
    let anchor_art = match op {
        Op::OpenArtifact {
            ty: ArtifactType::AnchorBroken,
            target,
            ..
        } => kind(i, rid(*target)) == RecordKind::CodeAnchor,
        Op::ResolveArtifact { id } => {
            let a = &i.artifacts[aid(*id).index()];
            a.ty == ArtifactType::AnchorBroken && kind(i, a.target) == RecordKind::CodeAnchor
        }
        _ => false,
    };
    let broken = match op {
        Op::OpenArtifact { ty, .. } => *ty == ArtifactType::AnchorBroken,
        Op::ResolveArtifact { id } => {
            i.artifacts[aid(*id).index()].ty == ArtifactType::AnchorBroken
        }
        _ => false,
    };
    let violation = (matches!(op, Op::SetPolicy { .. }) && c != Channel::Admin)
        || ((matches!(op, Op::ObserverWatermark { .. }) || broken) && c != Channel::Observer)
        || (c == Channel::Observer
            && !matches!(op, Op::ObserverWatermark { .. })
            && !anchor_write(i, op)
            && !anchor_art);
    if violation {
        return Err(TxError::ChannelRestricted {
            op: op.name().into(),
            channel: c,
        });
    }
    if anchor_write(i, op) && c != Channel::Observer {
        return Err(TxError::KindRestricted);
    }
    Ok(())
}
fn checked_id(len: usize, kind: CapacityKind) -> Result<u32, TxError> {
    u32::try_from(len).map_err(|_| TxError::CapacityExceeded { kind })
}
pub fn asserted_vector(i: &Inputs, j: &Rule) -> HeadVector {
    j.premises
        .iter()
        .filter(|p| p.kind == EdgeKind::Sem)
        .map(|p| (i.revisions[p.premise.index()].logical, p.premise))
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect()
}
pub fn current_vector(i: &Inputs, j: &Rule) -> HeadVector {
    asserted_vector(i, j)
        .into_iter()
        .map(|(l, _)| (l, i.heads[l.index()]))
        .collect()
}
pub fn semantic(i: &Inputs, id: RuleId) -> SemState {
    let j = &i.rules[id.index()];
    let asserted = asserted_vector(i, j);
    if asserted.is_empty() {
        SemState::NotRequired
    } else {
        let current = current_vector(i, j);
        if asserted == current {
            SemState::Verified
        } else {
            match i.validations.get(&(id, current)).map(|v| v.verdict) {
                Some(Verdict::Verified) => SemState::Verified,
                Some(Verdict::Rejected) => SemState::Rejected,
                _ => SemState::Pending,
            }
        }
    }
}
fn canonical(
    i: &Inputs,
    conclusion: RevisionId,
    raw: &[(RevRef, EdgeKind)],
) -> Result<(Vec<Incidence>, Vec<RevisionId>), TxError> {
    let mut map = BTreeMap::<RevisionId, EdgeKind>::new();
    for (r, k) in raw {
        map.entry(rid(*r))
            .and_modify(|old| *old = (*old).max(*k))
            .or_insert(*k);
    }
    if !map.values().any(|k| *k != EdgeKind::Ref) {
        return Err(TxError::InvalidRule(InvalidRule::EmptyPremises));
    }
    if map
        .iter()
        .any(|(r, k)| *r == conclusion && *k != EdgeKind::Ref)
    {
        return Err(TxError::InvalidRule(InvalidRule::SelfPremise));
    }
    let mut logicals = BTreeMap::new();
    for r in map.keys() {
        let l = i.revisions[r.index()].logical;
        if let Some(old) = logicals.insert(l, *r)
            && old != *r
        {
            return Err(TxError::InvalidRule(InvalidRule::ConflictingVersions(l)));
        }
    }
    if map.values().filter(|k| **k != EdgeKind::Ref).count() > 1024 {
        return Err(TxError::InvalidRule(InvalidRule::TooManyPremises));
    }
    if matches!(
        kind(i, conclusion),
        RecordKind::Evidence | RecordKind::CodeAnchor
    ) {
        return Err(TxError::InvalidRule(InvalidRule::ForbiddenConclusion));
    }
    if map
        .iter()
        .any(|(r, k)| kind(i, *r) == RecordKind::Summary && *k != EdgeKind::Sem)
    {
        return Err(TxError::InvalidRule(InvalidRule::ForbiddenPremise));
    }
    let (mut premises, mut references) = (Vec::new(), Vec::new());
    for (premise, k) in map {
        if k == EdgeKind::Ref {
            references.push(premise)
        } else {
            premises.push(Incidence { premise, kind: k })
        }
    }
    Ok((premises, references))
}
#[allow(clippy::too_many_arguments)]
fn insert_rule(
    i: &mut Inputs,
    a: &mut Allocations,
    b: &Batch,
    conclusion: RevisionId,
    raw: &[(RevRef, EdgeKind)],
    enabled: bool,
    op_index: usize,
    epoch: Epoch,
) -> Result<(), TxError> {
    let (premises, references) = canonical(i, conclusion, raw)?;
    if let Some((n, old)) = i.rules.iter().enumerate().find(|(_, j)| {
        j.conclusion == conclusion && j.premises == premises && j.references == references
    }) {
        return Err(TxError::DuplicateRule {
            existing: RuleId(n as u32),
            enabled: old.enabled,
        });
    }
    let id = RuleId(checked_id(i.rules.len(), CapacityKind::Rule)?);
    if i.rules.iter().map(|j| j.premises.len() as u64).sum::<u64>() + premises.len() as u64
        > u32::MAX as u64
    {
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
    if let Some(path) = cycle_path(i) {
        return Err(TxError::CycleDetected { op_index, path });
    }
    a.new_rules.push(id);
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn apply_op(
    i: &mut Inputs,
    a: &mut Allocations,
    b: &Batch,
    op: &Op,
    op_index: usize,
    epoch: Epoch,
    blobs: &dyn BlobPresence,
) -> Result<(), TxError> {
    let op = resolve(i, a, op)?;
    check_channel(i, b.channel, &op)?;
    match op {
        Op::CreateLogical {
            key,
            kind,
            scope_lane,
            content,
            adopt_as_base,
        } => {
            let valid = !key.is_empty()
                && key.len() <= 256
                && key
                    .bytes()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && key.bytes().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || b"._:/#-".contains(&c)
                })
                && (key.starts_with("code:") == (kind == RecordKind::CodeAnchor));
            if !valid {
                return Err(TxError::InvalidKey { key });
            }
            if !blobs.contains(&content) {
                return Err(TxError::MissingBlob { content });
            }
            if i.logicals.iter().any(|l| l.key == key) {
                return Err(TxError::LogicalExists { key });
            }
            let l = LogicalId(checked_id(i.logicals.len(), CapacityKind::Logical)?);
            let r = RevisionId(checked_id(i.revisions.len(), CapacityKind::Revision)?);
            i.logicals.push(Logical {
                key,
                kind,
                scope_lane,
                owner: b.principal,
                origin_branch: b.branch,
                origin_epoch: epoch,
            });
            i.revisions.push(Revision {
                logical: l,
                content,
                author: b.principal,
                origin_branch: b.branch,
                origin_epoch: epoch,
            });
            i.heads.push(r);
            i.base.push(adopt_as_base);
            i.metadata.push(Metadata::default());
            a.new_logicals.push(l);
            a.new_revisions.push(r);
        }
        Op::AddRevision {
            logical,
            content,
            adopt_as_base,
        } => {
            let l = lid(&logical);
            if i.logicals[l.index()].kind == RecordKind::Evidence {
                return Err(TxError::ImmutableKind);
            }
            if !blobs.contains(&content) {
                return Err(TxError::MissingBlob { content });
            }
            let r = if let Some(n) = i
                .revisions
                .iter()
                .position(|r| r.logical == l && r.content == content)
            {
                RevisionId(n as u32)
            } else {
                let r = RevisionId(checked_id(i.revisions.len(), CapacityKind::Revision)?);
                i.revisions.push(Revision {
                    logical: l,
                    content,
                    author: b.principal,
                    origin_branch: b.branch,
                    origin_epoch: epoch,
                });
                i.base.push(false);
                r
            };
            if adopt_as_base {
                i.base[r.index()] = true
            }
            a.new_revisions.push(r);
        }
        Op::SetHead {
            logical,
            expected,
            new,
        } => {
            let l = lid(&logical);
            let expected = rid(expected);
            let new = rid(new);
            if i.logicals[l.index()].kind == RecordKind::Evidence {
                return Err(TxError::ImmutableKind);
            }
            if i.revisions[new.index()].logical != l || i.revisions[expected.index()].logical != l {
                return Err(TxError::LogicalMismatch);
            }
            let actual = i.heads[l.index()];
            if actual != expected {
                return Err(TxError::HeadConflict {
                    logical: l,
                    expected,
                    actual,
                });
            }
            i.heads[l.index()] = new;
        }
        Op::AssertBase { rev } => i.base[rid(rev).index()] = true,
        Op::RetractBase { rev } => i.base[rid(rev).index()] = false,
        Op::AddRule {
            conclusion,
            incidences,
            enabled,
        } => insert_rule(
            i,
            a,
            b,
            rid(conclusion),
            &incidences,
            enabled,
            op_index,
            epoch,
        )?,
        Op::SetRuleEnabled { rule, enabled } => i.rules[jid(rule).index()].enabled = enabled,
        Op::Rebind { rule, from, to } => {
            let id = jid(rule);
            let from = rid(from);
            let to = rid(to);
            if i.revisions[from.index()].logical != i.revisions[to.index()].logical {
                return Err(TxError::LogicalMismatch);
            }
            let old = i.rules[id.index()].clone();
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
                        RevRef::Existing(if p.premise == from { to } else { p.premise }),
                        p.kind,
                    )
                })
                .chain(
                    old.references
                        .iter()
                        .map(|r| (RevRef::Existing(*r), EdgeKind::Ref)),
                )
                .collect();
            insert_rule(i, a, b, old.conclusion, &raw, old.enabled, op_index, epoch)?;
            i.rules[id.index()].enabled = false;
        }
        Op::RecordValidation {
            rule,
            curvec_seen,
            verdict,
            validator,
        } => {
            let id = jid(rule);
            let j = &i.rules[id.index()];
            if asserted_vector(i, j).is_empty() {
                return Err(TxError::NoSemIncidence { rule: id });
            }
            let current = current_vector(i, j);
            if current != curvec_seen {
                return Err(TxError::CurvecMoved {
                    rule: id,
                    seen: curvec_seen,
                    current,
                });
            }
            i.validations
                .insert((id, current), ValidationRecord { verdict, validator });
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
            if expected != actual {
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
            let l = lid(&logical);
            let old = &i.metadata[l.index()].dirs;
            let valid = dirs.iter().all(|d| {
                !d.is_empty()
                    && !d.starts_with('/')
                    && d.split('/')
                        .all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains('*'))
            }) && (old.is_empty()
                || (!dirs.is_empty()
                    && dirs.iter().all(|d| {
                        old.iter()
                            .any(|o| d == o || d.starts_with(&format!("{o}/")))
                    })));
            if !valid {
                return Err(TxError::InvalidMetadata);
            }
            i.metadata[l.index()] = Metadata { tags, dirs };
        }
        Op::OpenArtifact {
            ty,
            target,
            payload_hash,
        } => {
            let id = ArtifactId(checked_id(i.artifacts.len(), CapacityKind::Artifact)?);
            i.artifacts.push(Artifact {
                ty,
                target: rid(target),
                payload_hash,
                open: true,
                author: b.principal,
                origin_branch: b.branch,
                origin_epoch: epoch,
            });
            a.new_artifacts.push(id);
        }
        Op::ResolveArtifact { id } => {
            let id = aid(id);
            if !i.artifacts[id.index()].open {
                return Err(TxError::ArtifactNotOpen { id });
            }
            i.artifacts[id.index()].open = false;
        }
    }
    Ok(())
}

pub fn cycle_graph(i: &Inputs) -> BTreeMap<CycleNode, BTreeSet<CycleNode>> {
    let mut graph = BTreeMap::<_, BTreeSet<_>>::new();
    for r in 0..i.revisions.len() {
        graph
            .entry(CycleNode::Revision(RevisionId(r as u32)))
            .or_default();
    }
    let mut sem_logicals = BTreeSet::new();
    for j in &i.rules {
        for p in &j.premises {
            graph
                .entry(CycleNode::Revision(p.premise))
                .or_default()
                .insert(CycleNode::Revision(j.conclusion));
            if p.kind == EdgeKind::Sem {
                let l = i.revisions[p.premise.index()].logical;
                sem_logicals.insert(l);
                graph
                    .entry(CycleNode::Logical(l))
                    .or_default()
                    .insert(CycleNode::Revision(j.conclusion));
            }
        }
    }
    for (r, revision) in i.revisions.iter().enumerate() {
        if sem_logicals.contains(&revision.logical) {
            graph
                .entry(CycleNode::Revision(RevisionId(r as u32)))
                .or_default()
                .insert(CycleNode::Logical(revision.logical));
        }
    }
    graph
}
pub fn cycle_path(i: &Inputs) -> Option<Vec<CycleNode>> {
    fn visit(
        n: &CycleNode,
        g: &BTreeMap<CycleNode, BTreeSet<CycleNode>>,
        marks: &mut BTreeMap<CycleNode, u8>,
        stack: &mut Vec<CycleNode>,
    ) -> Option<Vec<CycleNode>> {
        match marks.get(n) {
            Some(1) => {
                let at = stack.iter().position(|x| x == n)?;
                let mut path = stack[at..].to_vec();
                path.push(n.clone());
                return Some(path);
            }
            Some(2) => return None,
            _ => {}
        }
        marks.insert(n.clone(), 1);
        stack.push(n.clone());
        if let Some(next) = g.get(n) {
            for v in next {
                if let Some(path) = visit(v, g, marks, stack) {
                    return Some(path);
                }
            }
        }
        stack.pop();
        marks.insert(n.clone(), 2);
        None
    }
    let g = cycle_graph(i);
    let mut marks = BTreeMap::new();
    for n in g.keys() {
        if let Some(path) = visit(n, &g, &mut marks, &mut Vec::new()) {
            return Some(path);
        }
    }
    None
}
