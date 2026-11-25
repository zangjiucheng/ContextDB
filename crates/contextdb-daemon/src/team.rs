//! P10 team exchange in `ctxd` (ADR 0018 §7.1, §8): every joined team's
//! replica and proposal queue live on the commit actor, the single writer.
//! Import decides new chain entries into the replica, then brings every open
//! branch's mirrors to it through ordinary replica-channel commits (§5.2);
//! a write the kernel refuses as a team write becomes a proposal (§6.1).
//!
//! Readers decide `ReplicationLag` from [`Live`]: the fetched tip `ctxd`
//! holds per team, withheld from `live_pos` while an announce is pending, a
//! fetch is in flight or import is stopped (§8.3).

use contextdb_core::Kernel;
use contextdb_model::*;
use contextdb_store::StoreError;
use contextdb_team::catchup;
use contextdb_team::commit::hex;
use contextdb_team::git::Repo;
use contextdb_team::proposals::{LocalPolicy, Queue};
use contextdb_team::replica::Replica;
use contextdb_team::store::{Status, TeamStore};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `<data>/teams`: one directory per joined team, named by its hex id.
pub const TEAMS_DIR: &str = "teams";
/// How often `ctxd` re-reads the team refs (ADR 0018 §8.3, default 2 s).
pub const LAG_CHECK: Duration = Duration::from_secs(2);
const CONFIG: &str = "config";

/// O5's publish policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishMode {
    Auto,
    Confirm,
}

/// What `ctx team join` records next to the replica (§7, §8.7, §9.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamConfig {
    /// The git work tree whose remote carries the chain.
    pub repo: PathBuf,
    /// The team member this store is bound to, and its signing key.
    pub member: String,
    pub key: String,
    pub publish: PublishMode,
    pub strict_push: bool,
    pub push_timeout: Duration,
    pub lag_check: Duration,
    pub warn_after: Duration,
    /// Where the chain lives on the remote (O4): custom refs, or the
    /// orphan-branch fallback. A config written before this field is
    /// `refs`.
    pub refs: contextdb_team::layout::RefLayout,
}

impl TeamConfig {
    pub fn new(repo: PathBuf, member: String, key: String) -> Self {
        Self {
            repo,
            member,
            key,
            publish: PublishMode::Auto,
            strict_push: false,
            push_timeout: Duration::from_secs(30),
            lag_check: Duration::from_secs(2),
            warn_after: Duration::from_secs(24 * 3600),
            refs: contextdb_team::layout::RefLayout::Refs,
        }
    }
    fn encode(&self) -> String {
        format!(
            "repo {}\nmember {}\nkey {}\npublish {}\nstrict_push {}\npush_timeout_ms {}\nlag_check_ms {}\nwarn_after_s {}\nrefs {}\n",
            self.repo.display(),
            self.member,
            self.key,
            match self.publish {
                PublishMode::Auto => "auto",
                PublishMode::Confirm => "confirm",
            },
            self.strict_push,
            self.push_timeout.as_millis(),
            self.lag_check.as_millis(),
            self.warn_after.as_secs(),
            self.refs.name(),
        )
    }
    fn decode(text: &str) -> Result<Self, String> {
        let field = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix(' ')))
                .ok_or_else(|| format!("team config: no {k}"))
        };
        let num = |k: &str| -> Result<u64, String> {
            field(k)?
                .parse()
                .map_err(|_| format!("team config: {k} is not a number"))
        };
        Ok(Self {
            repo: PathBuf::from(field("repo")?),
            member: field("member")?.to_owned(),
            key: field("key")?.to_owned(),
            publish: match field("publish")? {
                "auto" => PublishMode::Auto,
                "confirm" => PublishMode::Confirm,
                other => return Err(format!("team config: publish = {other}")),
            },
            strict_push: field("strict_push")? == "true",
            push_timeout: Duration::from_millis(num("push_timeout_ms")?),
            lag_check: Duration::from_millis(num("lag_check_ms")?),
            warn_after: Duration::from_secs(num("warn_after_s")?),
            refs: match field("refs") {
                Err(_) => contextdb_team::layout::RefLayout::Refs,
                Ok(v) => contextdb_team::layout::RefLayout::parse(v)
                    .ok_or_else(|| format!("team config: refs = {v}"))?,
            },
        })
    }
    pub fn load(dir: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(dir.join(CONFIG)).map_err(|e| e.to_string())?;
        Self::decode(&text)
    }
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let path = dir.join(CONFIG);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, self.encode())?;
        std::fs::File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        std::fs::File::open(dir)?.sync_all()
    }
}

/// A joined team on the actor.
pub struct Joined {
    pub store: TeamStore,
    pub queue: Queue,
    pub cfg: TeamConfig,
    /// The last import's outcome.
    pub status: Status,
    /// Publish step 5's set, which step 7 checks against (§7.2).
    pub prepared: Option<contextdb_team::publish::Prepared>,
    /// `leaving` is recorded (§8.5): never imported or caught up again;
    /// the leave is finished at the next import.
    pub leaving: bool,
}

