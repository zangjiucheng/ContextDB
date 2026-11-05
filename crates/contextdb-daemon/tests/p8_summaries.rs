//! P8 milestone 4 (ADR 0016 §6): a Summary's pending count, its
//! visibility on read, recall and briefing, and the "digest early stop",
//! which is revision deduplication by `(logical, content)`. T90.
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
fn attach(s: &Service<SimDisk>, key: &str) -> Binding {
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
        Response::Attached { session, .. } => Binding::root(session),
        other => panic!("{other:?}"),
    }
}
fn committed(r: Response) -> CommitResult {
    match r {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
fn create(
    s: &Service<SimDisk>,
    b: &Binding,
    key: &str,
    kind: RecordKind,
    body: &str,
    deps: Vec<Dep>,
) -> CommitResult {
    committed(
        s.handle(
            AGENT,
            Request::Record {
                binding: b.clone(),
                kind,
                body: body.as_bytes().to_vec(),
                target: WriteTarget::Create {
                    key: key.into(),
                    scope_lane: "repo".into(),
                },
                // A Summary rests on its rule, not on a base assertion.
                adopt_as_base: kind != RecordKind::Summary,
                deps,
                client_txn_id: None,
            },
        )
        .response,
    )
}
fn supersede(
    s: &Service<SimDisk>,
    b: &Binding,
    l: LogicalId,
    head: RevisionId,
    body: &str,
) -> Response {
    s.handle(
        AGENT,
        Request::Supersede {
            binding: b.clone(),
            logical: l,
            body: body.as_bytes().to_vec(),
            expected_head: head,
            adopt_as_base: true,
            carry_rules: false,
            reason: "edit".into(),
            client_txn_id: None,
        },
    )
    .response
}
fn read(s: &Service<SimDisk>, b: &Binding, key: &str) -> Item {
    match (s.handle(
        AGENT,
        Request::Read {
            binding: b.clone(),
            target: Target::Key(key.into()),
            mode: Mode::Advisory,
            level: None,
        },
    ))
    .response
    {
        Response::Read { item, .. } => item,
        other => panic!("{other:?}"),
    }
}
fn briefing(s: &Service<SimDisk>, b: &Binding) -> Vec<Item> {
    match (s.handle(
        AGENT,
        Request::Briefing {
            binding: Some(b.clone()),
            budget_chars: 1_500,
        },
    ))
    .response
    {
        Response::Briefing { summaries, .. } => summaries,
        other => panic!("{other:?}"),
    }
}
fn certified_recall(s: &Service<SimDisk>, b: &Binding, query: &str) -> (Vec<Item>, Vec<Item>) {
    match (s.handle(
        AGENT,
        Request::Recall {
            binding: b.clone(),
            query: query.into(),
            scope: String::new(),
            mode: Mode::Certified,
            limit: 10,
            index_wait_ms: 0,
            opts: RecallOpts::default(),
        },
    ))
    .response
    {
        Response::Recall {
            items, withheld, ..
        } => (items, withheld),
        other => panic!("{other:?}"),
    }
}
fn count(moved: u32, total: u32) -> Option<PremiseCount> {
    Some(PremiseCount { moved, total })
}

/// T90: the pending count follows the premises against the Verified
/// vector; reverting a premise to its validated text reuses the revision,
/// so the vector, the verdict and the label return without a new
/// validation; re-recording identical text changes nothing.
#[test]
fn t90_pending_count_and_the_digest_early_stop() {
    let disk = SimDisk::new();
    let s = service(&disk);
    let a = attach(&s, "A");
    let mut premises = Vec::new();
    for n in 0..3 {
        let r = create(
            &s,
            &a,
            &format!("p{n}"),
            RecordKind::Fact,
            &format!("fact {n}"),
            Vec::new(),
        );
        premises.push((
            r.allocations.new_logicals[0],
            r.allocations.new_revisions[0],
        ));
    }
    let deps = (premises.iter())
        .map(|(l, r)| Dep {
            target: Target::Pinned {
                logical: *l,
                rev: *r,
            },
            kind: EdgeKind::Sem,
        })
        .collect();
    let digest = create(&s, &a, "digest", RecordKind::Summary, "three facts", deps);
    let rule = digest.allocations.new_rules[0];
    // No Verified record: counted against the asserted vector, which the
    // rule's assertion stands for (SD10), so the Summary starts CERTIFIED.
    let item = read(&s, &a, "digest");
    assert_eq!(
        (item.summary.clone(), item.premises),
        (Summary::Certified, count(0, 3))
    );
    // Non-Summary items carry no count.
    assert_eq!(read(&s, &a, "p0").premises, None);
    // An admin validation of the current vector.
    let vector: HeadVector = premises.clone();
    let v = s.handle(
        ADMIN,
        Request::Raw {
            blobs: Vec::new(),
            batch: Batch::new(vec![Op::RecordValidation {
                rule: RuleRef::Existing(rule),
                curvec_seen: vector.clone(),
                verdict: Verdict::Verified,
                validator: "admin".into(),
                meta: None,
                if_absent: false,
            }]),
        },
    );
    assert!(matches!(v.response, Response::Committed { .. }), "{v:?}");
    let item = read(&s, &a, "digest");
    assert_eq!(
        (item.summary.clone(), item.premises),
        (Summary::Certified, count(0, 3))
    );
    assert!(briefing(&s, &a).is_empty());
    // A premise moves: PENDING, 1 of 3; withheld in certified recall with
    // its count; listed by the briefing.
    let (p0, r0) = premises[0];
    let moved = committed(supersede(&s, &a, p0, r0, "fact 0, revised"));
    let r0b = moved.allocations.new_revisions[0];
    let item = read(&s, &a, "digest");
    assert!(
        matches!(item.summary, Summary::Pending(_)),
        "{:?}",
        item.summary
    );
    assert_eq!(item.premises, count(1, 3));
    let (items, withheld) = certified_recall(&s, &a, "digest");
    assert!(items.iter().all(|i| i.key != "digest"));
    let w = withheld.iter().find(|i| i.key == "digest").unwrap();
    assert_eq!((w.premises, w.body.clone()), (count(1, 3), None));
    let listed = briefing(&s, &a);
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].key.as_str(), listed[0].premises),
        ("digest", count(1, 3))
    );
    // Revert to the validated text: the same RevisionId comes back, so the
    // vector equals the validated one and the stored verdict applies.
    let back = committed(supersede(&s, &a, p0, r0b, "fact 0"));
    // The commit names the reused revision, not a fresh one.
    assert_eq!(
        back.allocations.new_revisions,
        vec![r0],
        "the validated revision"
    );
    let head = read(&s, &a, "p0");
    assert_eq!(head.rev, r0, "reverting reuses the revision");
    let item = read(&s, &a, "digest");
    assert_eq!(
        (item.summary.clone(), item.premises),
        (Summary::Certified, count(0, 3))
    );
    assert!(briefing(&s, &a).is_empty());
    // Re-recording the head's identical text leaves everything as it is.
    // SPEC §6.3: `Committed`, naming the head itself, with no head move,
    // base flip or status change.
    let same = committed(supersede(&s, &a, p0, r0, "fact 0"));
    assert_eq!(same.allocations.new_revisions, vec![r0]);
    assert!(same.net_delta.head_moves.is_empty() && same.net_delta.base_flips.is_empty());
    assert!(same.status_diff.is_empty() && same.l0_flips.is_empty());
    assert_eq!(read(&s, &a, "p0").rev, r0);
    let item = read(&s, &a, "digest");
    assert_eq!(
        (item.summary.clone(), item.premises),
        (Summary::Certified, count(0, 3))
    );
    s.handle(ADMIN, Request::Shutdown);
}
