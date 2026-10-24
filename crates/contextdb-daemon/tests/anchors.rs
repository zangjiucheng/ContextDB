//! P5a service integration with real Git worktrees and simulated durable store.
use contextdb_anchors::{AnchorState, Repo, capture::OBSERVER_TIMEOUT, prepare};
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const OBSERVER: Identity = Identity {
    channel: Channel::Observer,
    principal: PrincipalId(0),
};
const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(0),
};
static TIMED_FIXTURE: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct Fixture {
    _serial: std::sync::MutexGuard<'static, ()>,
    root: PathBuf,
    data: PathBuf,
    repo: Repo,
    disk: SimDisk,
}
impl Fixture {
    fn new() -> Self {
        // Serialised: parallel Git startups would contend for the sample
        // deadline (2 s here; the 40 ms budget is measured by anchor_bench).
        let serial = TIMED_FIXTURE.lock().unwrap_or_else(|p| p.into_inner());
        let root = std::env::temp_dir().join(format!(
            "ctx-p5a-service-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let data = root.join(".contextdb");
        std::fs::create_dir_all(&data).unwrap();
        assert!(
            Command::new("git")
                .current_dir(&root)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        let repo = Repo::open(&root, Instant::now() + OBSERVER_TIMEOUT).unwrap();
        std::fs::write(
            data.join("anchors.routes"),
            vec![(repo.scope(), repo.root.to_str().unwrap().to_owned())].canonical_bytes(),
        )
        .unwrap();
        Self {
            _serial: serial,
            root,
            data,
            repo,
            disk: SimDisk::new(),
        }
    }
    fn service(&self) -> Service<SimDisk> {
        self.service_on(&self.disk)
    }
    fn service_on(&self, disk: &SimDisk) -> Service<SimDisk> {
        let (store, _) =
            Store::open(disk.clone(), Path::new("/db"), &OpenOptions::default()).unwrap();
        let (log, events, _) =
            SessionLog::open(disk.clone(), Path::new("/db"), false, 1 << 20).unwrap();
        let service = Service::new(actor::spawn(store).unwrap(), log, events).unwrap();
        service.set_anchor_routes(self.data.join("anchors.routes"));
        service.set_anchor_live_timeout(std::time::Duration::from_secs(2));
        service
    }
    fn scan(&self, s: &Service<SimDisk>, added: &[&str]) {
        let response = self.try_scan(s, added);
        assert!(
            matches!(response, None | Some(Response::Committed { .. })),
            "{response:?}"
        );
    }
    /// One observer pass; `None` when nothing changed.
    fn try_scan(&self, s: &Service<SimDisk>, added: &[&str]) -> Option<Response> {
        let prepared = self.prepare(s, added);
        (!prepared.ops.is_empty()).then(|| {
            s.handle(
                OBSERVER,
                Request::Raw {
                    blobs: prepared.blobs,
                    batch: Batch::new(prepared.ops),
                },
            )
            .response
        })
    }
    /// Load the committed anchor state, capture and classify (no submit).
    fn prepare(&self, s: &Service<SimDisk>, added: &[&str]) -> contextdb_anchors::Prepared {
        let Response::AnchorState {
            watermark, anchors, ..
        } = s
            .handle(
                OBSERVER,
                Request::AnchorState {
                    scope: self.repo.scope(),
                    branch: None,
                },
            )
            .response
        else {
            panic!()
        };
        let mut prior = BTreeMap::new();
        for a in anchors {
            let d = contextdb_anchors::Descriptor::from_bytes(&a.descriptor).unwrap();
            prior.insert(
                d.path,
                AnchorState {
                    logical: a.logical,
                    head: a.head,
                    content: a.content,
                    oid: d.oid,
                    base: a.base,
                    breaks: a.breaks,
                },
            );
        }
        let paths = prior
            .keys()
            .cloned()
            .chain(added.iter().map(|p| p.to_string()))
            .collect::<BTreeSet<_>>();
        let captured = self
            .repo
            .capture(&paths, Instant::now() + OBSERVER_TIMEOUT)
            .unwrap();
        prepare(
            &self.repo.id,
            &captured.entries,
            &prior,
            watermark,
            captured.position,
        )
        .unwrap()
    }
    fn start(&self, s: &Service<SimDisk>) -> SessionId {
        let Response::Session { session } = s
            .handle(
                AGENT,
                Request::SessionStart {
                    harness: "test".into(),
                    repo: self.repo.root.to_str().unwrap().into(),
                    head_sha: String::new(),
                    parent: None,
                    branch: None,
                    git_branch: None,
                },
            )
            .response
        else {
            panic!()
        };
        session
    }
    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.root.join(path), text).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn raw(s: &Service<SimDisk>, ops: Vec<Op>) {
    let body = b"decision".to_vec();
    let response = s
        .handle(
            ADMIN,
            Request::Raw {
                blobs: vec![body],
                batch: Batch::new(ops),
            },
        )
        .response;
    assert!(
        matches!(response, Response::Committed { .. }),
        "{response:?}"
    );
}
fn roots(s: &Service<SimDisk>) {
    let hash = *blake3::hash(b"decision").as_bytes();
    raw(
        s,
        vec![
            Op::CreateLogical {
                key: "root-a".into(),
                kind: RecordKind::Decision,
                scope_lane: "test".into(),
                content: hash,
                adopt_as_base: false,
            },
            Op::AddRule {
                conclusion: RevRef::New(0),
                incidences: vec![(RevisionId(0).into(), EdgeKind::Cur)],
                enabled: true,
            },
            Op::CreateLogical {
                key: "root-b".into(),
                kind: RecordKind::Decision,
                scope_lane: "test".into(),
                content: hash,
                adopt_as_base: false,
            },
            Op::AddRule {
                conclusion: RevRef::New(1),
                incidences: vec![(RevisionId(1).into(), EdgeKind::Cur)],
                enabled: true,
            },
        ],
    );
}
fn read(s: &Service<SimDisk>, session: SessionId, key: &str) -> Response {
    s.handle(
        AGENT,
        Request::Read {
            binding: Binding::root(session),
            target: Target::Key(key.into()),
            mode: Mode::Certified,
        },
    )
    .response
}
fn check(s: &Service<SimDisk>, session: SessionId, path: Option<&str>, strict: bool) -> Validation {
    let response = s
        .handle(
            AGENT,
            Request::Check {
                binding: Binding::root(session),
                cert_id: None,
                live_pos: vec![],
                action: path.map(|p| Action {
                    tool: ToolClass::Edit,
                    paths: vec![p.into()],
                    command: None,
                }),
                mode: None,
                harness_version: "test".into(),
                strict,
            },
        )
        .response;
    if let Response::Check {
        cert, validation, ..
    } = &response
    {
        if let Some(c) = cert {
            assert_eq!(c.cert_id, *blake3::hash(&c.body_bytes()).as_bytes());
        }
        if let Validation::ValidRecertified { new_cert, .. } = validation {
            assert_eq!(
                new_cert.cert_id,
                *blake3::hash(&new_cert.body_bytes()).as_bytes()
            );
        }
    }
    let Response::Check { validation, .. } = response else {
        panic!("{response:?}")
    };
    validation
}
#[test]
fn t46c_indirect_path_filter_preserves_full_read_set_and_aliases_fall_back() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let session = f.start(&s);
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    assert!(matches!(read(&s, session, "root-b"), Response::Read { .. }));
    assert!(matches!(
        check(&s, session, Some("a.rs"), false),
        Validation::Valid
    ));
    f.write("b.rs", "changed");
    f.scan(&s, &[]);
    assert!(matches!(
        check(&s, session, Some("a.rs"), false),
        Validation::Valid
    ));
    assert!(matches!(
        check(&s, session, None, false),
        Validation::Invalid { .. }
    ));
    // Missing target/uppercase alias/hardlink/parent symlink cannot exclude B.
    for path in ["missing.rs", "A.rs"] {
        assert!(matches!(
            check(&s, session, Some(path), false),
            Validation::Invalid { .. }
        ));
    }
    std::fs::hard_link(f.root.join("a.rs"), f.root.join("hard.rs")).unwrap();
    assert!(matches!(
        check(&s, session, Some("hard.rs"), false),
        Validation::Invalid { .. }
    ));
    std::os::unix::fs::symlink(".", f.root.join("alias")).unwrap();
    assert!(matches!(
        check(&s, session, Some("alias/a.rs"), false),
        Validation::Invalid { .. }
    ));
    s.handle(ADMIN, Request::Shutdown);
}
#[test]
fn t44_freshness_is_sampled_for_read_recall_check_and_client_cannot_override_failure() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let session = f.start(&s);
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    f.write("a.rs", "not observed");
    assert!(matches!(read(&s,session,"root-a"),Response::Error {code,..} if code=="NotCertified"));
    assert!(matches!(
        check(&s, session, Some("a.rs"), false),
        Validation::Unknown { .. }
    ));
    let response = s
        .handle(
            AGENT,
            Request::Recall {
                binding: Binding::root(session),
                query: "root-a".into(),
                scope: String::new(),
                mode: Mode::Certified,
                limit: 10,
            },
        )
        .response;
    assert!(
        matches!(response,Response::Recall {items,withheld,..} if items.is_empty() && withheld.len()==1)
    );
    std::fs::remove_file(f.data.join("anchors.routes")).unwrap();
    let watermark = s
        .actor
        .snapshot()
        .db()
        .inputs()
        .watermarks
        .into_iter()
        .collect();
    let response = s
        .handle(
            AGENT,
            Request::Check {
                binding: Binding::root(session),
                cert_id: None,
                live_pos: watermark,
                action: None,
                mode: None,
                harness_version: String::new(),
                strict: false,
            },
        )
        .response;
    assert!(matches!(
        response,
        Response::Check {
            validation: Validation::Unknown { .. },
            ..
        }
    ));
    s.handle(ADMIN, Request::Shutdown);
}
#[test]
fn filtered_strict_proof_keeps_aba_window_across_restart_until_explicit_reread() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let session = f.start(&s);
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    f.write("a.rs", "v2");
    f.scan(&s, &[]);
    f.write("a.rs", "a");
    f.scan(&s, &[]);
    assert!(matches!(
        check(&s, session, Some("a.rs"), true),
        Validation::Invalid {
            reason: InvalidReason::StrictChange,
            ..
        }
    ));
    s.handle(ADMIN, Request::Shutdown);
    drop(s);
    let s = f.service();
    assert!(matches!(
        check(&s, session, Some("a.rs"), true),
        Validation::Invalid {
            reason: InvalidReason::StrictChange,
            ..
        }
    ));
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    assert!(matches!(
        check(&s, session, Some("a.rs"), true),
        Validation::Valid
    ));
    s.handle(ADMIN, Request::Shutdown);
}

