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
use std::collections::BTreeMap;
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
        }
    }
    fn encode(&self) -> String {
        format!(
            "repo {}\nmember {}\nkey {}\npublish {}\nstrict_push {}\npush_timeout_ms {}\nlag_check_ms {}\nwarn_after_s {}\n",
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
        let opened = (|| -> Result<Joined, String> {
            let cfg = TeamConfig::load(&dir)?;
            let store = TeamStore::open(&dir).map_err(|e| format!("{e:?}"))?;
            let queue = Queue::open(&dir).map_err(|e| e.to_string())?;
            Ok(Joined {
                store,
                queue,
                cfg,
                status: Status::UpToDate,
            })
        })();
        match opened {
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
    /// Fetches `ctxd` runs itself, until the ref is read back.
    pub in_flight: u32,
    /// Import stopped (`ReplicaStalled`, `ReplicaDiverged`,
    /// `ReplicaIncomplete`).
    pub stopped: Option<String>,
}

impl Live {
    /// The position a read compares a branch's watermark with, or `None`
    /// (every read over the team's mirrors lags).
    pub fn position(&self) -> Option<Vec<u8>> {
        if !self.pending.is_empty() || self.in_flight > 0 || self.stopped.is_some() {
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

/// The contents a catch-up batch names, whose bytes the store must hold.
pub fn contents(b: &Batch) -> impl Iterator<Item = ContentHash> + '_ {
    b.ops.iter().filter_map(|op| match op {
        Op::CreateLogical { content, .. } | Op::AddRevision { content, .. } => Some(*content),
        Op::MirrorAttach { head, .. } => Some(*head),
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
    let tip = repo
        .resolve(&j.store.tracking_ref(j.store.replica().generation()))
        .ok()
        .flatten();
    // Bring every open branch to what the replica decided, even when the
    // import stopped part-way (a prefix is decided).
    let caught = match catch_up(team, j.store.replica(), j.store.history()) {
        Ok(()) => String::new(),
        Err((b, e)) => format!("; ReplicaStalled on branch {}: {e:?}", b.0),
    };
    {
        let mut l = live.lock().expect("team live");
        let e = l.entry(team).or_default();
        e.tip = tip;
        e.stopped = stopped.or_else(|| (!caught.is_empty()).then(|| caught.clone()));
    }
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
pub fn propose(
    teams: &mut BTreeMap<TeamId, Joined>,
    k: &Kernel,
    b: &Batch,
    blobs: &[Vec<u8>],
    refused: TxError,
) -> Routed {
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
    match j.queue.propose(
        j.store.replica(),
        &j.cfg.member,
        b.principal,
        ops,
        blobs,
        &policy,
    ) {
        Ok(id) => Ok(Some((team, id))),
        Err(e) => Err(propose_error(e)),
    }
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
