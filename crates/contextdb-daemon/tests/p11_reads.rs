//! P11 milestone 3 (ADR 0019 §3.6, §3.7, §9 G2): a ctxd read that touches
//! a node the frontier can still change pulls first, then answers by the
//! read-shape table. S13-rows-e2e: an unsettled record reads
//! PENDING(StructuralUnknown) and an unrelated one stays CERTIFIED. T61:
//! a certified consumer never gets a positive answer eager maintenance
//! would not give (`ctx_check`, the hook gate's check, `expect_cert_valid`,
//! a `require_certified` merge). WHY, DIFF and mirror export pull, or
//! answer the retryable `Settling{epoch}`.
//!
//! The actor drains a frontier whenever it is idle, so whether a read
//! still finds one is a race. With `lazy_budget = 1` a hub cascade takes
//! many slices; a test repeats its round until the read found the
//! frontier, and checks every round's answer either way. The tests run one
//! at a time, so the 50 ms default wait is not spent on other tests' idle
//! slices.
use contextdb_daemon::{
    actor::ActorHandle,
    proto::*,
    server::{Daemon, Options},
    service::Identity,
};
use contextdb_model::*;
use contextdb_store::StdDisk;
use contextdb_testkit::{Shape, shape};
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

const N: usize = 300;
const PAYLOAD: &[u8] = b"payload";
/// Rounds a test tries to catch a read during a frontier.
const ROUNDS: usize = 30;
const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId::STORE_OWNER,
};
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId::STORE_OWNER,
};

/// Held by each test for its whole run.
static SERIAL: Mutex<()> = Mutex::new(());
fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}
fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ctxd-p11r-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
/// A daemon whose cascades take one unit per slice, with this
/// `read_settle_ms` (`None`: the default, 50 ms).
/// A read_settle wide enough that a pull always finishes, even on a loaded
/// test host: these tests check what a completed pull answers, not the
/// default budget's timing.
const PULL_MS: u64 = 10_000;
fn start(name: &str, read_settle_ms: Option<u64>) -> Daemon {
    let dir = tmp(name);
    let mut toml = "[maintenance]\nlazy_budget = 1\n".to_owned();
    if let Some(ms) = read_settle_ms {
        toml.push_str(&format!("read_settle_ms = {ms}\n"));
    }
    std::fs::write(dir.join("ctxd.toml"), toml).unwrap();
    let options = Options {
        full_sync: false,
        ..Options::default()
    };
    Daemon::start(&dir, &options).unwrap()
}
fn actor(d: &Daemon) -> &ActorHandle<StdDisk> {
    &d.service.actor
}
fn call(d: &Daemon, id: Identity, req: Request) -> Response {
    d.service.handle(id, req).response
}
fn admin(d: &Daemon, b: Batch, blobs: Vec<Vec<u8>>) -> Response {
    let batch = Batch {
        channel: Channel::Admin,
        ..b
    };
    call(d, ADMIN, Request::Raw { blobs, batch })
}
/// Waits on publications until the published snapshot has no frontier.
fn settled(a: &ActorHandle<StdDisk>) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let snap = a.snapshot();
        if !snap.db().has_frontier() {
            return;
        }
        let g = snap.generation;
        drop(snap);
        assert!(Instant::now() < deadline, "never settled");
        a.wait_generation(g, Duration::from_secs(1));
    }
}
/// A hub of `N` revisions (keys `shape/k`): 0, 1 and 2 are bases and
/// every other revision concludes a rule on all three. Returns the
/// conclusion a cascade from revision 0 settles last (the highest rank).
fn hub(d: &Daemon) -> String {
    let content = *blake3::hash(PAYLOAD).as_bytes();
    for mut b in shape(Shape::Hub, N, 42).batches {
        for op in &mut b.ops {
            if let Op::CreateLogical { content: c, .. } = op {
                *c = content;
            }
        }
        let r = admin(d, b, vec![PAYLOAD.to_vec()]);
        assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    }
    settled(actor(d));
    let snap = actor(d).snapshot();
    let view = snap.view(MAIN);
    let last = (3..N)
        .max_by_key(|k| view.rank(RevisionId(*k as u32)))
        .unwrap();
    format!("shape/{last}")
}
/// Asserts (`true`) or retracts revision 0, the hub's shared base.
fn toggle(d: &Daemon, on: bool) -> (Epoch, bool) {
    let rev = RevisionId(0).into();
    let op = if on {
        Op::AssertBase { rev }
    } else {
        Op::RetractBase { rev }
    };
    match admin(d, Batch::new(vec![op]), Vec::new()) {
        Response::Committed { result, settling } => (result.epoch, settling),
        other => panic!("{other:?}"),
    }
}
/// Retracts the base: a cascade acknowledged before it settles.
fn retract(d: &Daemon) -> Epoch {
    let (e, settling) = toggle(d, false);
    assert!(settling, "the cascade exceeds the budget");
    e
}
/// Asserts the base again and waits for the cascade.
fn restore(d: &Daemon) {
    toggle(d, true);
    settled(actor(d));
}
fn create(d: &Daemon, branch: BranchId, key: &str) {
    let content = *blake3::hash(key.as_bytes()).as_bytes();
    let mut b = Batch::new(vec![Op::CreateLogical {
        origin: Default::default(),
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content,
        adopt_as_base: true,
    }]);
    b.branch = branch;
    let r = admin(d, b, vec![key.as_bytes().to_vec()]);
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
}
fn session(d: &Daemon) -> SessionId {
    let r = call(
        d,
        AGENT,
        Request::SessionStart {
            harness: "test".into(),
            repo: "r".into(),
            head_sha: "h".into(),
            parent: None,
            branch: None,
            git_branch: None,
        },
    );
    match r {
        Response::Session { session } => session,
        other => panic!("{other:?}"),
    }
}
fn read(d: &Daemon, s: SessionId, key: &str, mode: Mode) -> Response {
    call(
        d,
        AGENT,
        Request::Read {
            binding: Binding::root(s),
            target: Target::Key(key.into()),
            mode,
            level: None,
        },
    )
}
/// `ctx_check` (`gate: None`) or the hook gate's check: its verdict.
fn check(d: &Daemon, s: SessionId, gate: Option<GateMode>) -> Validation {
    let r = call(
        d,
        AGENT,
        Request::Check {
            binding: Binding::root(s),
            cert_id: None,
            live_pos: Vec::new(),
            action: None,
            mode: gate,
            harness_version: String::new(),
            strict: false,
        },
    );
    match r {
        Response::Check { validation, .. } => validation,
        other => panic!("{other:?}"),
    }
}
fn valid(v: &Validation) -> bool {
    matches!(v, Validation::Valid | Validation::ValidRecertified { .. })
}
fn error(r: &Response) -> Option<(&str, &str)> {
    match r {
        Response::Error { code, detail } => Some((code, detail)),
        _ => None,
    }
}
/// The epoch's `Settling` detail (SPEC §6.2).
fn is_settling(r: &Response, e: Epoch) -> bool {
    error(r).is_some_and(|(code, detail)| {
        code == "Settling" && detail.starts_with(&format!("epoch {}:", e.0))
    })
}
/// A session that read `key` certified and holds a valid certificate.
fn certified_session(d: &Daemon, key: &str) -> SessionId {
    let s = session(d);
    let r = read(d, s, key, Mode::Certified);
    assert!(matches!(r, Response::Read { .. }), "{r:?}");
    assert_eq!(check(d, s, None), Validation::Valid);
    s
}

