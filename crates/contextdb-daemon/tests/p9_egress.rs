//! P9 milestone 5 (ADR 0017 §7): egress. T105: no body of a lane whose
//! egress is off reaches a validator not marked `local`, the judge, the
//! embedder or the git mirror, checked with recording mocks and a real
//! repository; `Skipped{egress}` work runs once egress turns on or a local
//! validator is added.
use contextdb_daemon::{
    actor,
    candidates::{CandState, EMBEDDING},
    embedding::Settings,
    jobs::{self, JobTable},
    mirror::{self, REF_PREFIX},
    proto::*,
    service::{Identity, Service},
    validators::{Configured, Loaded, Ran, Scheduler},
};
use contextdb_model::*;
use contextdb_proto::docs::{Label, ValidatorKind};
use contextdb_proto::fsguard;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use contextdb_validate::config::{Filter, Limits};
use contextdb_validate::embed::{Embedder, Mock as MockEmbedder, VectorLimits};
use contextdb_validate::mock::Mock;
use contextdb_validate::{Outcome, Task, Validator, ValidatorError};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
type Svc = Service<SimDisk>;

/// Bodies of the `secret` lane carry this word; nothing else does.
const SECRET: &str = "zanzibar";

fn service() -> Svc {
    let disk = SimDisk::new();
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk, dir, false, 1 << 20).unwrap();
    Service::new(actor::spawn(store).unwrap(), log, events).unwrap()
}
fn temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ctx-p9-egress-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
fn h(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}
fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}
/// alice (the caller's uid) owns the store and bob is mapped. `repo` and
/// `secret` are read by every principal; `secret`'s egress is
/// `secret_egress`; `narrow` is alice's only (egress on, not readable by
/// all); `personal:bob` keeps the F6 default (egress off).
fn doc(secret_egress: bool) -> AccessDoc {
    let all = set(&["*members"]);
    let shared = LaneEntry {
        members: all.clone(),
        writers: all,
        curators: set(&["alice"]),
        ..LaneEntry::owner_only()
    };
    let lanes = BTreeMap::from([
        ("repo".to_owned(), shared.clone()),
        (
            "secret".to_owned(),
            LaneEntry {
                egress: secret_egress,
                ..shared
            },
        ),
        (
            "narrow".to_owned(),
            LaneEntry {
                members: set(&["alice"]),
                ..LaneEntry::owner_only()
            },
        ),
    ]);
    AccessDoc {
        version: 1,
        owner: "alice".into(),
        principals: BTreeMap::from([
            ("alice".to_owned(), fsguard::current_uid()),
            ("bob".to_owned(), 4242),
        ]),
        groups: BTreeMap::new(),
        lanes,
        code_lanes: LaneEntry::code_default(),
        personal_egress: false,
        default_lane: LaneEntry::owner_only(),
    }
}
fn set_access(s: &Svc, d: AccessDoc) {
    match s
        .handle(
            ADMIN,
            Request::SetAccess {
                doc: d,
                client_txn_id: None,
            },
        )
        .response
    {
        Response::Committed { .. } => {}
        other => panic!("{other:?}"),
    }
}
fn raw(s: &Svc, blobs: Vec<Vec<u8>>, ops: Vec<Op>) -> CommitResult {
    match s
        .handle(
            ADMIN,
            Request::Raw {
                blobs,
                batch: Batch::new(ops),
            },
        )
        .response
    {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
fn create(s: &Svc, lane: &str, key: &str, kind: RecordKind, body: &str) -> RevisionId {
    raw(
        s,
        vec![body.as_bytes().to_vec()],
        vec![Op::CreateLogical {
            key: key.into(),
            kind,
            scope_lane: lane.into(),
            content: h(body.as_bytes()),
            adopt_as_base: true,
        }],
    )
    .allocations
    .new_revisions[0]
}
fn move_head(s: &Svc, key: &str, body: &str) {
    let snap = s.actor.snapshot();
    let view = snap.view(MAIN);
    let l = view.logical_by_key(key).unwrap();
    let head = view.head(l).unwrap();
    drop(snap);
    raw(
        s,
        vec![body.as_bytes().to_vec()],
        vec![
            Op::AddRevision {
                logical: l.into(),
                content: h(body.as_bytes()),
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: l.into(),
                expected: head.into(),
                new: RevRef::New(0),
            },
        ],
    );
}
fn rule(s: &Svc, conclusion: RevisionId, premise: RevisionId) {
    raw(
        s,
        Vec::new(),
        vec![Op::AddRule {
            conclusion: conclusion.into(),
            incidences: vec![(premise.into(), EdgeKind::Sem)],
            enabled: true,
        }],
    );
}

/// A validator that keeps every body it is sent.
struct Recorder {
    name: String,
    kind: ValidatorKind,
    seen: Mutex<Vec<String>>,
}
impl Recorder {
    fn new(name: &str, kind: ValidatorKind) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            kind,
            seen: Mutex::new(Vec::new()),
        })
    }
    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
    fn saw_secret(&self) -> bool {
        self.seen.lock().unwrap().iter().any(|b| b.contains(SECRET))
    }
}
impl Validator for Recorder {
    fn kind(&self) -> ValidatorKind {
        self.kind
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "recorder"
    }
    fn validate(&self, task: &Task, _: Instant) -> Result<Outcome, ValidatorError> {
        let mut bodies = vec![task.conclusion.body.clone()];
        for p in &task.premises {
            bodies.push(p.asserted.body.clone());
            bodies.push(p.current.body.clone());
        }
        bodies.extend(task.pair.iter().map(|i| i.body.clone()));
        self.seen.lock().unwrap().push(bodies.join("\n"));
        Ok(Mock::outcome(Label::None))
    }
}
fn configured(v: Arc<Recorder>, local: bool, pairs: bool) -> Configured {
    Configured {
        filter: Filter::default(),
        validator: v,
        note: "test".into(),
        pairs,
        local,
    }
}
fn loaded(validators: Vec<Configured>) -> Loaded {
    Loaded {
        limits: Limits {
            workers: 1,
            lease_secs: 30,
            attempts: 3,
            llm_calls_per_hour: 200,
        },
        candidates: Default::default(),
        validators,
        error: None,
    }
}
fn scheduler(s: &Svc, validators: Vec<Configured>) -> Arc<Scheduler<SimDisk>> {
    let dir = temp("jobs");
    let mut sch = Scheduler::new(
        s.actor.clone(),
        JobTable::open(&dir.join(jobs::FILE)).unwrap(),
        None,
        loaded(validators),
    );
    Arc::get_mut(&mut sch).unwrap().retry_base = Duration::ZERO;
    sch.link_candidates(s.candidates.clone());
    sch
}
fn stat(sch: &Scheduler<SimDisk>, name: &str) -> u64 {
    let r = sch.report(ValidatorsAction::Status).unwrap();
    (r.stats.iter())
        .find(|c| c.name == name)
        .map(|c| c.value)
        .unwrap_or_else(|| panic!("no counter {name}"))
}
/// Runs rule jobs until none is claimable.
fn drain_jobs(sch: &Scheduler<SimDisk>) -> Vec<Ran> {
    sch.discover_once().unwrap();
    let mut out = Vec::new();
    while let Some(r) = sch.work_once().unwrap() {
        out.push(r);
        assert!(out.len() < 50);
    }
    out
}
/// Two Pending rule jobs: `open/c ← Sem open/p` (both in `repo`) and
/// `mixed/c ← Sem mixed/p`, whose premise lives in `secret`.
fn two_jobs(s: &Svc) {
    let p = create(s, "repo", "open/p", RecordKind::Fact, "the cache is redis");
    let c = create(
        s,
        "repo",
        "open/c",
        RecordKind::Decision,
        "use redis streams",
    );
    rule(s, c, p);
    let p = create(
        s,
        "secret",
        "mixed/p",
        RecordKind::Fact,
        &format!("the {SECRET} vendor holds the keys"),
    );
    let c = create(
        s,
        "repo",
        "mixed/c",
        RecordKind::Decision,
        "rotate keys yearly",
    );
    rule(s, c, p);
    move_head(s, "open/p", "the cache is memcached");
    move_head(s, "mixed/p", &format!("the {SECRET} vendor was replaced"));
}

