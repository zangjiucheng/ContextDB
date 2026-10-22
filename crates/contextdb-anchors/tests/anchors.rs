use contextdb_anchors::Observation;
use contextdb_anchors::{
    capture::{OBSERVER_TIMEOUT, blob_oid, git},
    descriptor::unhex,
    *,
};
use contextdb_core::{Db, IssueRequest, LogLink};
use contextdb_model::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new(format: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "ctx-p5a-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        assert!(
            Command::new("git")
                .current_dir(&p)
                .args(["init", "-q", &format!("--object-format={format}")])
                .status()
                .unwrap()
                .success()
        );
        Self(p)
    }
    fn repo(&self) -> Repo {
        Repo::open(&self.0, Instant::now() + OBSERVER_TIMEOUT).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn descriptor(repo: &str, path: &str, byte: u8) -> Descriptor {
    Descriptor {
        repo: repo.into(),
        path: path.into(),
        format: ObjectFormat::Sha1,
        oid: vec![byte; 20],
    }
}
#[test]
fn classifier_restores_identical_retracted_head_and_handles_broken_paths() {
    let repo = "a".repeat(32);
    let d = descriptor(&repo, "a.rs", 1);
    let state = AnchorState {
        logical: LogicalId(0),
        head: RevisionId(0),
        content: d.content(),
        oid: d.oid.clone(),
        base: false,
        breaks: vec![ArtifactId(0)],
    };
    let prior = BTreeMap::from([("a.rs".into(), state.clone())]);
    let restored = prepare(
        &repo,
        &BTreeMap::from([("a.rs".into(), Observation::Present(d))]),
        &prior,
        Some(vec![1]),
        vec![2],
    )
    .unwrap();
    assert!(restored.blobs.is_empty());
    assert_eq!(
        restored.ops,
        vec![
            Op::ObserverWatermark {
                scope: format!("code:{repo}"),
                expected: Some(vec![1]),
                position: vec![2]
            },
            Op::AssertBase {
                rev: RevisionId(0).into()
            },
            Op::ResolveArtifact {
                id: ArtifactId(0).into()
            },
        ]
    );
    let prior = BTreeMap::from([(
        "a.rs".into(),
        AnchorState {
            base: true,
            breaks: vec![],
            ..state
        },
    )]);
    let broken = prepare(
        &repo,
        &BTreeMap::from([("a.rs".into(), Observation::Broken("symlink".into()))]),
        &prior,
        Some(vec![1]),
        vec![2],
    )
    .unwrap();
    assert!(matches!(broken.ops[0], Op::ObserverWatermark { .. }));
    assert!(matches!(broken.ops[1], Op::RetractBase { .. }));
    assert!(matches!(
        broken.ops[2],
        Op::OpenArtifact {
            ty: ArtifactType::AnchorBroken,
            ..
        }
    ));
    assert!(
        std::str::from_utf8(&broken.blobs[0])
            .unwrap()
            .contains(":symlink;")
    );
}
fn prior(db: &Db, repo: &str) -> BTreeMap<String, AnchorState> {
    let i = db.inputs();
    let mut result = BTreeMap::new();
    for (l, logical) in i
        .logicals
        .iter()
        .enumerate()
        .filter(|(_, l)| l.kind == RecordKind::CodeAnchor)
    {
        let head = i.heads[l];
        let path = logical
            .key
            .strip_prefix(&format!("code:{repo}/"))
            .unwrap()
            .to_owned();
        result.insert(
            path,
            AnchorState {
                logical: LogicalId(l as u32),
                head,
                content: i.revisions[head.index()].content,
                oid: Vec::new(),
                base: i.base[head.index()],
                breaks: i
                    .artifacts
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| {
                        a.open
                            && a.ty == ArtifactType::AnchorBroken
                            && i.revisions[a.target.index()].logical == LogicalId(l as u32)
                    })
                    .map(|(id, _)| ArtifactId(id as u32))
                    .collect(),
            },
        );
    }
    result
}
fn observe(db: &mut Db, repo: &str, obs: Observation, pos: u8) -> Option<CommitResult> {
    let state = prior(db, repo);
    let scope = format!("code:{repo}");
    let p = prepare(
        repo,
        &BTreeMap::from([("a.rs".into(), obs)]),
        &state,
        db.watermark(&scope).map(<[u8]>::to_vec),
        vec![pos],
    )
    .unwrap();
    if p.ops.is_empty() {
        return None;
    }
    let mut batch = Batch::new(p.ops);
    batch.channel = Channel::Observer;
    let result = db.commit(&batch).unwrap();
    assert_eq!(db.derived(), &contextdb_oracle::jacobi(&db.inputs()));
    Some(result)
}
fn issue(db: &Db, root: u32, strict: bool) -> Certificate {
    db.issue_certificate(
        &IssueRequest {
            session_id: [1; 16],
            principal: PrincipalId(0),
            branch: MAIN,
            log_checksum: [0; 32],
            code_commit: String::new(),
            strict_no_aba: strict,
        },
        &[CertItem {
            logical: LogicalId(root),
            rev: db.head(LogicalId(root)).unwrap(),
            required: true,
            via: Via::Head,
            since_epoch: db.epoch(),
        }],
        &db.inputs().watermarks,
    )
    .unwrap()
}
#[test]
fn descriptor_canonical_bytes_bounds_and_git_oid_are_distinct_from_payload_hash() {
    let d = descriptor(&"a".repeat(32), "src/a.rs", 7);
    let bytes = d.canonical_bytes();
    assert_eq!(&bytes[..12], b"CTXANC01\x01\0\0\0");
    assert_eq!(Descriptor::from_bytes(&bytes).unwrap(), d);
    assert_eq!(d.content(), *blake3::hash(&bytes).as_bytes());
    let mut bad = bytes.clone();
    bad.push(0);
    assert_eq!(Descriptor::from_bytes(&bad), Err(WireError::TrailingBytes));
    for p in [
        "../x",
        "a/../x",
        "/a",
        "A.rs",
        "a//x",
        ".git/x",
        ".contextdb/db",
        "x@y",
        "a\\b",
    ] {
        assert!(validate_path(&d.repo, p).is_err(), "{p}");
    }
    let mut bad = d.clone();
    bad.oid.push(0);
    assert!(bad.validate().is_err());
    bad = d.clone();
    bad.repo = "z".repeat(32);
    assert!(bad.validate().is_err());
}
#[test]
fn raw_hashes_match_git_sha1_and_sha256_at_binary_crlf_and_block_boundaries() {
    for (alg, format) in [
        ("sha1", ObjectFormat::Sha1),
        ("sha256", ObjectFormat::Sha256),
    ] {
        let fixture = Fixture::new(alg);
        let repo = fixture.repo();
        assert_eq!(repo.format, format);
        for n in [
            0, 1, 9, 10, 55, 56, 63, 64, 65, 99, 100, 127, 128, 129, 1000, 16384,
        ] {
            let bytes: Vec<_> = (0..n).map(|k| ((k * 73) % 256) as u8).collect();
            let (ok, git_oid) = git(
                &fixture.0,
                &["hash-object", "--no-filters", "--stdin"],
                &bytes,
                128,
                Instant::now() + OBSERVER_TIMEOUT,
            )
            .unwrap();
            assert!(ok);
            let reference =
                unhex(std::str::from_utf8(&git_oid).unwrap().trim().as_bytes()).unwrap();
            assert_eq!(
                blob_oid(format, &bytes).unwrap(),
                reference,
                "{alg} len={n}"
            );
        }
        std::fs::write(
            fixture.0.join(".gitattributes"),
            "*.rs text eol=lf filter=must-not-run\n",
        )
        .unwrap();
        let bytes = b"a\r\n\0b\r\n";
        std::fs::write(fixture.0.join("a.rs"), bytes).unwrap();
        let cap = repo
            .capture(
                &BTreeSet::from(["a.rs".into()]),
                Instant::now() + OBSERVER_TIMEOUT,
            )
            .unwrap();
        let Observation::Present(d) = &cap.entries["a.rs"] else {
            panic!()
        };
        assert_eq!(d.oid, blob_oid(format, bytes).unwrap());
        assert!(cap.head.is_empty());
    }
}
#[test]
fn t42_t43_t46_cur_sem_pin_deletion_restore_old_head_and_exact_oracle_counters() {
    let repo = "a".repeat(32);
    let mut db = Db::default();
    let d = descriptor(&repo, "a.rs", 1);
    observe(&mut db, &repo, Observation::Present(d.clone()), 1);
    db.commit(&Batch::new(
        (1..=3)
            .map(|n| Op::CreateLogical {
                key: format!("root{n}"),
                kind: RecordKind::Decision,
                scope_lane: "test".into(),
                content: [n; 32],
                adopt_as_base: false,
            })
            .chain(
                [EdgeKind::Cur, EdgeKind::Sem, EdgeKind::Pin]
                    .into_iter()
                    .enumerate()
                    .map(|(j, kind)| Op::AddRule {
                        conclusion: RevisionId(j as u32 + 1).into(),
                        incidences: vec![(RevisionId(0).into(), kind)],
                        enabled: true,
                    }),
            )
            .collect(),
    ))
    .unwrap();
    let loose = issue(&db, 1, false);
    let strict = issue(&db, 1, true);
    let changed = descriptor(&repo, "a.rs", 2);
    let r = observe(&mut db, &repo, Observation::Present(changed), 2).unwrap();
    assert_eq!(r.allocations.new_revisions, vec![RevisionId(4)]);
    assert_eq!(
        db.status(RevisionId(1)).unwrap(),
        Summary::Stale(StaleReason::Unsupported)
    );
    assert_eq!(
        db.status(RevisionId(2)).unwrap(),
        Summary::Pending(PendingReason::Semantic)
    );
    assert_eq!(
        db.status(RevisionId(3)).unwrap(),
        Summary::Stale(StaleReason::SupersededPinnedPremise(RevisionId(0)))
    );
    let snapshot = db.derived();
    assert_eq!(snapshot.nodes[1].support, 0);
    assert_eq!(snapshot.unsat[0], 1);
    db.commit(&Batch::new(vec![Op::RecordValidation {
        rule: RuleId(1).into(),
        curvec_seen: vec![(LogicalId(0), RevisionId(4))],
        verdict: Verdict::Verified,
        validator: "human".into(),
    }]))
    .unwrap();
    assert_eq!(db.status(RevisionId(2)).unwrap(), Summary::Certified);
    observe(&mut db, &repo, Observation::Missing, 3);
    assert_eq!(db.inputs().artifacts.len(), 1);
    assert_eq!(
        db.status(RevisionId(2)).unwrap(),
        Summary::Stale(StaleReason::Unsupported)
    );
    assert!(observe(&mut db, &repo, Observation::Missing, 3).is_none());
    // Restore an older, non-current identity. AddRevision must use dedup's ID.
    let result = observe(&mut db, &repo, Observation::Present(d.clone()), 4).unwrap();
    assert_eq!(result.allocations.new_revisions, vec![RevisionId(0)]);
    assert_eq!(db.head(LogicalId(0)), Some(RevisionId(0)));
    assert!(!db.inputs().artifacts[0].open);
    assert_eq!(db.revision_count(), 5);
    for root in 1..=3 {
        assert_eq!(db.status(RevisionId(root)).unwrap(), Summary::Certified);
    }
    assert_eq!(
        db.validate_certificate(
            &loose,
            &db.inputs().watermarks,
            LogLink {
                matches: true,
                current_checksum: [0; 32]
            }
        )
        .unwrap(),
        Validation::Valid
    );
    assert!(matches!(
        db.validate_certificate(
            &strict,
            &db.inputs().watermarks,
            LogLink {
                matches: true,
                current_checksum: [0; 32]
            }
        )
        .unwrap(),
        Validation::Invalid {
            reason: InvalidReason::StrictChange,
            ..
        }
    ));
    assert!(observe(&mut db, &repo, Observation::Present(d), 4).is_none());
}
#[test]
fn t44_missing_live_evidence_and_sem_asserted_old_support_are_not_hidden_by_effective_vector() {
    let mut db = Db::default();
    let anchor = |key: &str, scope: &str, content| Op::CreateLogical {
        key: format!("code:{key}"),
        kind: RecordKind::CodeAnchor,
        scope_lane: scope.into(),
        content: [content; 32],
        adopt_as_base: true,
    };
    let mut batch = Batch::new(vec![
        anchor("a/x", "a", 1),
        anchor("b/x", "b", 2),
        Op::ObserverWatermark {
            scope: "a".into(),
            expected: None,
            position: vec![1],
        },
        Op::ObserverWatermark {
            scope: "b".into(),
            expected: None,
            position: vec![1],
        },
    ]);
    batch.channel = Channel::Observer;
    db.commit(&batch).unwrap();
    db.commit(&Batch::new(vec![
        Op::CreateLogical {
            key: "x".into(),
            kind: RecordKind::Decision,
            scope_lane: "test".into(),
            content: [3; 32],
            adopt_as_base: false,
        },
        Op::AddRule {
            conclusion: RevRef::New(0),
            incidences: vec![(RevisionId(0).into(), EdgeKind::Cur)],
            enabled: true,
        },
        Op::CreateLogical {
            key: "root".into(),
            kind: RecordKind::Decision,
            scope_lane: "test".into(),
            content: [4; 32],
            adopt_as_base: false,
        },
        Op::AddRule {
            conclusion: RevRef::New(1),
            incidences: vec![(RevisionId(2).into(), EdgeKind::Sem)],
            enabled: true,
        },
    ]))
    .unwrap();
    db.commit(&Batch::new(vec![
        Op::AddRevision {
            logical: LogicalId(2).into(),
            content: [5; 32],
            adopt_as_base: false,
        },
        Op::AddRule {
            conclusion: RevRef::New(0),
            incidences: vec![(RevisionId(1).into(), EdgeKind::Cur)],
            enabled: true,
        },
        Op::SetHead {
            logical: LogicalId(2).into(),
            expected: RevisionId(2).into(),
            new: RevRef::New(0),
        },
        Op::RecordValidation {
            rule: RuleId(1).into(),
            curvec_seen: vec![(LogicalId(2), RevisionId(4))],
            verdict: Verdict::Verified,
            validator: "human".into(),
        },
    ]))
    .unwrap();
    let cert = issue(&db, 3, false);
    assert_eq!(
        cert.watermarks,
        vec![("a".into(), vec![1]), ("b".into(), vec![1])]
    );
    assert!(!cert.vector.iter().any(|v| v.rev == RevisionId(2)));
    assert!(
        db.observation_support(&[RevisionId(3)])
            .unwrap()
            .contains(&RevisionId(2))
    );
    let live = BTreeMap::from([("a".into(), vec![2]), ("b".into(), vec![1])]);
    assert_eq!(
        db.summary_at(RevisionId(3), &live).unwrap(),
        Summary::Pending(PendingReason::ObservationLag)
    );
    assert_eq!(
        db.validate_certificate(
            &cert,
            &live,
            LogLink {
                matches: true,
                current_checksum: [0; 32]
            }
        )
        .unwrap(),
        Validation::Unknown {
            lag: vec!["a".into()]
        }
    );
    assert!(
        db.issue_certificate(
            &IssueRequest {
                session_id: [1; 16],
                principal: PrincipalId(0),
                branch: MAIN,
                log_checksum: [0; 32],
                code_commit: String::new(),
                strict_no_aba: false
            },
            &cert.entries,
            &BTreeMap::new()
        )
        .is_err()
    );
}
#[test]
fn symlinks_fifo_and_limits_never_follow_or_hang() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new("sha1");
    let repo = fixture.repo();
    std::fs::create_dir(fixture.0.join("real")).unwrap();
    std::fs::write(fixture.0.join("real/a.rs"), "a").unwrap();
    symlink("real", fixture.0.join("alias")).unwrap();
    symlink("real/a.rs", fixture.0.join("link.rs")).unwrap();
    nix::unistd::mkfifo(&fixture.0.join("fifo.rs"), nix::sys::stat::Mode::S_IRUSR).unwrap();
    let cap = repo
        .capture(
            &BTreeSet::from([
                "alias/a.rs".into(),
                "link.rs".into(),
                "fifo.rs".into(),
                "missing.rs".into(),
            ]),
            Instant::now() + OBSERVER_TIMEOUT,
        )
        .unwrap();
    assert!(matches!(cap.entries["alias/a.rs"], Observation::Broken(_)));
    assert!(matches!(cap.entries["link.rs"], Observation::Broken(_)));
    assert!(matches!(cap.entries["fifo.rs"], Observation::Broken(_)));
    assert_eq!(cap.entries["missing.rs"], Observation::Missing);
    // Review m1/m6: a Unix socket (EOPNOTSUPP on macOS) and a directory whose
    // `.git` is a symlink are Broken for that anchor; the scope stays live.
    let _socket = std::os::unix::net::UnixListener::bind(fixture.0.join("sock.rs")).unwrap();
    std::fs::create_dir(fixture.0.join("nested")).unwrap();
    std::fs::write(fixture.0.join("nested/x.rs"), "x").unwrap();
    symlink("/nowhere", fixture.0.join("nested/.git")).unwrap();
    let cap = repo
        .capture(
            &BTreeSet::from(["sock.rs".into(), "nested/x.rs".into(), "real/a.rs".into()]),
            Instant::now() + OBSERVER_TIMEOUT,
        )
        .unwrap();
    assert_eq!(
        cap.entries["sock.rs"],
        Observation::Broken("unsupported-type".into())
    );
    assert_eq!(
        cap.entries["nested/x.rs"],
        Observation::Broken("submodule".into())
    );
    assert!(matches!(cap.entries["real/a.rs"], Observation::Present(_)));
    // Review m4: registration requires the exact on-disk spelling.
    assert!(capture::exact_spelling(&repo.root, "real/a.rs").unwrap());
    std::fs::create_dir(fixture.0.join("Upper")).unwrap();
    std::fs::write(fixture.0.join("Upper/B.rs"), "b").unwrap();
    assert!(!capture::exact_spelling(&repo.root, "upper/b.rs").unwrap());
    assert!(!capture::exact_spelling(&repo.root, "missing.rs").unwrap());
    let f = std::fs::File::create(fixture.0.join("huge.rs")).unwrap();
    f.set_len((capture::MAX_FILE + 1) as u64).unwrap();
    assert!(
        repo.capture(
            &BTreeSet::from(["huge.rs".into()]),
            Instant::now() + OBSERVER_TIMEOUT
        )
        .is_err()
    );
    assert!(repo.capture(&BTreeSet::new(), Instant::now()).is_err());
}
#[test]
fn strict_hidden_scope_uses_original_premises_after_rebind() {
    let mut db = Db::default();
    let mut history = vec![db.inputs()];
    let commit = |db: &mut Db, history: &mut Vec<Inputs>, ops: Vec<Op>, observer: bool| {
        let mut b = Batch::new(ops);
        if observer {
            b.channel = Channel::Observer;
        }
        db.commit(&b).unwrap();
        history.push(db.inputs());
    };
    commit(
        &mut db,
        &mut history,
        vec![
            Op::CreateLogical {
                key: "code:a/x".into(),
                kind: RecordKind::CodeAnchor,
                scope_lane: "a".into(),
                content: [1; 32],
                adopt_as_base: true,
            },
            Op::ObserverWatermark {
                scope: "a".into(),
                expected: None,
                position: vec![1],
            },
        ],
        true,
    );
    let create = |key: &str, n| Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Decision,
        scope_lane: "test".into(),
        content: [n; 32],
        adopt_as_base: false,
    };
    commit(
        &mut db,
        &mut history,
        vec![
            create("y", 2),
            Op::AddRule {
                conclusion: RevRef::New(0),
                incidences: vec![(RevisionId(0).into(), EdgeKind::Cur)],
                enabled: true,
            },
            create("x", 3),
            Op::AddRule {
                conclusion: RevRef::New(1),
                incidences: vec![(RevisionId(1).into(), EdgeKind::Pin)],
                enabled: true,
            },
            create("root", 4),
            Op::AddRule {
                conclusion: RevRef::New(2),
                incidences: vec![(RevisionId(2).into(), EdgeKind::Sem)],
                enabled: true,
            },
        ],
        false,
    );
    commit(
        &mut db,
        &mut history,
        vec![
            Op::AddRevision {
                logical: LogicalId(2).into(),
                content: [5; 32],
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: LogicalId(2).into(),
                expected: RevisionId(2).into(),
                new: RevRef::New(0),
            },
            Op::RecordValidation {
                rule: RuleId(2).into(),
                curvec_seen: vec![(LogicalId(2), RevisionId(4))],
                verdict: Verdict::Verified,
                validator: "human".into(),
            },
        ],
        false,
    );
    let cert = issue(&db, 3, true);
    assert_eq!(cert.watermarks, vec![("a".into(), vec![1])]);
    assert!(!cert.vector.iter().any(|v| v.logical == LogicalId(0)));
    commit(
        &mut db,
        &mut history,
        vec![
            Op::AddRevision {
                logical: LogicalId(1).into(),
                content: [6; 32],
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: LogicalId(1).into(),
                expected: RevisionId(1).into(),
                new: RevRef::New(0),
            },
            Op::Rebind {
                rule: RuleId(1).into(),
                from: RevisionId(1).into(),
                to: RevRef::New(0),
            },
        ],
        false,
    );
    commit(
        &mut db,
        &mut history,
        vec![Op::ObserverWatermark {
            scope: "a".into(),
            expected: Some(vec![1]),
            position: vec![2],
        }],
        true,
    );
    let actual = db
        .validate_certificate(
            &cert,
            &db.inputs().watermarks,
            LogLink {
                matches: true,
                current_checksum: [0; 32],
            },
        )
        .unwrap();
    assert!(matches!(
        actual,
        Validation::Invalid {
            reason: InvalidReason::StrictChange,
            ..
        }
    ));
    assert_eq!(
        actual,
        contextdb_oracle::cert_validate_oracle(
            &history,
            &cert,
            &db.inputs().watermarks,
            true,
            [0; 32]
        )
    );
}
#[test]
fn gitlinks_and_corrupt_head_are_not_certified_as_regular_or_unborn() {
    let f = Fixture::new("sha1");
    let repo = f.repo();

    // Use the branch name Git actually selected, rather than assuming master.
    let head = std::fs::read_to_string(f.0.join(".git/HEAD")).unwrap();
    let branch = head.trim().strip_prefix("ref: ").unwrap();
    std::fs::create_dir_all(f.0.join(".git").join(branch).parent().unwrap()).unwrap();
    std::fs::write(f.0.join(".git").join(branch), "malformed").unwrap();
    assert!(
        repo.capture(&BTreeSet::new(), Instant::now() + OBSERVER_TIMEOUT)
            .is_err()
    );
    std::fs::remove_file(f.0.join(".git").join(branch)).unwrap();
    std::fs::create_dir(f.0.join("sub")).unwrap();
    std::fs::write(
        f.0.join("sub/a.rs"),
        "plain file in an uninitialized gitlink",
    )
    .unwrap();
    assert!(
        Command::new("git")
            .current_dir(&f.0)
            .args([
                "update-index",
                "--add",
                "--cacheinfo",
                "160000,1111111111111111111111111111111111111111,sub"
            ])
            .status()
            .unwrap()
            .success()
    );
    let c = repo
        .capture(
            &BTreeSet::from(["sub/a.rs".into()]),
            Instant::now() + OBSERVER_TIMEOUT,
        )
        .unwrap();
    assert!(matches!(c.entries["sub/a.rs"], Observation::Broken(_)));
}