const LEAVING: &str = "leaving";

/// Records `leaving` durably before any branch is written (§8.5, Q12).
pub fn mark_leaving(dir: &Path) -> std::io::Result<()> {
    let path = dir.join(LEAVING);
    std::fs::write(&path, b"leaving\n")?;
    std::fs::File::open(&path)?.sync_all()?;
    std::fs::File::open(dir)?.sync_all()
}

/// Opens one joined team's directory.
pub fn open_one(dir: &Path) -> Result<Joined, String> {
    let cfg = TeamConfig::load(dir)?;
    let store = TeamStore::open(dir).map_err(|e| format!("{e:?}"))?;
    let queue = Queue::open(dir).map_err(|e| e.to_string())?;
    Ok(Joined {
        store,
        queue,
        cfg,
        status: Status::UpToDate,
        prepared: None,
        leaving: dir.join(LEAVING).exists(),
    })
}

/// Opens every joined team under `<data>/teams`. A team that does not open
/// is reported and skipped; its reads lag (no live position).
pub fn open_all(data: &Path) -> (BTreeMap<TeamId, Joined>, Vec<String>) {
    let mut teams = BTreeMap::new();
    let mut errors = Vec::new();
    let Ok(entries) = std::fs::read_dir(data.join(TEAMS_DIR)) else {
        return (teams, errors);
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        match open_one(&dir) {
            Ok(j) => {
                teams.insert(j.store.team(), j);
            }
            Err(e) => errors.push(format!("{}: {e}", dir.display())),
        }
    }
    (teams, errors)
}

/// One team's read-side lag state (§8.3), under the lock reads take.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Live {
    /// The tracking ref's tip of the newest generation, as `ctxd` last read
    /// it.
    pub tip: Option<String>,
    /// Announces not yet resolved: `(ref, oid)` → the git process's pid.
    pub pending: BTreeMap<(String, String), u32>,
    /// Import stopped (`ReplicaStalled`, `ReplicaDiverged`,
    /// `ReplicaIncomplete`).
    pub stopped: Option<String>,
    /// §6.3: pending proposals by the full key of each record they write.
    pub proposed: BTreeMap<String, Vec<([u8; 16], PrincipalId)>>,
}

/// The record a team op writes, if any.
fn target(op: &contextdb_team::ops::TeamOp) -> Option<&str> {
    use contextdb_team::ops::TeamOp as T;
    match op {
        T::New { path, .. }
        | T::Revise { path, .. }
        | T::SetHead { path, .. }
        | T::SetBase { path, .. }
        | T::SetTags { path, .. } => Some(path),
        T::AddRule { rule, .. } | T::SetRuleEnabled { rule, .. } => Some(&rule.conclusion.0),
        _ => None,
    }
}

/// Recomputes `j`'s proposal markers (§6.3) after its queue changed.
pub fn refresh_proposed(j: &Joined, live: &LiveMap) {
    let team = j.store.team();
    let mut map: BTreeMap<String, Vec<([u8; 16], PrincipalId)>> = BTreeMap::new();
    for p in j.queue.proposals.iter().filter(|p| p.state.pending()) {
        for path in p.ops.iter().filter_map(target) {
            let e = map
                .entry(format!("team/{}/{path}", hex(&team)))
                .or_default();
            if !e.iter().any(|(id, _)| *id == p.id) {
                e.push((p.id, p.author));
            }
        }
    }
    live.lock()
        .expect("team live")
        .entry(team)
        .or_default()
        .proposed = map;
}

/// The markers of `key` (§6.3), before redaction.
pub fn proposed_for(live: &LiveMap, key: &str) -> Vec<([u8; 16], PrincipalId)> {
    if !key.starts_with("team/") {
        return Vec::new();
    }
    let live = live.lock().expect("team live");
    live.values()
        .filter_map(|l| l.proposed.get(key))
        .flatten()
        .copied()
        .collect()
}

impl Live {
    /// The position a read compares a branch's watermark with, or `None`
    /// (every read over the team's mirrors lags).
    pub fn position(&self) -> Option<Vec<u8>> {
        // A fetch in flight is a pending announce: `ctxd` runs no fetch of
        // its own (review finding 13).
        if !self.pending.is_empty() || self.stopped.is_some() {
            return None;
        }
        self.tip.as_ref().map(|t| t.as_bytes().to_vec())
    }
}

/// Per team, shared by the actor (which updates it) and readers.
pub type LiveMap = Arc<Mutex<BTreeMap<TeamId, Live>>>;

/// The team scopes of `live_pos`: every team whose position is known.
pub fn live_positions(live: &LiveMap) -> BTreeMap<String, Vec<u8>> {
    let live = live.lock().expect("team live");
    live.iter()
        .filter_map(|(t, l)| Some((team_scope(t), l.position()?)))
        .collect()
}

