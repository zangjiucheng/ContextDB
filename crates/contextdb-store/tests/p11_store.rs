//! P11 milestone 2 (ADR 0019 §3.5, §3.8, §3.9, §4): the store acknowledges
//! a commit whose maintenance is pending, persists exactly what it
//! acknowledged (a format-7 `settling` frame), and settles before every
//! kernel entry. Format-6 logs stay readable with their checksums.
use contextdb_core::Budget;
use contextdb_model::*;
use contextdb_store::*;
use contextdb_testkit::{Shape, shape};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

const N: usize = 300;
const PAGE: usize = 1_000_000;

fn small() -> Budget {
    Budget {
        units: 64,
        chunk: 16,
    }
}
fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("contextdb-p11-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
fn options(dir: &Path, lazy: Option<Budget>) -> OpenOptions {
    OpenOptions {
        projections: ProjectionFile::Path(dir.join(PROJECTIONS_FILE)),
        lazy,
        ..OpenOptions::default()
    }
}
fn open(dir: &Path, lazy: Option<Budget>) -> Store<StdDisk> {
    Store::open(StdDisk::default(), dir, &options(dir, lazy))
        .unwrap()
        .0
}
/// A hub of `N` revisions: 0, 1 and 2 are bases, every other revision
/// concludes a rule on all three, so toggling revision 0 reaches them all.
fn hub(s: &mut Store<StdDisk>) {
    let blob = s.put_blob(b"payload").unwrap();
    let mut generated = shape(Shape::Hub, N, 42);
    for b in &mut generated.batches {
        for op in &mut b.ops {
            if let Op::CreateLogical { content, .. } = op {
                *content = blob;
            }
        }
    }
    for b in &generated.batches {
        s.commit(b).unwrap();
    }
}
fn toggle(on: bool) -> Batch {
    let rev = RevisionId(0).into();
    Batch::new(vec![if on {
        Op::AssertBase { rev }
    } else {
        Op::RetractBase { rev }
    }])
}
fn create(s: &mut Store<StdDisk>, key: &str) -> Batch {
    let content = s.put_blob(key.as_bytes()).unwrap();
    Batch::new(vec![Op::CreateLogical {
        origin: Default::default(),
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content,
        adopt_as_base: true,
    }])
}
fn log_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir.join("log"))
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum()
}
fn segments(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir.join("log"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    out.sort();
    out
}
/// Every projection table but the watermark's checksum, which names
/// frames that differ between an eager and a lazy store.
fn dump(dir: &Path) -> Vec<String> {
    let c = Connection::open(dir.join(PROJECTIONS_FILE)).unwrap();
    let mut out = Vec::new();
    for q in [
        "SELECT 'diff', branch, epoch, revision, hex(post) FROM diff_log ORDER BY 2, 3, 4",
        "SELECT 'head', branch, logical_id, rev_id, from_epoch || ':' || ifnull(to_epoch, '') FROM head_history ORDER BY 2, 3, 5",
        "SELECT 'base', branch, rev_id, asserted_epoch, retracted_epoch FROM base_history ORDER BY 2, 3, 4",
        "SELECT 'rule', branch, rule_id, enabled, from_epoch FROM rule_enabled ORDER BY 2, 3, 5",
        "SELECT 'mark', branch, scope, hex(position), from_epoch FROM watermark_history ORDER BY 2, 3, 5",
        "SELECT 'branch', id, name, parent, fork_epoch || ':' || ifnull(archived_epoch, '') FROM branches ORDER BY 2",
        "SELECT 'visible', branch, kind, id, epoch FROM branch_visibility ORDER BY 2, 3, 4",
        "SELECT 'watermark', id, epoch, 0, 0 FROM projection_watermark",
    ] {
        let mut s = c.prepare(q).unwrap();
        let rows = s
            .query_map([], |r| {
                let v = |i| r.get::<_, rusqlite::types::Value>(i);
                Ok(format!("{:?}", (v(0)?, v(1)?, v(2)?, v(3)?, v(4)?)))
            })
            .unwrap();
        out.extend(rows.map(Result::unwrap));
    }
    out
}
fn changes(s: &Store<StdDisk>) -> ChangePage {
    s.changes(MAIN, Epoch(0), PAGE).unwrap()
}
/// An eager store given `batches` after the hub: what a lazy store must
/// equal once it settles.
fn eager(name: &str, batches: &[Batch]) -> (PathBuf, Store<StdDisk>) {
    let dir = tmp(name);
    let mut s = open(&dir, None);
    hub(&mut s);
    for b in batches {
        s.commit(b).unwrap();
    }
    (dir, s)
}

#[test]
fn lazy_ack_persists_the_ack_and_settles_to_the_eager_state() {
    let (eager_dir, eager_store) = eager("eager-ack", &[toggle(false)]);
    let eager_result = eager_store.read_row(eager_store.epoch()).unwrap();
    let eager_result = eager_result.result().unwrap();
    // Revision 0 and every conclusion change; bases 1 and 2 do not.
    assert_eq!(eager_result.status_diff.len(), N - 2);
    let dir = tmp("lazy-ack");
    let mut s = open(&dir, Some(small()));
    hub(&mut s);
    // The hub's rules batch is itself deferred under this budget.
    s.settle().unwrap();
    s.take_settled();
    let before = log_bytes(&dir);
    let acked = s.commit(&toggle(false)).unwrap();
    let e = acked.epoch;
    assert_eq!(s.frontier_epoch(), Some(e));
    assert!(acked.l0_flips.is_empty() && acked.status_diff.is_empty());
    assert_eq!(
        acked,
        CommitResult {
            l0_flips: Vec::new(),
            status_diff: Vec::new(),
            ..eager_result.clone()
        }
    );
    let row = s.read_row(e).unwrap();
    assert!(row.settling);
    assert_eq!(row.result().unwrap(), acked);
    assert!(log_bytes(&dir) - before <= 1024);
    assert_eq!(s.projection_watermark(), Epoch(e.0 - 1));
    match changes(&s) {
        ChangePage::Rows { reached, more, .. } => {
            assert_eq!((reached, more), (Epoch(e.0 - 1), false));
        }
        other => panic!("{other:?}"),
    }
    assert!(!s.take_settled());
    s.settle().unwrap();
    assert!(s.take_settled());
    assert!(!s.take_settled());
    assert_eq!(s.frontier_epoch(), None);
    assert_eq!(s.projection_watermark(), e);
    assert_eq!(changes(&s), changes(&eager_store));
    assert_eq!(dump(&dir), dump(&eager_dir));
    // The frame keeps what was acknowledged.
    assert_eq!(s.read_row(e).unwrap().result().unwrap(), acked);
}

#[test]
fn a_settling_commit_ends_its_group() {
    let dir = tmp("group");
    let mut s = open(&dir, Some(small()));
    hub(&mut s);
    let (w2, w3) = (create(&mut s, "w2"), create(&mut s, "w3"));
    let results = s.commit_group(&[toggle(false), w2.clone(), w3.clone()]);
    assert_eq!(results.len(), 1);
    let e = results[0].as_ref().unwrap().epoch;
    assert_eq!((s.epoch(), s.frontier_epoch()), (e, Some(e)));
    let results = s.commit_group(&[w2.clone(), w3.clone()]);
    assert!(s.take_settled());
    let epochs: Vec<Epoch> = results.into_iter().map(|r| r.unwrap().epoch).collect();
    assert_eq!(epochs, [Epoch(e.0 + 1), Epoch(e.0 + 2)]);
    assert_eq!(s.frontier_epoch(), None);
    let (eager_dir, mut eager_store) = eager("group-eager", &[]);
    let (x2, x3) = (
        create(&mut eager_store, "w2"),
        create(&mut eager_store, "w3"),
    );
    for b in [toggle(false), x2, x3] {
        eager_store.commit(&b).unwrap();
    }
    assert_eq!(changes(&s), changes(&eager_store));
    assert_eq!(dump(&dir), dump(&eager_dir));
}

#[test]
fn a_retry_of_a_settling_commit_returns_the_ack() {
    let dir = tmp("retry");
    let mut s = open(&dir, Some(small()));
    hub(&mut s);
    let mut batch = toggle(false);
    batch.client_txn_id = Some([7; 16]);
    let acked = s.commit(&batch).unwrap();
    assert!(acked.status_diff.is_empty() && s.frontier_epoch() == Some(acked.epoch));
    assert_eq!(s.commit(&batch).unwrap(), acked);
    assert_eq!(s.epoch(), acked.epoch);
    match s.txn_status(&[7; 16]).unwrap() {
        TxnStatus::Committed { result, .. } => assert_eq!(*result, acked),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_crash_before_settlement_replays_eagerly() {
    let (eager_dir, eager_store) = eager("crash-eager", &[toggle(false)]);
    let dir = tmp("crash");
    let e = {
        let mut s = open(&dir, Some(small()));
        hub(&mut s);
        let e = s.commit(&toggle(false)).unwrap().epoch;
        assert_eq!(s.frontier_epoch(), Some(e));
        e
    };
    let (s, report) = Store::open(StdDisk::default(), &dir, &options(&dir, Some(small()))).unwrap();
    assert_eq!(report.projection_watermark, Epoch(e.0 - 1));
    assert_eq!(s.frontier_epoch(), None);
    assert_eq!(s.projection_watermark(), e);
    assert!(s.read_row(e).unwrap().settling);
    assert_eq!(changes(&s), changes(&eager_store));
    assert_eq!(dump(&dir), dump(&eager_dir));
}

#[test]
fn a_format_6_log_opens_and_continues_in_a_format_7_segment() {
    let dir = tmp("format6");
    let disk = StdDisk::default();
    let generated = shape(Shape::Hub, 8, 42);
    assert_eq!(
        migrate::write_format6(&disk, &dir, &generated.batches).unwrap(),
        2
    );
    let old = migrate::rows_for_test(&disk, &dir, 6).unwrap();
    let expected: Vec<ContentHash> = (old.iter())
        .map(|r| log::checksum(&r.encode_body_format(6)))
        .collect();
    // Re-encoding the old rows in format 7 would change every checksum.
    assert!((old.iter().zip(&expected)).all(|(r, sum)| log::checksum(&r.encode_body()) != *sum));
    let checksums = |s: &Store<StdDisk>| -> Vec<ContentHash> {
        (1..=old.len() as u64)
            .map(|n| s.checksum_at(Epoch(n)).unwrap())
            .collect()
    };
    let mut s = open(&dir, None);
    assert_eq!(checksums(&s), expected);
    let reader = s.log_reader();
    assert_eq!(reader.checksum_at(Epoch(2)).unwrap(), expected[1]);
    assert_eq!(reader.read_row(Epoch(2)).unwrap(), old[1]);
    let e = s.commit(&toggle(false)).unwrap().epoch;
    assert_eq!(e, Epoch(3));
    let at_e = s.checksum_at(e).unwrap();
    let segs = segments(&dir);
    let format = |p: &Path| {
        log::parse_header(&std::fs::read(p).unwrap())
            .unwrap()
            .format
    };
    assert_eq!(segs.len(), 2);
    assert_eq!((format(&segs[0]), format(&segs[1])), (6, 7));
    drop(s);
    let s = open(&dir, None);
    assert_eq!(s.epoch(), e);
    assert_eq!(checksums(&s), expected);
    assert_eq!(s.checksum_at(e).unwrap(), at_e);
    assert!(!s.read_row(e).unwrap().settling);
    drop(s);
    // Offline tools read both formats: nothing to migrate, and a repair
    // plan walks the format-6 frames and lists the format-7 one.
    let report = migrate::migrate(&disk, &dir).unwrap();
    assert_eq!((report.from, report.to, report.frames), (6, 6, 0));
    let plan = doctor::plan_truncate_before(&disk, &dir, e).unwrap();
    assert_eq!(
        (plan.keep_through, plan.dropped_epochs),
        (Epoch(2), vec![e])
    );
}

#[test]
fn a_format_6_segment_after_a_format_7_one_is_refused() {
    let dir = tmp("downgrade");
    let checksum = {
        let mut s = open(&dir, None);
        let b = create(&mut s, "a");
        s.commit(&b).unwrap();
        s.checksum_at(Epoch(1)).unwrap()
    };
    let seg = dir.join("log").join(log::segment_name(Epoch(2)));
    std::fs::write(seg, log::segment_header_with(&log::OP_LOG, 2, checksum, 6)).unwrap();
    let opened = Store::open(StdDisk::default(), &dir, &options(&dir, None));
    assert!(matches!(
        opened,
        Err(StoreError::ChainBroken {
            epoch: Epoch(2),
            fault: log::FrameFault::SegmentHeader,
        })
    ));
}

#[test]
fn dry_runs_and_snapshots_settle_first() {
    let dir = tmp("entries");
    let mut s = open(&dir, Some(small()));
    hub(&mut s);
    let probe = create(&mut s, "probe");
    type Entry = fn(&mut Store<StdDisk>, &Batch);
    let entries: [(&str, Entry); 3] = [
        ("what_if_detailed", |s, b| {
            s.what_if_detailed(b).unwrap();
        }),
        ("impact_exact", |s, b| {
            s.impact_exact(b).unwrap();
        }),
        ("snapshot", |s, _| {
            s.snapshot().unwrap();
        }),
    ];
    for (k, (name, entry)) in entries.into_iter().enumerate() {
        let e = s.commit(&toggle(k % 2 == 1)).unwrap().epoch;
        assert_eq!(s.frontier_epoch(), Some(e), "{name}");
        entry(&mut s, &probe);
        assert_eq!(s.frontier_epoch(), None, "{name}");
        assert!(s.take_settled(), "{name}");
        assert_eq!(s.projection_watermark(), e, "{name}");
    }
}

#[test]
fn an_empty_format_6_tail_segment_is_rewritten_in_place() {
    // A format-6 log whose last segment has a header and no frame: the
    // first commit rewrites that segment in format 7 under the same name.
    let dir = tmp("format6-empty-tail");
    let disk = StdDisk::default();
    let generated = shape(Shape::Hub, 8, 42);
    migrate::write_format6(&disk, &dir, &generated.batches).unwrap();
    let old = migrate::rows_for_test(&disk, &dir, 6).unwrap();
    let last = log::checksum(&old.last().unwrap().encode_body_format(6));
    let next = old.len() as u64 + 1;
    std::fs::write(
        dir.join("log").join(log::segment_name(Epoch(next))),
        log::segment_header_with(&log::OP_LOG, next, last, 6),
    )
    .unwrap();
    let mut s = open(&dir, None);
    let e = s.commit(&toggle(false)).unwrap().epoch;
    assert_eq!(e, Epoch(next));
    let segs = segments(&dir);
    assert_eq!(segs.len(), 2);
    let format = |p: &Path| {
        log::parse_header(&std::fs::read(p).unwrap())
            .unwrap()
            .format
    };
    assert_eq!((format(&segs[0]), format(&segs[1])), (6, 7));
    let e2 = s.commit(&toggle(true)).unwrap().epoch;
    assert_eq!(s.log_reader().read_row(e2).unwrap().epoch, e2);
    let sum = s.checksum_at(e2).unwrap();
    drop(s);
    let s = open(&dir, None);
    assert_eq!(s.epoch(), e2);
    assert_eq!(s.checksum_at(e2).unwrap(), sum);
}
