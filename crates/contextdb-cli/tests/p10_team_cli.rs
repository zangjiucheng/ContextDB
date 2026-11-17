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
    // Q11: a git alias for `push -n` publishes nothing. Git runs the alias
    // of a builtin as a new `git push -n`, which the hook reads as a dry
    // run; where only the alias is visible, the subcommand is not
    // literally `push` and publish is skipped with a warning.
    let out = run(
        &a,
        &[
            "-c",
            "alias.pn=push -n",
            "pn",
            "origin",
            "HEAD:refs/heads/main",
        ],
    );
    assert!(
        out.contains("dry run") || out.contains("could not read the push's arguments"),
        "{out}"
    );
    let (_, status_a) = ctx(&["team", "status", data_a.to_str().unwrap()]);
    assert!(status_a.contains("Queued"), "{status_a}");
    // A push to another remote publishes nothing (§7.2 step 3).
    let other = tmp.join("other.git");
    git(&tmp, &["init", "-q", "--bare", other.to_str().unwrap()]);
    run(
        &a,
        &[
            "remote",
            "add",
            "other",
            &format!("file://{}", other.display()),
        ],
    );
    run(&a, &["push", "-q", "other", "HEAD:refs/heads/main"]);
    let (_, status_a) = ctx(&["team", "status", data_a.to_str().unwrap()]);
    assert!(status_a.contains("Queued"), "{status_a}");
    // A push whose command line cannot be read (git run under another
    // name, so no ancestor is `git`): publish is skipped with a warning,
    // the push itself goes through.
    let exec_path = git(&tmp, &["--exec-path"]);
    let g = tmp.join("g");
    std::os::unix::fs::symlink(Path::new(exec_path.trim()).join("git"), &g).unwrap();
    let o = Command::new(&g)
        .current_dir(&a)
        .args(["push", "origin", "HEAD:refs/heads/main"])
        .envs(ident)
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("could not read the push's arguments"), "{out}");
    let (_, status_a) = ctx(&["team", "status", data_a.to_str().unwrap()]);
    assert!(status_a.contains("Queued"), "{status_a}");
    std::fs::write(a.join("f.txt"), "y").unwrap();
    run(&a, &["add", "f.txt"]);
    run(
        &a,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "c2"],
    );
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
    // T119: the user's own reference-transaction hook is chained and its
    // veto stands (here on B, whose join installed the dispatcher).
    let veto = b.join(".git/hooks/reference-transaction.contextdb-original");
    std::fs::write(&veto, "#!/bin/sh\nexit 1\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&veto, std::fs::Permissions::from_mode(0o755)).unwrap();
    let target = format!("refs/contextdb/remotes/origin/team/{team}/g0");
    let update = || {
        Command::new("git")
            .current_dir(&b)
            .args(["update-ref", "refs/heads/vetoed", &target])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(!update(), "the chained hook's veto must stand");
    std::fs::remove_file(&veto).unwrap();
    assert!(update(), "without the veto the same update goes through");
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

fn status_field<'a>(out: &'a str, field: &str) -> &'a str {
    out.split(&format!("{field} "))
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("")
}

