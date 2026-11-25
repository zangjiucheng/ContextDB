//! P11 milestone 4 unit U3 (ADR 0019 §5.5, §5.7): `ctx gc` against a
//! running daemon, `ctx gc --compact` offline, and the reporting in
//! `ctx doctor` and `ctx team status`.
use contextdb_daemon::server::{Daemon, Options};
use contextdb_model::*;
use contextdb_store::{OpenOptions, ProjectionFile, StdDisk, Store};
use std::path::Path;
use std::process::Command;

const TEAM: TeamId = [0x7a; 16];

fn ctx(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(args)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}
fn open(dir: &Path) -> Store<StdDisk> {
    let options = OpenOptions {
        projections: ProjectionFile::Path(dir.join(contextdb_store::PROJECTIONS_FILE)),
        ..OpenOptions::default()
    };
    Store::open(StdDisk { full_sync: false }, dir, &options)
        .unwrap()
        .0
}
fn start(dir: &Path) -> Daemon {
    Daemon::start(
        dir,
        &Options {
            full_sync: false,
            team_lag_check: None,
            ..Options::default()
        },
    )
    .unwrap()
}
fn mirror_key() -> String {
    let hex: String = TEAM.iter().map(|b| format!("{b:02x}")).collect();
    format!("team/{hex}/x")
}
fn commit(s: &mut Store<StdDisk>, channel: Channel, bodies: &[&str], ops: Vec<Op>) -> CommitResult {
    for b in bodies {
        s.put_blob(b.as_bytes()).unwrap();
    }
    let mut batch = Batch::new(ops);
    batch.channel = channel;
    s.commit(&batch).unwrap()
}
fn h(b: &str) -> ContentHash {
    *blake3::hash(b.as_bytes()).as_bytes()
}
/// A record and a team mirror, each superseded once, then 105 more epochs.
fn populate(dir: &Path) -> RevisionId {
    let mut s = open(dir);
    let create = |key: &str, body: &str, origin: Origin, lane: &str| Op::CreateLogical {
        origin,
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: lane.into(),
        content: h(body),
        adopt_as_base: false,
    };
    let supersede = |key: &str, body: &str, expected: RevisionId| {
        vec![
            Op::AddRevision {
                logical: LogicalRef::Key(key.into()),
                content: h(body),
                adopt_as_base: false,
            },
            Op::SetHead {
                logical: LogicalRef::Key(key.into()),
                expected: expected.into(),
                new: RevRef::New(0),
            },
        ]
    };
    let ops = vec![create("old", "old one", Origin::Local, "repo")];
    let old1 = commit(&mut s, Channel::Agent, &["old one"], ops)
        .allocations
        .new_revisions[0];
    commit(
        &mut s,
        Channel::Agent,
        &["old two"],
        supersede("old", "old two", old1),
    );
    let hex: String = TEAM.iter().map(|b| format!("{b:02x}")).collect();
    let lane = format!("team:{hex}/main");
    let ops = vec![create(
        &mirror_key(),
        "x one",
        Origin::Replica { team: TEAM },
        &lane,
    )];
    let x1 = commit(&mut s, Channel::Replica, &["x one"], ops)
        .allocations
        .new_revisions[0];
    commit(
        &mut s,
        Channel::Replica,
        &["x two"],
        supersede(&mirror_key(), "x two", x1),
    );
    for n in 0..105 {
        let body = format!("filler {n}");
        let ops = vec![create(&format!("f/{n}"), &body, Origin::Local, "repo")];
        commit(&mut s, Channel::Agent, &[&body], ops);
    }
    x1
}

#[test]
fn ctx_gc_dry_run_apply_compact_and_reports() {
    let dir = std::env::temp_dir().join(format!("ctx-p11-gc-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.to_str().unwrap();
    let x1 = populate(&dir);
    let d = start(&dir);

    let (ok, out) = ctx(&["gc", path, "--keep-epochs", "100"]);
    assert!(ok, "{out}");
    assert!(out.contains("2 payload(s) collectable"), "{out}");
    assert!(out.contains("set by keep_epochs 100"), "{out}");
    assert!(out.contains("run again with --apply"), "{out}");
    let (ok, out) = ctx(&["gc", path, "--apply", "--keep-epochs", "99"]);
    assert!(
        !ok && out.contains("keep_epochs must be at least 100"),
        "{out}"
    );
    let (ok, out) = ctx(&["gc", path, "--apply", "--keep-epochs", "100"]);
    assert!(ok, "{out}");
    assert!(out.contains("collected 2 payload(s)"), "{out}");
    assert!(out.contains("projections purged"), "{out}");
    let (ok, out) = ctx(&["gc", path, "--compact"]);
    assert!(!ok && out.contains("ctxd holds the store lock"), "{out}");
    d.stop();
    drop(d);

    // Catch-up whose replica lost the bytes: the Replica channel may make
    // the collected revision the mirror's head again (ADR 0019 §5.4).
    let mut s = open(&dir);
    let x2 = {
        let v = s.kernel().unwrap().branch(MAIN).unwrap();
        v.head(v.logical_by_key(&mirror_key()).unwrap()).unwrap()
    };
    let back = vec![Op::SetHead {
        logical: LogicalRef::Key(mirror_key()),
        expected: x2.into(),
        new: x1.into(),
    }];
    commit(&mut s, Channel::Replica, &[], back);
    drop(s);

    let (ok, out) = ctx(&["gc", path, "--compact"]);
    assert!(ok, "{out}");
    assert!(out.contains("2 record(s) dropped"), "{out}");
    let (ok, out) = ctx(&["doctor", path]);
    assert!(ok, "{out}");
    assert!(
        out.contains("payloads: 2 unavailable (Collected 2); 1 problem(s)"),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "problem: head {}@{} on main reads Collected",
            mirror_key(),
            x1.0
        )),
        "{out}"
    );
    let d = start(&dir);
    let (ok, out) = ctx(&["team", "status", path]);
    assert!(ok, "{out}");
    assert!(
        out.contains(&format!(
            "problem: mirror head {}@{} on main reads Collected",
            mirror_key(),
            x1.0
        )),
        "{out}"
    );
    d.stop();
    drop(d);

    // Truncation below the GC cutoff needs --force.
    let (ok, out) = ctx(&["doctor", path, "--truncate-before", "2", "--yes"]);
    assert!(!ok, "{out}");
    assert!(out.contains("BELOW THE GC CUTOFF"), "{out}");
    assert!(out.contains("re-run with --force"), "{out}");
    let (ok, out) = ctx(&["doctor", path, "--truncate-before", "2", "--yes", "--force"]);
    assert!(ok && out.contains("truncated"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}
