//! P8 G6 (ADR 0016 §10): retrieval quality on the dogfood stores.
//!
//! Inputs stay out of the repository (project text): store copies, the
//! labellers' files and the outputs live in a private directory.
//!
//! - `p8_g6 export <out-dir> <name>=<store-copy>...` writes, per store, every
//!   live head on `main` grouped by lane and kind, with its key and full
//!   body: what the labelling agents read. They never see a system's output.
//! - `p8_g6 pairs <config.json> <out.jsonl>` embeds every Decision, Plan and
//!   Fact head with the configured embedder and lists each head's three
//!   nearest neighbours in its lane (cosine ≥ 0.55, direct neighbours
//!   excluded), for the `min_cosine` calibration.
//! - `p8_g6 eval <config.json>` reports, per system (key only, key + FTS,
//!   hybrid), recall@5, recall@10 and MRR against the two labellers'
//!   relevance sets (agreed: both; lenient: either); token cost at budgets
//!   1k, 2k and 4k against the relevant records' full bodies; the `bytes/4`
//!   estimate against `claude -p`'s counts on a sample; and the
//!   `min_cosine` calibration against the pair labels.
//!
//! `config.json`:
//! ```json
//! {"stores": {"name": "/copy"}, "embed": ["argv..."], "model": "m", "dims": 768,
//!  "queries": "q.jsonl", "labels": ["a.jsonl", "b.jsonl"],
//!  "pairs": "pairs.jsonl", "pair_labels": ["pa.jsonl", "pb.jsonl"],
//!  "claude": {"model": "claude-haiku-5-5", "token_file": "/path"}}
//! ```
//! A query line is `{"id", "store", "query"}`; a label line `{"id",
//! "relevant": [key...]}`; a pair label line `{"id", "label": "conflict" |
//! "none"}`.
use contextdb_daemon::{
    actor,
    embedding::Settings,
    packing,
    proto::*,
    ranking::KeyRank,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, Payload, StdDisk, Store, session::SessionLog};
use contextdb_validate::embed::{CommandEmbedder, VectorLimits, dot};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
type Svc = Service<StdDisk>;

