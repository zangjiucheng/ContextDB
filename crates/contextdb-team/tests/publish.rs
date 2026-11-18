//! Proposals and publish through real git (ADR 0018 §6–7; T111, T117).
//! A bare remote; an edge whose store is bound to member alice; local
//! principals bob, carol and dave under a local access document. Signing is
//! done in process with alice's key, standing in for the hook's git signing
//! (milestone 5).

use contextdb_core::access::AccessState;
use contextdb_model::*;
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::git::Repo;
use contextdb_team::ops::{Edge, Hash, RuleSpec, TeamBatch, TeamOp};
use contextdb_team::proposals::{LocalPolicy, ProposeError, Queue, State};
use contextdb_team::publish::{self, BeginError, Mode, Prepared};
use contextdb_team::sshsig;
use contextdb_team::store::{Status, TeamStore};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

const TEAM: [u8; 16] = [0x44; 16];
const ALICE: [u8; 32] = [1; 32];

fn git(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}
fn ok(dir: &Path, args: &[&str]) -> String {
    let (s, o) = git(dir, args);
    assert!(s, "git {args:?}: {o}");
    o
}

fn team_doc() -> TeamDoc {
    TeamDoc::from_toml(&format!(
        "version = 1\naccept_version = 1\nid = \"{}\"\nadmins = [\"alice\"]\n\
         [members.alice]\nkeys = [\"{}\"]\n\
         [lanes.main]\nwriters = [\"*members\"]\ncurators = [\"*members\"]\n\
         [lanes.arch]\nwriters = [\"*members\"]\ncurators = [\"*members\"]\n",
        hex(&TEAM),
        sshsig::public_key(&ALICE).to_line(),
    ))
    .unwrap()
}

fn lane_entry(members: &[&str], writers: &[&str], curators: &[&str], egress: bool) -> LaneEntry {
    let set = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<_>>();
    LaneEntry {
        members: set(members),
        writers: set(writers),
        curators: set(curators),
        egress,
        grants: BTreeMap::new(),
        deny: BTreeSet::new(),
    }
}

/// The local document: bob and carol write `main` (bob curates); only dave
/// reads and writes `arch`. `egress_main` toggles egress of `main`.
fn local(egress_main: bool) -> AccessState {
    let main = format!("team:{}/main", hex(&TEAM));
    let arch = format!("team:{}/arch", hex(&TEAM));
    let doc = AccessDoc {
        version: 1,
        owner: "zed".into(),
        principals: [
            ("zed".into(), 500),
            ("bob".into(), 501),
            ("carol".into(), 502),
            ("dave".into(), 503),
        ]
        .into(),
        groups: BTreeMap::new(),
        lanes: [
            (
                main,
                lane_entry(
                    &["*members"],
                    &["bob", "carol", "dave"],
                    &["bob"],
                    egress_main,
                ),
            ),
            (arch, lane_entry(&["dave"], &["dave"], &["dave"], true)),
        ]
        .into(),
        code_lanes: LaneEntry::code_default(),
        personal_egress: false,
        default_lane: LaneEntry::owner_only(),
        team_lanes: None,
    };
    AccessState::apply(None, &doc).unwrap()
}

struct World {
    tmp: PathBuf,
    edge: PathBuf,
    repo: Repo,
    store: TeamStore,
    queue: Queue,
}

fn blob(bytes: &[u8]) -> (Hash, BTreeMap<Hash, Vec<u8>>) {
    let h = *blake3::hash(bytes).as_bytes();
    (h, [(h, bytes.to_vec())].into())
}

