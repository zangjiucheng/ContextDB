//! P10 G5 with the real binary and hooks (ADR 0018 §13): what the team
//! hooks add to `git commit` (`reference-transaction`, ≤ 20 ms over a
//! trivial hook that only reads its input, owner decision 2026-11-17), what the
//! durable announce and its acknowledgement add to a fetch that moves a
//! team ref (≤ 50 ms), and what publishing ten proposals adds to `git push`
//! (≤ 2 s). Each is measured against the same operation without the team
//! component. Run on the remote host:
//! `cargo test --release -p contextdb-cli --test p10_g5_hooks -- --ignored --nocapture`.

use contextdb_daemon::server::{Daemon, Options};
use contextdb_daemon::service::Identity;
use contextdb_model::*;
use contextdb_proto::proto::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

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

fn git(dir: &Path, args: &[&str]) -> f64 {
    let t = Instant::now();
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .envs([
            ("GIT_AUTHOR_NAME", "t"),
            ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"),
            ("GIT_COMMITTER_EMAIL", "t@t"),
        ])
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    t.elapsed().as_secs_f64() * 1e3
}

fn stats(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(f64::total_cmp);
    (v[v.len() / 2], v[(v.len() * 99 / 100).min(v.len() - 1)])
}

const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};

#[test]
#[ignore = "G5 measurement; run explicitly"]
fn p10_g5_hook_and_publish_overheads() {
    let tmp = std::env::temp_dir().join(format!("p10-g5h-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let remote = tmp.join("remote.git");
    git(&tmp, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    let url = format!("file://{}", remote.display());
    let (a, b) = (tmp.join("a"), tmp.join("b"));
    git(&tmp, &["clone", "-q", &url, a.to_str().unwrap()]);
    git(&tmp, &["clone", "-q", &url, b.to_str().unwrap()]);
    let key = tmp.join("k");
    assert!(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success()
    );
    let (da_dir, db_dir) = (tmp.join("data-a"), tmp.join("data-b"));
    let da = Daemon::start(&da_dir, &Options::default()).unwrap();
    let db = Daemon::start(&db_dir, &Options::default()).unwrap();
    let (code, out) = ctx(&[
        "team",
        "create",
        da_dir.to_str().unwrap(),
        "--repo",
        a.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
        "--member",
        "m",
    ]);
    assert_eq!(code, 0, "{out}");
    let genesis = out
        .split("genesis ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let (code, out) = ctx(&[
        "team",
        "join",
        db_dir.to_str().unwrap(),
        "--repo",
        b.to_str().unwrap(),
        "--genesis",
        &genesis,
        "--key",
        key.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{out}");
    let team = std::fs::read_dir(da_dir.join("teams"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned();
    let commit_times = |repo: &PathBuf, n: usize, tag: &str| -> Vec<f64> {
        (0..n)
            .map(|i| {
                std::fs::write(repo.join("f"), format!("{tag}{i}")).unwrap();
                git(repo, &["add", "f"]);
                git(
                    repo,
                    &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "c"],
                )
            })
            .collect()
    };
    // 1. `git commit` with the team hooks, then with them off.
    let n = 60;
    let with = stats(commit_times(&a, n, "on"));
    assert_eq!(
        ctx(&["team", "hooks", da_dir.to_str().unwrap(), "off"]).0,
        0
    );
    let without = stats(commit_times(&a, n, "off"));
    // The floor any `reference-transaction` hook pays here: one that only
    // reads its input (owner decision 2026-11-17: the budget is ≤ 20 ms
    // over this floor).
    let trivial_hook = a.join(".git/hooks/reference-transaction");
    std::fs::write(&trivial_hook, "#!/bin/sh\ncat >/dev/null\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&trivial_hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let trivial = stats(commit_times(&a, n, "trivial"));
    std::fs::remove_file(&trivial_hook).unwrap();
    assert_eq!(ctx(&["team", "hooks", da_dir.to_str().unwrap(), "on"]).0, 0);
    println!(
        "{{\"part\":\"git-commit\",\"n\":{n},\"with_p50_ms\":{:.2},\"with_p99_ms\":{:.2},\"without_p50_ms\":{:.2},\"without_p99_ms\":{:.2},\"trivial_hook_p50_ms\":{:.2},\"added_p50_ms\":{:.2},\"added_over_trivial_hook_p50_ms\":{:.2},\"budget_over_trivial_ms\":20}}",
        with.0,
        with.1,
        without.0,
        without.1,
        trivial.0,
        with.0 - without.0,
        with.0 - trivial.0
    );
    // A team write proposed on A.
    let sid = match da
        .service
        .handle(
            AGENT,
            Request::SessionStart {
                harness: "g5".into(),
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
    };
    let propose = |name: String| {
        let body = name.clone().into_bytes();
        let r = da
            .service
            .handle(
                AGENT,
                Request::Commit {
                    binding: Binding::root(sid),
                    ops: vec![Op::CreateLogical {
                        origin: Origin::Local,
                        key: format!("team/{team}/k/{name}"),
                        kind: RecordKind::Decision,
                        scope_lane: format!("team:{team}/main"),
                        content: *blake3::hash(&body).as_bytes(),
                        adopt_as_base: true,
                    }],
                    blobs: vec![body],
                    expect_cert_valid: false,
                    client_txn_id: None,
                },
            )
            .response;
        assert!(matches!(r, Response::Proposed { .. }), "{r:?}");
    };
    // 2. Publishing ten proposals on push, against a push with none.
    let mut plain = Vec::new();
    let mut publishing = Vec::new();
    for round in 0..5 {
        std::fs::write(a.join("f"), format!("p{round}")).unwrap();
        git(&a, &["add", "f"]);
        git(
            &a,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "c"],
        );
        plain.push(git(&a, &["push", "-q", "origin", "HEAD:refs/heads/main"]));
        for i in 0..10 {
            propose(format!("r{round}-{i}"));
        }
        std::fs::write(a.join("f"), format!("q{round}")).unwrap();
        git(&a, &["add", "f"]);
        git(
            &a,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "c"],
        );
        publishing.push(git(&a, &["push", "-q", "origin", "HEAD:refs/heads/main"]));
    }
    let (p, q) = (stats(plain), stats(publishing));
    println!(
        "{{\"part\":\"publish-ten\",\"rounds\":5,\"plain_push_p50_ms\":{:.1},\"publishing_push_p50_ms\":{:.1},\"publishing_push_max_ms\":{:.1},\"added_p50_ms\":{:.1},\"budget_ms\":2000}}",
        p.0,
        q.0,
        q.1,
        q.0 - p.0
    );
    // 3. A fetch that moves the team ref: with the announce, and with the
    //    team hooks off.
    let mut on = Vec::new();
    let mut off = Vec::new();
    for round in 0..10 {
        propose(format!("f{round}"));
        assert_eq!(ctx(&["team", "publish", da_dir.to_str().unwrap()]).0, 0);
        let hooks_on = round % 2 == 0;
        if !hooks_on {
            assert_eq!(
                ctx(&["team", "hooks", db_dir.to_str().unwrap(), "off"]).0,
                0
            );
        }
        let t = git(&b, &["fetch", "-q", "origin"]);
        if hooks_on {
            on.push(t)
        } else {
            off.push(t)
        }
        if !hooks_on {
            assert_eq!(ctx(&["team", "hooks", db_dir.to_str().unwrap(), "on"]).0, 0);
        }
    }
    let (fo, ff) = (stats(on), stats(off));
    println!(
        "{{\"part\":\"fetch-team-ref\",\"n\":5,\"with_p50_ms\":{:.1},\"without_p50_ms\":{:.1},\"added_p50_ms\":{:.1},\"budget_ms\":50}}",
        fo.0,
        ff.0,
        fo.0 - ff.0
    );
    drop(da);
    drop(db);
    let _ = std::fs::remove_dir_all(&tmp);
}
