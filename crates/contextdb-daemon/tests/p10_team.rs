//! P10 milestone 5 (ADR 0018 §6–8) through the daemon, with a real git
//! remote: a joined team is imported and every open branch caught up on
//! the single writer; mirrors read certified while the branch is at the
//! fetched tip and `PENDING(ReplicationLag)` when it is not; a team write is
//! answered `Proposed` and queued, a mixed one `MixedTeamBatch`, a direct
//! write to a mirror by the agent the kernel's refusal; `ctx team sync`
//! brings a new root entry to every branch.

use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
    team::{self, TeamConfig},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::git::Repo;
use contextdb_team::ops::{Hash, TeamBatch, TeamOp};
use contextdb_team::sshsig;
use contextdb_team::store::TeamStore;
use std::path::{Path, PathBuf};
use std::process::Command;

const TEAM: [u8; 16] = [0x5e; 16];
const ALICE: [u8; 32] = [1; 32];
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn blob(bytes: &[u8]) -> (Hash, (String, Vec<u8>)) {
    let h = *blake3::hash(bytes).as_bytes();
    (h, (format!("blobs/{}", hex(&h)), bytes.to_vec()))
}

struct World {
    tmp: PathBuf,
    publisher: PathBuf,
    edge: PathBuf,
    repo: Repo,
    seq: u64,
    parent: Option<String>,
}