/// T105, validators: a job reading a non-egress lane never reaches a
/// validator not marked `local`; it is `Skipped{egress}` with no attempt
/// used, and runs on a local validator once one is added.
#[test]
fn t105_jobs_reading_a_non_egress_lane_go_only_to_local_validators() {
    let s = service();
    set_access(&s, doc(false));
    two_jobs(&s);
    let remote = Recorder::new("remote", ValidatorKind::Deterministic);
    let sch = scheduler(&s, vec![configured(remote.clone(), false, false)]);
    let ran = drain_jobs(&sch);
    assert_eq!(ran, vec![Ran::Recorded(Verdict::Verified)], "only open/c");
    assert_eq!(remote.calls(), 1);
    assert!(!remote.saw_secret());
    assert_eq!(stat(&sch, "skipped_egress"), 1);
    let queued = sch.jobs.list(jobs::JobState::Queued, 10).unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].attempts, 0, "no attempt counted");
    // A local validator after the remote one: the job skips the remote one.
    let local = Recorder::new("local", ValidatorKind::Deterministic);
    sch.set_loaded(loaded(vec![
        configured(remote.clone(), false, false),
        configured(local.clone(), true, false),
    ]));
    assert_eq!(drain_jobs(&sch), vec![Ran::Recorded(Verdict::Verified)]);
    assert_eq!((remote.calls(), local.calls()), (1, 1));
    assert!(local.saw_secret() && !remote.saw_secret());
    assert_eq!(stat(&sch, "skipped_egress"), 0);
}

