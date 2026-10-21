//! `ctx` against a running daemon: status, human validation (shown vector,
//! confirmation) and shutdown; offline commands are refused while it runs.
use contextdb_daemon::{
    proto::*,
    server::{Client, Daemon, Options},
};
use contextdb_model::*;
use std::process::{Command, Stdio};

fn ctx(args: &[&str], stdin: &str) -> (bool, String) {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}
#[test]
fn ctx_status_validate_and_shutdown_through_the_admin_socket() {
    let dir = std::env::temp_dir().join(format!("ctx-daemon-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let path = dir.to_str().unwrap();
    // An agent records X and a Sem-dependent D; X then moves to v2.
    let mut agent = Client::connect(d.socket(Channel::Agent)).unwrap();
    let Response::Session { session } = agent
        .call(&Request::SessionStart {
            harness: "t".into(),
            repo: "r".into(),
            head_sha: "h".into(),
            parent: None,
        })
        .unwrap()
        .response
    else {
        panic!()
    };
    let rec = |agent: &mut Client,
               key: &str,
               body: &str,
               target: WriteTarget,
               deps: Vec<Dep>,
               base: bool| {
        let r = agent
            .call(&Request::Record {
                binding: Binding::root(session),
                kind: RecordKind::Fact,
                body: body.as_bytes().to_vec(),
                target,
                deps,
                adopt_as_base: base,
                client_txn_id: None,
            })
            .unwrap();
        assert!(
            matches!(r.response, Response::Committed { .. }),
            "{key}: {r:?}"
        );
    };
    let create = |key: &str| WriteTarget::Create {
        key: key.into(),
        scope_lane: "repo".into(),
    };
    rec(&mut agent, "x", "x v1", create("x"), Vec::new(), true);
    let dep = Dep {
        target: Target::KeyRev {
            key: "x".into(),
            rev: RevisionId(0),
        },
        kind: EdgeKind::Sem,
    };
    rec(&mut agent, "d", "d", create("d"), vec![dep], false);
    rec(
        &mut agent,
        "x2",
        "x v2",
        WriteTarget::Revise {
            logical: LogicalId(0),
            expected_head: RevisionId(0),
        },
        Vec::new(),
        true,
    );
    let (ok, out) = ctx(&["status", path], "");
    assert!(ok && out.contains("epoch=3"), "{out}");
    // Offline commands open the store and are refused while ctxd holds the lock.
    let (ok, out) = ctx(&["replay", path], "");
    assert!(!ok && out.contains("Locked"), "{out}");
    // Not confirmed: nothing is recorded.
    let (ok, out) = ctx(&["validate", path, "d@1", "verified"], "n\n");
    assert!(
        !ok && out.contains("current [(LogicalId(0), RevisionId(2))]"),
        "{out}"
    );
    let (ok, out) = ctx(&["validate", path, "d@1", "verified"], "y\n");
    assert!(ok && out.contains("recorded at epoch 4"), "{out}");
    let (ok, out) = ctx(&["shutdown", path], "");
    assert!(ok, "{out}");
    d.wait();
    drop(d);
    let (ok, out) = ctx(&["status", path], "");
    assert!(!ok && out.contains("not running"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}