/// The contents a catch-up batch names, whose bytes the store must hold:
/// new revisions' contents, and the contents of existing revisions it makes
/// a head or a base again, resolved on `k`, when `collected` (ADR 0019
/// §5.4: the replica's bytes stage them again).
pub fn contents<'a>(
    k: &'a Kernel,
    b: &'a Batch,
    collected: &'a dyn Fn(&ContentHash) -> bool,
) -> impl Iterator<Item = ContentHash> + 'a {
    let existing = move |r: &RevRef| match r {
        RevRef::Existing(r) => (k.branch(b.branch)?.revision_content(*r).ok()).filter(collected),
        RevRef::New(_) => None,
    };
    b.ops.iter().filter_map(move |op| match op {
        Op::CreateLogical { content, .. } | Op::AddRevision { content, .. } => Some(*content),
        Op::MirrorAttach { head, .. } => Some(*head),
        Op::SetHead { new, .. } => existing(new),
        Op::AssertBase { rev } => existing(rev),
        _ => None,
    })
}

/// Brings every open branch to a team's replica; the actor's writer.
pub type CatchUp<'a> =
    dyn FnMut(TeamId, &Replica, &catchup::History) -> Result<(), (BranchId, StoreError)> + 'a;

/// Imports `j`'s tracking refs, then brings every open branch to the
/// replica (main first). Returns what to show for the team; updates `live`.
pub fn import(j: &mut Joined, live: &LiveMap, catch_up: &mut CatchUp<'_>) -> String {
    let team = j.store.team();
    let repo = match Repo::open(&j.cfg.repo) {
        Ok(r) => r,
        Err(e) => {
            set_stopped(live, team, Some(format!("ReplicaStalled: {e:?}")));
            return format!("{} ReplicaStalled: {e:?}", hex(&team));
        }
    };
    let status = match j.store.import(&repo) {
        Ok(s) => s,
        Err(e) => Status::Stalled(format!("{e:?}")),
    };
    let stopped = match &status {
        Status::Diverged(d) => Some(format!("ReplicaDiverged: {d}")),
        Status::Incomplete(m) => Some(format!("ReplicaIncomplete: {} objects", m.len())),
        Status::Stalled(s) => Some(format!("ReplicaStalled: {s}")),
        _ => None,
    };
    j.status = status.clone();
    // Published proposals take their outcome from what was decided, or go
    // back to the queue when the chain does not hold them (§7.4).
    let requeue = !publish_in_flight(j.store.dir());
    if let Err(e) =
        contextdb_team::publish::reconcile(&j.store, &mut j.queue, &j.cfg.member, &repo, requeue)
    {
        eprintln!("ctxd: team {}: reconcile: {e}", hex(&team));
    }
    // The position reads compare with. With a `Compact` pending (§8.6) no
    // ref is watched: entries appended to the terminal generation are
    // ignored (Q3), and a successor's state is Σc, already mirrored — a
    // squatted successor ref must not lag reads either. The tip is the last
    // decided entry (review finding 8, review N1).
    let tip = match j.store.replica().pending() {
        Some(_) => j.store.tip().map(str::to_owned),
        None => repo
            .resolve(&j.store.tracking_ref(j.store.replica().generation()))
            .ok()
            .flatten(),
    };
    // Bring every open branch to what the replica decided, even when the
    // import stopped part-way (a prefix is decided).
    let caught = match catch_up(team, j.store.replica(), j.store.history()) {
        Ok(()) => String::new(),
        Err((b, e)) => format!("; ReplicaStalled on branch {}: {e:?}", b.0),
    };
    {
        let mut l = live.lock().expect("team live");
        let e = l.entry(team).or_default();
        e.tip = tip.clone();
        e.stopped = stopped.or_else(|| (!caught.is_empty()).then(|| caught.clone()));
    }
    resolve_announces(&j.store, &repo, live);
    refresh_proposed(j, live);
    format!("{} {status:?}{caught}", hex(&team))
}

fn set_stopped(live: &LiveMap, team: TeamId, why: Option<String>) {
    live.lock()
        .expect("team live")
        .entry(team)
        .or_default()
        .stopped = why;
}

/// The local policy in force: the kernel's access document and its hash.
pub fn local_policy(k: &Kernel) -> (Option<contextdb_core::access::AccessState>, Vec<u8>) {
    let access = k.access().cloned();
    let version = match &access {
        Some(a) => {
            let mut bytes = Vec::new();
            contextdb_model::Wire::encode(&a.doc, &mut bytes);
            blake3::hash(&bytes).as_bytes().to_vec()
        }
        None => Vec::new(),
    };
    (access, version)
}

/// [`propose`]'s answer: the queued proposal, or not a team write.
pub type Routed = Result<Option<(TeamId, [u8; 16])>, TxError>;