/// T105, re-queue: a skipped job runs on the remote validator once the
/// lane's egress turns on; without an access document every lane egresses
/// (single-user stores are unchanged).
#[test]
fn t105_skipped_jobs_run_once_egress_turns_on() {
    let s = service();
    set_access(&s, doc(false));
    two_jobs(&s);
    let remote = Recorder::new("remote", ValidatorKind::Deterministic);
    let sch = scheduler(&s, vec![configured(remote.clone(), false, false)]);
    assert_eq!(drain_jobs(&sch).len(), 1);
    assert!(!remote.saw_secret());
    set_access(&s, doc(true));
    assert_eq!(drain_jobs(&sch), vec![Ran::Recorded(Verdict::Verified)]);
    assert!(remote.saw_secret());

    let plain = service();
    two_jobs(&plain);
    let remote = Recorder::new("remote", ValidatorKind::Deterministic);
    let sch = scheduler(&plain, vec![configured(remote.clone(), false, false)]);
    assert_eq!(
        drain_jobs(&sch).len(),
        2,
        "no access document: no egress rule"
    );
}

/// Waits until the lexical candidates have caught up.
fn settle_candidates(s: &Svc) {
    let start = Instant::now();
    while s.candidates.watermark().unwrap() != Some(s.actor.snapshot().epoch) {
        assert!(start.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(5));
    }
}
/// A lexical pair in `lane`, as in P7's judge tests.
fn pair_in(s: &Svc, lane: &str) {
    for (i, w) in ["alpha", "bravo", "charlie", "delta", "echo", "golf"]
        .iter()
        .enumerate()
    {
        create(
            s,
            lane,
            &format!("{lane}/f{i}"),
            RecordKind::Fact,
            &format!("{w} {w} note on {w}"),
        );
    }
    create(
        s,
        lane,
        &format!("{lane}/uploads-fact"),
        RecordKind::Fact,
        &format!("{SECRET} uploads are stored on the local disk of each node"),
    );
    settle_candidates(s);
    create(
        s,
        lane,
        &format!("{lane}/uploads"),
        RecordKind::Decision,
        &format!("{SECRET} uploads are stored in the s3 bucket, never on local disk"),
    );
    settle_candidates(s);
}
fn drain_pairs(sch: &Scheduler<SimDisk>) -> Vec<Ran> {
    let mut out = Vec::new();
    while let Some(r) = sch.judge_once().unwrap() {
        out.push(r);
        assert!(out.len() < 50);
    }
    out
}

