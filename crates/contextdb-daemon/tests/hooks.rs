//! P4 milestone 1 at the service layer (ADR 0010): session binding (attach,
//! claims, harness keys), per-agent ledger views (T66), the gate on `Check`,
//! `context_update` peek/ack, outbox facts (stream marks, gaps, skips, drift)
//! and the advisory briefing.
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
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
fn service(disk: &SimDisk) -> Service<SimDisk> {
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    Service::new(actor::spawn(store).unwrap(), log, events).unwrap()
}
/// Clean shutdown, then a new daemon on the same disk.
fn restart(s: Service<SimDisk>, disk: &SimDisk) -> Service<SimDisk> {
    assert_eq!(s.handle(ADMIN, Request::Shutdown).response, Response::Done);
    drop(s);
    service(disk)
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
fn attach(s: &Service<SimDisk>, key: &str) -> (SessionId, bool) {
    match call(
        s,
        Request::SessionAttach {
            harness: "claude".into(),
            harness_session: key.into(),
            repo: "/repo".into(),
            head_sha: "h".into(),
            branch: None,
            git_branch: None,
        },
    ) {
        Response::Attached { session, created } => (session, created),
        other => panic!("{other:?}"),
    }
}
fn at(session: SessionId, agent: Option<&str>) -> Binding {
    Binding::Session {
        session,
        agent: agent.map(Into::into),
    }
}
fn create(s: &Service<SimDisk>, b: Binding, key: &str, body: &str) -> CommitResult {
    match call(
        s,
        Request::Record {
            binding: b,
            kind: RecordKind::Decision,
            body: body.as_bytes().to_vec(),
            target: WriteTarget::Create {
                key: key.into(),
                scope_lane: "repo".into(),
            },
            deps: Vec::new(),
            adopt_as_base: true,
            client_txn_id: None,
        },
    ) {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
fn read(s: &Service<SimDisk>, b: Binding, key: &str) -> Response {
    call(
        s,
        Request::Read {
            binding: b,
            target: Target::Key(key.into()),
            mode: Mode::Certified,
        },
    )
}
fn head(s: &Service<SimDisk>, b: Binding, key: &str) -> (LogicalId, RevisionId) {
    match read(s, b, key) {
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
fn check(
    s: &Service<SimDisk>,
    b: Binding,
    mode: Option<GateMode>,
) -> (Validation, Option<Decision>, u32) {
    match call(
        s,
        Request::Check {
            binding: b,
            cert_id: None,
            live_pos: Vec::new(),
            action: Some(Action {
                tool: ToolClass::Edit,
                paths: vec!["src/a.rs".into()],
                command: None,
            }),
            mode,
            harness_version: "2.1.306".into(),
            strict: false,
        },
    ) {
        Response::Check {
            validation,
            gate,
            updates_pending,
            ..
        } => (validation, gate, updates_pending),
        other => panic!("{other:?}"),
    }
}
fn valid(v: &Validation) -> bool {
    matches!(v, Validation::Valid | Validation::ValidRecertified { .. })
}
fn telemetry(s: &Service<SimDisk>) -> Vec<TelemetryEntry> {
    match s
        .handle(
            ADMIN,
            Request::Telemetry {
                since: 0,
                limit: 10_000,
            },
        )
        .response
    {
        Response::Telemetry { entries, .. } => entries,
        other => panic!("{other:?}"),
    }
}

#[test]
fn attach_is_idempotent_per_key_and_restarts_after_session_end() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let (a, created) = attach(&s, "k1");
    assert!(created);
    assert_eq!(attach(&s, "k1"), (a, false));
    let (b, _) = attach(&s, "k2");
    assert_ne!(a, b);
    assert_eq!(code(&call(&s, Request::SessionEnd { session: a })), "ok");
    let (a2, created) = attach(&s, "k1");
    assert!(created && a2 != a, "an ended key starts a new session");
    // The binding survives a restart.
    let s = restart(s, &disk);
    assert_eq!(attach(&s, "k1"), (a2, false));
}

#[test]
fn claimed_calls_bind_to_their_session_and_unclaimed_calls_are_counted() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let (a, _) = attach(&s, "A");
    let (b, _) = attach(&s, "B");
    create(&s, at(a, None), "db", "pg");
    // Identical calls from two sessions, claimed by their own tool-use ids.
    for (session, id) in [(a, "toolu_a"), (b, "toolu_b")] {
        let claim = Request::Claim {
            session,
            agent: None,
            call: id.into(),
        };
        assert_eq!(code(&call(&s, claim)), "ok");
    }
    assert_eq!(
        code(&read(
            &s,
            Binding::Call {
                id: "toolu_b".into()
            },
            "db"
        )),
        "ok"
    );
    // Only B read: A's check is still Empty, B's is valid.
    assert_eq!(check(&s, at(a, None), None).0, Validation::Empty);
    assert!(valid(&check(&s, at(b, None), None).0));
    // Every request of the claimed tool call resolves the same way; an
    // unknown id is never guessed.
    assert_eq!(
        code(&read(
            &s,
            Binding::Call {
                id: "toolu_b".into()
            },
            "db"
        )),
        "ok"
    );
    assert_eq!(check(&s, at(a, None), None).0, Validation::Empty);
    let unknown = read(
        &s,
        Binding::Call {
            id: "toolu_zz".into(),
        },
        "db",
    );
    assert_eq!(code(&unknown), "Unclaimed");
    // A session's claims end with it.
    assert_eq!(code(&call(&s, Request::SessionEnd { session: b })), "ok");
    assert_eq!(
        code(&read(
            &s,
            Binding::Call {
                id: "toolu_b".into()
            },
            "db"
        )),
        "Unclaimed"
    );
    assert!(
        telemetry(&s)
            .iter()
            .any(|e| matches!(e, TelemetryEntry::Unclaimed { .. }))
    );
}

#[test]
fn harness_keys_attach_on_first_use_and_unknown_agents_are_the_root() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let key = |agent: Option<&str>| Binding::Key {
        harness: "codex".into(),
        harness_session: "thread-1".into(),
        agent: agent.map(Into::into),
    };
    create(&s, key(None), "db", "pg");
    // An MCP call keyed by the harness session started the session.
    let (session, created) = match call(
        &s,
        Request::SessionAttach {
            harness: "codex".into(),
            harness_session: "thread-1".into(),
            repo: "/repo".into(),
            head_sha: "h".into(),
            branch: None,
            git_branch: None,
        },
    ) {
        Response::Attached { session, created } => (session, created),
        other => panic!("{other:?}"),
    };
    assert!(!created);
    // An agent id no hook reported reads as the root.
    head(&s, key(Some("child-x")), "db");
    assert!(valid(&check(&s, at(session, None), None).0));
    // Once a hook reports the child, its reads are its own.
    assert_eq!(
        check(&s, at(session, Some("child-y")), None).0,
        Validation::Valid
    );
}

/// T66 (ADR 0010 §7): a child's read is in the root's view; staleness of it
/// makes both checks Invalid; the root's own read governs its view.
#[test]
fn t66_child_reads_are_covered_by_the_root_and_root_reads_govern_its_view() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let (session, _) = attach(&s, "S");
    let other = attach(&s, "O").0;
    create(&s, at(other, None), "x", "v1");
    create(&s, at(other, None), "y", "v1");
    // Child reads x; the root never read it but its check covers it.
    let (lx, rx1) = head(&s, at(session, Some("c1")), "x");
    let (v, ..) = check(&s, at(session, None), None);
    assert!(valid(&v), "{v:?}");
    // Root reads y; then y is superseded elsewhere and the child re-reads it.
    let (ly, ry1) = head(&s, at(session, None), "y");
    supersede(&s, at(other, None), ly, ry1, "v2");
    head(&s, at(session, Some("c1")), "y");
    assert!(
        matches!(
            check(&s, at(session, None), None).0,
            Validation::Invalid { .. }
        ),
        "the root's own stale read governs its view"
    );
    assert!(valid(&check(&s, at(session, Some("c1")), None).0));
    // x goes stale: both the child and the root see it.
    supersede(&s, at(other, None), lx, rx1, "v2");
    assert!(matches!(
        check(&s, at(session, Some("c1")), None).0,
        Validation::Invalid { .. }
    ));
    assert!(matches!(
        check(&s, at(session, None), None).0,
        Validation::Invalid { .. }
    ));
}

#[test]
fn check_applies_the_gate_and_logs_it() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let (session, _) = attach(&s, "S");
    let other = attach(&s, "O").0;
    // Empty never allows (I6).
    assert_eq!(
        check(&s, at(session, None), Some(GateMode::Enforce)).1,
        Some(Decision::Warn)
    );
    create(&s, at(other, None), "db", "pg");
    let (l, r) = head(&s, at(session, None), "db");
    assert_eq!(
        check(&s, at(session, None), Some(GateMode::Enforce)).1,
        Some(Decision::Allow)
    );
    supersede(&s, at(other, None), l, r, "mysql");
    assert_eq!(
        check(&s, at(session, None), Some(GateMode::Warn)).1,
        Some(Decision::Warn)
    );
    assert_eq!(
        check(&s, at(session, None), Some(GateMode::Enforce)).1,
        Some(Decision::Deny)
    );
    assert_eq!(check(&s, at(session, None), None).1, None);
    let gates: Vec<_> = telemetry(&s)
        .into_iter()
        .filter_map(|e| match e {
            TelemetryEntry::Gate {
                decision, verdict, ..
            } => Some((decision, verdict)),
            _ => None,
        })
        .collect();
    assert_eq!(
        gates,
        vec![
            (Decision::Warn, "Empty".to_string()),
            (Decision::Allow, "Valid".to_string()),
            (Decision::Warn, "Invalid".to_string()),
            (Decision::Deny, "Invalid".to_string()),
        ]
    );
    // Gate decisions are telemetry that survives a restart.
    let s = restart(s, &disk);
    assert_eq!(
        telemetry(&s)
            .iter()
            .filter(|e| matches!(e, TelemetryEntry::Gate { .. }))
            .count(),
        4
    );
}

