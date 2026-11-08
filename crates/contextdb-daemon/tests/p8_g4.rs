//! P8 G4 robustness (ADR 0016 §10).
//!
//! - **Kill points.** A real `ctxd` child aborts (no destructors, as
//!   SIGKILL; `CTX_P8_CRASH`, debug builds only) during `ctx reindex`,
//!   between embedding batches, between `commit-tree` and `update-ref`, and
//!   between `update-ref` and the cache write. Each restart ends with a
//!   consistent projection or ref: never a half-moved ref, a leftover
//!   commit object is harmless, and no false `MirrorModified`.
//! - **Disk full** in `recall.sqlite` and `vectors.sqlite` (a page cap, so
//!   SQLite fails as on a full disk): recall keeps answering, the state is
//!   `Failed`, and the work completes once space returns.
//! - **Git lock contention:** a held `<ref>.lock` fails the export without
//!   moving the ref (not `MirrorRaced`); a held `packed-refs.lock` blocks
//!   pruning a packed backup without harm.
//! - **A truncated log** (`doctor --truncate-before`, then new commits at
//!   the same epoch numbers) gives `MirrorStale`, not `MirrorModified`.
use contextdb_daemon::{
    actor,
    embedding::Settings,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_proto::client::Client;
use contextdb_store::{OpenOptions, SimDisk, StdDisk, Store, session::SessionLog};
use contextdb_validate::embed::{Mock, VectorLimits};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};

