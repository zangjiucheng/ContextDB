//! P7 milestone 6b (ADR 0015 §5): validators with `pairs = true` judge
//! contradiction candidates. `conflict` opens a non-gating PendingReview
//! on the newer side naming both; `none` commits nothing; neither opens a
//! Conflict, and an LLM batch never carries a `RecordValidation`.
use contextdb_daemon::{
    actor,
    candidates::{CandState, Candidate, Judge},
    jobs::{self, JobTable},
    proto::*,
    service::{Identity, Service},
    validators::{Configured, LLM_PRINCIPAL, Loaded, Ran, Scheduler, pair_txn},
};
use contextdb_model::*;
use contextdb_proto::docs::{Label, ReviewDoc, Suggestion, ValidationMeta, ValidatorKind};
use contextdb_store::{OpenOptions, Payload, SimDisk, Store, session::SessionLog};
use contextdb_validate::ValidatorError;
use contextdb_validate::config::{Filter, Limits};
use contextdb_validate::mock::{Mock, Script};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
type Svc = Service<SimDisk>;
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
        "ctx-p7-judge-{tag}-{}-{}",
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
fn create(s: &Svc, key: &str, kind: RecordKind, body: &str) -> RevisionId {
    raw(
        s,
        vec![body.as_bytes().to_vec()],
        vec![Op::CreateLogical {
            key: key.into(),
            kind,
            scope_lane: "repo".into(),
            content: h(body.as_bytes()),
            adopt_as_base: true,
        }],
    )
    .allocations
    .new_revisions[0]
}
fn settle(s: &Svc) {
    let start = Instant::now();
    while s.candidates.watermark().unwrap() != Some(s.actor.snapshot().epoch) {
        assert!(start.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(5));
    }
}
/// Fillers, then a Decision `a` and a Fact `b` that lexically pair.
fn setup(s: &Svc) -> (RevisionId, RevisionId) {
    for (i, w) in ["alpha", "bravo", "charlie", "delta", "echo", "golf"]
        .iter()
        .enumerate()
    {
        create(
            s,
            &format!("f{i}"),
            RecordKind::Fact,
            &format!("{w} {w} note on {w}"),
        );
    }
    let b = create(
        s,
        "uploads-fact",
        RecordKind::Fact,
        "user uploads are stored on the local disk of each node",
    );
    // `a` arrives in a later pass, so its query produces the pair.
    settle(s);
    let a = create(
        s,
        "uploads",
        RecordKind::Decision,
        "user uploads are stored in the s3 bucket, never on local disk",
    );
    settle(s);
    (a, b)
}
fn limits(llm_calls_per_hour: u32) -> Limits {
    Limits {
        workers: 1,
        lease_secs: 30,
        attempts: 3,
        llm_calls_per_hour,
    }
}
fn scheduler(s: &Svc, v: Arc<Mock>, pairs: bool, limits: Limits) -> Arc<Scheduler<SimDisk>> {
    let dir = temp("jobs");
    let mut sch = Scheduler::new(
        s.actor.clone(),
        JobTable::open(&dir.join(jobs::FILE)).unwrap(),
        None,
        Loaded {
            limits,
            candidates: Default::default(),
            validators: vec![Configured {
                filter: Filter::default(),
                validator: v,
                note: "test".into(),
                pairs,
                local: false,
            }],
            error: None,
        },
    );
    Arc::get_mut(&mut sch).unwrap().retry_base = Duration::ZERO;
    sch.link_candidates(s.candidates.clone());
    sch
}
fn the_pair(s: &Svc, a: RevisionId, b: RevisionId) -> Candidate {
    s.candidates
        .all()
        .unwrap()
        .into_iter()
        .find(|c| (c.a.min(c.b), c.a.max(c.b)) == (a.min(b), a.max(b)))
        .expect("candidate a-b")
}
fn conflict_on(evidence: &str) -> Script {
    let mut o = Mock::outcome(Label::Conflict);
    o.evidence = vec![evidence.into()];
    Script::Reply(o)
}
/// Runs `judge_once` until nothing is claimable; returns what it did.
fn drain(sch: &Scheduler<SimDisk>) -> Vec<Ran> {
    let mut out = Vec::new();
    while let Some(r) = sch.judge_once().unwrap() {
        out.push(r);
        assert!(out.len() < 100);
    }
    out
}