#[test]
fn failed_full_strict_check_reports_supersession_once_without_filtered_consumption() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let session = f.start(&s);
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    let Response::Read { item: old, .. } = read(&s, session, "root-b") else {
        panic!()
    };
    let body = b"new decision".to_vec();
    let hash = *blake3::hash(&body).as_bytes();
    let response = s
        .handle(
            ADMIN,
            Request::Raw {
                blobs: vec![body],
                batch: Batch::new(vec![
                    Op::AddRevision {
                        logical: old.logical.into(),
                        content: hash,
                        adopt_as_base: true,
                    },
                    Op::SetHead {
                        logical: old.logical.into(),
                        expected: old.rev.into(),
                        new: RevRef::New(0),
                    },
                ]),
            },
        )
        .response;
    assert!(
        matches!(response, Response::Committed { .. }),
        "{response:?}"
    );
    let Response::Read { item: new, .. } = read(&s, session, "root-b") else {
        panic!()
    };
    assert_ne!(old.rev, new.rev);
    f.write("a.rs", "changed");
    f.scan(&s, &[]);
    f.write("a.rs", "a");
    f.scan(&s, &[]);
    let request = |action| Request::Check {
        binding: Binding::root(session),
        cert_id: None,
        live_pos: vec![],
        action,
        mode: None,
        harness_version: "test".into(),
        strict: true,
    };
    let filtered = s
        .handle(
            AGENT,
            request(Some(Action {
                tool: ToolClass::Edit,
                paths: vec!["a.rs".into()],
                command: None,
            })),
        )
        .response;
    assert!(
        matches!(filtered, Response::Check { validation: Validation::Invalid { reason: InvalidReason::StrictChange, .. }, superseded, .. } if superseded.is_empty())
    );
    for expected in [1, 0] {
        let response = s.handle(AGENT, request(None)).response;
        assert!(
            matches!(&response, Response::Check { validation: Validation::Invalid { reason: InvalidReason::StrictChange, .. }, superseded, .. } if superseded.len() == expected),
            "{response:?}"
        );
    }
    s.handle(ADMIN, Request::Shutdown);
}

