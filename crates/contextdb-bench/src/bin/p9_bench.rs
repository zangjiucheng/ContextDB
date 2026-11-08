//! `p9_bench [small=10000] [large=100000] [migrate_mib=1024]` (ADR 0017 §11
//! G5). In process (`Service` on `SimDisk`, projections in real SQLite
//! files) for recall and shred; `StdDisk` in a temporary directory for
//! reads, the key log and the migration.
//!
//! - **recall p99** at `small` and `large` records: without an access
//!   document (the P8 path), and with one of 5 principals, 10 lanes and 50
//!   denies, for a principal who reads every row and one who reads about
//!   1 % (skipping dominates).
//! - **read p99** of a 4 KiB payload (decryption included).
//! - **shred of one content** at `large` records: the request returns
//!   once the key log and every projection are purged.
//! - **reported:** key-log load at open with `large` keys, and `ctx
//!   migrate` on `migrate_mib` MiB of plaintext packs.
//!
//! Commit p99 with encrypted blobs and key-log syncs is `store_bench`'s,
//! compared against the same binary at the P8 commit.
use contextdb_daemon::{
    actor,
    principals::PrincipalMap,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_proto::fsguard;
use contextdb_store::{OpenOptions, SimDisk, StdDisk, Store, session::SessionLog};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const OWNER: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
/// Reads every lane.
const FULL: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(16),
};
/// Reads only `l9`, which holds about 1 % of the rows.
const NARROW: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(17),
};
type Svc = Service<SimDisk>;

