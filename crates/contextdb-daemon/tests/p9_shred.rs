//! P9 milestone 6c (ADR 0017 §8.4): the purge after a shred, on a real
//! data directory.
//!
//! T103 (daemon half):
//! - after `ctx shred` reports `purged`, no file the daemon controls holds
//!   the shredded text: packs, keys, the op_log, snapshots and every
//!   projection with its `-wal` (the session log and the outbox are
//!   outside G-SHRED, §8.6);
//! - a shred committed with no purge (a kill right after the commit)
//!   shows as pending in `ctx status` and is purged at the next start.
use contextdb_daemon::{
    proto::*,
    server::{Daemon, Options},
};
use contextdb_model::*;
use contextdb_proto::client::Client;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A word only the shredded record holds.
const SECRET: &str = "zanzibarquokka";

fn temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ctx-p9-shred-{tag}-{}-{}",
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
fn raw(c: &mut Client, blobs: Vec<Vec<u8>>, batch: Batch) -> Response {
    c.call(&Request::Raw { blobs, batch }).unwrap().response
}
fn create(c: &mut Client, key: &str, kind: RecordKind, body: &str) {
    let r = raw(
        c,
        vec![body.as_bytes().to_vec()],
        Batch::new(vec![Op::CreateLogical {
            key: key.into(),
            kind,
            scope_lane: "repo".into(),
            content: h(body.as_bytes()),
            adopt_as_base: true,
        }]),
    );
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
}
fn status(c: &mut Client) -> (Epoch, IndexReport, Vec<Epoch>) {
    let r = c.call(&Request::Status).unwrap();
    match r.response {
        Response::Status {
            index,
            purge_pending,
            ..
        } => (r.epoch, index, purge_pending),
        other => panic!("{other:?}"),
    }
}
/// Waits until recall and vectors are Ready at the head.
fn indexed(c: &mut Client) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (e, i, _) = status(c);
        let ready =
            |s: &IndexState| matches!(s, IndexState::Ready { watermark } if *watermark == e);
        if ready(&i.fts) && ready(&i.vectors) {
            return;
        }
        assert!(Instant::now() < deadline, "{i:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
/// Records that pair lexically with the secret one, so candidates (and
/// their rows) exist for it, and the secret record itself.
fn populate(c: &mut Client) {
    for (i, w) in ["alpha", "bravo", "charlie", "delta", "echo"]
        .iter()
        .enumerate()
    {
        create(
            c,
            &format!("f{i}"),
            RecordKind::Fact,
            &format!("{w} {w} note"),
        );
    }
    create(
        c,
        "uploads-fact",
        RecordKind::Fact,
        "user uploads are stored on the local disk of each node",
    );
    create(
        c,
        "vendor/keys",
        RecordKind::Decision,
        &format!("user uploads are stored with the {SECRET} vendor, never on local disk"),
    );
}
/// Every file under `dir` holding `needle`, outside the session log.
fn holders(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            let t = e.file_type().unwrap();
            if t.is_dir() {
                if p.file_name().is_some_and(|n| n == "sessions") {
                    continue; // outside G-SHRED (ADR 0017 §8.6)
                }
                stack.push(p);
            } else if t.is_file()
                && std::fs::read(&p).is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle))
            {
                out.push(p);
            }
        }
    }
    out
}
fn start(dir: &Path) -> (Daemon, Client) {
    let d = Daemon::start(dir, &Options::default()).unwrap();
    let c = Client::connect(d.socket(Channel::Admin)).unwrap();
    (d, c)
}