/// A write the kernel refused as a team write: translated against the
/// author's overlays and entered into the team's queue. `Ok(None)`: not a
/// team write (the kernel's refusal stands).
///
/// A client transaction id is idempotent across both paths (SPEC §6.1):
/// the retry of a proposed request answers the same proposal, another
/// request under a proposal's id, or a proposal under an id the kernel
/// committed (`txn_committed`), is `TxnIdReuse`.
pub fn propose(
    teams: &mut BTreeMap<TeamId, Joined>,
    k: &Kernel,
    b: &Batch,
    blobs: &[Vec<u8>],
    refused: TxError,
    txn_committed: bool,
) -> Routed {
    let txn = b.client_txn_id.map(|t| (t, request_hash(b)));
    if let Some((t, hash)) = txn {
        if let Some((team, p)) = proposal_by_txn(teams, &t) {
            return match p.txn {
                Some((_, h)) if h == hash => Ok(Some((team, p.id))),
                _ => Err(TxError::TxnIdReuse { id: t }),
            };
        }
        if txn_committed {
            return Err(TxError::TxnIdReuse { id: t });
        }
    }
    if !contextdb_team::frontend::may_be_team(b, &refused) {
        return Ok(None);
    }
    let (access, version) = local_policy(k);
    let policy = LocalPolicy {
        access: access.as_ref(),
        version: &version,
    };
    let overlays: BTreeMap<TeamId, Replica> = (teams.iter())
        .map(|(t, j)| {
            (
                *t,
                j.queue
                    .overlay(j.store.replica(), &j.cfg.member, b.principal, &policy),
            )
        })
        .collect();
    let route = contextdb_team::frontend::translate(k, b, refused.clone(), &overlays)?;
    let contextdb_team::frontend::Route::Team { team, ops } = route else {
        return Ok(None);
    };
    let j = teams.get_mut(&team).ok_or(refused)?;
    let blobs = blobs
        .iter()
        .map(|b| (*blake3::hash(b).as_bytes(), b.clone()))
        .collect();
    match j.queue.propose_txn(
        j.store.replica(),
        &j.cfg.member,
        b.principal,
        ops,
        blobs,
        &policy,
        txn,
    ) {
        Ok(id) => Ok(Some((team, id))),
        Err(e) => Err(propose_error(e)),
    }
}

/// The proposal a client transaction id entered, in any joined team.
pub fn proposal_by_txn<'a>(
    teams: &'a BTreeMap<TeamId, Joined>,
    txn: &[u8; 16],
) -> Option<(TeamId, &'a contextdb_team::proposals::Proposal)> {
    (teams.iter()).find_map(|(t, j)| j.queue.by_txn(txn).map(|p| (*t, p)))
}

/// The idempotency hash of a request routed to a team: the batch as sent
/// (its ops, branch, channel, principal and id), which a retry repeats.
fn request_hash(b: &Batch) -> [u8; 32] {
    *blake3::hash(&b.canonical_bytes()).as_bytes()
}

/// A queue refusal as the client sees it (SPEC §6.1 catalog).
fn propose_error(e: contextdb_team::proposals::ProposeError) -> TxError {
    use contextdb_team::proposals::ProposeError as P;
    match e {
        P::Local(t) => t,
        P::EgressDenied { .. } => TxError::Team {
            code: "EgressDenied".into(),
            detail: String::new(),
        },
        P::Team(r) => TxError::Team {
            code: r.code.into(),
            detail: r.detail,
        },
        P::QueueConflict => TxError::Team {
            code: "conflicted".into(),
            detail: "queue".into(),
        },
        P::TooLarge(what) => TxError::Team {
            code: "ProposalTooLarge".into(),
            detail: what.into(),
        },
        P::Io(e) => TxError::Team {
            code: "ReplicaStalled".into(),
            detail: e.to_string(),
        },
    }
}

/// A team request on the actor (`ctx team status`, the publish steps).
#[derive(Clone, Debug)]
pub enum Call {
    Status {
        who: PrincipalId,
    },
    Prepare {
        yes: Option<[u8; 32]>,
        confirm: bool,
    },
    Begin {
        commit: String,
    },
    Refused,
    Why {
        path: String,
    },
    Rebuild,
    /// §8.6: step 1 (`Compact`) when none is pending, else step 2 (the
    /// successor, `--resume`).
    Compact,
    /// A portable proposal (`SetTeamAccess`, `TeamShred`) by `who`.
    Propose {
        who: PrincipalId,
        batch: Vec<u8>,
        blobs: Vec<Vec<u8>>,
    },
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    s.len()
        .is_multiple_of(2)
        .then(|| {
            (0..s.len() / 2)
                .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
                .collect()
        })
        .flatten()
}

