//! P2 G5 (initial): durable commit p99 with fullfsync on/off, and cold-start
//! replay of N transactions with and without a snapshot (ADR 0006 §10–§11).
//! Usage: store_bench [commit_samples=1000] [replay_txns=100000] [structural_every=1]
use contextdb_model::*;
use contextdb_store::*;
use std::{io, path::Path, time::Instant};

/// Bench-only: builds the cold-start log quickly. Never used for measured commits.
#[derive(Clone)]
struct Unsynced(StdDisk);
impl Durability for Unsynced {
    fn instance(&self) -> String {
        format!("unsynced-{}", std::process::id())
    }
    fn create_dir_all(&self, d: &Path) -> io::Result<()> {
        self.0.create_dir_all(d)
    }
    fn list(&self, d: &Path) -> io::Result<Vec<String>> {
        self.0.list(d)
    }
    fn exists(&self, p: &Path) -> bool {
        self.0.exists(p)
    }
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        self.0.read(p)
    }
    fn append(&self, p: &Path, d: &[u8]) -> io::Result<()> {
        self.0.append(p, d)
    }
    fn write_new(&self, p: &Path, d: &[u8]) -> io::Result<()> {
        self.0.write_new(p, d)
    }
    fn truncate(&self, p: &Path, n: u64) -> io::Result<()> {
        self.0.truncate(p, n)
    }
    fn remove(&self, p: &Path) -> io::Result<()> {
        self.0.remove(p)
    }
    fn rename(&self, a: &Path, b: &Path) -> io::Result<()> {
        self.0.rename(a, b)
    }
    fn sync(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn sync_dir(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn lock(&self, p: &Path) -> io::Result<Box<dyn durability::LockGuard>> {
        self.0.lock(p)
    }
}
fn p99(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v[(v.len() * 99).div_ceil(100).saturating_sub(1)]
}
fn create(n: usize, content: ContentHash) -> Op {
    Op::CreateLogical {
        key: format!("k{n}"),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content,
        adopt_as_base: true,
    }
}
fn commit_p99(dir: &Path, full_sync: bool, samples: usize) -> u128 {
    let _ = std::fs::remove_dir_all(dir);
    let disk = StdDisk { full_sync };
    let (mut store, _) = Store::open(disk, dir, &OpenOptions::default()).unwrap();
    let blob = store.put_blob(b"payload").unwrap();
    let mut times = Vec::with_capacity(samples);
    for n in 0..samples {
        let start = Instant::now();
        store.commit(&Batch::new(vec![create(n, blob)])).unwrap();
        times.push(start.elapsed().as_nanos());
    }
    p99(times)
}
/// `structural_every = k`: every k-th transaction creates a logical; the rest
/// toggle the base of an existing revision (fast-path transactions).
fn build_log(dir: &Path, txns: usize, structural_every: usize) {
    let _ = std::fs::remove_dir_all(dir);
    let (mut store, _) =
        Store::open(Unsynced(StdDisk::default()), dir, &OpenOptions::default()).unwrap();
    let blob = store.put_blob(b"payload").unwrap();
    let mut created = 0usize;
    for n in 0..txns {
        let op = if created == 0 || n % structural_every == 0 {
            created += 1;
            create(n, blob)
        } else {
            let r = RevisionId((n % created) as u32);
            if store.db().is_base(r).unwrap() {
                Op::RetractBase { rev: r.into() }
            } else {
                Op::AssertBase { rev: r.into() }
            }
        };
        store.commit(&Batch::new(vec![op])).unwrap();
    }
}
fn cold_open(dir: &Path) -> (u128, OpenReport) {
    let start = Instant::now();
    let (store, report) = Store::open(StdDisk::default(), dir, &OpenOptions::default()).unwrap();
    let t = start.elapsed().as_millis();
    drop(store);
    (t, report)
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let samples: usize = args.get(1).map_or(1000, |s| s.parse().unwrap());
    let txns: usize = args.get(2).map_or(100_000, |s| s.parse().unwrap());
    let every: usize = args.get(3).map_or(1, |s| s.parse().unwrap());
    let root = std::env::temp_dir().join(format!("contextdb-store-bench-{}", std::process::id()));
    let on = commit_p99(&root.join("on"), true, samples);
    let off = commit_p99(&root.join("off"), false, samples);
    println!("commit_p99_ns fullfsync_on={on} fullfsync_off={off} samples={samples}");
    let dir = root.join("replay");
    let build = Instant::now();
    build_log(&dir, txns, every);
    println!(
        "built txns={txns} structural_every={every} build_ms={}",
        build.elapsed().as_millis()
    );
    let (full, report) = cold_open(&dir);
    println!(
        "cold_open_full_replay_ms={full} replayed={}",
        report.replayed
    );
    {
        let (mut store, _) =
            Store::open(StdDisk::default(), &dir, &OpenOptions::default()).unwrap();
        store.snapshot().unwrap();
    }
    let (snap, report) = cold_open(&dir);
    println!(
        "cold_open_with_snapshot_ms={snap} snapshot_epoch={:?} replayed={}",
        report.snapshot_epoch, report.replayed
    );
    let budgets = on <= 25_000_000 && off <= 10_000_000 && full <= 5_000 && snap <= 1_000;
    println!("g5_initial_pass={budgets}");
    let _ = std::fs::remove_dir_all(&root);
}
