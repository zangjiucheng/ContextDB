//! Service details the end-to-end tests leave open (mutation testing of
//! `service.rs`): replay of the session log across restarts and log cuts,
//! recall ranking and withholding, certified reads, explicit deps, check
//! with a certificate id or `--strict`, update paging and peek retention,
//! outbox skips and late facts, drift's concurrency rules and write counts,
//! the briefing budget, retention bounds (claims, ended sessions), the FTS
//! indexer and update tokens.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use std::path::Path;

const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
const STRANGER: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(2),
};
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const DIR: &str = "/db";

fn service(disk: &SimDisk) -> Service<SimDisk> {
    service_with_recall(disk, None)
}
fn service_with_recall(disk: &SimDisk, recall: Option<&Path>) -> Service<SimDisk> {
    let dir = Path::new(DIR);
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    Service::with_recall(actor::spawn(store).unwrap(), log, events, recall).unwrap()
}
/// Clean shutdown, then a new daemon on the same disk.
fn restart(s: Service<SimDisk>, disk: &SimDisk) -> Service<SimDisk> {
    shutdown(s);
    service(disk)
}
fn shutdown(s: Service<SimDisk>) {
    assert_eq!(s.handle(ADMIN, Request::Shutdown).response, Response::Done);
    drop(s);
}
/// Commits creations straight to the store, with no daemon (and so no
/// session-log replay) in between.
fn offline_create(disk: &SimDisk, keys: &[(&str, &str)]) {
    let (mut store, _) =
        Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).unwrap();
    for (key, body) in keys {
        let content = store.put_blob(body.as_bytes()).unwrap();
        store
            .commit(&Batch::new(vec![Op::CreateLogical {
                key: (*key).into(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content,
                adopt_as_base: true,
            }]))
            .unwrap();
    }
}
/// `doctor --truncate-before epoch`: keeps the log through `epoch - 1`.
fn cut_before(disk: &SimDisk, epoch: u64) {
    let dir = Path::new(DIR);
    let plan = contextdb_store::doctor::plan_truncate_before(disk, dir, Epoch(epoch)).unwrap();
    contextdb_store::doctor::truncate_before(disk, dir, &plan).unwrap();
}
fn call(s: &Service<SimDisk>, req: Request) -> Response {
    s.handle(AGENT, req).response
}
fn code(r: &Response) -> &str {
    match r {
        Response::Error { code, .. } => code,
        _ => "ok",
    }
}
fn attach_repo(s: &Service<SimDisk>, key: &str, repo: &str) -> (SessionId, bool) {
    match call(
        s,
        Request::SessionAttach {
            harness: "claude".into(),
            harness_session: key.into(),
            repo: repo.into(),
            head_sha: "h".into(),
            branch: None,
            git_branch: None,
        },
    ) {
        Response::Attached { session, created } => (session, created),
        other => panic!("{other:?}"),
    }
}
fn attach(s: &Service<SimDisk>, key: &str) -> SessionId {
    attach_repo(s, key, "/repo").0
}
fn end(s: &Service<SimDisk>, session: SessionId) {
    assert_eq!(call(s, Request::SessionEnd { session }), Response::Done);
}
fn at(session: SessionId) -> Binding {
    Binding::root(session)
}
fn record(
    s: &Service<SimDisk>,
    b: Binding,
    key: &str,
    body: &str,
    adopt_as_base: bool,
    deps: Vec<Dep>,
) -> Response {
    call(
        s,
        Request::Record {
            binding: b,
            kind: RecordKind::Decision,
            body: body.as_bytes().to_vec(),
            target: WriteTarget::Create {
                key: key.into(),
                scope_lane: "repo".into(),
            },
            deps,
            adopt_as_base,
            client_txn_id: None,
        },
    )
}
fn create(s: &Service<SimDisk>, b: Binding, key: &str, body: &str) -> CommitResult {
    match record(s, b, key, body, true, Vec::new()) {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
fn read(s: &Service<SimDisk>, b: Binding, target: Target, mode: Mode) -> Response {
    call(
        s,
        Request::Read {
            binding: b,
            target,
            mode,
        },
    )
}
fn head(s: &Service<SimDisk>, b: Binding, key: &str) -> (LogicalId, RevisionId) {
    match read(s, b, Target::Key(key.into()), Mode::Certified) {
        Response::Read { item, .. } => (item.logical, item.rev),
        other => panic!("{other:?}"),
    }
}
fn supersede(s: &Service<SimDisk>, b: Binding, l: LogicalId, r: RevisionId, body: &str) {
    let resp = call(
        s,
        Request::Supersede {
            binding: b,
            logical: l,
            body: body.as_bytes().to_vec(),
            expected_head: r,
            adopt_as_base: true,
            carry_rules: false,
            reason: "changed".into(),
            client_txn_id: None,
        },
    );
    assert_eq!(code(&resp), "ok", "{resp:?}");
}
fn check_with(
    s: &Service<SimDisk>,
    b: Binding,
    cert_id: Option<ContentHash>,
    strict: bool,
    mode: Option<GateMode>,
) -> Response {
    call(
        s,
        Request::Check {
            binding: b,
            cert_id,
            live_pos: Vec::new(),
            action: None,
            mode,
            harness_version: "t".into(),
            strict,
        },
    )
}
fn check(s: &Service<SimDisk>, b: Binding) -> Response {
    check_with(s, b, None, false, None)
}
fn validation(r: &Response) -> &Validation {
    match r {
        Response::Check { validation, .. } => validation,
        other => panic!("{other:?}"),
    }
}
fn cert(r: &Response) -> &Certificate {
    match r {
        Response::Check { cert: Some(c), .. } => c,
        other => panic!("no certificate: {other:?}"),
    }
}
fn log_mismatch(r: &Response) -> bool {
    matches!(
        validation(r),
        Validation::Invalid {
            reason: InvalidReason::LogMismatch,
            ..
        }
    )
}
fn telemetry(s: &Service<SimDisk>) -> Vec<TelemetryEntry> {
    match s
        .handle(
            ADMIN,
            Request::Telemetry {
                since: 0,
                limit: 100_000,
            },
        )
        .response
    {
        Response::Telemetry { entries, .. } => entries,
        other => panic!("{other:?}"),
    }
}
fn facts(
    s: &Service<SimDisk>,
    key: &str,
    stream: u8,
    packets: Vec<Packet>,
    skip: Option<(u64, u64)>,
) -> Response {
    call(
        s,
        Request::Facts {
            harness: "claude".into(),
            harness_session: key.into(),
            stream: [stream; 16],
            packets,
            skip,
        },
    )
}
fn edit(seq: u64, path: &str) -> Packet {
    Packet {
        seq,
        agent: None,
        fact: Fact::Action {
            action: Action {
                tool: ToolClass::Edit,
                paths: vec![path.into()],
                command: None,
            },
            ok: true,
        },
    }
}
/// Delivers one `Drift` fact and returns its report.
fn drift(s: &Service<SimDisk>, key: &str, stream: u8, seq: u64, done: &[&str]) -> DriftReport {
    let packet = Packet {
        seq,
        agent: None,
        fact: Fact::Drift {
            done: Some(done.iter().map(|p| (*p).to_string()).collect()),
        },
    };
    match facts(s, key, stream, vec![packet], None) {
        Response::Facts { mut drift, .. } if drift.len() == 1 => drift.remove(0),
        other => panic!("{other:?}"),
    }
}
fn applied(r: &Response) -> u64 {
    match r {
        Response::Facts {
            applied_through, ..
        } => *applied_through,
        other => panic!("{other:?}"),
    }
}
fn recall_full(
    s: &Service<SimDisk>,
    b: Binding,
    query: &str,
    mode: Mode,
) -> (Vec<Item>, Vec<Item>, Option<ContentHash>) {
    match call(
        s,
        Request::Recall {
            binding: b,
            query: query.into(),
            scope: "repo".into(),
            mode,
            limit: 50,
        },
    ) {
        Response::Recall {
            items,
            withheld,
            cert_id,
        } => (items, withheld, cert_id),
        other => panic!("{other:?}"),
    }
}
fn keys(items: &[Item]) -> Vec<&str> {
    items.iter().map(|i| i.key.as_str()).collect()
}
/// Polls a recall until `done` holds (the FTS index lags the snapshot).
fn recall_until(
    s: &Service<SimDisk>,
    b: &Binding,
    query: &str,
    done: impl Fn(&[Item]) -> bool,
) -> Vec<Item> {
    let mut items = Vec::new();
    for _ in 0..300 {
        items = recall_full(s, b.clone(), query, Mode::Advisory).0;
        if done(&items) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    items
}

// ---------------------------------------------------------------- replay

/// A session started at the tail is healthy across restarts; a session
/// whose start checksum no longer matches the log (cut, then the cut
/// epochs reallocated offline) is broken even though it starts at the tail.
#[test]
fn replay_checks_the_start_epoch_and_checksum_and_marks_only_broken_sessions() {
    let disk = SimDisk::new();
    let a = {
        let s = service(&disk);
        let w = attach(&s, "W");
        create(&s, at(w), "x", "1");
        create(&s, at(w), "y", "2");
        let a = attach(&s, "A");
        shutdown(s);
        a
    };
    // Started exactly at the tail: not cut. Two restarts, so a session
    // wrongly marked durably broken by the first would show at the second.
    let s = service(&disk);
    assert_eq!(validation(&check(&s, at(a))), &Validation::Empty);
    let s = restart(s, &disk);
    assert_eq!(validation(&check(&s, at(a))), &Validation::Empty);
    shutdown(s);
    // Epoch 2 is cut and reallocated with other content before any daemon
    // runs: the start epoch is within the tail, only its checksum differs.
    cut_before(&disk, 2);
    offline_create(&disk, &[("z", "other")]);
    let s = service(&disk);
    let r = check(&s, at(a));
    assert!(log_mismatch(&r), "{r:?}");
}

/// A recall certificate issued at a cut epoch that was reallocated breaks
/// the session (its handles refuse), and validating that certificate by id
/// fails on the log link.
#[test]
fn a_certificate_over_a_reallocated_epoch_breaks_handles_and_its_log_link() {
    let disk = SimDisk::new();
    let (a, handle, rc) = {
        let s = service(&disk);
        let w = attach(&s, "W");
        create(&s, at(w), "x", "1");
        let a = attach(&s, "A");
        create(&s, at(w), "y", "2");
        let (items, _, cert_id) = recall_full(&s, at(a), "x", Mode::Certified);
        assert_eq!(keys(&items)[0], "x");
        let handle = items[0].handle.clone().unwrap();
        shutdown(s);
        (a, handle, cert_id.unwrap())
    };
    cut_before(&disk, 2);
    offline_create(&disk, &[("z", "other")]);
    let s = service(&disk);
    assert_eq!(
        code(&read(&s, at(a), Target::Handle(handle), Mode::Certified)),
        "InvalidDep"
    );
    let r = check_with(&s, at(a), Some(rc), false, None);
    assert!(log_mismatch(&r), "{r:?}");
}

/// The restored session certificate is the one the session-cert event
/// names, not another certificate of the session (here: a recall's).
#[test]
fn restart_restores_the_named_session_certificate() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let w = attach(&s, "W");
    let a = attach(&s, "A");
    create(&s, at(w), "x", "1");
    create(&s, at(w), "y", "2");
    recall_full(&s, at(a), "x", Mode::Certified);
    head(&s, at(a), "y");
    let c1 = cert(&check(&s, at(a))).cert_id;
    let s = restart(s, &disk);
    create(&s, at(w), "unrelated", "3");
    let r = check(&s, at(a));
    assert_eq!(validation(&r), &Validation::Valid);
    assert_eq!(cert(&r).cert_id, c1, "validated, not re-issued");
}

/// Writes are counted once (also when replayed) and survive a restart in
/// telemetry and in drift's write counts.
#[test]
fn writes_are_replayed_once_into_telemetry_and_drift() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    let r1 = create(&s, at(a), "x", "1");
    let x = match read(&s, at(a), Target::Key("x".into()), Mode::Advisory) {
        Response::Read { item, .. } => item,
        other => panic!("{other:?}"),
    };
    let dep = Dep {
        target: Target::KeyRev {
            key: "x".into(),
            rev: x.rev,
        },
        kind: EdgeKind::Cur,
    };
    let txn = [7u8; 16];
    let req = Request::Record {
        binding: at(a),
        kind: RecordKind::Plan,
        body: b"p".to_vec(),
        target: WriteTarget::Create {
            key: "p".into(),
            scope_lane: "repo".into(),
        },
        deps: vec![dep],
        adopt_as_base: false,
        client_txn_id: Some(txn),
    };
    assert_eq!(code(&call(&s, req.clone())), "ok");
    // A retry of the same client_txn_id is not a second write.
    assert_eq!(code(&call(&s, req.clone())), "ok");
    let writes = |s: &Service<SimDisk>| {
        telemetry(s)
            .into_iter()
            .filter_map(|e| match e {
                TelemetryEntry::Write {
                    with_deps, epoch, ..
                } => Some((with_deps, epoch)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let before = writes(&s);
    assert_eq!(before.len(), 2);
    assert_eq!(before[0], (false, r1.epoch));
    assert!(before[1].0);
    let report = drift(&s, "A", 1, 1, &[]);
    assert_eq!((report.writes, report.writes_with_deps), (2, 1));
    let s = restart(s, &disk);
    assert_eq!(writes(&s), before);
    let report = drift(&s, "A", 1, 2, &[]);
    assert_eq!((report.writes, report.writes_with_deps), (2, 1));
    assert_eq!(code(&call(&s, req)), "ok");
    assert_eq!(writes(&s), before, "a retry after restart is not counted");
}

// ------------------------------------------------- sessions and binding

#[test]
fn only_the_owning_principal_ends_a_session() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    assert_eq!(
        code(
            &s.handle(STRANGER, Request::SessionEnd { session: a })
                .response
        ),
        "UnknownSession"
    );
    assert_eq!(attach_repo(&s, "A", "/repo"), (a, false), "still live");
    end(&s, a);
}

/// A session an MCP call started gets its repository from the first hook
/// that names one, and keeps it afterwards (drift compares repositories).
#[test]
fn attach_fills_an_empty_repository_once() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let key = Binding::Key {
        harness: "claude".into(),
        harness_session: "M".into(),
        agent: None,
    };
    create(&s, key, "x", "1");
    attach(&s, "O");
    applied(&facts(&s, "O", 9, vec![edit(1, "o.rs")], None));
    // The MCP-started session has no repository: the concurrent session's
    // path is not covered yet.
    assert_eq!(drift(&s, "M", 1, 1, &["o.rs"]).undeclared, vec!["o.rs"]);
    assert!(!attach_repo(&s, "M", "/repo").1);
    assert!(drift(&s, "M", 1, 2, &["o.rs"]).undeclared.is_empty());
    // A later attach naming another repository does not move it.
    attach_repo(&s, "M", "/elsewhere");
    assert!(drift(&s, "M", 1, 3, &["o.rs"]).undeclared.is_empty());
}

/// Claims are kept up to `CLAIMS_KEPT` (65 536), oldest dropped first.
#[test]
fn claims_are_bounded_oldest_first() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    let w = attach(&s, "W");
    create(&s, at(w), "x", "1");
    let claim = |n: usize| {
        let r = call(
            &s,
            Request::Claim {
                session: a,
                agent: None,
                call: format!("toolu_{n}"),
            },
        );
        assert_eq!(r, Response::Done);
    };
    let by_call = |n: usize| {
        code(&read(
            &s,
            Binding::Call {
                id: format!("toolu_{n}"),
            },
            Target::Key("x".into()),
            Mode::Advisory,
        ))
        .to_owned()
    };
    for n in 0..65_536 {
        claim(n);
    }
    assert_eq!(by_call(0), "ok", "exactly CLAIMS_KEPT claims are kept");
    claim(65_536);
    assert_eq!(by_call(0), "Unclaimed");
    assert_eq!(by_call(1), "ok");
}

/// Ended sessions are kept up to `ENDED_KEPT` (256) for late facts and
/// drift; a resumed harness session sees its earlier session's paths.
#[test]
fn ended_sessions_are_kept_for_drift_up_to_the_bound() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "first");
    applied(&facts(&s, "first", 1, vec![edit(1, "kept.rs")], None));
    end(&s, a);
    for i in 0..255 {
        let f = attach(&s, &format!("filler-{i}"));
        end(&s, f);
    }
    let (b, created) = attach_repo(&s, "first", "/repo");
    assert!(created && b != a);
    assert!(drift(&s, "first", 2, 1, &["kept.rs"]).undeclared.is_empty());
    let f = attach(&s, "filler-last");
    end(&s, f);
    assert_eq!(
        drift(&s, "first", 2, 2, &["kept.rs"]).undeclared,
        vec!["kept.rs"],
        "the 257th ended session evicts the oldest"
    );
}

