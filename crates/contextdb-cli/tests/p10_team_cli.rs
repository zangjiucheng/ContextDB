//! P10 `ctx team` through the real binary, real git and real SSH keys:
//! create (genesis signed by `ssh-keygen` and pushed), an agent's team
//! write queued by `ctxd`, `ctx team publish` (sign, begin, push, import:
//! accepted), and a second clone joining from the out-of-band genesis and
//! mirroring the accepted record.

use contextdb_daemon::server::{Daemon, Options};
use contextdb_daemon::service::Identity;
use contextdb_model::*;
use contextdb_proto::proto::*;
use std::path::{Path, PathBuf};
use std::process::Command;

fn ctx(args: &[&str]) -> (i32, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(args)
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn temp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("p10-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};

fn session(d: &Daemon) -> SessionId {
    match d
        .service
        .handle(
            AGENT,
            Request::SessionStart {
                harness: "test".into(),
                repo: "r".into(),
                head_sha: "h".into(),
                parent: None,
                branch: None,
                git_branch: None,
            },
        )
        .response
    {
        Response::Session { session } => session,
        other => panic!("{other:?}"),
    }
}

fn team_dir_id(data: &Path) -> String {
    let mut ids: Vec<String> = std::fs::read_dir(data.join("teams"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort();
    assert_eq!(ids.len(), 1, "{ids:?}");
    ids.remove(0)
}

#[test]
fn p10_ctx_team_create_publish_join() {
    if Command::new("ssh-keygen").arg("-?").output().is_err()
        || Command::new("git").arg("--version").output().is_err()
    {
        return;
    }
    let tmp = temp("main");
    let remote = tmp.join("remote.git");
    git(&tmp, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    git(&remote, &["config", "uploadpack.allowFilter", "true"]);
    git(
        &remote,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
    );
    let url = format!("file://{}", remote.display());
    let a = tmp.join("a");
    let b = tmp.join("b");
    git(&tmp, &["clone", "-q", &url, a.to_str().unwrap()]);
    git(&tmp, &["clone", "-q", &url, b.to_str().unwrap()]);
    let key = tmp.join("alice");
    assert!(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "alice", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success()
    );
    let (data_a, data_b) = (tmp.join("data-a"), tmp.join("data-b"));
    let da = Daemon::start(&data_a, &Options::default()).unwrap();
    let (code, out) = ctx(&[
        "team",
        "create",
        data_a.to_str().unwrap(),
        "--repo",
        a.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
        "--member",
        "alice",
    ]);
    assert_eq!(code, 0, "{out}");
    let genesis = out
        .split("genesis ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap()
        .to_owned();
    let team = team_dir_id(&data_a);
    // An agent's team write: queued, not committed.
    let sid = session(&da);
    let body = b"use the root's naming".to_vec();
    let content = *blake3::hash(&body).as_bytes();
    let r = da
        .service
        .handle(
            AGENT,
            Request::Commit {
                binding: Binding::root(sid),
                ops: vec![Op::CreateLogical {
                    origin: Origin::Local,
                    key: format!("team/{team}/conv/naming"),
                    kind: RecordKind::Convention,
                    scope_lane: format!("team:{team}/main"),
                    content,
                    adopt_as_base: false,
                }],
                blobs: vec![body],
                expect_cert_valid: false,
                client_txn_id: None,
            },
        )
        .response;
    assert!(matches!(r, Response::Proposed { .. }), "{r:?}");
    let (code, out) = ctx(&["team", "publish", data_a.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("published 1 proposal"), "{out}");
    assert!(out.contains("Accepted"), "{out}");
    let snap = da.service.actor.snapshot();
    assert!(
        snap.view(MAIN)
            .logical_by_key(&format!("team/{team}/conv/naming"))
            .is_some()
    );
    drop(snap);
    // A second clone joins from the genesis given out of band.
    let db = Daemon::start(&data_b, &Options::default()).unwrap();
    let (code, out) = ctx(&[
        "team",
        "join",
        data_b.to_str().unwrap(),
        "--repo",
        b.to_str().unwrap(),
        "--genesis",
        &genesis,
        "--key",
        key.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("as alice"), "{out}");
    let snap = db.service.actor.snapshot();
    assert!(
        snap.view(MAIN)
            .logical_by_key(&format!("team/{team}/conv/naming"))
            .is_some(),
        "{out}"
    );
    drop(snap);
    let (code, out) = ctx(&["team", "status", data_b.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("member alice seq 1"), "{out}");
    let (code, out) = ctx(&[
        "team",
        "why",
        data_b.to_str().unwrap(),
        &format!("team/{team}/conv/naming"),
    ]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("team record conv/naming@"), "{out}");
    // Joining twice is refused.
    let (code, out) = ctx(&[
        "team",
        "join",
        data_b.to_str().unwrap(),
        "--repo",
        b.to_str().unwrap(),
        "--genesis",
        &genesis,
        "--key",
        key.to_str().unwrap(),
    ]);
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("already joined"), "{out}");
    // Through the hooks. A second team write on A, then a code push.
    let body = b"plans live in team:main".to_vec();
    let content = *blake3::hash(&body).as_bytes();
    let r = da
        .service
        .handle(
            AGENT,
            Request::Commit {
                binding: Binding::root(sid),
                ops: vec![Op::CreateLogical {
                    origin: Origin::Local,
                    key: format!("team/{team}/conv/plans"),
                    kind: RecordKind::Convention,
                    scope_lane: format!("team:{team}/main"),
                    content,
                    adopt_as_base: false,
                }],
                blobs: vec![body],
                expect_cert_valid: false,
                client_txn_id: None,
            },
        )
        .response;
    assert!(matches!(r, Response::Proposed { .. }), "{r:?}");
    std::fs::write(a.join("f.txt"), "x").unwrap();
    let ident = [
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@t"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@t"),
    ];
    let run = |dir: &Path, args: &[&str]| {
        let o = Command::new("git")
            .current_dir(dir)
            .args(args)
            .envs(ident)
            .output()
            .unwrap();
        let text =
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
        assert!(o.status.success(), "git {args:?}: {text}");
        text
    };
    run(&a, &["add", "f.txt"]);
    run(
        &a,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "c"],
    );
    // A dry run publishes nothing.
    let out = run(&a, &["push", "-n", "origin", "HEAD:refs/heads/main"]);
    assert!(out.contains("dry run"), "{out}");
    let (_, status_a) = ctx(&["team", "status", data_a.to_str().unwrap()]);
    assert!(status_a.contains("Queued"), "{status_a}");
    // The real push publishes it (pre-push: sync, gate, publish).
    let out = run(&a, &["push", "origin", "HEAD:refs/heads/main"]);
    assert!(out.contains("published 1 proposal"), "{out}");
    let (_, status_a) = ctx(&["team", "status", data_a.to_str().unwrap()]);
    assert!(!status_a.contains("Queued"), "{status_a}");
    // B fetches: reference-transaction announces and notifies; ctxd
    // imports without anyone asking.
    run(&b, &["fetch", "-q", "origin"]);
    let plans = format!("team/{team}/conv/plans");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let snap = db.service.actor.snapshot();
        if snap.view(MAIN).logical_by_key(&plans).is_some() {
            break;
        }
        drop(snap);
        assert!(
            std::time::Instant::now() < deadline,
            "B never imported {plans}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // A blobless clone joins (no ctxd running there): the join fetches the
    // entries' blobs by OID before importing.
    let c = tmp.join("c");
    git(
        &tmp,
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            &url,
            c.to_str().unwrap(),
        ],
    );
    let data_c = tmp.join("data-c");
    std::fs::create_dir_all(&data_c).unwrap();
    let (code, out) = ctx(&[
        "team",
        "join",
        data_c.to_str().unwrap(),
        "--repo",
        c.to_str().unwrap(),
        "--genesis",
        &genesis,
        "--key",
        key.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{out}");
    // A third proposal published from A; C's fetch alone brings its blobs
    // (reference-transaction `committed` starts the by-OID fetch).
    let body = b"third".to_vec();
    let r = da
        .service
        .handle(
            AGENT,
            Request::Commit {
                binding: Binding::root(sid),
                ops: vec![Op::CreateLogical {
                    origin: Origin::Local,
                    key: format!("team/{team}/conv/third"),
                    kind: RecordKind::Convention,
                    scope_lane: format!("team:{team}/main"),
                    content: *blake3::hash(&body).as_bytes(),
                    adopt_as_base: false,
                }],
                blobs: vec![body],
                expect_cert_valid: false,
                client_txn_id: None,
            },
        )
        .response;
    assert!(matches!(r, Response::Proposed { .. }), "{r:?}");
    let (code, out) = ctx(&["team", "publish", data_a.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    run(&c, &["fetch", "-q", "origin"]);
    let tracking = format!("refs/contextdb/remotes/origin/team/{team}/g0");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let o = Command::new("git")
            .current_dir(&c)
            .args(["rev-list", "--objects", "--missing=print", &tracking])
            .env("GIT_NO_LAZY_FETCH", "1")
            .output()
            .unwrap();
        let missing = String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| l.starts_with('?'))
            .count();
        if o.status.success() && missing == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{missing} objects still missing"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Deleting the team ref, or pushing tracking refs, is refused.
    let o = Command::new("git")
        .current_dir(&a)
        .args(["push", "origin", &format!(":refs/contextdb/team/{team}/g0")])
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("append-only"));
    drop(da);
    drop(db);
    let _ = std::fs::remove_dir_all(&tmp);
}