impl World {
    fn new(name: &str) -> Self {
        let tmp =
            std::env::temp_dir().join(format!("ctx-team-publish-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let remote = tmp.join("remote.git");
        ok(&tmp, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        // Genesis, made by an admin elsewhere and pushed.
        let creator = tmp.join("creator");
        ok(&tmp, &["init", "-q", creator.to_str().unwrap()]);
        let cr = Repo::open(&creator).unwrap();
        let batch = TeamBatch {
            proposals: vec![vec![TeamOp::SetTeamAccess {
                doc: Box::new(team_doc()),
            }]],
        }
        .encode();
        let tr = vec![
            ("Ctx-Team", hex(&TEAM)),
            ("Ctx-Team-Seq", "0".to_owned()),
            ("Ctx-Author", "alice".to_owned()),
            ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
            ("Ctx-Accept-Version", "1".to_owned()),
            ("Ctx-Proposal", hex(&[9; 16])),
        ];
        let tree = cr.write_tree(&[("batch".into(), batch)]).unwrap();
        let unsigned = commit::build(&tree, &[], "ctx team: genesis", &tr);
        let raw = commit::with_signature(
            &unsigned,
            &sshsig::sign(&ALICE, "git", &unsigned),
            ObjectFormat::Sha1,
        );
        let genesis = cr.write_commit(&raw).unwrap();
        let r = format!("refs/contextdb/team/{}/g0", hex(&TEAM));
        cr.update_ref(&r, &genesis, None).unwrap();
        ok(
            &creator,
            &["push", "-q", remote.to_str().unwrap(), &format!("{r}:{r}")],
        );
        // The edge.
        let edge = tmp.join("edge");
        ok(
            &tmp,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                edge.to_str().unwrap(),
            ],
        );
        ok(
            &edge,
            &[
                "config",
                "--add",
                "remote.origin.fetch",
                "+refs/contextdb/team/*:refs/contextdb/remotes/origin/team/*",
            ],
        );
        ok(&edge, &["fetch", "-q", "origin"]);
        let repo = Repo::open(&edge).unwrap();
        let data = tmp.join("data");
        let mut store =
            TeamStore::create(&data, TEAM, ObjectFormat::Sha1, "origin", &genesis).unwrap();
        assert_eq!(store.import(&repo).unwrap(), Status::Imported(1));
        let queue = Queue::open(&data).unwrap();
        Self {
            tmp,
            edge,
            repo,
            store,
            queue,
        }
    }

    fn policy<'a>(access: &'a AccessState, version: &'a [u8]) -> LocalPolicy<'a> {
        LocalPolicy {
            access: Some(access),
            version,
        }
    }

    fn propose(
        &mut self,
        who: &str,
        access: &AccessState,
        ops: Vec<TeamOp>,
        blobs: BTreeMap<Hash, Vec<u8>>,
    ) -> Result<[u8; 16], ProposeError> {
        let p = access.principal_of_name(who).unwrap();
        self.queue.propose(
            self.store.replica(),
            "alice",
            p,
            ops,
            blobs,
            &Self::policy(access, b"v1"),
        )
    }

    /// Signs the prepared entry with alice's key, writes it, and pushes it.
    /// Returns the push's success.
    fn sign_and_push(
        &mut self,
        prepared: &Prepared,
        policy_now: &[u8],
    ) -> (String, Result<(), BeginError>, bool) {
        let e = prepared.entry.as_ref().expect("an entry");
        let tree = self.repo.write_tree(&e.tree).unwrap();
        assert!(
            String::from_utf8_lossy(&e.payload).starts_with(&format!("tree {tree}\n")),
            "in-process tree id differs from git's"
        );
        let raw = commit::with_signature(
            &e.payload,
            &sshsig::sign(&ALICE, "git", &e.payload),
            ObjectFormat::Sha1,
        );
        let oid = self.repo.write_commit(&raw).unwrap();
        let begun = publish::begin(&mut self.queue, prepared, policy_now, &oid).unwrap();
        if begun.is_err() {
            return (oid, begun, false);
        }
        let r = format!("refs/contextdb/team/{}/g0", hex(&TEAM));
        let (pushed, _) = git(&self.edge, &["push", "-q", "origin", &format!("{oid}:{r}")]);
        (oid, begun, pushed)
    }

    fn sync(&mut self) {
        ok(&self.edge, &["fetch", "-q", "origin"]);
        self.store.import(&self.repo).unwrap();
        publish::reconcile(&self.store, &mut self.queue, "alice", &self.repo, true).unwrap();
    }

    fn state(&self, id: &[u8; 16]) -> State {
        self.queue.get(id).unwrap().state.clone()
    }

    fn remote_objects(&self) -> String {
        ok(
            &self.tmp.join("remote.git"),
            &["cat-file", "--batch-all-objects", "--batch-check"],
        )
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

fn new(path: &str, lane: &str, content: Hash, base: bool) -> TeamOp {
    TeamOp::New {
        path: path.into(),
        lane: lane.into(),
        kind: RecordKind::Convention,
        content,
        adopt_as_base: base,
    }
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

#[test]
fn publish_round_trip_and_queue_restart() {
    if !have_git() {
        return;
    }
    let mut w = World::new("round");
    let acc = local(true);
    let (a, ba) = blob(b"naming: snake_case");
    let id = w
        .propose("bob", &acc, vec![new("conv/naming", "main", a, true)], ba)
        .unwrap();
    assert_eq!(w.state(&id), State::Queued);
    let prepared = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&acc, b"v1"),
        Mode::Auto,
    )
    .unwrap();
    assert_eq!(prepared.entry.as_ref().unwrap().proposals, vec![id]);
    let (oid, begun, pushed) = w.sign_and_push(&prepared, b"v1");
    assert_eq!((begun, pushed), (Ok(()), true));
    assert!(matches!(w.state(&id), State::Published { commit } if commit == oid));
    w.sync();
    assert_eq!(w.state(&id), State::Accepted);
    assert_eq!(w.store.replica().certified("conv/naming"), Some(true));
    // The queue survives a restart.
    let dir = w.store.dir().to_owned();
    let q = Queue::open(&dir).unwrap();
    assert_eq!(q.get(&id).unwrap().state, State::Accepted);
}

#[test]
fn egress_turned_off_after_enqueue_never_leaves_the_machine() {
    if !have_git() {
        return;
    }
    let mut w = World::new("egress");
    let on = local(true);
    let (a, ba) = blob(b"secret-ish convention body");
    let p1 = w
        .propose("bob", &on, vec![new("conv/a", "main", a, false)], ba)
        .unwrap();
    let (b, bb) = blob(b"depends on a");
    let p2 = w
        .propose(
            "bob",
            &on,
            vec![TeamOp::Revise {
                path: "conv/a".into(),
                content: b,
                adopt_as_base: false,
            }],
            bb,
        )
        .unwrap();
    assert_eq!(w.queue.get(&p2).unwrap().deps, vec![p1]);
    let before = w.remote_objects();
    // The owner turns egress off; the next push publishes nothing.
    let off = local(false);
    let prepared = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&off, b"v2"),
        Mode::Auto,
    )
    .unwrap();
    assert!(prepared.entry.is_none());
    assert_eq!(
        w.state(&p1),
        State::Conflicted {
            reason: "policy: EgressDenied".into()
        }
    );
    assert_eq!(
        w.state(&p2),
        State::Conflicted {
            reason: "dependency".into()
        }
    );
    assert_eq!(w.remote_objects(), before, "nothing reached the remote");
    // The proposal's body, as a git blob, is not on the remote either.
    let blob_oid = hex(&contextdb_team::gitobj::object_id(
        ObjectFormat::Sha1,
        "blob",
        b"secret-ish convention body",
    )
    .unwrap());
    let (present, _) = git(&w.tmp.join("remote.git"), &["cat-file", "-e", &blob_oid]);
    assert!(!present, "the body must not have been uploaded");
    // (It is in the edge's queue, and only there.)
    assert!(w.queue.blob(&a).is_some());
    // Restoring the policy does not resurrect them.
    let prepared = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&on, b"v3"),
        Mode::Auto,
    )
    .unwrap();
    assert!(prepared.entry.is_none());
}