// ---------------------------------------------------------- facts, drift

/// Late facts of an ended session go to that session (live and after
/// replay), and a resumed harness session counts its paths as declared even
/// when the two were never active at the same time.
#[test]
fn late_facts_reach_the_ended_session_and_its_resumption_live_and_replayed() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "K");
    applied(&facts(&s, "K", 1, vec![edit(1, "early.rs")], None));
    end(&s, a);
    assert_eq!(
        applied(&facts(&s, "K", 1, vec![edit(2, "late.rs")], None)),
        2
    );
    // An epoch passes: the resumption does not overlap the ended session.
    let w = attach_repo(&s, "W", "/elsewhere").0;
    create(&s, at(w), "x", "1");
    let (b, created) = attach_repo(&s, "K", "/repo");
    assert!(created && b != a);
    let done = ["early.rs", "late.rs", "new.rs"];
    assert_eq!(drift(&s, "K", 2, 1, &done).undeclared, vec!["new.rs"]);
    let s = restart(s, &disk);
    assert_eq!(drift(&s, "K", 2, 2, &done).undeclared, vec!["new.rs"]);
}

/// Drift's "declared elsewhere" covers sessions of the same repository
/// active at the same time (ended at or after this start), and no others.
#[test]
fn drift_counts_only_concurrent_sessions_of_the_same_repository() {
    let disk = SimDisk::new();
    let s = service(&disk);
    // E ends before A starts (an epoch passes in between).
    let e = attach(&s, "E");
    applied(&facts(&s, "E", 1, vec![edit(1, "e.rs")], None));
    end(&s, e);
    let w = attach_repo(&s, "W", "/elsewhere").0;
    create(&s, at(w), "w", "1");
    attach(&s, "A");
    // O ends at A's start epoch: concurrent.
    let o = attach(&s, "O");
    applied(&facts(&s, "O", 2, vec![edit(1, "o.rs")], None));
    end(&s, o);
    // X is live, in another repository.
    attach_repo(&s, "X", "/other");
    applied(&facts(&s, "X", 3, vec![edit(1, "x.rs")], None));
    let report = drift(&s, "A", 4, 1, &["e.rs", "o.rs", "x.rs"]);
    assert_eq!(report.undeclared, vec!["e.rs", "x.rs"]);
}

