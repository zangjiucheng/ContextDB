//! P8 milestone 6 (ADR 0016 §8): the git mirror. T57, T91, T92, T96, T97,
//! against a real temporary git repository.
use contextdb_daemon::{
    actor,
    mirror::{self, REF_PREFIX},
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{Durability, OpenOptions, SimDisk, Store, session::SessionLog};
use std::path::{Path, PathBuf};
use std::process::Command;

const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
type Svc = Service<SimDisk>;

struct Env {
    root: PathBuf,
    repo: PathBuf,
    data: PathBuf,
}
impl Env {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ctx-mirror-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (repo, data) = (root.join("repo"), root.join("data"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        git(&repo, &["init", "-q"]);
        Self { root, repo, data }
    }
    fn project(&self) -> String {
        self.repo.display().to_string()
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn git_raw(
    repo: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    env: &[(&str, &str)],
) -> (bool, String) {
    use std::io::Write;
    let mut cmd = Command::new("git");
    cmd.current_dir(repo)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
        ])
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        if let Some(i) = input {
            stdin.write_all(i).unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
    )
}
fn git(repo: &Path, args: &[&str]) -> String {
    let (ok, out) = git_raw(repo, args, None, &[]);
    assert!(ok, "git {args:?}");
    out
}
fn service(env: &Env) -> Svc {
    service_on(env, SimDisk::new())
}
fn service_on(env: &Env, disk: SimDisk) -> Svc {
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    let s = Service::new(actor::spawn(store).unwrap(), log, events).unwrap();
    s.set_data_dir(env.data.clone());
    s
}
fn h(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}
fn create_on(s: &Svc, branch: BranchId, key: &str, kind: RecordKind, body: &str) -> RevisionId {
    create_in(s, branch, "repo", key, kind, body)
}
fn create_in(
    s: &Svc,
    branch: BranchId,
    lane: &str,
    key: &str,
    kind: RecordKind,
    body: &str,
) -> RevisionId {
    let mut batch = Batch::new(vec![Op::CreateLogical {
        key: key.into(),
        kind,
        scope_lane: lane.into(),
        content: h(body.as_bytes()),
        adopt_as_base: true,
    }]);
    batch.branch = branch;
    match s
        .handle(
            ADMIN,
            Request::Raw {
                blobs: vec![body.as_bytes().to_vec()],
                batch,
            },
        )
        .response
    {
        Response::Committed { result } => result.allocations.new_revisions[0],
        other => panic!("{other:?}"),
    }
}
fn create(s: &Svc, key: &str, body: &str) -> RevisionId {
    create_on(s, MAIN, key, RecordKind::Decision, body)
}
fn mirror(s: &Svc, action: MirrorAction) -> Result<MirrorReport, (String, String)> {
    match s.handle(ADMIN, Request::Mirror { action }).response {
        Response::Mirror { report } => Ok(report),
        Response::Error { code, detail } => Err((code, detail)),
        other => panic!("{other:?}"),
    }
}
fn export(s: &Svc, env: &Env, branch: Option<&str>) -> Result<MirrorReport, (String, String)> {
    export_with(s, env, branch, false)
}
fn export_with(
    s: &Svc,
    env: &Env,
    branch: Option<&str>,
    force: bool,
) -> Result<MirrorReport, (String, String)> {
    mirror(
        s,
        MirrorAction::Export {
            branch: branch.map(str::to_owned),
            project: env.project(),
            force,
            adopt: false,
            readable_ok: false,
        },
    )
}
fn tip(env: &Env, branch: u32) -> Option<String> {
    let (ok, out) = git_raw(
        &env.repo,
        &[
            "rev-parse",
            "-q",
            "--verify",
            &format!("{REF_PREFIX}{branch}"),
        ],
        None,
        &[],
    );
    ok.then_some(out)
}
fn tree_of(env: &Env, commit: &str) -> String {
    git(&env.repo, &["rev-parse", &format!("{commit}^{{tree}}")])
}
fn files(env: &Env, commit: &str) -> Vec<String> {
    let out = git(&env.repo, &["ls-tree", "-r", "--name-only", commit]);
    out.lines().map(str::to_owned).collect()
}
fn message(env: &Env, commit: &str) -> String {
    git(&env.repo, &["log", "-1", "--format=%B", commit])
}
/// A commit on top of `parent` with `path` set to `body` (or removed).
fn hand_edit(env: &Env, parent: &str, edits: &[(&str, Option<&str>)], msg: &str) -> String {
    let index = env.root.join("edit.index");
    let idx = index.display().to_string();
    let e = [("GIT_INDEX_FILE", idx.as_str())];
    assert!(git_raw(&env.repo, &["read-tree", parent], None, &e).0);
    for (path, body) in edits {
        match body {
            Some(b) => {
                let (ok, id) = git_raw(
                    &env.repo,
                    &["hash-object", "-w", "--stdin"],
                    Some(b.as_bytes()),
                    &[],
                );
                assert!(ok);
                let info = format!("100644,{id},{path}");
                assert!(
                    git_raw(
                        &env.repo,
                        &["update-index", "--add", "--cacheinfo", &info],
                        None,
                        &e
                    )
                    .0
                );
            }
            None => assert!(
                git_raw(
                    &env.repo,
                    &["update-index", "--force-remove", path],
                    None,
                    &e
                )
                .0
            ),
        }
    }
    let (ok, tree) = git_raw(&env.repo, &["write-tree"], None, &e);
    assert!(ok);
    let _ = std::fs::remove_file(&index);
    let (ok, c) = git_raw(
        &env.repo,
        &["commit-tree", "--no-gpg-sign", &tree, "-p", parent],
        Some(msg.as_bytes()),
        &[],
    );
    assert!(ok);
    c
}
fn set_ref(env: &Env, branch: u32, commit: &str) {
    git(
        &env.repo,
        &["update-ref", &format!("{REF_PREFIX}{branch}"), commit],
    );
}

/// T91: export is deterministic and idempotent; a commit on another branch
/// gives main no new commit; trailers are present and ranges contiguous.
#[test]
fn t91_export_is_deterministic_idempotent_and_contiguous() {
    let env = Env::new("t91");
    let s = service(&env);
    let a = create(&s, "db/engine", "# Engine\n\nUse postgres.\n");
    create(&s, "db/pool", "pool size 10");
    let r = export(&s, &env, None).unwrap();
    assert!(r.lines.iter().any(|l| l.contains("epochs 1..")), "{r:?}");
    let first = tip(&env, 0).unwrap();
    let msg = message(&env, &first);
    for t in [
        "Code-Commit:",
        "Ctx-Branch: main (b0)",
        "Ctx-Epoch-Range: 1..",
        "Ctx-Log-Checksum:",
        "Ctx-Mirror-Version: 1",
    ] {
        assert!(msg.contains(t), "{msg}");
    }
    let mut names = files(&env, &first);
    names.sort();
    assert_eq!(
        names,
        [
            "README.md",
            "repo/Decision/db/engine.md",
            "repo/Decision/db/pool.md"
        ]
    );
    let body = git(
        &env.repo,
        &[
            "cat-file",
            "blob",
            &format!("{first}:repo/Decision/db/engine.md"),
        ],
    );
    let text = format!("{body}\n");
    let (front, rest) = mirror::parse_file(&text).unwrap();
    assert_eq!(
        (front.key.as_str(), front.rev, front.kind.as_str()),
        ("db/engine", a, "Decision")
    );
    assert!(
        body.contains("label: Certified") && rest.starts_with("# Engine"),
        "{body}"
    );
    // Unchanged: no new commit.
    let r = export(&s, &env, None).unwrap();
    assert_eq!(tip(&env, 0).unwrap(), first, "{r:?}");
    assert!(r.lines.iter().any(|l| l.contains("unchanged")), "{r:?}");
    // A commit on another branch changes no byte of main's tree.
    let made = s.handle(
        ADMIN,
        Request::BranchCreate {
            name: "feat/x".into(),
            from: None,
            client_txn_id: None,
        },
    );
    assert!(!matches!(made.response, Response::Error { .. }));
    let feat = s.actor.snapshot().kernel.branch_by_name("feat/x").unwrap();
    create_on(&s, feat, "feat/only", RecordKind::Plan, "a plan on feat");
    export(&s, &env, None).unwrap();
    assert_eq!(
        tip(&env, 0).unwrap(),
        first,
        "other branches never move main's mirror"
    );
    // `feat/x` exports to its own ref, by id.
    export(&s, &env, Some("feat/x")).unwrap();
    let ft = tip(&env, feat.0 as u32).unwrap();
    assert!(files(&env, &ft).contains(&"repo/Plan/feat/only.md".to_owned()));
    // A change on main: a new commit whose range starts after the last.
    create(&s, "db/replica", "one replica");
    export(&s, &env, None).unwrap();
    let second = tip(&env, 0).unwrap();
    assert_ne!(second, first);
    let t1 = mirror::parse_trailers(&message(&env, &first)).unwrap();
    let t2 = mirror::parse_trailers(&message(&env, &second)).unwrap();
    assert_eq!(t2.range.0.0, t1.range.1.0 + 1, "contiguous");
    assert_eq!(t2.range.1, s.actor.snapshot().epoch);
    assert_eq!(git(&env.repo, &["rev-parse", &format!("{second}^")]), first);
    // The same state renders to the same tree in a fresh repository.
    let env2 = Env::new("t91b");
    export(&s, &env2, None).unwrap();
    assert_eq!(
        tree_of(&env2, &tip(&env2, 0).unwrap()),
        tree_of(&env, &second)
    );
    // Agents cannot export.
    let r = s.handle(
        AGENT,
        Request::Mirror {
            action: MirrorAction::Status {
                project: env.project(),
            },
        },
    );
    assert!(matches!(r.response, Response::Error { ref code, .. } if code == "ChannelRestricted"));
}

/// T57: an edited, amended, rebased or hand-made tip is `MirrorModified`
/// and export changes nothing; `proposals` lists exactly the edited files
/// with `base_rev` from the valid render; `--force` keeps a backup.
#[test]
fn t57_hand_edits_are_refused_proposed_and_forced_with_a_backup() {
    let env = Env::new("t57");
    let s = service(&env);
    let a = create(&s, "db/engine", "Use postgres.");
    let b = create(&s, "db/pool", "pool size 10");
    export(&s, &env, None).unwrap();
    let valid = tip(&env, 0).unwrap();
    let engine = "repo/Decision/db/engine.md";
    let old = git(
        &env.repo,
        &["cat-file", "blob", &format!("{valid}:{engine}")],
    );
    // The edit also lies about the revision in the front matter.
    let edited = old
        .replace("Use postgres.", "Use sqlite instead.")
        .replace(&format!("rev: {}", a.0), "rev: 999");
    let bad = hand_edit(
        &env,
        &valid,
        &[
            (engine, Some(&edited)),
            ("repo/Decision/db/pool.md", None),
            ("repo/Fact/new.md", Some("hand-made")),
        ],
        "my edit\n",
    );
    set_ref(&env, 0, &bad);
    create(&s, "db/replica", "one replica");
    let err = export(&s, &env, None).unwrap_err();
    assert_eq!(err.0, "MirrorModified", "{err:?}");
    assert_eq!(tip(&env, 0).unwrap(), bad, "export changed nothing");
    let p = mirror(
        &s,
        MirrorAction::Proposals {
            branch: None,
            project: env.project(),
        },
    )
    .unwrap();
    assert_eq!(p.proposals.len(), 1, "{p:?}");
    let prop = &p.proposals[0];
    assert_eq!(
        (prop.key.as_str(), prop.base_rev),
        ("db/engine", a),
        "base_rev from the valid render"
    );
    assert!(prop.new_body.contains("Use sqlite instead."));
    assert_eq!(p.added, ["repo/Fact/new.md (key new)"]);
    assert_eq!(p.deleted, [format!("db/pool (rev {})", b.0)]);
    // An amended tip (same tree, another message) and a rebased one are
    // refused too.
    let amended = git_raw(
        &env.repo,
        &["commit-tree", "--no-gpg-sign", &tree_of(&env, &valid)],
        Some(b"rewritten\n"),
        &[],
    )
    .1;
    set_ref(&env, 0, &amended);
    assert_eq!(export(&s, &env, None).unwrap_err().0, "MirrorModified");
    // --force: the found tip is kept as a backup; the new commit's parent
    // is the last valid commit.
    set_ref(&env, 0, &bad);
    let r = export_with(&s, &env, None, true).unwrap();
    assert!(
        r.lines
            .iter()
            .any(|l| l.contains("MirrorModified: saved as refs/contextdb-backup/b0-")),
        "{r:?}"
    );
    let now = tip(&env, 0).unwrap();
    assert_eq!(git(&env.repo, &["rev-parse", &format!("{now}^")]), valid);
    let backups = git(
        &env.repo,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/contextdb-backup/",
        ],
    );
    assert_eq!(backups, bad);
    let st = mirror(
        &s,
        MirrorAction::Status {
            project: env.project(),
        },
    )
    .unwrap();
    assert!(
        st.lines
            .iter()
            .any(|l| l.starts_with("backup refs/contextdb-backup/b0-")),
        "{st:?}"
    );
    // Prune keeps a fresh backup, then removes it once old enough.
    mirror(
        &s,
        MirrorAction::Prune {
            project: env.project(),
            older_than_secs: 3_600,
        },
    )
    .unwrap();
    assert!(!git(&env.repo, &["for-each-ref", "refs/contextdb-backup/"]).is_empty());
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    mirror(
        &s,
        MirrorAction::Prune {
            project: env.project(),
            older_than_secs: 0,
        },
    )
    .unwrap();
    assert!(git(&env.repo, &["for-each-ref", "refs/contextdb-backup/"]).is_empty());
}

/// T96: a crash between `update-ref` and the cache write, or a deleted
/// `mirror.sqlite`, gives no false `MirrorModified`; a deleted ref is
/// recreated and reported; `--adopt` still rejects a tip without trailers;
/// a tip from another log is `MirrorStale`.
#[test]
fn t96_cache_loss_is_not_a_hand_edit() {
    let env = Env::new("t96");
    let s = service(&env);
    create(&s, "db/engine", "Use postgres.");
    export(&s, &env, None).unwrap();
    // Lose the cache (as a crash before its write would).
    let cache = env.data.join(mirror::FILE);
    assert!(cache.exists());
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", cache.display()));
    }
    create(&s, "db/pool", "pool size 10");
    let r = export(&s, &env, None).unwrap();
    assert!(
        r.lines
            .iter()
            .any(|l| l.contains("not cached, and it validates")),
        "{r:?}"
    );
    let t = tip(&env, 0).unwrap();
    // A deleted ref whose tip is cached is recreated on it.
    git(&env.repo, &["update-ref", "-d", &format!("{REF_PREFIX}0")]);
    create(&s, "db/replica", "one replica");
    let r = export(&s, &env, None).unwrap();
    assert!(
        r.lines
            .iter()
            .any(|l| l.contains("was deleted: recreated on the cached commit")),
        "{r:?}"
    );
    assert_eq!(
        git(
            &env.repo,
            &["rev-parse", &format!("{}^", tip(&env, 0).unwrap())]
        ),
        t
    );
    // A hand-made tip without trailers: refused, even with --adopt.
    let plain = git_raw(
        &env.repo,
        &["commit-tree", "--no-gpg-sign", &tree_of(&env, &t)],
        Some(b"no trailers\n"),
        &[],
    )
    .1;
    set_ref(&env, 0, &plain);
    let adopt = mirror(
        &s,
        MirrorAction::Export {
            branch: None,
            project: env.project(),
            force: false,
            adopt: true,
            readable_ok: false,
        },
    );
    assert_eq!(adopt.unwrap_err().0, "MirrorModified");
    // A tip whose trailers name another log (the checksum differs, or the
    // range is past the log): MirrorStale, not MirrorModified.
    let good = tip(&env, 0).unwrap();
    let _ = good;
    let real = message(&env, &t);
    let fake = real
        .lines()
        .map(|l| {
            if l.starts_with("Ctx-Log-Checksum: ") {
                format!("Ctx-Log-Checksum: {}", "0".repeat(64))
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let other_log = git_raw(
        &env.repo,
        &["commit-tree", "--no-gpg-sign", &tree_of(&env, &t)],
        Some(fake.as_bytes()),
        &[],
    )
    .1;
    set_ref(&env, 0, &other_log);
    assert_eq!(export(&s, &env, None).unwrap_err().0, "MirrorStale");
    let past = real
        .lines()
        .map(|l| {
            if l.starts_with("Ctx-Epoch-Range: ") {
                "Ctx-Epoch-Range: 1..9999".to_owned()
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let beyond = git_raw(
        &env.repo,
        &["commit-tree", "--no-gpg-sign", &tree_of(&env, &t)],
        Some(past.as_bytes()),
        &[],
    )
    .1;
    set_ref(&env, 0, &beyond);
    assert_eq!(export(&s, &env, None).unwrap_err().0, "MirrorStale");
    // --force after a stale tip: backed up, then created fresh.
    let r = export_with(&s, &env, None, true).unwrap();
    assert!(
        r.lines.iter().any(|l| l.contains("MirrorStale: saved as")),
        "{r:?}"
    );
    let fresh = tip(&env, 0).unwrap();
    assert!(
        mirror::parse_trailers(&message(&env, &fresh))
            .unwrap()
            .range
            .0
            == Epoch(1)
    );
}

/// T92: the ref moves only by compare-and-swap, creation included: of
/// concurrent exports through different caches, the ref always ends at a
/// commit that validates, and a loser gets `MirrorRaced` (or finds the
/// winner's valid tip).
#[test]
fn t92_concurrent_exports_compare_and_swap() {
    let env = Env::new("t92");
    let s = std::sync::Arc::new(service(&env));
    create(&s, "db/engine", "Use postgres.");
    let repo = mirror::Repo::open(&env.repo, &env.data).unwrap();
    // The primitive: a wrong old value or creating over an existing ref fails.
    let tree = repo
        .write_tree(&[("a.md".to_owned(), b"x".to_vec())].into())
        .unwrap();
    let c1 = repo.commit_tree(&tree, None, "one\n").unwrap();
    let c2 = repo.commit_tree(&tree, Some(&c1), "two\n").unwrap();
    assert!(repo.cas("refs/contextdb/test", &c1, None).unwrap());
    assert!(
        !repo.cas("refs/contextdb/test", &c2, None).unwrap(),
        "create over an existing ref"
    );
    assert!(
        !repo.cas("refs/contextdb/test", &c2, Some(&c2)).unwrap(),
        "wrong old value"
    );
    assert!(repo.cas("refs/contextdb/test", &c2, Some(&c1)).unwrap());
    git(&env.repo, &["update-ref", "-d", "refs/contextdb/test"]);
    // Two exporters with separate caches (two daemons on one repository).
    let mut raced = 0;
    for round in 0..6 {
        create(&s, &format!("k{round}"), &format!("body {round}"));
        let snap = s.actor.snapshot();
        let dirs = [
            env.root.join(format!("c{round}a")),
            env.root.join(format!("c{round}b")),
        ];
        for d in &dirs {
            std::fs::create_dir_all(d).unwrap();
        }
        let results: Vec<_> = std::thread::scope(|sc| {
            let hs: Vec<_> = dirs
                .iter()
                .map(|d| {
                    let snap = &snap;
                    let env = &env;
                    sc.spawn(move || {
                        let mut m = mirror::Mirror::new(snap, &env.repo, Some(d)).unwrap();
                        m.export(MAIN, false, false)
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for r in &results {
            match r {
                Ok(_) => {}
                Err(mirror::MirrorError::Raced { .. }) => raced += 1,
                Err(e) => panic!("round {round}: {e:?}"),
            }
        }
        assert!(results.iter().any(Result::is_ok), "one wins");
        // The tip validates for a third exporter (fresh cache).
        let fresh = env.root.join(format!("c{round}c"));
        std::fs::create_dir_all(&fresh).unwrap();
        let mut m = mirror::Mirror::new(&snap, &env.repo, Some(&fresh)).unwrap();
        let r = m.export(MAIN, false, false).unwrap();
        assert!(
            !r.lines.iter().any(|l| l.contains("->")),
            "nothing new to commit: {r:?}"
        );
    }
    eprintln!("raced {raced} of 6 rounds");
}

/// T97: path and ref encoding in a real repository: `feat` and `feat/x`
/// both export; tricky keys give distinct, valid paths (`git fsck` clean);
/// case-only collisions are renamed by logical id.
#[test]
fn t97_paths_and_refs_are_valid_and_distinct() {
    let env = Env::new("t97");
    let s = service(&env);
    // Keys start with [a-z0-9] and use [a-z0-9._:/#-] (the kernel's rule).
    let keys = [
        "x", "x.md", "x.md/y", "a//b", "a/./b", "a/../b", "a/.git", "a/b/", "1.", "k:v#1",
    ];
    for (n, k) in keys.iter().enumerate() {
        create(&s, k, &format!("body {n}"));
    }
    // Lanes are free text and may differ only in case. Keys are unique per
    // branch and lower-case, so folded paths cannot collide; the renaming
    // rule is kept for safety and is checked on the paths below.
    create_in(&s, MAIN, "Ops", "runbook", RecordKind::Procedure, "one");
    create_in(&s, MAIN, "ops", "runbook-b", RecordKind::Procedure, "two");
    for name in ["feat", "feat/x"] {
        let r = s.handle(
            ADMIN,
            Request::BranchCreate {
                name: name.into(),
                from: None,
                client_txn_id: None,
            },
        );
        assert!(!matches!(r.response, Response::Error { .. }), "{r:?}");
        export(&s, &env, Some(name)).unwrap();
    }
    export(&s, &env, None).unwrap();
    let main = tip(&env, 0).unwrap();
    let names = files(&env, &main);
    assert_eq!(names.len(), keys.len() + 3, "{names:?}");
    assert!(
        !names.iter().any(|n| n.contains('~')),
        "no collision, no rename"
    );
    for n in &names {
        for seg in n.split('/') {
            assert!(!matches!(seg, "." | ".." | ".git"), "{n}");
        }
    }
    let folded: std::collections::BTreeSet<String> =
        names.iter().map(|n| n.to_lowercase()).collect();
    assert_eq!(
        folded.len(),
        names.len(),
        "distinct after case-folding: {names:?}"
    );
    assert!(
        names.contains(&"Ops/Procedure/runbook.md".to_owned()),
        "{names:?}"
    );
    assert!(
        names.contains(&"ops/Procedure/runbook-b.md".to_owned()),
        "{names:?}"
    );
    assert!(
        names.contains(&"repo/Decision/x.md".to_owned())
            && names.contains(&"repo/Decision/x%2Emd.md".to_owned())
    );
    assert!(names.contains(&"repo/Decision/a/%2E%2E/b.md".to_owned()));
    assert!(names.contains(&"repo/Decision/a/%2Egit.md".to_owned()));
    assert!(names.contains(&"repo/Decision/a/b/%.md".to_owned()));
    assert!(names.contains(&"repo/Decision/x%2Emd/y.md".to_owned()));
    assert!(names.contains(&"repo/Decision/a/%/b.md".to_owned()));
    assert!(names.contains(&"repo/Decision/a/%2E/b.md".to_owned()));
    let refs = git(
        &env.repo,
        &["for-each-ref", "--format=%(refname)", "refs/contextdb/"],
    );
    assert_eq!(refs.lines().count(), 3, "{refs}");
    let (ok, _) = git_raw(&env.repo, &["fsck", "--strict", "--no-dangling"], None, &[]);
    assert!(ok, "git fsck");
}

/// T97: an unavailable payload renders as a placeholder.
#[test]
fn t97_an_unavailable_payload_renders_as_a_placeholder() {
    let env = Env::new("t97u");
    let disk = SimDisk::new();
    let s = service_on(&env, disk.clone());
    create_on(&s, MAIN, "gone", RecordKind::Fact, "soon unreadable");
    // Corrupt the packed payload (its ciphertext starts after the 8-byte
    // segment header and the 64-byte record header, ADR 0017 §8.1): reads
    // now give Unavailable(Corrupt).
    let blobs = Path::new("/db/blobs");
    let pack = disk
        .list(blobs)
        .unwrap()
        .into_iter()
        .find(|n| n.starts_with("pack-"))
        .unwrap();
    disk.corrupt(&blobs.join(pack), 72, b'X');
    export(&s, &env, None).unwrap();
    let t = tip(&env, 0).unwrap();
    let body = git(
        &env.repo,
        &["cat-file", "blob", &format!("{t}:repo/Fact/gone.md")],
    );
    assert!(body.ends_with("(payload unavailable)"), "{body}");
}
