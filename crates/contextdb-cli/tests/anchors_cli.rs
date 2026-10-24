//! Real CLI/socket/watch and hook preservation, isolated from live dogfood.
use contextdb_daemon::server::{Client, Daemon, Options};
use contextdb_model::*;
use contextdb_proto::proto::*;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
    let p = std::env::temp_dir().join(format!("p5a-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}
fn watermark(d: &Daemon, scope: &str) -> Option<Vec<u8>> {
    let mut c = Client::connect(d.socket(Channel::Observer)).unwrap();
    match c
        .call(&Request::AnchorState {
            scope: scope.into(),
            branch: None,
        })
        .unwrap()
        .response
    {
        Response::AnchorState { watermark, .. } => watermark,
        r => panic!("{r:?}"),
    }
}
#[test]
fn concurrent_routes_watch_restart_unavailable_and_preserved_hooks() {
    let root = temp("main");
    let other = temp("other");
    let data = root.join("state");
    std::fs::create_dir_all(&data).unwrap();
    git(&root, &["init", "-q"]);
    git(&other, &["init", "-q"]);
    std::fs::write(root.join("a.rs"), "a").unwrap();
    std::fs::write(other.join("b.rs"), "b").unwrap();
    let d = Daemon::start(&data, &Options::default()).unwrap();
    let ds = data.to_str().unwrap();
    let rs = root.to_str().unwrap();
    let os = other.to_str().unwrap();
    let mut one = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["anchors", "add", ds, rs, "a.rs"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut two = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["anchors", "add", ds, os, "b.rs"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    assert!(one.wait().unwrap().success());
    assert!(two.wait().unwrap().success());
    let routes =
        Vec::<(String, String)>::from_bytes(&std::fs::read(data.join("anchors.routes")).unwrap())
            .unwrap();
    assert_eq!(routes.len(), 2);
    let repo =
        contextdb_anchors::Repo::open(&root, Instant::now() + Duration::from_secs(5)).unwrap();
    let before = watermark(&d, &repo.scope());
    std::fs::write(data.join("runtime.rs"), "runtime").unwrap();
    let (rc, out) = ctx(&["anchors", "add", ds, rs, "state/runtime.rs"]);
    assert_ne!(rc, 0, "{out}");
    assert!(out.contains("runtime"));
    let mut watch = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(["anchors", "watch", ds, rs])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::fs::write(root.join("a.rs"), "v2").unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while watermark(&d, &repo.scope()) == before {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(20));
    }
    watch.kill().unwrap();
    watch.wait().unwrap();
    let changed = watermark(&d, &repo.scope());
    // An interrupted observer misses a wakeup; a full scan after restart recovers.
    std::fs::write(root.join("a.rs"), "v3").unwrap();
    drop(d);
    let d = Daemon::start(&data, &Options::default()).unwrap();
    assert_eq!(ctx(&["anchors", "scan", ds, rs]).0, 0);
    assert_ne!(watermark(&d, &repo.scope()), changed);
    let good = watermark(&d, &repo.scope());
    let f = std::fs::File::create(root.join("a.rs")).unwrap();
    f.set_len((16 << 20) + 1).unwrap();
    assert_ne!(ctx(&["anchors", "scan", ds, rs]).0, 0);
    assert_eq!(watermark(&d, &repo.scope()), good);
    std::fs::write(root.join("a.rs"), "v3").unwrap();
    let hooks = root.join(".git/hooks");
    let old = b"#!/bin/sh\nprintf '%s' preserved > hook-result\n";
    std::fs::write(hooks.join("post-merge"), old).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        hooks.join("post-merge"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let pre = b"#!/bin/sh\ncat > push-input\n";
    std::fs::write(hooks.join("pre-push"), pre).unwrap();
    std::fs::set_permissions(
        hooks.join("pre-push"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let install = ["anchors", "hooks", ds, rs, "install", "--pre-push"];
    let (rc, out) = ctx(&install);
    assert_eq!(rc, 0, "{out}");
    let once = std::fs::read(hooks.join("post-merge")).unwrap();
    assert_eq!(ctx(&install).0, 0);
    assert_eq!(std::fs::read(hooks.join("post-merge")).unwrap(), once);
    assert!(
        Command::new(hooks.join("post-merge"))
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        std::fs::read(root.join("hook-result")).unwrap(),
        b"preserved"
    );
    let input = format!(
        "(delete) {} refs/heads/one {}\n(delete) {} refs/heads/two {}\n",
        "0".repeat(40),
        "1".repeat(40),
        "0".repeat(40),
        "2".repeat(40)
    );
    let mut pre = Command::new(hooks.join("pre-push"))
        .current_dir(&root)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    pre.stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    assert!(pre.wait().unwrap().success());
    assert_eq!(
        std::fs::read_to_string(root.join("push-input")).unwrap(),
        input
    );
    assert_eq!(ctx(&["anchors", "hooks", ds, rs, "uninstall"]).0, 0);
    assert_eq!(std::fs::read(hooks.join("post-merge")).unwrap(), old);
    assert_eq!(
        std::fs::read(hooks.join("pre-push")).unwrap(),
        b"#!/bin/sh\ncat > push-input\n"
    );
    drop(d);
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(other);
}

#[test]
fn pre_push_refuses_annotated_tag_oid_even_when_it_peels_to_head() {
    let root = temp("tag");
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("a.rs"), "a").unwrap();
    git(&root, &["add", "a.rs"]);
    git(
        &root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    git(
        &root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "tag.gpgsign=false",
            "tag",
            "-a",
            "-m",
            "tag",
            "fixture",
        ],
    );
    let tag = Command::new("git")
        .current_dir(&root)
        .args(["rev-parse", "refs/tags/fixture"])
        .output()
        .unwrap();
    let tag = String::from_utf8(tag.stdout).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args([
            "anchors-pre-push",
            root.to_str().unwrap(),
            root.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            format!(
                "refs/tags/fixture {} refs/tags/fixture {}\n",
                tag.trim(),
                "0".repeat(40)
            )
            .as_bytes(),
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("tags are not peeled"));
    let _ = std::fs::remove_dir_all(root);
}