fn pct(v: &mut [Duration], p: usize) -> f64 {
    v.sort();
    v[(v.len() * p).div_ceil(100).saturating_sub(1)].as_secs_f64() * 1000.
}
fn temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "p9-bench-{tag}-{}-{}",
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
    fn word(&mut self) -> String {
        // Zipf-like: small ranks are common.
        let r = (self.next() % 20_000) as f64;
        let rank = (r * r / 20_000.0) as u64;
        format!("w{rank}")
    }
}
fn body(rng: &mut Rng, i: usize) -> String {
    let words: Vec<String> = (0..30).map(|_| rng.word()).collect();
    format!("# Note {i}\n\n{}\n", words.join(" "))
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
    let mut batch = Batch::new(ops);
    batch.channel = Channel::Admin;
    match s.handle(ADMIN, Request::Raw { blobs, batch }).response {
        Response::Committed { result } => result,
        other => panic!("{other:?}"),
    }
}
fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}
/// 5 principals, 10 lanes, 50 denies (ADR 0017 §11 G5).
fn doc() -> AccessDoc {
    let principals: BTreeMap<String, u32> = [
        ("alice".to_owned(), fsguard::current_uid()),
        ("full".to_owned(), 60_001),
        ("narrow".to_owned(), 60_002),
        ("p3".to_owned(), 60_003),
        ("p4".to_owned(), 60_004),
    ]
    .into();
    let mut lanes = BTreeMap::new();
    for n in 0..10 {
        let members = if n == 9 {
            set(&["*members"])
        } else {
            set(&["alice", "full", "p3", "p4"])
        };
        let mut e = LaneEntry {
            members: members.clone(),
            writers: members,
            ..LaneEntry::owner_only()
        };
        if n == 0 {
            for d in 0..50 {
                e.deny.insert(("p3".to_owned(), format!("never{d}.")));
            }
        }
        lanes.insert(format!("l{n}"), e);
    }
    AccessDoc {
        version: 1,
        owner: "alice".into(),
        principals,
        groups: BTreeMap::new(),
        lanes,
        code_lanes: LaneEntry::code_default(),
        personal_egress: false,
        default_lane: LaneEntry::owner_only(),
    }
}
/// `n` records over the 10 lanes: 1 % in `l9`, the rest spread over l0–l8.
fn fill(s: &Svc, n: usize, rng: &mut Rng) {
    for chunk in (0..n).collect::<Vec<_>>().chunks(100) {
        let mut blobs = Vec::new();
        let mut ops = Vec::new();
        for &i in chunk {
            let b = body(rng, i);
            let lane = if i % 100 == 0 {
                "l9".to_owned()
            } else {
                format!("l{}", i % 9)
            };
            ops.push(Op::CreateLogical {
                key: format!("area{}/topic-{i}", i % 50),
                kind: RecordKind::Decision,
                scope_lane: lane,
                content: content_hash(b.as_bytes()),
                adopt_as_base: true,
            });
            blobs.push(b.into_bytes());
        }
        commit(s, blobs, ops);
    }
}
fn wait_index(s: &Svc) {
    let e = s.actor.snapshot().epoch;
    while !s.index_health.wait(e, Duration::from_secs(2)) {}
}
fn session(s: &Svc, id: Identity) -> Binding {
    match s
        .handle(
            id,
            Request::SessionStart {
                harness: "bench".into(),
                repo: "/repo".into(),
                head_sha: "h".into(),
                parent: None,
                branch: None,
                git_branch: None,
            },
        )
        .response
    {
        Response::Session { session } => Binding::root(session),
        other => panic!("{other:?}"),
    }
}
fn recall(s: &Svc, id: Identity, b: &Binding, q: &str) -> Duration {
    let t = Instant::now();
    let r = s.handle(
        id,
        Request::Recall {
            binding: b.clone(),
            query: q.into(),
            scope: String::new(),
            mode: Mode::Certified,
            limit: 10,
            index_wait_ms: 200,
            opts: RecallOpts::default(),
        },
    );
    let dt = t.elapsed();
    assert!(matches!(r.response, Response::Recall { .. }), "{r:?}");
    dt
}
fn queries(rng: &mut Rng, n: usize, records: usize) -> Vec<String> {
    (0..n)
        .map(|i| match i % 4 {
            0 => rng.word(),
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
fn p99(s: &Svc, id: Identity, qs: &[String]) -> (f64, f64) {
    let b = session(s, id);
    for q in qs.iter().take(50) {
        recall(s, id, &b, q);
    }
    let mut v: Vec<Duration> = qs.iter().map(|q| recall(s, id, &b, q)).collect();
    (pct(&mut v, 50), pct(&mut v, 99))
}

fn recall_section(n: usize, out: &mut Vec<String>) {
    let dir = temp(&format!("recall{n}"));
    let s = service(&dir);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ n as u64);
    let t = Instant::now();
    fill(&s, n, &mut rng);
    wait_index(&s);
    out.push(format!(
        "## recall, {n} records (filled and indexed in {:.1} s)",
        t.elapsed().as_secs_f64()
    ));
    let qs = queries(&mut rng, 600, n);
    let (b50, b99) = p99(&s, OWNER, &qs);
    out.push(format!(
        "no access document (P8 path): p50 {b50:.2} ms, p99 {b99:.2} ms"
    ));
    s.set_principals(
        PrincipalMap::new(
            Some("alice"),
            fsguard::current_uid(),
            [
                ("full".to_owned(), 60_001, PrincipalId(16)),
                ("narrow".to_owned(), 60_002, PrincipalId(17)),
                ("p3".to_owned(), 60_003, PrincipalId(18)),
                ("p4".to_owned(), 60_004, PrincipalId(19)),
            ],
        )
        .unwrap(),
    );
    let r = s.handle(
        ADMIN,
        Request::SetAccess {
            doc: doc(),
            client_txn_id: None,
        },
    );
    assert!(matches!(r.response, Response::Committed { .. }), "{r:?}");
    wait_index(&s);
    let (o50, o99) = p99(&s, OWNER, &qs);
    let (f50, f99) = p99(&s, FULL, &qs);
    let (n50, n99) = p99(&s, NARROW, &qs);
    out.push(format!(
        "access document, store owner: p50 {o50:.2} ms, p99 {o99:.2} ms ({:+.1} % p99)",
        (o99 / b99 - 1.0) * 100.0
    ));
    out.push(format!(
        "access document, reads every row: p50 {f50:.2} ms, p99 {f99:.2} ms ({:+.1} % p99)",
        (f99 / b99 - 1.0) * 100.0
    ));
    out.push(format!(
        "access document, reads ~1 %: p50 {n50:.2} ms, p99 {n99:.2} ms ({:+.1} % p99)",
        (n99 / b99 - 1.0) * 100.0
    ));
    s.stop_indexer();
    let _ = std::fs::remove_dir_all(&dir);
}

fn read_section(out: &mut Vec<String>) {
    let dir = temp("read");
    let (mut store, _) = Store::open(
        StdDisk::default(),
        &dir,
        &OpenOptions {
            create: true,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let mut hashes = Vec::new();
    for i in 0..200u32 {
        let mut b = vec![0u8; 4096];
        b[..4].copy_from_slice(&i.to_le_bytes());
        hashes.push(store.put_blob(&b).unwrap());
    }
    let mut v = Vec::new();
    for round in 0..10 {
        for h in &hashes {
            let t = Instant::now();
            let p = store.get_blob(h).unwrap();
            if round > 0 {
                v.push(t.elapsed());
            }
            assert!(matches!(p, contextdb_store::Payload::Available(_)));
        }
    }
    out.push(format!(
        "## read of a 4 KiB payload (StdDisk, decryption included): p50 {:.3} ms, p99 {:.3} ms (budget: within +0.5 ms of P8)",
        pct(&mut v, 50),
        pct(&mut v, 99)
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

fn shred_section(n: usize, out: &mut Vec<String>) {
    let dir = temp("shred");
    let s = service(&dir);
    let mut rng = Rng(0x51ED_0001 ^ n as u64);
    fill(&s, n, &mut rng);
    wait_index(&s);
    let t = Instant::now();
    let r = s.handle(
        ADMIN,
        Request::Shred {
            key: "area7/topic-7".into(),
            rev: None,
            with_reviews: false,
            confirm: true,
            client_txn_id: None,
        },
    );
    let dt = t.elapsed();
    let purged = matches!(r.response, Response::Shred { purged: true, .. });
    out.push(format!(
        "## shred of one content at {n} records (commit, key-log rewrite, purge of every projection): {:.2} s, purged={purged} (budget < 5 s)",
        dt.as_secs_f64()
    ));
    s.stop_indexer();
    let _ = std::fs::remove_dir_all(&dir);
}

fn key_log_section(n: usize, out: &mut Vec<String>) {
    let dir = temp("keys");
    {
        let (mut store, _) = Store::open(
            StdDisk::default(),
            &dir,
            &OpenOptions {
                create: true,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let blobs: Vec<Vec<u8>> = (0..n)
            .map(|i| format!("payload {i}").into_bytes())
            .collect();
        for chunk in blobs.chunks(1000) {
            let refs: Vec<&[u8]> = chunk.iter().map(Vec::as_slice).collect();
            store.put_blobs(&refs).unwrap();
        }
    }
    let t = Instant::now();
    let (store, _) = Store::open(StdDisk::default(), &dir, &OpenOptions::default()).unwrap();
    out.push(format!(
        "## reported: open with {n} keys (key-log load and pack scan): {:.2} s",
        t.elapsed().as_secs_f64()
    ));
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

fn migrate_section(mib: usize, out: &mut Vec<String>) {
    let dir = temp("migrate");
    let disk = StdDisk::default();
    let per = 256 * 1024;
    let count = mib * 1024 * 1024 / per;
    let blobs = dir.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    let mut batches = Vec::new();
    let mut seg = Vec::new();
    let mut segments = 0;
    for i in 0..count {
        let mut b = vec![(i % 251) as u8; per];
        b[..8].copy_from_slice(&(i as u64).to_le_bytes());
        batches.push(Batch::new(vec![Op::CreateLogical {
            key: format!("m/{i}"),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: content_hash(&b),
            adopt_as_base: true,
        }]));
        seg.extend(contextdb_store::plaintext_pack_record(&b));
        if seg.len() >= 64 << 20 || i + 1 == count {
            segments += 1;
            std::fs::write(blobs.join(format!("pack-{segments:08}")), &seg).unwrap();
            seg.clear();
        }
    }
    contextdb_store::migrate::write_format4(&disk, &dir, &batches).unwrap();
    let t = Instant::now();
    let r = contextdb_store::migrate::migrate(&disk, &dir).unwrap();
    let enc = r.encryption.unwrap();
    out.push(format!(
        "## reported: ctx migrate of {mib} MiB of packs ({} segments, {} records): {:.1} s",
        enc.segments,
        enc.records,
        t.elapsed().as_secs_f64()
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize, d: usize| args.get(i).and_then(|a| a.parse().ok()).unwrap_or(d);
    let (small, large, mib) = (arg(0, 10_000), arg(1, 100_000), arg(2, 1024));
    let mut out = vec![format!("# p9_bench ({})", std::env::consts::ARCH)];
    for n in [small, large] {
        recall_section(n, &mut out);
        println!("{}", out.join("\n"));
        out.clear();
    }
    read_section(&mut out);
    shred_section(large, &mut out);
    key_log_section(large, &mut out);
    migrate_section(mib, &mut out);
    println!("{}", out.join("\n"));
}