/// §8.6 for the bound member, an admin: the unsigned `Compact` entry on
/// `g<n>`, or with a `Compact` pending, the successor `F` on `g<n+1>`.
/// Every checkpoint object is written to the repository first.
fn compact(j: &Joined, t: TeamId) -> Answer {
    use contextdb_team::ops::{TeamBatch, TeamOp};
    let r = j.store.replica();
    let member = j.cfg.member.as_str();
    let doc = match r.doc() {
        Some(d) => d,
        None => return fail("Genesis", "no team document"),
    };
    if !doc.is_admin(member) {
        return fail("NotAdmin", format!("{member} is not an admin"));
    }
    let repo = match Repo::open(&j.cfg.repo) {
        Ok(r) => r,
        Err(e) => return fail("ReplicaStalled", format!("{e:?}")),
    };
    let layout = j.cfg.refs;
    let refname = |g: u32| layout.remote_ref(&t, g);
    let accept = doc.accept_version.to_string();
    if let Some(p) = r.pending() {
        // Step 2: the replica after the accepted `Compact` is Σc.
        let Some(sc) = r.keyed_state() else {
            return fail("Genesis", "no team document");
        };
        let files = r.checkpoint_files(&sc, p.generation);
        let tree = match repo.write_tree(&files) {
            Ok(t) => t,
            Err(e) => return fail("ReplicaStalled", format!("{e:?}")),
        };
        if unhex(&tree).as_deref() != Some(p.tree.as_slice()) {
            return fail(
                "CheckpointMismatch",
                "the rebuilt checkpoint differs from the one Compact committed",
            );
        }
        let seq = p.compact_seq + 1;
        let trailers: Vec<(&str, String)> = vec![
            ("Ctx-Team", hex(&t)),
            ("Ctx-Team-Seq", seq.to_string()),
            ("Ctx-Author", member.to_owned()),
            ("Ctx-Accept-Version", accept),
            ("Ctx-Compacts", p.compact_oid.clone()),
        ];
        let payload = contextdb_team::commit::build(&tree, &[], "ctx team: checkpoint", &trailers);
        return Answer::Compact(Entry {
            unsigned: contextdb_team::publish::Unsigned {
                payload,
                tree: Vec::new(),
                proposals: Vec::new(),
                seq,
                parent: None,
            },
            tree,
            refname: refname(p.generation),
        });
    }
    // Step 1: Σc from the replica at the tip and a fresh proposal id.
    let Some(tip) = j.store.tip().map(str::to_owned) else {
        return fail("Genesis", "nothing to compact");
    };
    let id = contextdb_team::proposals::fresh_id();
    let seq = r.seq().map_or(0, |s| s + 1);
    let Some(sc) = r.sigma_c(member, id, seq) else {
        return fail("Genesis", "no team document");
    };
    let next = r.generation() + 1;
    let ckpt = match repo.write_tree(&r.checkpoint_files(&sc, next)) {
        Ok(t) => t,
        Err(e) => return fail("ReplicaStalled", format!("{e:?}")),
    };
    let Some(raw) = unhex(&ckpt) else {
        return fail("ReplicaStalled", "tree id");
    };
    let batch = TeamBatch {
        proposals: vec![vec![TeamOp::Compact {
            generation: next,
            tree: raw,
            digest: sc.digest(),
        }]],
    }
    .encode();
    let trailers: Vec<(&str, String)> = vec![
        ("Ctx-Team", hex(&t)),
        ("Ctx-Team-Seq", seq.to_string()),
        ("Ctx-Author", member.to_owned()),
        ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
        ("Ctx-Accept-Version", accept),
        ("Ctx-Proposal", hex(&id)),
    ];
    let tree = match repo.write_tree(&[("batch".to_owned(), batch)]) {
        Ok(t) => t,
        Err(e) => return fail("ReplicaStalled", format!("{e:?}")),
    };
    let payload = contextdb_team::commit::build(
        &tree,
        std::slice::from_ref(&tip),
        "ctx team: compact",
        &trailers,
    );
    Answer::Compact(Entry {
        unsigned: contextdb_team::publish::Unsigned {
            payload,
            tree: Vec::new(),
            proposals: vec![id],
            seq,
            parent: Some(tip),
        },
        tree,
        refname: refname(r.generation()),
    })
}

/// A prepared entry: the unsigned commit, its tree id (written to the
/// repository) and the ref to push.
#[derive(Clone, Debug)]
pub struct Entry {
    pub unsigned: contextdb_team::publish::Unsigned,
    pub tree: String,
    pub refname: String,
}

#[derive(Clone, Debug)]
pub enum Answer {
    Lines(Vec<String>),
    Prepared {
        entry: Option<Entry>,
        listed: Option<contextdb_team::publish::Listing>,
        conflicted: Vec<([u8; 16], String)>,
    },
    Error {
        code: String,
        detail: String,
    },
    /// §8.6: an entry to sign and push (no publish bookkeeping).
    Compact(Entry),
}

fn fail(code: &str, detail: impl Into<String>) -> Answer {
    Answer::Error {
        code: code.into(),
        detail: detail.into(),
    }
}