#[test]
fn updates_peek_until_acknowledged_per_agent() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let (session, _) = attach(&s, "S");
    let other = attach(&s, "O").0;
    create(&s, at(other, None), "db", "pg");
    let (l, r) = head(&s, at(session, None), "db");
    let updates = |agent: Option<&str>, ack: Option<u64>| match call(
        &s,
        Request::Updates {
            binding: at(session, agent),
            ack,
            max_items: 10,
        },
    ) {
        Response::Updates { token, items, .. } => (token, items),
        other => panic!("{other:?}"),
    };
    assert!(updates(None, None).1.is_empty(), "nothing changed");
    supersede(&s, at(other, None), l, r, "mysql");
    assert_eq!(
        check(&s, at(session, None), None).2,
        1,
        "pending, not consumed"
    );
    let (t1, items) = updates(None, None);
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].key.as_str(), items[0].cert_rev), ("db", r));
    assert_ne!(items[0].current, Some(r));
    // A peek does not consume; a dropped output is re-sent.
    let (t2, again) = updates(None, None);
    assert_eq!(again, items);
    assert_ne!(t1, t2);
    // An unknown token acknowledges nothing; an earlier, still-kept token
    // does (another peek in between, e.g. ctx_check, must not void the
    // prompt hook's acknowledgement: review minor 2).
    assert_eq!(updates(None, Some(t2 ^ 0x5555)).1, items);
    assert!(updates(None, Some(t1)).1.is_empty());
    // The child sees the root's read in its view and has its own state.
    assert_eq!(updates(Some("c1"), None).1.len(), 1);
}

