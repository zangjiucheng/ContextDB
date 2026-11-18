//! Publishing queued proposals (ADR 0018 §7.2–7.4, SPEC SD21.12).
//!
//! The steps that read or write the store, in order; the git side (sign,
//! push) is the hook's, in the developer's environment:
//!
//! - **step 5, [`prepare`]** fixes the publish set: every queued proposal is
//!   checked again against the local policy now in force (egress included;
//!   failures become `conflicted{policy}` with their dependents), the
//!   publish policy is applied (confirm, or more than one local author:
//!   list and stop; `--yes <token>` publishes from that listing only), and
//!   the rest is rebuilt on the replica tip by scratch `accept` in queue
//!   order, as import will decide it. What passes goes into one unsigned
//!   entry within the limits; the rest waits.
//! - **step 7, [`begin`]** is the linearization point: in one write it
//!   checks that the policy version and the proposals' states are those step
//!   5 saw, and records them `published`. A policy change before it stops
//!   the entry; one after it applies from the next publish.
//! - **step 9, [`refused`]**: a non-fast-forward push returns the entry's
//!   proposals to `queued`.
//! - **after import, [`reconcile`]**: published proposals take their
//!   outcome from the decided entries, or go back to `queued` when the chain
//!   does not hold them.

use crate::commit::{self, hex};
use crate::git::Repo;
use crate::gitobj;
use crate::ops::{Hash, MAX_BATCH_BYTES, MAX_ENTRY_BYTES, MAX_OPS, TeamBatch, TeamOp};
use crate::proposals::{LocalPolicy, ProposeError, Queue, State, recheck};
use crate::store::TeamStore;
use contextdb_model::PrincipalId;
use std::collections::{BTreeMap, BTreeSet};

/// The publish policy (O5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Publish, unless the set holds more than one local author.
    Auto,
    /// List only.
    Confirm,
    /// Publish from the listing with this token.
    Yes(Hash),
}

/// An entry ready to sign: the object the signature covers and its tree.
#[derive(Clone, Debug)]
pub struct Unsigned {
    pub payload: Vec<u8>,
    pub tree: Vec<(String, Vec<u8>)>,
    pub proposals: Vec<[u8; 16]>,
    pub seq: u64,
    pub parent: Option<String>,
}

/// A confirm listing: its token and `(id, author)` of each proposal.
pub type Listing = (Hash, Vec<([u8; 16], PrincipalId)>);

#[derive(Clone, Debug, Default)]
pub struct Prepared {
    pub entry: Option<Unsigned>,
    pub listed: Option<Listing>,
    /// Proposals left out, with the reason shown to their authors.
    pub conflicted: Vec<([u8; 16], String)>,
    /// The policy version step 5 checked against.
    pub policy: Vec<u8>,
}

#[derive(Debug)]
pub enum PublishError {
    UnknownToken,
    NoTeam,
    Io(std::io::Error),
}

impl From<std::io::Error> for PublishError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

fn reason_of(e: &ProposeError) -> String {
    match e {
        ProposeError::Local(t) => format!(
            "policy: {}",
            format!("{t:?}").split([' ', '{', '(']).next().unwrap_or("")
        ),
        ProposeError::EgressDenied { .. } => "policy: EgressDenied".into(),
        other => format!("{other:?}"),
    }
}

/// Step 5.
pub fn prepare(
    store: &TeamStore,
    queue: &mut Queue,
    member: &str,
    policy: &LocalPolicy,
    mode: Mode,
) -> Result<Prepared, PublishError> {
    prepare_on(store.replica(), store.tip(), queue, member, policy, mode)
}

