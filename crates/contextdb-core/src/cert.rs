//! Certificate issuance and validation (SPEC §6.4, ADR 0008 §6): pure
//! functions of the database, its per-epoch change log and `live_pos`.
//! Session bookkeeping (ledger supersession, handles) lives in the daemon;
//! these functions receive the final entry list.
use crate::{Db, InputView, Maintainer, StatsSink};
use contextdb_model::*;
use std::collections::{BTreeMap, BTreeSet};

/// Net per-epoch changes that `strict_no_aba` checks (ADR 0008 §6.3). Indexed
/// by logical, revision and scope so a check is O(vector · log(changes)).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChangeLog {
    heads: BTreeMap<LogicalId, Vec<Epoch>>,
    base: BTreeMap<RevisionId, Vec<Epoch>>,
    watermarks: BTreeMap<String, Vec<Epoch>>,
    pub(crate) rebindings: BTreeMap<RuleId, Vec<(Epoch, Vec<Incidence>)>>,
}
impl ChangeLog {
    /// `old_revisions`: revisions that existed before `epoch`. Creating a
    /// revision (base or not) is not a flip of an existing one.
    pub(crate) fn record(
        &mut self,
        epoch: Epoch,
        delta: &NetDelta,
        watermark_moves: &[String],
        old_revisions: usize,
    ) {
        for (l, old, _) in &delta.head_moves {
            // A new logical's first head is not a move of an existing head.
            if old.is_some() {
                self.heads.entry(*l).or_default().push(epoch);
            }
        }
        for r in delta
            .base_flips
            .iter()
            .filter(|r| r.index() < old_revisions)
        {
            self.base.entry(*r).or_default().push(epoch);
        }
        for s in watermark_moves {
            self.watermarks.entry(s.clone()).or_default().push(epoch);
        }
    }
    /// Undoes the latest [`Self::record`] with the same arguments (WHAT_IF).
    pub(crate) fn unrecord(
        &mut self,
        epoch: Epoch,
        delta: &NetDelta,
        watermark_moves: &[String],
        old_revisions: usize,
    ) {
        fn pop<K: Ord + Clone>(m: &mut BTreeMap<K, Vec<Epoch>>, k: &K, epoch: Epoch) {
            let row = m.get_mut(k).expect("recorded row");
            crate::invariant_assert!(row.last() == Some(&epoch));
            row.pop();
            if row.is_empty() {
                m.remove(k);
            }
        }
        for (l, old, _) in &delta.head_moves {
            if old.is_some() {
                pop(&mut self.heads, l, epoch);
            }
        }
        for r in delta
            .base_flips
            .iter()
            .filter(|r| r.index() < old_revisions)
        {
            pop(&mut self.base, r, epoch);
        }
        for s in watermark_moves {
            pop(&mut self.watermarks, s, epoch);
        }
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        use crate::memory::{btree_slots, vec_bytes as b};
        btree_slots(self.rebindings.len())
            * std::mem::size_of::<(RuleId, Vec<(Epoch, Vec<Incidence>)>)>()
            + self
                .rebindings
                .values()
                .map(|rows| {
                    b(rows)
                        + rows
                            .iter()
                            .map(|(_, incidences)| b(incidences))
                            .sum::<usize>()
                })
                .sum::<usize>()
            + btree_slots(self.heads.len()) * std::mem::size_of::<(LogicalId, Vec<Epoch>)>()
            + self.heads.values().map(b).sum::<usize>()
            + btree_slots(self.base.len()) * std::mem::size_of::<(RevisionId, Vec<Epoch>)>()
            + self.base.values().map(b).sum::<usize>()
            + btree_slots(self.watermarks.len()) * std::mem::size_of::<(String, Vec<Epoch>)>()
            + self.watermarks.values().map(b).sum::<usize>()
    }
}
/// Per-entry result of SPEC §6.4 step 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryOutcome {
    Denied,
    Invalid,
    Conflicted,
    Unknown,
    Pending,
    Ok,
}
/// The SPEC §6.4 step 2 table, total over SD13 summaries (PROP11 totality).
/// `is_head`: the entry's recorded revision is still its logical's head.
pub(crate) fn entry_outcome(summary: &Summary, is_head: bool) -> EntryOutcome {
    match summary {
        Summary::Unavailable => EntryOutcome::Denied,
        _ if !is_head => EntryOutcome::Invalid,
        Summary::Stale(_) => EntryOutcome::Invalid,
        Summary::Conflicted => EntryOutcome::Conflicted,
        Summary::Pending(PendingReason::ObservationLag) => EntryOutcome::Unknown,
        Summary::Pending(PendingReason::Semantic | PendingReason::StructuralUnknown) => {
            EntryOutcome::Pending
        }
        Summary::Certified => EntryOutcome::Ok,
    }
}
/// Epochs are recorded in increasing order: any entry > `since`?
fn after(epochs: Option<&Vec<Epoch>>, since: Epoch) -> bool {
    epochs
        .and_then(|e| e.last())
        .is_some_and(|last| *last > since)
}