#[test]
fn range_is_bound_to_exact_manifest_and_head_and_excludes_ref_only_roots() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    assert!(
        Command::new("git")
            .current_dir(&f.root)
            .args(["add", "a.rs", "b.rs"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .current_dir(&f.root)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "fixture"
            ])
            .status()
            .unwrap()
            .success()
    );
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let hash = *blake3::hash(b"decision").as_bytes();
    raw(
        &s,
        vec![
            Op::CreateLogical {
                key: "ref-only".into(),
                kind: RecordKind::Decision,
                scope_lane: "test".into(),
                content: hash,
                adopt_as_base: false,
            },
            Op::AddRule {
                conclusion: RevRef::New(0),
                incidences: vec![
                    (RevisionId(0).into(), EdgeKind::Ref),
                    (RevisionId(1).into(), EdgeKind::Cur),
                ],
                enabled: false,
            },
        ],
    );
    let c = f
        .repo
        .capture(
            &BTreeSet::from(["a.rs".into(), "b.rs".into()]),
            Instant::now() + OBSERVER_TIMEOUT,
        )
        .unwrap();
    let request = Request::CheckAnchors {
        scope: f.repo.scope(),
        paths: vec!["a.rs".into()],
        expected_head: c.head,
        expected_position: c.position,
    };
    assert!(
        matches!(s.handle(ADMIN,request.clone()).response,Response::Check {validation:Validation::Valid,cert:Some(c),..} if c.strict_no_aba)
    );
    // CLI verified T, then the watcher publishes U before the daemon checks.
    f.write("a.rs", "U");
    f.scan(&s, &[]);
    assert!(matches!(
        s.handle(ADMIN, request).response,
        Response::Check {
            validation: Validation::Unknown { .. },
            ..
        }
    ));
    s.handle(ADMIN, Request::Shutdown);
    drop(s);
    let (_, events, _) =
        SessionLog::open(f.disk.clone(), Path::new("/db"), false, 1 << 20).unwrap();
    assert!(events.iter().any(|e|matches!(e,contextdb_store::session::SessionEvent::CertIssued {cert} if cert.strict_no_aba && !cert.code_commit.is_empty())));
}
#[test]
fn aggregate_explicit_certificate_retains_read_epoch_and_complete_diffs() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let session = f.start(&s);
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    assert!(matches!(read(&s, session, "root-b"), Response::Read { .. }));
    raw(
        &s,
        vec![Op::CreateLogical {
            key: "unrelated".into(),
            kind: RecordKind::Fact,
            scope_lane: "test".into(),
            content: *blake3::hash(b"decision").as_bytes(),
            adopt_as_base: true,
        }],
    );
    let response = s
        .handle(
            AGENT,
            Request::Check {
                binding: Binding::root(session),
                cert_id: None,
                live_pos: vec![],
                action: None,
                mode: None,
                harness_version: String::new(),
                strict: false,
            },
        )
        .response;
    let Response::Check {
        cert: Some(cert),
        updates_pending: 0,
        ..
    } = response
    else {
        panic!("{response:?}")
    };
    let response = s
        .handle(
            AGENT,
            Request::Check {
                binding: Binding::root(session),
                cert_id: Some(cert.cert_id),
                live_pos: vec![],
                action: Some(Action {
                    tool: ToolClass::Edit,
                    paths: vec!["a.rs".into()],
                    command: None,
                }),
                mode: None,
                harness_version: String::new(),
                strict: true,
            },
        )
        .response;
    assert!(
        matches!(
            response,
            Response::Check {
                validation: Validation::Valid,
                ..
            }
        ),
        "{response:?}"
    );
    let response = s
        .handle(
            AGENT,
            Request::Updates {
                binding: Binding::root(session),
                ack: None,
                max_items: 10,
            },
        )
        .response;
    assert!(matches!(response,Response::Updates {items,..} if items.is_empty()));
    f.write("a.rs", "v2");
    f.write("b.rs", "v2");
    f.scan(&s, &[]);
    assert!(
        matches!(check(&s,session,None,false),Validation::Invalid {diffs,..} if diffs.len()==2)
    );
    s.handle(ADMIN, Request::Shutdown);
}
#[test]
fn child_check_uses_the_owner_of_inherited_root_proofs() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let session = f.start(&s);
    let binding = Binding::Session {
        session,
        agent: Some("worker".into()),
    };
    assert!(matches!(
        s.handle(
            AGENT,
            Request::Read {
                binding: binding.clone(),
                target: Target::Key("root-a".into()),
                mode: Mode::Certified
            }
        )
        .response,
        Response::Read { .. }
    ));
    let response = s
        .handle(
            AGENT,
            Request::Check {
                binding: Binding::root(session),
                cert_id: None,
                live_pos: vec![],
                action: Some(Action {
                    tool: ToolClass::Edit,
                    paths: vec!["a.rs".into()],
                    command: None,
                }),
                mode: None,
                harness_version: String::new(),
                strict: true,
            },
        )
        .response;
    assert!(
        matches!(
            response,
            Response::Check {
                validation: Validation::Valid,
                ..
            }
        ),
        "{response:?}"
    );
    s.handle(ADMIN, Request::Shutdown);
}

