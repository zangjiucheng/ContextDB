//! Observer client implementation is isolated from persistence. Captures and
//! classifications are submitted with one Raw batch, never per-file commits.
use crate::{AnchorState, Prepared, Repo, Unavailable, prepare};
use contextdb_model::{Batch, CommitResult, Wire};
use contextdb_proto::{
    Client,
    proto::{Request, Response},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};
pub type ScopeState = (Option<Vec<u8>>, BTreeMap<String, AnchorState>);
pub fn state(client: &mut Client, scope: &str) -> Result<ScopeState, Unavailable> {
    let reply = client
        .call(&Request::AnchorState {
            scope: scope.into(),
        })
        .map_err(|e| Unavailable(format!("{e:?}")))?;
    match reply.response {
        Response::AnchorState { watermark, anchors } => {
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
            Ok((watermark, prior))
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
        let (watermark, prior) = state(client, &repo.scope())?;
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
        match submit(client, prepared) {
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
pub fn submit(
    client: &mut Client,
    prepared: Prepared,
) -> Result<Option<CommitResult>, Unavailable> {
    if prepared.ops.is_empty() {
        return Ok(None);
    }
    let reply = client
        .call(&Request::Raw {
            blobs: prepared.blobs,
            batch: Batch::new(prepared.ops),
        })
        .map_err(|e| Unavailable(format!("{e:?}")))?;
    match reply.response {
        Response::Committed { result } => Ok(Some(result)),
        other => Err(Unavailable(format!("{other:?}"))),
    }
}