/// Fields of a certificate that come from the session, not from the database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueRequest {
    pub session_id: [u8; 16],
    pub principal: PrincipalId,
    pub branch: BranchId,
    /// op_log checksum of the frame at the current epoch (genesis at 0).
    pub log_checksum: ContentHash,
    pub code_commit: String,
    pub strict_no_aba: bool,
}
/// Step 0 input from the store (ADR 0008 §4 op_log linkage).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogLink {
    pub matches: bool,
    pub current_checksum: ContentHash,
}
/// History needed for a strict check is no longer in memory; the store
/// answers from projections or log replay (ADR 0006 §8 source order).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryUnavailable {
    pub since: Epoch,
}

impl<M: Maintainer, S: StatsSink> Db<M, S> {
    /// Revision support for read-time observation/path proofs. Sem needs both
    /// its asserted revision and its current head; this is NOT the effective
    /// vector (which intentionally represents Sem with only its head).
    pub fn observation_support(&self, roots: &[RevisionId]) -> Result<Vec<RevisionId>, TxError> {
        let view = self.view();
        let mut seen = BTreeSet::new();
        let mut stack = roots.to_vec();
        while let Some(r) = stack.pop() {
            let node = self.node(r)?;
            if !seen.insert(r) {
                continue;
            }
            if let Some(Witness::Rule(j)) = node.witness[2] {
                for p in view.premises(j) {
                    if p.kind == EdgeKind::Ref {
                        continue;
                    }
                    stack.push(p.premise);
                    if p.kind == EdgeKind::Sem {
                        stack.push(view.head(view.revision_logical(p.premise)));
                    }
                }
            }
        }
        Ok(seen.into_iter().collect())
    }
    fn observation_scopes_at(
        &self,
        root: RevisionId,
        epoch: Epoch,
    ) -> Result<BTreeSet<String>, HistoryUnavailable> {
        let view = self.view();
        let mut stack = vec![root];
        let mut seen = BTreeSet::new();
        let mut scopes = BTreeSet::new();
        while let Some(r) = stack.pop() {
            if !seen.insert(r) {
                continue;
            }
            let node = self
                .read_historical(r, epoch)
                .map_err(|_| HistoryUnavailable { since: epoch })?;
            let logical = self.revisions.logical(r);
            if self.logicals.kind(logical) == RecordKind::CodeAnchor {
                scopes.insert(self.logicals.scope_lane(logical).to_owned());
            }
            if let Some(Witness::Rule(rule)) = node.witness[2] {
                for p in self
                    .changes
                    .rebindings
                    .get(&rule)
                    .and_then(|rows| rows.iter().find(|(changed, _)| *changed > epoch))
                    .map(|(_, old)| old.clone())
                    .unwrap_or_else(|| view.premises(rule).collect())
                {
                    if p.kind != EdgeKind::Ref {
                        stack.push(p.premise);
                    }
                    if p.kind == EdgeKind::Sem {
                        stack.push(
                            self.read_snapshot(view.revision_logical(p.premise), epoch)
                                .map_err(|_| HistoryUnavailable { since: epoch })?
                                .0,
                        );
                    }
                }
            }
        }
        Ok(scopes)
    }
    pub fn observation_scopes(&self, roots: &[RevisionId]) -> Result<BTreeSet<String>, TxError> {
        Ok(self
            .observation_support(roots)?
            .into_iter()
            .filter_map(|r| {
                let l = self.revisions.logical(r);
                (self.logicals.kind(l) == RecordKind::CodeAnchor)
                    .then(|| self.logicals.scope_lane(l).to_owned())
            })
            .collect())
    }
    /// Missing evidence is lagging from P5a onward (SD14b). Freshness is
    /// evaluated across the full observation support, never cached in L0/L1/L2.
    pub fn summary_at(
        &self,
        r: RevisionId,
        live_pos: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Summary, TxError> {
        let mut dim = self.node(r)?.dimensions;
        let scopes = self.observation_scopes(&[r])?;
        if !scopes.is_empty() {
            dim.observation = if scopes.iter().any(|s| {
                live_pos.get(s).is_none() || self.tables.watermarks.get(s) != live_pos.get(s)
            }) {
                Observation::Lagging
            } else {
                Observation::Fresh
            };
        }
        Ok(contextdb_oracle::summarize(&dim))
    }
    /// L2-witness closure of `roots` (ADR 0008 §6.2 step 3). Roots are
    /// `(revision, since_epoch)`; an item's `since_epoch` is the minimum over
    /// the roots reaching it. `Err(l)` names the smallest logical that gets two
    /// effective revisions (canonical, independent of traversal order).
    fn closure(&self, roots: &[(RevisionId, Epoch)]) -> Result<Vec<VectorItem>, LogicalId> {
        let view = self.view();
        let mut roots = roots.to_vec();
        // Ascending since: the first visit of a revision carries its minimum.
        roots.sort_by_key(|(r, since)| (*since, *r));
        let mut since: BTreeMap<RevisionId, Epoch> = BTreeMap::new();
        for (root, root_since) in roots {
            let mut stack = vec![root];
            while let Some(r) = stack.pop() {
                if since.contains_key(&r) {
                    continue;
                }
                since.insert(r, root_since);
                let node = self
                    .layers
                    .node_state(&view, r, self.maintainer.state().row(r));
                if let Some(Witness::Rule(j)) = node.witness[2] {
                    for p in view.premises(j) {
                        stack.push(match p.kind {
                            EdgeKind::Sem => view.head(view.revision_logical(p.premise)),
                            _ => p.premise,
                        });
                    }
                }
            }
        }
        let mut items: BTreeMap<LogicalId, (RevisionId, Epoch)> = BTreeMap::new();
        let mut ambiguous = BTreeSet::new();
        for (r, s) in since {
            let l = view.revision_logical(r);
            if items.insert(l, (r, s)).is_some() {
                ambiguous.insert(l);
            }
        }
        if let Some(l) = ambiguous.into_iter().next() {
            return Err(l);
        }
        Ok(items
            .into_iter()
            .map(|(logical, (rev, since_epoch))| VectorItem {
                logical,
                rev,
                since_epoch,
            })
            .collect())
    }
    /// Includes scopes hidden behind an asserted Sem revision, independently
    /// from effective-vector ambiguity checks.
    fn support_watermarks(&self, roots: &[(RevisionId, Epoch)]) -> Vec<(String, Vec<u8>)> {
        self.observation_scopes(&roots.iter().map(|(r, _)| *r).collect::<Vec<_>>())
            .expect("checked roots")
            .into_iter()
            .map(|s| {
                let pos = self.tables.watermarks.get(&s).cloned().unwrap_or_default();
                (s, pos)
            })
            .collect()
    }
    /// Issues a certificate for `entries` at the current epoch (SPEC §6.4).
    pub fn issue_certificate(
        &self,
        request: &IssueRequest,
        entries: &[CertItem],
        live_pos: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Certificate, IssueFailure> {
        let mut required: Vec<&CertItem> = entries.iter().filter(|e| e.required).collect();
        if required.is_empty() {
            return Err(IssueFailure::Empty);
        }
        required.sort_by_key(|e| (e.logical, e.rev));
        for pair in required.windows(2) {
            if pair[0].logical == pair[1].logical && pair[0].rev != pair[1].rev {
                return Err(IssueFailure::Ambiguous {
                    logical: pair[0].logical,
                });
            }
        }
        for e in &required {
            if e.rev.index() >= self.revisions.len() || self.revisions.logical(e.rev) != e.logical {
                return Err(IssueFailure::NotCertified {
                    logical: e.logical,
                    rev: e.rev,
                    summary: Summary::Stale(StaleReason::Unsupported),
                });
            }
            let summary = self.summary_at(e.rev, live_pos).expect("checked revision");
            if summary != Summary::Certified {
                return Err(IssueFailure::NotCertified {
                    logical: e.logical,
                    rev: e.rev,
                    summary,
                });
            }
        }
        let roots: Vec<_> = required.iter().map(|e| (e.rev, e.since_epoch)).collect();
        let vector = self
            .closure(&roots)
            .map_err(|logical| IssueFailure::Ambiguous { logical })?;
        let mut cert = Certificate {
            cert_id: [0; 32],
            session_id: request.session_id,
            principal: request.principal,
            branch: request.branch,
            issued_epoch: self.epoch,
            log_checksum_at_issue: request.log_checksum,
            code_commit: request.code_commit.clone(),
            watermarks: self.support_watermarks(&roots),
            entries: entries.to_vec(),
            vector,
            strict_no_aba: request.strict_no_aba,
        };
        cert.cert_id = *blake3::hash(&cert.body_bytes()).as_bytes();
        Ok(cert)
    }
    /// Validates `cert` against the current state (SPEC §6.4 steps 0–4).
    /// `log` is step 0, supplied by the store: whether the op_log frame at
    /// `issued_epoch` still has `log_checksum_at_issue`, and the checksum at
    /// the current epoch for a recertified certificate. The result never
    /// depends on epoch equality, only on `since_epoch` as the strict window
    /// start (SD16).
    pub fn validate_certificate(
        &self,
        cert: &Certificate,
        live_pos: &BTreeMap<String, Vec<u8>>,
        log: LogLink,
    ) -> Result<Validation, HistoryUnavailable> {
        let log_matches = log.matches;
        if !log_matches || cert.issued_epoch > self.epoch {
            return Ok(Validation::Invalid {
                reason: InvalidReason::LogMismatch,
                diffs: Vec::new(),
            });
        }
        let view = self.view();
        let mut strict_diffs = Vec::new();
        // Step 1: strict, from each vector item's own since_epoch.
        if cert.strict_no_aba {
            let mut diffs = Vec::new();
            for v in &cert.vector {
                if v.since_epoch < self.initial_epoch {
                    return Err(HistoryUnavailable {
                        since: v.since_epoch,
                    });
                }
                let scope = (self.logicals.kind(v.logical) == RecordKind::CodeAnchor)
                    .then(|| self.logicals.scope_lane(v.logical));
                let moved = after(self.changes.heads.get(&v.logical), v.since_epoch)
                    || after(self.changes.base.get(&v.rev), v.since_epoch)
                    || scope.is_some_and(|s| after(self.changes.watermarks.get(s), v.since_epoch));
                if moved {
                    let head = view.head(v.logical);
                    diffs.push(CertDiff {
                        logical: v.logical,
                        cert_rev: Some(v.rev),
                        current: Some(head),
                        summary: self.summary_at(head, live_pos).ok(),
                    });
                }
            }
            let mut scope_since: BTreeMap<String, Epoch> = BTreeMap::new();
            for e in cert.entries.iter().filter(|e| e.required) {
                for scope in self.observation_scopes_at(e.rev, cert.issued_epoch)? {
                    let since = scope_since.entry(scope).or_insert(e.since_epoch);
                    *since = (*since).min(e.since_epoch);
                }
            }
            if cert.watermarks.iter().any(|(scope, _)| {
                scope_since
                    .get(scope)
                    .is_some_and(|since| after(self.changes.watermarks.get(scope), *since))
            }) && diffs.is_empty()
                && let Some(e) = cert.entries.iter().find(|e| e.required)
            {
                diffs.push(CertDiff {
                    logical: e.logical,
                    cert_rev: Some(e.rev),
                    current: self.head(e.logical),
                    summary: self
                        .head(e.logical)
                        .and_then(|r| self.summary_at(r, live_pos).ok()),
                });
            }
            strict_diffs = diffs;
        }
        // Step 2: per required entry, the current head's summary.
        let required: Vec<&CertItem> = cert.entries.iter().filter(|e| e.required).collect();
        if required.is_empty() {
            return Ok(Validation::Empty);
        }
        let (mut denied, mut invalid, mut conflicted, mut lag) =
            (false, Vec::new(), Vec::new(), BTreeSet::new());
        let (mut pending, mut pending_rules) = (false, BTreeSet::new());
        for e in &required {
            let head = view.head(e.logical);
            let summary = self.summary_at(head, live_pos).expect("head exists");
            let diff = || CertDiff {
                logical: e.logical,
                cert_rev: Some(e.rev),
                current: Some(head),
                summary: Some(summary.clone()),
            };
            match entry_outcome(&summary, head == e.rev) {
                EntryOutcome::Denied => denied = true,
                EntryOutcome::Invalid => invalid.push(diff()),
                EntryOutcome::Conflicted => conflicted.push(e.logical),
                EntryOutcome::Unknown => {}
                EntryOutcome::Pending => {
                    pending = true;
                    pending_rules.extend(self.pending_rules(head));
                }
                EntryOutcome::Ok => {}
            }
        }
        // Step 3: the vector recomputed on the current state.
        let roots: Vec<_> = required
            .iter()
            .map(|e| (view.head(e.logical), e.since_epoch))
            .collect();
        let recomputed = self.closure(&roots);
        for scope in self
            .observation_scopes(&roots.iter().map(|(r, _)| *r).collect::<Vec<_>>())
            .expect("roots exist")
        {
            if live_pos.get(&scope).is_none()
                || self.tables.watermarks.get(&scope) != live_pos.get(&scope)
            {
                lag.insert(scope);
            }
        }
        if denied {
            return Ok(Validation::Denied);
        }
        if !strict_diffs.is_empty() {
            strict_diffs.extend(invalid);
            strict_diffs.sort_by_key(|d| d.logical);
            strict_diffs.dedup_by_key(|d| d.logical);
            return Ok(Validation::Invalid {
                reason: InvalidReason::StrictChange,
                diffs: strict_diffs,
            });
        }
        if !invalid.is_empty() {
            return Ok(Validation::Invalid {
                reason: InvalidReason::Changed,
                diffs: invalid,
            });
        }
        if !conflicted.is_empty() {
            return Ok(Validation::Conflicted {
                entries: conflicted,
            });
        }
        let vector = match recomputed {
            Err(logical) => return Ok(Validation::Ambiguous { logical }),
            Ok(v) => v,
        };
        if !lag.is_empty() {
            return Ok(Validation::Unknown {
                lag: lag.into_iter().collect(),
            });
        }
        if pending {
            return Ok(Validation::Pending {
                rules: pending_rules.into_iter().collect(),
            });
        }
        // Step 4: all Ok.
        let same = vector.len() == cert.vector.len()
            && vector
                .iter()
                .zip(&cert.vector)
                .all(|(a, b)| (a.logical, a.rev) == (b.logical, b.rev));
        if same {
            return Ok(Validation::Valid);
        }
        let old: BTreeMap<_, _> = cert.vector.iter().map(|v| (v.logical, v.rev)).collect();
        let new: BTreeMap<_, _> = vector.iter().map(|v| (v.logical, v.rev)).collect();
        let diffs: Vec<CertDiff> = old
            .keys()
            .chain(new.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|l| old.get(*l) != new.get(*l))
            .map(|l| CertDiff {
                logical: *l,
                cert_rev: old.get(l).copied(),
                current: new.get(l).copied(),
                summary: new.get(l).and_then(|r| self.summary_at(*r, live_pos).ok()),
            })
            .collect();
        let changed: BTreeSet<LogicalId> = diffs.iter().map(|d| d.logical).collect();
        let mut new_cert = cert.clone();
        new_cert.issued_epoch = self.epoch;
        new_cert.log_checksum_at_issue = log.current_checksum;
        new_cert.watermarks = self.support_watermarks(&roots);
        // An entry whose own closure changed is now certified via an alternative.
        for e in new_cert.entries.iter_mut().filter(|e| e.required) {
            let own = self
                .closure(&[(e.rev, e.since_epoch)])
                .expect("sub-closure of an unambiguous closure");
            if own.iter().any(|v| changed.contains(&v.logical)) {
                e.via = Via::Alternative;
            }
        }
        new_cert.vector = vector;
        new_cert.cert_id = *blake3::hash(&new_cert.body_bytes()).as_bytes();
        Ok(Validation::ValidRecertified {
            new_cert: Box::new(new_cert),
            diffs,
        })
    }
    /// Rules concluding `r` whose semantic gate is pending.
    fn pending_rules(&self, r: RevisionId) -> Vec<RuleId> {
        self.indexes
            .rules_by_concl
            .row(r.index())
            .iter()
            .filter(|j| self.layers.sem(**j) == SemState::Pending)
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entry_table_is_total_over_sd13_summaries() {
        use EntryOutcome as O;
        let stale = [
            StaleReason::HeadSuperseded,
            StaleReason::Unsupported,
            StaleReason::Rejected,
            StaleReason::SupersededPinnedPremise(RevisionId(3)),
        ];
        for head in [true, false] {
            assert_eq!(entry_outcome(&Summary::Unavailable, head), O::Denied);
            for r in &stale {
                assert_eq!(entry_outcome(&Summary::Stale(r.clone()), head), O::Invalid);
            }
        }
        // A non-head entry is Invalid whatever its head's summary (HeadSuperseded).
        for s in [
            Summary::Conflicted,
            Summary::Certified,
            Summary::Pending(PendingReason::Semantic),
            Summary::Pending(PendingReason::ObservationLag),
        ] {
            assert_eq!(entry_outcome(&s, false), O::Invalid);
        }
        assert_eq!(entry_outcome(&Summary::Conflicted, true), O::Conflicted);
        assert_eq!(
            entry_outcome(&Summary::Pending(PendingReason::ObservationLag), true),
            O::Unknown
        );
        assert_eq!(
            entry_outcome(&Summary::Pending(PendingReason::Semantic), true),
            O::Pending
        );
        // T65: PENDING(StructuralUnknown) → Pending (reachable from P10).
        assert_eq!(
            entry_outcome(&Summary::Pending(PendingReason::StructuralUnknown), true),
            O::Pending
        );
        assert_eq!(entry_outcome(&Summary::Certified, true), O::Ok);
    }
}