/// S13-rows-e2e with `read_settle_ms = 0`: a certified read of the
/// unsettled dependent is `NotCertified` with PENDING(StructuralUnknown),
/// never a certified read; an unrelated record stays CERTIFIED; once
/// drained the dependent reads the eager answer.
#[test]
fn s13_rows_an_unsettled_dependent_reads_pending_and_an_unrelated_record_certified() {
    let _serial = serial();
    let d = start("s13", Some(0));
    let a = actor(&d);
    let dep = hub(&d);
    create(&d, MAIN, "unrelated");
    let s = session(&d);
    let mut pending = 0;
    for _ in 0..ROUNDS {
        restore(&d);
        assert!(matches!(
            read(&d, s, &dep, Mode::Certified),
            Response::Read { .. }
        ));
        retract(&d);
        let r = read(&d, s, &dep, Mode::Certified);
        let (code, detail) =
            error(&r).unwrap_or_else(|| panic!("certified while eager is not: {r:?}"));
        assert_eq!(code, "NotCertified", "{detail}");
        if detail.ends_with("Pending(StructuralUnknown)") {
            pending += 1;
        } else {
            assert!(detail.contains("Stale("), "{detail}");
        }
        let r = read(&d, s, "unrelated", Mode::Certified);
        let Response::Read { item, .. } = r else {
            panic!("{r:?}")
        };
        assert_eq!(item.summary, Summary::Certified);
        if pending > 0 {
            break;
        }
    }
    assert!(pending > 0, "no read found the frontier");
    settled(a);
    let snap = a.snapshot();
    let eager = snap.log.replay_kernel_to(snap.epoch).unwrap();
    let logical = snap.view(MAIN).logical_by_key(&dep).unwrap();
    let head = snap.view(MAIN).head(logical).unwrap();
    let want = eager.branch(MAIN).unwrap().status(head).unwrap();
    drop(snap);
    assert!(matches!(want, Summary::Stale(_)), "{want:?}");
    let r = read(&d, s, &dep, Mode::Advisory);
    let Response::Read { item, .. } = r else {
        panic!("{r:?}")
    };
    assert_eq!(item.summary, want);
    let r = read(&d, s, &dep, Mode::Certified);
    assert_eq!(error(&r).map(|(c, _)| c), Some("NotCertified"));
}