impl World {
    fn new(name: &str) -> Self {
        let tmp = std::env::temp_dir().join(format!("ctx-p10-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let remote = tmp.join("remote.git");
        git(&tmp, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        let publisher = tmp.join("publisher");
        git(&tmp, &["init", "-q", publisher.to_str().unwrap()]);
        let url = format!("file://{}", remote.display());
        git(&publisher, &["remote", "add", "origin", &url]);
        let repo = Repo::open(&publisher).unwrap();
        let mut w = Self {
            edge: tmp.join("edge"),
            tmp,
            publisher,
            repo,
            seq: 0,
            parent: None,
        };
        let doc = TeamDoc::from_toml(&format!(
            "version = 1\naccept_version = 1\nid = \"{}\"\nadmins = [\"alice\"]\n\
             [members.alice]\nkeys = [\"{}\"]\n\
             [lanes.main]\nwriters = [\"*members\"]\ncurators = [\"*members\"]\n",
            hex(&TEAM),
            sshsig::public_key(&ALICE).to_line(),
        ))
        .unwrap();
        w.entry(vec![TeamOp::SetTeamAccess { doc: Box::new(doc) }], vec![]);
        w.push();
        git(&w.tmp, &["clone", "-q", &url, w.edge.to_str().unwrap()]);
        git(
            &w.edge,
            &[
                "config",
                "--add",
                "remote.origin.fetch",
                "+refs/contextdb/team/*:refs/contextdb/remotes/origin/team/*",
            ],
        );
        w.fetch();
        w
    }
    fn refname(&self) -> String {
        format!("refs/contextdb/team/{}/g0", hex(&TEAM))
    }
    fn entry(&mut self, ops: Vec<TeamOp>, blobs: Vec<(String, Vec<u8>)>) -> String {
        let batch = TeamBatch {
            proposals: vec![ops],
        }
        .encode();
        let tr: Vec<(&str, String)> = vec![
            ("Ctx-Team", hex(&TEAM)),
            ("Ctx-Team-Seq", self.seq.to_string()),
            ("Ctx-Author", "alice".into()),
            ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
            ("Ctx-Accept-Version", "1".into()),
            ("Ctx-Proposal", hex(&[self.seq as u8 + 1; 16])),
        ];
        let mut tree = vec![("batch".to_owned(), batch)];
        tree.extend(blobs);
        let tree_oid = self.repo.write_tree(&tree).unwrap();
        let parents: Vec<String> = self.parent.iter().cloned().collect();
        let unsigned = commit::build(&tree_oid, &parents, "ctx team: entry", &tr);
        let raw = commit::with_signature(
            &unsigned,
            &sshsig::sign(&ALICE, "git", &unsigned),
            ObjectFormat::Sha1,
        );
        let oid = self.repo.write_commit(&raw).unwrap();
        self.repo
            .update_ref(&self.refname(), &oid, self.parent.as_deref())
            .unwrap();
        self.parent = Some(oid.clone());
        self.seq += 1;
        oid
    }
    fn push(&self) {
        let r = self.refname();
        git(
            &self.publisher,
            &["push", "-q", "origin", &format!("{r}:{r}")],
        );
    }
    fn fetch(&self) {
        git(&self.edge, &["fetch", "-q", "origin"]);
    }
    fn genesis(&self) -> String {
        git(
            &self.publisher,
            &["rev-list", "--max-parents=0", &self.refname()],
        )
        .trim()
        .to_owned()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

fn new_op(path: &str, h: Hash, base: bool) -> TeamOp {
    TeamOp::New {
        path: path.into(),
        lane: "main".into(),
        kind: RecordKind::Decision,
        content: h,
        adopt_as_base: base,
    }
}

fn key(path: &str) -> String {
    format!("team/{}/{path}", hex(&TEAM))
}

/// The daemon over a SimDisk store, with the team joined at genesis.
fn daemon(w: &World) -> Service<SimDisk> {
    let data = w.tmp.join("data");
    let dir = data.join(team::TEAMS_DIR).join(hex(&TEAM));
    if !dir.exists() {
        TeamStore::create(&dir, TEAM, ObjectFormat::Sha1, "origin", &w.genesis()).unwrap();
        TeamConfig::new(w.edge.clone(), "alice".into(), "k".into())
            .save(&dir)
            .unwrap();
    }
    let (teams, errors) = team::open_all(&data);
    assert!(errors.is_empty(), "{errors:?}");
    let disk = SimDisk::new();
    let db = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), db, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk, db, false, 1 << 20).unwrap();
    Service::new(actor::spawn_with_teams(store, teams).unwrap(), log, events).unwrap()
}

fn sync(s: &Service<SimDisk>) -> Vec<String> {
    match s.handle(ADMIN, Request::TeamSync { team: None }).response {
        Response::TeamLines { lines } => lines,
        other => panic!("{other:?}"),
    }
}

fn session(s: &Service<SimDisk>) -> SessionId {
    match s
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

fn commit(s: &Service<SimDisk>, sid: SessionId, ops: Vec<Op>, blobs: Vec<Vec<u8>>) -> Response {
    s.handle(
        AGENT,
        Request::Commit {
            binding: Binding::root(sid),
            ops,
            blobs,
            expect_cert_valid: false,
            client_txn_id: None,
        },
    )
    .response
}

fn summary(s: &Service<SimDisk>, sid: SessionId, path: &str) -> Summary {
    let l = s
        .actor
        .snapshot()
        .view(MAIN)
        .logical_by_key(&key(path))
        .unwrap();
    match s
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
        Response::Read { item, .. } => item.summary,
        other => panic!("{other:?}"),
    }
}

fn status(s: &Service<SimDisk>) -> Vec<String> {
    match s.handle(ADMIN, Request::TeamStatus { team: None }).response {
        Response::TeamLines { lines } => lines,
        other => panic!("{other:?}"),
    }
}

/// The pre-push hook's publish, by hand: prepare (a confirm listing, then
/// `--yes`), sign as the bound member, begin, push, import.
fn publish(s: &Service<SimDisk>, w: &World, proposal: [u8; 16]) {
    let prepare = |yes: Option<[u8; 32]>, confirm: bool| match s
        .handle(
            ADMIN,
            Request::TeamPublishPrepare {
                team: TEAM,
                yes,
                confirm,
            },
        )
        .response
    {
        Response::TeamPrepared {
            entry,
            listed,
            conflicted,
        } => (entry, listed, conflicted),
        other => panic!("{other:?}"),
    };
    let (entry, listed, conflicted) = prepare(None, true);
    assert!(entry.is_none() && conflicted.is_empty());
    let (token, who) = listed.expect("a confirm listing");
    assert_eq!(who, vec![(proposal, AGENT.principal)]);
    let (entry, _, conflicted) = prepare(Some(token), false);
    assert!(conflicted.is_empty(), "{conflicted:?}");
    let e = entry.expect("an entry to sign");
    assert_eq!(e.proposals, vec![proposal]);
    let raw = commit::with_signature(
        &e.payload,
        &sshsig::sign(&ALICE, "git", &e.payload),
        ObjectFormat::Sha1,
    );
    let edge = Repo::open(&w.edge).unwrap();
    let oid = edge.write_commit(&raw).unwrap();
    let r = s
        .handle(
            ADMIN,
            Request::TeamPublishBegin {
                team: TEAM,
                commit: oid.clone(),
            },
        )
        .response;
    assert!(matches!(r, Response::TeamLines { .. }), "{r:?}");
    assert!(
        status(s).iter().any(|l| l.contains("Published")),
        "{:?}",
        status(s)
    );
    git(
        &w.edge,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "push",
            "-q",
            "origin",
            &format!("{oid}:{}", e.refname),
        ],
    );
    w.fetch();
    let lines = sync(s);
    assert!(lines[0].contains("Imported(1)"), "{lines:?}");
    assert!(
        status(s).iter().any(|l| l.contains("Accepted")),
        "{:?}",
        status(s)
    );
}

fn code(r: &Response) -> &str {
    match r {
        Response::Error { code, .. } => code,
        other => panic!("not an error: {other:?}"),
    }
}

#[test]
fn p10_daemon_imports_routes_and_lags() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("daemon");
    let (a1, ba) = blob(b"a1");
    let (b1, bb) = blob(b"b1");
    w.entry(
        vec![new_op("k/a", a1, true), new_op("k/b", b1, false)],
        vec![ba, bb],
    );
    w.push();
    w.fetch();
    let s = daemon(&w);
    let lines = sync(&s);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("Imported(2)"), "{lines:?}");
    // The replica's justification, in team terms (§5.4).
    match s
        .handle(
            ADMIN,
            Request::TeamWhy {
                team: TEAM,
                path: "k/a".into(),
            },
        )
        .response
    {
        Response::TeamLines { lines } => {
            assert!(lines[0].starts_with("team record k/a@"), "{lines:?}");
            assert!(
                lines[0].contains("Certified") && lines[0].contains("base"),
                "{lines:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    let r = s
        .handle(
            ADMIN,
            Request::TeamWhy {
                team: TEAM,
                path: "k/none".into(),
            },
        )
        .response;
    assert_eq!(code(&r), "UnknownKey");
    // Mirrors on main, at the fetched tip: certified.
    let sid = session(&s);
    assert_eq!(summary(&s, sid, "k/a"), Summary::Certified);
    // A team write from an agent: queued, nothing committed locally.
    let (c1, _) = blob(b"c1");
    let epoch = s.actor.snapshot().epoch;
    let r = commit(
        &s,
        sid,
        vec![Op::CreateLogical {
            origin: Origin::Local,
            key: key("k/c"),
            kind: RecordKind::Decision,
            scope_lane: format!("team:{}/main", hex(&TEAM)),
            content: c1,
            adopt_as_base: false,
        }],
        vec![b"c1".to_vec()],
    );
    let proposal = match &r {
        Response::Proposed { team, id } => {
            assert_eq!(*team, TEAM);
            *id
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(
        s.actor.snapshot().epoch,
        epoch,
        "a proposal commits nothing"
    );
    publish(&s, &w, proposal);
    let snap = s.actor.snapshot();
    assert!(
        snap.view(MAIN).logical_by_key(&key("k/c")).is_some(),
        "the accepted proposal is a mirror now"
    );
    drop(snap);
    // The publisher continues from the edge's entry.
    let tracking = format!("refs/contextdb/remotes/origin/team/{}/g0", hex(&TEAM));
    w.parent = Some(git(&w.edge, &["rev-parse", &tracking]).trim().to_owned());
    w.seq += 1;
    let r = w.refname();
    git(
        &w.publisher,
        &["fetch", "-q", "origin", &format!("+{r}:{r}")],
    );
    // Team and local targets together.
    let r = commit(
        &s,
        sid,
        vec![
            Op::CreateLogical {
                origin: Origin::Local,
                key: "dec:local".into(),
                kind: RecordKind::Decision,
                scope_lane: "repo".into(),
                content: *blake3::hash(b"l").as_bytes(),
                adopt_as_base: false,
            },
            Op::SetMetadata {
                logical: LogicalRef::Key(key("k/a")),
                tags: ["x".to_owned()].into(),
                dirs: Default::default(),
            },
        ],
        vec![b"l".to_vec()],
    );
    assert_eq!(code(&r), "MixedTeamBatch", "{r:?}");
    // Q11: a proposal over an entry's limits is refused at enqueue.
    let big = vec![7u8; (1 << 20) + 1];
    let r = commit(
        &s,
        sid,
        vec![Op::CreateLogical {
            origin: Origin::Local,
            key: key("k/big"),
            kind: RecordKind::Decision,
            scope_lane: format!("team:{}/main", hex(&TEAM)),
            content: *blake3::hash(&big).as_bytes(),
            adopt_as_base: false,
        }],
        vec![big],
    );
    assert_eq!(code(&r), "ProposalTooLarge", "{r:?}");
    // A queued proposal on a mirror is marked on reads of it (§6.3).
    let r = commit(
        &s,
        sid,
        vec![Op::SetMetadata {
            logical: LogicalRef::Key(key("k/a")),
            tags: ["reviewed".to_owned()].into(),
            dirs: Default::default(),
        }],
        vec![],
    );
    let tagged = match r {
        Response::Proposed { id, .. } => id,
        other => panic!("{other:?}"),
    };
    let l = s
        .actor
        .snapshot()
        .view(MAIN)
        .logical_by_key(&key("k/a"))
        .unwrap();
    match s
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
        Response::Read { item, .. } => assert_eq!(item.proposed, vec![(tagged, AGENT.principal)]),
        other => panic!("{other:?}"),
    }
    // A new root entry: fetched, not yet imported, reads are behind the
    // daemon's view of the ref only after it re-reads it (lag_check or
    // the hooks' announce); sync imports and catches up.
    let (a2, ba2) = blob(b"a2");
    w.entry(
        vec![
            TeamOp::Revise {
                path: "k/a".into(),
                content: a2,
                adopt_as_base: true,
            },
            TeamOp::SetHead {
                path: "k/a".into(),
                expected: a1,
                new: a2,
            },
        ],
        vec![ba2],
    );
    w.push();
    w.fetch();
    let lines = sync(&s);
    assert!(lines[0].contains("Imported(1)"), "{lines:?}");
    let snap = s.actor.snapshot();
    let v = snap.view(MAIN);
    let l = v.logical_by_key(&key("k/a")).unwrap();
    assert_eq!(v.revision_content(v.head(l).unwrap()).unwrap(), a2);
    drop(snap);
    assert_eq!(summary(&s, sid, "k/a"), Summary::Certified);
    // `ctx team rebuild`: replayed from the genesis, the same state; the
    // branch's position is still in the history (no gap).
    let digest = s.actor.snapshot().view(MAIN).digest();
    match s
        .handle(ADMIN, Request::TeamRebuild { team: TEAM })
        .response
    {
        Response::TeamLines { lines } => assert!(lines[0].contains("rebuilt"), "{lines:?}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.actor.snapshot().view(MAIN).digest(), digest);
    assert_eq!(summary(&s, sid, "k/a"), Summary::Certified);
    // Import stopped: every read over the team's mirrors lags.
    s.actor
        .team_live
        .lock()
        .unwrap()
        .get_mut(&TEAM)
        .unwrap()
        .pending
        .insert(("r".into(), "o".into()), 1);
    assert_eq!(
        summary(&s, sid, "k/a"),
        Summary::Pending(PendingReason::ReplicationLag)
    );
}

#[test]
fn p10_announce_fence_lags_reads_until_resolved() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("announce");
    let (a1, ba) = blob(b"a1");
    w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
    w.push();
    w.fetch();
    let s = daemon(&w);
    sync(&s);
    let sid = session(&s);
    assert_eq!(summary(&s, sid, "k/a"), Summary::Certified);
    let refname = format!("refs/contextdb/remotes/origin/team/{}/g0", hex(&TEAM));
    let announce = |oid: &str, pid: u32| {
        let r = s
            .handle(
                ADMIN,
                Request::TeamAnnounce {
                    team: TEAM,
                    refname: refname.clone(),
                    oid: oid.into(),
                    pid,
                },
            )
            .response;
        assert!(matches!(r, Response::TeamLines { .. }), "{r:?}");
    };
    let lag = Summary::Pending(PendingReason::ReplicationLag);
    // A live git process announces an OID the ref does not hold yet.
    let ahead = "1".repeat(40);
    announce(&ahead, std::process::id());
    assert_eq!(summary(&s, sid, "k/a"), lag);
    sync(&s);
    assert_eq!(
        summary(&s, sid, "k/a"),
        lag,
        "still pending: its process lives"
    );
    // aborted: withdrawn.
    s.handle(
        ADMIN,
        Request::TeamWithdraw {
            team: TEAM,
            refname: refname.clone(),
            oid: ahead.clone(),
        },
    );
    assert_eq!(summary(&s, sid, "k/a"), Summary::Certified);
    // A process that died before the ref moved: dropped at the next import.
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    announce(&ahead, dead);
    assert_eq!(summary(&s, sid, "k/a"), lag);
    sync(&s);
    assert_eq!(summary(&s, sid, "k/a"), Summary::Certified);
    // The ref moved to the announced OID: resolved by the import itself.
    let (a2, ba2) = blob(b"a2");
    let next = w.entry(
        vec![
            TeamOp::Revise {
                path: "k/a".into(),
                content: a2,
                adopt_as_base: true,
            },
            TeamOp::SetHead {
                path: "k/a".into(),
                expected: a1,
                new: a2,
            },
        ],
        vec![ba2],
    );
    w.push();
    announce(&next, std::process::id());
    w.fetch();
    assert_eq!(summary(&s, sid, "k/a"), lag);
    sync(&s);
    assert_eq!(summary(&s, sid, "k/a"), Summary::Certified);
}

#[test]
fn p10_announce_file_keeps_concurrent_appends() {
    let dir = std::env::temp_dir().join(format!("ctx-p10-announce-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let a = |oid: &str, pid| team::Announce {
        refname: "refs/x".into(),
        oid: oid.into(),
        pid,
    };
    team::append_announce(&dir, &a("o1", 1)).unwrap();
    team::append_announce(&dir, &a("o2", 2)).unwrap();
    assert_eq!(team::read_announces(&dir), vec![a("o1", 1), a("o2", 2)]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn p10_leave_retracts_every_mirror_and_resumes_after_a_crash() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("leave");
    let (a1, ba) = blob(b"a1");
    w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
    w.push();
    w.fetch();
    let s = daemon(&w);
    sync(&s);
    let sid = session(&s);
    assert_eq!(summary(&s, sid, "k/a"), Summary::Certified);
    // A local record supported by the mirror (Cur).
    let ma = {
        let snap = s.actor.snapshot();
        let v = snap.view(MAIN);
        v.head(v.logical_by_key(&key("k/a")).unwrap()).unwrap()
    };
    let r = commit(
        &s,
        sid,
        vec![
            Op::CreateLogical {
                origin: Origin::Local,
                key: "dec:on-a".into(),
                kind: RecordKind::Decision,
                scope_lane: "repo".into(),
                content: *blake3::hash(b"on a").as_bytes(),
                adopt_as_base: false,
            },
            Op::AddRule {
                conclusion: RevRef::New(0),
                incidences: vec![(ma.into(), EdgeKind::Cur)],
                enabled: true,
            },
        ],
        vec![b"on a".to_vec()],
    );
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    let local = |s: &Service<SimDisk>| {
        let snap = s.actor.snapshot();
        let v = snap.view(MAIN);
        let l = v.logical_by_key("dec:on-a").unwrap();
        v.summary_at(v.head(l).unwrap(), &Default::default())
            .unwrap()
    };
    assert_eq!(
        local(&s),
        Summary::Pending(PendingReason::ReplicationLag),
        "no live position given here"
    );
    let r = s.handle(ADMIN, Request::TeamLeave { team: TEAM }).response;
    assert!(matches!(r, Response::TeamLines { .. }), "{r:?}");
    // Every base retracted, the watermark `left`: the mirror and its
    // dependents are unsupported, and nothing lags.
    assert!(matches!(summary(&s, sid, "k/a"), Summary::Stale(_)));
    assert!(matches!(local(&s), Summary::Stale(_)));
    let snap = s.actor.snapshot();
    assert_eq!(
        snap.view(MAIN).watermark(&team_scope(&TEAM)),
        Some(TEAM_LEFT)
    );
    drop(snap);
    let dir = w.tmp.join("data").join(team::TEAMS_DIR).join(hex(&TEAM));
    assert!(!dir.exists());
    // Leaving twice: not joined.
    let r = s.handle(ADMIN, Request::TeamLeave { team: TEAM }).response;
    assert_eq!(code(&r), "TeamLeaveFailed");
}

#[test]
fn p10_a_recorded_leave_is_finished_at_the_next_import_never_caught_up() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("leave-crash");
    let (a1, ba) = blob(b"a1");
    w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
    w.push();
    w.fetch();
    let s = daemon(&w);
    sync(&s);
    // A crash right after `leaving` was recorded: the next start opens the
    // team leaving, and its import finishes the leave instead of catching
    // up (a new root entry is never mirrored).
    let dir = w.tmp.join("data").join(team::TEAMS_DIR).join(hex(&TEAM));
    team::mark_leaving(&dir).unwrap();
    let (teams, _) = team::open_all(&w.tmp.join("data"));
    assert!(teams[&TEAM].leaving);
    drop(teams);
    let (b1, bb) = blob(b"b1");
    w.entry(vec![new_op("k/b", b1, true)], vec![bb]);
    w.push();
    w.fetch();
    // The running daemon still holds the team open (not leaving): reopen
    // as a restart would.
    drop(s);
    let s = daemon(&w);
    let lines = sync(&s);
    assert!(lines[0].contains("left"), "{lines:?}");
    let snap = s.actor.snapshot();
    assert!(snap.view(MAIN).logical_by_key(&key("k/b")).is_none());
    assert!(!dir.exists());
}

#[test]
fn p10_a_merge_catches_branches_up_first() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("merge");
    let (a1, ba) = blob(b"a1");
    w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
    w.push();
    w.fetch();
    let s = daemon(&w);
    sync(&s);
    let r = s
        .handle(
            ADMIN,
            Request::BranchCreate {
                name: "feat".into(),
                from: None,
                client_txn_id: None,
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    // A new root entry, fetched but not imported (no notify, no sync).
    let (a2, ba2) = blob(b"a2");
    w.entry(
        vec![
            TeamOp::Revise {
                path: "k/a".into(),
                content: a2,
                adopt_as_base: true,
            },
            TeamOp::SetHead {
                path: "k/a".into(),
                expected: a1,
                new: a2,
            },
        ],
        vec![ba2],
    );
    w.push();
    w.fetch();
    // Q1: the merge imports and catches up before it runs.
    let r = s
        .handle(
            ADMIN,
            Request::Merge {
                binding: None,
                source: "feat".into(),
                into: Some("main".into()),
                require_certified: false,
                certify: Vec::new(),
                client_txn_id: None,
            },
        )
        .response;
    assert!(matches!(r, Response::Merged { .. }), "{r:?}");
    let snap = s.actor.snapshot();
    for b in [MAIN, snap.kernel.branch_by_name("feat").unwrap()] {
        let v = snap.view(b);
        let l = v.logical_by_key(&key("k/a")).unwrap();
        assert_eq!(v.revision_content(v.head(l).unwrap()).unwrap(), a2, "{b:?}");
    }
}

/// Prepares (`--yes token` when given) and returns the entry to sign.
fn prepare_entry(s: &Service<SimDisk>, yes: Option<[u8; 32]>, confirm: bool) -> Response {
    s.handle(
        ADMIN,
        Request::TeamPublishPrepare {
            team: TEAM,
            yes,
            confirm,
        },
    )
    .response
}

fn sign_entry(w: &World, e: &TeamEntry) -> String {
    let raw = commit::with_signature(
        &e.payload,
        &sshsig::sign(&ALICE, "git", &e.payload),
        ObjectFormat::Sha1,
    );
    Repo::open(&w.edge).unwrap().write_commit(&raw).unwrap()
}

fn begin(s: &Service<SimDisk>, oid: &str) -> Response {
    s.handle(
        ADMIN,
        Request::TeamPublishBegin {
            team: TEAM,
            commit: oid.to_owned(),
        },
    )
    .response
}

fn push_entry(w: &World, oid: &str, e: &TeamEntry) {
    git(
        &w.edge,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "push",
            "-q",
            "origin",
            &format!("{oid}:{}", e.refname),
        ],
    );
}

fn queue_states(s: &Service<SimDisk>) -> Vec<String> {
    status(s)
        .into_iter()
        .filter(|l| l.trim_start().starts_with("proposal"))
        .collect()
}

#[test]
fn t114_every_publish_crash_row_recovers() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("t114");
    let (a1, ba) = blob(b"a1");
    w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
    w.push();
    w.fetch();
    let s = daemon(&w);
    sync(&s);
    let sid = session(&s);
    let propose = |s: &Service<SimDisk>, sid: SessionId, path: &str| {
        let body = path.as_bytes().to_vec();
        match commit(
            s,
            sid,
            vec![Op::CreateLogical {
                origin: Origin::Local,
                key: key(path),
                kind: RecordKind::Decision,
                scope_lane: format!("team:{}/main", hex(&TEAM)),
                content: *blake3::hash(&body).as_bytes(),
                adopt_as_base: false,
            }],
            vec![body],
        ) {
            Response::Proposed { id, .. } => id,
            other => panic!("{other:?}"),
        }
    };
    let entry = |r: Response| match r {
        Response::TeamPrepared { entry: Some(e), .. } => e,
        other => panic!("{other:?}"),
    };
    // Row 1, before step 7: an unreferenced commit, proposals queued. The
    // next publish rebuilds (and checks the local layer) again.
    propose(&s, sid, "r1");
    let e = entry(prepare_entry(&s, None, false));
    let _orphan = sign_entry(&w, &e);
    assert!(queue_states(&s).iter().all(|l| l.contains("Queued")));
    // Row 2, between step 7 and the push: `published`, not on the remote.
    // A crash (the daemon restarts), then the next fetch finds it absent:
    // queued again.
    let e = entry(prepare_entry(&s, None, false));
    let oid = sign_entry(&w, &e);
    assert!(matches!(begin(&s, &oid), Response::TeamLines { .. }));
    assert!(queue_states(&s).iter().any(|l| l.contains("Published")));
    drop(s);
    let s = daemon(&w);
    sync(&s);
    assert!(
        queue_states(&s).iter().all(|l| l.contains("Queued")),
        "{:?}",
        queue_states(&s)
    );
    // Row 3, push outcome unknown: pushed, no reply seen; the next fetch
    // finds it on the chain and takes the outcome from the replica.
    let e = entry(prepare_entry(&s, None, false));
    let oid = sign_entry(&w, &e);
    assert!(matches!(begin(&s, &oid), Response::TeamLines { .. }));
    push_entry(&w, &oid, &e);
    drop(s);
    let s = daemon(&w);
    w.fetch();
    sync(&s);
    assert!(
        queue_states(&s).iter().all(|l| l.contains("Accepted")),
        "{:?}",
        queue_states(&s)
    );
    // Row 4, a confirm listing shown and not confirmed: the `listed` frame
    // survives a restart; `--yes <token>` publishes from it, not a proposal
    // queued after it; an unknown token is refused.
    let sid = session(&s);
    let listed = propose(&s, sid, "r4");
    let token = match prepare_entry(&s, None, true) {
        Response::TeamPrepared {
            listed: Some((t, who)),
            ..
        } => {
            assert_eq!(
                who.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                vec![listed]
            );
            t
        }
        other => panic!("{other:?}"),
    };
    drop(s);
    let s = daemon(&w);
    let sid = session(&s);
    let later = propose(&s, sid, "r4-later");
    let r = prepare_entry(&s, Some([7; 32]), false);
    assert!(matches!(r, Response::Error { .. }), "{r:?}");
    let e = entry(prepare_entry(&s, Some(token), false));
    assert_eq!(e.proposals, vec![listed]);
    assert!(!e.proposals.contains(&later));
}

#[test]
fn t115_root_changes_invalidate_local_dependents_precisely_on_every_branch() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("t115");
    let (a1, ba) = blob(b"a1");
    w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
    w.push();
    w.fetch();
    let s = daemon(&w);
    sync(&s);
    let r = s
        .handle(
            ADMIN,
            Request::BranchCreate {
                name: "feat".into(),
                from: None,
                client_txn_id: None,
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    let feat = s.actor.snapshot().kernel.branch_by_name("feat").unwrap();
    // On each branch, a Pin and a Cur dependent of the mirror's head.
    for (b, tag) in [(MAIN, "m"), (feat, "f")] {
        let ma = {
            let snap = s.actor.snapshot();
            let v = snap.view(b);
            v.head(v.logical_by_key(&key("k/a")).unwrap()).unwrap()
        };
        for (edge, name) in [(EdgeKind::Pin, "pin"), (EdgeKind::Cur, "cur")] {
            let body = format!("{tag}-{name}").into_bytes();
            let mut batch = Batch::new(vec![
                Op::CreateLogical {
                    origin: Origin::Local,
                    key: format!("dec:{tag}-{name}"),
                    kind: RecordKind::Decision,
                    scope_lane: "repo".into(),
                    content: *blake3::hash(&body).as_bytes(),
                    adopt_as_base: false,
                },
                Op::AddRule {
                    conclusion: RevRef::New(0),
                    incidences: vec![(ma.into(), edge)],
                    enabled: true,
                },
            ]);
            batch.branch = b;
            let r = s
                .handle(
                    ADMIN,
                    Request::Raw {
                        blobs: vec![body],
                        batch,
                    },
                )
                .response;
            assert!(matches!(r, Response::Committed { .. }), "{r:?}");
        }
    }
    // The summary of a local record on a branch, with that branch's team
    // position as the live one (no lag in these checks).
    let stat = |b: BranchId, k: &str| {
        let snap = s.actor.snapshot();
        let v = snap.view(b);
        let scope = team_scope(&TEAM);
        let live = [(scope.clone(), v.watermark(&scope).unwrap().to_vec())].into();
        let l = v.logical_by_key(k).unwrap();
        v.summary_at(v.head(l).unwrap(), &live).unwrap()
    };
    let all = |s_pin: fn(&Summary) -> bool, s_cur: fn(&Summary) -> bool, at: &str| {
        for (b, tag) in [(MAIN, "m"), (feat, "f")] {
            let (p, c) = (
                stat(b, &format!("dec:{tag}-pin")),
                stat(b, &format!("dec:{tag}-cur")),
            );
            assert!(s_pin(&p), "{at}: {tag} pin {p:?}");
            assert!(s_cur(&c), "{at}: {tag} cur {c:?}");
        }
    };
    let certified = |x: &Summary| *x == Summary::Certified;
    let stale = |x: &Summary| matches!(x, Summary::Stale(_));
    all(certified, certified, "before");
    let root = |w: &mut World, ops: Vec<TeamOp>, blobs: Vec<(String, Vec<u8>)>| {
        w.entry(ops, blobs);
        w.push();
        w.fetch();
        sync(&s);
    };
    // Q4: the root retracts the head's base: every dependent unsupported.
    root(
        &mut w,
        vec![TeamOp::SetBase {
            path: "k/a".into(),
            content: a1,
            expected: true,
            base: false,
        }],
        vec![],
    );
    all(stale, stale, "retracted");
    // Restored: both recover.
    root(
        &mut w,
        vec![TeamOp::SetBase {
            path: "k/a".into(),
            content: a1,
            expected: false,
            base: true,
        }],
        vec![],
    );
    all(certified, certified, "restored");
    // A root supersede: Pin STALE (its revision is no longer the head),
    // Cur unsupported until rebound.
    let (a2, ba2) = blob(b"a2");
    root(
        &mut w,
        vec![
            TeamOp::Revise {
                path: "k/a".into(),
                content: a2,
                adopt_as_base: true,
            },
            TeamOp::SetHead {
                path: "k/a".into(),
                expected: a1,
                new: a2,
            },
        ],
        vec![ba2],
    );
    all(stale, stale, "superseded");
    for (b, tag) in [(MAIN, "m"), (feat, "f")] {
        // A mirror's old head loses its base as the head moves (§5.1), so
        // the pinned premise has no L0: SEMANTICS' "no L0 -> Unsupported"
        // comes before the witness inspection that would name the
        // superseded Pin. §5.3 asks for STALE.
        let p = stat(b, &format!("dec:{tag}-pin"));
        assert_eq!(p, Summary::Stale(StaleReason::Unsupported), "{tag}");
    }
    // The head goes back: both recover.
    root(
        &mut w,
        vec![
            TeamOp::SetHead {
                path: "k/a".into(),
                expected: a2,
                new: a1,
            },
            TeamOp::SetBase {
                path: "k/a".into(),
                content: a1,
                expected: true,
                base: true,
            },
        ],
        vec![],
    );
    all(certified, certified, "head back");
}

#[test]
fn t117_team_shred_local_shred_and_q6_order() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("t117");
    let (a1, ba) = blob(b"a1");
    w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
    w.push();
    w.fetch();
    let s = daemon(&w);
    sync(&s);
    let shredded = |s: &Service<SimDisk>, h: &Hash| s.actor.snapshot().kernel.is_shredded(h);
    // A local record holding bytes the team will shred.
    let mine = b"also mine".to_vec();
    let mine_h = *blake3::hash(&mine).as_bytes();
    let r = s
        .handle(
            ADMIN,
            Request::Raw {
                blobs: vec![mine.clone()],
                batch: Batch::new(vec![Op::CreateLogical {
                    origin: Origin::Local,
                    key: "dec:mine".into(),
                    kind: RecordKind::Decision,
                    scope_lane: "repo".into(),
                    content: mine_h,
                    adopt_as_base: true,
                }]),
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    // Q6: a record created and shredded within one import; and a shred of
    // the bytes the local record holds.
    let (c1, bc) = blob(b"c1");
    w.entry(vec![new_op("k/c", c1, false)], vec![bc]);
    w.entry(vec![TeamOp::TeamShred { content: c1 }], vec![]);
    w.entry(
        vec![new_op("k/m", mine_h, false)],
        vec![(format!("blobs/{}", hex(&mine_h)), mine.clone())],
    );
    w.entry(vec![TeamOp::TeamShred { content: mine_h }], vec![]);
    w.push();
    w.fetch();
    let lines = sync(&s);
    assert!(!lines[0].contains("Stalled"), "{lines:?}");
    // The shred step ran before the mirror was created: no stall, the
    // mirror exists and its revision is shredded locally.
    assert!(shredded(&s, &c1));
    let snap = s.actor.snapshot();
    assert!(snap.view(MAIN).logical_by_key(&key("k/c")).is_some());
    // Bytes a local record holds are kept (m5).
    assert!(!shredded(&s, &mine_h));
    drop(snap);
    // A local shred of a mirror's bytes, then a root entry reusing them:
    // the mirror revision is created unavailable, never a stall.
    let r = s
        .handle(
            ADMIN,
            Request::Raw {
                blobs: Vec::new(),
                batch: Batch::new(vec![Op::ShredContent { content: a1 }]),
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    let (_, ba1) = blob(b"a1");
    w.entry(vec![new_op("k/reuse", a1, false)], vec![ba1]);
    w.push();
    w.fetch();
    let lines = sync(&s);
    assert!(!lines[0].contains("Stalled"), "{lines:?}");
    assert!(
        s.actor
            .snapshot()
            .view(MAIN)
            .logical_by_key(&key("k/reuse"))
            .is_some()
    );
}

#[test]
fn an_import_during_a_publish_never_loses_the_outcome() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("race");
    let (a1, ba) = blob(b"a1");
    w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
    w.push();
    w.fetch();
    let s = daemon(&w);
    sync(&s);
    let sid = session(&s);
    let dir = w.tmp.join("data").join(team::TEAMS_DIR).join(hex(&TEAM));
    let propose = |path: &str| {
        let body = path.as_bytes().to_vec();
        match commit(
            &s,
            sid,
            vec![Op::CreateLogical {
                origin: Origin::Local,
                key: key(path),
                kind: RecordKind::Decision,
                scope_lane: format!("team:{}/main", hex(&TEAM)),
                content: *blake3::hash(&body).as_bytes(),
                adopt_as_base: false,
            }],
            vec![body],
        ) {
            Response::Proposed { .. } => {}
            other => panic!("{other:?}"),
        }
    };
    let entry = |r: Response| match r {
        Response::TeamPrepared { entry: Some(e), .. } => e,
        other => panic!("{other:?}"),
    };
    // With the publish lock held, an import between step 7 and the push
    // (`lag_check`, a notify) leaves the proposal published.
    propose("p1");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("publish.lock"))
        .unwrap();
    let held = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusive).unwrap();
    let e = entry(prepare_entry(&s, None, false));
    let oid = sign_entry(&w, &e);
    assert!(matches!(begin(&s, &oid), Response::TeamLines { .. }));
    sync(&s);
    assert!(
        queue_states(&s).iter().any(|l| l.contains("Published")),
        "{:?}",
        queue_states(&s)
    );
    push_entry(&w, &oid, &e);
    w.fetch();
    sync(&s);
    drop(held);
    assert!(
        queue_states(&s).iter().all(|l| l.contains("Accepted")),
        "{:?}",
        queue_states(&s)
    );
    // Without the lock the proposal is requeued — and still takes its
    // outcome once the chain holds it.
    propose("p2");
    let e = entry(prepare_entry(&s, None, false));
    let oid = sign_entry(&w, &e);
    assert!(matches!(begin(&s, &oid), Response::TeamLines { .. }));
    sync(&s);
    assert!(
        queue_states(&s).iter().any(|l| l.contains("Queued")),
        "{:?}",
        queue_states(&s)
    );
    push_entry(&w, &oid, &e);
    w.fetch();
    sync(&s);
    assert!(
        queue_states(&s).iter().all(|l| l.contains("Accepted")),
        "{:?}",
        queue_states(&s)
    );
}

#[test]
fn an_announce_is_dropped_only_by_a_ref_read_after_its_process_ended() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("announce-race");
    let (a1, ba) = blob(b"a1");
    w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
    w.push();
    w.fetch();
    let repo = Repo::open(&w.edge).unwrap();
    let dir = w.tmp.join("ann-dir");
    let mut store =
        TeamStore::create(&dir, TEAM, ObjectFormat::Sha1, "origin", &w.genesis()).unwrap();
    store.import(&repo).unwrap();
    // T1 is fetched (the ref moved), its git process is gone, and the
    // replica is still at T0: a ref read before the process ended said T0.
    let (b1, bb) = blob(b"b1");
    let t1 = w.entry(vec![new_op("k/b", b1, true)], vec![bb]);
    w.push();
    w.fetch();
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let live = team::LiveMap::default();
    let tracking = format!("refs/contextdb/remotes/origin/team/{}/g0", hex(&TEAM));
    let pend = |refname: &str, oid: &str| {
        live.lock()
            .unwrap()
            .entry(TEAM)
            .or_default()
            .pending
            .insert((refname.to_owned(), oid.to_owned()), dead);
    };
    pend(&tracking, &t1);
    // Review N1, M4: refs import never follows resolve at once — another
    // remote's, and a generation that is not the one followed.
    pend(
        &format!("refs/contextdb/remotes/other/team/{}/g0", hex(&TEAM)),
        &t1,
    );
    pend(
        &format!("refs/contextdb/remotes/origin/team/{}/g5", hex(&TEAM)),
        &t1,
    );
    team::resolve_announces(&store, &repo, &live);
    let left: Vec<String> = (live.lock().unwrap()[&TEAM].pending.keys())
        .map(|(r, _)| r.clone())
        .collect();
    assert_eq!(
        left,
        vec![tracking.clone()],
        "the ref holds T1: still pending"
    );
    // Once the replica has T1, it resolves.
    store.import(&repo).unwrap();
    team::resolve_announces(&store, &repo, &live);
    assert!(live.lock().unwrap()[&TEAM].pending.is_empty());
}

#[test]
fn a_client_txn_id_is_idempotent_across_proposals_and_local_commits() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("txn");
    let (a1, ba) = blob(b"a1");
    w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
    w.push();
    w.fetch();
    let s = daemon(&w);
    sync(&s);
    let sid = session(&s);
    let send = |ops: Vec<Op>, blobs: Vec<Vec<u8>>, txn: [u8; 16]| {
        s.handle(
            AGENT,
            Request::Commit {
                binding: Binding::root(sid),
                ops,
                blobs,
                expect_cert_valid: false,
                client_txn_id: Some(txn),
            },
        )
        .response
    };
    let team_write = |path: &str| {
        let body = path.as_bytes().to_vec();
        (
            vec![Op::CreateLogical {
                origin: Origin::Local,
                key: key(path),
                kind: RecordKind::Decision,
                scope_lane: format!("team:{}/main", hex(&TEAM)),
                content: *blake3::hash(&body).as_bytes(),
                adopt_as_base: false,
            }],
            vec![body],
        )
    };
    let local_write = |name: &str| {
        let body = name.as_bytes().to_vec();
        (
            vec![Op::CreateLogical {
                origin: Origin::Local,
                key: format!("local/{name}"),
                kind: RecordKind::Decision,
                scope_lane: "main".into(),
                content: *blake3::hash(&body).as_bytes(),
                adopt_as_base: false,
            }],
            vec![body],
        )
    };
    let reuse = |r: &Response| code(r) == "TxnIdReuse";
    // A proposal and its retry: one proposal, the same id.
    let (ops, blobs) = team_write("p1");
    let first = match send(ops.clone(), blobs.clone(), [1; 16]) {
        Response::Proposed { id, .. } => id,
        other => panic!("{other:?}"),
    };
    match send(ops, blobs, [1; 16]) {
        Response::Proposed { id, .. } => assert_eq!(id, first),
        other => panic!("{other:?}"),
    }
    assert_eq!(queue_states(&s).len(), 1, "{:?}", queue_states(&s));
    // Another team write under the proposal's id.
    let (ops, blobs) = team_write("p2");
    let r = send(ops, blobs, [1; 16]);
    assert!(reuse(&r), "{r:?}");
    // A local write under the proposal's id commits nothing.
    let (ops, blobs) = local_write("l1");
    let r = send(ops, blobs, [1; 16]);
    assert!(reuse(&r), "{r:?}");
    // A team write under a locally committed id.
    let (ops, blobs) = local_write("l2");
    let r = send(ops, blobs, [2; 16]);
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    let (ops, blobs) = team_write("p3");
    let r = send(ops, blobs, [2; 16]);
    assert!(reuse(&r), "{r:?}");
    assert_eq!(queue_states(&s).len(), 1, "{:?}", queue_states(&s));
    // The id survives a restart of the queue.
    drop(s);
    let s = daemon(&w);
    let sid = session(&s);
    let (ops, blobs) = team_write("p1");
    let r = s
        .handle(
            AGENT,
            Request::Commit {
                binding: Binding::root(sid),
                ops,
                blobs,
                expect_cert_valid: false,
                client_txn_id: Some([1; 16]),
            },
        )
        .response;
    match r {
        Response::Proposed { id, .. } => assert_eq!(id, first),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_team_shred_keeps_bytes_a_local_artifact_or_note_holds() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("shredkeep");
    let (a1, ba) = blob(b"a1");
    w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
    w.push();
    w.fetch();
    let s = daemon(&w);
    sync(&s);
    let raw = |blobs: Vec<Vec<u8>>, ops: Vec<Op>| match s
        .handle(
            ADMIN,
            Request::Raw {
                blobs,
                batch: Batch::new(ops),
            },
        )
        .response
    {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    };
    let x = b"local record".to_vec();
    let r = raw(
        vec![x.clone()],
        vec![Op::CreateLogical {
            origin: Origin::Local,
            key: "dec:x".into(),
            kind: RecordKind::Decision,
            scope_lane: "repo".into(),
            content: *blake3::hash(&x).as_bytes(),
            adopt_as_base: true,
        }],
    );
    let rev = r.allocations.new_revisions[0];
    // A local artifact's payload and its resolution note.
    let (p, bp) = blob(b"payload");
    let (n, bn) = blob(b"note");
    let r = raw(
        vec![bp.1.clone()],
        vec![Op::OpenArtifact {
            ty: ArtifactType::PendingReview,
            target: rev.into(),
            payload_hash: p,
        }],
    );
    let art = r.allocations.new_artifacts[0];
    raw(
        vec![bn.1.clone()],
        vec![Op::ResolveArtifact {
            id: ArtifactRef::Existing(art),
            note: Some(n),
        }],
    );
    // The team writes and shreds the same bytes, and bytes nothing local
    // holds.
    let (q, bq) = blob(b"team only");
    w.entry(
        vec![
            new_op("k/p", p, false),
            new_op("k/n", n, false),
            new_op("k/q", q, false),
        ],
        vec![bp, bn, bq],
    );
    for h in [p, n, q] {
        w.entry(vec![TeamOp::TeamShred { content: h }], vec![]);
    }
    w.push();
    w.fetch();
    let lines = sync(&s);
    assert!(!lines[0].contains("Stalled"), "{lines:?}");
    let k = &s.actor.snapshot().kernel;
    assert!(k.is_shredded(&q));
    assert!(!k.is_shredded(&p), "an artifact payload is kept");
    assert!(!k.is_shredded(&n), "a resolution note is kept");
}

/// T117 (read redaction through the daemon): with `[team_lanes]` naming
/// bob only, carol's read of a mirror is UNAVAILABLE with the key echoed,
/// and bob reads it.
#[test]
fn t117_a_mirror_is_unavailable_to_a_principal_without_read_on_team_lanes() {
    use contextdb_daemon::principals::PrincipalMap;
    use contextdb_proto::fsguard;
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let mut w = World::new("t117r");
    let (a1, ba) = blob(b"a1");
    w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
    w.push();
    w.fetch();
    let s = daemon(&w);
    sync(&s);
    s.set_principals(
        PrincipalMap::new(
            Some("alice"),
            fsguard::current_uid(),
            [
                ("bob".to_owned(), 4242, PrincipalId(16)),
                ("carol".to_owned(), 4243, PrincipalId(17)),
            ],
        )
        .unwrap(),
    );
    let bob_only: std::collections::BTreeSet<String> = ["bob".to_owned()].into();
    let doc = AccessDoc {
        version: 1,
        owner: "alice".into(),
        principals: [
            ("alice".to_owned(), fsguard::current_uid()),
            ("bob".to_owned(), 4242),
            ("carol".to_owned(), 4243),
        ]
        .into(),
        groups: Default::default(),
        lanes: Default::default(),
        code_lanes: LaneEntry::code_default(),
        personal_egress: false,
        default_lane: LaneEntry::owner_only(),
        team_lanes: Some(Box::new(LaneEntry {
            members: bob_only.clone(),
            writers: bob_only,
            ..LaneEntry::owner_only()
        })),
    };
    let r = s
        .handle(
            ADMIN,
            Request::SetAccess {
                doc,
                client_txn_id: None,
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    let read_as = |who: Identity| {
        let sid = match s
            .handle(
                who,
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
        };
        s.handle(
            who,
            Request::Read {
                binding: Binding::root(sid),
                target: Target::Key(key("k/a")),
                mode: Mode::Advisory,
                level: None,
            },
        )
        .response
    };
    let bob = Identity {
        channel: Channel::Agent,
        principal: PrincipalId(16),
    };
    let carol = Identity {
        channel: Channel::Agent,
        principal: PrincipalId(17),
    };
    assert_eq!(read_as(carol), Response::Unavailable { target: key("k/a") });
    assert!(
        matches!(read_as(bob), Response::Read { .. }),
        "{:?}",
        read_as(bob)
    );
}

/// T113, G-LAG as a property (ADR 0018 §2.2, §8.3): random interleavings of
/// root entries, hook-running fetches (the `committed` notify delivered or
/// lost), hook aborts, hooks killed after their announce, fetches that
/// bypass the hooks, and `lag_check` imports. After every step, for
/// hook-running fetches: whenever the tracking ref's **actual** tip is not
/// the branch's watermark, a read of a mirror is not CERTIFIED. A bypassing
/// fetch is exempt until the next `lag_check` (§2.3). After each
/// `lag_check` with nothing pending, the read is CERTIFIED again.
/// Seeds: `CONTEXTDB_GLAG_SEEDS` (default 12).
#[test]
fn t113_g_lag_holds_at_every_step() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let seeds: u64 = std::env::var("CONTEXTDB_GLAG_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    let mut counts = std::collections::BTreeMap::<&str, u64>::new();
    for seed in 0..seeds {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let mut w = World::new(&format!("glag{seed}"));
        let (a1, ba) = blob(b"a1");
        w.entry(vec![new_op("k/a", a1, true)], vec![ba]);
        w.push();
        w.fetch();
        let s = daemon(&w);
        sync(&s);
        let sid = session(&s);
        let tracking = format!("refs/contextdb/remotes/origin/team/{}/g0", hex(&TEAM));
        let announce = |oid: &str, pid: u32| {
            let r = s
                .handle(
                    ADMIN,
                    Request::TeamAnnounce {
                        team: TEAM,
                        refname: tracking.clone(),
                        oid: oid.into(),
                        pid,
                    },
                )
                .response;
            assert!(matches!(r, Response::TeamLines { .. }), "{r:?}");
        };
        let edge = w.edge.clone();
        let actual = || git(&edge, &["rev-parse", &tracking]).trim().to_owned();
        let mut bypassed = false;
        let mut n = 0u32;
        for step in 0..14 {
            let remote_tip = w.parent.clone().expect("genesis");
            let event = match next(8) {
                0 | 1 => {
                    n += 1;
                    let (c, bc) = blob(format!("p{seed}-{n}").as_bytes());
                    w.entry(vec![new_op(&format!("k/p{n}"), c, false)], vec![bc]);
                    w.push();
                    "root-entry"
                }
                2 | 3 => {
                    // reference-transaction: `prepared` announces, git moves
                    // the ref, `committed` notifies (or the notify is lost).
                    let moves = actual() != remote_tip;
                    if moves {
                        announce(&remote_tip, std::process::id());
                    }
                    w.fetch();
                    if next(2) == 0 {
                        s.handle(ADMIN, Request::TeamNotify { team: TEAM });
                        "hook-fetch"
                    } else {
                        "hook-fetch-notify-lost"
                    }
                }
                4 => {
                    let oid = "2".repeat(40);
                    announce(&oid, std::process::id());
                    s.handle(
                        ADMIN,
                        Request::TeamWithdraw {
                            team: TEAM,
                            refname: tracking.clone(),
                            oid,
                        },
                    );
                    "hook-aborted"
                }
                5 => {
                    let mut child = Command::new("true").spawn().unwrap();
                    let dead = child.id();
                    child.wait().unwrap();
                    announce(&remote_tip, dead);
                    "hook-killed-after-announce"
                }
                6 => {
                    let before = actual();
                    w.fetch();
                    bypassed |= actual() != before;
                    "fetch-without-hooks"
                }
                _ => {
                    sync(&s);
                    bypassed = false;
                    assert_eq!(
                        summary(&s, sid, "k/a"),
                        Summary::Certified,
                        "seed {seed} step {step}: lag_check with nothing pending"
                    );
                    "lag-check"
                }
            };
            *counts.entry(event).or_default() += 1;
            if bypassed {
                continue;
            }
            let tip = actual();
            let watermark = (s.actor.snapshot().view(MAIN).watermark(&team_scope(&TEAM)))
                .map(|p| String::from_utf8_lossy(p).into_owned());
            if watermark.as_deref() != Some(tip.as_str()) {
                *counts.entry("checked-unmirrored").or_default() += 1;
                assert_ne!(
                    summary(&s, sid, "k/a"),
                    Summary::Certified,
                    "seed {seed} step {step} after {event}: the actual tip is not mirrored"
                );
            }
        }
    }
    eprintln!("G-LAG over {seeds} seeds: {counts:?}");
}