#[test]
fn facts_skips_gaps_partial_batches_and_unavailable_gates() {
    let disk = SimDisk::new();
    let s = service(&disk);
    attach(&s, "S");
    let send = |packets, skip| facts(&s, "S", 1, packets, skip);
    assert_eq!(applied(&send(vec![edit(1, "a"), edit(2, "b")], None)), 2);
    // A skip that starts beyond the next sequence number is a gap.
    assert_eq!(
        send(vec![edit(7, "c")], Some((5, 6))),
        Response::Gap { expected: 3 }
    );
    // A skip of what is already applied skips nothing.
    assert_eq!(applied(&send(vec![edit(3, "c")], Some((1, 2)))), 3);
    let skips = |s: &Service<SimDisk>| {
        telemetry(s)
            .into_iter()
            .filter_map(|e| match e {
                TelemetryEntry::Skip { from, to, .. } => Some((from, to)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert!(skips(&s).is_empty());
    // An overlapping skip skips only what is not applied yet.
    assert_eq!(applied(&send(vec![edit(6, "d")], Some((2, 5)))), 6);
    assert_eq!(skips(&s), vec![(4, 5)]);
    // A batch that breaks off after applied packets applies the prefix.
    let gate = Packet {
        seq: 7,
        agent: None,
        fact: Fact::Gate {
            action: Action {
                tool: ToolClass::Shell,
                paths: Vec::new(),
                command: Some("make".into()),
            },
            mode: GateMode::Enforce,
            decision: Decision::Allow,
            harness_version: "2.1".into(),
            at: 1_234,
        },
    };
    assert_eq!(applied(&send(vec![gate, edit(9, "e")], None)), 7);
    let gates = |s: &Service<SimDisk>| {
        telemetry(s)
            .into_iter()
            .filter_map(|e| match e {
                TelemetryEntry::Gate {
                    verdict,
                    decision,
                    available,
                    at,
                    ..
                } => Some((verdict, decision, available, at)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let expected = vec![("Unavailable".to_string(), Decision::Allow, false, 1_234)];
    assert_eq!(gates(&s), expected);
    let s = restart(s, &disk);
    assert_eq!(gates(&s), expected);
    assert_eq!(skips(&s), vec![(4, 5)]);
}

/// The daemon's own gate decisions carry the wall-clock time.
#[test]
fn gate_telemetry_is_stamped_with_the_wall_clock() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    check_with(&s, at(a), None, false, Some(GateMode::Warn));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let at = telemetry(&s)
        .into_iter()
        .find_map(|e| match e {
            TelemetryEntry::Gate { at, .. } => Some(at),
            _ => None,
        })
        .unwrap();
    assert!(now.abs_diff(at) < 600, "{at} vs {now}");
}

// --------------------------------------------------- reads, deps, check

#[test]
fn certified_reads_fail_closed_with_the_heads_own_summary() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    let w = attach(&s, "W");
    assert_eq!(
        code(&record(&s, at(w), "weak", "w", false, Vec::new())),
        "ok"
    );
    create(&s, at(w), "good", "g");
    let weak = match read(&s, at(a), Target::Key("weak".into()), Mode::Advisory) {
        Response::Read { item, .. } => item,
        other => panic!("{other:?}"),
    };
    assert_ne!(weak.summary, Summary::Certified);
    match read(&s, at(a), Target::Key("weak".into()), Mode::Certified) {
        Response::Error { code, detail } => {
            assert_eq!(code, "NotCertified");
            assert_eq!(detail, format!("{:?} {:?}", weak.logical, weak.summary));
        }
        other => panic!("{other:?}"),
    }
    // An advisory read of a certified head adds no required read.
    let b = attach(&s, "B");
    assert_eq!(
        code(&read(&s, at(b), Target::Key("good".into()), Mode::Advisory)),
        "ok"
    );
    assert_eq!(validation(&check(&s, at(b))), &Validation::Empty);
}

#[test]
fn explicit_revisions_must_belong_to_the_named_logical() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    create(&s, at(a), "x", "1");
    create(&s, at(a), "y", "2");
    let (lx, rx) = head(&s, at(a), "x");
    let (_, ry) = head(&s, at(a), "y");
    let why = |target| code(&call(&s, Request::Why { target, depth: 1 })).to_owned();
    assert_eq!(
        why(Target::Pinned {
            logical: lx,
            rev: rx
        }),
        "ok"
    );
    assert_eq!(
        why(Target::Pinned {
            logical: lx,
            rev: ry
        }),
        "InvalidArgument"
    );
    let dep = |rev| {
        vec![Dep {
            target: Target::KeyRev {
                key: "x".into(),
                rev,
            },
            kind: EdgeKind::Cur,
        }]
    };
    assert_eq!(
        code(&record(&s, at(a), "p1", "p", false, dep(ry))),
        "InvalidDep"
    );
    assert_eq!(code(&record(&s, at(a), "p2", "p", false, dep(rx))), "ok");
}

/// Re-reading the same revision is not a supersession; reading a newer one is.
#[test]
fn only_a_read_of_another_revision_supersedes() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    let w = attach(&s, "W");
    create(&s, at(w), "x", "1");
    let (l, r0) = head(&s, at(a), "x");
    head(&s, at(a), "x");
    let superseded = |r: &Response| match r {
        Response::Check { superseded, .. } => superseded
            .iter()
            .map(|d| (d.logical, d.cert_rev, d.current))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert!(superseded(&check(&s, at(a))).is_empty());
    supersede(&s, at(w), l, r0, "2");
    let (_, r1) = head(&s, at(a), "x");
    assert_eq!(superseded(&check(&s, at(a))), vec![(l, Some(r0), Some(r1))]);
}

/// `ctx check --cert` validates a certificate issued to this session;
/// `--strict` applies to it and to the session certificate; an unchanged,
/// covering session certificate is returned as is.
#[test]
fn check_by_certificate_id_strict_and_steady_session_certificate() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    let w = attach(&s, "W");
    create(&s, at(w), "x", "1");
    let (_, _, rc) = recall_full(&s, at(a), "x", Mode::Certified);
    let rc = rc.unwrap();
    let r = check_with(&s, at(a), Some(rc), true, None);
    assert_eq!(validation(&r), &Validation::Valid);
    assert_eq!(cert(&r).cert_id, rc);
    assert!(
        !cert(&r).strict_no_aba,
        "strict request is a validation overlay on an immutable explicit certificate"
    );
    let c1 = cert(&check(&s, at(a))).clone();
    assert!(!c1.strict_no_aba);
    create(&s, at(w), "unrelated", "2");
    let r = check(&s, at(a));
    assert_eq!(validation(&r), &Validation::Valid);
    assert_eq!(cert(&r).cert_id, c1.cert_id, "covered: not re-issued");
    let r = check_with(&s, at(a), None, true, None);
    assert!(cert(&r).strict_no_aba, "{r:?}");
    assert_eq!(
        cert(&r).cert_id,
        *blake3::hash(&cert(&r).body_bytes()).as_bytes()
    );
    let upgraded = cert(&r).cert_id;
    s.handle(
        Identity {
            channel: Channel::Admin,
            principal: PrincipalId(0),
        },
        Request::Shutdown,
    );
    drop(s);
    let s = service(&disk);
    let r = check(&s, at(a));
    assert!(cert(&r).strict_no_aba);
    assert_eq!(cert(&r).cert_id, upgraded);
}

// -------------------------------------------------------------- updates

#[test]
fn updates_page_by_max_items_and_keep_eight_peeks() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    let w = attach(&s, "W");
    for k in ["a", "b", "c"] {
        create(&s, at(w), k, "1");
    }
    for k in ["a", "b", "c"] {
        let (l, r) = head(&s, at(a), k);
        supersede(&s, at(w), l, r, "2");
    }
    let updates = |ack: Option<u64>, max_items: u32| match call(
        &s,
        Request::Updates {
            binding: at(a),
            ack,
            max_items,
        },
    ) {
        Response::Updates { token, items, more } => (token, items.len(), more),
        other => panic!("{other:?}"),
    };
    // 0 asks for the default page.
    let (_, n, more) = updates(None, 0);
    assert_eq!((n, more), (3, false));
    let (_, n, more) = updates(None, 2);
    assert_eq!((n, more), (2, true));
    let (_, n, more) = updates(None, 3);
    assert_eq!((n, more), (3, false));
    let (_, n, more) = updates(None, 5);
    assert_eq!((n, more), (3, false));
    // Eight peeks are kept: the oldest of eight still acknowledges...
    let tokens: Vec<u64> = (0..8).map(|_| updates(None, 10).0).collect();
    assert_eq!(updates(Some(tokens[0]), 10).1, 0);
    // ...the oldest of nine does not.
    let (l, r) = head(&s, at(a), "a");
    supersede(&s, at(w), l, r, "3");
    let tokens: Vec<u64> = (0..9).map(|_| updates(None, 10).0).collect();
    assert_eq!(updates(Some(tokens[0]), 10).1, 1);
    assert_eq!(updates(Some(tokens[8]), 10).1, 0);
}

/// Update tokens start below 2^63, so they never wrap.
#[test]
fn update_tokens_start_in_the_lower_half() {
    for _ in 0..32 {
        let disk = SimDisk::new();
        let s = service(&disk);
        let a = attach(&s, "A");
        let token = match call(
            &s,
            Request::Updates {
                binding: at(a),
                ack: None,
                max_items: 1,
            },
        ) {
            Response::Updates { token, .. } => token,
            other => panic!("{other:?}"),
        };
        assert!(token < 1 << 63, "{token:#x}");
    }
}

// -------------------------------------------------------------- recall

/// Exact key, then key prefix, then key substring, then full-text hits in
/// rank order; a logical is listed once.
#[test]
fn recall_ranks_key_matches_before_text_hits_without_duplicates() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    for (key, body) in [
        ("doc-a", "zebra one"),
        ("doc-b", "zebra zebra two"),
        ("doc-c", "zebra three zebra zebra"),
        ("xzebrax", "plain"),
        ("zebra", "plain zebra"),
        ("zebras", "plain"),
    ] {
        create(&s, at(a), key, body);
    }
    let items = recall_until(&s, &at(a), "zebra", |items| {
        ["doc-a", "doc-b", "doc-c"]
            .iter()
            .all(|k| items.iter().any(|i| i.key == *k))
    });
    let got = keys(&items);
    assert_eq!(got.len(), 6, "{got:?}");
    assert_eq!(&got[..3], ["zebra", "zebras", "xzebrax"]);
    let mut rest = got[3..].to_vec();
    rest.sort();
    assert_eq!(rest, ["doc-a", "doc-b", "doc-c"]);
}

/// A certified recall withholds what is not certified and issues a
/// certificate; an advisory recall withholds nothing and issues none.
#[test]
fn recall_withholds_uncertified_items_only_in_certified_mode() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    create(&s, at(a), "rw-good", "g");
    assert_eq!(
        code(&record(&s, at(a), "rw-weak", "w", false, Vec::new())),
        "ok"
    );
    let (items, withheld, cert_id) = recall_full(&s, at(a), "rw-", Mode::Certified);
    assert_eq!(
        (keys(&items), keys(&withheld)),
        (vec!["rw-good"], vec!["rw-weak"])
    );
    assert!(cert_id.is_some());
    let (items, withheld, cert_id) = recall_full(&s, at(a), "rw-", Mode::Advisory);
    assert_eq!(keys(&items), vec!["rw-good", "rw-weak"]);
    assert!(withheld.is_empty());
    assert_eq!(cert_id, None);
    // Not even when everything found is certified.
    let (items, _, cert_id) = recall_full(&s, at(a), "rw-good", Mode::Advisory);
    assert_eq!(keys(&items), vec!["rw-good"]);
    assert_eq!(cert_id, None);
    assert!(items[0].handle.is_none());
}

/// The indexer keeps payload text beyond the first kilobytes (64 KiB).
#[test]
fn recall_indexes_long_payloads() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    let long = format!("{} quokka", "filler ".repeat(1_000));
    create(&s, at(a), "long", &long);
    create(&s, at(a), "marker", "wombat");
    let found = recall_until(&s, &at(a), "wombat", |i| !i.is_empty());
    assert_eq!(keys(&found), ["marker"]);
    assert_eq!(
        keys(&recall_full(&s, at(a), "quokka", Mode::Advisory).0),
        ["long"]
    );
}