#[test]
fn facts_apply_in_order_once_with_gaps_and_skips_and_survive_restart() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let (session, _) = attach(&s, "S");
    let stream = [5u8; 16];
    let action = |path: &str| Fact::Action {
        action: Action {
            tool: ToolClass::Edit,
            paths: vec![path.into()],
            command: None,
        },
        ok: true,
    };
    let send = |s: &Service<SimDisk>, packets: Vec<Packet>, skip| {
        call(
            s,
            Request::Facts {
                harness: "claude".into(),
                harness_session: "S".into(),
                stream,
                packets,
                skip,
            },
        )
    };
    let p = |seq, fact| Packet {
        seq,
        agent: None,
        fact,
    };
    assert_eq!(
        send(&s, vec![p(1, action("a.rs")), p(2, action("b.rs"))], None),
        Response::Facts {
            applied_through: 2,
            drift: Vec::new()
        }
    );
    // Duplicates are skipped; a gap is refused.
    assert_eq!(
        send(&s, vec![p(1, action("a.rs")), p(2, action("b.rs"))], None),
        Response::Facts {
            applied_through: 2,
            drift: Vec::new()
        }
    );
    assert_eq!(
        send(&s, vec![p(4, Fact::StreamEnd)], None),
        Response::Gap { expected: 3 }
    );
    // The marks survive a restart (durable StreamMark).
    let s = restart(s, &disk);
    assert_eq!(
        send(&s, vec![p(2, action("b.rs"))], None),
        Response::Facts {
            applied_through: 2,
            drift: Vec::new()
        }
    );
    // A range the client cannot resend is skipped and counted.
    assert_eq!(
        send(&s, vec![p(5, Fact::StreamEnd)], Some((3, 4))),
        Response::Facts {
            applied_through: 5,
            drift: Vec::new()
        }
    );
    assert!(
        telemetry(&s)
            .iter()
            .any(|e| matches!(e, TelemetryEntry::Skip { from: 3, to: 4, .. }))
    );
    let _ = session;
}