#[test]
fn strict_watermark_window_belongs_only_to_roots_that_originally_reached_it() {
    let mut db = Db::default();
    let mut history = vec![db.inputs()];
    let b = Batch::new(vec![Op::CreateLogical {
        key: "plain".into(),
        kind: RecordKind::Fact,
        scope_lane: "test".into(),
        content: [1; 32],
        adopt_as_base: true,
    }]);
    db.commit(&b).unwrap();
    history.push(db.inputs());
    let old = db.epoch();
    let mut b = Batch::new(vec![
        Op::CreateLogical {
            key: "code:a/x".into(),
            kind: RecordKind::CodeAnchor,
            scope_lane: "a".into(),
            content: [2; 32],
            adopt_as_base: true,
        },
        Op::ObserverWatermark {
            scope: "a".into(),
            expected: None,
            position: vec![1],
        },
    ]);
    b.channel = Channel::Observer;
    db.commit(&b).unwrap();
    history.push(db.inputs());
    let mut b = Batch::new(vec![Op::ObserverWatermark {
        scope: "a".into(),
        expected: Some(vec![1]),
        position: vec![2],
    }]);
    b.channel = Channel::Observer;
    db.commit(&b).unwrap();
    history.push(db.inputs());
    let entries = vec![
        CertItem {
            logical: LogicalId(0),
            rev: RevisionId(0),
            required: true,
            via: Via::Head,
            since_epoch: old,
        },
        CertItem {
            logical: LogicalId(1),
            rev: RevisionId(1),
            required: true,
            via: Via::Head,
            since_epoch: db.epoch(),
        },
    ];
    let cert = db
        .issue_certificate(
            &IssueRequest {
                session_id: [1; 16],
                principal: PrincipalId(0),
                branch: MAIN,
                log_checksum: [0; 32],
                code_commit: String::new(),
                strict_no_aba: true,
            },
            &entries,
            &db.inputs().watermarks,
        )
        .unwrap();
    let actual = db
        .validate_certificate(
            &cert,
            &db.inputs().watermarks,
            LogLink {
                matches: true,
                current_checksum: [0; 32],
            },
        )
        .unwrap();
    assert_eq!(actual, Validation::Valid);
    assert_eq!(
        actual,
        contextdb_oracle::cert_validate_oracle(
            &history,
            &cert,
            &db.inputs().watermarks,
            true,
            [0; 32]
        )
    );
}
#[test]
fn classifier_rejects_partial_or_misrouted_manifest_and_suggests_only_equal_raw_oid() {
    let repo = "a".repeat(32);
    let d = descriptor(&repo, "old.rs", 1);
    let state = AnchorState {
        logical: LogicalId(0),
        head: RevisionId(0),
        content: d.content(),
        oid: d.oid.clone(),
        base: true,
        breaks: vec![],
    };
    let prior = BTreeMap::from([("old.rs".into(), state)]);
    assert!(prepare(&repo, &BTreeMap::new(), &prior, None, vec![1]).is_err());
    assert!(
        prepare(
            &repo,
            &BTreeMap::from([("wrong.rs".into(), Observation::Present(d.clone()))]),
            &BTreeMap::new(),
            None,
            vec![1]
        )
        .is_err()
    );
    let new = Descriptor {
        path: "new.rs".into(),
        ..d
    };
    let observations = BTreeMap::from([
        ("old.rs".into(), Observation::Missing),
        ("new.rs".into(), Observation::Present(new)),
    ]);
    let p = prepare(&repo, &observations, &prior, None, vec![1]).unwrap();
    assert!(p.blobs.iter().any(|b| {
        std::str::from_utf8(b).is_ok_and(|t| t.contains("rebind=explicit;candidates=new.rs"))
    }));
    let mut invalid = observations.clone();
    if let Observation::Present(d) = invalid.get_mut("new.rs").unwrap() {
        d.oid = vec![3; 20];
    }
    let p = prepare(&repo, &invalid, &prior, None, vec![1]).unwrap();
    assert!(
        !p.blobs
            .iter()
            .any(|b| std::str::from_utf8(b).is_ok_and(|t| t.contains("candidates=new.rs")))
    );
}
