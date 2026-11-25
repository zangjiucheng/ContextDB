//! P11 milestone 4, unit U4 (ADR 0019 §5.6, owner decision O5): git
//! mirrors over history whose payloads GC collected. A collected body is
//! verified by its hash and its front matter, so such a mirror validates
//! and exports; only a shredded content makes it `PayloadGone`.
//!
//! The helpers are those of `p8_mirror.rs`; GC runs on the reopened store
//! (`gc_plan`, `gc_apply`), as in `contextdb-store/tests/p11_gc.rs`.
use contextdb_daemon::{
    actor,
    mirror::{self, REF_PREFIX},
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, Payload, SimDisk, Store, Unavailable, gc, session::SessionLog};
use std::path::{Path, PathBuf};
use std::process::Command;

const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
type Svc = Service<SimDisk>;
const ENGINE: &str = "repo/Decision/db/engine.md";
const V1: &str = "# Engine\n\nUse postgres.";
const V2: &str = "# Engine\n\nUse postgres 17.";

struct Env {
    root: PathBuf,
    repo: PathBuf,
    data: PathBuf,
}
impl Env {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("ctx-p11-mirror-{name}-{}", std::process::id()));
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
    /// `mirror.sqlite` lost: the next export validates the tip.
    fn lose_cache(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(self.data.join(format!("{}{suffix}", mirror::FILE)));
        }
    }
    fn clean_mark(&self) -> Option<(String, mirror::CleanMark)> {
        mirror::Cache::open(Some(&self.data.join(mirror::FILE)))
            .unwrap()
            .clean(&self.project(), MAIN)
            .unwrap()
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
fn service_on(env: &Env, disk: &SimDisk) -> Svc {
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
fn commit(s: &Svc, blobs: Vec<Vec<u8>>, ops: Vec<Op>) {
    match (s.handle(
        ADMIN,
        Request::Raw {
            blobs,
            batch: Batch::new(ops),
        },
    ))
    .response
    {
        Response::Committed { .. } => {}
        other => panic!("{other:?}"),
    }
}
fn create(s: &Svc, key: &str, body: &str) {
    let op = Op::CreateLogical {
        origin: Default::default(),
        key: key.into(),
        kind: RecordKind::Decision,
        scope_lane: "repo".into(),
        content: h(body.as_bytes()),
        adopt_as_base: false,
    };
    commit(s, vec![body.as_bytes().to_vec()], vec![op]);
}
/// A new head revision of `key`: its old head is superseded.
fn supersede(s: &Svc, key: &str, body: &str) {
    let snap = s.actor.snapshot();
    let v = snap.kernel.branch(MAIN).unwrap();
    let l = v.logical_by_key(key).unwrap();
    let ops = vec![
        Op::AddRevision {
            logical: LogicalRef::Key(key.into()),
            content: h(body.as_bytes()),
            adopt_as_base: false,
        },
        Op::SetHead {
            logical: l.into(),
            expected: v.head(l).unwrap().into(),
            new: RevRef::New(0),
        },
    ];
    commit(s, vec![body.as_bytes().to_vec()], ops);
}
fn shred(s: &Svc, key: &str) {
    match (s.handle(
        ADMIN,
        Request::Shred {
            key: key.into(),
            rev: None,
            with_reviews: false,
            confirm: true,
            client_txn_id: None,
        },
    ))
    .response
    {
        Response::Shred { epoch: Some(_), .. } => {}
        other => panic!("{other:?}"),
    }
}
/// Payload GC with the cutoff at the tail (no K3), on the store reopened
/// after `s` stops; returns the collected contents and a running service.
fn collect(env: &Env, disk: &SimDisk, s: Svc) -> (Vec<ContentHash>, Svc) {
    s.handle(ADMIN, Request::Shutdown);
    drop(s);
    let (mut store, _) =
        Store::open(disk.clone(), Path::new("/db"), &OpenOptions::default()).unwrap();
    let now = store.epoch();
    let none = |_: &[u8]| Vec::new();
    let plan = gc::gc_plan(
        store.kernel().unwrap(),
        &store.log_reader(),
        now,
        now,
        &none,
    )
    .unwrap();
    let report = store
        .gc_apply(&plan, &Default::default(), now, &none)
        .unwrap();
    drop(store);
    (report.collected, service_on(env, disk))
}
fn mirror(s: &Svc, action: MirrorAction) -> Result<MirrorReport, (String, String)> {
    match s.handle(ADMIN, Request::Mirror { action }).response {
        Response::Mirror { report } => Ok(report),
        Response::Error { code, detail } => Err((code, detail)),
        other => panic!("{other:?}"),
    }
}
fn export_with(
    s: &Svc,
    env: &Env,
    force: bool,
    rebuild: bool,
) -> Result<MirrorReport, (String, String)> {
    mirror(
        s,
        MirrorAction::Export {
            branch: None,
            project: env.project(),
            force,
            adopt: false,
            readable_ok: false,
            rebuild,
        },
    )
}
fn export(s: &Svc, env: &Env) -> Result<MirrorReport, (String, String)> {
    export_with(s, env, false, false)
}
fn tip(env: &Env) -> String {
    git(&env.repo, &["rev-parse", &format!("{REF_PREFIX}0")])
}
fn show(env: &Env, commit: &str, path: &str) -> String {
    git(
        &env.repo,
        &["cat-file", "blob", &format!("{commit}:{path}")],
    )
}
/// A commit on `parent` with `path` set to `body` and the message (so the
/// trailers) of `like`: only its tree can tell it from a valid export.
fn forge(env: &Env, parent: &str, path: &str, body: &str, like: &str) -> String {
    let index = env.root.join("edit.index");
    let idx = index.display().to_string();
    let e = [("GIT_INDEX_FILE", idx.as_str())];
    assert!(git_raw(&env.repo, &["read-tree", parent], None, &e).0);
    let (ok, id) = git_raw(
        &env.repo,
        &["hash-object", "-w", "--stdin"],
        Some(body.as_bytes()),
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
    let (ok, tree) = git_raw(&env.repo, &["write-tree"], None, &e);
    assert!(ok);
    let _ = std::fs::remove_file(&index);
    let msg = git(&env.repo, &["log", "-1", "--format=%B", like]);
    let (ok, c) = git_raw(
        &env.repo,
        &["commit-tree", "--no-gpg-sign", &tree, "-p", parent],
        Some(msg.as_bytes()),
        &[],
    );
    assert!(ok);
    c
}
fn set_tip(env: &Env, commit: &str) {
    git(
        &env.repo,
        &["update-ref", &format!("{REF_PREFIX}0"), commit],
    );
}

/// A mirror exported while `db/engine` read V1; V1 is then superseded by
/// V2 and collected. Returns the first tip and a running service.
fn exported_then_collected(env: &Env, disk: &SimDisk) -> (String, Svc) {
    let s = service_on(env, disk);
    create(&s, "db/engine", V1);
    create(&s, "db/pool", "pool size 10");
    export(&s, env).unwrap();
    let first = tip(env);
    assert!(show(env, &first, ENGINE).ends_with(V1));
    supersede(&s, "db/engine", V2);
    let (collected, s) = collect(env, disk, s);
    assert_eq!(collected, vec![h(V1.as_bytes())]);
    assert_eq!(
        s.actor.snapshot().log.read_blob(&h(V1.as_bytes())),
        Payload::Unavailable(Unavailable::Collected)
    );
    (first, s)
}

/// A tip whose epoch had the collected V1 as a head still validates from
/// its own tree (cache lost, so nothing short-circuits); `proposals` finds
/// no hand edits; a new export extends it; `--rebuild` writes the current
/// render as one fresh commit.
#[test]
fn a_mirror_over_collected_history_validates_exports_and_rebuilds() {
    let env = Env::new("validates");
    let disk = SimDisk::new();
    let (first, s) = exported_then_collected(&env, &disk);
    env.lose_cache();
    let r = mirror(
        &s,
        MirrorAction::Proposals {
            branch: None,
            project: env.project(),
        },
    )
    .unwrap();
    assert!(
        r.lines.iter().any(|l| l.contains("has no hand edits")),
        "{r:?}"
    );
    let r = export(&s, &env).unwrap();
    assert!(
        r.lines
            .iter()
            .any(|l| l.contains(&format!("accepted {first}: not cached, and it validates"))),
        "{r:?}"
    );
    let second = tip(&env);
    assert_ne!(second, first);
    assert_eq!(git(&env.repo, &["rev-parse", &format!("{second}^")]), first);
    assert!(show(&env, &second, ENGINE).ends_with(V2));
    // Unchanged after a second cache loss: the new tip validates too.
    env.lose_cache();
    let r = export(&s, &env).unwrap();
    assert!(r.lines.iter().any(|l| l.contains("unchanged")), "{r:?}");
    assert_eq!(tip(&env), second);

    let r = export_with(&s, &env, false, true).unwrap();
    assert!(
        r.lines
            .iter()
            .any(|l| l.contains("a fresh history replacing")),
        "{r:?}"
    );
    let rebuilt = tip(&env);
    assert_eq!(git(&env.repo, &["rev-list", "--count", &rebuilt]), "1");
    assert_eq!(
        git(&env.repo, &["rev-parse", &format!("{rebuilt}^{{tree}}")]),
        git(&env.repo, &["rev-parse", &format!("{second}^{{tree}}")])
    );
    env.lose_cache();
    let r = export(&s, &env).unwrap();
    assert!(r.lines.iter().any(|l| l.contains("unchanged")), "{r:?}");
}

/// A collected body is accepted only with the front matter the re-render
/// computes: an edited label, with the tip's trailers, is `MirrorModified`.
#[test]
fn an_edited_front_matter_on_a_collected_body_is_modified() {
    let env = Env::new("front");
    let disk = SimDisk::new();
    let (first, s) = exported_then_collected(&env, &disk);
    let file = show(&env, &first, ENGINE);
    let label = file.lines().find(|l| l.starts_with("label: ")).unwrap();
    let edited = file.replace(label, "label: Forged");
    set_tip(&env, &forge(&env, &first, ENGINE, &edited, &first));
    let (code, _) = export(&s, &env).unwrap_err();
    assert_eq!(code, "MirrorModified");
    // The unedited file, under the same trailers, validates.
    set_tip(&env, &forge(&env, &first, ENGINE, &file, &first));
    export(&s, &env).unwrap();
}

/// A collected body is accepted only if it hashes to the content: an
/// edited body, with the tip's trailers, is `MirrorModified`.
#[test]
fn an_edited_collected_body_is_modified() {
    let env = Env::new("body");
    let disk = SimDisk::new();
    let (first, s) = exported_then_collected(&env, &disk);
    let file = show(&env, &first, ENGINE);
    let edited = file.replace("Use postgres.", "Use mysql.");
    assert_ne!(edited, file);
    set_tip(&env, &forge(&env, &first, ENGINE, &edited, &first));
    let (code, _) = export(&s, &env).unwrap_err();
    assert_eq!(code, "MirrorModified");
    // `proposals` finds the valid parent across the collected body.
    let r = mirror(
        &s,
        MirrorAction::Proposals {
            branch: None,
            project: env.project(),
        },
    )
    .unwrap();
    let keys: Vec<&str> = r.proposals.iter().map(|p| p.key.as_str()).collect();
    assert_eq!(keys, ["db/engine"], "{r:?}");
}

/// Over collected history, a shredded content still makes the mirror
/// `MirrorStale{PayloadGone}`, even with `--force`.
#[test]
fn a_shredded_content_is_still_payload_gone() {
    let env = Env::new("shred");
    let disk = SimDisk::new();
    let (_, s) = exported_then_collected(&env, &disk);
    export(&s, &env).unwrap();
    shred(&s, "db/pool");
    for force in [false, true] {
        let (code, detail) = export_with(&s, &env, force, false).unwrap_err();
        assert_eq!(code, "MirrorStale");
        assert!(detail.contains("PayloadGone"), "{detail}");
    }
}

/// The clean-history verdict is cached with the GC generation: a GC
/// invalidates it, and the next export records the new one.
#[test]
fn a_gc_changes_the_clean_cache_mark() {
    let env = Env::new("mark");
    let disk = SimDisk::new();
    let s = service_on(&env, &disk);
    create(&s, "db/engine", V1);
    supersede(&s, "db/engine", V2);
    create(&s, "secret", "a shredded secret");
    shred(&s, "secret");
    export(&s, &env).unwrap();
    let first = tip(&env);
    let (c, before) = env.clean_mark().unwrap();
    assert_eq!((c.as_str(), before.gc), (first.as_str(), 0));
    let (collected, s) = collect(&env, &disk, s);
    assert_eq!(collected, vec![h(V1.as_bytes())]);
    let r = export(&s, &env).unwrap();
    assert!(r.lines.iter().any(|l| l.contains("unchanged")), "{r:?}");
    let (c, after) = env.clean_mark().unwrap();
    assert_eq!(c, first);
    assert_eq!(after.shred, before.shred);
    assert!(after.gc > 0, "{after:?}");
}