/// Runs `c` on the actor.
pub fn call(
    teams: &mut BTreeMap<TeamId, Joined>,
    k: &Kernel,
    team: Option<TeamId>,
    c: Call,
) -> Answer {
    let (access, version) = local_policy(k);
    let policy = LocalPolicy {
        access: access.as_ref(),
        version: &version,
    };
    if let Call::Status { who } = c {
        let mut lines = Vec::new();
        for (t, j) in teams.iter() {
            if team.is_some_and(|x| x != *t) {
                continue;
            }
            let r = j.store.replica();
            lines.push(format!(
                "team {} member {} seq {} generation {} digest {} import {:?}{}",
                hex(t),
                j.cfg.member,
                r.seq().map_or("-".to_owned(), |s| s.to_string()),
                r.generation(),
                r.digest().map_or("-".to_owned(), |d| hex(&d[..8])),
                j.status,
                if r.pending().is_some() {
                    " CompactPending"
                } else {
                    ""
                },
            ));
            // §6.2: only proposals `who` may see, and no count of others.
            for p in j.queue.listing_for(who, r, &policy) {
                lines.push(format!(
                    "  proposal {} by {} {:?}",
                    hex(&p.id),
                    p.author.0,
                    p.state
                ));
            }
        }
        return Answer::Lines(lines);
    }
    let Some(t) = team else {
        return fail("InvalidRequest", "a publish step names its team");
    };
    let Some(j) = teams.get_mut(&t) else {
        return fail("UnknownTeam", hex(&t));
    };
    match c {
        Call::Status { .. } => unreachable!("answered above"),
        Call::Prepare { yes, confirm } => {
            use contextdb_team::publish::Mode;
            // §8.6: with a `Compact` pending, publish waits for its successor.
            if j.store.replica().pending().is_some() {
                return fail("CompactPending", "publish waits for the successor");
            }
            let mode = match (yes, confirm, j.cfg.publish) {
                (Some(token), _, _) => Mode::Yes(token),
                (None, true, _) | (None, _, PublishMode::Confirm) => Mode::Confirm,
                (None, false, PublishMode::Auto) => Mode::Auto,
            };
            let prepared = match contextdb_team::publish::prepare(
                &j.store,
                &mut j.queue,
                &j.cfg.member,
                &policy,
                mode,
            ) {
                Ok(p) => p,
                Err(e) => return fail("PublishFailed", format!("{e:?}")),
            };
            let entry = match &prepared.entry {
                None => None,
                Some(u) => {
                    let repo = match Repo::open(&j.cfg.repo) {
                        Ok(r) => r,
                        Err(e) => return fail("ReplicaStalled", format!("{e:?}")),
                    };
                    let tree = match repo.write_tree(&u.tree) {
                        Ok(t) => t,
                        Err(e) => return fail("ReplicaStalled", format!("{e:?}")),
                    };
                    Some(Entry {
                        unsigned: u.clone(),
                        tree,
                        refname: j.cfg.refs.remote_ref(&t, j.store.replica().generation()),
                    })
                }
            };
            let answer = Answer::Prepared {
                entry,
                listed: prepared.listed.clone(),
                conflicted: prepared.conflicted.clone(),
            };
            j.prepared = Some(prepared);
            answer
        }
        Call::Begin { commit } => {
            let Some(p) = &j.prepared else {
                return fail("NotPrepared", "no prepared publish");
            };
            match contextdb_team::publish::begin(&mut j.queue, p, &version, &commit) {
                Ok(Ok(())) => Answer::Lines(vec![format!("published {commit}")]),
                Ok(Err(e)) => fail(&format!("{e:?}"), "prepare again"),
                Err(e) => fail("ReplicaStalled", e.to_string()),
            }
        }
        Call::Why { path } => {
            let r = j.store.replica();
            let Some(w) = r.why(&path) else {
                return fail("UnknownKey", path);
            };
            let short = |h: &[u8; 32]| hex(&h[..6]);
            let mut lines = vec![format!(
                "team record {path}@{}: {:?} at the root (seq {}){}",
                short(&w.head),
                w.status,
                r.seq().map_or("-".to_owned(), |s| s.to_string()),
                if w.base { ", base" } else { "" }
            )];
            for (enabled, premises) in w.rules {
                let named: Vec<String> = premises
                    .iter()
                    .map(|(p, k, c)| format!("{k:?} {p}@{}", short(c)))
                    .collect();
                lines.push(format!(
                    "  rule{}: {}",
                    if enabled { "" } else { " (disabled)" },
                    named.join(", ")
                ));
            }
            Answer::Lines(lines)
        }
        Call::Compact => compact(j, t),
        Call::Propose { who, batch, blobs } => {
            let ops = match contextdb_team::ops::TeamBatch::decode(&batch) {
                Ok(b) if b.proposals.len() == 1 => b.proposals.into_iter().next().expect("one"),
                _ => return fail("InvalidRequest", "one CXTB proposal"),
            };
            let blobs = blobs
                .into_iter()
                .map(|b| (*blake3::hash(&b).as_bytes(), b))
                .collect();
            match j
                .queue
                .propose(j.store.replica(), &j.cfg.member, who, ops, blobs, &policy)
            {
                Ok(id) => Answer::Lines(vec![format!("proposed {}", hex(&id))]),
                Err(e) => match propose_error(e) {
                    TxError::Team { code, detail } => fail(&code, detail),
                    other => fail(&format!("{other:?}"), ""),
                },
            }
        }
        Call::Rebuild => {
            let repo = match Repo::open(&j.cfg.repo) {
                Ok(r) => r,
                Err(e) => return fail("ReplicaStalled", format!("{e:?}")),
            };
            let dir = j.store.dir().to_owned();
            match TeamStore::rebuild(&dir, &repo) {
                Ok((store, status)) => {
                    j.store = store;
                    j.status = status.clone();
                    j.prepared = None;
                    Answer::Lines(vec![format!("{} rebuilt: {status:?}", hex(&t))])
                }
                Err(e) => fail("ReplicaStalled", format!("{e:?}")),
            }
        }
        Call::Refused => {
            let Some(p) = j.prepared.take() else {
                return Answer::Lines(Vec::new());
            };
            match contextdb_team::publish::refused(&mut j.queue, &p) {
                Ok(()) => Answer::Lines(vec!["queued again".into()]),
                Err(e) => fail("ReplicaStalled", e.to_string()),
            }
        }
    }
}

