//! `p8_bench [small=10000] [large=100000] [epochs=100000] [dense]` (ADR
//! 0016 §10 G5). Record bodies and query terms are Zipf-distributed (s = 1)
//! over 20,000 synthetic words; `dense` draws them uniformly from 40 words
//! instead, so every term matches about half the rows (reported only). In process (`Service` on `SimDisk`, projections in real SQLite
//! files), on the reference Mac:
//!
//! - **recall p50/p99** (key + FTS), certified, limit 10, at `small` and
//!   `large` records: idle, and while a writer commits every 2 ms so the
//!   index keeps lagging (bounded wait 200 ms; its hit rate is reported).
//! - **indexer lag** after a commit: time until the FTS watermark reaches
//!   the commit's epoch, 300 commits.
//! - **packing overhead:** the same queries with `budget_tokens = 2000` at
//!   `outline` against header-only, p50 difference.
//! - **hybrid** with a 1,024-dim mock embedder: recall p99 from the vector
//!   cache at `small`, and at `large` (reported), plus the uncached path
//!   (`max_cache_mb = 0`).
//! - **mirror export** at `small` records into a fresh git repository:
//!   first export, unchanged re-export (cache hit), and a cache-miss
//!   validation (deleted `mirror.sqlite`) after `epochs` epochs, replay
//!   included.
use contextdb_daemon::{
    actor,
    embedding::Settings,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use contextdb_validate::embed::{Mock, VectorLimits};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
const VOCAB: [&str; 40] = [
    "otter", "quartz", "maple", "ember", "falcon", "harbor", "lantern", "meadow", "nickel",
    "orchid", "pepper", "ridge", "saddle", "tundra", "umber", "velvet", "willow", "yonder",
    "zephyr", "anchor", "basalt", "cobalt", "dune", "ermine", "fjord", "garnet", "heron", "iris",
    "jasper", "kelp", "lichen", "mesa", "nectar", "onyx", "prairie", "quill", "raven", "sable",
    "thistle", "upland",
];
type Svc = Service<SimDisk>;

fn pct(v: &mut [Duration], p: usize) -> f64 {
    v.sort();
    v[(v.len() * p).div_ceil(100).saturating_sub(1)].as_secs_f64() * 1000.
}
fn temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "p8-bench-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// A word: Zipf-distributed (s = 1) over [`ZIPF_WORDS`] synthetic words
    /// by default, or uniform over the 40-word [`VOCAB`] with `dense`.
    fn word(&mut self) -> &'static str {
        if DENSE.load(Ordering::Relaxed) {
            return VOCAB[self.next() as usize % VOCAB.len()];
        }
        let (words, cdf) = zipf();
        let u = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        let i = cdf.partition_point(|c| *c < u).min(words.len() - 1);
        words[i].as_str()
    }
}
/// `dense` on the command line: every query term matches about half the
/// rows (the worst case for bm25, which scores every match). Reported, not
/// gated: record text has a long-tailed vocabulary.
static DENSE: AtomicBool = AtomicBool::new(false);
const ZIPF_WORDS: usize = 20_000;
/// Distinct letter-only words (consonant-vowel syllables) and the Zipf
/// cumulative distribution over them.
fn zipf() -> &'static (Vec<String>, Vec<f64>) {
    static Z: std::sync::OnceLock<(Vec<String>, Vec<f64>)> = std::sync::OnceLock::new();
    Z.get_or_init(|| {
        const C: &[u8] = b"bdfgklmnprstvz";
        const V: &[u8] = b"aeiou";
        let words: Vec<String> = (0..ZIPF_WORDS)
            .map(|mut n| {
                let mut w = String::new();
                for _ in 0..4 {
                    w.push(C[n % C.len()] as char);
                    n /= C.len();
                    w.push(V[n % V.len()] as char);
                    n /= V.len();
                }
                w
            })
            .collect();
        let total: f64 = (1..=ZIPF_WORDS).map(|r| 1.0 / r as f64).sum();
        let mut acc = 0.0;
        let cdf = (1..=ZIPF_WORDS)
            .map(|r| {
                acc += 1.0 / r as f64 / total;
                acc
            })
            .collect();
        (words, cdf)
    })
}
fn body(rng: &mut Rng, i: usize) -> String {
    let words: Vec<&str> = (0..30).map(|_| rng.word()).collect();
    format!(
        "# Note {i}\n\n{}\n\n## Detail\n{}\n",
        words[..12].join(" "),
        words[12..].join(" ")
    )
}
fn service(dir: &Path) -> Svc {
    let disk = SimDisk::new();
    let db = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), db, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk, db, false, 64 << 20).unwrap();
    let s = Service::with_recall(
        actor::spawn(store).unwrap(),
        log,
        events,
        Some(&dir.join("recall.sqlite")),
    )
    .unwrap();
    s.set_data_dir(dir.to_path_buf());
    s
}
fn commit(s: &Svc, blobs: Vec<Vec<u8>>, ops: Vec<Op>) -> CommitResult {
    match s
        .handle(
            ADMIN,
            Request::Raw {
                blobs,
                batch: Batch::new(ops),
            },
        )
        .response
    {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
/// `n` records, 100 per commit.
fn fill(s: &Svc, n: usize, rng: &mut Rng) {
    for chunk in (0..n).collect::<Vec<_>>().chunks(100) {
        let mut blobs = Vec::new();
        let mut ops = Vec::new();
        for &i in chunk {
            let b = body(rng, i);
            ops.push(Op::CreateLogical {
                key: format!("area{}/topic-{i}", i % 50),
                kind: RecordKind::Decision,
                scope_lane: "repo".into(),
                content: content_hash(b.as_bytes()),
                adopt_as_base: true,
            });
            blobs.push(b.into_bytes());
        }
        commit(s, blobs, ops);
    }
}
/// Waits for the text index and the candidate pass: idle means no
/// background indexing work left.
fn wait_fts(s: &Svc) {
    let e = s.actor.snapshot().epoch;
    while !s.index_health.wait(e, Duration::from_secs(2)) {}
    while s.candidates.watermark().ok().flatten() != Some(e) {
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn attach(s: &Svc) -> Binding {
    match s
        .handle(
            AGENT,
            Request::SessionAttach {
                harness: "claude".into(),
                harness_session: "bench".into(),
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
/// One recall: its latency, and whether the text index was Ready.
fn recall(
    s: &Svc,
    b: &Binding,
    q: &str,
    opts: RecallOpts,
    wait_ms: u32,
) -> (Duration, bool, usize) {
    let t = Instant::now();
    let r = s.handle(
        AGENT,
        Request::Recall {
            binding: b.clone(),
            query: q.into(),
            scope: String::new(),
            mode: Mode::Certified,
            limit: 10,
            index_wait_ms: wait_ms,
            opts,
        },
    );
    let dt = t.elapsed();
    match r.response {
        Response::Recall { items, index, .. } => (
            dt,
            matches!(index.fts, IndexState::Ready { .. }),
            items.len(),
        ),
        other => panic!("{other:?}"),
    }
}
fn queries(rng: &mut Rng, n: usize, records: usize) -> Vec<String> {
    (0..n)
        .map(|i| match i % 4 {
            0 => rng.word().to_owned(),
            1 => format!("{} {}", rng.word(), rng.word()),
            2 => format!(
                "area{}/topic-{}",
                rng.next() % 50,
                rng.next() as usize % records
            ),
            _ => format!("area{}", rng.next() % 50),
        })
        .collect()
}

fn recall_section(n: usize, out: &mut Vec<String>) {
    let dir = temp(&format!("recall{n}"));
    let s = service(&dir);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ n as u64);
    let t = Instant::now();
    fill(&s, n, &mut rng);
    wait_fts(&s);
    out.push(format!(
        "## {n} records (filled and indexed in {:.1} s)",
        t.elapsed().as_secs_f64()
    ));
    let b = attach(&s);
    let qs = queries(&mut rng, 600, n);
    // Warm up.
    for q in qs.iter().take(50) {
        recall(&s, &b, q, RecallOpts::default(), 200);
    }
    let mut idle: Vec<Duration> = qs
        .iter()
        .map(|q| recall(&s, &b, q, RecallOpts::default(), 200).0)
        .collect();
    out.push(format!(
        "recall idle (key + FTS, certified, limit 10): p50 {:.2} ms, p99 {:.2} ms",
        pct(&mut idle, 50),
        pct(&mut idle, 99)
    ));
    // Packing.
    let packed_opts = RecallOpts {
        budget_tokens: Some(2_000),
        level: Some(levels::Level::Outline),
        ..RecallOpts::default()
    };
    let mut packed: Vec<Duration> = qs
        .iter()
        .map(|q| recall(&s, &b, q, packed_opts.clone(), 200).0)
        .collect();
    let mut plain: Vec<Duration> = qs
        .iter()
        .map(|q| recall(&s, &b, q, RecallOpts::default(), 200).0)
        .collect();
    out.push(format!(
        "packing overhead (budget 2000, outline): p50 {:.2} ms vs {:.2} ms header-only (+{:.2} ms)",
        pct(&mut packed, 50),
        pct(&mut plain, 50),
        pct(&mut packed, 50) - pct(&mut plain, 50)
    ));
    // Indexer lag after a commit.
    let mut lag = Vec::new();
    for i in 0..300 {
        let b = body(&mut rng, n + i);
        let r = commit(
            &s,
            vec![b.clone().into_bytes()],
            vec![Op::CreateLogical {
                key: format!("lag/k-{i}"),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: content_hash(b.as_bytes()),
                adopt_as_base: true,
            }],
        );
        let t = Instant::now();
        s.index_health.wait(r.epoch, Duration::from_secs(2));
        lag.push(t.elapsed());
    }
    out.push(format!(
        "indexer lag after a commit: p50 {:.2} ms, p99 {:.2} ms (budget p99 < 500 ms)",
        pct(&mut lag, 50),
        pct(&mut lag, 99)
    ));
    let s = Arc::new(s);
    for period in [2u64, 50] {
        stream(&s, &b, &qs, period, out);
    }
    s.stop_indexer();
    let _ = std::fs::remove_dir_all(&dir);
}
/// Recall while a writer commits every `period` ms.
fn stream(s: &Arc<Svc>, b: &Binding, qs: &[String], period: u64, out: &mut Vec<String>) {
    let stop = Arc::new(AtomicBool::new(false));
    // Indexer lag under the stream: from each commit's reply until the
    // index reaches its epoch.
    let (to_probe, commits) = std::sync::mpsc::channel::<(Epoch, Instant)>();
    let probe = {
        let s = s.clone();
        std::thread::spawn(move || {
            let mut lag = Vec::new();
            for (e, t) in commits {
                s.index_health.wait(e, Duration::from_secs(2));
                lag.push(t.elapsed());
            }
            lag
        })
    };
    let writer = {
        let (s, stop) = (s.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut rng = Rng(42 + period);
            let mut i = 0;
            while !stop.load(Ordering::Acquire) {
                let b = body(&mut rng, i);
                let r = commit(
                    &s,
                    vec![b.clone().into_bytes()],
                    vec![Op::CreateLogical {
                        key: format!("stream{period}/k-{i}"),
                        kind: RecordKind::Fact,
                        scope_lane: "repo".into(),
                        content: content_hash(b.as_bytes()),
                        adopt_as_base: true,
                    }],
                );
                let _ = to_probe.send((r.epoch, Instant::now()));
                i += 1;
                std::thread::sleep(Duration::from_millis(period));
            }
            i
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    // Latency without waiting (the index may lag), then the bounded
    // wait's hit rate on a sample.
    let mut busy = Vec::new();
    let mut lagging = 0;
    let scanned0 = s.text_hits_scanned.load(Ordering::Relaxed);
    let items0 = s.recall_items.load(Ordering::Relaxed);
    for q in qs {
        let (dt, ready, _) = recall(s, b, q, RecallOpts::default(), 0);
        busy.push(dt);
        lagging += usize::from(!ready);
    }
    let scanned = s.text_hits_scanned.load(Ordering::Relaxed) - scanned0;
    let items = s.recall_items.load(Ordering::Relaxed) - items0;
    let sample = &qs[..100];
    let mut hits = 0;
    for q in sample {
        hits += usize::from(recall(s, b, q, RecallOpts::default(), 200).1);
    }
    stop.store(true, Ordering::Release);
    let written = writer.join().unwrap();
    let mut lag = probe.join().unwrap();
    out.push(format!(
        "recall under a commit stream ({written} commits, one per {period} ms), no wait: p50 {:.2} ms, p99 {:.2} ms, index lagging in {lagging} of {}; with the 200 ms bounded wait it reached Ready in {hits} of {}; bm25 hits examined per returned item {:.1}; indexer lag p50 {:.2} ms, p99 {:.2} ms",
        pct(&mut busy, 50),
        pct(&mut busy, 99),
        qs.len(),
        sample.len(),
        scanned as f64 / items.max(1) as f64,
        pct(&mut lag, 50),
        pct(&mut lag, 99)
    ));
}

fn hybrid_section(n: usize, cache_mb: usize, out: &mut Vec<String>) {
    let dir = temp(&format!("hybrid{n}"));
    let s = service(&dir);
    let mut rng = Rng(0xABCD ^ n as u64);
    fill(&s, n, &mut rng);
    wait_fts(&s);
    let t = Instant::now();
    s.start_vectors(
        Settings {
            embedder: Arc::new(Mock::new(1_024)),
            max_batch: 256,
            timeout: Duration::from_secs(60),
            limits: VectorLimits {
                max_cache_mb: cache_mb,
                ..VectorLimits::default()
            },
        },
        Some(&dir.join("vectors.sqlite")),
    )
    .unwrap();
    let e = s.actor.snapshot().epoch;
    loop {
        if matches!(s.index_report(e).vectors, IndexState::Ready { .. }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let filled = t.elapsed();
    let b = attach(&s);
    let qs = queries(&mut rng, 300, n);
    for q in qs.iter().take(30) {
        recall(&s, &b, q, RecallOpts::default(), 200);
    }
    let mut lat: Vec<Duration> = qs
        .iter()
        .map(|q| recall(&s, &b, q, RecallOpts::default(), 200).0)
        .collect();
    let v = s.vectors.get().unwrap();
    out.push(format!(
        "hybrid, 1,024 dims, {n} records, cache {cache_mb} MB: p50 {:.2} ms, p99 {:.2} ms (vectors filled in {:.1} s; cache hits {}, misses {})",
        pct(&mut lat, 50),
        pct(&mut lat, 99),
        filled.as_secs_f64(),
        v.stats.cache_hits.load(Ordering::Relaxed),
        v.stats.cache_misses.load(Ordering::Relaxed),
    ));
    s.stop_indexer();
    let _ = std::fs::remove_dir_all(&dir);
}

fn mirror_section(n: usize, epochs: usize, out: &mut Vec<String>) {
    let dir = temp("mirror");
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let s = service(&dir);
    let mut rng = Rng(0x5151);
    let t = Instant::now();
    // `n` records in single commits, then moves until `epochs` epochs.
    let mut heads: Vec<RevisionId> = Vec::with_capacity(n);
    for i in 0..n {
        let b = body(&mut rng, i);
        let r = commit(
            &s,
            vec![b.clone().into_bytes()],
            vec![Op::CreateLogical {
                key: format!("area{}/topic-{i}", i % 50),
                kind: RecordKind::Decision,
                scope_lane: "repo".into(),
                content: content_hash(b.as_bytes()),
                adopt_as_base: true,
            }],
        );
        heads.push(r.allocations.new_revisions[0]);
    }
    let mut e = s.actor.snapshot().epoch.0 as usize;
    while e < epochs {
        let i = rng.next() as usize % n;
        let b = body(&mut rng, n + e);
        let r = commit(
            &s,
            vec![b.clone().into_bytes()],
            vec![
                Op::AddRevision {
                    logical: LogicalId(i as u32).into(),
                    content: content_hash(b.as_bytes()),
                    adopt_as_base: true,
                },
                Op::SetHead {
                    logical: LogicalId(i as u32).into(),
                    expected: heads[i].into(),
                    new: RevRef::New(0),
                },
            ],
        );
        heads[i] = r.allocations.new_revisions[0];
        e = r.epoch.0 as usize;
    }
    out.push(format!(
        "## mirror: {n} records, {e} epochs (built in {:.1} s)",
        t.elapsed().as_secs_f64()
    ));
    let export = || {
        let t = Instant::now();
        match s
            .handle(
                ADMIN,
                Request::Mirror {
                    action: MirrorAction::Export {
                        branch: None,
                        project: repo.display().to_string(),
                        force: false,
                        adopt: false,
                        readable_ok: false,
                    },
                },
            )
            .response
        {
            Response::Mirror { report } => (t.elapsed(), report),
            other => panic!("{other:?}"),
        }
    };
    let (first, r) = export();
    out.push(format!(
        "first export: {:.2} s (budget < 10 s); {}",
        first.as_secs_f64(),
        r.lines.first().cloned().unwrap_or_default()
    ));
    let (again, _) = export();
    out.push(format!(
        "unchanged re-export (cache hit): {:.3} s (budget < 1 s)",
        again.as_secs_f64()
    ));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", dir.join("mirror.sqlite").display()));
    }
    let (miss, r) = export();
    out.push(format!(
        "cache-miss validation at {e} epochs (replay included): {:.2} s (budget < 60 s); {}",
        miss.as_secs_f64(),
        r.lines.first().cloned().unwrap_or_default()
    ));
    s.stop_indexer();
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    DENSE.store(std::env::args().any(|a| a == "dense"), Ordering::Relaxed);
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let small = args.first().copied().unwrap_or(10_000);
    let large = args.get(1).copied().unwrap_or(100_000);
    let epochs = args.get(2).copied().unwrap_or(100_000);
    let mut out = vec![format!(
        "# p8_bench small={small} large={large} epochs={epochs} vocabulary={} (SimDisk op_log, SQLite projections on disk)",
        if DENSE.load(Ordering::Relaxed) {
            "dense: 40 words, uniform"
        } else {
            "Zipf s=1 over 20,000 words"
        }
    )];
    let flush = |out: &mut Vec<String>| {
        for l in out.drain(..) {
            println!("{l}");
        }
    };
    flush(&mut out);
    recall_section(small, &mut out);
    flush(&mut out);
    // `large = 0` skips the large runs, `epochs = 0` the mirror.
    if large > 0 {
        recall_section(large, &mut out);
        flush(&mut out);
    }
    hybrid_section(small, 256, &mut out);
    hybrid_section(small, 0, &mut out);
    flush(&mut out);
    if large > 0 {
        hybrid_section(large, 1_024, &mut out);
        flush(&mut out);
    }
    if epochs > 0 {
        mirror_section(small, epochs, &mut out);
        flush(&mut out);
    }
}