/// T61 through `ctx_check` with `read_settle_ms = 0`: a certificate whose
/// required entry is unsettled is never valid. The hook gate's check has
/// its own deadline, pulls and answers the eager verdict.
#[test]
fn t61_ctx_check_is_never_valid_on_an_unsettled_entry_and_the_gate_pulls() {
    let _serial = serial();
    let d = start("t61-zero", Some(0));
    let a = actor(&d);
    let dep = hub(&d);
    let s = certified_session(&d, &dep);
    let mut pending = 0;
    for _ in 0..ROUNDS {
        restore(&d);
        assert_eq!(check(&d, s, None), Validation::Valid);
        retract(&d);
        let v = check(&d, s, None);
        assert!(!valid(&v), "valid on a retracted base: {v:?}");
        if matches!(v, Validation::Pending { .. }) {
            pending += 1;
            break;
        }
    }
    assert!(pending > 0, "no check found the frontier");
    // The gate, during a fresh frontier.
    restore(&d);
    retract(&d);
    let gate = check(&d, s, Some(GateMode::Warn));
    settled(a);
    let eager = check(&d, s, None);
    assert!(matches!(eager, Validation::Invalid { .. }), "{eager:?}");
    assert_eq!(gate, eager);
}

/// T61 through `ctx_check` with the default 50 ms: the check pulls the
/// required entry's rank and answers the eager verdict.
#[test]
fn t61_ctx_check_pulls_and_answers_the_eager_verdict() {
    let _serial = serial();
    let d = start("t61-default", Some(PULL_MS));
    let a = actor(&d);
    let dep = hub(&d);
    let s = certified_session(&d, &dep);
    for _ in 0..3 {
        restore(&d);
        assert_eq!(check(&d, s, None), Validation::Valid);
        retract(&d);
        let pulled = check(&d, s, None);
        settled(a);
        let eager = check(&d, s, None);
        assert!(matches!(eager, Validation::Invalid { .. }), "{eager:?}");
        assert_eq!(pulled, eager);
    }
}

/// T61 on writes: the actor settles before it validates `expect_cert_valid`
/// and before a `require_certified` merge checks certification, so both
/// refuse exactly as eager maintenance would, during a frontier.
#[test]
fn t61_expect_cert_valid_and_a_require_certified_merge_refuse_as_eager() {
    let _serial = serial();
    let d = start("t61-writes", Some(0));
    let dep = hub(&d);
    let s = certified_session(&d, &dep);
    let r = admin(
        &d,
        Batch::new(vec![Op::CreateBranch {
            name: "feat".into(),
        }]),
        Vec::new(),
    );
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    let feat = actor(&d).snapshot().kernel.branch_by_name("feat").unwrap();
    create(&d, feat, "feat/x");
    for round in 0..3 {
        restore(&d);
        retract(&d);
        let content = format!("after/{round}");
        let r = call(
            &d,
            AGENT,
            Request::Commit {
                binding: Binding::root(s),
                ops: vec![Op::CreateLogical {
                    origin: Default::default(),
                    key: content.clone(),
                    kind: RecordKind::Fact,
                    scope_lane: "repo".into(),
                    content: *blake3::hash(content.as_bytes()).as_bytes(),
                    adopt_as_base: true,
                }],
                blobs: vec![content.into_bytes()],
                expect_cert_valid: true,
                client_txn_id: None,
            },
        );
        let Response::Refused { validation } = r else {
            panic!("{r:?}")
        };
        assert!(
            matches!(validation, Validation::Invalid { .. }),
            "not the eager verdict: {validation:?}"
        );
        restore(&d);
        retract(&d);
        let r = admin(
            &d,
            Batch::new(vec![Op::Merge {
                source: feat,
                require_certified: true,
                certify: vec![LogicalRef::Key(dep.clone())],
            }]),
            Vec::new(),
        );
        assert_eq!(
            error(&r).map(|(c, _)| c),
            Some("MergeNotCertified"),
            "{r:?}"
        );
    }
}

