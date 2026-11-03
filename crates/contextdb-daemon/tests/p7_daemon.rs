//! P7 milestone 4 end to end: `ctxd` reads `validators.toml`, discovery
//! and the worker run in the background, and `ctx validators` reports on
//! the admin socket only.
use contextdb_daemon::{
    proto::*,
    server::{Client, Daemon, Options},
};
use contextdb_model::*;
use contextdb_proto::docs::ValidatorKind;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ctxd-p7-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
fn h(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}
fn raw(c: &mut Client, blobs: Vec<Vec<u8>>, ops: Vec<Op>) {
    let r = c
        .call(&Request::Raw {
            blobs,
            batch: Batch::new(ops),
        })
        .unwrap();
    assert!(matches!(r.response, Response::Committed { .. }), "{r:?}");
}
const TRUSTED: &str = r#"
[limits]
workers = 1
[[validator]]
name = "always-yes"
type = "command"
command = ["/bin/sh", "-c", "cat > /dev/null; echo '{\"label\": \"reinforcement\", \"rationale\": \"ok\", \"evidence\": []}'"]
trusted = true
"#;
fn report(c: &mut Client, action: ValidatorsAction) -> ValidatorsReport {
    match c.call(&Request::Validators { action }).unwrap().response {
        Response::Validators { report } => report,
        other => panic!("{other:?}"),
    }
}
fn count(r: &[Counter], name: &str) -> u64 {
    r.iter().find(|c| c.name == name).unwrap().value
}

#[test]
fn ctxd_validates_pending_rules_in_the_background() {
    let dir = tmp("bg");
    std::fs::write(dir.join("validators.toml"), TRUSTED).unwrap();
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    raw(
        &mut admin,
        vec![b"a0".to_vec(), b"b".to_vec()],
        vec![
            Op::CreateLogical {
                key: "a".into(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: h(b"a0"),
                adopt_as_base: true,
            },
            Op::CreateLogical {
                key: "b".into(),
                kind: RecordKind::Decision,
                scope_lane: "repo".into(),
                content: h(b"b"),
                adopt_as_base: true,
            },
        ],
    );
    raw(
        &mut admin,
        Vec::new(),
        vec![Op::AddRule {
            conclusion: RevisionId(1).into(),
            incidences: vec![(RevisionId(0).into(), EdgeKind::Sem)],
            enabled: true,
        }],
    );
    raw(
        &mut admin,
        vec![b"a1".to_vec()],
        vec![
            Op::AddRevision {
                logical: LogicalId(0).into(),
                content: h(b"a1"),
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: LogicalId(0).into(),
                expected: RevisionId(0).into(),
                new: RevRef::New(0),
            },
        ],
    );
    let start = Instant::now();
    loop {
        if !d
            .service
            .actor
            .snapshot()
            .db()
            .inputs()
            .validations
            .is_empty()
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "no validation recorded"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let r = report(&mut admin, ValidatorsAction::Status);
    assert_eq!(r.validators.len(), 1);
    assert_eq!(r.validators[0].kind, ValidatorKind::Deterministic);
    assert_eq!(count(&r.jobs, "done"), 1);
    assert_eq!(count(&r.stats, "records"), 1);
    assert_eq!(r.error, None);

    // Agents cannot see or drive the workers.
    let mut agent = Client::connect(d.socket(Channel::Agent)).unwrap();
    let env = agent
        .call(&Request::Validators {
            action: ValidatorsAction::Status,
        })
        .unwrap();
    assert!(
        matches!(&env.response, Response::Error { code, .. } if code == "ChannelRestricted"),
        "{env:?}"
    );

    // A broken file on reload is reported; the old configuration stays.
    std::fs::write(dir.join("validators.toml"), "[[validator]]\nname = 1").unwrap();
    let r = report(&mut admin, ValidatorsAction::Reload);
    assert!(r.error.is_some());
    assert_eq!(r.validators.len(), 1);
    // A good file replaces it.
    std::fs::write(dir.join("validators.toml"), "").unwrap();
    let r = report(&mut admin, ValidatorsAction::Reload);
    assert_eq!((r.error, r.validators.len()), (None, 0));
    let r = report(&mut admin, ValidatorsAction::Check);
    assert!(r.check[0].contains("no validator configured"));
    d.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ctxd_without_validators_toml_calls_nothing() {
    let dir = tmp("none");
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    let r = report(&mut admin, ValidatorsAction::Status);
    assert!(r.validators.is_empty());
    assert_eq!(count(&r.stats, "calls"), 0);
    assert!(dir.join("jobs.sqlite").exists());
    d.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

fn create(c: &mut Client, key: &str, kind: RecordKind, body: &str) {
    raw(
        c,
        vec![body.as_bytes().to_vec()],
        vec![Op::CreateLogical {
            key: key.into(),
            kind,
            scope_lane: "repo".into(),
            content: h(body.as_bytes()),
            adopt_as_base: true,
        }],
    );
}
fn candidates_settled(d: &Daemon) {
    let start = Instant::now();
    while d.service.candidates.watermark().unwrap() != Some(d.service.actor.snapshot().epoch) {
        assert!(start.elapsed() < Duration::from_secs(30), "no candidates");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Candidates live in `<data>/candidates.sqlite`: a store written before
/// they existed is backfilled once, a restart keeps the pairs without
/// querying again, and `[candidates] per_head = 0` turns new pairs off.
#[test]
fn ctxd_candidates_backfill_persist_and_obey_the_config() {
    let dir = tmp("cand");
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    for (i, w) in ["alpha", "bravo", "charlie", "delta", "echo", "golf"]
        .iter()
        .enumerate()
    {
        create(
            &mut admin,
            &format!("f{i}"),
            RecordKind::Fact,
            &format!("{w} {w} note on {w}"),
        );
    }
    create(
        &mut admin,
        "cache",
        RecordKind::Decision,
        "api responses are cached in memcached for ten minutes",
    );
    create(
        &mut admin,
        "cache-fact",
        RecordKind::Fact,
        "api responses are never cached in memcached",
    );
    candidates_settled(&d);
    let first = d.service.candidates.all().unwrap();
    assert!(!first.is_empty());
    let r = report(&mut admin, ValidatorsAction::Status);
    assert!(count(&r.stats, "candidate_pairs") >= 1);
    assert_eq!(count(&r.limits, "candidate_per_head"), 5);
    d.stop();
    assert!(dir.join("candidates.sqlite").exists());

    // Restart: same pairs, nothing queried again.
    std::fs::write(dir.join("validators.toml"), "[candidates]\nper_head = 0\n").unwrap();
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    candidates_settled(&d);
    assert_eq!(d.service.candidates.all().unwrap(), first);
    let r = report(&mut admin, ValidatorsAction::Status);
    assert_eq!(count(&r.stats, "candidate_heads"), 0);
    assert_eq!(count(&r.limits, "candidate_per_head"), 0);
    // per_head = 0: a new similar head adds no pair.
    create(
        &mut admin,
        "cache-plan",
        RecordKind::Plan,
        "move api responses cached in memcached to redis",
    );
    candidates_settled(&d);
    assert_eq!(d.service.candidates.all().unwrap(), first);
    // Reload with the default: the next head pairs again.
    std::fs::write(dir.join("validators.toml"), "").unwrap();
    report(&mut admin, ValidatorsAction::Reload);
    create(
        &mut admin,
        "cache-ttl",
        RecordKind::Decision,
        "api responses stay in memcached for one hour",
    );
    candidates_settled(&d);
    assert!(d.service.candidates.all().unwrap().len() > first.len());
    d.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