#[test]
fn legacy_missing_original_proof_cannot_be_filtered_out_by_current_support() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let session = f.start(&s);
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    s.handle(ADMIN, Request::Shutdown);
    drop(s);
    // A v1 session has its durable ledger, but no P5a RootProof events.
    let (store, _) =
        Store::open(f.disk.clone(), Path::new("/db"), &OpenOptions::default()).unwrap();
    let (log, events, _) =
        SessionLog::open(f.disk.clone(), Path::new("/db"), false, 1 << 20).unwrap();
    let events = events
        .into_iter()
        .filter(|e| !matches!(e, contextdb_store::session::SessionEvent::RootProof { .. }))
        .collect();
    let s = Service::new(actor::spawn(store).unwrap(), log, events).unwrap();
    s.set_anchor_routes(f.data.join("anchors.routes"));
    // Same root now derives from B, so its original A support is unknowable.
    raw(
        &s,
        vec![
            Op::SetRuleEnabled {
                rule: RuleId(0).into(),
                enabled: false,
            },
            Op::AddRule {
                conclusion: RevisionId(2).into(),
                incidences: vec![(RevisionId(1).into(), EdgeKind::Cur)],
                enabled: true,
            },
        ],
    );
    assert!(
        matches!(check(&s,session,Some("a.rs"),false),Validation::Unknown {lag} if lag==["MissingBaseline"])
    );
    assert!(
        matches!(check(&s,session,None,true),Validation::Unknown {lag} if lag==["MissingBaseline"])
    );
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    assert!(matches!(
        check(&s, session, Some("b.rs"), true),
        Validation::Valid
    ));
    s.handle(ADMIN, Request::Shutdown);
}