#[test]
fn a_policy_change_between_check_and_begin_stops_the_entry() {
    if !have_git() {
        return;
    }
    let mut w = World::new("lin");
    let acc = local(true);
    let (a, ba) = blob(b"a");
    let id = w
        .propose("bob", &acc, vec![new("conv/a", "main", a, false)], ba)
        .unwrap();
    let before = w.remote_objects();
    let prepared = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&acc, b"v1"),
        Mode::Auto,
    )
    .unwrap();
    // A SetAccess commits between step 5 and step 7.
    let (_, begun, pushed) = w.sign_and_push(&prepared, b"v2");
    assert_eq!((begun, pushed), (Err(BeginError::PolicyChanged), false));
    assert_eq!(w.state(&id), State::Queued);
    assert_eq!(w.remote_objects(), before);
    // Prepared again under the new policy, it goes out.
    let prepared = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&acc, b"v2"),
        Mode::Auto,
    )
    .unwrap();
    let (_, begun, pushed) = w.sign_and_push(&prepared, b"v2");
    assert_eq!((begun, pushed), (Ok(()), true));
    w.sync();
    assert_eq!(w.state(&id), State::Accepted);
}

#[test]
fn confirm_publishes_exactly_the_listed_set() {
    if !have_git() {
        return;
    }
    let mut w = World::new("confirm");
    let acc = local(true);
    let (a, ba) = blob(b"a");
    let (b, bb) = blob(b"b");
    let pb = w
        .propose("bob", &acc, vec![new("conv/a", "main", a, false)], ba)
        .unwrap();
    let pc = w
        .propose("carol", &acc, vec![new("conv/b", "main", b, false)], bb)
        .unwrap();
    // Two local authors: auto becomes confirm, nothing sent.
    let listed = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&acc, b"v1"),
        Mode::Auto,
    )
    .unwrap();
    assert!(listed.entry.is_none());
    let (token, who) = listed.listed.clone().unwrap();
    assert_eq!(who.len(), 2);
    // Queued after the listing: not part of it.
    let (c, bc) = blob(b"c");
    let late = w
        .propose("carol", &acc, vec![new("conv/c", "main", c, false)], bc)
        .unwrap();
    let yes = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&acc, b"v1"),
        Mode::Yes(token),
    )
    .unwrap();
    let mut sent = yes.entry.as_ref().unwrap().proposals.clone();
    sent.sort();
    let mut want = vec![pb, pc];
    want.sort();
    assert_eq!(sent, want);
    let (_, begun, pushed) = w.sign_and_push(&yes, b"v1");
    assert_eq!((begun, pushed), (Ok(()), true));
    w.sync();
    assert_eq!(
        (w.state(&pb), w.state(&pc), w.state(&late)),
        (State::Accepted, State::Accepted, State::Queued)
    );
    // An unknown token is refused.
    assert!(
        publish::prepare(
            &w.store,
            &mut w.queue,
            "alice",
            &World::policy(&acc, b"v1"),
            Mode::Yes([7; 32])
        )
        .is_err()
    );
}