/// `conflict` opens one PendingReview on the newer side, by the LLM
/// principal, naming both sides; it does not gate and opens no Conflict.
/// The candidate is marked judged and still listed for an agent to confirm.
#[test]
fn llm_conflict_opens_a_non_gating_review_on_the_newer_side() {
    let s = service();
    let (a, b) = setup(&s);
    let c = the_pair(&s, a, b);
    assert_eq!(c.a, a, "the newer head produced the pair");
    let mock = Arc::new(Mock::new("judge", ValidatorKind::Llm));
    mock.set_default(Script::Reply(Mock::outcome(Label::None)));
    mock.script(pair_txn(&c), vec![conflict_on("uploads-fact@6")]);
    let sch = scheduler(&s, mock.clone(), true, limits(200));
    let epoch = s.actor.snapshot().epoch;
    let ran = drain(&sch);
    assert!(ran.contains(&Ran::Reviewed(Suggestion::Reject)), "{ran:?}");
    let snap = s.actor.snapshot();
    let view = snap.view(MAIN);
    // Only the review batch was committed.
    let reviews: Vec<_> = (view.artifacts())
        .filter(|(_, x)| x.open && x.ty == ArtifactType::PendingReview)
        .collect();
    assert_eq!(reviews.len(), 1);
    assert!(
        view.artifacts()
            .all(|(_, x)| x.ty != ArtifactType::Conflict)
    );
    let (id, art) = &reviews[0];
    assert_eq!((art.target, art.author), (a, LLM_PRINCIPAL));
    let Payload::Available(bytes) = snap.log.read_blob(&art.payload_hash) else {
        panic!("review doc")
    };
    let doc = ReviewDoc::from_bytes(&bytes).unwrap();
    assert_eq!(doc.suggestion, Suggestion::Reject);
    assert_eq!((doc.rule, doc.sides.clone()), (None, vec![a, b]));
    let Payload::Available(bytes) = snap.log.read_blob(&doc.meta) else {
        panic!("meta")
    };
    let meta = ValidationMeta::from_bytes(&bytes).unwrap();
    assert_eq!(meta.label, Label::Conflict);
    assert_eq!(meta.job, Some(pair_txn(&c)));
    assert_eq!(meta.evidence, vec!["uploads-fact@6".to_string()]);
    let row = snap.log.read_row(Epoch(epoch.0 + 1)).unwrap();
    let batch = row.batch().unwrap();
    assert_eq!(batch.principal, LLM_PRINCIPAL);
    // The review, plus (P7 m4) both sides as expected heads.
    assert!(
        (batch.ops.iter()).all(|op| matches!(op, Op::OpenArtifact { .. } | Op::ExpectHeads { .. })),
        "{:?}",
        batch.ops
    );
    let expected: Vec<RevisionId> = (batch.ops.iter())
        .flat_map(|op| match op {
            Op::ExpectHeads { entries, .. } => entries.iter().map(|(_, r)| *r).collect(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(expected.len(), 2);
    assert!(expected.contains(&a) && expected.contains(&b));
    assert_eq!(snap.epoch, Epoch(epoch.0 + 1));
    // Non-gating.
    assert_eq!(view.status(a).unwrap(), Summary::Certified);
    assert_eq!(view.status(b).unwrap(), Summary::Certified);
    drop(snap);
    // The row: judged conflict with its review; still listed.
    let c = s.candidates.get(c.id).unwrap().unwrap();
    assert_eq!(
        (c.judge, c.label.as_deref(), c.review),
        (Judge::Done, Some("conflict"), Some(*id))
    );
    assert_eq!(c.state, CandState::New);
    let items = match s
        .handle(
            ADMIN,
            Request::Conflicts {
                binding: None,
                branch: None,
                scope: None,
                state: ConflictState::All,
                limit: 0,
            },
        )
        .response
    {
        Response::Conflicts { items, .. } => items,
        other => panic!("{other:?}"),
    };
    let cand = items
        .iter()
        .find(|i| i.candidate == Some(c.id))
        .expect("listed");
    assert_eq!(
        cand.marks,
        vec![format!("judged conflict by judge (review {})", id.0)]
    );
    let review = items.iter().find(|i| i.kind == "review").expect("review");
    assert!(review.reason.starts_with("reject"), "{review:?}");
    assert_eq!(review.sides.len(), 2);
    // Judged once: nothing left, one call per pair.
    assert!(sch.judge_once().unwrap().is_none());
    assert_eq!(mock.calls(), s.candidates.all().unwrap().len());
}

/// `none` commits nothing and keeps the label and metadata in the row; a
/// review already committed for the pair (a crash before the row was
/// updated) is found through `TxnStatus` without calling again.
#[test]
fn none_commits_nothing_and_a_committed_review_is_not_judged_again() {
    let s = service();
    let (a, b) = setup(&s);
    let mock = Arc::new(Mock::new("judge", ValidatorKind::Llm));
    mock.set_default(Script::Reply(Mock::outcome(Label::None)));
    let c = the_pair(&s, a, b);
    // The pair's review was committed before a crash.
    let review = ReviewDoc {
        v: 1,
        suggestion: Suggestion::Reject,
        branch: MAIN,
        rule: None,
        curvec: Vec::new(),
        sides: vec![a, b],
        meta: h(b"m"),
    };
    let (hash, bytes) = contextdb_proto::docs::blob(&review).unwrap();
    let mut batch = Batch::new(vec![Op::OpenArtifact {
        ty: ArtifactType::PendingReview,
        target: a.into(),
        payload_hash: hash,
    }]);
    batch.channel = Channel::Admin;
    batch.principal = LLM_PRINCIPAL;
    batch.client_txn_id = Some(pair_txn(&c));
    s.actor.write(vec![bytes], batch).unwrap().unwrap();
    let epoch = s.actor.snapshot().epoch;
    let sch = scheduler(&s, mock.clone(), true, limits(200));
    let ran = drain(&sch);
    assert!(ran.contains(&Ran::AlreadyCommitted), "{ran:?}");
    let total = s.candidates.all().unwrap().len();
    assert_eq!(mock.calls(), total - 1, "the committed pair was not called");
    // Every other pair: none, nothing committed.
    assert_eq!(s.actor.snapshot().epoch, epoch);
    for x in s.candidates.all().unwrap() {
        assert_eq!(x.judge, Judge::Done);
        if x.id != c.id {
            assert_eq!(x.label.as_deref(), Some("none"));
            assert_eq!(x.validator.as_deref(), Some("judge"));
        }
    }
}

/// Without `pairs = true` nothing is judged; a pair a side of which moved
/// is skipped without a call; the hourly cap defers without counting an
/// attempt; failures retry and end `Failed` after the attempts.
#[test]
fn opt_in_staleness_cap_and_failures() {
    let s = service();
    let (a, b) = setup(&s);
    let mock = Arc::new(Mock::new("judge", ValidatorKind::Llm));
    let off = scheduler(&s, mock.clone(), false, limits(200));
    assert!(off.judge_once().unwrap().is_none());
    assert_eq!(mock.calls(), 0);
    // The cap: given back, no attempt counted.
    let capped = scheduler(&s, mock.clone(), true, limits(0));
    assert_eq!(capped.judge_once().unwrap(), Some(Ran::Deferred));
    assert_eq!(mock.calls(), 0);
    let c = the_pair(&s, a, b);
    let row = s.candidates.all().unwrap();
    assert!(
        row.iter()
            .all(|x| x.attempts == 0 && x.judge == Judge::Pending)
    );
    // Failures: three attempts, then Failed.
    mock.set_default(Script::Fail(ValidatorError::Failed("down".into())));
    let sch = scheduler(&s, mock.clone(), true, limits(200));
    // Supersede `b` first: its pairs are stale and never called.
    let snap = s.actor.snapshot();
    let lb = snap.view(MAIN).revision_logical(b).unwrap();
    drop(snap);
    raw(
        &s,
        vec![b"uploads moved".to_vec()],
        vec![
            Op::AddRevision {
                logical: lb.into(),
                content: h(b"uploads moved"),
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: lb.into(),
                expected: b.into(),
                new: RevRef::New(0),
            },
        ],
    );
    settle(&s);
    let ran = drain(&sch);
    let stale = s.candidates.get(c.id).unwrap().unwrap();
    assert_ne!(stale.judge, Judge::Running);
    assert!(matches!(stale.judge, Judge::Skipped) || stale.state == CandState::Gone);
    let failed: Vec<_> = (s.candidates.all().unwrap().into_iter())
        .filter(|x| x.state == CandState::New)
        .collect();
    for x in &failed {
        assert_eq!((x.judge, x.attempts), (Judge::Failed, 3), "{x:?}");
        assert_eq!(x.error.as_deref(), Some("validator failed: down"));
    }
    assert_eq!(
        ran.iter().filter(|r| matches!(r, Ran::Failed(_))).count(),
        failed.len() * 3
    );
    assert_eq!(mock.calls(), failed.len() * 3);
}

/// A pairs validator's filters match the newer side: one limited to Plans
/// takes no Decision-led pair.
#[test]
fn filters_match_the_newer_side() {
    let s = service();
    setup(&s);
    let mock = Arc::new(Mock::new("judge", ValidatorKind::Llm));
    let dir = temp("filter");
    let sch = Scheduler::new(
        s.actor.clone(),
        JobTable::open(&dir.join(jobs::FILE)).unwrap(),
        None,
        Loaded {
            limits: limits(200),
            candidates: Default::default(),
            validators: vec![Configured {
                filter: Filter {
                    kinds: vec![RecordKind::Plan],
                    ..Filter::default()
                },
                validator: mock.clone(),
                note: "test".into(),
                pairs: true,
                local: false,
            }],
            error: None,
        },
    );
    sch.link_candidates(s.candidates.clone());
    assert!(sch.judge_once().unwrap().is_none());
    assert_eq!(mock.calls(), 0);
    // Not taken pairs wait, pending, for a later configuration.
    assert!(
        (s.candidates.all().unwrap().iter()).all(|x| x.judge == Judge::Pending && x.attempts == 0)
    );
}

/// T93 (P7 m4): a side that moves while the validator runs supersedes the
/// pair; no review lands on the old revision.
#[test]
fn t93_a_side_moving_during_the_call_supersedes_the_pair() {
    let s = service();
    let (a, b) = setup(&s);
    let c = the_pair(&s, a, b);
    let mock = Arc::new(Mock::new("judge", ValidatorKind::Llm));
    // After the delay the mock replies with its default: a conflict.
    mock.set_default(conflict_on(&format!("uploads-fact@{}", b.0)));
    mock.script(
        pair_txn(&c),
        vec![Script::Delay(Duration::from_millis(400))],
    );
    let sch = scheduler(&s, mock.clone(), true, limits(200));
    let ran = std::thread::scope(|sc| {
        let judge = sc.spawn(|| sch.judge_once().unwrap());
        std::thread::sleep(Duration::from_millis(100));
        // Move `a` (logical "uploads") while the call runs.
        let l = s.actor.snapshot().view(MAIN).revision_logical(a).unwrap();
        let body = b"user uploads go to the new object store";
        raw(
            &s,
            vec![body.to_vec()],
            vec![
                Op::AddRevision {
                    logical: l.into(),
                    content: h(body),
                    adopt_as_base: true,
                },
                Op::SetHead {
                    logical: l.into(),
                    expected: a.into(),
                    new: RevRef::New(0),
                },
            ],
        );
        judge.join().unwrap()
    });
    assert_eq!(ran, Some(Ran::Superseded));
    assert_eq!(mock.calls(), 1);
    let snap = s.actor.snapshot();
    assert!(
        (snap.view(MAIN).artifacts()).all(|(_, x)| x.ty != ArtifactType::PendingReview),
        "no review on the old revision"
    );
    let pair = the_pair(&s, a, b);
    assert_eq!((pair.judge, pair.attempts), (Judge::Skipped, 0));
}

/// T93 (P7 m4): a pair no validator takes is postponed without a claim,
/// so an expiry sweep can never count an attempt for it.
#[test]
fn t93_a_skipped_pair_never_counts_an_attempt() {
    let s = service();
    let (a, b) = setup(&s);
    let mock = Arc::new(Mock::new("judge", ValidatorKind::Llm));
    let dir = temp("skip");
    let sch = Scheduler::new(
        s.actor.clone(),
        JobTable::open(&dir.join(jobs::FILE)).unwrap(),
        None,
        Loaded {
            limits: limits(200),
            candidates: Default::default(),
            validators: vec![Configured {
                filter: Filter {
                    kinds: vec![RecordKind::Plan],
                    ..Filter::default()
                },
                validator: mock.clone(),
                note: "test".into(),
                pairs: true,
                local: false,
            }],
            error: None,
        },
    );
    sch.link_candidates(s.candidates.clone());
    assert!(sch.judge_once().unwrap().is_none());
    // An expiry sweep far in the future: nothing was ever claimed.
    let expired = s.candidates.judge_expire(i64::MAX, 3).unwrap();
    assert_eq!(expired, 0);
    let pair = the_pair(&s, a, b);
    assert_eq!((pair.judge, pair.attempts), (Judge::Pending, 0));
    // Postponed: not claimable again right away.
    assert!(sch.judge_once().unwrap().is_none());
}