/// One durable announce (§8.3): a team ref about to move to `oid`, by the
/// git process `pid`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Announce {
    pub refname: String,
    pub oid: String,
    pub pid: u32,
}

const ANNOUNCE: &str = "announce";
const ANNOUNCE_LOCK: &str = "announce.lock";

fn with_announce_lock<T>(dir: &Path, f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    use nix::fcntl::{Flock, FlockArg};
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(ANNOUNCE_LOCK))?;
    let _lock = Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_, e)| std::io::Error::other(e.to_string()))?;
    f()
}

fn parse_announces(text: &str) -> Vec<Announce> {
    text.lines()
        .filter_map(|l| {
            let mut p = l.split(' ');
            Some(Announce {
                refname: p.next()?.to_owned(),
                oid: p.next()?.to_owned(),
                pid: p.next()?.parse().ok()?,
            })
        })
        .collect()
}

/// The hook's step 1: append and fsync, under the announce lock.
pub fn append_announce(dir: &Path, a: &Announce) -> std::io::Result<()> {
    use std::io::Write;
    with_announce_lock(dir, || {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(ANNOUNCE))?;
        f.write_all(format!("{} {} {}\n", a.refname, a.oid, a.pid).as_bytes())?;
        f.sync_all()
    })
}

/// Every durable announce of the team (`ctxd` at start).
pub fn read_announces(dir: &Path) -> Vec<Announce> {
    with_announce_lock(dir, || {
        Ok(parse_announces(
            &std::fs::read_to_string(dir.join(ANNOUNCE)).unwrap_or_default(),
        ))
    })
    .unwrap_or_default()
}

/// Drops resolved announces from the file, under the lock: what another
/// hook appended meanwhile is kept.
fn forget_announces(dir: &Path, resolved: &[Announce]) -> std::io::Result<()> {
    with_announce_lock(dir, || {
        let path = dir.join(ANNOUNCE);
        let all = parse_announces(&std::fs::read_to_string(&path).unwrap_or_default());
        let keep: String = all
            .iter()
            .filter(|a| !resolved.contains(a))
            .map(|a| format!("{} {} {}\n", a.refname, a.oid, a.pid))
            .collect();
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, keep)?;
        std::fs::File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        std::fs::File::open(dir)?.sync_all()
    })
}

fn pid_alive(pid: u32) -> bool {
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    let Ok(p) = i32::try_from(pid) else {
        return false;
    };
    matches!(kill(Pid::from_raw(p), None), Ok(()) | Err(Errno::EPERM))
}

