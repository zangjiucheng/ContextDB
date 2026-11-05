//! `ctx_conflicts` and `ctx_resolve` (ADR 0015 §2, §6; SD20). All three act
//! on the caller's branch: the bound session's, or for an unbound admin
//! call the named branch, else `main`.
//!
//! - **list** returns open Conflicts grouped into their pairs (marked `side
//!   superseded` when a side is no longer the head), open PendingReviews
//!   (marked `stale curvec` when the rule's vector moved on) and lexical
//!   candidate pairs not yet acted on, both sides still the branch's heads.
//! - **open** commits the pair (or one side) in one batch with one shared
//!   `ConflictDoc`; `from = review` resolves that review in the same batch,
//!   and `from = candidate` marks the candidate opened once committed.
//! - **resolve** closes Conflicts and PendingReviews only, with a
//!   `ResolutionDoc` note; `AnchorBroken` stays observer-only.
//!
//! Agents and admin may do all three (D3); writes are ordinary certified
//! writes (`expect_cert_valid` for bound callers).
use crate::{
    actor::Snapshot,
    service::{Bound, Identity, Service, err},
};
use contextdb_core::BranchView;
use contextdb_model::*;
use contextdb_proto::docs::{
    ConflictDoc, ConflictSource, ResolutionDoc, ReviewDoc, Suggestion, ValidationMeta,
};
use contextdb_proto::proto::*;
use contextdb_store::{Durability, Payload};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Most items one list returns.
pub const MAX_LIST: u32 = 1_000;
/// Default list size.
pub const DEFAULT_LIST: u32 = 50;