/// A persistent recall projection ahead of the log (e.g. after a restore)
/// is rebuilt, not trusted.
#[test]
fn a_recall_projection_ahead_of_the_log_is_rebuilt() {
    let path = std::env::temp_dir().join(format!(
        "ctxd-recall-ahead-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cleanup = || {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    };
    {
        let disk = SimDisk::new();
        let s = service_with_recall(&disk, Some(&path));
        let a = attach(&s, "A");
        create(&s, at(a), "k0", "narwhal");
        create(&s, at(a), "k1", "one");
        create(&s, at(a), "k2", "two");
        let found = recall_until(&s, &at(a), "two", |i| !i.is_empty());
        assert_eq!(keys(&found), ["k2"]);
        shutdown(s);
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    // Another history, shorter than the projection's watermark.
    let disk = SimDisk::new();
    offline_create(&disk, &[("k0", "plain")]);
    let s = service_with_recall(&disk, Some(&path));
    let a = attach(&s, "A");
    let found = recall_until(&s, &at(a), "plain", |i| !i.is_empty());
    let stale = recall_full(&s, at(a), "narwhal", Mode::Advisory).0;
    shutdown(s);
    cleanup();
    assert_eq!(keys(&found), ["k0"]);
    assert!(stale.is_empty(), "stale projection row: {stale:?}");
}

// ------------------------------------------------------- briefing, diff

/// Items fit the character budget: each costs its key plus 48.
#[test]
fn briefing_items_fit_the_budget_exactly() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    create(&s, at(a), "k1", "1");
    create(&s, at(a), "k2", "2");
    let brief = |budget| match call(
        &s,
        Request::Briefing {
            binding: None,
            budget_chars: budget,
        },
    ) {
        Response::Briefing { items, .. } => items.into_iter().map(|i| i.key).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert!(brief(49).is_empty());
    assert_eq!(brief(50), ["k2"]);
    assert_eq!(brief(99), ["k2"]);
    assert_eq!(brief(100), ["k2", "k1"]);
}

/// A diff reaching before the loaded snapshot replays the log.
#[test]
fn diff_before_the_snapshot_replays_the_log() {
    let disk = SimDisk::new();
    offline_create(&disk, &[("x", "1"), ("y", "2")]);
    {
        let (mut store, _) =
            Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).unwrap();
        assert_eq!(store.snapshot().unwrap(), Epoch(2));
    }
    offline_create(&disk, &[("z", "3")]);
    let s = service(&disk);
    for (from, to) in [(0, 3), (1, 2), (2, 3), (3, 0)] {
        let r = call(
            &s,
            Request::Diff {
                from: Epoch(from),
                to: Epoch(to),
            },
        );
        assert_eq!(code(&r), "ok", "{from}..{to}: {r:?}");
    }
}

/// Mutation closure: the briefing counts PENDING heads (a Sem dependency
/// awaiting validation), and `stop_writer` makes lazily queued events durable
/// at once, before the store is released.
#[test]
fn briefing_counts_pending_heads_and_stop_writer_syncs_lazy_events() {
    let disk = contextdb_store::SimDisk::new();
    let s = briefing_service(&disk);
    let Response::Attached { session, .. } = s
        .handle(
            BRIEF_AGENT,
            Request::SessionAttach {
                harness: "t".into(),
                harness_session: "k".into(),
                repo: String::new(),
                head_sha: String::new(),
                branch: None,
                git_branch: None,
            },
        )
        .response
    else {
        panic!()
    };
    // Records without deps are base; `y` is supported only by its Sem rule.
    let rec = |key: &str, deps: Vec<Dep>| match s
        .handle(
            BRIEF_AGENT,
            Request::Record {
                binding: Binding::root(session),
                kind: RecordKind::Decision,
                body: key.as_bytes().to_vec(),
                target: WriteTarget::Create {
                    key: key.into(),
                    scope_lane: "repo".into(),
                },
                adopt_as_base: deps.is_empty(),
                deps,
                client_txn_id: None,
            },
        )
        .response
    {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    };
    let x = rec("x", Vec::new());
    let x_rev = x.allocations.new_revisions[0];
    rec(
        "y",
        vec![Dep {
            target: Target::Pinned {
                logical: LogicalId(0),
                rev: x_rev,
            },
            kind: EdgeKind::Sem,
        }],
    );
    // x moves: y's Sem premise awaits validation, so y is PENDING.
    let moved = s.handle(
        BRIEF_AGENT,
        Request::Supersede {
            binding: Binding::root(session),
            logical: LogicalId(0),
            body: b"x2".to_vec(),
            expected_head: x_rev,
            adopt_as_base: true,
            carry_rules: false,
            reason: String::new(),
            client_txn_id: None,
        },
    );
    assert!(
        matches!(moved.response, Response::Committed { .. }),
        "{moved:?}"
    );
    match s
        .handle(
            BRIEF_AGENT,
            Request::Briefing {
                binding: None,
                budget_chars: 0,
            },
        )
        .response
    {
        Response::Briefing { pending, stale, .. } => assert_eq!((pending, stale), (1, 0)),
        other => panic!("{other:?}"),
    }
    // A lazily synced read, then stop_writer, then power loss at once.
    let read = s.handle(
        BRIEF_AGENT,
        Request::Read {
            binding: Binding::root(session),
            target: Target::Key("x".into()),
            mode: Mode::Certified,
        },
    );
    assert!(matches!(read.response, Response::Read { .. }), "{read:?}");
    s.stop_writer();
    let crashed = disk.crash(contextdb_store::TailFate::ZeroFill, 0);
    let (_, events, _) = contextdb_store::session::SessionLog::open(
        crashed,
        std::path::Path::new("/db"),
        false,
        1 << 20,
    )
    .unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        contextdb_store::session::SessionEvent::Read { session_id, .. } if *session_id == session
    )));
}
const BRIEF_AGENT: contextdb_daemon::service::Identity = contextdb_daemon::service::Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
fn briefing_service(
    disk: &contextdb_store::SimDisk,
) -> contextdb_daemon::service::Service<contextdb_store::SimDisk> {
    let dir = std::path::Path::new("/db");
    let (store, _) =
        contextdb_store::Store::open(disk.clone(), dir, &contextdb_store::OpenOptions::default())
            .unwrap();
    let (log, events, _) =
        contextdb_store::session::SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    contextdb_daemon::service::Service::new(
        contextdb_daemon::actor::spawn(store).unwrap(),
        log,
        events,
    )
    .unwrap()
}
