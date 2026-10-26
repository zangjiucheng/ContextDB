//! P6 observer on branches with a real git checkout (ADR 0013 §7, m3): the
//! observer commits a scope on the ContextDB branch named like the
//! worktree's checked-out git branch; a git branch with no ContextDB branch
//! writes its observations to `main`.
use contextdb_daemon::server::{Client, Daemon, Options};
use contextdb_model::*;
use contextdb_proto::proto::*;
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

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
fn git(root: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(root)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}
fn temp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("p6-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}
/// The anchor keys of `scope` on `branch` (by git name), and the branch read.
fn anchors(d: &Daemon, scope: &str, branch: Option<&str>) -> (BranchId, Vec<String>) {
    let mut c = Client::connect(d.socket(Channel::Observer)).unwrap();
    match c
        .call(&Request::AnchorState {
            scope: scope.into(),
            branch: branch.map(str::to_owned),
        })
        .unwrap()
        .response
    {
        Response::AnchorState {
            branch, anchors, ..
        } => (branch, anchors.into_iter().map(|a| a.key).collect()),
        r => panic!("{r:?}"),
    }
}

#[test]
fn observer_follows_the_worktree_branch() {
    let root = temp("repo");
    let data = temp("data");
    git(&root, &["init", "-q"]);
    git(&root, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(root.join("a.rs"), "a").unwrap();
    let d = Daemon::start(&data, &Options::default()).unwrap();
    let (ds, rs) = (data.to_str().unwrap(), root.to_str().unwrap());
    let (rc, out) = ctx(&["branch", "create", ds, "feature"]);
    assert_eq!(rc, 0, "{out}");
    let scope = contextdb_anchors::Repo::open(&root, Instant::now() + Duration::from_secs(5))
        .unwrap()
        .scope();
    // On git branch `feature`, observations land on ContextDB `feature`.
    let (rc, out) = ctx(&["anchors", "add", ds, rs, "a.rs"]);
    assert_eq!(rc, 0, "{out}");
    let (on, keys) = anchors(&d, &scope, Some("feature"));
    assert_eq!(on, BranchId(1));
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(keys[0].ends_with("a.rs"));
    let (on, keys) = anchors(&d, &scope, None);
    assert_eq!(
        (on, keys.len()),
        (MAIN, 0),
        "main does not see the branch's anchor"
    );
    // A git branch with no ContextDB branch observes into main.
    git(&root, &["checkout", "-q", "-b", "elsewhere"]);
    std::fs::write(root.join("b.rs"), "b").unwrap();
    let (rc, out) = ctx(&["anchors", "add", ds, rs, "b.rs"]);
    assert_eq!(rc, 0, "{out}");
    let (on, keys) = anchors(&d, &scope, Some("elsewhere"));
    assert_eq!(on, MAIN);
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(keys[0].ends_with("b.rs"));
    let (_, keys) = anchors(&d, &scope, Some("feature"));
    assert_eq!(keys.len(), 1, "feature keeps only its own anchor");
    drop(d);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}