/// WHY with `read_settle_ms = 0` answers `Settling{epoch}` while its
/// target is unsettled; with 50 ms it pulls and answers the settled proof.
#[test]
fn why_answers_settling_or_pulls_the_settled_proof() {
    let _serial = serial();
    let why = |d: &Daemon, key: &str| {
        let target = Target::Key(key.into());
        call(d, AGENT, Request::Why { target, depth: 4 })
    };
    let zero = start("why-zero", Some(0));
    let dep = hub(&zero);
    let mut answered = 0;
    for _ in 0..ROUNDS {
        restore(&zero);
        let e = retract(&zero);
        let r = why(&zero, &dep);
        if is_settling(&r, e) {
            answered += 1;
            break;
        }
        assert!(matches!(r, Response::Why { .. }), "{r:?}");
    }
    assert!(answered > 0, "no WHY found the frontier");

    let d = start("why-default", Some(PULL_MS));
    let dep = hub(&d);
    for _ in 0..3 {
        restore(&d);
        retract(&d);
        let pulled = why(&d, &dep);
        let not = call(
            &d,
            AGENT,
            Request::WhyNot {
                target: Target::Key(dep.clone()),
                depth: 4,
            },
        );
        settled(actor(&d));
        assert!(matches!(pulled, Response::Why { .. }), "{pulled:?}");
        assert_eq!(pulled, why(&d, &dep));
        assert!(matches!(not, Response::WhyNot { .. }), "{not:?}");
    }
}

/// DIFF at the frontier's epoch needs every node settled: `Settling` with
/// `read_settle_ms = 0`, else the settled diff, equal to the diff after
/// the drain.
#[test]
fn diff_at_the_frontier_epoch_is_settling_or_the_settled_diff() {
    let _serial = serial();
    let diff = |d: &Daemon, e: Epoch| {
        let (from, to) = (Epoch(e.0 - 1), e);
        call(d, AGENT, Request::Diff { from, to })
    };
    let zero = start("diff-zero", Some(0));
    hub(&zero);
    let mut answered = 0;
    for _ in 0..ROUNDS {
        restore(&zero);
        let e = retract(&zero);
        let r = diff(&zero, e);
        if is_settling(&r, e) {
            answered += 1;
            break;
        }
        settled(actor(&zero));
        assert_eq!(r, diff(&zero, e));
    }
    assert!(answered > 0, "no DIFF found the frontier");

    let d = start("diff-default", Some(PULL_MS));
    hub(&d);
    for _ in 0..3 {
        restore(&d);
        let e = retract(&d);
        let pulled = diff(&d, e);
        settled(actor(&d));
        let Response::Diff { diff: settled_diff } = diff(&d, e) else {
            panic!()
        };
        assert_eq!(settled_diff.changes.len(), N - 2);
        assert_eq!(pulled, Response::Diff { diff: settled_diff });
    }
}

fn git(repo: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .current_dir(repo)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}
fn export(d: &Daemon, repo: &Path) -> Response {
    let action = MirrorAction::Export {
        branch: None,
        project: repo.display().to_string(),
        force: false,
        adopt: false,
        readable_ok: false,
        rebuild: false,
    };
    call(d, ADMIN, Request::Mirror { action })
}
/// An export that committed nothing: the tree equals its tip's.
fn unchanged(r: &Response) -> bool {
    match r {
        Response::Mirror { report } => report.lines.iter().any(|l| l.contains("unchanged at")),
        other => panic!("{other:?}"),
    }
}

/// Mirror export during a frontier pulls to the end and renders the
/// settled tree (an export after the drain commits nothing); with a zero
/// deadline it answers `Settling` instead of rendering the frontier.
#[test]
fn mirror_export_renders_only_a_settled_tree() {
    let _serial = serial();
    let d = start("mirror", Some(PULL_MS));
    let repo = tmp("mirror-repo");
    git(&repo, &["init", "-q"]);
    hub(&d);
    for _ in 0..3 {
        restore(&d);
        retract(&d);
        let r = export(&d, &repo);
        assert!(matches!(r, Response::Mirror { .. }), "{r:?}");
        settled(actor(&d));
        assert!(unchanged(&export(&d, &repo)), "rendered from a frontier");
    }
    d.service.set_export_settle(Duration::ZERO);
    let mut answered = 0;
    for _ in 0..ROUNDS {
        restore(&d);
        let e = retract(&d);
        let r = export(&d, &repo);
        if is_settling(&r, e) {
            answered += 1;
            break;
        }
        settled(actor(&d));
        assert!(unchanged(&export(&d, &repo)), "rendered from a frontier");
    }
    assert!(answered > 0, "no export found the frontier");
    let _ = std::fs::remove_dir_all(&repo);
}
