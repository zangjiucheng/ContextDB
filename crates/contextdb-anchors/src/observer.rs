//! Observer client implementation is isolated from persistence. Captures and
//! classifications are submitted with one Raw batch, never per-file commits.
use crate::{AnchorDescriptor, AnchorState, Prepared, Repo, Unavailable, prepare};
use contextdb_model::{Batch, BranchId, CommitResult, MAIN, Wire};
use contextdb_proto::{
    Client,
    proto::{Request, Response},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};
pub type ScopeState = (Option<Vec<u8>>, BTreeMap<String, AnchorState>);
/// The scope's committed state on `main`.
pub fn state(client: &mut Client, scope: &str) -> Result<ScopeState, Unavailable> {
    state_on(client, scope, None).map(|(_, s)| s)
}
/// The scope's committed state on the branch named like `git_branch` (the
/// observer worktree's checked-out branch) if it exists and is open, else
/// `main`; the branch it was read on is where the next batch commits
/// (ADR 0013 §7, m3).
pub fn state_on(
    client: &mut Client,
    scope: &str,
    git_branch: Option<&str>,
) -> Result<(BranchId, ScopeState), Unavailable> {
    let reply = client
        .call(&Request::AnchorState {
            scope: scope.into(),
            branch: git_branch.map(str::to_owned),
        })
        .map_err(|e| Unavailable(format!("{e:?}")))?;
    match reply.response {
        Response::AnchorState {
            branch,
            watermark,
            anchors,
        } => {
            let mut prior = BTreeMap::new();
            for a in anchors {
                let d = crate::Descriptor::from_bytes(&a.descriptor)
                    .map_err(|e| Unavailable(e.to_string()))?;
                if d.scope() != scope || d.key() != a.key || d.content() != a.content {
                    return Err(Unavailable("corrupt anchor descriptor".into()));
                }
                prior.insert(
                    d.path,
                    AnchorState {
                        logical: a.logical,
                        head: a.head,
                        content: a.content,
                        oid: d.oid,
                        base: a.base,
                        breaks: a.breaks,
                    },
                );
            }
            Ok((branch, (watermark, prior)))
        }
        other => Err(Unavailable(format!("{other:?}"))),
    }
}
/// Committed anchors of a scope keyed by logical key, file and symbol alike,
/// with their decoded descriptors (ADR 0012 §5, §7): paths always come from
/// descriptors, never from splitting keys.
pub type KeyedState = (
    Option<Vec<u8>>,
    BTreeMap<String, (AnchorDescriptor, AnchorState)>,
);
/// [`state_by_key_on`] on `main`.
pub fn state_by_key(client: &mut Client, scope: &str) -> Result<KeyedState, Unavailable> {
    state_by_key_on(client, scope, None).map(|(_, s)| s)
}
/// As [`state_on`], keyed by logical key.
pub fn state_by_key_on(
    client: &mut Client,
    scope: &str,
    git_branch: Option<&str>,
) -> Result<(BranchId, KeyedState), Unavailable> {
    let reply = client
        .call(&Request::AnchorState {
            scope: scope.into(),
            branch: git_branch.map(str::to_owned),
        })
        .map_err(|e| Unavailable(format!("{e:?}")))?;
    match reply.response {
        Response::AnchorState {
            branch,
            watermark,
            anchors,
        } => {
            let mut out = BTreeMap::new();
            for a in anchors {
                let d = AnchorDescriptor::from_bytes(&a.descriptor)
                    .map_err(|e| Unavailable(format!("corrupt anchor descriptor: {e}")))?;
                if d.scope() != scope || d.key() != a.key || d.content() != a.content {
                    return Err(Unavailable("corrupt anchor descriptor".into()));
                }
                let state = AnchorState {
                    logical: a.logical,
                    head: a.head,
                    content: a.content,
                    oid: d.match_id().to_vec(),
                    base: a.base,
                    breaks: a.breaks,
                };
                out.insert(a.key, (d, state));
            }
            Ok((branch, (watermark, out)))
        }
        other => Err(Unavailable(format!("{other:?}"))),
    }
}
/// Each CAS retry reloads committed state and captures again. No obsolete
/// manifest may be paired with a newly read watermark.
pub fn reconcile(
    client: &mut Client,
    repo: &Repo,
    added: &BTreeSet<String>,
    deadline: Instant,
) -> Result<Option<CommitResult>, Unavailable> {
    for _ in 0..3 {
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| Unavailable("observer deadline".into()))?;
        client
            .set_timeout(timeout)
            .map_err(|e| Unavailable(e.to_string()))?;
        let git_branch = contextdb_proto::git::checked_out_branch(&repo.root);
        let (branch, (watermark, prior)) = state_on(client, &repo.scope(), git_branch.as_deref())?;
        let paths = prior.keys().cloned().chain(added.iter().cloned()).collect();
        let captured = repo.capture(&paths, deadline)?;
        let prepared = prepare(
            &repo.id,
            &captured.entries,
            &prior,
            watermark,
            captured.position,
        )
        .map_err(Unavailable)?;
        client
            .set_timeout(
                deadline
                    .checked_duration_since(Instant::now())
                    .ok_or_else(|| Unavailable("observer deadline".into()))?,
            )
            .map_err(|e| Unavailable(e.to_string()))?;
        match submit_on(client, prepared, branch) {
            Ok(result) => return Ok(result),
            Err(e)
                if ["HeadConflict", "WatermarkConflict", "LogicalExists"]
                    .iter()
                    .any(|c| e.0.contains(c)) => {}
            Err(e) => return Err(e),
        }
    }
    Err(Unavailable("observer CAS retry limit".into()))
}
/// [`submit_on`] `main`.
pub fn submit(
    client: &mut Client,
    prepared: Prepared,
) -> Result<Option<CommitResult>, Unavailable> {
    submit_on(client, prepared, MAIN)
}
/// Commits a prepared observation on `branch` (the one its state was read on).
pub fn submit_on(
    client: &mut Client,
    prepared: Prepared,
    branch: BranchId,
) -> Result<Option<CommitResult>, Unavailable> {
    if prepared.ops.is_empty() {
        return Ok(None);
    }
    let mut batch = Batch::new(prepared.ops);
    batch.branch = branch;
    let reply = client
        .call(&Request::Raw {
            blobs: prepared.blobs,
            batch,
        })
        .map_err(|e| Unavailable(format!("{e:?}")))?;
    match reply.response {
        Response::Committed { result } => Ok(Some(result)),
        other => Err(Unavailable(format!("{other:?}"))),
    }
}
