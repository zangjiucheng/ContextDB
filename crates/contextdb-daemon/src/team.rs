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
    // Published proposals take their outcome from what was decided, or go
    // back to the queue when the chain does not hold them (§7.4).
    if let Err(e) = contextdb_team::publish::reconcile(&j.store, &mut j.queue, &j.cfg.member, &repo)
    {
        eprintln!("ctxd: team {}: reconcile: {e}", hex(&team));
    }
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
        e.tip = tip.clone();
        e.stopped = stopped.or_else(|| (!caught.is_empty()).then(|| caught.clone()));
    }
    resolve_announces(j.store.dir(), team, &repo, tip.as_deref(), live);
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
                "team {} member {} seq {} generation {} import {:?}",
                hex(t),
                j.cfg.member,
                r.seq().map_or("-".to_owned(), |s| s.to_string()),
                r.generation(),
                j.status,
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
                        refname: format!(
                            "refs/contextdb/team/{}/g{}",
                            hex(&t),
                            j.store.replica().generation()
                        ),
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
pub fn resolve_announces(dir: &Path, team: TeamId, repo: &Repo, tip: Option<&str>, live: &LiveMap) {
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
    let contains =
        |oid: &str| tip.is_some_and(|t| t == oid || repo.is_ancestor(oid, t).unwrap_or(false));
    let resolved: Vec<Announce> = pending
        .into_iter()
        .filter(|a| contains(&a.oid) || !pid_alive(a.pid))
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
