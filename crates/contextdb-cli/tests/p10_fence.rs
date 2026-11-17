//! T113 (ADR 0018 §8.3, R5-M4): the announce fence with real hooks and a
//! real `ctxd` whose `lag_check` is disabled, so it cannot help. Every read
//! is issued at once.
//!
//! - a fetch with hooks disabled reads CERTIFIED until a sync (declared,
//!   §2.3);
//! - a durable announce whose `committed` notify is lost: lag at once;
//! - `ctxd` down during the fetch, then started: not a stale CERTIFIED
//!   read before the announce resolves;
//! - a `ctxd` that accepts but does not acknowledge: the fetch fails.

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

fn git(dir: &Path, args: &[&str]) -> (bool, String) {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
    )
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let (s, out) = git(dir, args);
    assert!(s, "git {args:?}: {out}");
    out
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

fn no_lag_check() -> Options {
    Options {
        team_lag_check: None,
        anchor_live_timeout: Some(std::time::Duration::from_secs(2)),
        ..Options::default()
    }
}

struct World {
    tmp: PathBuf,
    a: PathBuf,
    b: PathBuf,
    data_a: PathBuf,
    data_b: PathBuf,
    team: String,
    da: Daemon,
    sid_a: SessionId,
}

impl World {
    fn new() -> Option<(Self, String)> {
        if Command::new("ssh-keygen").arg("-?").output().is_err() {
            return None;
        }
        let tmp = std::env::temp_dir().join(format!("p10-fence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let remote = tmp.join("remote.git");
        ok(&tmp, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        let url = format!("file://{}", remote.display());
        let (a, b) = (tmp.join("a"), tmp.join("b"));
        ok(&tmp, &["clone", "-q", &url, a.to_str().unwrap()]);
        ok(&tmp, &["clone", "-q", &url, b.to_str().unwrap()]);
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
        let team = std::fs::read_dir(data_a.join("teams"))
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .into_owned();
        let sid_a = session(&da);
        let w = Self {
            tmp,
            a,
            b,
            data_a,
            data_b,
            team,
            da,
            sid_a,
        };
        Some((w, format!("{genesis} {}", key.display())))
    }
    /// A team record published from A.
    fn publish(&self, name: &str) {
        let body = name.as_bytes().to_vec();
        let r = self
            .da
            .service
            .handle(
                AGENT,
                Request::Commit {
                    binding: Binding::root(self.sid_a),
                    ops: vec![Op::CreateLogical {
                        origin: Origin::Local,
                        key: self.key(name),
                        kind: RecordKind::Convention,
                        scope_lane: format!("team:{}/main", self.team),
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
        let (code, out) = ctx(&["team", "publish", self.data_a.to_str().unwrap()]);
        assert_eq!(code, 0, "{out}");
    }
    fn key(&self, name: &str) -> String {
        format!("team/{}/conv/{name}", self.team)
    }
    fn tracking(&self) -> String {
        format!("refs/contextdb/remotes/origin/team/{}/g0", self.team)
    }
}

/// `(mirrored?, summary of the record)` on B, read at once.
fn read(db: &Daemon, sid: SessionId, key: &str) -> Option<Summary> {
    let l = db.service.actor.snapshot().view(MAIN).logical_by_key(key)?;
    match db
        .service
        .handle(
            AGENT,
            Request::Read {
                binding: Binding::root(sid),
                target: Target::Logical(l),
                mode: Mode::Advisory,
                level: None,
            },
        )
        .response
    {
        Response::Read { item, .. } => Some(item.summary),
        other => panic!("{other:?}"),
    }
}

fn sync(data: &Path) {
    let (code, out) = ctx(&["team", "sync", data.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn t113_the_announce_fence_with_real_hooks_and_no_lag_check() {
    let Some((w, join)) = World::new() else {
        return;
    };
    let (genesis, key) = join.split_once(' ').unwrap();
    w.publish("one");
    let db = Daemon::start(&w.data_b, &no_lag_check()).unwrap();
    let (code, out) = ctx(&[
        "team",
        "join",
        w.data_b.to_str().unwrap(),
        "--repo",
        w.b.to_str().unwrap(),
        "--genesis",
        genesis,
        "--key",
        key,
    ]);
    assert_eq!(code, 0, "{out}");
    let sid = session(&db);
    let lag = Some(Summary::Pending(PendingReason::ReplicationLag));
    assert_eq!(read(&db, sid, &w.key("one")), Some(Summary::Certified));

    // 1. Hooks disabled: no announce, no notify. Reads stay CERTIFIED on
    //    the old state until a sync (declared: `lag_check` is the only
    //    cover, and it is off here).
    w.publish("two");
    ok(
        &w.b,
        &["-c", "core.hooksPath=/dev/null", "fetch", "-q", "origin"],
    );
    assert_eq!(read(&db, sid, &w.key("one")), Some(Summary::Certified));
    assert_eq!(read(&db, sid, &w.key("two")), None);
    sync(&w.data_b);
    assert_eq!(read(&db, sid, &w.key("two")), Some(Summary::Certified));

    // 2. The announce is durable and acknowledged, the `committed` notify
    //    is lost: lag at once, until an import resolves it.
    w.publish("three");
    let tip = ok(
        &w.a,
        &[
            "ls-remote",
            "origin",
            &format!("refs/contextdb/team/{}/g0", w.team),
        ],
    );
    let oid = tip.split_whitespace().next().unwrap().to_owned();
    let team_id = contextdb_team::commit::parse_id128(&w.team).unwrap();
    let r = db
        .service
        .handle(
            Identity {
                channel: Channel::Admin,
                principal: PrincipalId(0),
            },
            Request::TeamAnnounce {
                team: team_id,
                refname: w.tracking(),
                oid: oid.clone(),
                pid: std::process::id(),
            },
        )
        .response;
    assert!(matches!(r, Response::TeamLines { .. }), "{r:?}");
    ok(
        &w.b,
        &["-c", "core.hooksPath=/dev/null", "fetch", "-q", "origin"],
    );
    assert_eq!(read(&db, sid, &w.key("one")), lag);
    sync(&w.data_b);
    assert_eq!(read(&db, sid, &w.key("one")), Some(Summary::Certified));
    assert_eq!(read(&db, sid, &w.key("three")), Some(Summary::Certified));

    // 3. ctxd down during the fetch (the hook leaves the durable announce
    //    alone), then started: never a stale CERTIFIED read.
    drop(db);
    w.publish("four");
    ok(&w.b, &["fetch", "-q", "origin"]);
    let announced = std::fs::read_to_string(w.data_b.join("teams").join(&w.team).join("announce"))
        .unwrap_or_default();
    assert!(
        announced.contains(&w.tracking()),
        "durable announce: {announced:?}"
    );
    let db = Daemon::start(&w.data_b, &no_lag_check()).unwrap();
    let sid = session(&db);
    match read(&db, sid, &w.key("four")) {
        // Imported at start; the announce of a git process now gone
        // resolves at the next import, and lags until then.
        Some(s) => assert!(s == Summary::Certified || Some(s.clone()) == lag, "{s:?}"),
        None => assert_eq!(read(&db, sid, &w.key("one")), lag),
    }
    sync(&w.data_b);
    assert_eq!(read(&db, sid, &w.key("four")), Some(Summary::Certified));

    // 4. A ctxd that accepts the announce but never acknowledges: the
    //    fetch fails, the ref does not move.
    drop(db);
    let lease = w.data_b.join("ctxd.lease");
    let sock = w.tmp.join("mute.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let mute = std::thread::spawn(move || {
        let held: Vec<_> = listener.incoming().take(1).flatten().collect();
        std::thread::sleep(std::time::Duration::from_secs(3));
        drop(held);
    });
    contextdb_proto::lease::write_lease(
        &lease,
        &contextdb_proto::lease::Lease {
            pid: std::process::id(),
            start_time: contextdb_proto::lease::process_start_time(std::process::id()).unwrap(),
            protocol: PROTOCOL_VERSION,
            sockets: vec![sock.clone().with_file_name("admin.sock")],
        },
    )
    .unwrap();
    std::fs::rename(&sock, sock.with_file_name("admin.sock")).unwrap();
    let before = ok(&w.b, &["rev-parse", &w.tracking()]);
    w.publish("five");
    let (fetched, out) = git(&w.b, &["fetch", "-q", "origin"]);
    assert!(
        !fetched,
        "the fetch must fail without an acknowledgement: {out}"
    );
    assert_eq!(ok(&w.b, &["rev-parse", &w.tracking()]), before);
    mute.join().unwrap();
    let _ = std::fs::remove_file(&lease);
    drop(w.da);
    let _ = std::fs::remove_dir_all(&w.tmp);
}