#[test]
fn drift_subtracts_declared_paths_of_this_and_concurrent_sessions() {
    let disk = SimDisk::new();
    let s = service(&disk);
    attach(&s, "S");
    attach(&s, "O");
    let facts = |key: &str, stream: u8, packets: Vec<Packet>| {
        let r = call(
            &s,
            Request::Facts {
                harness: "claude".into(),
                harness_session: key.into(),
                stream: [stream; 16],
                packets,
                skip: None,
            },
        );
        assert!(matches!(r, Response::Facts { .. }), "{r:?}");
    };
    let edit = |seq, agent: Option<&str>, path: &str, tool| Packet {
        seq,
        agent: agent.map(Into::into),
        fact: Fact::Action {
            action: Action {
                tool,
                paths: vec![path.into()],
                command: None,
            },
            ok: true,
        },
    };
    facts(
        "S",
        1,
        vec![
            edit(1, None, "a.rs", ToolClass::Edit),
            edit(2, Some("c1"), "d.rs", ToolClass::Patch),
            edit(3, None, "", ToolClass::Shell),
        ],
    );
    facts("O", 2, vec![edit(1, None, "c.rs", ToolClass::Write)]);
    facts(
        "S",
        1,
        vec![Packet {
            seq: 4,
            agent: None,
            fact: Fact::Drift {
                done: Some(vec![
                    "a.rs".into(),
                    "b.rs".into(),
                    "c.rs".into(),
                    "d.rs".into(),
                ]),
            },
        }],
    );
    let report = telemetry(&s)
        .into_iter()
        .find_map(|e| match e {
            TelemetryEntry::Drift { report, .. } => Some(report),
            _ => None,
        })
        .unwrap();
    assert_eq!(report.undeclared, vec!["b.rs".to_string()]);
    assert!(report.unrealised.is_empty());
    assert_eq!(report.shell_actions, 1);
}

#[test]
fn briefing_is_advisory_and_records_writes_for_coverage() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let (session, _) = attach(&s, "S");
    create(&s, at(session, None), "db", "pg");
    match call(
        &s,
        Request::Briefing {
            binding: Some(at(session, None)),
            budget_chars: 1500,
        },
    ) {
        Response::Briefing { items, stale, .. } => {
            assert_eq!(items.len(), 1);
            assert_eq!(stale, 0);
            assert!(items[0].handle.is_none());
        }
        other => panic!("{other:?}"),
    }
    // The briefing added no read: the session is still Empty (I6).
    assert_eq!(check(&s, at(session, None), None).0, Validation::Empty);
    assert!(telemetry(&s).iter().any(|e| matches!(
        e,
        TelemetryEntry::Write {
            with_deps: false,
            ..
        }
    )));
    // Telemetry is admin-only.
    assert_eq!(
        code(&call(&s, Request::Telemetry { since: 0, limit: 1 })),
        "ChannelRestricted"
    );
}