/// Resolves pending announces after an import read the tip (§8.3): one is
/// done when the tracking ref contains its OID, or when its git process is
/// gone and the ref does not contain it (the transaction ended without
/// it). Git runs outside the lock reads take.
pub fn resolve_announces(store: &TeamStore, repo: &Repo, live: &LiveMap) {
    let (dir, team, tip) = (store.dir(), store.team(), store.tip());
    let pending: Vec<Announce> = {
        let l = live.lock().expect("team live");
        l.get(&team)
            .map(|e| {
                e.pending
                    .iter()
                    .map(|((r, o), p)| Announce {
                        refname: r.clone(),
                        oid: o.clone(),
                        pid: *p,
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    if pending.is_empty() {
        return;
    }
    let within = |oid: &str, t: &str| t == oid || repo.is_ancestor(oid, t).unwrap_or(false);
    // The tracking refs of this team on its remote: `<prefix><generation>`.
    let ours = store.tracking_ref(0);
    let prefix = &ours[..ours.len() - 1];
    let watched = store.replica().generation();
    let compacting = store.replica().pending().is_some();
    // Resolved: a ref import does not follow — another remote's, another
    // generation's, or any while a `Compact` is pending (its appended
    // entries are ignored, its successor's state is already mirrored; Q3,
    // review N1 and M4); or the replica (and so every branch) has the OID;
    // or its git process is gone and the ref, read again now, does not
    // contain it — the transaction ended without it. A ref read before the
    // process exited is never used for that (review finding 2).
    let resolved: Vec<Announce> = pending
        .into_iter()
        .filter(|a| {
            let generation = (a.refname.strip_prefix(prefix)).and_then(|g| g.parse::<u32>().ok());
            if compacting || generation != Some(watched) {
                return true;
            }
            if tip.is_some_and(|t| within(&a.oid, t)) {
                return true;
            }
            if pid_alive(a.pid) {
                return false;
            }
            let now = repo.resolve(&a.refname).ok().flatten();
            !now.is_some_and(|r| within(&a.oid, &r))
        })
        .collect();
    if resolved.is_empty() {
        return;
    }
    if let Err(e) = forget_announces(dir, &resolved) {
        eprintln!("ctxd: team {}: announce file: {e}", hex(&team));
    }
    let mut l = live.lock().expect("team live");
    if let Some(e) = l.get_mut(&team) {
        for a in resolved {
            e.pending.remove(&(a.refname, a.oid));
        }
    }
}

/// §11 (Q6): before catch-up creates or attaches a mirror revision with
/// bytes the team shredded, the store owner's `ShredContent` on `main`, as
/// its own batch — only when nothing local holds them (a team admin cannot
/// destroy a developer's private data): a record that is not a mirror, an
/// artifact's payload, a ValidationRecord's metadata, or a resolution note
/// (`notes`: those of `todo` a `ResolveArtifact` in the op log names;
/// review finding 10). `todo`: [`shred_todo`], less what is known kept.
/// Returns the contents kept for that reason.
pub fn shred_step<W>(
    w: &mut W,
    todo: &BTreeSet<ContentHash>,
    notes: &BTreeSet<ContentHash>,
) -> Result<Vec<ContentHash>, StoreError>
where
    W: catchup::Writer<Error = StoreError> + ?Sized,
{
    if todo.is_empty() {
        return Ok(Vec::new());
    }
    let k = w.kernel();
    let mut held: BTreeSet<ContentHash> = notes.intersection(todo).copied().collect();
    for b in k.branches() {
        let Some(v) = k.branch(b.id) else {
            continue;
        };
        let db = v.db();
        for r in (0..db.revision_count() as u32).map(RevisionId) {
            if let Ok(c) = db.revision_content(r)
                && todo.contains(&c)
                && db
                    .revision_logical(r)
                    .is_some_and(|l| db.logical_origin(l) == Origin::Local)
            {
                held.insert(c);
            }
        }
        held.extend(
            (v.artifacts())
                .map(|(_, a)| a.payload_hash)
                .filter(|h| todo.contains(h)),
        );
        held.extend(v.validation_metas().filter(|h| todo.contains(h)));
    }
    let mut kept = Vec::new();
    for &h in todo {
        if held.contains(&h) {
            kept.push(h);
            continue;
        }
        let mut b = Batch::new(vec![Op::ShredContent { content: h }]);
        b.channel = Channel::Admin;
        b.branch = MAIN;
        w.commit(&b)?;
    }
    Ok(kept)
}

/// The team's shredded contents the store has not shredded yet.
pub fn shred_todo(k: &Kernel, replica: &Replica) -> BTreeSet<ContentHash> {
    (replica.shredded().iter())
        .filter(|h| !k.is_shredded(h))
        .copied()
        .collect()
}

/// Which of `hashes` a `ResolveArtifact` note or a `RecordValidation`'s
/// metadata in the op log names (notes live only there), read through
/// `row`.
pub fn notes_in_log(
    last: Epoch,
    row: impl Fn(Epoch) -> Result<contextdb_store::log::LogRow, StoreError>,
    hashes: &BTreeSet<ContentHash>,
) -> Result<BTreeSet<ContentHash>, StoreError> {
    let mut found = BTreeSet::new();
    for e in 1..=last.0 {
        let batch = (row(Epoch(e))?.batch()).map_err(|e| StoreError::Io(format!("{e:?}")))?;
        for op in &batch.ops {
            // A note lives only here; a ValidationRecord's metadata too, once
            // a later record for the same (rule, vector) replaced it in the
            // state (review M3).
            let held = match op {
                Op::ResolveArtifact { note: Some(n), .. } => Some(n),
                Op::RecordValidation { meta: Some(m), .. } => Some(m),
                _ => None,
            };
            if let Some(h) = held
                && hashes.contains(h)
            {
                found.insert(*h);
            }
        }
    }
    Ok(found)
}

/// Whether a publish holds `<team dir>/publish.lock` (between its step 7
/// and its push nothing may requeue its proposals). The lock is a `flock`,
/// released by the kernel when its holder dies.
pub fn publish_in_flight(dir: &Path) -> bool {
    use nix::fcntl::{Flock, FlockArg};
    let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("publish.lock"))
    else {
        return false;
    };
    Flock::lock(f, FlockArg::LockExclusiveNonblock).is_err()
}
