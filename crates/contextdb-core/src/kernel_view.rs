//! Branch-aware reads with global ids (ADR 0013 §7; protocol 4). Each read
//! maps its arguments to the branch database's local ids, runs the
//! single-branch query and maps every id in the answer back. Certificates
//! are re-sorted and re-hashed under global ids. On `main`, current-mode
//! reads annotate `BRANCH_ONLY` (a read-time dimension, never stored).
use super::{Branch, BranchInfo, INVISIBLE, Kernel, Kind};
use crate::{
    Counting, Db, EpochDiff, HistoryUnavailable, Impact, IssueRequest, LogLink, Maintainer,
    NoStats, NodeChange, Proof, StatsSink,
};
use contextdb_model::*;
use std::{collections::BTreeMap, sync::Arc};

/// Read access to one branch with global ids.
pub struct BranchView<'a, M: Maintainer = Counting, S: StatsSink = NoStats> {
    pub(super) b: &'a Branch<M, S>,
    pub(super) k: &'a Kernel<M, S>,
}

impl<'a, M: Maintainer, S: StatsSink> BranchView<'a, M, S> {
    pub fn info(&self) -> &BranchInfo {
        &self.b.info
    }
    /// Objects this branch imported by dedup or merge, with the epoch.
    pub fn imports(&self) -> &[(super::Import, Epoch)] {
        &self.b.imports
    }
    /// The branch's single-branch database (local ids).
    pub fn db(&self) -> &'a Db<M, S> {
        &self.b.db
    }
    /// The global epoch: "now" for certificates on this branch.
    pub fn epoch(&self) -> Epoch {
        self.k.epoch
    }
    /// Whether the branch still shares its state with the branch it forked
    /// from (or a fork of it): neither has written since.
    pub fn shares_state_with(&self, other: &BranchView<'_, M, S>) -> bool {
        Arc::ptr_eq(&self.b.db, &other.b.db)
    }

    // Id maps.
    fn local(&self, k: Kind, global: u32) -> Option<u32> {
        let l = self.b.ids.local(k, global);
        (l != INVISIBLE).then_some(l)
    }
    fn lr(&self, r: RevisionId) -> Result<RevisionId, TxError> {
        self.local(Kind::Revision, r.0)
            .map(RevisionId)
            .ok_or(TxError::UnknownRef)
    }
    fn ll(&self, l: LogicalId) -> Result<LogicalId, TxError> {
        self.local(Kind::Logical, l.0)
            .map(LogicalId)
            .ok_or(TxError::UnknownRef)
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
    fn g_witness(&self, w: Witness) -> Witness {
        match w {
            Witness::Rule(j) => Witness::Rule(self.global_rule(j)),
            Witness::Base => Witness::Base,
        }
    }
    fn g_reason(&self, r: StaleReason) -> StaleReason {
        match r {
            StaleReason::SupersededPinnedPremise(p) => {
                StaleReason::SupersededPinnedPremise(self.global_revision(p))
            }
            other => other,
        }
    }
    fn g_summary(&self, s: Summary) -> Summary {
        match s {
            Summary::Stale(r) => Summary::Stale(self.g_reason(r)),
            other => other,
        }
    }
    fn g_node(&self, mut n: NodeState) -> NodeState {
        for w in &mut n.witness {
            *w = w.map(|w| self.g_witness(w));
        }
        n.dimensions.stale_reason = self.g_reason(n.dimensions.stale_reason);
        n
    }
    fn g_vector(&self, v: &HeadVector) -> HeadVector {
        let mut out: HeadVector = (v.iter())
            .map(|(l, r)| (self.global_logical(*l), self.global_revision(*r)))
            .collect();
        out.sort_unstable();
        out
    }
    fn g_explanation(&self, e: Explanation) -> Explanation {
        Explanation {
            revision: self.global_revision(e.revision),
            base: e.base,
            active_rules: e
                .active_rules
                .iter()
                .map(|j| self.global_rule(*j))
                .collect(),
            witness: e.witness.map(|w| self.g_witness(w)),
            references: (e.references.iter())
                .map(|(j, r)| (self.global_rule(*j), self.global_revision(*r)))
                .collect(),
        }
    }
    fn g_proof(&self, p: Proof) -> Proof {
        Proof {
            explanation: self.g_explanation(p.explanation),
            premises: p.premises.into_iter().map(|p| self.g_proof(p)).collect(),
            truncated: p.truncated,
        }
    }
    fn g_why_not(&self, w: WhyNot) -> WhyNot {
        let r = |x: RevisionId| self.global_revision(x);
        WhyNot {
            revision: r(w.revision),
            rules: (w.rules.into_iter())
                .map(|f| RuleFailure {
                    rule: self.global_rule(f.rule),
                    disabled: f.disabled,
                    gate: f.gate,
                    asserted: self.g_vector(&f.asserted),
                    current: self.g_vector(&f.current),
                    incidences: (f.incidences.into_iter())
                        .map(|i| match i {
                            IncidenceFailure::Unsupported(p) => IncidenceFailure::Unsupported(r(p)),
                            IncidenceFailure::NotCurrent { premise, head } => {
                                IncidenceFailure::NotCurrent {
                                    premise: r(premise),
                                    head: r(head),
                                }
                            }
                            IncidenceFailure::SupersededPinned { premise, head } => {
                                IncidenceFailure::SupersededPinned {
                                    premise: r(premise),
                                    head: r(head),
                                }
                            }
                            IncidenceFailure::SemanticHeadUnsupported(p) => {
                                IncidenceFailure::SemanticHeadUnsupported(r(p))
                            }
                        })
                        .collect(),
                    upstream: f.upstream.into_iter().map(|u| self.g_why_not(u)).collect(),
                })
                .collect(),
        }
    }

    // BRANCH_ONLY (§7): on `main`, a visible revision superseded here but
    // the head of an open branch.
    fn branch_only(&self, local: RevisionId) -> bool {
        if self.b.info.id != MAIN {
            return false;
        }
        let db = &self.b.db;
        let Some(logical) = db.revision_logical(local) else {
            return false;
        };
        if db.head(logical) == Some(local) {
            return false;
        }
        let (gl, gr) = (self.global_logical(logical), self.global_revision(local));
        (self.k.branches.iter())
            .filter(|b| b.info.id != MAIN && !b.info.archived)
            .any(|b| {
                let other = BranchView { b, k: self.k };
                other.head(gl) == Some(gr)
            })
    }
    fn annotate(&self, local: RevisionId, mut n: NodeState) -> NodeState {
        if self.branch_only(local) {
            n.dimensions.currency = Currency::BranchOnly;
        }
        n
    }

    // Current-mode reads.
    pub fn logical_by_key(&self, key: &str) -> Option<LogicalId> {
        let l = self.b.db.logical_by_key(key)?;
        Some(self.global_logical(l))
    }
    /// Visible logicals, in the branch's canonical order.
    pub fn logicals(&self) -> impl Iterator<Item = LogicalId> + '_ {
        (0..self.b.db.logical_count() as u32).map(|l| self.global_logical(LogicalId(l)))
    }
    pub fn logical_count(&self) -> usize {
        self.b.db.logical_count()
    }
    pub fn revision_count(&self) -> usize {
        self.b.db.revision_count()
    }
    pub fn logical_info(&self, l: LogicalId) -> Option<(String, RecordKind, String)> {
        self.b.db.logical_info(self.ll(l).ok()?)
    }
    pub fn head(&self, l: LogicalId) -> Option<RevisionId> {
        let r = self.b.db.head(self.ll(l).ok()?)?;
        Some(self.global_revision(r))
    }
    pub fn revision_logical(&self, r: RevisionId) -> Option<LogicalId> {
        let l = self.b.db.revision_logical(self.lr(r).ok()?)?;
        Some(self.global_logical(l))
    }
    pub fn revision_content(&self, r: RevisionId) -> Result<ContentHash, TxError> {
        self.b.db.revision_content(self.lr(r)?)
    }
    pub fn is_base(&self, r: RevisionId) -> Result<bool, TxError> {
        self.b.db.is_base(self.lr(r)?)
    }
    pub fn rule_enabled(&self, j: RuleId) -> Option<bool> {
        let local = self.local(Kind::Rule, j.0)?;
        Some(self.b.db.rules.enabled[local as usize])
    }
    pub fn is_visible_revision(&self, r: RevisionId) -> bool {
        self.local(Kind::Revision, r.0).is_some()
    }
    pub fn watermark(&self, scope: &str) -> Option<&'a [u8]> {
        self.b.db.watermark(scope)
    }
    pub fn anchor_unobserved(&self, l: LogicalId) -> bool {
        self.ll(l).is_ok_and(|l| self.b.db.anchor_unobserved(l))
    }
    pub fn initial_epoch(&self) -> Epoch {
        self.b.db.initial_epoch()
    }
    pub fn node(&self, r: RevisionId) -> Result<NodeState, TxError> {
        let local = self.lr(r)?;
        Ok(self.annotate(local, self.g_node(self.b.db.node(local)?)))
    }
    pub fn status(&self, r: RevisionId) -> Result<Summary, TxError> {
        Ok(contextdb_oracle::summarize(&self.node(r)?.dimensions))
    }
    pub fn current(&self, l: LogicalId) -> Result<(RevisionId, NodeState), TxError> {
        let (r, n) = self.b.db.current(self.ll(l)?)?;
        Ok((self.global_revision(r), self.g_node(n)))
    }
    /// The summary with the read-time observation (and on `main`,
    /// `BRANCH_ONLY`) dimensions.
    pub fn summary_at(
        &self,
        r: RevisionId,
        live_pos: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Summary, TxError> {
        let local = self.lr(r)?;
        let summary = self.b.db.summary_at(local, live_pos)?;
        // BRANCH_ONLY's summary is STALE(HeadSuperseded), as Superseded's.
        Ok(self.g_summary(summary))
    }
    pub fn why(&self, r: RevisionId) -> Result<Explanation, TxError> {
        Ok(self.g_explanation(self.b.db.why(self.lr(r)?)?))
    }
    pub fn why_depth(&self, r: RevisionId, depth: usize) -> Result<Proof, TxError> {
        Ok(self.g_proof(self.b.db.why_depth(self.lr(r)?, depth)?))
    }
    pub fn why_not(&self, r: RevisionId, depth: usize) -> Result<WhyNot, TxError> {
        Ok(self.g_why_not(self.b.db.why_not(self.lr(r)?, depth)?))
    }
    pub fn rules_concluding(
        &self,
        r: RevisionId,
    ) -> Vec<(RuleId, bool, Vec<Incidence>, Vec<RevisionId>)> {
        let Ok(local) = self.lr(r) else {
            return Vec::new();
        };
        (self.b.db.rules_concluding(local).into_iter())
            .map(|(j, enabled, premises, refs)| {
                let mut premises: Vec<Incidence> = (premises.into_iter())
                    .map(|p| Incidence {
                        premise: self.global_revision(p.premise),
                        kind: p.kind,
                    })
                    .collect();
                premises.sort_unstable();
                let mut refs: Vec<RevisionId> =
                    refs.into_iter().map(|r| self.global_revision(r)).collect();
                refs.sort_unstable();
                (self.global_rule(j), enabled, premises, refs)
            })
            .collect()
    }
    /// Revisions an observation/path proof of `roots` rests on.
    pub fn observation_support(&self, roots: &[RevisionId]) -> Result<Vec<RevisionId>, TxError> {
        let roots = roots
            .iter()
            .map(|r| self.lr(*r))
            .collect::<Result<Vec<_>, _>>()?;
        let mut out: Vec<RevisionId> = (self.b.db.observation_support(&roots)?.into_iter())
            .map(|r| self.global_revision(r))
            .collect();
        out.sort_unstable();
        Ok(out)
    }
    pub fn impact_reachable(&self, roots: &[RevisionId]) -> Result<Impact, TxError> {
        let roots = roots
            .iter()
            .map(|r| self.lr(*r))
            .collect::<Result<Vec<_>, _>>()?;
        let i = self.b.db.impact_reachable(&roots)?;
        let g = |v: Vec<RevisionId>| {
            let mut v: Vec<_> = v.into_iter().map(|r| self.global_revision(r)).collect();
            v.sort_unstable();
            v
        };
        Ok(Impact {
            supported: g(i.supported),
            reference_only: g(i.reference_only),
        })
    }

    // Historical reads (never `BRANCH_ONLY`). The branch database is
    // unchanged between its own commits, so later epochs read its latest.
    fn at(&self, e: Epoch) -> Epoch {
        e.min(self.b.db.epoch())
    }
    pub fn read_historical(&self, r: RevisionId, e: Epoch) -> Result<NodeState, TxError> {
        if e > self.k.epoch {
            return Err(TxError::UnknownRef);
        }
        Ok(self.g_node(self.b.db.read_historical(self.lr(r)?, self.at(e))?))
    }
    pub fn read_snapshot(
        &self,
        l: LogicalId,
        e: Epoch,
    ) -> Result<(RevisionId, NodeState), TxError> {
        if e > self.k.epoch {
            return Err(TxError::UnknownRef);
        }
        let (r, n) = self.b.db.read_snapshot(self.ll(l)?, self.at(e))?;
        Ok((self.global_revision(r), self.g_node(n)))
    }
    pub fn diff(&self, a: Epoch, b: Epoch) -> Result<EpochDiff, TxError> {
        if a.max(b) > self.k.epoch {
            return Err(TxError::UnknownRef);
        }
        let d = self.b.db.diff(self.at(a), self.at(b))?;
        let mut changes: Vec<NodeChange> = (d.changes.into_iter())
            .map(|c| NodeChange {
                revision: self.global_revision(c.revision),
                before: c.before.map(|n| self.g_node(n)),
                after: c.after.map(|n| self.g_node(n)),
            })
            .collect();
        changes.sort_by_key(|c| c.revision);
        Ok(EpochDiff {
            from: a,
            to: b,
            changes,
        })
    }

    // Certificates (§7): issued and validated on this branch at the global
    // epoch, with global ids; `cert_id` hashes the global form.
    fn g_cert(&self, mut c: Certificate) -> Certificate {
        for e in &mut c.entries {
            e.logical = self.global_logical(e.logical);
            e.rev = self.global_revision(e.rev);
        }
        for v in &mut c.vector {
            v.logical = self.global_logical(v.logical);
            v.rev = self.global_revision(v.rev);
        }
        c.vector.sort();
        c.cert_id = *blake3::hash(&c.body_bytes()).as_bytes();
        c
    }
    fn g_diff(&self, d: CertDiff) -> CertDiff {
        CertDiff {
            logical: self.global_logical(d.logical),
            cert_rev: d.cert_rev.map(|r| self.global_revision(r)),
            current: d.current.map(|r| self.global_revision(r)),
            summary: d.summary.map(|s| self.g_summary(s)),
        }
    }
    pub fn issue_certificate(
        &self,
        request: &IssueRequest,
        entries: &[CertItem],
        live_pos: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Certificate, IssueFailure> {
        let mut local = Vec::with_capacity(entries.len());
        for e in entries {
            match (self.ll(e.logical), self.lr(e.rev)) {
                (Ok(logical), Ok(rev)) => local.push(CertItem { logical, rev, ..*e }),
                _ if !e.required => {}
                _ => {
                    return Err(IssueFailure::NotCertified {
                        logical: e.logical,
                        rev: e.rev,
                        summary: Summary::Stale(StaleReason::Unsupported),
                    });
                }
            }
        }
        let issued = self
            .b
            .db
            .issue_certificate_at(request, &local, live_pos, self.k.epoch);
        match issued {
            Ok(c) => {
                let mut c = self.g_cert(c);
                // The entries as requested (advisory ones invisible here too).
                c.entries = entries.to_vec();
                c.cert_id = *blake3::hash(&c.body_bytes()).as_bytes();
                Ok(c)
            }
            Err(IssueFailure::NotCertified {
                logical,
                rev,
                summary,
            }) => Err(IssueFailure::NotCertified {
                logical: self.global_logical(logical),
                rev: self.global_revision(rev),
                summary: self.g_summary(summary),
            }),
            Err(IssueFailure::Ambiguous { logical }) => Err(IssueFailure::Ambiguous {
                logical: self.global_logical(logical),
            }),
            Err(IssueFailure::Empty) => Err(IssueFailure::Empty),
        }
    }
    pub fn validate_certificate(
        &self,
        cert: &Certificate,
        live_pos: &BTreeMap<String, Vec<u8>>,
        log: LogLink,
    ) -> Result<Validation, HistoryUnavailable> {
        if cert.branch != self.b.info.id {
            return Ok(Validation::Invalid {
                reason: InvalidReason::BranchMismatch,
                diffs: Vec::new(),
            });
        }
        // Visibility only grows, so everything a certificate issued on this
        // branch requires is still visible; a required entry that is not is
        // a changed entry. Advisory entries invisible here are skipped.
        let mut local = cert.clone();
        let mut kept = Vec::new();
        local.entries.clear();
        for (n, e) in cert.entries.iter().enumerate() {
            match (self.ll(e.logical), self.lr(e.rev)) {
                (Ok(logical), Ok(rev)) => {
                    local.entries.push(CertItem { logical, rev, ..*e });
                    kept.push(n);
                }
                _ if !e.required => {}
                _ => {
                    return Ok(Validation::Invalid {
                        reason: InvalidReason::Changed,
                        diffs: vec![CertDiff {
                            logical: e.logical,
                            cert_rev: Some(e.rev),
                            current: None,
                            summary: None,
                        }],
                    });
                }
            }
        }
        let mut vector = Vec::with_capacity(cert.vector.len());
        for v in &cert.vector {
            let (Ok(logical), Ok(rev)) = (self.ll(v.logical), self.lr(v.rev)) else {
                return Ok(Validation::Invalid {
                    reason: InvalidReason::Changed,
                    diffs: Vec::new(),
                });
            };
            vector.push(VectorItem {
                logical,
                rev,
                since_epoch: v.since_epoch,
            });
        }
        vector.sort();
        local.vector = vector;
        let v = self
            .b
            .db
            .validate_certificate_at(&local, live_pos, log, self.k.epoch)?;
        Ok(match v {
            Validation::Invalid { reason, diffs } => Validation::Invalid {
                reason,
                diffs: diffs.into_iter().map(|d| self.g_diff(d)).collect(),
            },
            Validation::Conflicted { entries } => Validation::Conflicted {
                entries: entries
                    .into_iter()
                    .map(|l| self.global_logical(l))
                    .collect(),
            },
            Validation::Ambiguous { logical } => Validation::Ambiguous {
                logical: self.global_logical(logical),
            },
            Validation::Pending { rules } => Validation::Pending {
                rules: rules.into_iter().map(|j| self.global_rule(j)).collect(),
            },
            Validation::ValidRecertified { new_cert, diffs } => {
                // The original entries, with `via` as recertification set it.
                let mut entries = cert.entries.clone();
                for (n, e) in kept.iter().zip(&new_cert.entries) {
                    entries[*n].via = e.via;
                }
                let mut new_cert = self.g_cert(*new_cert);
                new_cert.entries = entries;
                new_cert.cert_id = *blake3::hash(&new_cert.body_bytes()).as_bytes();
                Validation::ValidRecertified {
                    new_cert: Box::new(new_cert),
                    diffs: diffs.into_iter().map(|d| self.g_diff(d)).collect(),
                }
            }
            other => other,
        })
    }

    /// The branch digest: its visible inputs and derived state under its
    /// canonical (local) ids, then the branch-table entry and the sync
    /// points it received (a fork in the child's, a merge in the target's;
    /// ADR 0013 §3, §6.1). `main` without merges hashes as its database, so
    /// single-branch digests are unchanged.
    pub fn digest(&self) -> ContentHash {
        let info = &self.b.info;
        let received: Vec<_> = (self.k.syncs.iter())
            .filter(|p| p.dst == info.id)
            .map(|p| (p.src, p.epoch))
            .collect();
        if info.id == MAIN && received.is_empty() {
            return self.b.db.digest();
        }
        let mut h = blake3::Hasher::new();
        h.update(&self.b.db.digest());
        h.update(b"branch");
        h.update(&info.id.0.to_le_bytes());
        h.update(&(info.name.len() as u32).to_le_bytes());
        h.update(info.name.as_bytes());
        h.update(
            &info
                .parent
                .map_or(u32::MAX, |p| u32::from(p.0))
                .to_le_bytes(),
        );
        h.update(&info.fork_epoch.0.to_le_bytes());
        h.update(&[u8::from(info.archived)]);
        h.update(&(received.len() as u32).to_le_bytes());
        for (src, epoch) in received {
            h.update(&src.0.to_le_bytes());
            h.update(&epoch.0.to_le_bytes());
        }
        *h.finalize().as_bytes()
    }
}