/// T105, the judge: a candidate pair in a non-egress lane is postponed for
/// a remote judge (no call) and judged by a local one; a `SetAccess` that
/// turns egress on wakes it for the remote judge.
#[test]
fn t105_pairs_in_a_non_egress_lane_are_judged_only_locally() {
    let s = service();
    set_access(&s, doc(false));
    pair_in(&s, "secret");
    assert!(
        (s.candidates.all().unwrap().iter()).any(|c| c.state == CandState::New),
        "a lexical pair exists"
    );
    let remote = Recorder::new("remote", ValidatorKind::Llm);
    let sch = scheduler(&s, vec![configured(remote.clone(), false, true)]);
    assert!(drain_pairs(&sch).is_empty());
    assert_eq!(remote.calls(), 0);
    assert!(stat(&sch, "pair_skipped_egress") >= 1);
    // Egress on: the SetAccess wakes the postponed pair.
    sch.discover_once().unwrap();
    set_access(&s, doc(true));
    sch.discover_once().unwrap();
    let ran = drain_pairs(&sch);
    assert!(ran.contains(&Ran::Kept(Label::None)), "{ran:?}");
    assert!(remote.saw_secret());

    // A local judge takes the pair while egress stays off.
    let s = service();
    set_access(&s, doc(false));
    pair_in(&s, "secret");
    let remote = Recorder::new("remote", ValidatorKind::Llm);
    let sch = scheduler(&s, vec![configured(remote.clone(), false, true)]);
    assert!(drain_pairs(&sch).is_empty());
    let local = Recorder::new("local", ValidatorKind::Llm);
    sch.set_loaded(loaded(vec![
        configured(remote.clone(), false, true),
        configured(local.clone(), true, true),
    ]));
    assert!(drain_pairs(&sch).contains(&Ran::Kept(Label::None)));
    assert!(local.saw_secret());
    assert_eq!(remote.calls(), 0);
}

