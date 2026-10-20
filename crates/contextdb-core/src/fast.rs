//! Mutable-table transactions: ordered staging with a write-set undo log.
use crate::{
    InputView,
    arena::{RevisionArena, RuleArena},
    view::StoredView,
};
use contextdb_model::*;
use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub(crate) struct Undo {
    base: BTreeMap<RevisionId, bool>,
    heads: BTreeMap<LogicalId, RevisionId>,
    enabled: BTreeMap<RuleId, bool>,
}
pub(crate) fn eligible(b: &Batch) -> bool {
    b.ops.iter().all(|op| {
        matches!(
            op,
            Op::AssertBase { .. }
                | Op::RetractBase { .. }
                | Op::SetHead { .. }
                | Op::SetRuleEnabled { .. }
        )
    })
}
fn r(r: RevRef, revisions: &RevisionArena) -> Result<RevisionId, TxError> {
    let RevRef::Existing(r) = r else {
        return Err(TxError::UnknownRef);
    };
    if r.index() >= revisions.len() {
        return Err(TxError::UnknownRef);
    }
    Ok(r)
}
fn l(l: &LogicalRef, i: &Inputs) -> Result<LogicalId, TxError> {
    let id = match l {
        LogicalRef::Existing(l) => *l,
        LogicalRef::New(_) => return Err(TxError::UnknownRef),
        LogicalRef::Key(k) => LogicalId(
            i.logicals
                .iter()
                .position(|l| l.key == *k)
                .ok_or(TxError::UnknownRef)? as u32,
        ),
    };
    if id.index() >= i.logicals.len() {
        return Err(TxError::UnknownRef);
    }
    Ok(id)
}
fn channel(b: &Batch, op: &Op, anchor: bool) -> Result<(), TxError> {
    if b.channel == Channel::Observer && !anchor {
        return Err(TxError::ChannelRestricted {
            op: op.name().into(),
            channel: b.channel,
        });
    }
    if b.channel != Channel::Observer && anchor {
        return Err(TxError::KindRestricted);
    }
    Ok(())
}
impl Undo {
    pub fn restore(&self, i: &mut Inputs, rules: &mut RuleArena) {
        for (r, b) in &self.base {
            i.base[r.index()] = *b;
        }
        for (l, h) in &self.heads {
            i.heads[l.index()] = *h;
        }
        for (j, b) in &self.enabled {
            rules.enabled[j.index()] = *b;
        }
    }
    pub fn delta(&self, i: &Inputs, rules: &RuleArena) -> NetDelta {
        NetDelta {
            base_flips: self
                .base
                .iter()
                .filter(|(r, b)| i.base[r.index()] != **b)
                .map(|(r, _)| *r)
                .collect(),
            head_moves: self
                .heads
                .iter()
                .filter(|(l, h)| i.heads[l.index()] != **h)
                .map(|(l, h)| (*l, Some(*h), i.heads[l.index()]))
                .collect(),
            enable_flips: self
                .enabled
                .iter()
                .filter(|(j, b)| rules.enabled[j.index()] != **b)
                .map(|(j, _)| *j)
                .collect(),
            new_rules: vec![],
        }
    }
    pub fn stage(
        i: &mut Inputs,
        revisions: &RevisionArena,
        rules: &mut RuleArena,
        b: &Batch,
        epoch: Epoch,
    ) -> Result<Self, TxError> {
        let mut undo = Self::default();
        let result = (|| {
            for op in &b.ops {
                match op {
                    Op::AssertBase { rev } | Op::RetractBase { rev } => {
                        let r = r(*rev, revisions)?;
                        let kind = i.logicals[revisions.logical(r).index()].kind;
                        channel(b, op, kind == RecordKind::CodeAnchor)?;
                        undo.base.entry(r).or_insert(i.base[r.index()]);
                        i.base[r.index()] = matches!(op, Op::AssertBase { .. });
                    }
                    Op::SetHead {
                        logical,
                        expected,
                        new,
                    } => {
                        let logical = l(logical, i)?;
                        let expected = r(*expected, revisions)?;
                        let new = r(*new, revisions)?;
                        let kind = i.logicals[logical.index()].kind;
                        channel(b, op, kind == RecordKind::CodeAnchor)?;
                        if kind == RecordKind::Evidence {
                            return Err(TxError::ImmutableKind);
                        }
                        if revisions.logical(expected) != logical
                            || revisions.logical(new) != logical
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
                        undo.heads.entry(logical).or_insert(actual);
                        i.heads[logical.index()] = new;
                    }
                    Op::SetRuleEnabled { rule, enabled } => {
                        let RuleRef::Existing(j) = rule else {
                            return Err(TxError::UnknownRef);
                        };
                        if j.index() >= rules.len() {
                            return Err(TxError::UnknownRef);
                        }
                        channel(b, op, false)?;
                        undo.enabled.entry(*j).or_insert(rules.enabled[j.index()]);
                        rules.enabled[j.index()] = *enabled;
                    }
                    _ => unreachable!("eligible batch"),
                }
            }
            for r in undo.base.keys() {
                let logical = revisions.logical(*r);
                let kind = i.logicals[logical.index()].kind;
                if i.base[r.index()]
                    && i.policy.require_deps.contains(&kind)
                    && !matches!(kind, RecordKind::Evidence | RecordKind::CodeAnchor)
                {
                    return Err(TxError::PolicyViolation {
                        kind,
                        logical,
                        reason: PolicyReason::BaseNotAllowed,
                    });
                }
            }
            epoch.0.checked_add(1).ok_or(TxError::CapacityExceeded {
                kind: CapacityKind::Epoch,
            })?;
            Ok(())
        })();
        if let Err(error) = result {
            undo.restore(i, rules);
            return Err(error);
        }
        Ok(undo)
    }
}
pub(crate) struct Before<'a> {
    pub view: StoredView<'a>,
    pub undo: &'a Undo,
}
/// Direct status seeds. The diff walker follows dependencies only while a
/// premise's layer truth or inherited stale reason changes.
pub(crate) fn status_seeds(
    i: &impl InputView,
    index: &crate::Indexes,
    delta: &NetDelta,
) -> std::collections::BTreeSet<RevisionId> {
    let mut seen = std::collections::BTreeSet::new();
    for r in &delta.base_flips {
        seen.insert(*r);
    }
    for j in &delta.enable_flips {
        seen.insert(i.conclusion(*j));
    }
    for (l, old, new) in &delta.head_moves {
        for r in old.iter().chain(std::iter::once(new)) {
            seen.insert(*r);
            for id in index.exact_by_rev[r.index()]
                .iter()
                .chain(&index.pin_by_rev[r.index()])
            {
                seen.insert(i.conclusion(index.inc_rule[id.index()]));
            }
        }
        for id in &index.sem_by_logical[l.index()] {
            seen.insert(i.conclusion(index.inc_rule[id.index()]));
        }
    }
    seen
}
impl InputView for Before<'_> {
    fn revision_count(&self) -> usize {
        self.view.revision_count()
    }
    fn rule_count(&self) -> usize {
        self.view.rule_count()
    }
    fn logical(&self, id: LogicalId) -> &Logical {
        self.view.logical(id)
    }
    fn revision_logical(&self, id: RevisionId) -> LogicalId {
        self.view.revision_logical(id)
    }
    fn head(&self, id: LogicalId) -> RevisionId {
        self.undo
            .heads
            .get(&id)
            .copied()
            .unwrap_or_else(|| self.view.head(id))
    }
    fn base(&self, id: RevisionId) -> bool {
        self.undo
            .base
            .get(&id)
            .copied()
            .unwrap_or_else(|| self.view.base(id))
    }
    fn enabled(&self, id: RuleId) -> bool {
        self.undo
            .enabled
            .get(&id)
            .copied()
            .unwrap_or_else(|| self.view.enabled(id))
    }
    fn conclusion(&self, id: RuleId) -> RevisionId {
        self.view.conclusion(id)
    }
    fn premises(&self, id: RuleId) -> impl Iterator<Item = Incidence> {
        self.view.premises(id)
    }
    fn references(&self, id: RuleId) -> &[RevisionId] {
        self.view.references(id)
    }
    fn validation(&self, id: RuleId, v: &HeadVector) -> Option<&ValidationRecord> {
        self.view.validation(id, v)
    }
}
