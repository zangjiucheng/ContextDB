//! `ctxd --multi-user` (ADR 0017 §3, owner decision 2026-11-08): the switch
//! that lets the committed access document map other uids. Without it a
//! document naming other principals maps nobody (single-user, as in P8);
//! with it the same document enters multi-user mode.
use contextdb_daemon::proto::*;
use contextdb_model::*;
use contextdb_proto::{client::Client, fsguard, lease};
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn base(name: &str) -> PathBuf {
    let d = Path::new("/tmp")
        .canonicalize()
        .unwrap()
        .join(format!("ctxd-multi-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o711)).unwrap();
    d
}
/// Kills the daemon whatever happens in the test.
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn start(dir: &Path, multi_user: bool) -> Running {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ctxd"));
    cmd.arg(dir);
    if multi_user {
        cmd.arg("--multi-user");
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let running = Running(child);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !lease::read_lease(&dir.join("ctxd.lease")).is_some_and(|l| l.is_live()) {
        assert!(Instant::now() < deadline, "no lease within 20 s");
        std::thread::sleep(Duration::from_millis(50));
    }
    running
}
fn admin(dir: &Path) -> Client {
    let sock = lease::socket_dir(dir, fsguard::current_uid())
        .join("private")
        .join("admin.sock");
    Client::connect(&sock).unwrap()
}
fn doc() -> AccessDoc {
    let all: BTreeSet<String> = ["*members".to_owned()].into();
    AccessDoc {
        version: 1,
        owner: "alice".into(),
        principals: BTreeMap::from([
            ("alice".to_owned(), fsguard::current_uid()),
            ("bob".to_owned(), 4242),
        ]),
        groups: BTreeMap::new(),
        lanes: BTreeMap::from([(
            "repo".to_owned(),
            LaneEntry {
                members: all.clone(),
                writers: all,
                ..LaneEntry::owner_only()
            },
        )]),
        code_lanes: LaneEntry::code_default(),
        personal_egress: false,
        default_lane: LaneEntry::owner_only(),
    }
}
/// Commits the document and reports whether the daemon is now multi-user.
fn apply_and_show(dir: &Path) -> bool {
    let mut c = admin(dir);
    let r = c
        .call(&Request::SetAccess {
            doc: doc(),
            client_txn_id: None,
        })
        .unwrap()
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    match c.call(&Request::AccessShow).unwrap().response {
        Response::Access { multi_user, .. } => multi_user,
        other => panic!("{other:?}"),
    }
}

#[test]
fn multi_user_is_a_switch() {
    fsguard::private_umask();
    let off = base("off");
    {
        let _d = start(&off, false);
        assert!(
            !apply_and_show(&off),
            "without --multi-user nobody is mapped"
        );
    }
    let on = base("on");
    {
        let _d = start(&on, true);
        assert!(
            apply_and_show(&on),
            "with --multi-user the document maps bob"
        );
    }
    let help = Command::new(env!("CARGO_BIN_EXE_ctxd"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&help.stderr).contains("--multi-user"));
    let _ = std::fs::remove_dir_all(&off);
    let _ = std::fs::remove_dir_all(&on);
}