/// An embedder that keeps every text it is sent.
struct RecordingEmbedder {
    inner: MockEmbedder,
    seen: Mutex<Vec<String>>,
}
impl Embedder for RecordingEmbedder {
    fn model(&self) -> &str {
        self.inner.model()
    }
    fn dims(&self) -> usize {
        self.inner.dims()
    }
    fn embed(&self, texts: &[String], deadline: Instant) -> Result<Vec<Vec<f32>>, ValidatorError> {
        self.seen.lock().unwrap().extend(texts.iter().cloned());
        self.inner.embed(texts, deadline)
    }
}
fn settle_vectors(s: &Svc) {
    let start = Instant::now();
    loop {
        let epoch = s.actor.snapshot().epoch;
        let v = s.vectors.get().unwrap();
        if matches!(v.state(epoch), IndexState::Ready { .. })
            && s.candidates.embed_watermark().unwrap() == Some(epoch)
        {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// T105, the embedder: a body only non-egress heads carry is never sent;
/// a vector shared through equal content is not used for a non-egress head
/// (ranking, embedding candidates); egress on embeds it.
#[test]
fn t105_the_embedder_never_receives_a_non_egress_body() {
    let s = service();
    set_access(&s, doc(false));
    let e = Arc::new(RecordingEmbedder {
        inner: MockEmbedder::new(64),
        seen: Mutex::new(Vec::new()),
    });
    s.start_vectors(
        Settings {
            embedder: e.clone(),
            max_batch: 4,
            timeout: Duration::from_secs(5),
            limits: VectorLimits {
                min_cosine: 0.0,
                ..VectorLimits::default()
            },
        },
        None,
    )
    .unwrap();
    let shared = "ledger closes monthly with the vendor";
    create(&s, "repo", "books", RecordKind::Decision, shared);
    create(&s, "secret", "books-copy", RecordKind::Decision, shared);
    create(
        &s,
        "secret",
        "vault",
        RecordKind::Decision,
        &format!("{SECRET} ledger closes monthly with the vendor"),
    );
    create(
        &s,
        "personal:bob",
        "notes",
        RecordKind::Fact,
        &format!("{SECRET} ledger notes"),
    );
    settle_vectors(&s);
    let seen = e.seen.lock().unwrap().clone();
    assert!(seen.iter().any(|t| t.contains("ledger closes")), "{seen:?}");
    assert!(!seen.iter().any(|t| t.contains(SECRET)), "{seen:?}");
    let snap = s.actor.snapshot();
    let view = snap.view(MAIN);
    let id = |k: &str| view.logical_by_key(k).unwrap();
    let v = s.vectors.get().unwrap();
    let q = v.query_vector("ledger closes monthly").unwrap();
    let ranked: Vec<LogicalId> = (contextdb_daemon::embedding::ranked(v, &view, &q))
        .into_iter()
        .map(|(l, _)| l)
        .collect();
    assert!(ranked.contains(&id("books")), "{ranked:?}");
    assert!(!ranked.contains(&id("books-copy")), "shared vector unused");
    let secret_revs: BTreeSet<RevisionId> = ["books-copy", "vault", "notes"]
        .iter()
        .map(|k| view.head(id(k)).unwrap())
        .collect();
    assert!(
        (s.candidates.all().unwrap().iter())
            .filter(|c| c.source & EMBEDDING != 0)
            .all(|c| !secret_revs.contains(&c.a) && !secret_revs.contains(&c.b)),
        "no embedding pair touches a non-egress head"
    );
    drop(snap);
    set_access(&s, doc(true));
    settle_vectors(&s);
    assert!(
        (e.seen.lock().unwrap().iter()).any(|t| t.contains(SECRET) && t.contains("closes")),
        "egress on: the secret lane is embedded"
    );
    assert!(
        !(e.seen.lock().unwrap().iter()).any(|t| t.contains("ledger notes")),
        "personal lanes stay off"
    );
}

fn git(repo: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
    )
}
fn export(s: &Svc, repo: &Path, readable_ok: bool) -> Result<MirrorReport, String> {
    match s
        .handle(
            ADMIN,
            Request::Mirror {
                action: MirrorAction::Export {
                    branch: None,
                    project: repo.display().to_string(),
                    force: false,
                    adopt: false,
                    readable_ok,
                    rebuild: false,
                },
            },
        )
        .response
    {
        Response::Mirror { report } => Ok(report),
        Response::Error { code, .. } => Err(code),
        other => panic!("{other:?}"),
    }
}
/// Top-level directories (decoded lanes) of the mirror's tip.
fn mirrored_lanes(repo: &Path) -> BTreeSet<String> {
    let (ok, out) = git(repo, &["ls-tree", "--name-only", &format!("{REF_PREFIX}0")]);
    assert!(ok);
    out.lines()
        .filter(|l| *l != "README.md")
        .map(|l| mirror::decode_segment(l).unwrap())
        .collect()
}

/// T105, the mirror: a multi-user store exports only egress lanes every
/// principal reads, drops the others' files with `Ctx-Egress-Removed`,
/// refuses a `.git` others can read unless `readable_ok`, and the tip
/// still validates against a replayed render (the access document is
/// committed state).
#[test]
fn t105_the_mirror_exports_only_egress_lanes_every_principal_reads() {
    let s = service();
    let root = temp("mirror");
    let (repo, data) = (root.join("repo"), root.join("data"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    s.set_data_dir(data);
    assert!(git(&repo, &["init", "-q"]).0);
    for (lane, key) in [
        ("repo", "a"),
        ("secret", "b"),
        ("narrow", "c"),
        ("personal:bob", "d"),
    ] {
        create(
            &s,
            lane,
            key,
            RecordKind::Decision,
            &format!("{SECRET} {key}"),
        );
    }
    // No access document: every lane, as in P8.
    export(&s, &repo, false).unwrap();
    assert_eq!(
        mirrored_lanes(&repo),
        set(&["repo", "secret", "narrow", "personal:bob"])
    );
    set_access(&s, doc(false));
    let git_dir = repo.join(".git");
    std::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(export(&s, &repo, false).unwrap_err(), "MirrorReadable");
    std::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let report = export(&s, &repo, false).unwrap();
    assert_eq!(mirrored_lanes(&repo), set(&["repo"]), "{:?}", report.lines);
    let (_, msg) = git(
        &repo,
        &["log", "-1", "--format=%B", &format!("{REF_PREFIX}0")],
    );
    assert!(
        msg.contains("Ctx-Egress-Removed: narrow, personal:bob, secret"),
        "{msg}"
    );
    let (_, tree) = git(
        &repo,
        &["ls-tree", "-r", "--name-only", &format!("{REF_PREFIX}0")],
    );
    let (_, bodies) = git(&repo, &["grep", "-c", SECRET, &format!("{REF_PREFIX}0")]);
    assert_eq!(bodies.lines().count(), 1, "only repo/a: {tree}");
    // Not cached: the tip validates by a replayed render at its epoch.
    let snap = s.actor.snapshot();
    let mut m = mirror::Mirror::new(&snap, &repo, None).unwrap();
    let r = m.export(MAIN, false, false).unwrap();
    assert!(
        r.lines
            .iter()
            .any(|l| l.contains("validates") || l.contains("unchanged")),
        "{:?}",
        r.lines
    );
    // `readable_ok` lets an owner export into a readable `.git`.
    std::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    export(&s, &repo, true).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