fn doc<D: Durability, T: Wire>(snap: &Snapshot<D>, hash: &ContentHash) -> Option<T> {
    match snap.log.read_blob(hash) {
        Payload::Available(b) => T::from_bytes(&b).ok(),
        _ => None,
    }
}
fn side(view: &BranchView<'_>, r: RevisionId) -> ConflictSide {
    let l = view.revision_logical(r);
    ConflictSide {
        rev: r,
        key: (l.and_then(|l| view.logical_info(l)))
            .map(|(k, _, _)| k)
            .unwrap_or_default(),
        current: l.is_some_and(|l| view.head(l) == Some(r)),
    }
}
fn in_scope(view: &BranchView<'_>, sides: &[RevisionId], scope: Option<&str>) -> bool {
    let Some(scope) = scope else { return true };
    sides.iter().any(|r| {
        (view.revision_logical(*r))
            .and_then(|l| view.logical_info(l))
            .is_some_and(|(_, _, s)| s == scope)
    })
}
fn source_name(s: &ConflictSource) -> String {
    match s {
        ConflictSource::Agent => "agent".into(),
        ConflictSource::Admin => "admin".into(),
        ConflictSource::Candidate(id) => format!("candidate {id}"),
        ConflictSource::Review(a) => format!("review {}", a.0),
    }
}
fn suggestion_name(s: Suggestion) -> &'static str {
    match s {
        Suggestion::Verify => "verify",
        Suggestion::Reject => "reject",
        Suggestion::Investigate => "investigate",
    }
}
/// Artifacts sharing one payload: its decoded doc, ids and targets.
type Group = (Option<ConflictDoc>, Vec<ArtifactId>, Vec<RevisionId>);
/// Open Conflicts on the branch, grouped by their shared `ConflictDoc`.
fn conflicts<D: Durability>(
    snap: &Snapshot<D>,
    view: &BranchView<'_>,
    scope: Option<&str>,
) -> Vec<ConflictItem> {
    let mut docs: BTreeMap<ContentHash, Group> = BTreeMap::new();
    let mut out = Vec::new();
    for (id, a) in view.artifacts() {
        if !a.open || a.ty != ArtifactType::Conflict {
            continue;
        }
        let entry = docs.entry(a.payload_hash).or_insert_with(|| {
            (
                doc::<D, ConflictDoc>(snap, &a.payload_hash),
                Vec::new(),
                Vec::new(),
            )
        });
        entry.1.push(id);
        entry.2.push(a.target);
    }
    for (_, (d, ids, targets)) in docs {
        match d {
            Some(d) => {
                if !in_scope(view, &d.sides, scope) {
                    continue;
                }
                let sides: Vec<ConflictSide> = d.sides.iter().map(|r| side(view, *r)).collect();
                let marks = if sides.iter().any(|s| !s.current) {
                    vec!["side superseded".into()]
                } else {
                    Vec::new()
                };
                out.push(ConflictItem {
                    kind: "conflict".into(),
                    artifacts: ids,
                    sides,
                    reason: d.reason,
                    origin: source_name(&d.source),
                    rule: None,
                    marks,
                    evidence: d.evidence,
                    candidate: None,
                });
            }
            // A Conflict without a ConflictDoc (a merge conflict, P6): one
            // item per artifact.
            None => {
                for (id, t) in ids.into_iter().zip(targets) {
                    if !in_scope(view, &[t], scope) {
                        continue;
                    }
                    let s = side(view, t);
                    let marks = if s.current {
                        Vec::new()
                    } else {
                        vec!["side superseded".into()]
                    };
                    out.push(ConflictItem {
                        kind: "conflict".into(),
                        artifacts: vec![id],
                        sides: vec![s],
                        reason: "merge conflict".into(),
                        origin: "merge".into(),
                        rule: None,
                        marks,
                        evidence: Vec::new(),
                        candidate: None,
                    });
                }
            }
        }
    }
    out.sort_by_key(|i| i.artifacts.first().copied());
    out
}
/// Open PendingReviews on the branch.
fn reviews<D: Durability>(
    snap: &Snapshot<D>,
    view: &BranchView<'_>,
    scope: Option<&str>,
) -> Vec<ConflictItem> {
    let mut out = Vec::new();
    for (id, a) in view.artifacts() {
        if !a.open || a.ty != ArtifactType::PendingReview {
            continue;
        }
        let review = doc::<D, ReviewDoc>(snap, &a.payload_hash);
        let mut sides = vec![a.target];
        if let Some(r) = &review {
            sides.extend(r.sides.iter().copied().filter(|s| *s != a.target));
        }
        if !in_scope(view, &sides, scope) {
            continue;
        }
        let mut item = ConflictItem {
            kind: "review".into(),
            artifacts: vec![id],
            sides: sides.iter().map(|r| side(view, *r)).collect(),
            reason: String::new(),
            origin: "review".into(),
            rule: None,
            marks: Vec::new(),
            evidence: Vec::new(),
            candidate: None,
        };
        if let Some(r) = review {
            let meta = doc::<D, ValidationMeta>(snap, &r.meta);
            item.reason = suggestion_name(r.suggestion).into();
            if let Some(m) = &meta {
                item.reason = format!("{}: {}", item.reason, m.rationale);
                item.origin = match &m.model {
                    Some(model) => format!("{} ({model})", m.validator),
                    None => m.validator.clone(),
                };
                item.evidence = m.evidence.clone();
            }
            item.rule = r.rule;
            if let Some(j) = r.rule {
                let moved = view.rule_semantic(j).is_none_or(|s| s.current != r.curvec);
                if moved {
                    item.marks.push("stale curvec".into());
                }
            }
        }
        out.push(item);
    }
    out
}
/// Whether an open Conflict on the branch already names both revisions.
fn pair_open(view: &BranchView<'_>, a: RevisionId, b: RevisionId) -> bool {
    let mut by_payload: BTreeMap<ContentHash, Vec<RevisionId>> = BTreeMap::new();
    for (_, x) in view.artifacts() {
        if x.open && x.ty == ArtifactType::Conflict {
            by_payload.entry(x.payload_hash).or_default().push(x.target);
        }
    }
    by_payload
        .values()
        .any(|t| t.contains(&a) && t.contains(&b))
}
impl<D: Durability + Clone + 'static> Service<D> {
    /// Candidate pairs on the branch not yet acted on. A pair a side of
    /// which is no longer the head is marked `gone`; one already open as a
    /// Conflict is marked `opened`. Neither is listed.
    fn candidate_items(&self, view: &BranchView<'_>, scope: Option<&str>) -> Vec<ConflictItem> {
        use crate::candidates::{CandState, Judge, current};
        let mut out = Vec::new();
        let listed = self
            .candidates
            .listed(Some(view.info().id))
            .unwrap_or_default();
        for c in listed {
            if !current(Some(view), &c) {
                let _ = self.candidates.set_state(c.id, CandState::Gone);
                continue;
            }
            if pair_open(view, c.a, c.b) {
                let _ = self.candidates.set_state(c.id, CandState::Opened);
                continue;
            }
            if !in_scope(view, &[c.a, c.b], scope) {
                continue;
            }
            // A validator's judgement, when one judged the pair (§5).
            let mut marks = Vec::new();
            if let (Judge::Done, Some(label)) = (c.judge, &c.label) {
                let by = c.validator.as_deref().unwrap_or("validator");
                marks.push(match c.review {
                    Some(r) => format!("judged {label} by {by} (review {})", r.0),
                    None => format!("judged {label} by {by}"),
                });
            }
            out.push(ConflictItem {
                kind: "candidate".into(),
                artifacts: Vec::new(),
                sides: vec![side(view, c.a), side(view, c.b)],
                reason: candidate_reason(&c),
                origin: format!("candidate {}", c.id),
                rule: None,
                marks,
                evidence: Vec::new(),
                candidate: Some(c.id),
            });
        }
        out
    }
    /// The branch a conflicts request acts on, with the session when bound.
    #[allow(clippy::result_large_err)]
    fn conflict_branch(
        &self,
        id: Identity,
        binding: &Option<Binding>,
        branch: &Option<String>,
        snap: &Arc<Snapshot<D>>,
    ) -> Result<(BranchId, Option<Bound>), Response> {
        match binding {
            Some(b) => {
                let bound = self.bind(id, b, snap)?;
                let mine = bound.1.lock().expect("session").branch;
                if let Some(name) = branch
                    && snap.kernel.branch_by_name(name) != Some(mine)
                {
                    return Err(err(
                        "BranchMismatch",
                        format!("the session is bound to another branch than {name}"),
                    ));
                }
                Ok((mine, Some(bound)))
            }
            None if id.channel == Channel::Agent => Err(err(
                "InvalidArgument",
                "agents act on conflicts through a session binding",
            )),
            None => match branch {
                Some(name) => {
                    crate::service::resolve_branch(snap, Some(name), None).map(|b| (b, None))
                }
                None => Ok((MAIN, None)),
            },
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn conflicts_list(
        &self,
        id: Identity,
        binding: Option<Binding>,
        branch: Option<String>,
        scope: Option<String>,
        state: ConflictState,
        limit: u32,
        snap: Arc<Snapshot<D>>,
    ) -> Response {
        let b = match self.conflict_branch(id, &binding, &branch, &snap) {
            Ok((b, _)) => b,
            Err(r) => return r,
        };
        let Some(view) = snap.kernel.branch(b) else {
            return err("UnknownBranch", "no such branch");
        };
        let scope = scope.as_deref();
        let mut items = Vec::new();
        if matches!(state, ConflictState::Open | ConflictState::All) {
            items.extend(conflicts(&snap, &view, scope));
        }
        if matches!(state, ConflictState::Review | ConflictState::All) {
            items.extend(reviews(&snap, &view, scope));
        }
        if matches!(state, ConflictState::Candidates | ConflictState::All) {
            items.extend(self.candidate_items(&view, scope));
        }
        let limit = match limit {
            0 => DEFAULT_LIST,
            n => n.min(MAX_LIST),
        } as usize;
        let truncated = items.len() > limit;
        items.truncate(limit);
        Response::Conflicts { items, truncated }
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn conflict_open(
        &self,
        id: Identity,
        binding: Option<Binding>,
        branch: Option<String>,
        a: Target,
        b: Option<Target>,
        reason: String,
        evidence: Vec<String>,
        from: Option<ConflictFrom>,
        expect_cert_valid: bool,
        client_txn_id: Option<[u8; 16]>,
        snap: Arc<Snapshot<D>>,
    ) -> Response {
        let (branch, bound) = match self.conflict_branch(id, &binding, &branch, &snap) {
            Ok(x) => x,
            Err(r) => return r,
        };
        let Some(view) = snap.kernel.branch(branch) else {
            return err("UnknownBranch", "no such branch");
        };
        if reason.trim().is_empty() {
            return err("InvalidArgument", "a Conflict needs a reason");
        }
        // Sides must be visible on the branch (resolution is on its view).
        let side_a = match self.resolve_plain(&a, &view) {
            Ok((_, r)) => r,
            Err(r) => return r,
        };
        let mut side_b = match &b {
            Some(t) => match self.resolve_plain(t, &view) {
                Ok((_, r)) => Some(r),
                Err(r) => return r,
            },
            None => None,
        };
        // A candidate names the pair: `a` must be one of its sides, and `b`
        // (when given) the other; an omitted `b` is filled in.
        if let Some(ConflictFrom::Candidate(n)) = from {
            use crate::candidates::{CandState, current};
            let c = match self.candidates.get(n) {
                Ok(Some(c)) if c.branch == branch => c,
                Ok(_) => return err("UnknownRef", format!("no candidate {n} on this branch")),
                Err(e) => return err("Internal", format!("candidates: {e}")),
            };
            let gone = || {
                err(
                    "InvalidArgument",
                    format!("candidate {n} is gone: a side is no longer the head"),
                )
            };
            match c.state {
                CandState::New => {}
                CandState::Opened => {
                    return err(
                        "InvalidArgument",
                        format!("candidate {n} was already opened"),
                    );
                }
                CandState::Gone => return gone(),
            }
            if !current(Some(&view), &c) {
                let _ = self.candidates.set_state(n, CandState::Gone);
                return gone();
            }
            let not_side = |r: RevisionId| {
                err(
                    "InvalidArgument",
                    format!("revision {} is not a side of candidate {n}", r.0),
                )
            };
            let other = if side_a == c.a {
                c.b
            } else if side_a == c.b {
                c.a
            } else {
                return not_side(side_a);
            };
            match side_b {
                None => side_b = Some(other),
                Some(x) if x == other => {}
                Some(x) => return not_side(x),
            }
        }
        if side_b == Some(side_a) {
            return err("InvalidArgument", "a Conflict pair needs two revisions");
        }
        let source = match from {
            Some(ConflictFrom::Candidate(n)) => ConflictSource::Candidate(n),
            Some(ConflictFrom::Review(r)) => match view.artifact(r) {
                Some(x) if x.ty == ArtifactType::PendingReview && x.open => {
                    ConflictSource::Review(r)
                }
                Some(_) => {
                    return err(
                        "InvalidArgument",
                        format!("artifact {} is not an open PendingReview", r.0),
                    );
                }
                None => return err("UnknownRef", format!("no artifact {}", r.0)),
            },
            None if id.channel == Channel::Agent => ConflictSource::Agent,
            None => ConflictSource::Admin,
        };
        let sides: Vec<RevisionId> = std::iter::once(side_a).chain(side_b).collect();
        let doc = ConflictDoc {
            v: 1,
            sides: sides.clone(),
            reason,
            source,
            evidence,
        };
        let (hash, bytes) = match contextdb_proto::docs::blob(&doc) {
            Ok(x) => x,
            Err(e) => return err("InvalidArgument", e),
        };
        // P7 m7: opens are serialised, and checked on a fresh snapshot, so
        // two concurrent opens of one pair give one Conflict.
        let _serial = self.conflict_opens.lock().expect("conflict opens");
        drop(snap);
        let snap = self.actor.snapshot();
        let Some(view) = snap.kernel.branch(branch) else {
            return err("UnknownBranch", "no such branch");
        };
        if let [a, b] = sides[..] {
            // A pair with an open two-sided Conflict, whatever its reason
            // or evidence: answer with it. A one-sided Conflict on a side
            // is not the pair's.
            if let Some(found) = open_pair(&view, a, b) {
                return Response::AlreadyOpen { artifacts: found };
            }
        } else if view.artifacts().any(|(_, x)| {
            // One side: the same document on the same revision.
            x.open && x.ty == ArtifactType::Conflict && x.payload_hash == hash && x.target == side_a
        }) {
            return err("InvalidArgument", "this Conflict is already open");
        }
        let mut ops: Vec<Op> = (sides.iter())
            .map(|r| Op::OpenArtifact {
                ty: ArtifactType::Conflict,
                target: (*r).into(),
                payload_hash: hash,
            })
            .collect();
        if let Some(ConflictFrom::Review(r)) = from {
            ops.push(Op::ResolveArtifact {
                id: r.into(),
                note: None,
            });
        }
        let response = self.conflict_write(
            id,
            branch,
            bound,
            ops,
            vec![bytes],
            expect_cert_valid,
            client_txn_id,
            snap,
        );
        // Lost on a crash here, the mark is restored by the next list, which
        // finds the pair open.
        if let (Some(ConflictFrom::Candidate(n)), Response::Committed { .. }) = (from, &response) {
            let _ = self
                .candidates
                .set_state(n, crate::candidates::CandState::Opened);
        }
        response
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resolve(
        &self,
        id: Identity,
        binding: Option<Binding>,
        branch: Option<String>,
        artifacts: Vec<ArtifactId>,
        note: String,
        expect_cert_valid: bool,
        client_txn_id: Option<[u8; 16]>,
        snap: Arc<Snapshot<D>>,
    ) -> Response {
        let (branch, bound) = match self.conflict_branch(id, &binding, &branch, &snap) {
            Ok(x) => x,
            Err(r) => return r,
        };
        let Some(view) = snap.kernel.branch(branch) else {
            return err("UnknownBranch", "no such branch");
        };
        let mut ids = artifacts;
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return err("InvalidArgument", "name at least one artifact");
        }
        for a in &ids {
            match view.artifact(*a) {
                None => return err("UnknownRef", format!("no artifact {}", a.0)),
                Some(x)
                    if !matches!(x.ty, ArtifactType::Conflict | ArtifactType::PendingReview) =>
                {
                    return err(
                        "InvalidArgument",
                        format!(
                            "artifact {} is a {:?}: ctx_resolve closes Conflicts and PendingReviews only",
                            a.0, x.ty
                        ),
                    );
                }
                Some(x) if !x.open => {
                    return err("InvalidArgument", format!("artifact {} is not open", a.0));
                }
                Some(_) => {}
            }
        }
        let mut blobs = Vec::new();
        let note = if note.trim().is_empty() {
            None
        } else {
            let doc = ResolutionDoc {
                v: 1,
                note,
                at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()),
            };
            match contextdb_proto::docs::blob(&doc) {
                Ok((hash, bytes)) => {
                    blobs.push(bytes);
                    Some(hash)
                }
                Err(e) => return err("InvalidArgument", e),
            }
        };
        let ops = (ids.into_iter())
            .map(|a| Op::ResolveArtifact { id: a.into(), note })
            .collect();
        self.conflict_write(
            id,
            branch,
            bound,
            ops,
            blobs,
            expect_cert_valid,
            client_txn_id,
            snap,
        )
    }
    /// A bound caller writes through `ctx_commit`'s path (certificate
    /// expectation, session write accounting); an unbound admin call
    /// commits on the branch directly.
    #[allow(clippy::too_many_arguments)]
    fn conflict_write(
        &self,
        id: Identity,
        branch: BranchId,
        bound: Option<Bound>,
        ops: Vec<Op>,
        blobs: Vec<Vec<u8>>,
        expect_cert_valid: bool,
        client_txn_id: Option<[u8; 16]>,
        snap: Arc<Snapshot<D>>,
    ) -> Response {
        match bound {
            Some(b) => self.commit_ops(id, b, ops, blobs, expect_cert_valid, client_txn_id, snap),
            None if expect_cert_valid => err(
                "InvalidArgument",
                "expect_cert_valid needs a session binding",
            ),
            None => {
                drop(snap);
                let mut batch = Batch::new(ops);
                batch.branch = branch;
                self.commit(id, blobs, batch, client_txn_id)
            }
        }
    }
}

/// The artifacts of an open two-sided Conflict between `a` and `b`: two
/// open Conflict artifacts sharing one document, one on each side.
fn open_pair(view: &BranchView<'_>, a: RevisionId, b: RevisionId) -> Option<Vec<ArtifactId>> {
    let open: Vec<(ArtifactId, Artifact)> = (view.artifacts())
        .filter(|(_, x)| x.open && x.ty == ArtifactType::Conflict)
        .collect();
    open.iter()
        .filter(|(_, x)| x.target == a)
        .find_map(|(ia, xa)| {
            let (ib, _) = open
                .iter()
                .find(|(_, xb)| xb.target == b && xb.payload_hash == xa.payload_hash)?;
            let mut ids = vec![*ia, *ib];
            ids.sort_unstable();
            Some(ids)
        })
}

/// How a candidate was found (ADR 0016 §7.3).
fn candidate_reason(c: &crate::candidates::Candidate) -> String {
    use crate::candidates::{EMBEDDING, LEXICAL};
    let cos = c.cosine.unwrap_or(0.0);
    match (c.source & LEXICAL != 0, c.source & EMBEDDING != 0) {
        (true, true) => format!(
            "lexical and embedding similarity, bm25 {:.2}, cosine {cos:.2}",
            c.score
        ),
        (false, true) => format!("embedding similarity, cosine {cos:.2}"),
        _ => format!("lexical similarity, bm25 {:.2}", c.score),
    }
}
