//! P7 G6 (ADR 0015 §11): the real LLM evaluation, O2 numbers and the
//! `candidate_min_score` calibration.
//!
//! - `p7_g6 collect <items.jsonl> <store-copy>...` builds the items:
//!   - **dogfood rule tasks**: every enabled rule of each store copy with a
//!     premise whose logical's head is no longer the cited revision (the
//!     cited body and the current head's body). The stores cite with Cur
//!     and Pin, never Sem, so these are the real premise changes;
//!   - **dogfood pairs**: every candidate pair in the copy's
//!     `candidates.sqlite`, with its bm25 score;
//!   - **generated** rule tasks and pairs from 20 domains, labelled by
//!     construction (the deterministic oracle), with pair scores from a
//!     recall index over the generated records.
//!
//!   Dogfood items carry no label: independent agents label them.
//! - `p7_g6 run <items.jsonl> <out.jsonl> <model> <token-file> [workers]`
//!   calls `claude -p` through the sandboxed `ClaudeCli` adapter per item.
//! - `p7_g6 score <items.jsonl> <outcomes.jsonl> <refs.jsonl>...` reports
//!   calls, tokens, agreement with the references, disputed items and the
//!   score calibration. A reference line is `{"id", "label"}`; with two
//!   reference files, items where they differ are disputed.
//! - `p7_g6 o2 <seeds> <store-copy>...` reports jobs per head move and the
//!   calls a per-revision union vector would save.
use contextdb_core::{CommitStats, Counting, Db};
use contextdb_model::*;
use contextdb_store::{OpenOptions, Payload, StdDisk, Store, recall::RecallIndex, recall::Row};
use contextdb_validate::claude::ClaudeCli;
use contextdb_validate::{Item, PremiseChange, Task};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn kind_name(k: RecordKind) -> String {
    format!("{k:?}")
}
fn kind_of(s: &str) -> RecordKind {
    use RecordKind::*;
    [
        Fact,
        Assumption,
        Decision,
        Plan,
        Convention,
        Procedure,
        Artifact,
        Evidence,
        CodeAnchor,
        Summary,
    ]
    .into_iter()
    .find(|k| kind_name(*k) == s)
    .unwrap_or(Fact)
}
fn item_json(i: &Item) -> Value {
    json!({"handle": i.handle, "key": i.key, "kind": kind_name(i.kind), "body": i.body})
}
fn item_of(v: &Value) -> Item {
    Item {
        handle: v["handle"].as_str().unwrap_or("").into(),
        key: v["key"].as_str().unwrap_or("").into(),
        kind: kind_of(v["kind"].as_str().unwrap_or("Fact")),
        body: v["body"].as_str().unwrap_or("").into(),
    }
}
fn job_id(id: &str) -> [u8; 16] {
    contextdb_proto::proto::content_hash(id.as_bytes())[..16]
        .try_into()
        .unwrap()
}
fn task_of(v: &Value) -> Task {
    let id = v["id"].as_str().unwrap();
    if v["kind"] == "pair" {
        Task::pair(job_id(id), MAIN, item_of(&v["a"]), item_of(&v["b"]))
    } else {
        Task {
            job: job_id(id),
            branch: MAIN,
            rule: RuleId(0),
            curvec: vec![(LogicalId(0), RevisionId(1))],
            conclusion: item_of(&v["conclusion"]),
            premises: (v["premises"].as_array().unwrap().iter())
                .map(|p| PremiseChange {
                    key: p["key"].as_str().unwrap().into(),
                    kind: kind_of(p["kind"].as_str().unwrap()),
                    asserted: item_of(&p["asserted"]),
                    current: item_of(&p["current"]),
                })
                .collect(),
            pair: None,
        }
    }
}

// ---- collect -------------------------------------------------------------