#[test]
fn successful_full_strict_check_is_durable_and_sticky_after_restart() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let session = f.start(&s);
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    assert!(matches!(check(&s, session, None, true), Validation::Valid));
    s.handle(ADMIN, Request::Shutdown);
    drop(s);
    let s = f.service();
    f.write("a.rs", "v2");
    f.scan(&s, &[]);
    f.write("a.rs", "a");
    f.scan(&s, &[]);
    assert!(matches!(
        check(&s, session, None, false),
        Validation::Invalid {
            reason: InvalidReason::StrictChange,
            ..
        }
    ));
    s.handle(ADMIN, Request::Shutdown);
}

#[test]
fn reread_replaces_baseline_without_losing_strict_mode_on_restart() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let session = f.start(&s);
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    assert!(matches!(check(&s, session, None, true), Validation::Valid));
    f.write("a.rs", "v2");
    f.scan(&s, &[]);
    f.write("a.rs", "a");
    f.scan(&s, &[]);
    assert!(matches!(
        check(&s, session, None, false),
        Validation::Invalid { .. }
    ));
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    s.handle(ADMIN, Request::Shutdown);
    drop(s);
    let s = f.service();
    let response = s
        .handle(
            AGENT,
            Request::Check {
                binding: Binding::root(session),
                cert_id: None,
                live_pos: vec![],
                action: None,
                mode: None,
                harness_version: String::new(),
                strict: false,
            },
        )
        .response;
    assert!(
        matches!(response,Response::Check {validation:Validation::Valid,cert:Some(c),..} if c.strict_no_aba)
    );
    s.handle(ADMIN, Request::Shutdown);
}

