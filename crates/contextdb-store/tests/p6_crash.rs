//! P6 G4 (ADR 0013 §11) on the simulated disk:
//! - crash injection around the branch Ops: the in-flight batch is a
//!   `CreateBranch`, a `Merge` or a commit on a branch other than `main`,
//!   killed before the WAL write, between write and sync, or after the sync
//!   and before publication. Recovery is a committed prefix of the kernel
//!   (every branch), acked ids keep their results, and a retry applies at
//!   most once. Gate count via CONTEXTDB_G4_RUNS (≥ 1,000 fired per point).
//! - `ctx migrate` 2→3 interrupted at every disk step, crashed, and run
//!   again: it completes (or resumes the swap) and the store equals the
//!   uninterrupted migration.
#[path = "../../contextdb-testkit/tests/common/branch_gen.rs"]
mod branch_gen;
use branch_gen::Gen;
use contextdb_core::{Counting, Kernel};
use contextdb_model::*;
use contextdb_store::*;
use std::path::Path;

const DIR: &str = "/db";
fn txn(n: u64) -> [u8; 16] {
    let mut id = [0; 16];
    id[..8].copy_from_slice(&n.to_le_bytes());
    id
}
fn with_blobs(store: &mut Store<SimDisk>, batch: &Batch) -> Batch {
    let mut b = batch.clone();
    for op in &mut b.ops {
        if let Op::CreateLogical { content, .. } | Op::AddRevision { content, .. } = op {
            *content = store.put_blob(content).unwrap();
        }
    }
    b
}
fn digests(k: &Kernel) -> Vec<(String, ContentHash)> {
    k.branches()
        .map(|b| (b.name.clone(), k.branch(b.id).unwrap().digest()))
        .collect()
}
#[derive(Clone, Copy, Debug)]
enum Point {
    PreWal,
    PostWalPreSync,
    PostSyncPrePublish,
}
const FATES: [TailFate; 4] = [
    TailFate::Torn,
    TailFate::ZeroFill,
    TailFate::Garbage,
    TailFate::OutOfOrder,
];
/// The in-flight branch Op for `seed`: a fork, a merge, or a commit on a
/// branch other than `main` (a fork when there is no other branch yet).
fn branch_op(g: &mut Gen, k: &Kernel, seed: u64) -> Batch {
    let n = k.branches().count();
    let other = |g: &mut Gen| BranchId(1 + g.pick(n - 1) as u16);
    match (seed / 4 % 3, n) {
        (1, 2..) => {
            let (into, source) = if g.coin(50) {
                (MAIN, other(g))
            } else {
                (other(g), MAIN)
            };
            g.merge(into, source)
        }
        (2, 2..) => {
            let b = other(g);
            g.batch(k, b)
        }
        _ => {
            let on = g.branch(k);
            g.create_branch(on)
        }
    }
}
/// One run; whether the injection fired, or the violated invariant.
fn run(seed: u64, point: Point, fate: TailFate) -> Result<bool, String> {
    let disk = SimDisk::new();
    let options = OpenOptions {
        segment_target: if seed % 2 == 1 {
            512
        } else {
            log::SEGMENT_TARGET
        },
        ..OpenOptions::default()
    };
    let reopen = |d: &SimDisk| Store::open(d.clone(), Path::new(DIR), &options);
    let (mut store, _) = reopen(&disk).map_err(|e| format!("{e:?}"))?;
    let mut g = Gen::new(seed);
    let steps = 5 + g.pick(25);
    let mut acked: Vec<(Batch, CommitResult)> = Vec::new();
    for n in 0..steps {
        let b = g.step(store.kernel().unwrap());
        let mut b = with_blobs(&mut store, &b);
        b.client_txn_id = Some(txn(n as u64));
        if let Ok(r) = store.commit(&b) {
            acked.push((b, r));
        }
    }
    let b = branch_op(&mut g, store.kernel().unwrap(), seed);
    let mut in_flight = with_blobs(&mut store, &b);
    in_flight.client_txn_id = Some(txn(steps as u64));
    // A fork, merge or branch commit that the kernel rejects writes nothing.
    if Kernel::<Counting>::import(store.kernel().unwrap().export())
        .unwrap()
        .commit(&in_flight)
        .is_err()
    {
        return Ok(false);
    }
    match point {
        Point::PreWal => disk.fail_next("append"),
        Point::PostWalPreSync => disk.fail_next("sync"),
        Point::PostSyncPrePublish => store.crash_after_next_sync(),
    }
    let fired = store.commit(&in_flight) == Err(StoreError::Poisoned);
    if fired {
        let last = acked.last().map_or(0, |(_, r)| r.epoch.0);
        if store.epoch().0 != last || store.kernel().is_ok() {
            return Err("poisoned store serves non-durable state".into());
        }
    }
    drop(store);
    let crashed = disk.crash(fate, seed);
    let (mut store, _) = reopen(&crashed).map_err(|e| format!("open: {e:?}"))?;
    let recovered = store.epoch().0;
    let last_acked = acked.last().map_or(0, |(_, r)| r.epoch.0);
    if recovered < last_acked || recovered > last_acked + 1 {
        return Err(format!("recovered {recovered}, acked {last_acked}"));
    }
    if fired && matches!(point, Point::PostSyncPrePublish) && recovered != last_acked + 1 {
        return Err("synced frame lost".into());
    }
    if fired && matches!(point, Point::PreWal) && recovered != last_acked {
        return Err("unwritten frame recovered".into());
    }
    // Every branch equals the kernel replaying the acked prefix (and the
    // in-flight batch if its frame survived).
    let mut reference = Kernel::default();
    for (b, _) in &acked {
        reference.commit(b).map_err(|e| format!("{e:?}"))?;
    }
    if recovered == last_acked + 1 {
        reference.commit(&in_flight).map_err(|e| format!("{e:?}"))?;
    }
    let k = store.kernel().map_err(|e| format!("{e:?}"))?;
    if digests(k) != digests(&reference) {
        return Err("recovered branches are not a committed prefix".into());
    }
    for info in k.branches() {
        let view = k.branch(info.id).unwrap();
        if contextdb_model::DerivedView::derived(view.db())
            != &contextdb_oracle::jacobi(&view.db().inputs())
        {
            return Err(format!("{:?}: replayed derived state differs", info.id));
        }
    }
    for (b, r) in &acked {
        match store.txn_status(&b.client_txn_id.unwrap()) {
            Ok(TxnStatus::Committed { result, .. }) if *result == *r => {}
            other => return Err(format!("acked txn lost: {other:?}")),
        }
    }
    let before = store.epoch();
    let status = store
        .txn_status(&in_flight.client_txn_id.unwrap())
        .map_err(|e| format!("{e:?}"))?;
    let retry = store.commit(&in_flight);
    match status {
        TxnStatus::Committed { result, .. } => {
            if retry != Ok(*result) || store.epoch() != before {
                return Err("retry of a committed branch op applied again".into());
            }
        }
        TxnStatus::NotCommitted => {
            if retry.is_err() || store.epoch().0 != before.0 + 1 {
                return Err(format!("retry not applied exactly once: {retry:?}"));
            }
        }
    }
    Ok(fired)
}
fn runs() -> u64 {
    std::env::var("CONTEXTDB_G4_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
}
fn g4(point: Point) {
    let target = runs();
    let (mut fired, mut kinds, mut seed) = (0, [0u64; 3], 0);
    while fired < target {
        let fate = FATES[(seed % 4) as usize];
        match run(seed, point, fate) {
            Ok(true) => {
                fired += 1;
                kinds[(seed / 4 % 3) as usize] += 1;
            }
            Ok(false) => {}
            Err(e) => panic!("{point:?} seed {seed} {fate:?}: {e}"),
        }
        seed += 1;
        assert!(seed < target * 20, "{point:?}: injections rarely fire");
    }
    eprintln!("{point:?}: {fired} fired, [fork-or-fallback, merge, branch commit] = {kinds:?}");
    assert!(kinds.iter().all(|&k| k * 5 >= target), "{kinds:?}");
}
#[test]
fn g4_branch_ops_pre_wal() {
    g4(Point::PreWal);
}
#[test]
fn g4_branch_ops_post_wal_pre_sync() {
    g4(Point::PostWalPreSync);
}
#[test]
fn g4_branch_ops_post_sync_pre_publish() {
    g4(Point::PostSyncPrePublish);
}

/// A format-2 store of `seed`'s P1 trace.
fn format2(seed: u64) -> (SimDisk, Vec<Batch>) {
    let disk = SimDisk::new();
    let batches: Vec<Batch> = contextdb_testkit::generate_p1(seed)
        .batches
        .into_iter()
        .enumerate()
        .map(|(n, mut b)| {
            b.client_txn_id = Some(txn(n as u64));
            b
        })
        .collect();
    // Only batches the kernel accepts were ever logged.
    let mut k = Kernel::default();
    let batches: Vec<Batch> = batches
        .into_iter()
        .filter(|b| k.commit(b).is_ok())
        .collect();
    migrate::write_format2(&disk, Path::new(DIR), &batches).unwrap();
    (disk, batches)
}
fn opened(disk: &SimDisk) -> (Vec<(String, ContentHash)>, Vec<Vec<u8>>) {
    let (store, _) = Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).unwrap();
    let rows = (1..=store.epoch().0)
        .map(|e| {
            let row = store.read_row(Epoch(e)).unwrap();
            [
                row.result_blob.clone(),
                row.acked_result.clone().unwrap_or_default(),
            ]
            .concat()
        })
        .collect();
    (digests(store.kernel().unwrap()), rows)
}
#[test]
fn g4_migrate_2_to_3_interrupted_and_resumed() {
    let (mut interrupted, mut resumed) = (0, 0);
    for seed in 0..std::env::var("CONTEXTDB_G4_MIGRATE_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8u64)
    {
        let (clean, _) = format2(seed);
        let report = migrate::migrate(&clean, Path::new(DIR)).unwrap();
        let expected = opened(&clean);
        for op in ["write", "append", "sync", "sync_dir", "rename"] {
            for n in 0.. {
                let mut fired = false;
                for (f, fate) in FATES.iter().enumerate() {
                    let (disk, _) = format2(seed);
                    disk.fail_nth(op, n);
                    if migrate::migrate(&disk, Path::new(DIR)).is_ok() {
                        break;
                    }
                    fired = true;
                    interrupted += 1;
                    let crashed = disk.crash(*fate, seed * 31 + n as u64 * 7 + f as u64);
                    // Before the swap the store still needs migration and between
                    // the renames it refuses to open; `migrate` then finishes. A
                    // failure after both renames may leave the swap durable: the
                    // store is migrated and `migrate` has nothing to do.
                    let done =
                        Store::open(crashed.clone(), Path::new(DIR), &OpenOptions::default())
                            .is_ok();
                    let swap = !crashed.exists(&Path::new(DIR).join("log"));
                    let again = migrate::migrate(&crashed, Path::new(DIR))
                        .unwrap_or_else(|e| panic!("seed {seed} {op}#{n} {fate:?}: {e:?}"));
                    if done {
                        assert_eq!((again.from, again.to, again.frames), (3, 3, 0));
                    } else {
                        assert_eq!((again.from, again.to), (2, 3));
                        assert_eq!(again.frames, report.frames);
                    }
                    resumed += usize::from(swap);
                    assert_eq!(
                        opened(&crashed),
                        expected,
                        "seed {seed} {op}#{n} {fate:?}: differs from the clean migration"
                    );
                    assert!(crashed.exists(&Path::new(DIR).join("log.v2.bak")));
                }
                if !fired {
                    break;
                }
            }
        }
    }
    eprintln!("migrate 2→3: {interrupted} interrupted runs, {resumed} after the first rename");
    assert!(interrupted > 0 && resumed > 0);
}