fn temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ctx-p8-g4-{tag}-{}-{}",
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
fn git(repo: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .stderr(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
    )
}
fn repo(dir: &Path) -> PathBuf {
    let r = dir.join("repo");
    std::fs::create_dir_all(&r).unwrap();
    assert!(git(&r, &["init", "-q"]).0);
    r
}
/// A `ctxd` child on `dir/db`; returns it and its admin socket.
fn spawn(dir: &Path, crash: Option<&str>) -> (Child, PathBuf) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ctxd"));
    cmd.arg(dir.join("db"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    match crash {
        Some(step) => cmd.env("CTX_P8_CRASH", step),
        None => cmd.env_remove("CTX_P8_CRASH"),
    };
    let mut child = cmd.spawn().unwrap();
    let out = BufReader::new(child.stdout.take().unwrap());
    let mut admin = None;
    for line in out.lines() {
        let line = line.unwrap();
        if let Some(p) = line.strip_prefix("listening ")
            && p.ends_with("admin.sock")
        {
            admin = Some(PathBuf::from(p));
            break;
        }
    }
    (child, admin.expect("ctxd printed its admin socket"))
}
fn wait_crash(child: &mut Child, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            assert!(!s.success(), "{what}: {s:?}");
            return;
        }
        assert!(Instant::now() < deadline, "{what}: ctxd did not crash");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn stop(mut child: Child, c: &mut Client) {
    let _ = c.call(&Request::Shutdown);
    let _ = child.wait();
}
fn create(c: &mut Client, key: &str, body: &str) {
    let r = c
        .call(&Request::Raw {
            blobs: vec![body.as_bytes().to_vec()],
            batch: Batch::new(vec![Op::CreateLogical {
                key: key.into(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: h(body.as_bytes()),
                adopt_as_base: true,
            }]),
        })
        .unwrap();
    assert!(matches!(r.response, Response::Committed { .. }), "{r:?}");
}
fn index(c: &mut Client) -> (Epoch, IndexReport) {
    let r = c.call(&Request::Status).unwrap();
    match r.response {
        Response::Status { index, .. } => (r.epoch, index),
        other => panic!("{other:?}"),
    }
}
fn until(c: &mut Client, what: &str, ok: impl Fn(&IndexReport, Epoch) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (e, i) = index(c);
        if ok(&i, e) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: {i:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn ready(s: &IndexState, e: Epoch) -> bool {
    matches!(s, IndexState::Ready { watermark } if *watermark == e)
}
fn mirror(c: &mut Client, repo: &Path) -> Result<MirrorReport, String> {
    let r = c
        .call(&Request::Mirror {
            action: MirrorAction::Export {
                branch: None,
                project: repo.display().to_string(),
                force: false,
                adopt: false,
                readable_ok: false,
                rebuild: false,
            },
        })
        .unwrap();
    match r.response {
        Response::Mirror { report } => Ok(report),
        Response::Error { code, .. } => Err(code),
        other => panic!("{other:?}"),
    }
}
fn debug_only() -> bool {
    if !cfg!(debug_assertions) {
        eprintln!("crash points are compiled out of release builds; skipped");
    }
    cfg!(debug_assertions)
}

/// Killed during a full rebuild (after its first chunk): the restart
/// rebuilds and the index is complete.
#[test]
fn kill_during_reindex() {
    if !debug_only() {
        return;
    }
    let dir = temp("reindex");
    let (child, sock) = spawn(&dir, None);
    let mut c = Client::connect(&sock).unwrap();
    for i in 0..300 {
        create(&mut c, &format!("k{i:03}"), &format!("otter note {i}"));
    }
    until(&mut c, "indexed", |i, e| ready(&i.fts, e));
    stop(child, &mut c);
    let (mut child, sock) = spawn(&dir, Some("reindex-chunk"));
    let mut c = Client::connect(&sock).unwrap();
    let _ = c.call(&Request::Reindex {
        target: ReindexTarget::Fts,
    });
    wait_crash(&mut child, "reindex");
    let (child, sock) = spawn(&dir, None);
    let mut c = Client::connect(&sock).unwrap();
    until(&mut c, "rebuilt", |i, e| ready(&i.fts, e));
    let rows = contextdb_store::recall::RecallIndex::open(Some(&dir.join("db/recall.sqlite")))
        .unwrap()
        .rows()
        .unwrap();
    assert_eq!(rows, 300);
    stop(child, &mut c);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Killed after the first embedding batch is stored: the restart embeds
/// only what is missing.
#[test]
fn kill_during_an_embedding_batch() {
    if !debug_only() {
        return;
    }
    let dir = temp("embed");
    let (child, sock) = spawn(&dir, None);
    let mut c = Client::connect(&sock).unwrap();
    for i in 0..20 {
        create(&mut c, &format!("k{i}"), &format!("otter note {i}"));
    }
    stop(child, &mut c);
    std::fs::write(
        dir.join("db/embeddings.toml"),
        "[embedder]\ntype = \"mock\"\ndims = 16\nmax_batch = 4\n",
    )
    .unwrap();
    // The crash may come before ctxd prints its sockets.
    let mut child = Command::new(env!("CARGO_BIN_EXE_ctxd"))
        .arg(dir.join("db"))
        .env("CTX_P8_CRASH", "embed-batch")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_crash(&mut child, "embedding");
    let stored = |dir: &Path| {
        contextdb_store::vectors::VectorStore::open(Some(&dir.join("db/vectors.sqlite")))
            .unwrap()
            .count("mock-bow-16")
            .unwrap()
    };
    let before = stored(&dir);
    assert!(before < 20, "the crash came before every batch: {before}");
    let (child, sock) = spawn(&dir, None);
    let mut c = Client::connect(&sock).unwrap();
    until(&mut c, "embedded", |i, e| ready(&i.vectors, e));
    stop(child, &mut c);
    assert_eq!(stored(&dir), 20);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Killed between `commit-tree` and `update-ref`: the ref never moved, the
/// leftover commit is harmless, and the next export succeeds.
#[test]
fn kill_between_commit_tree_and_update_ref() {
    if !debug_only() {
        return;
    }
    let dir = temp("committree");
    let repo = repo(&dir);
    let (child, sock) = spawn(&dir, None);
    let mut c = Client::connect(&sock).unwrap();
    create(&mut c, "a", "first");
    mirror(&mut c, &repo).unwrap();
    let before = git(&repo, &["rev-parse", "refs/contextdb/b0"]).1;
    stop(child, &mut c);
    let (mut child, sock) = spawn(&dir, Some("mirror-commit-tree"));
    let mut c = Client::connect(&sock).unwrap();
    create(&mut c, "b", "second");
    let _ = c.call(&Request::Mirror {
        action: MirrorAction::Export {
            branch: None,
            project: repo.display().to_string(),
            force: false,
            adopt: false,
            readable_ok: false,
            rebuild: false,
        },
    });
    wait_crash(&mut child, "commit-tree");
    assert_eq!(
        git(&repo, &["rev-parse", "refs/contextdb/b0"]).1,
        before,
        "never half-moved"
    );
    let (child, sock) = spawn(&dir, None);
    let mut c = Client::connect(&sock).unwrap();
    let r = mirror(&mut c, &repo).unwrap();
    assert!(r.lines.iter().any(|l| l.contains("->")), "{r:?}");
    assert!(git(&repo, &["fsck", "--strict"]).0);
    stop(child, &mut c);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Killed between `update-ref` and the cache write: the restart validates
/// the tip instead of calling it modified.
#[test]
fn kill_between_update_ref_and_the_cache_write() {
    if !debug_only() {
        return;
    }
    let dir = temp("updateref");
    let repo = repo(&dir);
    let (mut child, sock) = spawn(&dir, Some("mirror-update-ref"));
    let mut c = Client::connect(&sock).unwrap();
    create(&mut c, "a", "first");
    let _ = c.call(&Request::Mirror {
        action: MirrorAction::Export {
            branch: None,
            project: repo.display().to_string(),
            force: false,
            adopt: false,
            readable_ok: false,
            rebuild: false,
        },
    });
    wait_crash(&mut child, "update-ref");
    let moved = git(&repo, &["rev-parse", "refs/contextdb/b0"]).1;
    let (child, sock) = spawn(&dir, None);
    let mut c = Client::connect(&sock).unwrap();
    let r = mirror(&mut c, &repo).unwrap();
    assert!(
        r.lines
            .iter()
            .any(|l| l.contains("not cached, and it validates")),
        "{r:?}"
    );
    assert!(r.lines.iter().any(|l| l.contains("unchanged")), "{r:?}");
    assert_eq!(git(&repo, &["rev-parse", "refs/contextdb/b0"]).1, moved);
    stop(child, &mut c);
    let _ = std::fs::remove_dir_all(&dir);
}

fn service_at(dir: &Path) -> Service<SimDisk> {
    let disk = SimDisk::new();
    let db = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), db, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk, db, false, 1 << 20).unwrap();
    Service::with_recall(
        actor::spawn(store).unwrap(),
        log,
        events,
        Some(&dir.join("recall.sqlite")),
    )
    .unwrap()
}
fn raw(s: &Service<SimDisk>, key: &str, body: &str) {
    let r = s.handle(
        ADMIN,
        Request::Raw {
            blobs: vec![body.as_bytes().to_vec()],
            batch: Batch::new(vec![Op::CreateLogical {
                key: key.into(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: h(body.as_bytes()),
                adopt_as_base: true,
            }]),
        },
    );
    assert!(matches!(r.response, Response::Committed { .. }), "{r:?}");
}
fn wait_state(s: &Service<SimDisk>, what: &str, ok: impl Fn(&IndexReport, Epoch) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let e = s.actor.snapshot().epoch;
        let r = s.index_report(e);
        if ok(&r, e) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: {r:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Disk full in both projections: recall keeps answering (keys, and the
/// rows already written), the state is `Failed`, and everything catches up
/// once space returns.
#[test]
fn disk_full_in_recall_and_vectors() {
    let dir = temp("full");
    let s = service_at(&dir);
    s.start_vectors(
        Settings {
            embedder: Arc::new(Mock::new(256)),
            max_batch: 8,
            timeout: Duration::from_secs(5),
            limits: VectorLimits::default(),
        },
        Some(&dir.join("vectors.sqlite")),
    )
    .unwrap();
    for i in 0..10 {
        raw(&s, &format!("early-{i}"), &format!("otter early {i}"));
    }
    wait_state(&s, "ready", |r, e| ready(&r.fts, e) && ready(&r.vectors, e));
    s.limit_recall(Some(0));
    s.vectors.get().unwrap().limit_store(Some(0));
    let big = "lorem ipsum dolor ".repeat(4_000);
    for i in 0..20 {
        raw(&s, &format!("late-{i}"), &format!("otter late {i} {big}"));
    }
    wait_state(&s, "failed", |r, _| {
        matches!(r.fts, IndexState::Failed { .. }) && matches!(r.vectors, IndexState::Failed { .. })
    });
    let b = match s
        .handle(
            AGENT,
            Request::SessionAttach {
                harness: "claude".into(),
                harness_session: "A".into(),
                repo: "/repo".into(),
                head_sha: "h".into(),
                branch: None,
                git_branch: None,
            },
        )
        .response
    {
        Response::Attached { session, .. } => Binding::root(session),
        other => panic!("{other:?}"),
    };
    let recall = |q: &str| match s
        .handle(
            AGENT,
            Request::Recall {
                binding: b.clone(),
                query: q.into(),
                scope: String::new(),
                mode: Mode::Certified,
                limit: 100,
                index_wait_ms: 0,
                opts: RecallOpts::default(),
            },
        )
        .response
    {
        Response::Recall { items, index, .. } => (items, index),
        other => panic!("{other:?}"),
    };
    let (items, index) = recall("otter");
    assert!(matches!(index.fts, IndexState::Failed { .. }), "{index:?}");
    assert!(
        items.iter().filter(|i| i.key.starts_with("early-")).count() == 10,
        "old rows serve"
    );
    let (items, _) = recall("late-7");
    assert_eq!(
        items.first().map(|i| i.key.as_str()),
        Some("late-7"),
        "keys never lag"
    );
    // Space returns: both catch up.
    s.limit_recall(None);
    s.vectors.get().unwrap().limit_store(None);
    wait_state(&s, "recovered", |r, e| {
        ready(&r.fts, e) && ready(&r.vectors, e)
    });
    let (items, _) = recall("otter");
    assert_eq!(items.len(), 30);
    assert_eq!(s.vectors.get().unwrap().stored(), 30);
    s.stop_indexer();
    let _ = std::fs::remove_dir_all(&dir);
}

fn mirror_service(dir: &Path) -> Service<SimDisk> {
    let s = service_at(dir);
    s.set_data_dir(dir.to_path_buf());
    s
}
fn export(s: &Service<SimDisk>, repo: &Path) -> Result<MirrorReport, String> {
    match s
        .handle(
            ADMIN,
            Request::Mirror {
                action: MirrorAction::Export {
                    branch: None,
                    project: repo.display().to_string(),
                    force: false,
                    adopt: false,
                    readable_ok: false,
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

/// A held `<ref>.lock` fails the export with the ref unchanged (not a
/// race); a held `packed-refs.lock` makes pruning a packed backup a no-op.
#[test]
fn git_lock_contention() {
    let dir = temp("locks");
    let repo = repo(&dir);
    let s = mirror_service(&dir);
    raw(&s, "a", "first");
    export(&s, &repo).unwrap();
    let tip = git(&repo, &["rev-parse", "refs/contextdb/b0"]).1;
    let lock = repo.join(".git/refs/contextdb/b0.lock");
    std::fs::write(&lock, "").unwrap();
    raw(&s, "b", "second");
    assert_eq!(export(&s, &repo), Err("MirrorFailed".into()));
    assert_eq!(git(&repo, &["rev-parse", "refs/contextdb/b0"]).1, tip);
    std::fs::remove_file(&lock).unwrap();
    export(&s, &repo).unwrap();
    assert_ne!(git(&repo, &["rev-parse", "refs/contextdb/b0"]).1, tip);
    // A packed backup ref with packed-refs.lock held.
    assert!(git(&repo, &["update-ref", "refs/contextdb-backup/b0-1", &tip]).0);
    assert!(git(&repo, &["pack-refs", "--all"]).0);
    let plock = repo.join(".git/packed-refs.lock");
    std::fs::write(&plock, "").unwrap();
    let pruned = s.handle(
        ADMIN,
        Request::Mirror {
            action: MirrorAction::Prune {
                project: repo.display().to_string(),
                older_than_secs: 0,
            },
        },
    );
    assert!(
        matches!(pruned.response, Response::Mirror { .. }),
        "{pruned:?}"
    );
    assert!(
        git(&repo, &["rev-parse", "refs/contextdb-backup/b0-1"]).0,
        "kept while locked"
    );
    std::fs::remove_file(&plock).unwrap();
    s.handle(
        ADMIN,
        Request::Mirror {
            action: MirrorAction::Prune {
                project: repo.display().to_string(),
                older_than_secs: 0,
            },
        },
    );
    assert!(
        !git(
            &repo,
            &["rev-parse", "-q", "--verify", "refs/contextdb-backup/b0-1"]
        )
        .0
    );
    s.stop_indexer();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `doctor --truncate-before` then new commits at the same epoch numbers:
/// the old tip names a history that no longer exists, `MirrorStale`.
#[test]
fn a_truncated_log_is_stale_not_modified() {
    use contextdb_daemon::server::{Daemon, Options};
    let dir = temp("truncate");
    let repo = repo(&dir);
    let db = dir.join("db");
    let d = Daemon::start(&db, &Options::default()).unwrap();
    let mut c = Client::connect(d.socket(Channel::Admin)).unwrap();
    for i in 0..4 {
        create(&mut c, &format!("k{i}"), &format!("body {i}"));
    }
    mirror(&mut c, &repo).unwrap();
    d.stop();
    let disk = StdDisk::default();
    // Other tests in this binary spawn git meanwhile; a child between fork
    // and exec briefly holds the released lock (`StdDisk::lock` waits it out).
    let plan = contextdb_store::doctor::plan_truncate_before(&disk, &db, Epoch(3)).unwrap();
    contextdb_store::doctor::truncate_before(&disk, &db, &plan).unwrap();
    let d = Daemon::start(&db, &Options::default()).unwrap();
    let mut c = Client::connect(d.socket(Channel::Admin)).unwrap();
    for i in 0..3 {
        create(&mut c, &format!("other{i}"), &format!("another {i}"));
    }
    assert_eq!(mirror(&mut c, &repo), Err("MirrorStale".into()));
    // The ref deleted by hand: the cached commit names the old log, so the
    // export starts fresh instead of following it (review m4).
    assert!(git(&repo, &["update-ref", "-d", "refs/contextdb/b0"]).0);
    let report = mirror(&mut c, &repo).unwrap();
    assert!(
        report.lines.iter().any(|l| l.contains("created fresh")),
        "{:?}",
        report.lines
    );
    assert_eq!(
        git(&repo, &["rev-list", "--count", "refs/contextdb/b0"]).1,
        "1"
    );
    d.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
