//! P8 milestone 3 (ADR 0016 §4–§5): read levels, the ledger's highest
//! level, session log format 4 and packing. T55, T88.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::levels::{self, Level};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use std::path::Path;
use std::time::Duration;

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
fn shutdown(s: Service<SimDisk>) {
    assert_eq!(s.handle(ADMIN, Request::Shutdown).response, Response::Done);
}
fn attach(s: &Service<SimDisk>, key: &str) -> (SessionId, Binding) {
    match (s.handle(
        AGENT,
        Request::SessionAttach {
            harness: "claude".into(),
            harness_session: key.into(),
            repo: "/repo".into(),
            head_sha: "h".into(),
            branch: None,
            git_branch: None,
        },
    ))
    .response
    {
        Response::Attached { session, .. } => (session, Binding::root(session)),
        other => panic!("{other:?}"),
    }
}
fn create(s: &Service<SimDisk>, b: &Binding, key: &str, body: &str, certified: bool) -> LogicalId {
    match (s.handle(
        AGENT,
        Request::Record {
            binding: b.clone(),
            kind: RecordKind::Decision,
            body: body.as_bytes().to_vec(),
            target: WriteTarget::Create {
                key: key.into(),
                scope_lane: "repo".into(),
            },
            deps: Vec::new(),
            adopt_as_base: certified,
            client_txn_id: None,
        },
    ))
    .response
    {
        Response::Committed { result } => result.allocations.new_logicals[0],
        other => panic!("{key}: {other:?}"),
    }
}
fn read(
    s: &Service<SimDisk>,
    b: &Binding,
    key: &str,
    mode: Mode,
    level: Option<Level>,
) -> Response {
    s.handle(
        AGENT,
        Request::Read {
            binding: b.clone(),
            target: Target::Key(key.into()),
            mode,
            level,
        },
    )
    .response
}
struct Got {
    items: Vec<Item>,
    withheld: Vec<Item>,
    omitted: u32,
    estimate: Option<String>,
}
fn recall(
    s: &Service<SimDisk>,
    b: &Binding,
    query: &str,
    mode: Mode,
    level: Option<Level>,
    budget: Option<u32>,
) -> Got {
    for _ in 0..400 {
        match (s.handle(
            AGENT,
            Request::Recall {
                binding: b.clone(),
                query: query.into(),
                scope: String::new(),
                mode,
                limit: 100,
                index_wait_ms: 2_000,
                opts: RecallOpts {
                    level,
                    budget_tokens: budget,
                    ..RecallOpts::default()
                },
            },
        ))
        .response
        {
            Response::Recall {
                items,
                withheld,
                index,
                omitted,
                estimate,
                ..
            } => {
                if matches!(index.fts, IndexState::Ready { .. }) {
                    return Got {
                        items,
                        withheld,
                        omitted,
                        estimate,
                    };
                }
            }
            other => panic!("{other:?}"),
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the index never became ready")
}
fn body(n: usize) -> String {
    format!(
        "# Decision {n}\n\nUse the otter queue for batch {n}; {}\n\n## Why\nlatency {}\n\n## Risks\n{}\n",
        "because ".repeat(n * 7),
        n % 5,
        "slow ".repeat(n * 11)
    )
}

/// T55: for every budget, each included item and each withheld entry has
/// the label it has without a budget, the included items are a prefix of
/// the unbudgeted order, and a header is never cut off its label.
#[test]
fn t55_packing_keeps_labels_and_order_for_every_budget() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let (_, a) = attach(&s, "A");
    for n in 0..24 {
        create(&s, &a, &format!("otter-{n:02}"), &body(n), n % 3 != 0);
    }
    for mode in [Mode::Advisory, Mode::Certified] {
        let base = recall(&s, &a, "otter", mode, None, None);
        assert!(base.items.iter().all(|i| i.body.is_none()), "header shape");
        assert_eq!(base.omitted, 0);
        assert_eq!(base.estimate, None);
        let label = |i: &Item| (i.key.clone(), i.rev, i.summary.clone());
        for budget in (0..1_200).step_by(7).chain([5_000, 50_000]) {
            let got = recall(&s, &a, "otter", mode, None, Some(budget));
            assert_eq!(got.estimate.as_deref(), Some("bytes/4 v1"));
            assert_eq!(
                got.items.len() + got.omitted as usize,
                base.items.len(),
                "budget {budget}"
            );
            for (i, b) in got.items.iter().zip(&base.items) {
                assert_eq!(label(i), label(b), "budget {budget}: prefix and label");
                let r = i.body.as_ref().expect("budgeted items are rendered");
                assert!(r.level <= Level::Outline, "default level is outline");
                if mode == Mode::Certified {
                    assert_eq!(i.summary, Summary::Certified);
                }
            }
            assert_eq!(
                got.withheld.iter().map(label).collect::<Vec<_>>(),
                base.withheld.iter().map(label).collect::<Vec<_>>()
            );
            assert!(got.withheld.iter().all(|w| w.body.is_none()), "no text");
            // Omitted items get no handle: only included items carry one.
            if mode == Mode::Certified {
                assert!(got.items.iter().all(|i| i.handle.is_some()));
            }
        }
    }
    shutdown(s);
}

/// Levels in recall and read equal the pure functions; `full` through
/// recall is the payload; a read below full returns no payload.
#[test]
fn levels_render_like_the_pure_functions() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let (_, a) = attach(&s, "A");
    let text = body(4);
    create(&s, &a, "doc", &text, true);
    for level in [Level::Header, Level::Abstract, Level::Outline, Level::Full] {
        let got = recall(&s, &a, "doc", Mode::Advisory, Some(level), None);
        let r = got.items[0].body.clone().unwrap();
        assert_eq!(r.level, level);
        let want = levels::render(text.as_bytes(), level);
        assert_eq!(r.text, want.text, "{level:?}");
        if level != Level::Header {
            assert_eq!(r.cut, want.cut);
        } else {
            assert!(r.cut);
        }
        match read(&s, &a, "doc", Mode::Certified, Some(level)) {
            Response::Read { item, payload, .. } if level == Level::Full => {
                assert_eq!(payload.unwrap(), text.as_bytes());
                assert!(item.body.is_none());
            }
            Response::Read { item, payload, .. } => {
                assert!(payload.is_none());
                assert_eq!(item.body.unwrap().text, want.text);
            }
            other => panic!("{other:?}"),
        }
    }
    // Certified mode still fails closed on an uncertified head, at any level.
    create(&s, &a, "weak", "w", false);
    for level in [None, Some(Level::Abstract)] {
        assert!(matches!(
            read(&s, &a, "weak", Mode::Certified, level),
            Response::Error { code, .. } if code == "NotCertified"
        ));
    }
    shutdown(s);
}