/// G4 (ADR 0010 §11): the daemon dies between applying a batch and syncing
/// its stream mark, or after the sync but before the reply reached the hook.
/// Either way the hook resends and every fact is applied exactly once.
#[test]
fn facts_survive_a_daemon_crash_at_either_side_of_the_sync() {
    let drift = || Packet {
        seq: 1,
        agent: None,
        fact: Fact::Drift {
            done: Some(vec!["x.rs".into()]),
        },
    };
    let send = |s: &Service<SimDisk>| {
        call(
            s,
            Request::Facts {
                harness: "claude".into(),
                harness_session: "S".into(),
                stream: [9; 16],
                packets: vec![drift()],
                skip: None,
            },
        )
    };
    let drifts = |s: &Service<SimDisk>| {
        telemetry(s)
            .iter()
            .filter(|e| matches!(e, TelemetryEntry::Drift { .. }))
            .count()
    };
    // Before the sync: the reply is an error, nothing is durable.
    let disk = SimDisk::new();
    let s = service(&disk);
    attach(&s, "S");
    disk.fail_next("sync");
    assert_ne!(code(&send(&s)), "ok");
    drop(s);
    let disk = disk.crash(contextdb_store::TailFate::Torn, 1);
    let s = service(&disk);
    assert!(matches!(
        send(&s),
        Response::Facts {
            applied_through: 1,
            ..
        }
    ));
    assert_eq!(drifts(&s), 1);
    // After the sync, before the reply: the resend is a duplicate.
    let s = restart(s, &disk);
    let disk = disk.crash(contextdb_store::TailFate::Torn, 2);
    drop(s);
    let s = service(&disk);
    assert!(matches!(
        send(&s),
        Response::Facts {
            applied_through: 1,
            ref drift,
        } if drift.is_empty()
    ));
    assert_eq!(drifts(&s), 1, "applied once across the crash");
}

/// Review B1 and M1: many threads attaching (new and shared keys), ending
/// sessions, delivering drift and checking at once never deadlock, and
/// concurrent attaches of one key start exactly one session.
#[test]
fn concurrent_attach_end_facts_and_check_neither_deadlock_nor_split() {
    use std::sync::{Arc, mpsc};
    let disk = SimDisk::new();
    let s = Arc::new(service(&disk));
    let (done_tx, done_rx) = mpsc::channel();
    let shared: Vec<_> = (0..8)
        .map(|t| {
            let (s, done) = (s.clone(), done_tx.clone());
            std::thread::spawn(move || {
                let mut ids = Vec::new();
                for i in 0..200 {
                    // Every thread attaches the same per-round key...
                    ids.push(attach(&s, &format!("shared-{i}")).0);
                    // ...and its own, which it then uses and ends.
                    let (own, _) = attach(&s, &format!("own-{t}-{i}"));
                    let _ = s.handle(
                        AGENT,
                        Request::Facts {
                            harness: "claude".into(),
                            harness_session: format!("own-{t}-{i}"),
                            stream: [t as u8; 16],
                            packets: vec![Packet {
                                seq: i as u64 + 1,
                                agent: None,
                                fact: Fact::Drift {
                                    done: Some(vec!["x".into()]),
                                },
                            }],
                            skip: None,
                        },
                    );
                    let _ = check(&s, at(own, None), Some(GateMode::Warn));
                    let _ = s.handle(AGENT, Request::SessionEnd { session: own });
                }
                done.send(()).unwrap();
                ids
            })
        })
        .collect();
    drop(done_tx);
    for _ in 0..8 {
        done_rx
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("deadlock: a worker did not finish");
    }
    let all: Vec<Vec<SessionId>> = shared.into_iter().map(|h| h.join().unwrap()).collect();
    for i in 0..200 {
        let first = all[0][i];
        assert!(
            all.iter().all(|ids| ids[i] == first),
            "key shared-{i} split"
        );
    }
}