/// After `ctx shred` reports `purged`, the text is in no file.
#[test]
fn t103_after_the_purge_no_file_holds_the_text() {
    let dir = temp("purged");
    std::fs::write(
        dir.join("embeddings.toml"),
        "[embedder]\ntype = \"mock\"\ndims = 32\n",
    )
    .unwrap();
    let (d, mut c) = start(&dir);
    populate(&mut c);
    indexed(&mut c);
    // Before: the text is on disk, at least in recall.sqlite (or its WAL).
    assert!(
        !holders(&dir, SECRET.as_bytes()).is_empty(),
        "the projection holds the text before the shred"
    );
    let r = c
        .call(&Request::Shred {
            key: "vendor/keys".into(),
            rev: None,
            with_reviews: false,
            confirm: true,
            client_txn_id: None,
        })
        .unwrap()
        .response;
    let Response::Shred {
        epoch: Some(e),
        purged,
        ..
    } = r
    else {
        panic!("{r:?}")
    };
    assert!(purged, "purged at epoch {}", e.0);
    assert_eq!(status(&mut c).2, Vec::<Epoch>::new());
    d.stop();
    assert_eq!(holders(&dir, SECRET.as_bytes()), Vec::<PathBuf>::new());
    // Recovery keeps it so, and the payload stays shredded.
    let (d, mut c) = start(&dir);
    assert_eq!(status(&mut c).2, Vec::<Epoch>::new());
    d.stop();
    assert_eq!(holders(&dir, SECRET.as_bytes()), Vec::<PathBuf>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A shred committed with no purge (as if the daemon died right after the
/// commit) is pending in `ctx status`, and the next start purges it.
#[test]
fn t103_a_shred_left_unpurged_is_purged_at_the_next_start() {
    let dir = temp("pending");
    let (d, mut c) = start(&dir);
    populate(&mut c);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !matches!(status(&mut c), (e, IndexReport { fts: IndexState::Ready { watermark }, .. }, _) if watermark == e)
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let content = h(format!(
        "user uploads are stored with the {SECRET} vendor, never on local disk"
    )
    .as_bytes());
    let mut b = Batch::new(vec![Op::ShredContent { content }]);
    b.channel = Channel::Admin;
    let r = raw(&mut c, vec![], b);
    let Response::Committed { result } = r else {
        panic!("{r:?}")
    };
    // The key log is purged by the store with the commit; the projections
    // were not touched.
    assert_eq!(status(&mut c).2, vec![result.epoch]);
    d.stop();
    let (d, mut c) = start(&dir);
    assert_eq!(status(&mut c).2, Vec::<Epoch>::new(), "purged at start");
    d.stop();
    assert_eq!(holders(&dir, SECRET.as_bytes()), Vec::<PathBuf>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

/// T104 (daemon half, ADR 0017 §10): `ctxd` refuses a store marked
/// mid-migration, and a store whose projection lacks the format stamp
/// (created before P9); after `ctx migrate` it starts, with its records.
#[test]
fn t104_the_daemon_refuses_an_incomplete_migration() {
    use contextdb_daemon::server::StartError;
    use contextdb_store::{StdDisk, StoreError, migrate, migrate5, purge};
    let dir = temp("migrate");
    let (d, mut c) = start(&dir);
    create(&mut c, "k", RecordKind::Fact, "kept across the migration");
    d.stop();
    drop(d);
    std::fs::write(dir.join(migrate5::MARKER), b"5\n").unwrap();
    match Daemon::start(&dir, &Options::default()) {
        Err(StartError::Store(StoreError::MigrationIncomplete { step: 6 })) => {}
        Err(e) => panic!("{e:?}"),
        Ok(_) => panic!("started mid-migration"),
    }
    migrate::migrate(&StdDisk::default(), &dir).unwrap();
    // A projection file from before P9: no stamp.
    let p = dir.join(purge::FILES[1]);
    let _ = std::fs::remove_file(&p);
    rusqlite::Connection::open(&p)
        .unwrap()
        .execute_batch("CREATE TABLE old(x TEXT);")
        .unwrap();
    match Daemon::start(&dir, &Options::default()) {
        Err(StartError::Store(StoreError::MigrationIncomplete { step: 4 })) => {}
        Err(e) => panic!("{e:?}"),
        Ok(_) => panic!("started with an unstamped projection"),
    }
    migrate::migrate(&StdDisk::default(), &dir).unwrap();
    let (d, mut c) = start(&dir);
    assert_eq!(status(&mut c).0, Epoch(1), "the record survived");
    d.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

mod jobs {
    //! T103's validator rows: the purge marks jobs `Skipped` and deletes
    //! their output; a call in flight when the payload is shredded has its
    //! output dropped at the token fence.
    use contextdb_daemon::{
        actor,
        jobs::{self, JobState, JobTable},
        proto::*,
        service::{Identity, Service},
        validators::{Configured, Loaded, Ran, Scheduler},
    };
    use contextdb_model::*;
    use contextdb_proto::docs::{Label, ValidatorKind};
    use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
    use contextdb_validate::config::{Filter, Limits};
    use contextdb_validate::mock::Mock;
    use contextdb_validate::{Outcome, Task, Validator, ValidatorError};
    use std::path::Path;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const ADMIN: Identity = Identity {
        channel: Channel::Admin,
        principal: PrincipalId(0),
    };
    type Svc = Service<SimDisk>;
    fn h(b: &[u8]) -> ContentHash {
        *blake3::hash(b).as_bytes()
    }
    fn service() -> Arc<Svc> {
        let disk = SimDisk::new();
        let dir = Path::new("/db");
        let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
        let (log, events, _) = SessionLog::open(disk, dir, false, 1 << 20).unwrap();
        Arc::new(Service::new(actor::spawn(store).unwrap(), log, events).unwrap())
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
    /// `c ← Sem p`, then p moves: one Pending job reading c's body.
    fn pending_job(s: &Svc) {
        let r = raw(
            s,
            vec![b"p: postgres".to_vec(), b"c: zanzibar joins".to_vec()],
            vec![
                Op::CreateLogical {
                    key: "p".into(),
                    kind: RecordKind::Fact,
                    scope_lane: "repo".into(),
                    content: h(b"p: postgres"),
                    adopt_as_base: true,
                },
                Op::CreateLogical {
                    key: "c".into(),
                    kind: RecordKind::Decision,
                    scope_lane: "repo".into(),
                    content: h(b"c: zanzibar joins"),
                    adopt_as_base: true,
                },
            ],
        );
        let (p, c) = (
            r.allocations.new_revisions[0],
            r.allocations.new_revisions[1],
        );
        raw(
            s,
            vec![],
            vec![Op::AddRule {
                conclusion: c.into(),
                incidences: vec![(p.into(), EdgeKind::Sem)],
                enabled: true,
            }],
        );
        raw(
            s,
            vec![b"p: mongodb".to_vec()],
            vec![
                Op::AddRevision {
                    logical: LogicalRef::Key("p".into()),
                    content: h(b"p: mongodb"),
                    adopt_as_base: true,
                },
                Op::SetHead {
                    logical: LogicalRef::Key("p".into()),
                    expected: p.into(),
                    new: RevRef::New(0),
                },
            ],
        );
    }
    fn shred_c(s: &Svc) -> Response {
        s.handle(
            ADMIN,
            Request::Shred {
                key: "c".into(),
                rev: None,
                with_reviews: false,
                confirm: true,
                client_txn_id: None,
            },
        )
        .response
    }
    fn scheduler(s: &Arc<Svc>, v: Arc<dyn Validator>) -> Arc<Scheduler<SimDisk>> {
        let dir = std::env::temp_dir().join(format!(
            "ctx-p9-shred-jobs-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sch = Scheduler::new(
            s.actor.clone(),
            JobTable::open(&dir.join(jobs::FILE)).unwrap(),
            None,
            Loaded {
                limits: Limits {
                    workers: 1,
                    lease_secs: 30,
                    attempts: 3,
                    llm_calls_per_hour: 200,
                },
                candidates: Default::default(),
                validators: vec![Configured {
                    filter: Filter::default(),
                    validator: v,
                    note: "test".into(),
                    pairs: false,
                    local: false,
                }],
                error: None,
            },
        );
        sch.link_candidates(s.candidates.clone());
        let _ = s.validators.set(sch.clone());
        sch
    }

    /// A queued job reading a shredded payload becomes `Skipped`.
    #[test]
    fn the_purge_skips_jobs_that_read_the_payload() {
        let s = service();
        pending_job(&s);
        let sch = scheduler(&s, Arc::new(Mock::new("m", ValidatorKind::Deterministic)));
        assert_eq!(sch.discover_once().unwrap(), 1);
        let Response::Shred { purged: true, .. } = shred_c(&s) else {
            panic!()
        };
        let all = sch.jobs.all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].state, JobState::Skipped);
        assert_eq!(all[0].outcome_meta, None);
        assert_eq!(sch.work_once().unwrap(), None, "nothing left to run");
    }

    /// A validator whose call is in flight when the payload is shredded:
    /// its output (which quotes the body) is dropped, nothing is committed.
    struct ShredsDuringTheCall {
        svc: OnceLock<Arc<Svc>>,
        seen: Mutex<Vec<String>>,
    }
    impl Validator for ShredsDuringTheCall {
        fn kind(&self) -> ValidatorKind {
            ValidatorKind::Llm
        }
        fn name(&self) -> &str {
            "slow"
        }
        fn version(&self) -> &str {
            "test"
        }
        fn validate(&self, task: &Task, _: Instant) -> Result<Outcome, ValidatorError> {
            self.seen.lock().unwrap().push(task.conclusion.body.clone());
            let r = shred_c(self.svc.get().unwrap());
            assert!(matches!(r, Response::Shred { .. }), "{r:?}");
            let mut o = Mock::outcome(Label::Conflict);
            o.rationale = format!("quotes: {}", task.conclusion.body);
            Ok(o)
        }
    }
    #[test]
    fn output_of_a_call_in_flight_at_the_shred_is_dropped() {
        let s = service();
        pending_job(&s);
        let v = Arc::new(ShredsDuringTheCall {
            svc: OnceLock::new(),
            seen: Mutex::new(Vec::new()),
        });
        let _ = v.svc.set(s.clone());
        let sch = scheduler(&s, v.clone());
        let epoch = s.actor.snapshot().epoch;
        sch.discover_once().unwrap();
        let ran = sch.work_once().unwrap();
        assert!(
            matches!(ran, Some(Ran::SkippedShredded) | Some(Ran::Lost)),
            "{ran:?}"
        );
        assert_eq!(v.seen.lock().unwrap().len(), 1, "the call happened");
        let snap = s.actor.snapshot();
        assert_eq!(snap.epoch.0, epoch.0 + 1, "only the shred committed");
        assert!(
            snap.view(MAIN)
                .artifacts()
                .all(|(_, a)| a.ty != ArtifactType::PendingReview)
        );
        let all = sch.jobs.all().unwrap();
        assert_eq!(all[0].state, JobState::Skipped);
        assert_eq!(all[0].outcome_meta, None);
        let _ = Duration::ZERO;
    }
}

/// Security review m11: a key-log rewrite that failed inside the shred's
/// commit is retried by `Service::purge` (no restart needed), and `ctx
/// status` sees the result at once.
#[test]
fn a_failed_key_log_rewrite_is_retried_by_the_purge() {
    use contextdb_daemon::{actor, service::Service};
    use contextdb_store::{Durability, OpenOptions, SimDisk, Store, session::SessionLog};
    let disk = SimDisk::new();
    let dir = Path::new("/db");
    let (store, _) = Store::open(
        disk.clone(),
        dir,
        &OpenOptions {
            create: true,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    let s = Service::new(actor::spawn(store).unwrap(), log, events).unwrap();
    let admin = contextdb_daemon::service::Identity {
        channel: Channel::Admin,
        principal: PrincipalId(0),
    };
    let body = b"the zanzibar vendor holds the keys".to_vec();
    let content = h(&body);
    let r = s.handle(
        admin,
        Request::Raw {
            blobs: vec![body],
            batch: Batch::new(vec![Op::CreateLogical {
                key: "k".into(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content,
                adopt_as_base: true,
            }]),
        },
    );
    assert!(matches!(r.response, Response::Committed { .. }), "{r:?}");
    // The rewrite's rename fails; the shred's own commit never renames.
    disk.fail_nth("rename", 0);
    let mut b = Batch::new(vec![Op::ShredContent { content }]);
    b.channel = Channel::Admin;
    let r = s.handle(
        admin,
        Request::Raw {
            blobs: vec![],
            batch: b,
        },
    );
    let Response::Committed { result } = r.response else {
        panic!("{r:?}")
    };
    let pending = |s: &Service<SimDisk>| match s.handle(admin, Request::Status).response {
        Response::Status { purge_pending, .. } => purge_pending,
        other => panic!("{other:?}"),
    };
    assert_eq!(pending(&s), vec![result.epoch], "the key purge failed");
    let state = s.purge().unwrap();
    assert!(state.complete() && state.keys >= result.epoch, "{state:?}");
    assert_eq!(pending(&s), Vec::<Epoch>::new());
    for name in disk.list(&dir.join("keys")).unwrap() {
        let bytes = disk.read(&dir.join("keys").join(&name)).unwrap();
        assert!(!bytes.windows(32).any(|w| w == content), "{name}");
    }
}