#[test]
fn p10_ctx_team_compact_resume_follow_and_prune() {
    if Command::new("ssh-keygen").arg("-?").output().is_err()
        || Command::new("git").arg("--version").output().is_err()
    {
        return;
    }
    let tmp = temp("compact");
    let remote = tmp.join("remote.git");
    git(&tmp, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    let url = format!("file://{}", remote.display());
    let (a, b) = (tmp.join("a"), tmp.join("b"));
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
    let genesis = status_field(&out, "genesis").to_owned();
    let team = team_dir_id(&data_a);
    let sid = session(&da);
    let propose = |name: &str| {
        let body = name.as_bytes().to_vec();
        let r = da
            .service
            .handle(
                AGENT,
                Request::Commit {
                    binding: Binding::root(sid),
                    ops: vec![Op::CreateLogical {
                        origin: Origin::Local,
                        key: format!("team/{team}/conv/{name}"),
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
    };
    propose("one");
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
    // Step 1 by hand, then a crash: the Compact is accepted, no successor.
    let mut c = contextdb_daemon::server::Client::connect(da.socket(Channel::Admin)).unwrap();
    let hexid = |s: &str| {
        let mut t = [0u8; 16];
        for (i, x) in t.iter_mut().enumerate() {
            *x = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        t
    };
    let entry = match c
        .call(&Request::TeamCompact { team: hexid(&team) })
        .unwrap()
        .response
    {
        Response::TeamPrepared { entry: Some(e), .. } => e,
        other => panic!("{other:?}"),
    };
    assert_eq!(entry.proposals.len(), 1, "step 1: the Compact entry");
    let repo = contextdb_team::git::Repo::open(&a).unwrap();
    let sig = contextdb_team::sign::sign(&key, &entry.payload).unwrap();
    let raw = contextdb_team::commit::with_signature(&entry.payload, &sig, repo.format());
    let oid = repo.write_commit(&raw).unwrap();
    git(
        &a,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "push",
            "-q",
            "origin",
            &format!("{oid}:{}", entry.refname),
        ],
    );
    let (_, out) = ctx(&["team", "sync", data_a.to_str().unwrap()]);
    let (_, out2) = ctx(&["team", "status", data_a.to_str().unwrap()]);
    assert!(out2.contains("CompactPending"), "{out}\n{out2}");
    // A squatter takes refs/.../g1 first with a commit that is no
    // successor: `--resume` does not replace it (still pending) ...
    let squat = {
        let o = Command::new("git")
            .current_dir(&a)
            .args([
                "commit-tree",
                "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                "-m",
                "squat",
            ])
            .envs([
                ("GIT_AUTHOR_NAME", "s"),
                ("GIT_AUTHOR_EMAIL", "s@s"),
                ("GIT_COMMITTER_NAME", "s"),
                ("GIT_COMMITTER_EMAIL", "s@s"),
            ])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_owned()
    };
    run_git(
        &a,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "push",
            "-q",
            "origin",
            &format!("{squat}:refs/contextdb/team/{team}/g1"),
        ],
    );
    let (_, out) = ctx(&["team", "compact", data_a.to_str().unwrap(), "--resume"]);
    assert_ne!(status_field(&out, "generation"), "1", "{out}");
    // ... and `--resume --force-ref` does.
    let (code, out) = ctx(&[
        "team",
        "compact",
        data_a.to_str().unwrap(),
        "--resume",
        "--force-ref",
    ]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(status_field(&out, "generation"), "1", "{out}");
    // The follower follows the Compact and its successor: same state.
    run_git(&b, &["fetch", "-q", "origin"]);
    let (_, sb) = ctx(&["team", "sync", data_b.to_str().unwrap()]);
    let (_, sb) = (sb, ctx(&["team", "status", data_b.to_str().unwrap()]).1);
    assert_eq!(status_field(&sb, "generation"), "1", "{sb}");
    assert_eq!(
        status_field(&sb, "digest"),
        status_field(&out, "digest"),
        "{sb}\n{out}"
    );
    // B's real hooks announced the Compact on g0 and the successor on g1:
    // both resolve, so reads over the team have a position and do not lag
    // (review N1: an announce on the terminal generation lagged every read
    // for good).
    {
        let live = db.service.actor.team_live.lock().unwrap();
        let l = live.values().next().unwrap();
        assert!(l.pending.is_empty(), "{:?}", l.pending);
        assert!(l.position().is_some());
    }
    // Publishing goes on in generation 1.
    propose("two");
    run_git(&b, &["fetch", "-q", "origin"]);
    ctx(&["team", "sync", data_b.to_str().unwrap()]);
    let snap = db.service.actor.snapshot();
    assert!(
        snap.view(MAIN)
            .logical_by_key(&format!("team/{team}/conv/two"))
            .is_some()
    );
    drop(snap);
    // Prune: g0 leaves the remote.
    let (code, out) = ctx(&["team", "prune", data_a.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 0, "{out}");
    let refs = git(
        &a,
        &[
            "ls-remote",
            "origin",
            &format!("refs/contextdb/team/{team}/*"),
        ],
    );
    assert!(!refs.contains("/g0") && refs.contains("/g1"), "{refs}");
    drop(da);
    drop(db);
    let _ = std::fs::remove_dir_all(&tmp);
}

fn run_git(dir: &Path, args: &[&str]) {
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
}

#[test]
fn p10_ctx_team_access_adds_a_member_who_joins_and_publishes() {
    if Command::new("ssh-keygen").arg("-?").output().is_err()
        || Command::new("git").arg("--version").output().is_err()
    {
        return;
    }
    let tmp = temp("access");
    let remote = tmp.join("remote.git");
    git(&tmp, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    let url = format!("file://{}", remote.display());
    let (a, b) = (tmp.join("a"), tmp.join("b"));
    git(&tmp, &["clone", "-q", &url, a.to_str().unwrap()]);
    git(&tmp, &["clone", "-q", &url, b.to_str().unwrap()]);
    let keygen = |name: &str| {
        let k = tmp.join(name);
        assert!(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
                .arg(&k)
                .status()
                .unwrap()
                .success()
        );
        k
    };
    let (alice, bob) = (keygen("alice"), keygen("bob"));
    let (data_a, data_b) = (tmp.join("data-a"), tmp.join("data-b"));
    let da = Daemon::start(&data_a, &Options::default()).unwrap();
    let (code, out) = ctx(&[
        "team",
        "create",
        data_a.to_str().unwrap(),
        "--repo",
        a.to_str().unwrap(),
        "--key",
        alice.to_str().unwrap(),
        "--member",
        "alice",
    ]);
    assert_eq!(code, 0, "{out}");
    let genesis = status_field(&out, "genesis").to_owned();
    let team = team_dir_id(&data_a);
    // bob cannot join yet.
    let db = Daemon::start(&data_b, &Options::default()).unwrap();
    let join = |key: &Path| {
        ctx(&[
            "team",
            "join",
            data_b.to_str().unwrap(),
            "--repo",
            b.to_str().unwrap(),
            "--genesis",
            &genesis,
            "--key",
            key.to_str().unwrap(),
        ])
    };
    let (code, out) = join(&bob);
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("NotTeamMember"), "{out}");
    // alice, an admin, adds bob.
    let pubkey = |k: &Path| {
        std::fs::read_to_string(k.with_extension("pub"))
            .unwrap()
            .trim()
            .to_owned()
    };
    let doc = format!(
        "version = 1\naccept_version = 1\nid = \"{team}\"\nadmins = [\"alice\"]\n\
         [members.alice]\nkeys = [\"{}\"]\n[members.bob]\nkeys = [\"{}\"]\n\
         [lanes.main]\nwriters = [\"*members\"]\ncurators = [\"*admins\"]\n",
        pubkey(&alice),
        pubkey(&bob)
    );
    let file = tmp.join("team.toml");
    std::fs::write(&file, doc).unwrap();
    let (code, out) = ctx(&[
        "team",
        "access",
        data_a.to_str().unwrap(),
        "--file",
        file.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = ctx(&["team", "publish", data_a.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("Accepted"), "{out}");
    let (code, out) = join(&bob);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("as bob"), "{out}");
    // bob proposes and publishes, signed as bob; alice imports it.
    let sid = session(&db);
    let body = b"bob's convention".to_vec();
    let r = db
        .service
        .handle(
            AGENT,
            Request::Commit {
                binding: Binding::root(sid),
                ops: vec![Op::CreateLogical {
                    origin: Origin::Local,
                    key: format!("team/{team}/conv/bob"),
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
    // Signing never uses git's signing configuration (N8): bob's global
    // config asks for OpenPGP signing of every commit.
    let gitconfig = tmp.join("gitconfig-openpgp");
    std::fs::write(
        &gitconfig,
        "[gpg]\n\tformat = openpgp\n[user]\n\tsigningkey = DEADBEEFDEADBEEF\n[commit]\n\tgpgsign = true\n",
    )
    .unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["team", "publish", data_b.to_str().unwrap()])
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("Accepted"), "{out}");
    run_git(&a, &["fetch", "-q", "origin"]);
    ctx(&["team", "sync", data_a.to_str().unwrap()]);
    let snap = da.service.actor.snapshot();
    assert!(
        snap.view(MAIN)
            .logical_by_key(&format!("team/{team}/conv/bob"))
            .is_some()
    );
    drop(snap);
    // hooks off and on again.
    let (code, out) = ctx(&["team", "hooks", data_b.to_str().unwrap(), "off"]);
    assert_eq!(code, 0, "{out}");
    assert!(!b.join(".git/hooks/reference-transaction").exists());
    let (code, out) = ctx(&["team", "hooks", data_b.to_str().unwrap(), "on"]);
    assert_eq!(code, 0, "{out}");
    assert!(b.join(".git/hooks/reference-transaction").exists());
    drop(da);
    drop(db);
    let _ = std::fs::remove_dir_all(&tmp);
}