struct Copy {
    name: String,
    store: Store<StdDisk>,
}
fn open_copy(dir: &Path) -> Copy {
    let (store, _) = Store::open(StdDisk::default(), dir, &OpenOptions::default())
        .unwrap_or_else(|e| panic!("{}: {e:?}", dir.display()));
    Copy {
        name: dir.file_name().unwrap().to_string_lossy().into_owned(),
        store,
    }
}
fn body(c: &Copy, content: &ContentHash) -> String {
    match c.store.get_blob(content) {
        Ok(Payload::Available(b)) => String::from_utf8_lossy(&b).into_owned(),
        _ => "(body unavailable)".into(),
    }
}
fn item(c: &Copy, i: &Inputs, r: RevisionId) -> Item {
    let rev = &i.revisions[r.index()];
    let l = &i.logicals[rev.logical.index()];
    Item {
        handle: format!("{}@{}", l.key, r.0),
        key: l.key.clone(),
        kind: l.kind,
        body: body(c, &rev.content),
    }
}
fn dogfood_rules(c: &Copy, out: &mut Vec<Value>) {
    let k = c.store.kernel().unwrap();
    let view = k.branch(MAIN).unwrap();
    let i = view.db().inputs();
    for (n, j) in i.rules.iter().enumerate() {
        if !j.enabled {
            continue;
        }
        let mut premises = Vec::new();
        // The stores' agents cite with Cur and Pin, not Sem: any premise
        // whose logical moved past the cited revision is a real change.
        let mut edges = BTreeSet::new();
        for p in j.premises.iter() {
            let l = i.revisions[p.premise.index()].logical;
            let head = i.heads[l.index()];
            if head != p.premise {
                edges.insert(format!("{:?}", p.kind));
                premises.push(PremiseChange {
                    key: i.logicals[l.index()].key.clone(),
                    kind: i.logicals[l.index()].kind,
                    asserted: item(c, &i, p.premise),
                    current: item(c, &i, head),
                });
            }
        }
        if premises.is_empty() {
            continue;
        }
        out.push(json!({
            "id": format!("{}/rule/{n}", c.name),
            "source": "dogfood",
            "kind": "rule",
            "edges": edges.into_iter().collect::<Vec<_>>(),
            "conclusion": item_json(&item(c, &i, j.conclusion)),
            "premises": premises.iter().map(|p| json!({
                "key": p.key, "kind": kind_name(p.kind),
                "asserted": item_json(&p.asserted), "current": item_json(&p.current),
            })).collect::<Vec<_>>(),
        }));
    }
}
fn dogfood_pairs(c: &Copy, dir: &Path, out: &mut Vec<Value>) {
    let conn = rusqlite_open(&dir.join("candidates.sqlite"));
    let k = c.store.kernel().unwrap();
    let view = k.branch(MAIN).unwrap();
    let i = view.db().inputs();
    for (id, a, b, score) in conn {
        if a as usize >= i.revisions.len() || b as usize >= i.revisions.len() {
            continue;
        }
        out.push(json!({
            "id": format!("{}/pair/{id}", c.name),
            "source": "dogfood",
            "kind": "pair",
            "score": score,
            "a": item_json(&item(c, &i, RevisionId(a))),
            "b": item_json(&item(c, &i, RevisionId(b))),
        }));
    }
}
/// `(id, a_rev, b_rev, score)` of every main-branch candidate pair.
fn rusqlite_open(path: &Path) -> Vec<(i64, u32, u32, f64)> {
    let t = contextdb_daemon::candidates::Candidates::open(Some(path)).unwrap();
    (t.all().unwrap().into_iter())
        .filter(|c| c.branch == MAIN)
        .map(|c| (c.id as i64, c.a.0, c.b.0, c.score))
        .collect()
}

