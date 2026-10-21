//! Independent certificate issuance and validation (SPEC §6.4, ADR 0008
//! §6.5): full recomputation with Jacobi over explicit input snapshots. Strict
//! changes come from comparing consecutive committed states, not from any
//! change log. `cert_id` is left zero: the oracle depends on the model only,
//! and PROP11 compares every other field.
use crate::{jacobi, summarize};
use contextdb_model::*;
use std::collections::{BTreeMap, BTreeSet};

/// Committed input states by epoch, from genesis (index = epoch).
pub type History = [Inputs];

fn summary(
    i: &Inputs,
    d: &DerivedSnapshot,
    r: RevisionId,
    live: &BTreeMap<String, Vec<u8>>,
) -> Summary {
    let mut dim = d.nodes[r.index()].dimensions.clone();
    let l = &i.logicals[i.revisions[r.index()].logical.index()];
    if l.kind == RecordKind::CodeAnchor {
        let lagging = live
            .get(&l.scope_lane)
            .is_some_and(|v| i.watermarks.get(&l.scope_lane) != Some(v));
        dim.observation = if lagging {
            Observation::Lagging
        } else {
            Observation::Fresh
        };
    }
    summarize(&dim)
}
/// Every revision reachable along L2 witnesses with its minimal root `since`,
/// then grouped by logical.
fn closure(
    i: &Inputs,
    d: &DerivedSnapshot,
    roots: &[(RevisionId, Epoch)],
) -> Result<Vec<VectorItem>, LogicalId> {
    let mut since: BTreeMap<RevisionId, Epoch> = BTreeMap::new();
    for (root, s) in roots {
        let mut frontier = vec![*root];
        let mut reached = BTreeSet::new();
        while let Some(r) = frontier.pop() {
            if !reached.insert(r) {
                continue;
            }
            if let Some(Witness::Rule(j)) = d.nodes[r.index()].witness[2] {
                for p in &i.rules[j.index()].premises {
                    frontier.push(match p.kind {
                        EdgeKind::Sem => i.heads[i.revisions[p.premise.index()].logical.index()],
                        _ => p.premise,
                    });
                }
            }
        }
        for r in reached {
            let e = since.entry(r).or_insert(*s);
            *e = (*e).min(*s);
        }
    }
    let mut by_logical: BTreeMap<LogicalId, Vec<(RevisionId, Epoch)>> = BTreeMap::new();
    for (r, s) in since {
        by_logical
            .entry(i.revisions[r.index()].logical)
            .or_default()
            .push((r, s));
    }
    if let Some((l, _)) = by_logical.iter().find(|(_, v)| v.len() > 1) {
        return Err(*l);
    }
    Ok(by_logical
        .into_iter()
        .map(|(logical, v)| VectorItem {
            logical,
            rev: v[0].0,
            since_epoch: v[0].1,
        })
        .collect())
}
fn watermarks(i: &Inputs, vector: &[VectorItem]) -> Vec<(String, Vec<u8>)> {
    let scopes: BTreeSet<&String> = vector
        .iter()
        .map(|v| &i.logicals[v.logical.index()])
        .filter(|l| l.kind == RecordKind::CodeAnchor)
        .map(|l| &l.scope_lane)
        .collect();
    scopes
        .into_iter()
        .map(|s| (s.clone(), i.watermarks.get(s).cloned().unwrap_or_default()))
        .collect()
}
/// Session-supplied fields, mirroring the engine's `IssueRequest`.
#[derive(Clone, Debug)]
pub struct CertParams {
    pub session_id: [u8; 16],
    pub principal: PrincipalId,
    pub branch: BranchId,
    pub log_checksum: ContentHash,
    pub code_commit: String,
    pub strict_no_aba: bool,
}
pub fn cert_issue_oracle(
    history: &History,
    params: &CertParams,
    entries: &[CertItem],
    live: &BTreeMap<String, Vec<u8>>,
) -> Result<Certificate, IssueFailure> {
    let i = history.last().expect("genesis state");
    let d = jacobi(i);
    let mut required: Vec<&CertItem> = entries.iter().filter(|e| e.required).collect();
    if required.is_empty() {
        return Err(IssueFailure::Empty);
    }
    required.sort_by_key(|e| (e.logical, e.rev));
    let mut seen: BTreeMap<LogicalId, RevisionId> = BTreeMap::new();
    for e in &required {
        if let Some(other) = seen.insert(e.logical, e.rev)
            && other != e.rev
        {
            return Err(IssueFailure::Ambiguous { logical: e.logical });
        }
    }
    for e in &required {
        let valid = i
            .revisions
            .get(e.rev.index())
            .is_some_and(|r| r.logical == e.logical);
        let s = if valid {
            summary(i, &d, e.rev, live)
        } else {
            Summary::Stale(StaleReason::Unsupported)
        };
        if s != Summary::Certified {
            return Err(IssueFailure::NotCertified {
                logical: e.logical,
                rev: e.rev,
                summary: s,
            });
        }
    }
    let roots: Vec<_> = required.iter().map(|e| (e.rev, e.since_epoch)).collect();
    let vector = closure(i, &d, &roots).map_err(|logical| IssueFailure::Ambiguous { logical })?;
    Ok(Certificate {
        cert_id: [0; 32],
        session_id: params.session_id,
        principal: params.principal,
        branch: params.branch,
        issued_epoch: Epoch(history.len() as u64 - 1),
        log_checksum_at_issue: params.log_checksum,
        code_commit: params.code_commit.clone(),
        watermarks: watermarks(i, &vector),
        entries: entries.to_vec(),
        vector,
        strict_no_aba: params.strict_no_aba,
    })
}
/// Did anything the strict check watches change in `(since, now]`?
fn strict_moved(history: &History, v: &VectorItem) -> bool {
    let start = v.since_epoch.0 as usize;
    (start + 1..history.len()).any(|e| {
        let (a, b) = (&history[e - 1], &history[e]);
        let l = v.logical.index();
        let head = a.heads.get(l).is_some_and(|h| b.heads.get(l) != Some(h));
        let base = a
            .base
            .get(v.rev.index())
            .is_some_and(|x| b.base.get(v.rev.index()) != Some(x));
        // A logical that did not exist yet cannot have moved its watermark scope.
        let watermark = b.logicals.get(l).is_some_and(|logical| {
            logical.kind == RecordKind::CodeAnchor
                && a.watermarks.get(&logical.scope_lane) != b.watermarks.get(&logical.scope_lane)
        });
        head || base || watermark
    })
}
pub fn cert_validate_oracle(
    history: &History,
    cert: &Certificate,
    live: &BTreeMap<String, Vec<u8>>,
    log_matches: bool,
    current_checksum: ContentHash,
) -> Validation {
    let now = history.len() as u64 - 1;
    if !log_matches || cert.issued_epoch.0 > now {
        return Validation::Invalid {
            reason: InvalidReason::LogMismatch,
            diffs: Vec::new(),
        };
    }
    let i = history.last().expect("genesis state");
    let d = jacobi(i);
    let head = |l: LogicalId| i.heads[l.index()];
    if cert.strict_no_aba {
        let diffs: Vec<CertDiff> = cert
            .vector
            .iter()
            .filter(|v| strict_moved(history, v))
            .map(|v| CertDiff {
                logical: v.logical,
                cert_rev: Some(v.rev),
                current: Some(head(v.logical)),
                summary: Some(summary(i, &d, head(v.logical), live)),
            })
            .collect();
        if !diffs.is_empty() {
            return Validation::Invalid {
                reason: InvalidReason::StrictChange,
                diffs,
            };
        }
    }
    let required: Vec<&CertItem> = cert.entries.iter().filter(|e| e.required).collect();
    if required.is_empty() {
        return Validation::Empty;
    }
    let mut results = Vec::new();
    for e in &required {
        let h = head(e.logical);
        let s = summary(i, &d, h, live);
        results.push((e, h, s));
    }
    let roots: Vec<_> = required
        .iter()
        .map(|e| (head(e.logical), e.since_epoch))
        .collect();
    let recomputed = closure(i, &d, &roots);
    if results.iter().any(|(_, _, s)| *s == Summary::Unavailable) {
        return Validation::Denied;
    }
    let invalid: Vec<CertDiff> = results
        .iter()
        .filter(|(e, h, s)| *h != e.rev || matches!(s, Summary::Stale(_)))
        .map(|(e, h, s)| CertDiff {
            logical: e.logical,
            cert_rev: Some(e.rev),
            current: Some(*h),
            summary: Some(s.clone()),
        })
        .collect();
    if !invalid.is_empty() {
        return Validation::Invalid {
            reason: InvalidReason::Changed,
            diffs: invalid,
        };
    }
    let conflicted: Vec<LogicalId> = results
        .iter()
        .filter(|(_, _, s)| *s == Summary::Conflicted)
        .map(|(e, _, _)| e.logical)
        .collect();
    if !conflicted.is_empty() {
        return Validation::Conflicted {
            entries: conflicted,
        };
    }
    let vector = match recomputed {
        Err(logical) => return Validation::Ambiguous { logical },
        Ok(v) => v,
    };
    let mut lag: BTreeSet<String> = BTreeSet::new();
    for (e, _, s) in &results {
        if *s == Summary::Pending(PendingReason::ObservationLag) {
            lag.insert(i.logicals[e.logical.index()].scope_lane.clone());
        }
    }
    for v in &vector {
        let l = &i.logicals[v.logical.index()];
        if l.kind == RecordKind::CodeAnchor
            && summary(i, &d, v.rev, live) == Summary::Pending(PendingReason::ObservationLag)
        {
            lag.insert(l.scope_lane.clone());
        }
    }
    if !lag.is_empty() {
        return Validation::Unknown {
            lag: lag.into_iter().collect(),
        };
    }
    let pending: Vec<_> = results
        .iter()
        .filter(|(_, _, s)| matches!(s, Summary::Pending(_)))
        .collect();
    if !pending.is_empty() {
        let rules: BTreeSet<RuleId> = pending
            .iter()
            .flat_map(|(_, h, _)| {
                i.rules
                    .iter()
                    .enumerate()
                    .filter(|(n, j)| j.conclusion == *h && d.sem[*n] == SemState::Pending)
                    .map(|(n, _)| RuleId(n as u32))
            })
            .collect();
        return Validation::Pending {
            rules: rules.into_iter().collect(),
        };
    }
    let old: BTreeMap<_, _> = cert.vector.iter().map(|v| (v.logical, v.rev)).collect();
    let new: BTreeMap<_, _> = vector.iter().map(|v| (v.logical, v.rev)).collect();
    if old == new {
        return Validation::Valid;
    }
    let all: BTreeSet<LogicalId> = old.keys().chain(new.keys()).copied().collect();
    let diffs: Vec<CertDiff> = all
        .into_iter()
        .filter(|l| old.get(l) != new.get(l))
        .map(|l| CertDiff {
            logical: l,
            cert_rev: old.get(&l).copied(),
            current: new.get(&l).copied(),
            summary: new.get(&l).map(|r| summary(i, &d, *r, live)),
        })
        .collect();
    let changed: BTreeSet<LogicalId> = diffs.iter().map(|x| x.logical).collect();
    let mut new_cert = cert.clone();
    new_cert.cert_id = [0; 32];
    new_cert.issued_epoch = Epoch(now);
    new_cert.log_checksum_at_issue = current_checksum;
    new_cert.watermarks = watermarks(i, &vector);
    for e in new_cert.entries.iter_mut().filter(|e| e.required) {
        let own = closure(i, &d, &[(e.rev, e.since_epoch)]).expect("sub-closure");
        if own.iter().any(|v| changed.contains(&v.logical)) {
            e.via = Via::Alternative;
        }
    }
    new_cert.vector = vector;
    Validation::ValidRecertified {
        new_cert: Box::new(new_cert),
        diffs,
    }
}
