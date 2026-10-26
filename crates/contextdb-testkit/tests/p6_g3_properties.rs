//! P6 G3 properties per branch (ADR 0013 §11, SPEC §8.2).
//!
//! - PROP7 for the branch Ops: batches on disjoint resources commute, under
//!   the comparison rule (local ids, epochs excluded). `CreateBranch` reads
//!   the parent and writes the branch table; `Merge` reads both branches and
//!   writes the target and the sync point; `ExpectHeads` reads the listed
//!   heads, so it does not commute with a move of one of them.
//! - PROP11 per branch: certificates issued and validated on a branch equal
//!   the oracle's recomputation over that branch's history at global epochs.
//!   The oracle sees the branch's local ids; the test maps them to global
//!   ids itself.
#[path = "common/branch_gen.rs"]
mod branch_gen;
use branch_gen::*;
use contextdb_core::{IssueRequest, Kernel, LogLink};
use contextdb_model::*;
use contextdb_oracle::{CertParams, cert_issue_oracle, cert_validate_oracle, jacobi};
use std::collections::{BTreeMap, HashMap};

fn seeds(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
fn fork(k: &Kernel) -> Kernel {
    Kernel::import(k.export()).unwrap()
}
/// A branch's state under the PROP7 comparison rule: its inputs in local
/// ids with every epoch zeroed, and the derived state recomputed.
fn local_state(k: &Kernel, b: BranchId) -> (String, Option<BranchId>, bool, Inputs, String) {
    let view = k.branch(b).unwrap();
    let mut i = view.db().inputs();
    for l in &mut i.logicals {
        l.origin_epoch = Epoch(0);
    }
    for r in &mut i.revisions {
        r.origin_epoch = Epoch(0);
    }
    for j in &mut i.rules {
        j.origin_epoch = Epoch(0);
    }
    for a in &mut i.artifacts {
        a.origin_epoch = Epoch(0);
        // A merge Conflict's canonical payload starts with the merge epoch
        // (ADR 0013 §6.2); the generator's own artifacts use [7; 32].
        if a.payload_hash != [7; 32] {
            a.payload_hash = [0; 32];
        }
    }
    let derived = format!("{:?}", jacobi(&i));
    let info = view.info();
    (info.name.clone(), info.parent, info.archived, i, derived)
}
fn states(k: &Kernel) -> Vec<(String, Option<BranchId>, bool, Inputs, String)> {
    k.branches().map(|b| local_state(k, b.id)).collect()
}
/// Commit outcomes compared without the ids a batch allocates, which
/// depend on the order.
fn outcome(r: &Result<CommitResult, TxError>) -> Result<(), TxError> {
    r.as_ref().map(|_| ()).map_err(Clone::clone)
}
/// The heads of 1–3 logicals of `b`, in global ids.
fn expect_heads(g: &mut Gen, k: &Kernel, b: BranchId) -> Option<(Op, Vec<LogicalId>)> {
    let view = k.branch(b).unwrap();
    let ls: Vec<LogicalId> = view.logicals().collect();
    if ls.is_empty() {
        return None;
    }
    let mut picked: Vec<LogicalId> = (0..1 + g.pick(3)).map(|_| ls[g.pick(ls.len())]).collect();
    picked.sort();
    picked.dedup();
    let entries = picked
        .iter()
        .map(|l| (LogicalRef::Existing(*l), view.head(*l).unwrap()))
        .collect();
    Some((
        Op::ExpectHeads {
            entries,
            source: ReadSource::Client,
        },
        picked,
    ))
}

#[test]
fn prop7_branch_ops_commute_on_disjoint_resources() {
    let mut cases = [0u32; 5];
    for seed in 0..seeds("CONTEXTDB_P6_PROP7_SEEDS", 10_000) {
        let mut g = Gen::new(seed);
        let mut k = Kernel::default();
        for _ in 0..20 + g.pick(20) {
            let b = g.step(&k);
            let _ = k.commit(&b);
        }
        let n = k.branches().count();
        if n < 2 {
            continue;
        }
        let a = g.branch(&k);
        let other = |g: &mut Gen, not: &[BranchId]| {
            let rest: Vec<BranchId> = (0..n as u16)
                .map(BranchId)
                .filter(|b| !not.contains(b))
                .collect();
            (!rest.is_empty()).then(|| rest[g.pick(rest.len())])
        };
        let Some(b) = other(&mut g, &[a]) else {
            continue;
        };
        let case = g.pick(5);
        let (x, y) = match case {
            // Commits on two branches.
            0 => (g.batch(&k, a), g.batch(&k, b)),
            // A fork of `a` and a commit on `b`.
            1 => (g.create_branch(a), g.batch(&k, b)),
            // A merge b ← a and a commit on a third branch.
            2 => {
                let Some(u) = other(&mut g, &[a, b]) else {
                    continue;
                };
                (g.merge(b, a), g.batch(&k, u))
            }
            // ExpectHeads on `a` with writes there, and a commit on `b`
            // (its heads are other resources).
            3 => {
                let Some((expect, _)) = expect_heads(&mut g, &k, a) else {
                    continue;
                };
                let mut x = g.batch(&k, a);
                x.ops.insert(0, expect);
                (x, g.batch(&k, b))
            }
            // Observer batches on two branches (watermarks are per branch).
            _ => (g.observer(&k, a), g.observer(&k, b)),
        };
        // Keys are one namespace across branches and revisions are deduped
        // by global (logical, content) (ADR 0013 §2): two creations of one
        // key or one revision share that allocator (the first creator is
        // the origin branch).
        let keys = |b: &Batch| -> Vec<String> {
            (b.ops.iter())
                .filter_map(|op| match op {
                    Op::CreateLogical { key, .. } => Some(key.clone()),
                    Op::AddRevision {
                        logical: LogicalRef::Existing(l),
                        content,
                        ..
                    } => Some(format!("{l:?}/{content:?}")),
                    _ => None,
                })
                .collect()
        };
        if keys(&x).iter().any(|key| keys(&y).contains(key)) {
            continue;
        }
        let (mut k1, mut k2) = (fork(&k), fork(&k));
        let (x1, y1) = (k1.commit(&x), k1.commit(&y));
        let (y2, x2) = (k2.commit(&y), k2.commit(&x));
        assert_eq!(
            (outcome(&x1), outcome(&y1)),
            (outcome(&x2), outcome(&y2)),
            "seed {seed} case {case}: outcomes\n{x:?}\n{y:?}"
        );
        assert_eq!(
            states(&k1),
            states(&k2),
            "seed {seed} case {case}: states\n{x:?}\n{y:?}"
        );
        if x1.is_ok() && y1.is_ok() {
            cases[case] += 1;
        }
    }
    eprintln!("PROP7 branch cases with both batches committed: {cases:?}");
    assert!(cases.iter().all(|&c| c >= 50), "{cases:?}");
}

/// The read set is real: `ExpectHeads(l)` and a move of `l` on the same
/// branch do not commute (one order rejects the read).
#[test]
fn prop7_expect_heads_reads_the_listed_heads() {
    let (mut distinguished, n) = (0, seeds("CONTEXTDB_P6_PROP7_SEEDS", 10_000) / 4);
    for seed in 0..n {
        let mut g = Gen::new(seed ^ 0x77);
        let mut k = Kernel::default();
        for _ in 0..20 + g.pick(20) {
            let b = g.step(&k);
            let _ = k.commit(&b);
        }
        let a = g.branch(&k);
        let Some((expect, picked)) = expect_heads(&mut g, &k, a) else {
            continue;
        };
        let view = k.branch(a).unwrap();
        let l = picked[0];
        let head = view.head(l).unwrap();
        let Some(other) = (0..view.revision_count() as u32)
            .map(|n| view.global_revision(RevisionId(n)))
            .find(|r| *r != head && view.revision_logical(*r) == Some(l))
        else {
            continue;
        };
        let x = on_branch(a, vec![expect]);
        let y = on_branch(
            a,
            vec![Op::SetHead {
                logical: l.into(),
                expected: head.into(),
                new: other.into(),
            }],
        );
        let (mut k1, mut k2) = (fork(&k), fork(&k));
        let (x1, y1) = (k1.commit(&x), k1.commit(&y));
        let (y2, x2) = (k2.commit(&y), k2.commit(&x));
        assert!(x1.is_ok(), "seed {seed}: {x1:?}");
        if y1.is_ok() {
            assert!(y2.is_ok());
            assert!(
                matches!(x2, Err(TxError::ReadSetConflict { logical, read, now })
                    if logical == l && read == head && now == other),
                "seed {seed}: {x2:?}"
            );
            distinguished += 1;
        }
    }
    eprintln!("ExpectHeads/SetHead pairs distinguished: {distinguished} of {n} seeds");
    assert!(distinguished * 4 >= n, "{distinguished} of {n}");
}

/// Local ⇄ global ids of one branch at one moment.
struct Ids {
    l: Vec<LogicalId>,
    r: Vec<RevisionId>,
    j: Vec<RuleId>,
    gl: HashMap<LogicalId, LogicalId>,
    gr: HashMap<RevisionId, RevisionId>,
}
impl Ids {
    fn of(k: &Kernel, b: BranchId) -> Self {
        let view = k.branch(b).unwrap();
        let i = view.db().inputs();
        let l: Vec<_> = (0..i.logicals.len())
            .map(|n| view.global_logical(LogicalId(n as u32)))
            .collect();
        let r: Vec<_> = (0..i.revisions.len())
            .map(|n| view.global_revision(RevisionId(n as u32)))
            .collect();
        let j = (0..i.rules.len())
            .map(|n| view.global_rule(RuleId(n as u32)))
            .collect();
        let gl = l
            .iter()
            .enumerate()
            .map(|(n, g)| (*g, LogicalId(n as u32)))
            .collect();
        let gr = r
            .iter()
            .enumerate()
            .map(|(n, g)| (*g, RevisionId(n as u32)))
            .collect();
        Ids { l, r, j, gl, gr }
    }
    fn local(&self, logical: LogicalId, rev: RevisionId) -> Option<(LogicalId, RevisionId)> {
        Some((*self.gl.get(&logical)?, *self.gr.get(&rev)?))
    }
    fn summary(&self, s: Summary) -> Summary {
        match s {
            Summary::Stale(StaleReason::SupersededPinnedPremise(r)) => {
                Summary::Stale(StaleReason::SupersededPinnedPremise(self.r[r.index()]))
            }
            other => other,
        }
    }
    fn diff(&self, d: CertDiff) -> CertDiff {
        CertDiff {
            logical: self.l[d.logical.index()],
            cert_rev: d.cert_rev.map(|r| self.r[r.index()]),
            current: d.current.map(|r| self.r[r.index()]),
            summary: d.summary.map(|s| self.summary(s)),
        }
    }
    fn cert(&self, mut c: Certificate) -> Certificate {
        for e in &mut c.entries {
            e.logical = self.l[e.logical.index()];
            e.rev = self.r[e.rev.index()];
        }
        for v in &mut c.vector {
            v.logical = self.l[v.logical.index()];
            v.rev = self.r[v.rev.index()];
        }
        c.vector.sort();
        c
    }
}
fn zero_id(mut c: Certificate) -> Certificate {
    c.cert_id = [0; 32];
    c
}
/// The branch rule for advisory entries the branch cannot see (ADR 0013
/// §7): required ones fail, advisory ones are left out. Returns the local
/// entries and the kept positions.
fn visible_entries(
    ids: &Ids,
    entries: &[CertItem],
) -> Result<(Vec<CertItem>, Vec<usize>), CertItem> {
    let (mut local, mut kept) = (Vec::new(), Vec::new());
    for (n, e) in entries.iter().enumerate() {
        match ids.local(e.logical, e.rev) {
            Some((logical, rev)) => {
                local.push(CertItem { logical, rev, ..*e });
                kept.push(n);
            }
            None if !e.required => {}
            None => return Err(e.clone()),
        }
    }
    Ok((local, kept))
}
fn oracle_issue(
    ids: &Ids,
    history: &[Inputs],
    params: &CertParams,
    entries: &[CertItem],
    live: &BTreeMap<String, Vec<u8>>,
) -> Result<Certificate, IssueFailure> {
    let (local, _) = visible_entries(ids, entries).map_err(|e| IssueFailure::NotCertified {
        logical: e.logical,
        rev: e.rev,
        summary: Summary::Stale(StaleReason::Unsupported),
    })?;
    match cert_issue_oracle(history, params, &local, live) {
        Ok(c) => {
            let mut c = ids.cert(c);
            c.entries = entries.to_vec();
            Ok(c)
        }
        Err(IssueFailure::NotCertified {
            logical,
            rev,
            summary,
        }) => Err(IssueFailure::NotCertified {
            logical: ids.l[logical.index()],
            rev: ids.r[rev.index()],
            summary: ids.summary(summary),
        }),
        Err(IssueFailure::Ambiguous { logical }) => Err(IssueFailure::Ambiguous {
            logical: ids.l[logical.index()],
        }),
        Err(IssueFailure::Empty) => Err(IssueFailure::Empty),
    }
}
fn oracle_validate(
    ids: &Ids,
    history: &[Inputs],
    cert: &Certificate,
    live: &BTreeMap<String, Vec<u8>>,
    matches: bool,
    current: ContentHash,
) -> Validation {
    let (entries, kept) = match visible_entries(ids, &cert.entries) {
        Ok(v) => v,
        Err(e) => {
            return Validation::Invalid {
                reason: InvalidReason::Changed,
                diffs: vec![CertDiff {
                    logical: e.logical,
                    cert_rev: Some(e.rev),
                    current: None,
                    summary: None,
                }],
            };
        }
    };
    let mut local = cert.clone();
    local.entries = entries;
    // Vector revisions stay visible: visibility only grows on a branch.
    local.vector = (cert.vector.iter())
        .map(|v| {
            let (logical, rev) = ids.local(v.logical, v.rev).expect("still visible");
            VectorItem { logical, rev, ..*v }
        })
        .collect();
    local.vector.sort();
    match cert_validate_oracle(history, &local, live, matches, current) {
        Validation::Invalid { reason, diffs } => Validation::Invalid {
            reason,
            diffs: diffs.into_iter().map(|d| ids.diff(d)).collect(),
        },
        Validation::Conflicted { entries } => Validation::Conflicted {
            entries: entries.into_iter().map(|l| ids.l[l.index()]).collect(),
        },
        Validation::Ambiguous { logical } => Validation::Ambiguous {
            logical: ids.l[logical.index()],
        },
        Validation::Pending { rules } => Validation::Pending {
            rules: rules.into_iter().map(|j| ids.j[j.index()]).collect(),
        },
        Validation::ValidRecertified { new_cert, diffs } => {
            let mut entries = cert.entries.clone();
            for (n, e) in kept.iter().zip(&new_cert.entries) {
                entries[*n].via = e.via;
            }
            let mut new_cert = ids.cert(*new_cert);
            new_cert.entries = entries;
            Validation::ValidRecertified {
                new_cert: Box::new(new_cert),
                diffs: diffs.into_iter().map(|d| ids.diff(d)).collect(),
            }
        }
        other => other,
    }
}
fn normalize(v: Validation) -> Validation {
    match v {
        Validation::ValidRecertified { new_cert, diffs } => Validation::ValidRecertified {
            new_cert: Box::new(zero_id(*new_cert)),
            diffs,
        },
        other => other,
    }
}
fn kind(v: &Validation) -> usize {
    match v {
        Validation::Denied => 0,
        Validation::Invalid {
            reason: InvalidReason::Changed,
            ..
        } => 1,
        Validation::Invalid {
            reason: InvalidReason::StrictChange,
            ..
        } => 2,
        Validation::Invalid {
            reason: InvalidReason::LogMismatch,
            ..
        } => 3,
        Validation::Invalid {
            reason: InvalidReason::BranchMismatch,
            ..
        } => 4,
        Validation::Conflicted { .. } => 5,
        Validation::Ambiguous { .. } => 6,
        Validation::Unknown { .. } => 7,
        Validation::Pending { .. } => 8,
        Validation::Empty => 9,
        Validation::Valid => 10,
        Validation::ValidRecertified { .. } => 11,
    }
}
fn live_pos(g: &mut Gen, k: &Kernel, b: BranchId) -> BTreeMap<String, Vec<u8>> {
    match g.pick(3) {
        0 => BTreeMap::new(),
        1 => (k.branch(b).unwrap().watermark(SCOPE))
            .map(|p| BTreeMap::from([(SCOPE.to_owned(), p.to_vec())]))
            .unwrap_or_default(),
        _ => BTreeMap::from([(SCOPE.to_owned(), vec![9])]),
    }
}
/// 1–3 reads on `b`, mostly heads; sometimes an object of another branch,
/// which `b` may not see.
fn reads(g: &mut Gen, k: &Kernel, b: BranchId) -> Vec<CertItem> {
    let now = k.epoch();
    (0..1 + g.pick(3))
        .filter_map(|_| {
            let from = if g.coin(10) { g.branch(k) } else { b };
            let view = k.branch(from).unwrap();
            let ls: Vec<LogicalId> = view.logicals().collect();
            if ls.is_empty() {
                return None;
            }
            let l = ls[g.pick(ls.len())];
            let revs: Vec<RevisionId> = (0..view.revision_count() as u32)
                .map(|n| view.global_revision(RevisionId(n)))
                .filter(|r| view.revision_logical(*r) == Some(l))
                .collect();
            let rev = if g.coin(25) {
                revs[g.pick(revs.len())]
            } else {
                view.head(l).unwrap()
            };
            Some(CertItem {
                logical: l,
                rev,
                required: !g.coin(20),
                via: Via::Head,
                since_epoch: Epoch(g.next() % (now.0 + 1)),
            })
        })
        .collect()
}

#[test]
fn prop11_certificates_per_branch_equal_the_oracle() {
    let mut verdicts = [0u64; 12];
    let mut issued = [0u64; 4]; // ok, not-certified, ambiguous, empty
    for seed in 0..seeds("CONTEXTDB_P6_PROP11_SEEDS", 10_000) {
        let mut g = Gen::new(seed ^ 0x11);
        let mut k = Kernel::default();
        // Each branch's inputs (local ids) at every global epoch; a branch
        // starts with its parent's history up to the fork.
        let mut history: Vec<Vec<Inputs>> = vec![vec![k.branch(MAIN).unwrap().db().inputs()]];
        let mut certs: Vec<Certificate> = Vec::new();
        for step in 0..40 {
            let batch = match g.pick(4) {
                0 => {
                    // Link a head to the anchor so anchors enter closures.
                    let b = g.branch(&k);
                    let view = k.branch(b).unwrap();
                    match (view.logical_by_key(ANCHOR), view.logicals().next()) {
                        (Some(a), Some(t)) if t != a => on_branch(
                            b,
                            vec![Op::AddRule {
                                conclusion: view.head(t).unwrap().into(),
                                incidences: vec![(
                                    view.head(a).unwrap().into(),
                                    [EdgeKind::Cur, EdgeKind::Sem][g.pick(2)],
                                )],
                                enabled: true,
                            }],
                        ),
                        _ => g.step(&k),
                    }
                }
                _ => g.step(&k),
            };
            let before = k.epoch();
            if k.commit(&batch).is_ok() && k.epoch() != before {
                if let [Op::CreateBranch { .. }] = batch.ops[..] {
                    history.push(history[batch.branch.0 as usize].clone());
                }
                for (n, h) in history.iter_mut().enumerate() {
                    h.push(k.branch(BranchId(n as u16)).unwrap().db().inputs());
                }
            }
            for h in &history {
                assert_eq!(h.len() as u64 - 1, k.epoch().0);
            }
            let b = g.branch(&k);
            let ids = Ids::of(&k, b);
            let view = k.branch(b).unwrap();
            if g.coin(35) {
                let entries = reads(&mut g, &k, b);
                let request = IssueRequest {
                    session_id: [seed as u8; 16],
                    principal: PrincipalId(1),
                    branch: b,
                    log_checksum: [k.epoch().0 as u8; 32],
                    code_commit: "abc".into(),
                    strict_no_aba: g.coin(50),
                };
                let params = CertParams {
                    session_id: request.session_id,
                    principal: request.principal,
                    branch: b,
                    log_checksum: request.log_checksum,
                    code_commit: request.code_commit.clone(),
                    strict_no_aba: request.strict_no_aba,
                };
                let live = live_pos(&mut g, &k, b);
                let engine = view.issue_certificate(&request, &entries, &live);
                let oracle = oracle_issue(&ids, &history[b.0 as usize], &params, &entries, &live);
                assert_eq!(
                    engine.clone().map(zero_id),
                    oracle,
                    "seed {seed} step {step}: issuance on {b:?}"
                );
                issued[match &engine {
                    Ok(_) => 0,
                    Err(IssueFailure::NotCertified { .. }) => 1,
                    Err(IssueFailure::Ambiguous { .. }) => 2,
                    Err(IssueFailure::Empty) => 3,
                }] += 1;
                if let Ok(c) = engine {
                    assert_eq!(c.cert_id, *blake3::hash(&c.body_bytes()).as_bytes());
                    certs.push(c);
                }
            }
            // Validate every certificate on the branch the step picked:
            // its own branch recomputes, any other is a branch mismatch.
            for c in &certs {
                let live = live_pos(&mut g, &k, b);
                let matches = !g.coin(5);
                let current = [k.epoch().0 as u8; 32];
                let engine = view
                    .validate_certificate(
                        c,
                        &live,
                        LogLink {
                            matches,
                            current_checksum: current,
                        },
                    )
                    .expect("history from genesis");
                let oracle = if c.branch == b {
                    oracle_validate(&ids, &history[b.0 as usize], c, &live, matches, current)
                } else {
                    Validation::Invalid {
                        reason: InvalidReason::BranchMismatch,
                        diffs: Vec::new(),
                    }
                };
                verdicts[kind(&engine)] += 1;
                if let Validation::ValidRecertified { new_cert, .. } = &engine {
                    assert_eq!(
                        new_cert.cert_id,
                        *blake3::hash(&new_cert.body_bytes()).as_bytes()
                    );
                }
                assert_eq!(
                    normalize(engine),
                    normalize(oracle),
                    "seed {seed} step {step}: validation on {b:?} of a cert issued on {:?} at {:?}",
                    c.branch,
                    c.issued_epoch
                );
            }
        }
    }
    eprintln!(
        "PROP11 per branch: issue [ok,not-certified,ambiguous,empty]={issued:?} \
         verdicts [denied,changed,strict,log,branch,conflicted,ambiguous,unknown,pending,empty,valid,recert]={verdicts:?}"
    );
    for (n, name) in [
        (1, "Changed"),
        (2, "StrictChange"),
        (3, "LogMismatch"),
        (4, "BranchMismatch"),
        (7, "Unknown"),
        (10, "Valid"),
        (11, "ValidRecertified"),
    ] {
        assert!(verdicts[n] > 0, "never {name}: {verdicts:?}");
    }
    assert!(issued[0] > 0 && issued[1] > 0, "{issued:?}");
}