/// (subject, v0, v1, an unrelated but lexically close statement).
const DOMAINS: [(&str, &str, &str, &str); 20] = [
    (
        "the orders service",
        "PostgreSQL 16",
        "MongoDB 7",
        "exposes its REST API on port 8080",
    ),
    (
        "the session store",
        "the shared Redis cluster",
        "process memory only",
        "expires idle sessions after 30 minutes",
    ),
    (
        "the web frontend",
        "React 19",
        "Svelte 5",
        "is served behind the company CDN",
    ),
    (
        "production deployment",
        "Kubernetes on AWS EKS",
        "a single VM managed by systemd",
        "happens every Tuesday after the release review",
    ),
    (
        "user login",
        "OAuth through GitHub",
        "usernames and passwords stored in our own database",
        "shows the company logo on the sign-in page",
    ),
    (
        "continuous integration",
        "GitHub Actions",
        "a self-hosted Jenkins server",
        "posts its results to the team chat channel",
    ),
    (
        "the backend",
        "Rust",
        "Go",
        "keeps its source code in the monorepo under services/",
    ),
    (
        "the event pipeline",
        "Apache Kafka",
        "RabbitMQ",
        "has a dashboard owned by the data team",
    ),
    (
        "application logging",
        "structured JSON lines written to stdout",
        "plain-text log files rotated daily on disk",
        "is reviewed in the weekly operations meeting",
    ),
    (
        "the public API",
        "GraphQL",
        "REST with OpenAPI",
        "is documented in the developer portal",
    ),
    (
        "payment processing",
        "Stripe",
        "PayPal",
        "sends a receipt email after each purchase",
    ),
    (
        "timestamp storage",
        "UTC everywhere",
        "the server's local time zone",
        "is covered by the data-retention policy",
    ),
    (
        "the JavaScript packages",
        "pnpm",
        "npm",
        "are audited for licenses every quarter",
    ),
    (
        "the integration test suite",
        "a real PostgreSQL instance started in Docker",
        "in-memory mocks only, never a real database",
        "runs on every pull request",
    ),
    (
        "the product catalog cache",
        "a 5-minute TTL",
        "no caching at all",
        "is warmed after each deploy",
    ),
    (
        "record identifiers",
        "UUIDv7",
        "auto-increment integers",
        "appear in the admin console URLs",
    ),
    (
        "the git workflow",
        "trunk-based development with short-lived branches",
        "git-flow with long-lived release branches",
        "requires signed commits",
    ),
    (
        "secret management",
        "HashiCorp Vault",
        ".env files committed to the repository",
        "is owned by the platform team",
    ),
    (
        "the mobile apps",
        "native Swift and Kotlin code",
        "a single React Native codebase",
        "are released through the app stores every two weeks",
    ),
    (
        "product search",
        "Elasticsearch",
        "PostgreSQL full-text search",
        "logs every query for relevance tuning",
    ),
];
fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}
fn gen_item(key: &str, rev: u32, kind: RecordKind, body: String) -> Item {
    Item {
        handle: format!("{key}@{rev}"),
        key: key.into(),
        kind,
        body,
    }
}
fn generated(out: &mut Vec<Value>) {
    let mut records: Vec<(String, String)> = Vec::new();
    let mut pairs: Vec<(String, usize, usize)> = Vec::new();
    for (d, (subject, v0, v1, other)) in DOMAINS.iter().enumerate() {
        let fact = format!("gen/{d}/fact");
        let plan = format!("gen/{d}/plan");
        let asserted = gen_item(
            &fact,
            0,
            RecordKind::Fact,
            format!("{} uses {v0}.", cap(subject)),
        );
        let conclusions = [
            gen_item(
                &plan,
                1,
                RecordKind::Plan,
                format!(
                    "Build the next milestone of {subject} on {v0}, relying on features specific to {v0}."
                ),
            ),
            gen_item(
                &format!("gen/{d}/decision"),
                3,
                RecordKind::Decision,
                format!("All code that touches {subject} goes through the official {v0} client library."),
            ),
            gen_item(
                &format!("gen/{d}/runbook"),
                4,
                RecordKind::Procedure,
                format!(
                    "Runbook: when {subject} misbehaves, first check the {v0} dashboards and restart the {v0} component."
                ),
            ),
        ];
        for (c, conclusion) in conclusions.iter().enumerate() {
            for (case, current, label) in [
                (
                    "moved",
                    format!("{} has moved from {v0} to {v1}; {v0} is no longer used.", cap(subject)),
                    "conflict",
                ),
                (
                    "confirmed",
                    format!(
                        "{} uses {v0}; this was re-confirmed in the latest architecture review.",
                        cap(subject)
                    ),
                    "reinforcement",
                ),
            ] {
                out.push(json!({
                    "id": format!("gen/rule/{d}/{c}/{case}"),
                    "source": "generated",
                    "kind": "rule",
                    "oracle": label,
                    "conclusion": item_json(conclusion),
                    "premises": [{
                        "key": fact, "kind": "Fact",
                        "asserted": item_json(&asserted),
                        "current": item_json(&gen_item(&fact, 2, RecordKind::Fact, current)),
                    }],
                }));
            }
        }
        let only = |v: &str, not: Option<&str>| match not {
            Some(n) => format!("{} must use only {v}; {n} is not allowed.", cap(subject)),
            None => format!("{} must use only {v}.", cap(subject)),
        };
        let a = only(v0, None);
        let b = only(v1, Some(v0));
        let c = format!("{} {other}.", cap(subject));
        let base = records.len();
        records.push((format!("gen/{d}/a"), a));
        records.push((format!("gen/{d}/b"), b));
        records.push((format!("gen/{d}/c"), c));
        pairs.push(("conflict".into(), base, base + 1));
        pairs.push(("none".into(), base, base + 2));
    }
    // Scores as the generator computes them: the recall index over the
    // generated records, `a`'s top terms, `b`'s bm25.
    let mut idx = RecallIndex::open(None).unwrap();
    let entries: Vec<_> = (records.iter().enumerate())
        .map(|(n, (key, body))| {
            (
                LogicalId(n as u32),
                vec![Row {
                    branch: MAIN,
                    rev: RevisionId(n as u32),
                    key: key.clone(),
                    kind: RecordKind::Decision,
                    scope: "repo".into(),
                    body: body.clone(),
                }],
            )
        })
        .collect();
    idx.update(Epoch(1), &entries).unwrap();
    for (label, a, b) in pairs {
        let (ka, ba) = &records[a];
        let (kb, bb) = &records[b];
        let terms = idx
            .top_terms(&format!("{ka} {ba}"), contextdb_daemon::candidates::TERMS)
            .unwrap();
        let score = (idx.any_of(&terms, "repo", MAIN, 100).unwrap().into_iter())
            .find(|h| h.logical.0 as usize == b)
            .map_or(0.0, |h| h.score);
        let d = ka.split('/').nth(1).unwrap();
        out.push(json!({
            "id": format!("gen/pair/{d}/{label}"),
            "source": "generated",
            "kind": "pair",
            "oracle": label,
            "score": score,
            "a": item_json(&gen_item(ka, a as u32, RecordKind::Decision, ba.clone())),
            "b": item_json(&gen_item(kb, b as u32, RecordKind::Decision, bb.clone())),
        }));
    }
}
fn collect(out: &Path, dirs: &[PathBuf]) {
    let mut items = Vec::new();
    for dir in dirs {
        let c = open_copy(dir);
        dogfood_rules(&c, &mut items);
        dogfood_pairs(&c, dir, &mut items);
    }
    generated(&mut items);
    let mut f = std::fs::File::create(out).unwrap();
    for i in &items {
        writeln!(f, "{i}").unwrap();
    }
    let count = |src: &str, kind: &str| {
        (items.iter())
            .filter(|i| i["source"] == src && i["kind"] == kind)
            .count()
    };
    println!(
        "items: dogfood rules {}, dogfood pairs {}, generated rules {}, generated pairs {}",
        count("dogfood", "rule"),
        count("dogfood", "pair"),
        count("generated", "rule"),
        count("generated", "pair")
    );
}