#[test]
fn unanchored_reads_remain_required_and_briefing_uses_fresh_evidence() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let session = f.start(&s);
    let response = s
        .handle(
            AGENT,
            Request::Briefing {
                binding: Some(Binding::root(session)),
                budget_chars: 4096,
            },
        )
        .response;
    assert!(
        matches!(response,Response::Briefing {pending:0,items,..} if items.iter().all(|i|i.summary==Summary::Certified))
    );
    raw(
        &s,
        vec![Op::CreateLogical {
            key: "plain".into(),
            kind: RecordKind::Fact,
            scope_lane: "test".into(),
            content: *blake3::hash(b"decision").as_bytes(),
            adopt_as_base: true,
        }],
    );
    assert!(matches!(read(&s, session, "plain"), Response::Read { .. }));
    raw(
        &s,
        vec![Op::RetractBase {
            rev: RevisionId(4).into(),
        }],
    );
    assert!(matches!(
        check(&s, session, Some("a.rs"), false),
        Validation::Invalid { .. }
    ));
    s.handle(ADMIN, Request::Shutdown);
}
fn anchor_state(f: &Fixture, s: &Service<SimDisk>) -> (Option<Vec<u8>>, Vec<ContentHash>) {
    let Response::AnchorState {
        watermark, anchors, ..
    } = s
        .handle(
            OBSERVER,
            Request::AnchorState {
                scope: f.repo.scope(),
                branch: None,
            },
        )
        .response
    else {
        panic!()
    };
    (watermark, anchors.into_iter().map(|a| a.content).collect())
}
/// G4: an observer batch interrupted at any store step (blob pack, op_log,
/// sync or rename), then a crash, leaves the committed anchor state wholly
/// before or wholly after it. A full scan after restart converges and the
/// dependent root is certified again; nothing partial is ever fresh.
#[test]
fn g4_interrupted_observer_batch_is_atomic_and_rescan_converges() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let base = {
        let s = f.service();
        f.scan(&s, &["a.rs", "b.rs"]);
        roots(&s);
        s.handle(ADMIN, Request::Shutdown);
        // A clean shutdown made everything durable: an independent copy.
        f.disk.crash(contextdb_store::TailFate::ZeroFill, 0)
    };
    let steps = [
        "append",
        "write",
        "sync",
        "sync_weak",
        "rename",
        "sync_dir",
        "sync_dir_weak",
    ];
    let mut interrupted = 0;
    for (seed, step) in steps.iter().enumerate() {
        let disk = base.crash(contextdb_store::TailFate::ZeroFill, 0);
        let old = {
            let s = f.service_on(&disk);
            let s_old = anchor_state(&f, &s);
            f.write("a.rs", &format!("edit {seed}"));
            disk.fail_next(step);
            let response = f.try_scan(&s, &[]);
            if !matches!(response, Some(Response::Committed { .. })) {
                interrupted += 1;
            }
            s.handle(ADMIN, Request::Shutdown);
            s_old
        };
        let live = f
            .repo
            .capture(
                &["a.rs".to_owned(), "b.rs".to_owned()].into(),
                Instant::now() + OBSERVER_TIMEOUT,
            )
            .unwrap();
        for fate in [
            contextdb_store::TailFate::Torn,
            contextdb_store::TailFate::ZeroFill,
            contextdb_store::TailFate::Garbage,
        ] {
            for crash_seed in 0..4 {
                let crashed = disk.crash(fate, crash_seed);
                let s = f.service_on(&crashed);
                let (watermark, heads) = anchor_state(&f, &s);
                if watermark == old.0 {
                    assert_eq!(heads, old.1, "{step}: old watermark with new heads");
                } else {
                    assert_eq!(watermark.as_deref(), Some(&live.position[..]), "{step}");
                    assert_ne!(heads, old.1, "{step}: new watermark with old heads");
                }
                f.scan(&s, &[]);
                assert_eq!(
                    anchor_state(&f, &s).0.as_deref(),
                    Some(&live.position[..]),
                    "{step}: rescan converges"
                );
                // root-b rests on the untouched b.rs; root-a's Cur premise
                // a.rs was superseded by the edit, so it is STALE. Each
                // restart samples cold (two Git processes) against the real
                // 40 ms budget; under load that is Unknown by design, so the
                // fresh read is retried while the caches warm.
                let session = f.start(&s);
                assert!(
                    (0..5).any(|_| matches!(read(&s, session, "root-b"), Response::Read { .. })),
                    "{step}"
                );
                assert!(
                    matches!(read(&s, session, "root-a"), Response::Error { code, .. } if code == "NotCertified"),
                    "{step}"
                );
                s.handle(ADMIN, Request::Shutdown);
            }
        }
    }
    // The harness must actually interrupt the observer, not only succeed.
    assert!(interrupted >= 3, "only {interrupted} steps interrupted");
}
/// G4: a corrupt CodeAnchor descriptor makes its whole scope Unknown for
/// reads and checks (never fresh), and the observer refuses to reconcile it.
#[test]
fn g4_corrupt_descriptor_makes_the_scope_unknown() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let session = f.start(&s);
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    let body = b"not a descriptor".to_vec();
    let response = s
        .handle(
            OBSERVER,
            Request::Raw {
                blobs: vec![body.clone()],
                batch: Batch::new(vec![Op::CreateLogical {
                    key: format!("{}/c.rs", f.repo.scope()),
                    kind: RecordKind::CodeAnchor,
                    scope_lane: f.repo.scope(),
                    content: *blake3::hash(&body).as_bytes(),
                    adopt_as_base: true,
                }]),
            },
        )
        .response;
    assert!(
        matches!(response, Response::Committed { .. }),
        "{response:?}"
    );
    let fresh = f.start(&s);
    assert!(
        matches!(read(&s, fresh, "root-a"), Response::Error { code, .. } if code == "NotCertified")
    );
    assert!(matches!(
        check(&s, session, None, false),
        Validation::Unknown { .. }
    ));
    let Response::AnchorState { anchors, .. } = s
        .handle(
            OBSERVER,
            Request::AnchorState {
                scope: f.repo.scope(),
                branch: None,
            },
        )
        .response
    else {
        panic!()
    };
    assert!(
        anchors
            .iter()
            .any(|a| contextdb_anchors::Descriptor::from_bytes(&a.descriptor).is_err())
    );
    s.handle(ADMIN, Request::Shutdown);
}
/// G4: concurrent wakeups for one repository race through the watermark/head
/// CAS. Losers fail with a conflict (the CLI then reloads and recaptures);
/// the edit is committed once and the scope converges to the live state.
#[test]
fn g4_concurrent_wakeups_commit_once_and_converge() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    // Edit, delete, restore: racing restores of a broken anchor must also
    // lose on the watermark CAS (review m7), not on ResolveArtifact.
    for round in 0..9 {
        if round % 3 == 1 {
            std::fs::remove_file(f.root.join("a.rs")).unwrap();
        } else {
            f.write("a.rs", &format!("round {round}"));
        }
        let epoch = s.actor.snapshot().epoch;
        let responses: Vec<_> = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| f.try_scan(&s, &[])))
                .collect();
            jobs.into_iter().map(|j| j.join().unwrap()).collect()
        });
        let committed = responses
            .iter()
            .filter(|r| matches!(r, Some(Response::Committed { .. })))
            .count();
        for r in &responses {
            assert!(
                match r {
                    None | Some(Response::Committed { .. }) => true,
                    Some(Response::Error { code, .. }) =>
                        ["HeadConflict", "WatermarkConflict"].contains(&code.as_str()),
                    _ => false,
                },
                "{r:?}"
            );
        }
        assert_eq!(committed, 1, "round {round}: {responses:?}");
        assert_eq!(s.actor.snapshot().epoch.0, epoch.0 + 1);
        assert_eq!(f.try_scan(&s, &[]), None, "converged");
    }
    s.handle(ADMIN, Request::Shutdown);
}
/// Review M1: a target inside a nested repository or linked worktree belongs
/// to another scope, so the bound-repo key cannot select its roots; the check
/// falls back to the full session certificate instead of excluding them.
#[test]
fn nested_repository_target_falls_back_to_full_check() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    f.write("b.rs", "b");
    let s = f.service();
    f.scan(&s, &["a.rs", "b.rs"]);
    roots(&s);
    let session = f.start(&s);
    assert!(matches!(read(&s, session, "root-a"), Response::Read { .. }));
    assert!(matches!(read(&s, session, "root-b"), Response::Read { .. }));
    f.write("b.rs", "changed");
    f.scan(&s, &[]);
    std::fs::create_dir_all(f.root.join("wt")).unwrap();
    assert!(
        Command::new("git")
            .current_dir(f.root.join("wt"))
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    f.write("wt/x.rs", "x");
    // A plain directory target is still filtered (only root-a is selected).
    std::fs::create_dir_all(f.root.join("plain")).unwrap();
    f.write("plain/x.rs", "x");
    assert!(matches!(
        check(&s, session, Some("plain/x.rs"), false),
        Validation::Empty | Validation::Valid
    ));
    assert!(matches!(
        check(&s, session, Some("wt/x.rs"), false),
        Validation::Invalid { .. }
    ));
    // A gitfile (linked worktree / submodule layout) counts as well.
    std::fs::create_dir_all(f.root.join("linked")).unwrap();
    f.write("linked/.git", "gitdir: /nowhere\n");
    f.write("linked/x.rs", "x");
    assert!(matches!(
        check(&s, session, Some("linked/x.rs"), false),
        Validation::Invalid { .. }
    ));
    s.handle(ADMIN, Request::Shutdown);
}
/// Review m7: two observers that captured the same restore of a broken
/// anchor race; the loser must fail on the watermark CAS (which the observer
/// retries after a reload), not on resolving the already-resolved break.
#[test]
fn racing_restore_of_a_broken_anchor_loses_on_the_watermark() {
    let f = Fixture::new();
    f.write("a.rs", "a");
    let s = f.service();
    f.scan(&s, &["a.rs"]);
    std::fs::remove_file(f.root.join("a.rs")).unwrap();
    f.scan(&s, &[]);
    f.write("a.rs", "a");
    let one = f.prepare(&s, &[]);
    let two = f.prepare(&s, &[]);
    let submit = |p: contextdb_anchors::Prepared| {
        s.handle(
            OBSERVER,
            Request::Raw {
                blobs: p.blobs,
                batch: Batch::new(p.ops),
            },
        )
        .response
    };
    assert!(matches!(submit(one), Response::Committed { .. }));
    let lost = submit(two);
    assert!(
        matches!(&lost, Response::Error { code, .. } if code == "WatermarkConflict"),
        "{lost:?}"
    );
    s.handle(ADMIN, Request::Shutdown);
}
/// ADR 0011 §9 end to end: the announce record gates live evidence. A failed
/// announce (sentinel), a deleted record, or a data dir not marked for
/// announcing appenders withhold certified reads; repair and restart restore
/// them; 2,000 outbox streams do not.
#[test]
fn announce_record_gates_live_evidence() {
    use contextdb_proto::outbox::Outbox;
    let f = Fixture::new();
    f.write("a.rs", "a");
    let s = f.service();
    f.scan(&s, &["a.rs"]);
    roots(&s);
    let session = f.start(&s);
    let certified =
        |s: &Service<SimDisk>| matches!(read(s, session, "root-a"), Response::Read { .. });
    assert!(certified(&s));
    let ob = Outbox::new(&f.data);
    let out = f.data.join("outbox");
    // Failed announce: Unknown until repaired (new epoch).
    std::fs::File::create(out.join("announce.failed")).unwrap();
    assert!(!certified(&s));
    ob.repair_announce().unwrap();
    assert!(certified(&s));
    // Deleted record: never read as zero; ctxd start recreates it.
    std::fs::remove_file(out.join("announce")).unwrap();
    assert!(!certified(&s));
    s.set_anchor_routes(f.data.join("anchors.routes"));
    assert!(certified(&s));
    // A data dir with streams and no marker (earlier hooks may append).
    std::fs::remove_file(out.join("appenders")).unwrap();
    for k in 0..2000u32 {
        let mut stream = [0u8; 16];
        stream[..4].copy_from_slice(&k.to_le_bytes());
        ob.append(&stream, None, Fact::StreamEnd).unwrap();
    }
    s.set_anchor_routes(f.data.join("anchors.routes"));
    assert!(!certified(&s), "streams exist and no announce-v1 marker");
    ob.mark_appenders().unwrap();
    // 2,000 streams cost nothing on the live path.
    s.set_anchor_live_timeout(contextdb_anchors::capture::LIVE_TIMEOUT);
    assert!((0..5).any(|_| certified(&s)));
    s.handle(ADMIN, Request::Shutdown);
}
