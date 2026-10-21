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
    fn read_range(&self, p: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.0.read_range(p, offset, len)
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
    fn rename_dir(&self, a: &Path, b: &Path) -> io::Result<()> {
        self.0.rename_dir(a, b)
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
/// Production options (`ctx`): SQLite projections in a file next to the log.
fn file_projections(dir: &Path) -> OpenOptions {
    OpenOptions {
        projections: ProjectionFile::Path(dir.join("projections.sqlite")),
        ..OpenOptions::default()
    }
}
fn commit_p99(dir: &Path, full_sync: bool, samples: usize, file: bool) -> u128 {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let disk = StdDisk { full_sync };
    let options = if file {
        file_projections(dir)
    } else {
        OpenOptions::default()
    };
    let (mut store, _) = Store::open(disk, dir, &options).unwrap();
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
            if store.db().unwrap().is_base(r).unwrap() {
                Op::RetractBase { rev: r.into() }
            } else {
                Op::AssertBase { rev: r.into() }
            }
        };
        store.commit(&Batch::new(vec![op])).unwrap();
    }
}
fn cold_open(dir: &Path) -> (u128, OpenReport) {
    cold_open_with(dir, &OpenOptions::default())
}
fn cold_open_with(dir: &Path, options: &OpenOptions) -> (u128, OpenReport) {
    let start = Instant::now();
    let (store, report) = Store::open(StdDisk::default(), dir, options).unwrap();
    let t = start.elapsed().as_millis();
    drop(store);
    (t, report)
}
/// One Historical read at `e` on a freshly opened store (ms).
fn historical_ms(dir: &Path, options: &OpenOptions, e: Epoch) -> u128 {
    let (store, _) = Store::open(StdDisk::default(), dir, options).unwrap();
    let start = Instant::now();
    store.read_historical(RevisionId(0), e).unwrap();
    start.elapsed().as_millis()
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let samples: usize = args.get(1).map_or(1000, |s| s.parse().unwrap());
    let txns: usize = args.get(2).map_or(100_000, |s| s.parse().unwrap());
    let every: usize = args.get(3).map_or(1, |s| s.parse().unwrap());
    let root = std::env::temp_dir().join(format!("contextdb-store-bench-{}", std::process::id()));
    let on = commit_p99(&root.join("on"), true, samples, false);
    let off = commit_p99(&root.join("off"), false, samples, false);
    let on_file = commit_p99(&root.join("on-file"), true, samples, true);
    println!(
        "commit_p99_ns fullfsync_on={on} fullfsync_off={off} fullfsync_on_file_projections={on_file} samples={samples}"
    );
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
    // Production configuration: the first open also writes every epoch's
    // projection rows; later opens find them current.
    let file = file_projections(&dir);
    let (first, _) = cold_open_with(&dir, &file);
    let (again, report) = cold_open_with(&dir, &file);
    println!(
        "cold_open_file_projections_ms first={first} current={again} watermark={}",
        report.projection_watermark.0
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
    let (snap_file, _) = cold_open_with(&dir, &file);
    println!("cold_open_with_snapshot_file_projections_ms={snap_file}");
    // Open verifies (blake3) and syncs every pack segment (ADR 0008 §17):
    // the same snapshot open with one ~200-byte payload per transaction.
    {
        let (mut store, _) =
            Store::open(StdDisk { full_sync: false }, &dir, &OpenOptions::default()).unwrap();
        let payloads: Vec<Vec<u8>> = (0..txns)
            .map(|n| format!("payload {n:>8} {}", "x".repeat(180)).into_bytes())
            .collect();
        for chunk in payloads.chunks(1_000) {
            let refs: Vec<&[u8]> = chunk.iter().map(Vec::as_slice).collect();
            store.put_blobs(&refs).unwrap();
        }
    }
    let (snap_packs, _) = cold_open(&dir);
    println!("cold_open_with_snapshot_and_packed_payloads_ms={snap_packs} payloads={txns}");
    // Reads below the snapshot: log replay (memory projections) versus the
    // projection tables. Informational; no SPEC budget yet (P3 sets read budgets).
    let mid = Epoch(txns as u64 / 2);
    println!(
        "historical_read_below_snapshot_ms replay={} projections={}",
        historical_ms(&dir, &OpenOptions::default(), mid),
        historical_ms(&dir, &file, mid)
    );
    let budgets = on <= 25_000_000
        && off <= 10_000_000
        && on_file <= 25_000_000
        && full <= 5_000
        && first <= 5_000
        && snap <= 1_000
        && snap_file <= 1_000
        && snap_packs <= 1_000;
    println!("g5_initial_pass={budgets}");
    let _ = std::fs::remove_dir_all(&root);
}
