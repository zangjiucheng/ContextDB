//! P9 milestone 2 (ADR 0017 §3.2, §4): the access document in the daemon.
//!
//! - `SetAccess` (admin only), the daemon's owner-uid check, `AccessShow`
//!   and `AccessCheck` with the deciding step (branch-dependent `dirs`).
//! - The uid mapping comes from the committed document: applied at start
//!   and after every `SetAccess`, only when multi-user mode is enabled
//!   (`Options::multi_user`, off by default and never set by `ctxd` in
//!   milestone 2), and refused while the file-system rules fail.
use contextdb_daemon::{
    actor,
    proto::*,
    server::{Client, Daemon, Options, PeerUidHook, StartError},
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_proto::fsguard;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const OWNER_AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
const BOB_UID: u32 = 4242;
const CAROL_UID: u32 = 4243;

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}
/// alice (the caller's uid) owns the store; bob and carol share `repo`;
/// carol is narrowed to `src/billing` in `billing` and denied `secrets.`.
fn doc(others: &[(&str, u32)]) -> AccessDoc {
    let mut principals: BTreeMap<String, u32> = [("alice".into(), fsguard::current_uid())].into();
    principals.extend(others.iter().map(|(n, u)| ((*n).to_owned(), *u)));
    let all = set(&["*members"]);
    let mut lanes = BTreeMap::new();
    let mut repo = LaneEntry {
        members: all.clone(),
        writers: all.clone(),
        curators: set(&["alice"]),
        ..LaneEntry::owner_only()
    };
    let mut billing = repo.clone();
    if principals.contains_key("carol") {
        repo.deny.insert(("carol".into(), "secrets.".into()));
        billing.grants.insert("carol".into(), set(&["src/billing"]));
    }
    lanes.insert("repo".to_owned(), repo);
    lanes.insert("billing".to_owned(), billing);
    AccessDoc {
        version: 1,
        owner: "alice".into(),
        principals,
        groups: BTreeMap::new(),
        lanes,
        code_lanes: LaneEntry::code_default(),
        personal_egress: false,
        default_lane: LaneEntry::owner_only(),
    }
}
fn code(r: &Response) -> &str {
    match r {
        Response::Error { code, .. } => code,
        _ => "ok",
    }
}
fn apply(s: &Service<SimDisk>, id: Identity, d: AccessDoc) -> Response {
    s.handle(
        id,
        Request::SetAccess {
            doc: d,
            client_txn_id: None,
        },
    )
    .response
}
fn check(s: &Service<SimDisk>, who: &str, key: &str, branch: Option<&str>) -> (bool, String) {
    match s
        .handle(
            ADMIN,
            Request::AccessCheck {
                principal: who.into(),
                key: key.into(),
                branch: branch.map(str::to_owned),
            },
        )
        .response
    {
        Response::AccessDecision { allowed, step, .. } => (allowed, step),
        other => panic!("{other:?}"),
    }
}
fn service() -> Service<SimDisk> {
    let disk = SimDisk::new();
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    Service::new(actor::spawn(store).unwrap(), log, events).unwrap()
}
fn create(s: &Service<SimDisk>, key: &str, lane: &str) {
    let body = key.as_bytes().to_vec();
    let r = s
        .handle(
            ADMIN,
            Request::Raw {
                blobs: vec![body.clone()],
                batch: Batch::new(vec![Op::CreateLogical {
                    key: key.into(),
                    kind: RecordKind::Fact,
                    scope_lane: lane.into(),
                    content: *blake3::hash(&body).as_bytes(),
                    adopt_as_base: true,
                }]),
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
}

#[test]
fn set_access_is_admin_only_and_names_the_daemon_uid_as_owner() {
    let s = service();
    let shown = |s: &Service<SimDisk>| match s.handle(ADMIN, Request::AccessShow).response {
        Response::Access {
            doc,
            ids,
            multi_user,
        } => (doc, ids, multi_user),
        other => panic!("{other:?}"),
    };
    assert_eq!(shown(&s), (None, vec![], false));
    assert_eq!(
        code(&apply(&s, OWNER_AGENT, doc(&[("bob", BOB_UID)]))),
        "ChannelRestricted"
    );
    assert_eq!(
        code(&s.handle(OWNER_AGENT, Request::AccessShow).response),
        "ChannelRestricted"
    );
    // The owner entry must carry the daemon's uid, whatever the path.
    let mut wrong = doc(&[("bob", BOB_UID)]);
    wrong.principals.insert("alice".into(), BOB_UID + 100);
    assert_eq!(code(&apply(&s, ADMIN, wrong.clone())), "InvalidAccess");
    let raw = |d: AccessDoc| {
        let mut b = Batch::new(vec![Op::SetAccess { doc: Box::new(d) }]);
        b.channel = Channel::Admin;
        s.handle(
            ADMIN,
            Request::Raw {
                blobs: vec![],
                batch: b,
            },
        )
        .response
    };
    assert_eq!(code(&raw(wrong)), "InvalidAccess");
    // The kernel's own validation.
    let mut nested = doc(&[("bob", BOB_UID)]);
    nested.groups.insert("g".into(), set(&["@g"]));
    assert_eq!(code(&apply(&s, ADMIN, nested)), "InvalidAccess");
    assert_eq!(shown(&s).0, None, "nothing committed");
    // ctx_commit and ctx_what_if cannot carry it.
    let session = match s
        .handle(
            OWNER_AGENT,
            Request::SessionStart {
                harness: "t".into(),
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
    let ops = vec![Op::SetAccess {
        doc: Box::new(doc(&[])),
    }];
    let r = s.handle(
        OWNER_AGENT,
        Request::Commit {
            binding: Binding::root(session),
            ops: ops.clone(),
            blobs: vec![],
            expect_cert_valid: false,
            client_txn_id: None,
        },
    );
    assert_eq!(code(&r.response), "InvalidRequest");
    let r = s.handle(
        OWNER_AGENT,
        Request::WhatIf {
            binding: Binding::root(session),
            ops,
            blobs: Vec::new(),
        },
    );
    assert_ne!(code(&r.response), "ok");
    // Applied: the document and its ids, and no mapping without multi-user
    // mode.
    let r = apply(&s, ADMIN, doc(&[("bob", BOB_UID), ("carol", CAROL_UID)]));
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    let (d, ids, multi) = shown(&s);
    assert_eq!(d.unwrap().owner, "alice");
    assert_eq!(
        ids,
        vec![
            ("alice".into(), PrincipalId(0)),
            ("bob".into(), PrincipalId(16)),
            ("carol".into(), PrincipalId(17)),
        ]
    );
    assert!(!multi);
    assert!(!s.principals().is_multi_user());
}

#[test]
fn access_check_reports_the_deciding_step_per_branch() {
    let s = service();
    create(&s, "db.engine", "repo");
    create(&s, "secrets.key", "repo");
    create(&s, "invoices", "billing");
    create(&s, "mine", "personal:alice");
    let r = apply(&s, ADMIN, doc(&[("bob", BOB_UID), ("carol", CAROL_UID)]));
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    let yes = |step: &str| (true, step.to_owned());
    let no = |step: &str| (false, step.to_owned());
    assert_eq!(check(&s, "alice", "secrets.key", None), yes("owner"));
    assert_eq!(check(&s, "bob", "db.engine", None), yes("allowed"));
    assert_eq!(check(&s, "carol", "secrets.key", None), no("acl"));
    assert_eq!(check(&s, "bob", "mine", None), no("membership"));
    // carol's grant needs a covered dir; without dirs, fail closed.
    assert_eq!(check(&s, "carol", "invoices", None), no("role"));
    // dirs are per branch: set on `f` only, carol may read there.
    let r = s
        .handle(
            ADMIN,
            Request::BranchCreate {
                name: "f".into(),
                from: None,
                client_txn_id: None,
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    let f = match s.handle(ADMIN, Request::BranchList).response {
        Response::Branches { branches } => branches.iter().find(|b| b.name == "f").unwrap().id,
        other => panic!("{other:?}"),
    };
    let mut meta = Batch::new(vec![Op::SetMetadata {
        logical: LogicalRef::Key("invoices".into()),
        tags: BTreeSet::new(),
        dirs: set(&["src/billing/2026"]),
    }]);
    meta.branch = f;
    let r = s
        .handle(
            ADMIN,
            Request::Raw {
                blobs: vec![],
                batch: meta,
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    assert_eq!(check(&s, "carol", "invoices", Some("f")), yes("allowed"));
    assert_eq!(check(&s, "carol", "invoices", None), no("role"));
    // Errors.
    let err = |who: &str, key: &str, b: Option<&str>| {
        code(
            &s.handle(
                ADMIN,
                Request::AccessCheck {
                    principal: who.into(),
                    key: key.into(),
                    branch: b.map(str::to_owned),
                },
            )
            .response,
        )
        .to_owned()
    };
    assert_eq!(err("zed", "db.engine", None), "UnknownPrincipal");
    assert_eq!(err("bob", "nope", None), "UnknownRef");
    assert_eq!(err("bob", "db.engine", Some("nope")), "UnknownBranch");
}

// ----------------------------------------------------------------- daemon

fn base(name: &str) -> PathBuf {
    let d = Path::new("/tmp")
        .canonicalize()
        .unwrap()
        .join(format!("ctx-p9a-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o711)).unwrap();
    d
}
fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}
fn start(dir: &Path, multi_user: bool, uid: &Arc<AtomicU32>) -> Result<Daemon, StartError> {
    fsguard::private_umask();
    let u = uid.clone();
    Daemon::start(
        dir,
        &Options {
            peer_uid: Some(PeerUidHook(Arc::new(move |_| {
                Some(u.load(Ordering::SeqCst))
            }))),
            multi_user,
            ..Options::default()
        },
    )
}
fn admin_apply(d: &Daemon, doc: AccessDoc) -> Response {
    let mut c = Client::connect(d.socket(Channel::Admin)).unwrap();
    c.call(&Request::SetAccess {
        doc,
        client_txn_id: None,
    })
    .unwrap()
    .response
}
fn session_start(c: &mut Client) -> Response {
    c.call(&Request::SessionStart {
        harness: "mcp".into(),
        repo: "r".into(),
        head_sha: "h".into(),
        parent: None,
        branch: None,
        git_branch: None,
    })
    .unwrap()
    .response
}
fn refused(path: &Path) -> bool {
    match Client::connect(path) {
        Err(_) => true,
        Ok(mut c) => c.call(&Request::BranchList).is_err(),
    }
}

/// Without `Options::multi_user` (every `ctxd` in milestone 2) a committed
/// document maps nobody: the agent socket stays 0600 and other uids are
/// refused.
#[test]
fn without_multi_user_mode_the_document_maps_nobody() {
    let dir = base("off");
    let me = fsguard::current_uid();
    let uid = Arc::new(AtomicU32::new(me));
    let d = start(&dir, false, &uid).unwrap();
    let r = admin_apply(&d, doc(&[("bob", BOB_UID)]));
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    assert_eq!(mode(d.socket(Channel::Agent)), 0o600);
    assert!(!d.service.principals().is_multi_user());
    uid.store(BOB_UID, Ordering::SeqCst);
    assert!(refused(d.socket(Channel::Agent)));
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

/// With multi-user mode enabled the mapping follows the committed
/// document: after `SetAccess`, after a revoking `SetAccess`, and at start.
#[test]
fn multi_user_mode_maps_the_committed_document() {
    let dir = base("on");
    let me = fsguard::current_uid();
    let uid = Arc::new(AtomicU32::new(me));
    let d = start(&dir, true, &uid).unwrap();
    assert_eq!(mode(d.socket(Channel::Agent)), 0o600, "no document yet");
    let r = admin_apply(&d, doc(&[("bob", BOB_UID)]));
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    assert_eq!(mode(d.socket(Channel::Agent)), 0o666);
    assert_eq!(
        d.service.principals().principal_of_uid(BOB_UID),
        Some(PrincipalId(16))
    );
    uid.store(BOB_UID, Ordering::SeqCst);
    let mut bob = Client::connect(d.socket(Channel::Agent)).unwrap();
    assert!(matches!(session_start(&mut bob), Response::Session { .. }));
    assert_eq!(
        code(&bob.call(&Request::Status).unwrap().response),
        "ChannelRestricted"
    );
    // A document without bob revokes the open connection.
    uid.store(me, Ordering::SeqCst);
    let r = admin_apply(&d, doc(&[("carol", CAROL_UID)]));
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    assert_eq!(
        code(&bob.call(&Request::BranchList).unwrap().response),
        "AccessRevoked"
    );
    // bob back, then a restart: the mapping comes from the log.
    let r = admin_apply(&d, doc(&[("bob", BOB_UID), ("carol", CAROL_UID)]));
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    drop(d);
    let d = start(&dir, true, &uid).unwrap();
    assert_eq!(mode(d.socket(Channel::Agent)), 0o666);
    let map = d.service.principals();
    assert_eq!(map.principal_of_uid(BOB_UID), Some(PrincipalId(16)));
    assert_eq!(map.principal_of_uid(CAROL_UID), Some(PrincipalId(17)));
    assert_eq!(map.principal_of_name("alice"), Some(PrincipalId(0)));
    uid.store(BOB_UID, Ordering::SeqCst);
    let mut bob = Client::connect(d.socket(Channel::Agent)).unwrap();
    assert!(matches!(session_start(&mut bob), Response::Session { .. }));
    drop(bob);
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The file-system rules gate both paths: a `SetAccess` mapping another uid
/// is refused (nothing committed) and a start with a mapping document is
/// refused, naming the entry.
#[test]
fn multi_user_mode_refuses_a_data_directory_breaking_the_rules() {
    let dir = base("rules");
    let me = fsguard::current_uid();
    let uid = Arc::new(AtomicU32::new(me));
    let d = start(&dir, true, &uid).unwrap();
    let loose = dir.join("loose");
    std::fs::write(&loose, "x").unwrap();
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
    let r = admin_apply(&d, doc(&[("bob", BOB_UID)]));
    match &r {
        Response::Error { code, detail } => {
            assert_eq!(code, "InvalidAccess");
            assert!(detail.contains("loose"), "{detail}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(mode(d.socket(Channel::Agent)), 0o600);
    // A document naming only the owner needs no rules.
    let r = admin_apply(&d, doc(&[]));
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    // Commit a mapping document while the rules hold, then break them and
    // restart.
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o600)).unwrap();
    let r = admin_apply(&d, doc(&[("bob", BOB_UID)]));
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    drop(d);
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
    match start(&dir, true, &uid) {
        Err(StartError::MultiUser(v)) => assert!(
            v.iter()
                .any(|v| matches!(v, fsguard::Violation::Mode { path, .. } if *path == loose)),
            "{v:?}"
        ),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("started with a loose entry"),
    }
    // Single-user mode still starts.
    let d = start(&dir, false, &uid).unwrap();
    assert_eq!(mode(d.socket(Channel::Agent)), 0o600);
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}
