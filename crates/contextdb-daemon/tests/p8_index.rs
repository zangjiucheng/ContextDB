//! P8 milestone 1 (ADR 0016 §2, §4.1): the recall projection's health,
//! the bounded wait, `ctx reindex`, and the stored renderings. T56, T86,
//! T87 (projection half), T94.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::levels;
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const DIR: &str = "/db";

fn service(disk: &SimDisk, recall: Option<&Path>) -> Service<SimDisk> {
    let dir = Path::new(DIR);
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    Service::with_recall(actor::spawn(store).unwrap(), log, events, recall).unwrap()
}
fn shutdown(s: Service<SimDisk>) {
    assert_eq!(s.handle(ADMIN, Request::Shutdown).response, Response::Done);
    drop(s);
}
fn call(s: &Service<SimDisk>, req: Request) -> Envelope {
    s.handle(AGENT, req)
}
fn attach(s: &Service<SimDisk>, key: &str) -> Binding {
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
    )
    .response
    {
        Response::Attached { session, .. } => Binding::root(session),
        other => panic!("{other:?}"),
    }
}
fn create(s: &Service<SimDisk>, b: &Binding, key: &str, body: &str) -> CommitResult {
    match call(
        s,
        Request::Record {
            binding: b.clone(),
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
    )
    .response
    {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
fn supersede(s: &Service<SimDisk>, b: &Binding, l: LogicalId, r: RevisionId, body: &str) {
    let resp = call(
        s,
        Request::Supersede {
            binding: b.clone(),
            logical: l,
            body: body.as_bytes().to_vec(),
            expected_head: r,
            adopt_as_base: true,
            carry_rules: false,
            reason: "changed".into(),
            client_txn_id: None,
        },
    )
    .response;
    assert!(matches!(resp, Response::Committed { .. }), "{resp:?}");
}
struct Recalled {
    epoch: Epoch,
    items: Vec<Item>,
    index: IndexReport,
}
fn recall(s: &Service<SimDisk>, b: &Binding, query: &str, wait_ms: u32) -> Recalled {
    let env = call(
        s,
        Request::Recall {
            binding: b.clone(),
            query: query.into(),
            scope: "repo".into(),
            mode: Mode::Advisory,
            limit: 100,
            index_wait_ms: wait_ms,
            opts: Default::default(),
        },
    );
    match env.response {
        Response::Recall { items, index, .. } => Recalled {
            epoch: env.epoch,
            items,
            index,
        },
        other => panic!("{other:?}"),
    }
}
/// What a recall shows, as a set: key, revision and label (T56 compares
/// sets because bm25 ties may break differently after a rebuild).
fn shown(items: &[Item]) -> BTreeSet<(String, u32, String)> {
    (items.iter())
        .map(|i| (i.key.clone(), i.rev.0, format!("{:?}", i.summary)))
        .collect()
}
fn ready(s: &Service<SimDisk>, b: &Binding, query: &str) -> Recalled {
    for _ in 0..400 {
        let r = recall(s, b, query, 2_000);
        if matches!(r.index.fts, IndexState::Ready { .. }) {
            return r;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the index never became ready");
}
fn temp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "ctxd-p8-{tag}-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}
fn remove(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
fn corpus(s: &Service<SimDisk>, b: &Binding) {
    for n in 0..40 {
        create(
            s,
            b,
            &format!("k{n}"),
            &format!(
                "# Decision {n}\n\nUse the otter queue for batch {n}.\n\n## Why\nlatency {}\n",
                n % 7
            ),
        );
    }
}

/// T94: the index state is in every recall and in `ctx status`; a reindex
/// answers with the rebuild (or, if it already finished, a ready index),
/// and the index is ready again afterwards.
#[test]
fn t94_index_state_in_recall_status_and_reindex() {
    let disk = SimDisk::new();
    let s = service(&disk, None);
    let a = attach(&s, "A");
    corpus(&s, &a);
    let r = ready(&s, &a, "otter");
    assert_eq!(r.index.fts, IndexState::Ready { watermark: r.epoch });
    assert_eq!(r.index.vectors, IndexState::Disabled);
    match s.handle(ADMIN, Request::Status).response {
        Response::Status { index, .. } => {
            assert!(matches!(index.fts, IndexState::Ready { .. }), "{index:?}")
        }
        other => panic!("{other:?}"),
    }
    // Agents cannot reindex.
    match call(
        &s,
        Request::Reindex {
            target: ReindexTarget::Fts,
        },
    )
    .response
    {
        Response::Error { code, .. } => assert_eq!(code, "ChannelRestricted"),
        other => panic!("{other:?}"),
    }
    let env = s.handle(
        ADMIN,
        Request::Reindex {
            target: ReindexTarget::All,
        },
    );
    match env.response {
        Response::Reindexed { index } => assert!(
            matches!(index.fts, IndexState::Rebuilding { .. })
                || index.fts
                    == IndexState::Ready {
                        watermark: env.epoch
                    },
            "{index:?}"
        ),
        other => panic!("{other:?}"),
    }
    let after = ready(&s, &a, "otter");
    assert_eq!(shown(&after.items), shown(&r.items));
    shutdown(s);
}

/// T56: a recall shows the same items with the same labels before and
/// after `ctx reindex`, and after the projection file is deleted.
#[test]
fn t56_results_and_labels_survive_reindex_and_a_deleted_projection() {
    let path = temp_path("t56");
    let disk = SimDisk::new();
    let before = {
        let s = service(&disk, Some(&path));
        let a = attach(&s, "A");
        corpus(&s, &a);
        let r = ready(&s, &a, "otter batch");
        assert_eq!(r.items.len(), 40);
        s.handle(
            ADMIN,
            Request::Reindex {
                target: ReindexTarget::Fts,
            },
        );
        let again = ready(&s, &a, "otter batch");
        assert_eq!(shown(&again.items), shown(&r.items), "after ctx reindex");
        shutdown(s);
        r
    };
    remove(&path);
    let s = service(&disk, Some(&path));
    let a = attach(&s, "B");
    let rebuilt = ready(&s, &a, "otter batch");
    assert_eq!(
        shown(&rebuilt.items),
        shown(&before.items),
        "after deleting the projection"
    );
    shutdown(s);
    remove(&path);
}

/// T86: whatever the index's state, every item a recall returns is the
/// current head, and a `Ready` index is at the response's epoch; the
/// bounded wait finds a record committed just before the call.
#[test]
fn t86_lag_never_returns_a_non_current_revision() {
    let disk = SimDisk::new();
    let s = service(&disk, None);
    let a = attach(&s, "A");
    corpus(&s, &a);
    ready(&s, &a, "otter");
    let mut heads: Vec<(LogicalId, RevisionId)> = Vec::new();
    for n in 0..40u32 {
        let r = recall(&s, &a, &format!("k{n}"), 0);
        let it = r.items.iter().find(|i| i.key == format!("k{n}")).unwrap();
        heads.push((it.logical, it.rev));
    }
    let mut lagging = 0;
    for round in 0..30 {
        let n = (round * 7) % 40;
        let (l, r) = heads[n];
        supersede(
            &s,
            &a,
            l,
            r,
            &format!("# Decision {n}\n\nSwitch to the heron queue, round {round}.\n"),
        );
        // No wait: the index may lag the snapshot.
        let got = recall(&s, &a, "otter heron", 0);
        if matches!(got.index.fts, IndexState::Lagging { .. }) {
            lagging += 1;
        }
        if let IndexState::Ready { watermark } = got.index.fts {
            assert_eq!(watermark, got.epoch);
        }
        let fresh = recall(&s, &a, &format!("k{n}"), 0);
        let head = fresh
            .items
            .iter()
            .find(|i| i.key == format!("k{n}"))
            .unwrap();
        heads[n] = (head.logical, head.rev);
        for i in &got.items {
            let now = recall(&s, &a, &i.key, 0);
            let cur = now.items.iter().find(|x| x.key == i.key).unwrap();
            assert!(
                i.rev == cur.rev || now.epoch > got.epoch,
                "{} returned at rev {} while the head is {}",
                i.key,
                i.rev.0,
                cur.rev.0
            );
        }
        // With the bounded wait the superseded text is found at once.
        let waited = recall(&s, &a, &format!("heron round {round}"), 2_000);
        assert!(
            waited.items.iter().any(|i| i.key == format!("k{n}")),
            "round {round}: {:?}",
            waited.index
        );
    }
    eprintln!("lagging responses without a wait: {lagging}/30");
    shutdown(s);
}

/// T87 (projection half): the stored abstract and outline equal a
/// recomputation from the payload, including a payload over the 64 KiB
/// body cut.
#[test]
fn t87_stored_renderings_equal_recomputation() {
    let disk = SimDisk::new();
    let s = service(&disk, None);
    let a = attach(&s, "A");
    let bodies = [
        ("short", "Just one line.".to_string()),
        (
            "doc",
            "---\nk: v\n---\n# Title\n\nFirst para.\n\n## A\nalpha\n## B\nbeta\n".to_string(),
        ),
        (
            "huge",
            format!(
                "# Huge\n\n{}\n## Tail section\nonly after 64 KiB\n",
                "word ".repeat(20_000)
            ),
        ),
    ];
    let mut revs = Vec::new();
    for (k, b) in &bodies {
        let res = create(&s, &a, k, b);
        revs.push(res.allocations.new_revisions[0]);
    }
    ready(&s, &a, "Just");
    for ((k, b), rev) in bodies.iter().zip(&revs) {
        let r = recall(&s, &a, k, 0);
        let item = r.items.iter().find(|i| i.key == *k).unwrap();
        assert_eq!(item.rev, *rev);
        let stored = s.recall_levels(item.logical, *rev).expect("row");
        assert_eq!(stored.abstract_, levels::abstract_of(b.as_bytes()), "{k}");
        assert_eq!(stored.outline, levels::outline_of(b.as_bytes()), "{k}");
    }
    let huge = s
        .recall_levels(recall(&s, &a, "huge", 0).items[0].logical, revs[2])
        .unwrap();
    assert!(
        huge.outline.text.contains("## Tail section"),
        "outline from the whole payload"
    );
    shutdown(s);
}