fn read_jsonl(p: &Path) -> Vec<Value> {
    std::fs::read_to_string(p)
        .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
fn stores(args: &[String]) -> Vec<(String, PathBuf)> {
    args.iter()
        .map(|a| {
            let (n, p) = a.split_once('=').expect("<name>=<store-copy>");
            (n.to_owned(), PathBuf::from(p))
        })
        .collect()
}
fn open(dir: &Path) -> Svc {
    let disk = StdDisk::default();
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk, dir, false, 64 << 20).unwrap();
    Service::new(actor::spawn(store).unwrap(), log, events).unwrap()
}
fn close(s: Svc) {
    s.handle(ADMIN, Request::Shutdown);
}
/// `(key, kind, lane, body)` of every live head on `main`.
fn live(s: &Svc) -> Vec<(LogicalId, String, RecordKind, String, String)> {
    let snap = s.actor.snapshot();
    let view = snap.kernel.branch(MAIN).unwrap();
    let mut out = Vec::new();
    for l in view.logicals() {
        let (Some(r), Some((key, kind, lane))) = (view.head(l), view.logical_info(l)) else {
            continue;
        };
        let body = match view.revision_content(r).map(|c| snap.log.read_blob(&c)) {
            Ok(Payload::Available(b)) => String::from_utf8_lossy(&b).into_owned(),
            _ => "(payload unavailable)".into(),
        };
        out.push((l, key, kind, lane, body));
    }
    out
}
fn export(out: &Path, stores: &[(String, PathBuf)]) {
    std::fs::create_dir_all(out).unwrap();
    for (name, dir) in stores {
        let s = open(dir);
        let mut heads = live(&s);
        heads.sort_by(|a, b| {
            (&a.3, format!("{:?}", a.2), &a.1).cmp(&(&b.3, format!("{:?}", b.2), &b.1))
        });
        let mut text = format!("# Store `{name}`: {} live records\n", heads.len());
        let mut section = String::new();
        for (_, key, kind, lane, body) in &heads {
            let sec = format!("{lane} / {kind:?}");
            if sec != section {
                text.push_str(&format!("\n## {sec}\n"));
                section = sec;
            }
            text.push_str(&format!("\n### `{key}`\n\n{}\n", body.trim_end()));
        }
        std::fs::write(out.join(format!("{name}.md")), text).unwrap();
        println!("{name}: {} records", heads.len());
        close(s);
    }
}
struct Config {
    stores: Vec<(String, PathBuf)>,
    embed: Vec<String>,
    model: String,
    dims: usize,
    raw: Value,
    base: PathBuf,
}
fn config(path: &Path) -> Config {
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    Config {
        stores: (raw["stores"].as_object().unwrap().iter())
            .map(|(k, v)| (k.clone(), PathBuf::from(v.as_str().unwrap())))
            .collect(),
        embed: (raw["embed"].as_array().unwrap().iter())
            .map(|v| v.as_str().unwrap().to_owned())
            .collect(),
        model: raw["model"].as_str().unwrap().to_owned(),
        dims: raw["dims"].as_u64().unwrap() as usize,
        base: path.parent().unwrap().to_path_buf(),
        raw,
    }
}
impl Config {
    fn file(&self, key: &str) -> Option<PathBuf> {
        self.raw[key].as_str().map(|f| self.base.join(f))
    }
    fn files(&self, key: &str) -> Vec<PathBuf> {
        (self.raw[key].as_array().into_iter().flatten())
            .filter_map(|v| v.as_str())
            .map(|f| self.base.join(f))
            .collect()
    }
    fn settings(&self) -> Settings {
        Settings {
            embedder: Arc::new(CommandEmbedder {
                program: PathBuf::from(&self.embed[0]),
                args: self.embed[1..].to_vec(),
                model: self.model.clone(),
                dims: self.dims,
            }),
            max_batch: 32,
            timeout: Duration::from_secs(120),
            limits: VectorLimits::default(),
        }
    }
}
fn with_vectors(c: &Config, dir: &Path) -> Svc {
    let s = open(dir);
    s.start_vectors(c.settings(), None).unwrap();
    let e = s.actor.snapshot().epoch;
    let t = Instant::now();
    loop {
        match s.index_report(e).vectors {
            IndexState::Ready { .. } => break,
            IndexState::Failed { error } if t.elapsed() > Duration::from_secs(30) => {
                panic!("embedder: {error}")
            }
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
        assert!(
            t.elapsed() < Duration::from_secs(900),
            "vectors never Ready"
        );
    }
    s
}
const KINDS: [RecordKind; 3] = [RecordKind::Decision, RecordKind::Plan, RecordKind::Fact];
fn pairs(c: &Config, out: &Path) {
    let mut f = std::fs::File::create(out).unwrap();
    let mut n = 0;
    for (name, dir) in &c.stores {
        let s = with_vectors(c, dir);
        let v = s.vectors.get().unwrap().clone();
        let snap = s.actor.snapshot();
        let view = snap.kernel.branch(MAIN).unwrap();
        let heads: Vec<_> = live(&s)
            .into_iter()
            .filter(|h| KINDS.contains(&h.2))
            .collect();
        let vec_of = |l: LogicalId| {
            view.head(l)
                .and_then(|r| view.revision_content(r).ok())
                .and_then(|c| v.vector(&c))
        };
        let mut seen = BTreeSet::new();
        for (l, key, kind, lane, body) in &heads {
            let Some(a) = vec_of(*l) else { continue };
            let skip: BTreeSet<LogicalId> = view.neighbour_logicals(*l).into_iter().collect();
            let mut near: Vec<(f32, usize)> = (heads.iter().enumerate())
                .filter(|(_, o)| o.0 != *l && o.3 == *lane && !skip.contains(&o.0))
                .filter_map(|(i, o)| Some((dot(&a, &vec_of(o.0)?), i)))
                .filter(|(cos, _)| *cos >= 0.55)
                .collect();
            near.sort_by(|x, y| y.0.total_cmp(&x.0));
            for (cos, i) in near.into_iter().take(3) {
                let o = &heads[i];
                let pair = ((*l).min(o.0), (*l).max(o.0));
                if !seen.insert(pair) {
                    continue;
                }
                n += 1;
                writeln!(
                    f,
                    "{}",
                    json!({
                        "id": format!("{name}-{n}"),
                        "store": name,
                        "cosine": cos,
                        "a": {"key": key, "kind": format!("{kind:?}"), "body": body},
                        "b": {"key": o.1, "kind": format!("{:?}", o.2), "body": o.4},
                    })
                )
                .unwrap();
            }
        }
        println!("{name}: {} heads, {n} pairs so far", heads.len());
        drop(snap);
        close(s);
    }
}
fn attach(s: &Svc) -> Binding {
    match s
        .handle(
            AGENT,
            Request::SessionAttach {
                harness: "g6".into(),
                harness_session: "g6".into(),
                repo: "/g6".into(),
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
fn recall(s: &Svc, b: &Binding, q: &str, opts: RecallOpts) -> Vec<Item> {
    match s
        .handle(
            AGENT,
            Request::Recall {
                binding: b.clone(),
                query: q.into(),
                scope: String::new(),
                mode: Mode::Advisory,
                limit: 10,
                index_wait_ms: 2_000,
                opts,
            },
        )
        .response
    {
        Response::Recall { items, .. } => items,
        other => panic!("{other:?}"),
    }
}
#[derive(Default, Clone)]
struct Score {
    n: usize,
    r5: f64,
    r10: f64,
    mrr: f64,
}
impl Score {
    fn add(&mut self, ranked: &[String], relevant: &BTreeSet<String>) {
        if relevant.is_empty() {
            return;
        }
        let hit = |k: usize| {
            ranked
                .iter()
                .take(k)
                .filter(|x| relevant.contains(*x))
                .count() as f64
                / relevant.len() as f64
        };
        self.n += 1;
        self.r5 += hit(5);
        self.r10 += hit(10);
        self.mrr += ranked
            .iter()
            .position(|x| relevant.contains(x))
            .map_or(0.0, |p| 1.0 / (p + 1) as f64);
    }
    fn line(&self, name: &str) -> String {
        let d = self.n.max(1) as f64;
        format!(
            "{name:<12} n={:<4} recall@5 {:.3}  recall@10 {:.3}  MRR {:.3}",
            self.n,
            self.r5 / d,
            self.r10 / d,
            self.mrr / d
        )
    }
}
fn key_only(s: &Svc, q: &str) -> Vec<String> {
    let snap = s.actor.snapshot();
    let view = snap.kernel.branch(MAIN).unwrap();
    let mut keyed: Vec<(KeyRank, LogicalId, String)> = view
        .logicals()
        .filter(|l| view.head(*l).is_some())
        .filter_map(|l| {
            let (key, ..) = view.logical_info(l)?;
            Some((KeyRank::of(&key, q)?, l, key))
        })
        .collect();
    keyed.sort();
    keyed.into_iter().take(10).map(|(.., k)| k).collect()
}
fn eval(c: &Config) {
    let queries = read_jsonl(&c.file("queries").expect("queries"));
    let labels: Vec<BTreeMap<String, BTreeSet<String>>> = (c.files("labels").iter())
        .map(|f| {
            read_jsonl(f)
                .into_iter()
                .map(|v| {
                    let rel = (v["relevant"].as_array().into_iter().flatten())
                        .filter_map(|k| k.as_str().map(str::to_owned))
                        .collect();
                    (v["id"].as_str().unwrap().to_owned(), rel)
                })
                .collect()
        })
        .collect();
    assert_eq!(labels.len(), 2, "two labellers");
    let mut systems: BTreeMap<&str, [Score; 2]> = BTreeMap::new();
    let mut tokens: BTreeMap<u32, (u64, u64, usize, usize)> = BTreeMap::new();
    let (mut agree, mut disputed, mut empty) = (0, 0, 0);
    let mut samples: Vec<String> = Vec::new();
    for (name, dir) in &c.stores {
        let qs: Vec<&Value> = queries
            .iter()
            .filter(|q| q["store"] == name.as_str())
            .collect();
        if qs.is_empty() {
            continue;
        }
        let sets = |q: &Value| {
            let id = q["id"].as_str().unwrap();
            let a = labels[0].get(id).cloned().unwrap_or_default();
            let b = labels[1].get(id).cloned().unwrap_or_default();
            (
                a.intersection(&b).cloned().collect::<BTreeSet<_>>(),
                a.union(&b).cloned().collect::<BTreeSet<_>>(),
                a == b,
            )
        };
        // Key only and key + FTS (no embedder).
        let s = open(dir);
        let b = attach(&s);
        let bodies: BTreeMap<String, String> = live(&s).into_iter().map(|h| (h.1, h.4)).collect();
        samples.extend(bodies.values().take(4).cloned());
        for q in &qs {
            let text = q["query"].as_str().unwrap();
            let (strict, lenient, same) = sets(q);
            if same {
                agree += 1;
            } else {
                disputed += 1;
            }
            if strict.is_empty() {
                empty += 1;
            }
            let k = key_only(&s, text);
            let f: Vec<String> = recall(&s, &b, text, RecallOpts::default())
                .into_iter()
                .map(|i| i.key)
                .collect();
            for (sys, ranked) in [("key", &k), ("key+fts", &f)] {
                let e = systems.entry(sys).or_default();
                e[0].add(ranked, &strict);
                e[1].add(ranked, &lenient);
            }
            // Token cost at budgets, against the relevant full bodies.
            let full: u64 = (lenient.iter())
                .filter_map(|k| bodies.get(k))
                .map(|b| packing::tokens(b) as u64)
                .sum();
            for budget in [1_000u32, 2_000, 4_000] {
                let items = recall(
                    &s,
                    &b,
                    text,
                    RecallOpts {
                        budget_tokens: Some(budget),
                        ..RecallOpts::default()
                    },
                );
                let spent: u64 = (items.iter())
                    .map(|i| {
                        (packing::HEADER_TOKENS
                            + packing::tokens(&i.key)
                            + i.body.as_ref().map_or(0, |r| packing::tokens(&r.text)))
                            as u64
                    })
                    .sum();
                let found = items.iter().filter(|i| lenient.contains(&i.key)).count();
                let t = tokens.entry(budget).or_default();
                t.0 += spent;
                t.1 += full;
                t.2 += found;
                t.3 += lenient.len();
            }
        }
        close(s);
        // Hybrid.
        let s = with_vectors(c, dir);
        let b = attach(&s);
        for q in &qs {
            let text = q["query"].as_str().unwrap();
            let (strict, lenient, _) = sets(q);
            let h: Vec<String> = recall(&s, &b, text, RecallOpts::default())
                .into_iter()
                .map(|i| i.key)
                .collect();
            let e = systems.entry("hybrid").or_default();
            e[0].add(&h, &strict);
            e[1].add(&h, &lenient);
        }
        close(s);
        println!("{name}: {} queries", qs.len());
    }
    println!(
        "\n## Relevance (labellers agreed exactly on {agree} queries, differed on {disputed}; {empty} with no agreed relevant record)"
    );
    for (which, label) in [
        (0, "agreed (both labellers)"),
        (1, "lenient (either labeller)"),
    ] {
        println!("\n### {label}");
        for (sys, sc) in &systems {
            println!("{}", sc[which].line(sys));
        }
    }
    println!("\n## Token cost (packed recall, outline, bytes/4)");
    for (budget, (spent, full, found, total)) in &tokens {
        println!(
            "budget {budget}: {spent} tokens returned vs {full} for the relevant full bodies; {found} of {total} relevant records included"
        );
    }
    claude_check(c, &samples);
    calibrate(c);
}
/// `bytes/4` against `claude -p`'s input token counts on sample bodies.
fn claude_check(c: &Config, samples: &[String]) {
    let Some(model) = c.raw["claude"]["model"].as_str() else {
        println!("\n## bytes/4 vs claude -p: not configured");
        return;
    };
    let token = PathBuf::from(c.raw["claude"]["token_file"].as_str().unwrap());
    let cli = contextdb_validate::claude::ClaudeCli {
        name: "g6".into(),
        program: "claude".into(),
        model: model.into(),
        version: "g6".into(),
        isolated_login: None,
        token_file: Some(token),
    };
    use contextdb_validate::{Item, Task, Validator};
    let item = |body: &str| Item {
        handle: "x@0".into(),
        key: "x".into(),
        kind: RecordKind::Fact,
        body: body.into(),
    };
    let input = |body: &str| -> Option<u32> {
        let t = Task::pair([0; 16], MAIN, item(body), item(""));
        cli.validate(&t, Instant::now() + Duration::from_secs(120))
            .ok()
            .and_then(|o| o.tokens)
            .map(|(i, _)| i)
    };
    let Some(base) = input("") else {
        println!("\n## bytes/4 vs claude -p: the baseline call failed");
        return;
    };
    println!("\n## bytes/4 vs claude -p (input tokens over an empty-body baseline of {base})");
    let mut ratios = Vec::new();
    for s in samples.iter().take(10) {
        if let Some(n) = input(s) {
            let real = n.saturating_sub(base).max(1);
            let est = packing::tokens(s);
            ratios.push(est as f64 / real as f64);
            println!(
                "{} bytes: estimate {est}, claude {real} (ratio {:.2})",
                s.len(),
                est as f64 / real as f64
            );
        }
    }
    if !ratios.is_empty() {
        ratios.sort_by(f64::total_cmp);
        println!(
            "ratio estimate/claude: min {:.2}, median {:.2}, max {:.2}",
            ratios[0],
            ratios[ratios.len() / 2],
            ratios[ratios.len() - 1]
        );
    }
}
/// `min_cosine` against the agreed pair labels.
fn calibrate(c: &Config) {
    let (Some(pairs), files) = (c.file("pairs"), c.files("pair_labels")) else {
        return;
    };
    if files.len() != 2 {
        return;
    }
    let cos: BTreeMap<String, f64> = read_jsonl(&pairs)
        .into_iter()
        .map(|v| {
            (
                v["id"].as_str().unwrap().to_owned(),
                v["cosine"].as_f64().unwrap(),
            )
        })
        .collect();
    let la: BTreeMap<String, String> = read_jsonl(&files[0])
        .into_iter()
        .map(|v| {
            (
                v["id"].as_str().unwrap().to_owned(),
                v["label"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let lb: BTreeMap<String, String> = read_jsonl(&files[1])
        .into_iter()
        .map(|v| {
            (
                v["id"].as_str().unwrap().to_owned(),
                v["label"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let mut agreed: Vec<(f64, bool)> = Vec::new();
    let mut disputed = 0;
    for (id, x) in &cos {
        match (la.get(id), lb.get(id)) {
            (Some(a), Some(b)) if a == b => agreed.push((*x, a == "conflict")),
            (Some(_), Some(_)) => disputed += 1,
            _ => {}
        }
    }
    let positives = agreed.iter().filter(|p| p.1).count();
    println!(
        "\n## min_cosine calibration: {} agreed pairs ({positives} conflict), {disputed} disputed",
        agreed.len()
    );
    let mut best = (0.0, 0.82);
    for t in (55..=95).step_by(1) {
        let t = t as f64 / 100.0;
        let kept: Vec<_> = agreed.iter().filter(|p| p.0 >= t).collect();
        let tp = kept.iter().filter(|p| p.1).count() as f64;
        let precision = if kept.is_empty() {
            0.0
        } else {
            tp / kept.len() as f64
        };
        let recall = if positives == 0 {
            0.0
        } else {
            tp / positives as f64
        };
        let f1 = if precision + recall == 0.0 {
            0.0
        } else {
            2.0 * precision * recall / (precision + recall)
        };
        if ((t * 100.0).round() as u32).is_multiple_of(5) || (t - 0.82).abs() < 1e-9 {
            println!(
                "min_cosine {t:.2}: {} pairs kept, precision {precision:.2}, recall {recall:.2}, F1 {f1:.2}",
                kept.len()
            );
        }
        if f1 > best.0 {
            best = (f1, t);
        }
    }
    println!("best F1 {:.2} at min_cosine {:.2}", best.0, best.1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("export") => export(Path::new(&args[1]), &stores(&args[2..])),
        Some("pairs") => pairs(&config(Path::new(&args[1])), Path::new(&args[2])),
        Some("eval") => eval(&config(Path::new(&args[1]))),
        _ => {
            eprintln!(
                "usage: p8_g6 export <out> <name>=<copy>... | pairs <config> <out> | eval <config>"
            );
            std::process::exit(2);
        }
    }
}
