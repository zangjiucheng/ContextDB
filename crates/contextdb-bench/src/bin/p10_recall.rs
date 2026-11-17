//! `p10_recall [records=10000]` (ADR 0018 §13 G5): recall p99 with
//! `records` mirrors stays within +10 % of the same store without them,
//! at the same record count. Two in-process services (`Service` on
//! `SimDisk`, recall in real SQLite): the baseline holds 2·`records` local
//! records; the other holds `records` local records and `records` mirrors
//! written by the replica channel, with the team watermark at the live
//! position (no lag). The corpus and the queries are P9's.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const OWNER: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
const TEAM: [u8; 16] = [0x5a; 16];
type Svc = Service<SimDisk>;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn word(&mut self) -> String {
        let r = (self.next() % 20_000) as f64;
        format!("w{}", (r * r / 20_000.0) as u64)
    }
}
fn body(rng: &mut Rng, i: usize) -> String {
    let words: Vec<String> = (0..30).map(|_| rng.word()).collect();
    format!("# Note {i}\n\n{}\n", words.join(" "))
}
fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("p10-recall-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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
/// `n` records from index `from`, local or mirrors of `TEAM`.
fn fill(s: &Svc, from: usize, n: usize, mirrors: bool, rng: &mut Rng) {
    let hex: String = TEAM.iter().map(|b| format!("{b:02x}")).collect();
    for chunk in (from..from + n).collect::<Vec<_>>().chunks(100) {
        let mut blobs = Vec::new();
        let mut ops = Vec::new();
        for &i in chunk {
            let b = body(rng, i);
            let (key, lane, origin) = if mirrors {
                (
                    format!("team/{hex}/area{}/topic-{i}", i % 50),
                    format!("team:{hex}/main"),
                    Origin::Replica { team: TEAM },
                )
            } else {
                (
                    format!("area{}/topic-{i}", i % 50),
                    format!("l{}", i % 9),
                    Origin::Local,
                )
            };
            ops.push(Op::CreateLogical {
                origin,
                key,
                kind: RecordKind::Decision,
                scope_lane: lane,
                content: *blake3::hash(b.as_bytes()).as_bytes(),
                adopt_as_base: true,
            });
            blobs.push(b.into_bytes());
        }
        let mut batch = Batch::new(ops);
        if mirrors {
            batch.channel = Channel::Replica;
            s.actor.write(blobs, batch).unwrap().unwrap();
        } else {
            batch.channel = Channel::Admin;
            match s.handle(ADMIN, Request::Raw { blobs, batch }).response {
                Response::Committed { .. } => {}
                other => panic!("{other:?}"),
            }
        }
    }
    if mirrors {
        // The branch at the team's live position: mirrors read fresh.
        let scope = team_scope(&TEAM);
        let mut batch = Batch::new(vec![Op::ObserverWatermark {
            scope: scope.clone(),
            expected: None,
            position: b"tip".to_vec(),
            gap: false,
        }]);
        batch.channel = Channel::Replica;
        s.actor.write(Vec::new(), batch).unwrap().unwrap();
        s.actor
            .team_live
            .lock()
            .unwrap()
            .entry(TEAM)
            .or_default()
            .tip = Some("tip".into());
    }
}
fn wait_index(s: &Svc) {
    let e = s.actor.snapshot().epoch;
    while !s.index_health.wait(e, Duration::from_secs(2)) {}
}
fn session(s: &Svc) -> Binding {
    match s
        .handle(
            OWNER,
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
fn p99(s: &Svc, qs: &[String]) -> (f64, f64) {
    let b = session(s);
    let run = |q: &String| {
        let t = Instant::now();
        let r = s.handle(
            OWNER,
            Request::Recall {
                binding: b.clone(),
                query: q.clone(),
                scope: String::new(),
                mode: Mode::Certified,
                limit: 10,
                index_wait_ms: 200,
                opts: RecallOpts::default(),
            },
        );
        assert!(matches!(r.response, Response::Recall { .. }), "{r:?}");
        t.elapsed()
    };
    for q in qs.iter().take(50) {
        run(q);
    }
    let mut v: Vec<Duration> = qs.iter().map(run).collect();
    v.sort();
    let at = |p: usize| v[(v.len() * p).div_ceil(100).saturating_sub(1)].as_secs_f64() * 1000.;
    (at(50), at(99))
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);
    let mut results = Vec::new();
    for (tag, mirrors) in [("baseline", false), ("mirrors", true)] {
        let dir = temp(tag);
        let s = service(&dir);
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        fill(&s, 0, n, false, &mut rng);
        fill(&s, n, n, mirrors, &mut rng);
        wait_index(&s);
        let mut qrng = Rng(0xC0FF_EE00_1234_5678);
        let qs = queries(&mut qrng, 600, 2 * n);
        let (p50, p99) = p99(&s, &qs);
        println!(
            "{{\"part\":\"recall\",\"store\":\"{tag}\",\"records\":{},\"mirrors\":{},\"p50_ms\":{p50:.2},\"p99_ms\":{p99:.2}}}",
            2 * n,
            if mirrors { n } else { 0 }
        );
        results.push(p99);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }
    let rel = results[1] / results[0] - 1.0;
    println!(
        "{{\"part\":\"recall-delta\",\"p99_change_pct\":{:.1},\"budget_pct\":10}}",
        rel * 100.0
    );
}