/// T88: every expansion is a read event; the ledger keeps the highest level
/// read of a revision and never lowers it; a new revision starts over; and
/// the level survives a restart (session log format 4).
#[test]
fn t88_ledger_keeps_the_highest_level_across_restart() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let (session, a) = attach(&s, "A");
    let l = create(&s, &a, "doc", &body(3), true);
    // A certified recall at header shape records `header`.
    recall(&s, &a, "doc", Mode::Certified, None, None);
    let rev = s.ledger_level(session, l).unwrap().0;
    assert_eq!(s.ledger_level(session, l), Some((rev, Level::Header)));
    read(&s, &a, "doc", Mode::Certified, Some(Level::Outline));
    assert_eq!(s.ledger_level(session, l), Some((rev, Level::Outline)));
    read(&s, &a, "doc", Mode::Certified, Some(Level::Abstract));
    recall(&s, &a, "doc", Mode::Certified, Some(Level::Header), None);
    assert_eq!(
        s.ledger_level(session, l),
        Some((rev, Level::Outline)),
        "never lowered"
    );
    // Flush, then reopen from the session log.
    shutdown(s);
    let (_, events, _) = SessionLog::open(disk.clone(), Path::new("/db"), false, 1 << 20).unwrap();
    let levels: Vec<Level> = (events.iter())
        .filter_map(|e| match e {
            contextdb_store::session::SessionEvent::Read {
                session_id, level, ..
            } if *session_id == session => Some(*level),
            _ => None,
        })
        .collect();
    assert_eq!(
        levels,
        [
            Level::Header,
            Level::Outline,
            Level::Abstract,
            Level::Header
        ],
        "each expansion is its own event"
    );
    let s = service(&disk);
    assert_eq!(s.ledger_level(session, l), Some((rev, Level::Outline)));
    // A re-read at full raises it.
    let (_, a2) = attach(&s, "A");
    read(&s, &a2, "doc", Mode::Certified, None);
    assert_eq!(s.ledger_level(session, l), Some((rev, Level::Full)));
    shutdown(s);
}
