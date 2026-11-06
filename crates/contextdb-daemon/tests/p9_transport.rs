//! P9 milestone 1 (ADR 0017 §3): principals and the transport.
//!
//! - T58: no request field or binding changes who a request acts for; the
//!   committed row carries the transport's principal.
//! - T98: the socket layout and modes, the endpoint record, mapped and
//!   unmapped uids (through the debug-build peer-uid hook), the start-up
//!   checks before multi-user mode (modes, ACLs, ancestors, the long-path
//!   fallback), revocation, and the client's check of the server.
//! - T99: sessions, harness keys and handoffs per principal.
//!
//! The daemon tests use `/tmp` (root-owned and sticky) so a second uid could
//! search down to the shared socket; the per-user temp directory could not.
use contextdb_daemon::{
    actor,
    principals::PrincipalMap,
    proto::*,
    server::{Client, Daemon, Options, PeerUidHook},
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_proto::{fsguard, lease};
use contextdb_store::{OpenOptions, SimDisk, Store, TailFate, session::SessionLog};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

const OWNER_AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const BOB: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(16),
};
const CAROL: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(17),
};
const BOB_UID: u32 = 4242;
fn map() -> PrincipalMap {
    PrincipalMap::new(
        Some("alice"),
        fsguard::current_uid(),
        [
            ("bob".to_owned(), BOB_UID, PrincipalId(16)),
            ("carol".to_owned(), BOB_UID + 1, PrincipalId(17)),
        ],
    )
    .unwrap()
}

// ---------------------------------------------------------------- service

