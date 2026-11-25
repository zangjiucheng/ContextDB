//! Mutable-table transactions: ordered staging with a write-set undo log.
use crate::{
    InputView,
    arena::{LogicalArena, RevisionArena, RuleArena},
    view::StoredView,
};
use contextdb_model::*;
use std::collections::BTreeMap;

/// Pre-batch values of overwritten published entries; the `Before` view reads
/// through them. Structural commits also record replaced validation records.
#[derive(Clone, Debug, Default)]
pub(crate) struct Undo {
    pub base: BTreeMap<RevisionId, bool>,
    pub heads: BTreeMap<LogicalId, RevisionId>,
    pub enabled: BTreeMap<RuleId, bool>,
    pub validations: BTreeMap<(RuleId, HeadVector), Option<ValidationRecord>>,
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
fn l(l: &LogicalRef, logicals: &LogicalArena) -> Result<LogicalId, TxError> {
    let id = match l {
        LogicalRef::Existing(l) => *l,
        LogicalRef::New(_) => return Err(TxError::UnknownRef),
        LogicalRef::Key(k) => logicals.find_key(k).ok_or(TxError::UnknownRef)?,
    };
    if id.index() >= logicals.len() {
        return Err(TxError::UnknownRef);
    }
    Ok(id)
}
/// The fast path's slice of `txn::channel` (same order): `mirror` is
/// whether the op writes replica-origin state, `anchor` CodeAnchor state.
fn channel(b: &Batch, op: &Op, anchor: bool, mirror: bool) -> Result<(), TxError> {
    let c = b.channel;
    if (c == Channel::Observer && !anchor) || (c == Channel::Replica && !mirror) {
        return Err(TxError::ChannelRestricted {
            op: op.name().into(),
            channel: c,
        });
    }
    if mirror && c != Channel::Replica {
        return Err(TxError::ReplicaReadOnly);
    }
    if anchor && c != Channel::Observer {
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
        logicals: &LogicalArena,
        revisions: &RevisionArena,
        rules: &mut RuleArena,
        b: &Batch,
        blobs: &dyn BlobPresence,
        shredded: Option<&BTreeMap<ContentHash, Epoch>>,
    ) -> Result<Self, TxError> {
        let mut undo = Self::default();
        let result = (|| {
            for op in &b.ops {
                match op {
                    Op::AssertBase { rev } | Op::RetractBase { rev } => {
                        let r = r(*rev, revisions)?;
                        let logical = revisions.logical(r);
                        let kind = logicals.kind(logical);
                        let mirror = logicals.origin_of(logical) != Origin::Local;
                        channel(b, op, kind == RecordKind::CodeAnchor, mirror)?;
                        if matches!(op, Op::AssertBase { .. }) {
                            crate::txn::live_again(b, blobs, shredded, revisions.content(r))?;
                        }
                        undo.base.entry(r).or_insert(i.base[r.index()]);
                        i.base[r.index()] = matches!(op, Op::AssertBase { .. });
                    }
                    Op::SetHead {
                        logical,
                        expected,
                        new,
                    } => {
                        let logical = l(logical, logicals)?;
                        let expected = r(*expected, revisions)?;
                        let new = r(*new, revisions)?;
                        let kind = logicals.kind(logical);
                        let mirror = logicals.origin_of(logical) != Origin::Local;
                        channel(b, op, kind == RecordKind::CodeAnchor, mirror)?;
                        if kind == RecordKind::Evidence {
                            return Err(TxError::ImmutableKind);
                        }
                        crate::txn::live_again(b, blobs, shredded, revisions.content(new))?;
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
                        // A rule never concludes a mirror (A8), so this is
                        // local state.
                        channel(b, op, false, false)?;
                        undo.enabled.entry(*j).or_insert(rules.enabled[j.index()]);
                        rules.enabled[j.index()] = *enabled;
                    }
                    _ => unreachable!("eligible batch"),
                }
            }
            for r in undo.base.keys() {
                let logical = revisions.logical(*r);
                let kind = logicals.kind(logical);
                if i.base[r.index()]
                    && i.policy.require_deps.contains(&kind)
                    && !matches!(kind, RecordKind::Evidence | RecordKind::CodeAnchor)
                    && logicals.origin_of(logical) == Origin::Local
                {
                    return Err(TxError::PolicyViolation {
                        kind,
                        logical,
                        reason: PolicyReason::BaseNotAllowed,
                    });
                }
            }
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
            for id in index
                .exact_by_rev
                .row(r.index())
                .iter()
                .chain(index.pin_by_rev.row(r.index()))
            {
                seen.insert(i.conclusion(index.inc_rule[id.index()]));
            }
        }
        for id in index.sem_by_logical.row(l.index()) {
            seen.insert(i.conclusion(index.inc_rule[id.index()]));
        }
    }
    seen
}
impl InputView for Before<'_> {
    fn revision_count(&self) -> usize {
        self.view.revision_count()
    }
    fn logical_count(&self) -> usize {
        self.view.logical_count()
    }
    fn rule_count(&self) -> usize {
        self.view.rule_count()
    }
    fn logical_kind(&self, id: LogicalId) -> RecordKind {
        self.view.logical_kind(id)
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
    fn premise_at(&self, id: RuleId, k: usize) -> Incidence {
        self.view.premise_at(id, k)
    }
    fn references(&self, id: RuleId) -> &[RevisionId] {
        self.view.references(id)
    }
    fn validation(&self, id: RuleId, v: &HeadVector) -> Option<&ValidationRecord> {
        match self.undo.validations.get(&(id, v.clone())) {
            Some(old) => old.as_ref(),
            None => self.view.validation(id, v),
        }
    }
}