// ---- run -----------------------------------------------------------------

fn read_jsonl(p: &Path) -> Vec<Value> {
    (std::fs::read_to_string(p).unwrap().lines())
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
fn run(items: &Path, out: &Path, model: &str, token: &Path, workers: usize) {
    let items = read_jsonl(items);
    let done: BTreeSet<String> = if out.exists() {
        (read_jsonl(out).iter())
            .filter(|o| o["error"].is_null())
            .map(|o| o["id"].as_str().unwrap().to_owned())
            .collect()
    } else {
        BTreeSet::new()
    };
    let version = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let cli = Arc::new(ClaudeCli {
        name: "g6".into(),
        program: "claude".into(),
        model: model.into(),
        version: version.clone(),
        isolated_login: None,
        token_file: Some(token.to_path_buf()),
    });
    let queue = Arc::new(Mutex::new(
        (items.into_iter())
            .filter(|i| !done.contains(i["id"].as_str().unwrap()))
            .collect::<Vec<_>>(),
    ));
    let file = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(out)
            .unwrap(),
    ));
    println!(
        "claude {version}, model {model}: {} items to run ({} done), {workers} workers",
        queue.lock().unwrap().len(),
        done.len()
    );
    let threads: Vec<_> = (0..workers)
        .map(|_| {
            let (queue, file, cli) = (queue.clone(), file.clone(), cli.clone());
            std::thread::spawn(move || {
                loop {
                    let Some(item) = queue.lock().unwrap().pop() else {
                        return;
                    };
                    let task = task_of(&item);
                    let t = Instant::now();
                    let got = contextdb_validate::run(
                        cli.as_ref(),
                        &task,
                        Instant::now() + Duration::from_secs(180),
                    );
                    let ms = t.elapsed().as_millis() as u64;
                    let line = match got {
                        Ok(o) => json!({
                            "id": item["id"], "label": format!("{:?}", o.label).to_lowercase(),
                            "rationale": o.rationale, "evidence": o.evidence,
                            "model": o.model, "tokens_in": o.tokens.map(|t| t.0),
                            "tokens_out": o.tokens.map(|t| t.1), "ms": ms, "error": null,
                        }),
                        Err(e) => json!({"id": item["id"], "error": e.to_string(), "ms": ms}),
                    };
                    let mut f = file.lock().unwrap();
                    writeln!(f, "{line}").unwrap();
                    eprintln!("{} {}", item["id"], line["label"]);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

// ---- score ---------------------------------------------------------------

/// `boundarychange` / `newrule` as the protocol spells them.
fn norm(l: &str) -> String {
    match l {
        "boundarychange" => "boundary_change".into(),
        "newrule" => "new_rule".into(),
        other => other.into(),
    }
}
fn score(items: &Path, outcomes: &Path, refs: &[PathBuf]) {
    let items = read_jsonl(items);
    let outs: BTreeMap<String, Value> = (read_jsonl(outcomes).into_iter())
        .filter(|o| o["error"].is_null())
        .map(|o| (o["id"].as_str().unwrap().to_owned(), o))
        .collect();
    let errors = read_jsonl(outcomes)
        .iter()
        .filter(|o| !o["error"].is_null())
        .count();
    let refs: Vec<BTreeMap<String, String>> = (refs.iter())
        .map(|p| {
            (read_jsonl(p).into_iter())
                .map(|r| {
                    (
                        r["id"].as_str().unwrap().to_owned(),
                        r["label"].as_str().unwrap().to_owned(),
                    )
                })
                .collect()
        })
        .collect();
    let (mut tin, mut tout, mut ms) = (0u64, 0u64, Vec::new());
    for o in outs.values() {
        tin += o["tokens_in"].as_u64().unwrap_or(0);
        tout += o["tokens_out"].as_u64().unwrap_or(0);
        ms.push(o["ms"].as_u64().unwrap_or(0));
    }
    ms.sort();
    println!(
        "calls {} ok + {errors} errors; tokens in {tin}, out {tout}; latency p50 {} ms, p95 {} ms",
        outs.len(),
        ms.get(ms.len() / 2).copied().unwrap_or(0),
        ms.get(ms.len() * 95 / 100).copied().unwrap_or(0)
    );
    // Per (source, kind): agreement of the LLM label with the reference.
    let mut groups: BTreeMap<(String, String), (u32, u32, u32)> = BTreeMap::new();
    let mut confusion: BTreeMap<(String, String, String), u32> = BTreeMap::new();
    let mut disputed = Vec::new();
    let mut disagreements = Vec::new();
    let mut calib: Vec<(f64, bool)> = Vec::new();
    for i in &items {
        let id = i["id"].as_str().unwrap();
        let key = (
            i["source"].as_str().unwrap().to_owned(),
            i["kind"].as_str().unwrap().to_owned(),
        );
        let reference = match i["oracle"].as_str() {
            Some(l) => Some(l.to_owned()),
            None => {
                let labels: Vec<&String> = refs.iter().filter_map(|r| r.get(id)).collect();
                match labels.as_slice() {
                    [] => None,
                    [a, rest @ ..] if rest.iter().all(|b| b == a) => Some((*a).clone()),
                    _ => {
                        disputed.push(format!("{id}: {labels:?}"));
                        None
                    }
                }
            }
        };
        let g = groups.entry(key.clone()).or_default();
        let Some(reference) = reference else {
            g.2 += 1;
            continue;
        };
        if let Some(s) = i["score"].as_f64() {
            calib.push((s, reference == "conflict"));
        }
        let Some(o) = outs.get(id) else { continue };
        let got = norm(o["label"].as_str().unwrap());
        g.0 += 1;
        if got == reference {
            g.1 += 1;
        } else {
            disagreements.push(format!("{id}: reference {reference}, LLM {got}"));
        }
        *confusion
            .entry((key.1.clone(), reference.clone(), got))
            .or_default() += 1;
    }
    for ((src, kind), (n, ok, disp)) in &groups {
        println!(
            "{src} {kind}: {ok}/{n} agree ({:.0}%), {disp} without a reference (disputed or unlabelled)",
            100.0 * f64::from(*ok) / f64::from((*n).max(1))
        );
    }
    println!("confusion (kind, reference, LLM): count");
    for ((k, r, g), n) in &confusion {
        println!("  {k} {r} -> {g}: {n}");
    }
    println!("disputed references ({}):", disputed.len());
    for d in &disputed {
        println!("  {d}");
    }
    println!("LLM disagreements ({}):", disagreements.len());
    for d in &disagreements {
        println!("  {d}");
    }
    // Calibration: which min_score keeps the conflicts and drops the rest.
    calib.sort_by(|a, b| a.0.total_cmp(&b.0));
    let conflicts = calib.iter().filter(|c| c.1).count();
    println!(
        "calibration over {} labelled pairs ({conflicts} conflicts):",
        calib.len()
    );
    for t in [0.0, 0.5, 1.0, 1.5, 2.0, 3.0, 4.0, 5.0, 7.0, 10.0] {
        let kept: Vec<_> = calib.iter().filter(|c| c.0 >= t).collect();
        let kc = kept.iter().filter(|c| c.1).count();
        println!(
            "  min_score {t:>4}: keeps {:>3} pairs, {kc}/{conflicts} conflicts, {} non-conflicts",
            kept.len(),
            kept.len() - kc
        );
    }
}

// ---- o2 ------------------------------------------------------------------

/// Jobs one head move creates (enabled rules with a Sem premise on a moved
/// logical) and the calls a per-revision union vector would make (one per
/// distinct conclusion revision among them).
fn o2_counts(i: &Inputs, moved: &BTreeSet<LogicalId>) -> (u64, u64) {
    let mut jobs = 0;
    let mut conclusions = BTreeSet::new();
    for j in i.rules.iter().filter(|j| j.enabled) {
        if (j.premises.iter()).any(|p| {
            p.kind == EdgeKind::Sem && moved.contains(&i.revisions[p.premise.index()].logical)
        }) {
            jobs += 1;
            conclusions.insert(j.conclusion);
        }
    }
    (jobs, conclusions.len() as u64)
}
fn o2(seeds: u64, dirs: &[PathBuf]) {
    let report = |name: &str, per_move: &[(u64, u64)]| {
        let moves = per_move.len() as u64;
        let jobs: u64 = per_move.iter().map(|x| x.0).sum();
        let union: u64 = per_move.iter().map(|x| x.1).sum();
        let mut sorted: Vec<u64> = per_move.iter().map(|x| x.0).collect();
        sorted.sort();
        println!(
            "O2 {name}: {moves} head-moving epochs, {jobs} jobs (mean {:.2}, max {}), union-vector calls {union}, saved {} ({:.1}%)",
            jobs as f64 / moves.max(1) as f64,
            sorted.last().copied().unwrap_or(0),
            jobs - union,
            100.0 * (jobs - union) as f64 / jobs.max(1) as f64
        );
    };
    for dir in dirs {
        let c = open_copy(dir);
        let mut db = Db::<Counting, CommitStats>::new();
        let mut per_move = Vec::new();
        for e in 1..=c.store.epoch().0 {
            let row = c.store.read_row(Epoch(e)).unwrap();
            let batch = row.batch().unwrap();
            let result = row.result().unwrap();
            if batch.branch != MAIN {
                continue;
            }
            let _ = db.commit(&batch);
            let moved: BTreeSet<LogicalId> =
                (result.net_delta.head_moves.iter()).map(|m| m.0).collect();
            if !moved.is_empty() {
                per_move.push(o2_counts(&db.inputs(), &moved));
            }
        }
        report(&c.name, &per_move);
    }
    let mut per_move = Vec::new();
    for seed in 0..seeds {
        let trace = contextdb_testkit::generate_p7(seed);
        let mut db = Db::<Counting, CommitStats>::new();
        for b in &trace.batches {
            if let Ok(r) = db.commit(b) {
                let moved: BTreeSet<LogicalId> =
                    (r.net_delta.head_moves.iter()).map(|m| m.0).collect();
                if !moved.is_empty() {
                    per_move.push(o2_counts(&db.inputs(), &moved));
                }
            }
        }
    }
    report(&format!("generator ({seeds} seeds)"), &per_move);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths = |from: usize| -> Vec<PathBuf> { args[from..].iter().map(PathBuf::from).collect() };
    match args.first().map(String::as_str) {
        Some("collect") => collect(Path::new(&args[1]), &paths(2)),
        Some("run") => run(
            Path::new(&args[1]),
            Path::new(&args[2]),
            &args[3],
            Path::new(&args[4]),
            args.get(5).and_then(|s| s.parse().ok()).unwrap_or(4),
        ),
        Some("score") => score(Path::new(&args[1]), Path::new(&args[2]), &paths(3)),
        Some("o2") => o2(args[1].parse().unwrap(), &paths(2)),
        _ => eprintln!("p7_g6 collect|run|score|o2 ..."),
    }
}