/// [`prepare`] on a replica whose last decided entry is `tip` (PROP18 runs
/// it without a repository).
pub fn prepare_on(
    replica: &crate::replica::Replica,
    tip: Option<&str>,
    queue: &mut Queue,
    member: &str,
    policy: &LocalPolicy,
    mode: Mode,
) -> Result<Prepared, PublishError> {
    let doc = replica.doc().ok_or(PublishError::NoTeam)?.clone();
    let mut out = Prepared {
        policy: policy.version.to_vec(),
        ..Default::default()
    };
    let mut dead: BTreeSet<[u8; 16]> = (queue.proposals.iter())
        .filter(|p| {
            matches!(
                p.state,
                State::Conflicted { .. } | State::Rejected { .. } | State::Dropped
            )
        })
        .map(|p| p.id)
        .collect();
    let in_flight: BTreeSet<[u8; 16]> = (queue.proposals.iter())
        .filter(|p| matches!(p.state, State::Published { .. }))
        .map(|p| p.id)
        .collect();
    // The local layer again, under the policy now in force; dependents of
    // a dead proposal die with it.
    let mut changes = Vec::new();
    let mut candidates = Vec::new();
    for (i, p) in queue.proposals.iter().enumerate() {
        if p.state != State::Queued {
            continue;
        }
        let reason = if p.deps.iter().any(|d| dead.contains(d)) {
            Some("dependency".to_owned())
        } else {
            recheck(replica, queue, i, p, policy)
                .err()
                .map(|e| reason_of(&e))
        };
        match reason {
            Some(r) => {
                dead.insert(p.id);
                out.conflicted.push((p.id, r.clone()));
                changes.push((p.id, State::Conflicted { reason: r }));
            }
            // A proposal depending on one in flight waits for its outcome.
            None if p.deps.iter().any(|d| in_flight.contains(d)) => {}
            None => candidates.push(p.id),
        }
    }
    // The publish policy.
    let authors: BTreeSet<PrincipalId> = candidates
        .iter()
        .map(|id| queue.get(id).expect("queued").author)
        .collect();
    let set: Vec<[u8; 16]> = match mode {
        Mode::Yes(token) => {
            let listed = queue.listed.get(&token).ok_or(PublishError::UnknownToken)?;
            candidates
                .iter()
                .copied()
                .filter(|id| listed.contains(id))
                .collect()
        }
        Mode::Confirm => Vec::new(),
        Mode::Auto if authors.len() > 1 => Vec::new(),
        Mode::Auto => candidates.clone(),
    };
    if matches!(mode, Mode::Confirm) || (mode == Mode::Auto && authors.len() > 1) {
        if !candidates.is_empty() {
            let token = queue.list(candidates.clone())?;
            let who = candidates
                .iter()
                .map(|id| (*id, queue.get(id).expect("queued").author))
                .collect();
            out.listed = Some((token, who));
        }
        queue.transition(changes)?;
        return Ok(out);
    }
    // The team layer on the tip, in queue order, within the limits.
    let mut scratch = replica.clone();
    let mut included = Vec::new();
    let mut ops_all = Vec::new();
    let mut blobs: BTreeMap<Hash, Vec<u8>> = BTreeMap::new();
    let (mut n_ops, mut bytes) = (0usize, 0usize);
    // Proposals that wait for the next entry, and so their dependents
    // (review finding 5): never conflicted for want of room.
    let mut deferred: BTreeSet<[u8; 16]> = BTreeSet::new();
    for id in &set {
        let p = queue.get(id).expect("queued").clone();
        if p.deps.iter().any(|d| deferred.contains(d)) {
            deferred.insert(p.id);
            continue;
        }
        if p.deps.iter().any(|d| dead.contains(d)) {
            dead.insert(p.id);
            out.conflicted.push((p.id, "dependency".into()));
            changes.push((
                p.id,
                State::Conflicted {
                    reason: "dependency".into(),
                },
            ));
            continue;
        }
        // A sole op (`SetTeamAccess`) must be its entry's only op (accept's
        // `SoleOpRequired`): it waits for an entry of its own, and nothing
        // after it in queue order joins that entry or overtakes it.
        let sole = p.ops.iter().any(TeamOp::is_sole);
        if sole && !included.is_empty() {
            break;
        }
        let pb = queue.blobs_of(&p);
        let new_blobs: BTreeMap<Hash, Vec<u8>> = pb
            .iter()
            .filter(|(h, _)| replica.blob(h).is_none() && !blobs.contains_key(*h))
            .map(|(h, b)| (*h, b.clone()))
            .collect();
        let size = TeamBatch {
            proposals: vec![p.ops.clone()],
        }
        .encode()
        .len();
        let extra: usize = new_blobs.values().map(Vec::len).sum();
        if n_ops + p.ops.len() > MAX_OPS
            || bytes + size > MAX_BATCH_BYTES
            || bytes + size + extra + blobs.values().map(Vec::len).sum::<usize>() > MAX_ENTRY_BYTES
        {
            // Next entry.
            deferred.insert(p.id);
            continue;
        }
        let mut trial = scratch.clone();
        match trial.scratch_apply(member, p.id, &p.ops, &pb) {
            Ok(()) => {
                scratch = trial;
                n_ops += p.ops.len();
                bytes += size;
                blobs.extend(new_blobs);
                included.push(p.id);
                ops_all.push(p.ops.clone());
                if sole {
                    break;
                }
            }
            Err(r) => {
                dead.insert(p.id);
                let reason = format!("{}: {}", r.code, r.detail);
                out.conflicted.push((p.id, reason.clone()));
                changes.push((p.id, State::Conflicted { reason }));
            }
        }
    }
    queue.transition(changes)?;
    if included.is_empty() {
        return Ok(out);
    }
    let batch = TeamBatch { proposals: ops_all }.encode();
    let seq = replica.seq().map_or(0, |s| s + 1);
    let mut trailers: Vec<(&str, String)> = vec![
        ("Ctx-Team", hex(&replica.team())),
        ("Ctx-Team-Seq", seq.to_string()),
        ("Ctx-Author", member.to_owned()),
        ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
        ("Ctx-Accept-Version", doc.accept_version.to_string()),
    ];
    for id in &included {
        trailers.push(("Ctx-Proposal", hex(id)));
    }
    let mut tree = vec![("batch".to_owned(), batch)];
    tree.extend(
        blobs
            .into_iter()
            .map(|(h, b)| (format!("blobs/{}", hex(&h)), b)),
    );
    let tree_id = gitobj::tree_id(replica.format(), &tree)
        .map_err(|_| PublishError::Io(std::io::Error::other("SHA-1 collision in an entry tree")))?;
    let parent = tip.map(str::to_owned);
    let parents: Vec<String> = parent.iter().cloned().collect();
    let subject = format!(
        "ctx team: {} proposal{}",
        included.len(),
        if included.len() == 1 { "" } else { "s" }
    );
    out.entry = Some(Unsigned {
        payload: commit::build(&hex(&tree_id), &parents, &subject, &trailers),
        tree,
        proposals: included,
        seq,
        parent,
    });
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
pub enum BeginError {
    /// The local policy changed since step 5: prepare again.
    PolicyChanged,
    /// A proposal of the entry changed state since step 5.
    QueueChanged,
}

/// Step 7, the linearization point.
pub fn begin(
    queue: &mut Queue,
    prepared: &Prepared,
    policy_now: &[u8],
    commit: &str,
) -> Result<Result<(), BeginError>, std::io::Error> {
    let Some(entry) = &prepared.entry else {
        return Ok(Ok(()));
    };
    if prepared.policy != policy_now {
        return Ok(Err(BeginError::PolicyChanged));
    }
    if entry
        .proposals
        .iter()
        .any(|id| queue.get(id).is_none_or(|p| p.state != State::Queued))
    {
        return Ok(Err(BeginError::QueueChanged));
    }
    queue.transition(
        entry
            .proposals
            .iter()
            .map(|id| {
                (
                    *id,
                    State::Published {
                        commit: commit.to_owned(),
                    },
                )
            })
            .collect(),
    )?;
    Ok(Ok(()))
}

/// Step 9: the push was refused (non-fast-forward); back to the queue.
pub fn refused(queue: &mut Queue, prepared: &Prepared) -> std::io::Result<()> {
    let Some(entry) = &prepared.entry else {
        return Ok(());
    };
    queue.transition(
        entry
            .proposals
            .iter()
            .map(|id| (*id, State::Queued))
            .collect(),
    )
}

/// After an import: a pending proposal the chain decided takes its outcome
/// (whatever its local state, so an outcome is never lost to a requeue);
/// a published one the tracking ref's chain does not hold is queued again
/// (the push did not land) — only with `requeue`, i.e. when no publish is
/// in flight between its step 7 and its push.
pub fn reconcile(
    store: &TeamStore,
    queue: &mut Queue,
    member: &str,
    repo: &Repo,
    requeue: bool,
) -> std::io::Result<usize> {
    let tip = repo
        .resolve(&store.tracking_ref(store.replica().generation()))
        .ok()
        .flatten();
    let mut changes = Vec::new();
    for p in &queue.proposals {
        if p.state == State::Queued {
            match store.decision(member, &p.id) {
                Some(Ok(())) => changes.push((p.id, State::Accepted)),
                Some(Err(r)) => changes.push((
                    p.id,
                    State::Rejected {
                        code: r.code.to_owned(),
                        detail: r.detail.clone(),
                    },
                )),
                None => {}
            }
            continue;
        }
        let State::Published { commit } = &p.state else {
            continue;
        };
        match store.decision(member, &p.id) {
            Some(Ok(())) => changes.push((p.id, State::Accepted)),
            Some(Err(r)) => changes.push((
                p.id,
                State::Rejected {
                    code: r.code.to_owned(),
                    detail: r.detail.clone(),
                },
            )),
            // Our own entry, rejected without deciding its proposals (its
            // signature failed, e.g. a key the team document lacks): the
            // commit id is the one this edge pushed (review M1).
            None if store.rejected_entry(commit).is_some() => {
                let r = store.rejected_entry(commit).expect("rejected");
                changes.push((
                    p.id,
                    State::Rejected {
                        code: r.code.to_owned(),
                        detail: r.detail.clone(),
                    },
                ));
            }
            None => {
                let landed = tip
                    .as_ref()
                    .is_some_and(|t| repo.is_ancestor(commit, t).unwrap_or(false));
                if !landed && requeue {
                    changes.push((p.id, State::Queued));
                }
            }
        }
    }
    let n = changes.len();
    queue.transition(changes)?;
    Ok(n)
}
