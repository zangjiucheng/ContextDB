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
    pub(crate) fn retained_bytes(&self) -> usize {
        use crate::memory::{btree_slots, vec_bytes as b};
        btree_slots(self.heads.len()) * std::mem::size_of::<(LogicalId, Vec<Epoch>)>()
            + self.heads.values().map(b).sum::<usize>()
            + btree_slots(self.base.len()) * std::mem::size_of::<(RevisionId, Vec<Epoch>)>()
            + self.base.values().map(b).sum::<usize>()
            + btree_slots(self.watermarks.len()) * std::mem::size_of::<(String, Vec<Epoch>)>()
            + self.watermarks.values().map(b).sum::<usize>()
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
    /// SD13 summary of `r` with the read-time observation dimension (SD14b):
    /// a CodeAnchor whose scope has a `live_pos` different from the committed
    /// watermark is lagging. A scope missing from `live_pos` is fresh.
    pub fn summary_at(
        &self,
        r: RevisionId,
        live_pos: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Summary, TxError> {
        let mut dim = self.node(r)?.dimensions;
        let l = self.revisions.logical(r);
        if self.logicals.kind(l) == RecordKind::CodeAnchor {
            let scope = self.logicals.scope_lane(l);
            if live_pos
                .get(scope)
                .is_some_and(|v| self.tables.watermarks.get(scope) != Some(v))
            {
                dim.observation = Observation::Lagging;
            } else {
                dim.observation = Observation::Fresh;
            }
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
    /// Committed watermark positions of the CodeAnchor scopes in `vector`.
    fn vector_watermarks(&self, vector: &[VectorItem]) -> Vec<(String, Vec<u8>)> {
        let scopes: BTreeSet<&str> = vector
            .iter()
            .filter(|v| self.logicals.kind(v.logical) == RecordKind::CodeAnchor)
            .map(|v| self.logicals.scope_lane(v.logical))
            .collect();
        scopes
            .into_iter()
            .map(|s| {
                let pos = self.tables.watermarks.get(s).cloned().unwrap_or_default();
                (s.to_owned(), pos)
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
            watermarks: self.vector_watermarks(&vector),
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
            if !diffs.is_empty() {
                return Ok(Validation::Invalid {
                    reason: InvalidReason::StrictChange,
                    diffs,
                });
            }
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
            if summary == Summary::Unavailable {
                denied = true;
            } else if head != e.rev || matches!(summary, Summary::Stale(_)) {
                invalid.push(diff());
            } else {
                match summary {
                    Summary::Conflicted => conflicted.push(e.logical),
                    Summary::Pending(PendingReason::ObservationLag) => {
                        lag.insert(self.logicals.scope_lane(e.logical).to_owned());
                    }
                    Summary::Pending(_) => {
                        pending = true;
                        pending_rules.extend(self.pending_rules(head));
                    }
                    _ => {}
                }
            }
        }
        // Step 3: the vector recomputed on the current state.
        let roots: Vec<_> = required
            .iter()
            .map(|e| (view.head(e.logical), e.since_epoch))
            .collect();
        let recomputed = self.closure(&roots);
        if let Ok(vector) = &recomputed {
            for v in vector {
                if self.logicals.kind(v.logical) == RecordKind::CodeAnchor
                    && self.summary_at(v.rev, live_pos).expect("vector revision")
                        == Summary::Pending(PendingReason::ObservationLag)
                {
                    lag.insert(self.logicals.scope_lane(v.logical).to_owned());
                }
            }
        }
        if denied {
            return Ok(Validation::Denied);
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
        new_cert.watermarks = self.vector_watermarks(&vector);
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