#[test]
fn a_lost_race_requeues_and_rebuilds_on_the_new_tip() {
    if !have_git() {
        return;
    }
    let mut w = World::new("race");
    let acc = local(true);
    let (a, ba) = blob(b"a");
    let base = w
        .propose("bob", &acc, vec![new("conv/a", "main", a, true)], ba)
        .unwrap();
    let first = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&acc, b"v1"),
        Mode::Auto,
    )
    .unwrap();
    let (_, begun, pushed) = w.sign_and_push(&first, b"v1");
    assert_eq!((begun, pushed), (Ok(()), true));
    w.sync();
    assert_eq!(w.state(&base), State::Accepted);
    // Two proposals: one with a Cur premise on conv/a's head.
    let (b, bb) = blob(b"b");
    let rule = RuleSpec {
        conclusion: ("conv/b".into(), b),
        incidences: vec![("conv/a".into(), Edge::Cur(a))],
    };
    let dependent = w
        .propose(
            "bob",
            &acc,
            vec![
                new("conv/b", "main", b, false),
                TeamOp::AddRule {
                    rule,
                    enabled: true,
                },
            ],
            bb,
        )
        .unwrap();
    let ours = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&acc, b"v1"),
        Mode::Auto,
    )
    .unwrap();
    // Meanwhile another of alice's machines moves conv/a's head.
    let other = w.tmp.join("other");
    ok(
        &w.tmp,
        &[
            "clone",
            "-q",
            w.tmp.join("remote.git").to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    ok(
        &other,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/contextdb/team/*:refs/contextdb/team/*",
        ],
    );
    let or = Repo::open(&other).unwrap();
    let r = format!("refs/contextdb/team/{}/g0", hex(&TEAM));
    let tip = or.resolve(&r).unwrap().unwrap();
    let (a2, _) = blob(b"a2");
    let batch = TeamBatch {
        proposals: vec![vec![
            TeamOp::Revise {
                path: "conv/a".into(),
                content: a2,
                adopt_as_base: true,
            },
            TeamOp::SetHead {
                path: "conv/a".into(),
                expected: a,
                new: a2,
            },
        ]],
    }
    .encode();
    let tr = vec![
        ("Ctx-Team", hex(&TEAM)),
        ("Ctx-Team-Seq", "2".to_owned()),
        ("Ctx-Author", "alice".to_owned()),
        ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
        ("Ctx-Accept-Version", "1".to_owned()),
        ("Ctx-Proposal", hex(&[0x77; 16])),
    ];
    let tree = or
        .write_tree(&[
            ("batch".into(), batch),
            (format!("blobs/{}", hex(&a2)), b"a2".to_vec()),
        ])
        .unwrap();
    let unsigned = commit::build(
        &tree,
        std::slice::from_ref(&tip),
        "ctx team: other machine",
        &tr,
    );
    let raw = commit::with_signature(
        &unsigned,
        &sshsig::sign(&ALICE, "git", &unsigned),
        ObjectFormat::Sha1,
    );
    let theirs = or.write_commit(&raw).unwrap();
    ok(&other, &["push", "-q", "origin", &format!("{theirs}:{r}")]);
    // Our push loses the race (non-fast-forward): back to the queue.
    let (_, begun, pushed) = w.sign_and_push(&ours, b"v1");
    assert_eq!((begun, pushed), (Ok(()), false));
    publish::refused(&mut w.queue, &ours).unwrap();
    assert_eq!(w.state(&dependent), State::Queued);
    // After fetch and import it is rebuilt on the new tip: its Cur premise
    // moved, so it is left out as conflicted.
    w.sync();
    let again = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&acc, b"v1"),
        Mode::Auto,
    )
    .unwrap();
    assert!(again.entry.is_none());
    assert!(
        matches!(w.state(&dependent), State::Conflicted { reason } if reason.starts_with("CurMoved"))
    );
}

#[test]
fn the_local_layer_hides_what_its_author_cannot_read() {
    if !have_git() {
        return;
    }
    let mut w = World::new("local");
    let acc = local(true);
    // A record in arch, which only dave reads, published by the owner.
    let (x, bx) = blob(b"arch secret");
    let owner_id = w
        .queue
        .propose(
            w.store.replica(),
            "alice",
            PrincipalId::STORE_OWNER,
            vec![new("arch/x", "arch", x, true)],
            bx,
            &World::policy(&acc, b"v1"),
        )
        .unwrap();
    let p = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&acc, b"v1"),
        Mode::Auto,
    )
    .unwrap();
    let (_, begun, pushed) = w.sign_and_push(&p, b"v1");
    assert_eq!((begun, pushed), (Ok(()), true));
    w.sync();
    assert_eq!(w.state(&owner_id), State::Accepted);
    let (c, bc) = blob(b"carol's");
    let carol_rule = |path: &str, h: Hash| {
        vec![
            new("conv/c", "main", c, false),
            TeamOp::AddRule {
                rule: RuleSpec {
                    conclusion: ("conv/c".into(), c),
                    incidences: vec![(path.into(), Edge::Pin(h))],
                },
                enabled: true,
            },
        ]
    };
    // Naming a record carol cannot read and naming none: the same refusal.
    let hidden = w
        .propose("carol", &acc, carol_rule("arch/x", x), bc.clone())
        .unwrap_err();
    let missing = w
        .propose("carol", &acc, carol_rule("arch/none", x), bc.clone())
        .unwrap_err();
    let shape = |e: &ProposeError| match e {
        ProposeError::Local(TxError::Unauthorized { op_index, need, .. }) => (*op_index, *need),
        other => panic!("{other:?}"),
    };
    assert_eq!(shape(&hidden), (1, AccessNeed::Read));
    assert_eq!(shape(&missing), shape(&hidden));
    // A record that exists only in dave's queued proposal is hidden too.
    let (y, by) = blob(b"dave's queued");
    let dave = w
        .propose("dave", &acc, vec![new("arch/y", "arch", y, false)], by)
        .unwrap();
    assert_eq!(
        shape(
            &w.propose("carol", &acc, carol_rule("arch/y", y), bc.clone())
                .unwrap_err()
        ),
        (1, AccessNeed::Read)
    );
    // Its key is taken (the global key namespace), and nothing more is said.
    assert!(matches!(
        w.propose(
            "carol",
            &acc,
            vec![new("arch/y", "main", c, false)],
            bc.clone()
        )
        .unwrap_err(),
        ProposeError::Local(TxError::LogicalExists { .. })
    ));
    // Carol's listing shows neither dave's proposal nor a count of it.
    let carol = acc.principal_of_name("carol").unwrap();
    assert!(
        w.queue
            .listing_for(carol, w.store.replica(), &World::policy(&acc, b"v1"))
            .iter()
            .all(|p| p.id != dave)
    );
    let dave_p = acc.principal_of_name("dave").unwrap();
    assert!(
        w.queue
            .listing_for(dave_p, w.store.replica(), &World::policy(&acc, b"v1"))
            .iter()
            .any(|p| p.id == dave)
    );
    // Roles: carol may not curate main, nor write arch.
    assert!(matches!(
        w.propose(
            "carol",
            &acc,
            vec![new("conv/d", "main", c, true)],
            bc.clone()
        )
        .unwrap_err(),
        ProposeError::Local(TxError::Unauthorized {
            need: AccessNeed::Curate,
            ..
        })
    ));
    assert!(matches!(
        w.propose("carol", &acc, vec![new("arch/z", "arch", c, false)], bc)
            .unwrap_err(),
        ProposeError::Local(TxError::Unauthorized {
            need: AccessNeed::Write,
            ..
        })
    ));
}

#[test]
fn published_but_never_pushed_is_queued_again() {
    if !have_git() {
        return;
    }
    let mut w = World::new("crash");
    let acc = local(true);
    let (a, ba) = blob(b"a");
    let id = w
        .propose("bob", &acc, vec![new("conv/a", "main", a, false)], ba)
        .unwrap();
    let prepared = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&acc, b"v1"),
        Mode::Auto,
    )
    .unwrap();
    // Step 7 records `published`; the process dies before the push.
    let oid = "1".repeat(40);
    assert_eq!(
        publish::begin(&mut w.queue, &prepared, b"v1", &oid).unwrap(),
        Ok(())
    );
    let dir = w.store.dir().to_owned();
    w.queue = Queue::open(&dir).unwrap();
    assert!(matches!(w.state(&id), State::Published { .. }));
    // The next fetch shows the chain does not hold it: queued again, and the
    // next publish sends it.
    w.sync();
    assert_eq!(w.state(&id), State::Queued);
    let again = publish::prepare(
        &w.store,
        &mut w.queue,
        "alice",
        &World::policy(&acc, b"v1"),
        Mode::Auto,
    )
    .unwrap();
    let (_, begun, pushed) = w.sign_and_push(&again, b"v1");
    assert_eq!((begun, pushed), (Ok(()), true));
    w.sync();
    assert_eq!(w.state(&id), State::Accepted);
}

#[test]
fn an_entry_over_the_limits_is_split_and_the_rest_waits() {
    if !have_git() {
        return;
    }
    let mut w = World::new("split");
    let acc = local(true);
    let (a, ba) = blob(b"base record");
    let first = w
        .propose("bob", &acc, vec![new("conv/a", "main", a, true)], ba)
        .unwrap();
    let pol = World::policy(&acc, b"v1");
    let p = publish::prepare(&w.store, &mut w.queue, "alice", &pol, Mode::Auto).unwrap();
    assert_eq!(p.entry.as_ref().unwrap().proposals, vec![first]);
    let (_, begun, pushed) = w.sign_and_push(&p, b"v1");
    assert_eq!((begun, pushed), (Ok(()), true));
    w.sync();
    // Two proposals of 501 ops each: together over MAX_OPS (1,000).
    let expect = |n: usize| -> Vec<TeamOp> {
        (0..n)
            .map(|_| TeamOp::Expect {
                path: "conv/a".into(),
                content: a,
            })
            .collect()
    };
    let x = w
        .propose("bob", &acc, expect(501), BTreeMap::new())
        .unwrap();
    let y = w
        .propose("bob", &acc, expect(501), BTreeMap::new())
        .unwrap();
    let pol = World::policy(&acc, b"v1");
    let p = publish::prepare(&w.store, &mut w.queue, "alice", &pol, Mode::Auto).unwrap();
    assert_eq!(
        p.entry.as_ref().unwrap().proposals,
        vec![x],
        "y waits for the next entry"
    );
    assert!(p.conflicted.is_empty());
    let (_, begun, pushed) = w.sign_and_push(&p, b"v1");
    assert_eq!((begun, pushed), (Ok(()), true));
    w.sync();
    assert_eq!(w.state(&x), State::Accepted);
    assert_eq!(w.state(&y), State::Queued);
    let pol = World::policy(&acc, b"v1");
    let p = publish::prepare(&w.store, &mut w.queue, "alice", &pol, Mode::Auto).unwrap();
    assert_eq!(p.entry.as_ref().unwrap().proposals, vec![y]);
    let (_, begun, pushed) = w.sign_and_push(&p, b"v1");
    assert_eq!((begun, pushed), (Ok(()), true));
    w.sync();
    assert_eq!(w.state(&y), State::Accepted);
}

#[test]
fn a_dependent_of_a_deferred_proposal_waits_too_and_oversized_is_refused() {
    if !have_git() {
        return;
    }
    let mut w = World::new("defer");
    let acc = local(true);
    let (a, ba) = blob(b"base");
    let base = w
        .propose("bob", &acc, vec![new("conv/a", "main", a, true)], ba)
        .unwrap();
    let pol = World::policy(&acc, b"v1");
    let p = publish::prepare(&w.store, &mut w.queue, "alice", &pol, Mode::Auto).unwrap();
    assert_eq!(p.entry.as_ref().unwrap().proposals, vec![base]);
    let (_, begun, pushed) = w.sign_and_push(&p, b"v1");
    assert_eq!((begun, pushed), (Ok(()), true));
    w.sync();
    let expect = |n: usize| -> Vec<TeamOp> {
        (0..n)
            .map(|_| TeamOp::Expect {
                path: "conv/a".into(),
                content: a,
            })
            .collect()
    };
    let x = w
        .propose("bob", &acc, expect(501), BTreeMap::new())
        .unwrap();
    let (b, bb) = blob(b"b1");
    let mut y_ops = vec![new("conv/b", "main", b, false)];
    y_ops.extend(expect(500));
    let y = w.propose("bob", &acc, y_ops, bb).unwrap();
    let (b2, bb2) = blob(b"b2");
    let z = w
        .propose(
            "bob",
            &acc,
            vec![TeamOp::Revise {
                path: "conv/b".into(),
                content: b2,
                adopt_as_base: false,
            }],
            bb2,
        )
        .unwrap();
    let pol = World::policy(&acc, b"v1");
    let p = publish::prepare(&w.store, &mut w.queue, "alice", &pol, Mode::Auto).unwrap();
    assert_eq!(p.entry.as_ref().unwrap().proposals, vec![x]);
    assert!(
        p.conflicted.is_empty(),
        "z waits with y: {:?}",
        p.conflicted
    );
    let (_, begun, pushed) = w.sign_and_push(&p, b"v1");
    assert_eq!((begun, pushed), (Ok(()), true));
    w.sync();
    assert_eq!(w.state(&z), State::Queued);
    let pol = World::policy(&acc, b"v1");
    let p = publish::prepare(&w.store, &mut w.queue, "alice", &pol, Mode::Auto).unwrap();
    assert_eq!(p.entry.as_ref().unwrap().proposals, vec![y, z]);
    let (_, begun, pushed) = w.sign_and_push(&p, b"v1");
    assert_eq!((begun, pushed), (Ok(()), true));
    w.sync();
    assert_eq!(
        (w.state(&y), w.state(&z)),
        (State::Accepted, State::Accepted)
    );
    // Q11: a proposal no entry can hold is refused at enqueue.
    let mut ops = Vec::new();
    let mut blobs = BTreeMap::new();
    for i in 0..9u8 {
        let body = vec![i; (1 << 20) - 16];
        let h = *blake3::hash(&body).as_bytes();
        ops.push(new(&format!("big/{i}"), "main", h, false));
        blobs.insert(h, body);
    }
    assert!(matches!(
        w.propose("bob", &acc, ops, blobs),
        Err(ProposeError::TooLarge("entry"))
    ));
}

/// A `SetTeamAccess` proposal is never packed with another proposal: accept
/// refuses such an entry (`SoleOpRequired`) and every proposal in it with
/// it. It takes an entry of its own, and the proposals after it wait.
/// (Team administration is the store owner's, zed's, locally.)
#[test]
fn a_team_access_proposal_takes_an_entry_of_its_own() {
    if !have_git() {
        return;
    }
    let mut w = World::new("sole");
    let acc = local(true);
    let (a, ba) = blob(b"before the access change");
    let (b, bb) = blob(b"after the access change");
    let first = w
        .propose("zed", &acc, vec![new("conv/a", "main", a, true)], ba)
        .unwrap();
    let access = w
        .propose(
            "zed",
            &acc,
            vec![TeamOp::SetTeamAccess {
                doc: Box::new(team_doc()),
            }],
            BTreeMap::new(),
        )
        .unwrap();
    let last = w
        .propose("zed", &acc, vec![new("conv/b", "main", b, true)], bb)
        .unwrap();
    for expect in [first, access, last] {
        let pol = World::policy(&acc, b"v1");
        let p = publish::prepare(&w.store, &mut w.queue, "alice", &pol, Mode::Auto).unwrap();
        assert!(p.conflicted.is_empty(), "{:?}", p.conflicted);
        assert_eq!(p.entry.as_ref().unwrap().proposals, vec![expect]);
        let (_, begun, pushed) = w.sign_and_push(&p, b"v1");
        assert_eq!((begun, pushed), (Ok(()), true));
        w.sync();
        assert_eq!(w.state(&expect), State::Accepted);
    }
}