type Svc = Service<SimDisk>;
fn service(disk: &SimDisk) -> Svc {
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    let s = Service::new(actor::spawn(store).unwrap(), log, events).unwrap();
    s.set_principals(map());
    s
}
fn code(r: &Response) -> &str {
    match r {
        Response::Error { code, .. } => code,
        _ => "ok",
    }
}
fn h(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}
fn records(s: &Svc, n: u32) {
    let ops = (0..n)
        .map(|i| Op::CreateLogical {
            key: format!("k{i}"),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: h(format!("body {i}").as_bytes()),
            adopt_as_base: true,
        })
        .collect();
    let blobs = (0..n).map(|i| format!("body {i}").into_bytes()).collect();
    let r = s
        .handle(
            ADMIN,
            Request::Raw {
                blobs,
                batch: Batch::new(ops),
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
}
fn start(s: &Svc, id: Identity) -> SessionId {
    match s
        .handle(
            id,
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
fn read(s: &Svc, id: Identity, session: SessionId, key: &str) -> Response {
    s.handle(
        id,
        Request::Read {
            binding: Binding::root(session),
            target: Target::Key(key.into()),
            mode: Mode::Certified,
            level: None,
        },
    )
    .response
}
fn attach(s: &Svc, id: Identity, key: &str) -> (SessionId, bool) {
    match s
        .handle(
            id,
            Request::SessionAttach {
                harness: "claude".into(),
                harness_session: key.into(),
                repo: "r".into(),
                head_sha: "h".into(),
                branch: None,
                git_branch: None,
            },
        )
        .response
    {
        Response::Attached { session, created } => (session, created),
        other => panic!("{other:?}"),
    }
}
fn begin(s: &Svc, id: Identity, session: SessionId, to: Option<&str>) -> Response {
    s.handle(
        id,
        Request::HandoffBegin {
            binding: Binding::root(session),
            summary: "half done".into(),
            to_agent: None,
            to: to.map(str::to_owned),
        },
    )
    .response
}
fn begun(r: Response) -> [u8; 16] {
    match r {
        Response::HandoffBegun { id, .. } => id,
        other => panic!("{other:?}"),
    }
}
fn listed(s: &Svc, id: Identity, session: SessionId) -> Vec<[u8; 16]> {
    match s
        .handle(
            id,
            Request::HandoffAccept {
                binding: Binding::root(session),
                id: None,
            },
        )
        .response
    {
        Response::Handoffs { items } => items.into_iter().map(|i| i.id).collect(),
        other => panic!("{other:?}"),
    }
}
fn accept(s: &Svc, id: Identity, session: SessionId, h: [u8; 16]) -> Response {
    s.handle(
        id,
        Request::HandoffAccept {
            binding: Binding::root(session),
            id: Some(h),
        },
    )
    .response
}

/// T58: the transport decides the principal. A non-owner cannot reach the
/// requests that carry a whole batch (`Raw`) or a verdict (`Validate`), and
/// what it does commit is recorded under its own principal and channel.
#[test]
fn t58_identity_comes_from_the_transport_only() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let mut forged = Batch::new(vec![Op::CreateLogical {
        key: "forged".into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: h(b"x"),
        adopt_as_base: true,
    }]);
    forged.principal = PrincipalId(0);
    forged.channel = Channel::Admin;
    let r = s
        .handle(
            BOB,
            Request::Raw {
                blobs: vec![b"x".to_vec()],
                batch: forged.clone(),
            },
        )
        .response;
    assert_eq!(code(&r), "ChannelRestricted");
    // The dry run of the same batch runs as BOB (no admin op is allowed).
    let r = s
        .handle(BOB, Request::ImpactBatch { batch: forged })
        .response;
    assert!(matches!(r, Response::ImpactExact { .. }), "{r:?}");
    let session = start(&s, BOB);
    let r = s
        .handle(
            BOB,
            Request::Record {
                binding: Binding::root(session),
                kind: RecordKind::Fact,
                body: b"bob's".to_vec(),
                target: WriteTarget::Create {
                    key: "bob/fact".into(),
                    scope_lane: "repo".into(),
                },
                deps: Vec::new(),
                adopt_as_base: false,
                client_txn_id: None,
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    let snap = s.actor.snapshot();
    let row = snap.log.read_row(snap.epoch).unwrap();
    assert_eq!(
        (row.principal, row.channel),
        (PrincipalId(16), Channel::Agent)
    );
    // The owner's channels keep their principals.
    let mine = start(&s, OWNER_AGENT);
    let r = s
        .handle(
            OWNER_AGENT,
            Request::Record {
                binding: Binding::root(mine),
                kind: RecordKind::Fact,
                body: b"alice's".to_vec(),
                target: WriteTarget::Create {
                    key: "alice/fact".into(),
                    scope_lane: "repo".into(),
                },
                deps: Vec::new(),
                adopt_as_base: false,
                client_txn_id: None,
            },
        )
        .response;
    assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    let snap = s.actor.snapshot();
    assert_eq!(
        snap.log.read_row(snap.epoch).unwrap().principal,
        PrincipalId(1)
    );
}

/// T99: a session is bound only by its principal, through every binding;
/// harness keys are per principal, also after a restart.
#[test]
fn t99_sessions_and_harness_keys_are_per_principal() {
    let disk = SimDisk::new();
    let s = service(&disk);
    records(&s, 1);
    let alice = start(&s, OWNER_AGENT);
    // The owner's channels are one principal.
    assert_eq!(code(&read(&s, ADMIN, alice, "k0")), "ok");
    assert_eq!(
        code(&read(&s, BOB, alice, "k0")),
        "SessionOfAnotherPrincipal"
    );
    let r = s
        .handle(BOB, Request::SessionEnd { session: alice })
        .response;
    assert_eq!(code(&r), "SessionOfAnotherPrincipal");
    // A tool-call id the owner's hook claimed cannot be used by Bob, and Bob
    // cannot claim one (the hook request is the owner's).
    let r = s
        .handle(
            OWNER_AGENT,
            Request::Claim {
                session: alice,
                agent: None,
                call: "toolu_1".into(),
            },
        )
        .response;
    assert_eq!(code(&r), "ok");
    let by_call = |id: Identity| {
        s.handle(
            id,
            Request::Read {
                binding: Binding::Call {
                    id: "toolu_1".into(),
                },
                target: Target::Key("k0".into()),
                mode: Mode::Certified,
                level: None,
            },
        )
        .response
    };
    assert_eq!(code(&by_call(OWNER_AGENT)), "ok");
    assert_eq!(code(&by_call(BOB)), "SessionOfAnotherPrincipal");
    let bob = start(&s, BOB);
    let r = s
        .handle(
            BOB,
            Request::Claim {
                session: bob,
                agent: None,
                call: "toolu_2".into(),
            },
        )
        .response;
    assert_eq!(code(&r), "ChannelRestricted");
    // The same harness key names different sessions for different principals.
    let (a1, created) = attach(&s, OWNER_AGENT, "thread-1");
    assert!(created);
    let (b1, created) = attach(&s, BOB, "thread-1");
    assert!(created);
    assert_ne!(a1, b1);
    assert_eq!(attach(&s, OWNER_AGENT, "thread-1"), (a1, false));
    assert_eq!(attach(&s, BOB, "thread-1"), (b1, false));
    let by_key = |id: Identity| {
        s.handle(
            id,
            Request::Read {
                binding: Binding::Key {
                    harness: "claude".into(),
                    harness_session: "thread-1".into(),
                    agent: None,
                },
                target: Target::Key("k0".into()),
                mode: Mode::Certified,
                level: None,
            },
        )
        .response
    };
    assert_eq!(code(&by_key(BOB)), "ok");
    assert_eq!(code(&by_key(OWNER_AGENT)), "ok");
    drop(s);
    // After a restart (session events are durable before their replies) the
    // keys still resolve per principal.
    let s = service(&disk.crash(TailFate::Torn, 1));
    assert_eq!(attach(&s, OWNER_AGENT, "thread-1"), (a1, false));
    assert_eq!(attach(&s, BOB, "thread-1"), (b1, false));
    assert!(attach(&s, CAROL, "thread-1").1);
}

/// T99: a handoff across principals names its recipient; listing and accept
/// show only handoffs addressed to the caller's principal; the accept
/// reports `dropped_unauthorized`; addressing survives a restart.
#[test]
fn t99_handoffs_are_addressed_to_a_principal() {
    let disk = SimDisk::new();
    let s = service(&disk);
    records(&s, 2);
    let alice = start(&s, OWNER_AGENT);
    assert_eq!(code(&read(&s, OWNER_AGENT, alice, "k0")), "ok");
    assert_eq!(code(&read(&s, OWNER_AGENT, alice, "k1")), "ok");
    assert_eq!(
        code(&begin(&s, OWNER_AGENT, alice, Some("nobody"))),
        "InvalidArgument"
    );
    let to_bob = begun(begin(&s, OWNER_AGENT, alice, Some("bob")));
    let unnamed = begun(begin(&s, OWNER_AGENT, alice, None));
    let to_carol = begun(begin(&s, OWNER_AGENT, alice, Some("carol")));
    // Naming one's own principal is the unnamed handoff.
    let to_self = begun(begin(&s, OWNER_AGENT, alice, Some("alice")));
    let bob = start(&s, BOB);
    let carol = start(&s, CAROL);
    let alice2 = start(&s, OWNER_AGENT);
    assert_eq!(listed(&s, BOB, bob), vec![to_bob]);
    assert_eq!(listed(&s, CAROL, carol), vec![to_carol]);
    let mut own = listed(&s, OWNER_AGENT, alice2);
    own.sort();
    let mut expect = vec![unnamed, to_self];
    expect.sort();
    assert_eq!(own, expect);
    // Not addressed to the caller: it does not exist for it.
    assert_eq!(code(&accept(&s, CAROL, carol, to_bob)), "UnknownRef");
    assert_eq!(code(&accept(&s, BOB, bob, unnamed)), "UnknownRef");
    match accept(&s, BOB, bob, to_bob) {
        Response::HandoffAccepted {
            validation,
            copied,
            dropped_unauthorized,
            ..
        } => {
            assert_eq!(validation, Validation::Valid);
            assert_eq!((copied, dropped_unauthorized), (2, 0));
        }
        other => panic!("{other:?}"),
    }
    // The owner cannot take Bob's handoff either.
    assert_eq!(code(&accept(&s, OWNER_AGENT, alice2, to_bob)), "UnknownRef");
    drop(s);
    let s = service(&disk.crash(TailFate::Torn, 1));
    let carol = start(&s, CAROL);
    let bob = start(&s, BOB);
    assert_eq!(listed(&s, CAROL, carol), vec![to_carol]);
    assert_eq!(listed(&s, BOB, bob), Vec::<[u8; 16]>::new());
    assert!(matches!(
        accept(&s, CAROL, carol, to_carol),
        Response::HandoffAccepted { .. }
    ));
    // A non-owner's unnamed handoff is for its own principal only.
    let bob_reads = start(&s, BOB);
    assert_eq!(code(&read(&s, BOB, bob_reads, "k0")), "ok");
    let bobs = begun(begin(&s, BOB, bob_reads, None));
    let alice = start(&s, OWNER_AGENT);
    assert!(!listed(&s, OWNER_AGENT, alice).contains(&bobs));
    assert_eq!(listed(&s, BOB, bob), vec![bobs]);
}

// ----------------------------------------------------------------- daemon

fn base(name: &str) -> PathBuf {
    let d = Path::new("/tmp")
        .canonicalize()
        .unwrap()
        .join(format!("ctx-p9-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    chmod(&d, 0o711);
    d
}
fn chmod(p: &Path, m: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
}
fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}
/// The daemon with a peer-uid hook reporting `uid` for new connections.
fn daemon(dir: &Path) -> (Daemon, Arc<AtomicU32>) {
    fsguard::private_umask();
    let uid = Arc::new(AtomicU32::new(fsguard::current_uid()));
    let u = uid.clone();
    let options = Options {
        peer_uid: Some(PeerUidHook(Arc::new(move |_| {
            Some(u.load(Ordering::SeqCst))
        }))),
        ..Options::default()
    };
    (Daemon::start(dir, &options).unwrap(), uid)
}
fn refused(path: &Path) -> bool {
    match Client::connect(path) {
        Err(_) => true,
        Ok(mut c) => c.call(&Request::BranchList).is_err(),
    }
}

/// T98: the layout and modes, and the endpoint record.
#[test]
fn t98_socket_layout_modes_and_endpoint() {
    let dir = base("layout");
    let (d, _) = daemon(&dir);
    let run = dir.join("run");
    assert_eq!(
        d.socket(Channel::Agent),
        run.join("shared").join("agent.sock")
    );
    assert_eq!(
        d.socket(Channel::Observer),
        run.join("private").join("observer.sock")
    );
    assert_eq!(
        d.socket(Channel::Admin),
        run.join("private").join("admin.sock")
    );
    assert_eq!(mode(&run), 0o711);
    assert_eq!(mode(&run.join("shared")), 0o711);
    assert_eq!(mode(&run.join("private")), 0o700);
    for c in [Channel::Agent, Channel::Observer, Channel::Admin] {
        assert_eq!(mode(d.socket(c)), 0o600, "{c:?}");
    }
    let endpoint = run.join("shared").join(lease::ENDPOINT);
    assert_eq!(mode(&endpoint), 0o644);
    assert_eq!(
        lease::Endpoint::parse(&std::fs::read_to_string(&endpoint).unwrap()),
        Some(lease::Endpoint {
            protocol: PROTOCOL_VERSION,
            socket: "agent.sock".into(),
            owner_uid: fsguard::current_uid(),
        })
    );
    // Every entry the daemon wrote is private, SQLite's own files included,
    // and carries no ACL.
    let mut c = Client::connect(d.socket(Channel::Admin)).unwrap();
    let r = c
        .call(&Request::Raw {
            blobs: vec![b"x".to_vec()],
            batch: Batch::new(vec![Op::CreateLogical {
                key: "k".into(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: h(b"x"),
                adopt_as_base: true,
            }]),
        })
        .unwrap();
    assert!(matches!(r.response, Response::Committed { .. }));
    assert_eq!(fsguard::walk(&dir, fsguard::current_uid(), 16), vec![]);
    assert!(
        std::fs::read_dir(&dir).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with("-wal")),
        "a SQLite WAL file was checked"
    );
    // The store owner may connect through the public endpoint too.
    let mut shared = Client::connect_shared(&dir, None).unwrap();
    assert!(shared.call(&Request::BranchList).is_ok());
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

/// T98: unmapped uids are refused; a mapped uid gets its principal on the
/// agent socket only, the non-owner allowlist, and `AccessRevoked` once
/// unmapped.
#[test]
fn t98_mapped_and_unmapped_uids_and_revocation() {
    let dir = base("map");
    let (d, uid) = daemon(&dir);
    let me = fsguard::current_uid();
    uid.store(BOB_UID, Ordering::SeqCst);
    assert!(refused(d.socket(Channel::Agent)), "unmapped uid");
    d.set_principals(map()).unwrap();
    assert_eq!(mode(d.socket(Channel::Agent)), 0o666);
    for c in [Channel::Observer, Channel::Admin] {
        assert!(refused(d.socket(c)), "{c:?} refuses a mapped non-owner");
    }
    let mut bob = Client::connect(d.socket(Channel::Agent)).unwrap();
    let r = bob
        .call(&Request::SessionStart {
            harness: "mcp".into(),
            repo: "r".into(),
            head_sha: "h".into(),
            parent: None,
            branch: None,
            git_branch: None,
        })
        .unwrap();
    assert!(matches!(r.response, Response::Session { .. }), "{r:?}");
    let r = bob.call(&Request::Status).unwrap();
    assert_eq!(code(&r.response), "ChannelRestricted");
    // The owner still has every socket.
    uid.store(me, Ordering::SeqCst);
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    assert!(matches!(
        admin.call(&Request::Status).unwrap().response,
        Response::Status { .. }
    ));
    // Revocation: the open connection gets AccessRevoked, then closes.
    d.set_principals(PrincipalMap::default()).unwrap();
    assert_eq!(mode(d.socket(Channel::Agent)), 0o600);
    let r = bob.call(&Request::BranchList).unwrap();
    assert_eq!(code(&r.response), "AccessRevoked");
    assert!(bob.call(&Request::BranchList).is_err());
    uid.store(BOB_UID, Ordering::SeqCst);
    assert!(refused(d.socket(Channel::Agent)));
    // Remapped to another principal id: the old connection is revoked too.
    d.set_principals(map()).unwrap();
    let mut bob = Client::connect(d.socket(Channel::Agent)).unwrap();
    assert!(bob.call(&Request::BranchList).is_ok());
    d.set_principals(
        PrincipalMap::new(None, me, [("bob".to_owned(), BOB_UID, PrincipalId(18))]).unwrap(),
    )
    .unwrap();
    let r = bob.call(&Request::BranchList).unwrap();
    assert_eq!(code(&r.response), "AccessRevoked");
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

/// T98: multi-user mode is refused while the data directory breaks the
/// file-system rules, naming the entry; `fix_modes` repairs modes and ACLs.
#[test]
fn t98_multi_user_mode_requires_private_entries_and_safe_ancestors() {
    let dir = base("modes");
    let (d, _) = daemon(&dir);
    let loose = dir.join("loose");
    std::fs::write(&loose, "x").unwrap();
    chmod(&loose, 0o644);
    let v = d.set_principals(map()).unwrap_err();
    assert!(
        v.iter()
            .any(|v| matches!(v, fsguard::Violation::Mode { path, .. } if *path == loose)),
        "{v:?}"
    );
    assert_eq!(mode(d.socket(Channel::Agent)), 0o600, "not opened");
    #[cfg(target_os = "macos")]
    {
        // An inheritable ACE on a directory inside <data>: refused, and so is
        // the entry it is copied onto.
        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let ok = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read,file_inherit,directory_inherit"])
            .arg(&sub)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        std::fs::write(sub.join("f"), "x").unwrap();
        let v = d.set_principals(map()).unwrap_err();
        let acl: Vec<_> = v
            .iter()
            .filter_map(|v| match v {
                fsguard::Violation::Acl { path } => Some(path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(acl, vec![sub.clone(), sub.join("f")], "{v:?}");
    }
    let (fixed, foreign) = fsguard::fix_modes(&dir, fsguard::current_uid()).unwrap();
    assert!(fixed >= 1 && foreign.is_empty());
    d.set_principals(map()).unwrap();
    assert_eq!(mode(d.socket(Channel::Agent)), 0o666);
    d.set_principals(PrincipalMap::default()).unwrap();
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);

    // A world-writable, non-sticky ancestor: refused, and a client refuses
    // to trust a server there.
    let parent = base("anc");
    chmod(&parent, 0o777);
    let dir = parent.join("data");
    std::fs::create_dir(&dir).unwrap();
    chmod(&dir, 0o711);
    let (d, _) = daemon(&dir);
    let v = d.set_principals(map()).unwrap_err();
    assert!(
        v.iter().any(
            |v| matches!(v, fsguard::Violation::AncestorWritable { path, .. } if *path == parent)
        ),
        "{v:?}"
    );
    let e = Client::connect_shared(&dir, None).err().unwrap();
    assert!(e.starts_with("ServerNotOwner"), "{e}");
    drop(d);
    let _ = std::fs::remove_dir_all(&parent);

    // The long-path fallback puts the sockets in the per-user temp
    // directory: multi-user mode is refused.
    let long = base("long");
    let dir = long.join("d".repeat(90));
    std::fs::create_dir(&dir).unwrap();
    chmod(&dir, 0o711);
    let (d, _) = daemon(&dir);
    assert!(!d.socket(Channel::Agent).starts_with(&dir));
    let v = d.set_principals(map()).unwrap_err();
    assert!(matches!(
        v.as_slice(),
        [fsguard::Violation::SocketsOutsideData { .. }]
    ));
    drop(d);
    let _ = std::fs::remove_dir_all(&long);
}

/// T98: a client checks the server before sending anything: the endpoint
/// must name the data directory's owner.
#[test]
fn t98_clients_refuse_a_server_that_is_not_the_owner() {
    let dir = base("client");
    let (d, _) = daemon(&dir);
    assert!(Client::connect_shared(&dir, None).is_ok());
    let endpoint = dir.join("run").join("shared").join(lease::ENDPOINT);
    let text = std::fs::read_to_string(&endpoint).unwrap();
    let mut forged = lease::Endpoint::parse(&text).unwrap();
    forged.owner_uid += 1;
    lease::write_endpoint(&endpoint, &forged).unwrap();
    let e = Client::connect_shared(&dir, None).err().unwrap();
    assert!(e.starts_with("ServerNotOwner"), "{e}");
    std::fs::write(&endpoint, "protocol=7\nsocket=../../x\nowner_uid=0\n").unwrap();
    let e = Client::connect_shared(&dir, None).err().unwrap();
    assert!(e.starts_with("ServerNotOwner"), "{e}");
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}
