//! P5b G6 replay (ADR 0012 §9): symbol-level against file-level anchors on
//! the P5a replay's repositories and commits, scored on agent labels.
//!
//!   symbol_replay prepare <clone> <p5a.tsv> <out.tsv> [--ranks A-B] [--live-at-parent]
//!   symbol_replay sample <sample.tsv> <population.tsv>... [--id-prefix P]
//!   symbol_replay batches <sample.tsv> <clones dir> <out dir> <batches>
//!   symbol_replay score <sample.tsv> <labels.tsv> [--predictions <p.tsv>] [--moves-invalidate]
//!   symbol_replay predict <sample.tsv> <clones dir> <out.tsv>
//!
//! `prepare` reads the P5a replay's header (repository, selection commit,
//! last commit, anchor files) and replays the same 1,000 first-parent commits
//! through the real daemon and symbol observer in a `--shared --no-checkout`
//! clone (the source clone is never modified). **Symbol selection** (fixed
//! before any replay): in each P5a anchor file of a supported language, the
//! symbols without a parse error at the selection commit, sorted by
//! `git hash-object("<path>#<symbol>")`, the first 5. **Pairs**: every
//! (symbol, commit) where the host file or a file in the host's directory
//! changed. The r0.7 retest takes ranks 6-10 instead of 1-5 and keeps only
//! pairs whose symbol is live (not Broken) at the parent (`--ranks 6-10
//! --live-at-parent`). File level fires when the host blob changed; symbol level fires
//! when the observer gives the anchor a new head or breaks it. An automatic
//! check (not a label) recomputes the symbol event from the commit's blobs
//! with `parse` and counts disagreements.
//!
//! `sample` stratifies by repository × file fired × symbol fired and keeps
//! the first 35 per stratum by `git hash-object("<commit>:<path>#<symbol>")`;
//! it records every stratum's population. `batches` writes labelling batches
//! that carry no predictor output. `score` estimates, per predictor, the
//! false invalidation share (fired pairs that do not invalidate, FP / fired)
//! and the missed invalidation share (invalidating pairs not fired,
//! FN / positives), each stratum weighted by population / labelled, with
//! stratified-bootstrap 95 % intervals, and applies the gate.
use contextdb_anchors::{
    Language, ObjectFormat, Repo,
    capture::{OBSERVER_TIMEOUT, blob_oid, git},
    observer::state_by_key,
    symbol::encode_symbol,
};
use contextdb_daemon::server::{Daemon, Options};
use contextdb_model::*;
use contextdb_proto::Client;
use contextdb_symbols::{
    ParsedFile,
    observer::{Added, Mode, reconcile},
    parse,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

const PER_FILE: usize = 5;
const PER_STRATUM: usize = 35;
const HEADER: &str = "repo_url\tcommit\tparent\tpath\tsymbol\tfile_fired\tsymbol_fired\tsymbol_event\thost_added\thost_removed\tsubject";

fn run(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let (ok, out) = git(
        root,
        args,
        &[],
        256 << 20,
        Instant::now() + Duration::from_secs(300),
    )
    .map_err(|e| e.to_string())?;
    ok.then_some(out)
        .ok_or_else(|| format!("git {} failed", args.join(" ")))
}
fn text(root: &Path, args: &[&str]) -> Result<String, String> {
    String::from_utf8(run(root, args)?)
        .map(|s| s.trim_end().to_owned())
        .map_err(|e| e.to_string())
}
/// `git hash-object` of a string: the reproducible ordering key.
fn order(s: &str) -> Vec<u8> {
    blob_oid(ObjectFormat::Sha1, s.as_bytes()).expect("sha1")
}
fn tsv(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}
fn dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}
/// A header value `# ... key=value ...` of a P5a replay file.
fn header<'a>(lines: &'a [&str], key: &str) -> Option<&'a str> {
    lines
        .iter()
        .filter(|l| l.starts_with('#'))
        .flat_map(|l| l.split_whitespace())
        .find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
}

/// A host file at `commit`, parsed: `None` when it does not exist there.
fn host_at(root: &Path, commit: &str, path: &str) -> Result<Option<ParsedFile>, String> {
    let language = Language::of_path(path).ok_or("unsupported")?;
    let Ok(bytes) = run(root, &["cat-file", "blob", &format!("{commit}:{path}")]) else {
        return Ok(None);
    };
    parse(language, &bytes, Instant::now() + Duration::from_secs(60))
        .map(Some)
        .map_err(|e| e.0)
}
/// The symbol's hash: `None` when the host is missing, the symbol is absent
/// or it has a parse error (the observer's Broken).
fn hash_of(file: &Option<ParsedFile>, symbol: &str) -> Option<[u8; 32]> {
    file.as_ref()?
        .symbols
        .get(symbol)
        .filter(|s| !s.error)
        .map(|s| s.hash)
}

/// Which ranked symbols per file, and whether pairs need a live symbol at
/// the parent.
struct Selection {
    ranks: std::ops::RangeInclusive<usize>,
    live_at_parent: bool,
}

fn prepare(src: &Path, p5a: &Path, out: &Path, sel: &Selection) -> Result<(), String> {
    let src = src.canonicalize().map_err(|e| e.to_string())?;
    let p5a_text = std::fs::read_to_string(p5a).map_err(|e| e.to_string())?;
    let lines: Vec<&str> = p5a_text.lines().collect();
    let url = header(&lines, "repo_url").ok_or("no repo_url")?.to_owned();
    let last = header(&lines, "last").ok_or("no last")?.to_owned();
    let base = lines
        .iter()
        .find_map(|l| l.strip_prefix("# selection: ")?.split(" at ").nth(1))
        .and_then(|s| s.split_whitespace().next())
        .ok_or("no selection commit")?
        .to_owned();
    let anchors: Vec<String> = lines
        .iter()
        .find_map(|l| l.strip_prefix("# anchors: "))
        .ok_or("no anchors")?
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let mut list = vec![base.clone()];
    list.extend(
        text(
            &src,
            &[
                "rev-list",
                "--first-parent",
                "--reverse",
                &format!("{base}..{last}"),
            ],
        )?
        .lines()
        .map(str::to_owned),
    );
    if list.len() != 1001 || Some(list[1].as_str()) != header(&lines, "first") {
        return Err(format!(
            "{}: the P5a range does not reproduce",
            src.display()
        ));
    }

    // Symbol selection, fixed before any replay.
    let hosts: Vec<&String> = anchors
        .iter()
        .filter(|p| Language::of_path(p).is_some())
        .collect();
    let mut selected: Vec<(String, String)> = Vec::new();
    for path in &hosts {
        let file = host_at(&src, &base, path)?.ok_or("anchor file missing at selection")?;
        let mut symbols: Vec<(Vec<u8>, String)> = file
            .symbols
            .iter()
            .filter(|(_, s)| !s.error)
            .map(|(name, _)| (order(&format!("{path}#{name}")), name.clone()))
            .collect();
        symbols.sort();
        selected.extend(
            symbols
                .into_iter()
                .skip(sel.ranks.start() - 1)
                .take(sel.ranks.end() + 1 - sel.ranks.start())
                .map(|(_, s)| ((*path).clone(), s)),
        );
    }

    let tmp = std::env::temp_dir().join(format!("symbol-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("db")).map_err(|e| e.to_string())?;
    run(
        &tmp,
        &[
            "clone",
            "-q",
            "--shared",
            "--no-checkout",
            src.to_str().ok_or("non-UTF8 path")?,
            "work",
        ],
    )?;
    let work = tmp.join("work");
    run(&work, &["checkout", "-q", "--detach", &base])?;
    let repo = Repo::open(&work, Instant::now() + OBSERVER_TIMEOUT).map_err(|e| e.to_string())?;
    let data = tmp.join("db");
    std::fs::write(
        data.join("anchors.routes"),
        vec![(repo.scope(), repo.root.to_str().unwrap().to_owned())].canonical_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let daemon = Daemon::start(&data, &Options::default()).map_err(|e| format!("{e:?}"))?;
    let mut client =
        Client::connect(daemon.socket(Channel::Observer)).map_err(|e| e.to_string())?;
    let deadline = || Instant::now() + Duration::from_secs(60);
    let added = Added {
        files: BTreeSet::new(),
        symbols: selected.iter().cloned().collect(),
    };
    reconcile(&mut client, &repo, &added, Mode::Scan, deadline()).map_err(|e| e.0)?;
    let key = |path: &str, symbol: &str| {
        format!(
            "{}/{path}#{}",
            repo.scope(),
            encode_symbol(symbol).expect("extracted names encode")
        )
    };
    let state = |client: &mut Client| -> Result<BTreeMap<String, (RevisionId, bool)>, String> {
        let (_, s) = state_by_key(client, &repo.scope()).map_err(|e| e.0)?;
        Ok(s.into_iter()
            .map(|(k, (_, a))| (k, (a.head, a.base)))
            .collect())
    };
    let mut before = state(&mut client)?;
    // The automatic check: each host parsed once per commit (reused while
    // its blob is unchanged), and each symbol's last good hash (the
    // observer's head; every selected symbol starts without an error).
    let mut prev: BTreeMap<String, Option<ParsedFile>> = BTreeMap::new();
    for host in &hosts {
        prev.insert((*host).clone(), host_at(&src, &base, host)?);
    }
    let mut good: BTreeMap<(String, String), [u8; 32]> = BTreeMap::new();
    for (path, symbol) in &selected {
        let h = hash_of(&prev[path], symbol).expect("selected without errors");
        good.insert((path.clone(), symbol.clone()), h);
    }
    let mut rows = Vec::new();
    let mut disagreements = Vec::new();
    let (mut host_commits, mut host_errors) = (0usize, 0usize);
    for pair in list.windows(2) {
        let (parent, commit) = (pair[0].as_str(), pair[1].as_str());
        run(&work, &["checkout", "-q", "--detach", commit])?;
        reconcile(
            &mut client,
            &repo,
            &Added::default(),
            Mode::Scan,
            deadline(),
        )
        .map_err(|e| format!("{commit}: {}", e.0))?;
        let after = state(&mut client)?;
        let numstat = text(
            &work,
            &["diff", "--numstat", "--no-renames", parent, commit],
        )?;
        let mut stat: BTreeMap<String, (String, String)> = BTreeMap::new();
        for line in numstat.lines() {
            let f: Vec<&str> = line.splitn(3, '\t').collect();
            if let [added, removed, path] = f[..] {
                stat.insert(path.to_owned(), (added.to_owned(), removed.to_owned()));
            }
        }
        let subject = tsv(&text(&work, &["log", "-1", "--format=%s", commit])?);
        let mut cur: BTreeMap<String, Option<ParsedFile>> = BTreeMap::new();
        for host in &hosts {
            let file = if stat.contains_key(host.as_str()) {
                host_at(&src, commit, host)?
            } else {
                prev[host.as_str()].clone()
            };
            host_commits += 1;
            host_errors += usize::from(file.as_ref().is_some_and(|f| f.has_error));
            cur.insert((*host).clone(), file);
        }
        for (path, symbol) in &selected {
            let k = key(path, symbol);
            let (b, a) = (before.get(&k), after.get(&k));
            let event = match (b, a) {
                (Some(b), Some(a)) if b == a => "none",
                (Some((_, true)), Some((_, false))) => "broken",
                (Some((h, _)), Some((g, _))) if h != g => "changed",
                (Some((_, false)), Some((_, true))) => "restored",
                _ => "other",
            };
            let symbol_fired = matches!(event, "broken" | "changed");
            let last = good
                .get_mut(&(path.clone(), symbol.clone()))
                .expect("selected");
            let check = match hash_of(&cur[path], symbol) {
                Some(h) => {
                    let fired = h != *last;
                    *last = h;
                    fired
                }
                None => hash_of(&prev[path], symbol).is_some(),
            };
            if check != symbol_fired {
                disagreements.push(format!("{commit} {path}#{symbol} {event}"));
            }
            let file_fired = stat.contains_key(path.as_str());
            let near = stat.keys().any(|p| p != path && dir(p) == dir(path));
            let live = b.is_some_and(|(_, base)| *base);
            if (file_fired || near) && (live || !sel.live_at_parent) {
                let (added, removed) = stat.get(path.as_str()).cloned().unwrap_or_default();
                rows.push(format!(
                    "{}\t{commit}\t{parent}\t{}\t{}\t{}\t{}\t{event}\t{added}\t{removed}\t{subject}",
                    tsv(&url),
                    tsv(path),
                    tsv(symbol),
                    if file_fired { "y" } else { "n" },
                    if symbol_fired { "y" } else { "n" },
                ));
            }
        }
        before = after;
        prev = cur;
    }
    let mut f = std::fs::File::create(out).map_err(|e| e.to_string())?;
    let mut w = |s: String| writeln!(f, "{s}").map_err(|e| e.to_string());
    w("# P5b G6 replay: (symbol, commit) pairs for agent labelling".into())?;
    w(format!("# repo_url={url}"))?;
    w(format!(
        "# replayed=1000 first={} last={last} selection_commit={base}",
        list[1]
    ))?;
    w(format!(
        "# selection: in the P5a anchor files of a supported language, symbols without a parse error at {base}, ranks {}-{} per file by git hash-object(<path>#<symbol>){}",
        sel.ranks.start(),
        sel.ranks.end(),
        if sel.live_at_parent {
            "; pairs only where the symbol is live at the parent"
        } else {
            ""
        }
    ))?;
    w(format!(
        "# normaliser: {}",
        contextdb_symbols::normaliser_version(Language::Rust)
    ))?;
    w(format!(
        "# hosts={} symbols={} pairs={} automatic_check_disagreements={} {}",
        hosts.len(),
        selected.len(),
        rows.len(),
        disagreements.len(),
        disagreements.join("; ")
    ))?;
    w(format!(
        "# host_parse_errors={host_errors}/{host_commits} (host files × replayed commits with a parse error anywhere)"
    ))?;
    w(format!(
        "# symbols: {}",
        selected
            .iter()
            .map(|(p, s)| format!("{p}#{s}"))
            .collect::<Vec<_>>()
            .join(" ")
    ))?;
    w(HEADER.into())?;
    for r in &rows {
        w(r.clone())?;
    }
    println!(
        "{url}: {} hosts, {} symbols, {} pairs, {} disagreements, host parse errors {host_errors}/{host_commits} -> {}",
        hosts.len(),
        selected.len(),
        rows.len(),
        disagreements.len(),
        out.display()
    );
    drop(daemon);
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

/// A stratum: repository, file fired, symbol fired.
type Key = (String, String, String);

/// One population row, by column name.
struct Row(BTreeMap<String, String>);
impl Row {
    fn get(&self, k: &str) -> &str {
        self.0.get(k).map_or("", String::as_str)
    }
    fn repo(&self) -> String {
        let url = self.get("repo_url");
        url.rsplit('/')
            .next()
            .unwrap_or(url)
            .trim_end_matches(".git")
            .to_owned()
    }
    fn stratum(&self) -> Key {
        (
            self.repo(),
            self.get("file_fired").to_owned(),
            self.get("symbol_fired").to_owned(),
        )
    }
}
fn read_rows(path: &Path) -> Result<(Vec<String>, Vec<Row>), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let comments: Vec<String> = text
        .lines()
        .filter(|l| l.starts_with('#'))
        .map(str::to_owned)
        .collect();
    let mut lines = text.lines().filter(|l| !l.starts_with('#'));
    let head: Vec<&str> = lines.next().ok_or("empty")?.split('\t').collect();
    let rows = lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            Row(head
                .iter()
                .zip(l.split('\t'))
                .map(|(k, v)| ((*k).to_owned(), v.to_owned()))
                .collect())
        })
        .collect();
    Ok((comments, rows))
}

fn sample(out: &Path, files: &[String], prefix: &str) -> Result<(), String> {
    let mut strata: BTreeMap<Key, Vec<(Vec<u8>, Row)>> = BTreeMap::new();
    let mut sources = Vec::new();
    for file in files {
        let (_, rows) = read_rows(Path::new(file))?;
        sources.push(format!("{file} ({} pairs)", rows.len()));
        for row in rows {
            let k = order(&format!(
                "{}:{}#{}",
                row.get("commit"),
                row.get("path"),
                row.get("symbol")
            ));
            strata.entry(row.stratum()).or_default().push((k, row));
        }
    }
    let mut picked: Vec<(Vec<u8>, String)> = Vec::new();
    let mut lines = Vec::new();
    for ((repo, file, symbol), mut rows) in strata {
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let population = rows.len();
        let kept: Vec<_> = rows.into_iter().take(PER_STRATUM).collect();
        lines.push(format!(
            "# stratum repo={repo} file_fired={file} symbol_fired={symbol} population={population} sampled={}",
            kept.len()
        ));
        for (k, row) in kept {
            let cols: Vec<&str> = HEADER.split('\t').map(|c| row.get(c)).collect();
            picked.push((order(&format!("p5b-g6:{}", hex(&k))), cols.join("\t")));
        }
    }
    // Labelling order: by a hash of the ordering key, so batches mix strata.
    picked.sort();
    let mut f = std::fs::File::create(out).map_err(|e| e.to_string())?;
    let mut w = |s: &str| writeln!(f, "{s}").map_err(|e| e.to_string());
    w("# P5b G6 sample, fixed before labelling (ADR 0012 §9)")?;
    w(&format!(
        "# rule: strata repo × file_fired × symbol_fired; first {PER_STRATUM} per stratum by git hash-object(<commit>:<path>#<symbol>)"
    ))?;
    w(&format!("# sources: {}", sources.join("; ")))?;
    for l in &lines {
        w(l)?;
    }
    w(&format!("id\t{HEADER}"))?;
    for (i, (_, row)) in picked.iter().enumerate() {
        w(&format!("{prefix}{:03}\t{row}", i + 1))?;
    }
    println!(
        "{} sampled pairs in {} strata -> {}",
        picked.len(),
        lines.len(),
        out.display()
    );
    Ok(())
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn batches(sample: &Path, clones: &Path, out: &Path, count: usize) -> Result<(), String> {
    let (_, rows) = read_rows(sample)?;
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let per = rows.len().div_ceil(count);
    for (b, chunk) in rows.chunks(per).enumerate() {
        let mut s = format!(
            "# P5b G6 labelling batch {:02}\n\nFor each pair, answer: **does this commit invalidate a decision resting on this symbol?** \
A decision rests on the symbol as it was at the parent commit: what it declares, does or promises (signature, behaviour, contract, documented meaning). \
Answer `y` if, after this commit, such a decision must be re-validated: the symbol itself changed in a way that can change what it declares or does, it was removed, \
or a change outside it (in its file or directory) changes its behaviour or contract (for example a constant, type or helper it uses directly changed meaning). \
Answer `n` if the commit cannot change what the symbol declares or does (unrelated code, other symbols, formatting, comments that are not its documentation, tests elsewhere); \
a pure move or rename with identical code is `n` (finding it again is a rebind, not a re-validation). \
Use `unsure` only when the evidence cannot decide.\n\n\
Inspect with Git in the clone (read-only): `git -C <clone> show <commit> -- <dir>` for the change, and `git -C <clone> show <parent>:<path>` for the symbol at the parent.\n\n\
| id | clone | commit | parent | path | symbol | subject |\n|---|---|---|---|---|---|---|\n",
            b + 1
        );
        for row in chunk {
            let clone = clones.join(row.repo());
            s += &format!(
                "| {} | {} | {} | {} | {} | `{}` | {} |\n",
                row.get("id"),
                clone.display(),
                row.get("commit"),
                row.get("parent"),
                row.get("path"),
                row.get("symbol"),
                row.get("subject").replace('|', "/")
            );
        }
        std::fs::write(out.join(format!("batch-{:02}.md", b + 1)), s).map_err(|e| e.to_string())?;
    }
    println!(
        "{} pairs in {} batches -> {}",
        rows.len(),
        rows.len().div_ceil(per),
        out.display()
    );
    Ok(())
}

/// Deterministic generator for the bootstrap.
struct SplitMix(u64);
impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// One stratum's labelled pairs: (invalidates, symbol level fired). File
/// level is constant in a stratum; symbol level is too unless per-pair
/// predictions replace it (a predictor scored on a sample stratified by
/// another).
struct Stratum {
    file_fired: bool,
    population: f64,
    labels: Vec<(bool, bool)>,
}
/// (symbol FI, file FI, symbol MI, file MI, symbol MI among host-changed positives)
fn estimate(strata: &[(&Stratum, Vec<(bool, bool)>)]) -> [f64; 5] {
    let (mut fired, mut fp) = ([0.; 2], [0.; 2]);
    let (mut pos, mut missed) = (0., [0.; 2]);
    let (mut host_pos, mut host_missed) = (0., 0.);
    for (s, labels) in strata {
        if labels.is_empty() {
            continue;
        }
        // Each labelled pair stands for population / labelled pairs.
        let w = s.population / labels.len() as f64;
        for &(invalidates, symbol) in labels {
            if invalidates {
                pos += w;
            }
            for (i, fires) in [symbol, s.file_fired].into_iter().enumerate() {
                if fires {
                    fired[i] += w;
                    if !invalidates {
                        fp[i] += w;
                    }
                } else if invalidates {
                    missed[i] += w;
                }
            }
            if s.file_fired && invalidates {
                host_pos += w;
                if !symbol {
                    host_missed += w;
                }
            }
        }
    }
    let div = |a: f64, b: f64| if b > 0. { a / b } else { 0. };
    [
        div(fp[0], fired[0]),
        div(fp[1], fired[1]),
        div(missed[0], pos),
        div(missed[1], pos),
        div(host_missed, host_pos),
    ]
}

/// `moves_invalidate` (owner convention, SPEC v1.6.7): a pure move or rename
/// invalidates, because the anchor must be rebound. Every Broken pair
/// labelled `n` in both G6 samples is a pure move or rename (checked against
/// the rationales), so those labels count as `y`.
fn score(
    sample: &Path,
    labels: &Path,
    predictions: Option<&Path>,
    moves_invalidate: bool,
) -> Result<bool, String> {
    let (comments, rows) = read_rows(sample)?;
    let predicted: Option<BTreeMap<String, bool>> = match predictions {
        Some(p) => Some(
            read_rows(p)?
                .1
                .iter()
                .map(|r| (r.get("id").to_owned(), r.get("symbol_fired") == "y"))
                .collect(),
        ),
        None => None,
    };
    let (_, label_rows) = read_rows(labels)?;
    let label: BTreeMap<&str, &str> = label_rows
        .iter()
        .map(|r| (r.get("id"), r.get("invalidates")))
        .collect();
    let mut population: BTreeMap<Key, f64> = BTreeMap::new();
    for c in &comments {
        let Some(rest) = c.strip_prefix("# stratum ") else {
            continue;
        };
        let v: BTreeMap<&str, &str> = rest
            .split(' ')
            .filter_map(|kv| kv.split_once('='))
            .collect();
        population.insert(
            (
                v["repo"].to_owned(),
                v["file_fired"].to_owned(),
                v["symbol_fired"].to_owned(),
            ),
            v["population"].parse().map_err(|_| "population")?,
        );
    }
    let mut strata: BTreeMap<Key, Stratum> = BTreeMap::new();
    let (mut missing, mut unsure) = (Vec::new(), 0);
    for r in &rows {
        let k = r.stratum();
        let s = strata.entry(k.clone()).or_insert(Stratum {
            file_fired: k.1 == "y",
            population: population.get(&k).copied().unwrap_or(0.),
            labels: Vec::new(),
        });
        let symbol = match &predicted {
            Some(p) => *p
                .get(r.get("id"))
                .ok_or_else(|| format!("no prediction for {}", r.get("id")))?,
            None => k.2 == "y",
        };
        let moved = moves_invalidate && r.get("symbol_event") == "broken";
        match label.get(r.get("id")).copied() {
            Some("y") => s.labels.push((true, symbol)),
            Some("n") if moved => s.labels.push((true, symbol)),
            Some("n") => s.labels.push((false, symbol)),
            Some("unsure") => unsure += 1,
            _ => missing.push(r.get("id").to_owned()),
        }
    }
    if !missing.is_empty() {
        return Err(format!("unlabelled pairs: {}", missing.join(" ")));
    }
    println!(
        "# P5b G6 score; sample {}; labels {}{}",
        sample.display(),
        labels.display(),
        if moves_invalidate {
            "; pure moves invalidate (SPEC v1.6.7)"
        } else {
            ""
        }
    );
    println!("repo\tfile_fired\tsymbol_fired\tpopulation\tlabelled\tinvalidating");
    for ((repo, f, s), st) in &strata {
        println!(
            "{repo}\t{f}\t{s}\t{}\t{}\t{}",
            st.population,
            st.labels.len(),
            st.labels.iter().filter(|l| l.0).count()
        );
    }
    let report = |name: &str, keep: &dyn Fn(&str) -> bool| -> [f64; 5] {
        let chosen: Vec<(&Stratum, Vec<(bool, bool)>)> = strata
            .iter()
            .filter(|(k, _)| keep(&k.0))
            .map(|(_, s)| (s, s.labels.clone()))
            .collect();
        let point = estimate(&chosen);
        let mut rng = SplitMix(0x5eed_0006);
        let mut draws: Vec<[f64; 6]> = Vec::with_capacity(10_000);
        for _ in 0..10_000 {
            let resampled: Vec<(&Stratum, Vec<(bool, bool)>)> = chosen
                .iter()
                .map(|(s, l)| {
                    let n = l.len();
                    let r = (0..n)
                        .map(|_| l[(rng.next() % n as u64) as usize])
                        .collect();
                    (*s, r)
                })
                .collect();
            let e = estimate(&resampled);
            draws.push([e[0], e[1], e[2], e[3], e[4], e[2] - e[3]]);
        }
        let ci = |i: usize| {
            let mut v: Vec<f64> = draws.iter().map(|d| d[i]).collect();
            v.sort_by(f64::total_cmp);
            (v[249] * 100., v[9_749] * 100.)
        };
        let names = [
            "symbol_false_invalidation",
            "file_false_invalidation",
            "symbol_missed_invalidation",
            "file_missed_invalidation",
            "symbol_missed_given_host_changed",
        ];
        for (i, n) in names.iter().enumerate() {
            let (lo, hi) = ci(i);
            println!(
                "{name}\t{n}={:.1}%\t95%=[{lo:.1}, {hi:.1}]",
                point[i] * 100.
            );
        }
        let (lo, hi) = ci(5);
        println!(
            "{name}\tmissed_difference_pp={:.1}\t95%=[{lo:.1}, {hi:.1}]",
            (point[2] - point[3]) * 100.
        );
        point
    };
    let all = report("all", &|_| true);
    let repos: BTreeSet<String> = strata.keys().map(|k| k.0.clone()).collect();
    for repo in &repos {
        report(repo, &|r| r == repo);
    }
    println!("unsure={unsure} (excluded from rates)");
    // ADR 0012 §9 r0.7 / SPEC v1.6.6 (owner decision 2026-10-23): weighted
    // point estimates decide; FI ≤ ½ × file level (v1.6.5 had FI ≤ 10 %).
    let fi = all[0] <= 0.5 * all[1];
    let mi = all[2] <= all[3] + 0.02;
    println!(
        "threshold_fi={fi} (symbol {:.1}% <= {:.1}%) threshold_missed={mi}",
        all[0] * 100.,
        all[1] * 50.
    );
    let pass = fi && mi;
    println!("p5b_g6_pass={pass}");
    Ok(pass)
}

/// Symbol level v2 (ADR 0012 r0.7) on a sample's pairs, from the blobs:
/// v1 fired (a new head or a break), or the symbol is present at both
/// commits and its same-file context hash changed.
fn predict(sample: &Path, clones: &Path, out: &Path) -> Result<(), String> {
    let (_, rows) = read_rows(sample)?;
    let mut f = std::fs::File::create(out).map_err(|e| e.to_string())?;
    writeln!(f, "id\tsymbol_fired\tv1\tcontext_changed\tspan_fired").map_err(|e| e.to_string())?;
    let (mut fired, mut added) = (0, 0);
    for r in &rows {
        let clone = clones.join(r.repo());
        let symbol = r.get("symbol");
        let at = |c: &str| -> Result<Option<contextdb_symbols::Symbol>, String> {
            Ok(host_at(&clone, c, r.get("path"))?
                .and_then(|file| file.symbols.get(symbol).cloned())
                .filter(|s| !s.error))
        };
        let (before, after) = (at(r.get("parent"))?, at(r.get("commit"))?);
        let v1 = r.get("symbol_fired") == "y";
        let context = matches!((&before, &after), (Some(b), Some(a)) if b.context != a.context);
        // Span only (walk 2's rule): the span changed, or the symbol broke.
        let span = match (&before, &after) {
            (Some(b), Some(a)) => b.span != a.span,
            (Some(_), None) => true,
            _ => r.get("symbol_event") == "broken",
        };
        let v2 = v1 || context;
        fired += usize::from(v2);
        added += usize::from(v2 && !v1);
        writeln!(
            f,
            "{}\t{}\t{}\t{}\t{}",
            r.get("id"),
            if v2 { "y" } else { "n" },
            if v1 { "y" } else { "n" },
            if context { "y" } else { "n" },
            if span { "y" } else { "n" }
        )
        .map_err(|e| e.to_string())?;
    }
    println!(
        "{} pairs: v2 fires on {fired}, {added} more than v1 -> {}",
        rows.len(),
        out.display()
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: symbol_replay prepare <clone> <p5a.tsv> <out.tsv> | sample <sample.tsv> <population.tsv>... | batches <sample.tsv> <clones dir> <out dir> <n> | score <sample.tsv> <labels.tsv>";
    let result = match args.first().map(String::as_str) {
        Some("prepare") if args.len() >= 4 => {
            let mut sel = Selection {
                ranks: 1..=PER_FILE,
                live_at_parent: false,
            };
            let mut rest = args[4..].iter();
            let mut ok = true;
            while let Some(a) = rest.next() {
                match a.as_str() {
                    "--live-at-parent" => sel.live_at_parent = true,
                    "--ranks" => {
                        let r = rest.next().and_then(|r| {
                            let (a, b) = r.split_once('-')?;
                            Some(a.parse::<usize>().ok()?..=b.parse::<usize>().ok()?)
                        });
                        match r {
                            Some(r) if *r.start() >= 1 && r.start() <= r.end() => sel.ranks = r,
                            _ => ok = false,
                        }
                    }
                    _ => ok = false,
                }
            }
            if ok {
                prepare(
                    Path::new(&args[1]),
                    Path::new(&args[2]),
                    Path::new(&args[3]),
                    &sel,
                )
                .map(|_| true)
            } else {
                Err(usage.to_owned())
            }
        }
        Some("sample") if args.len() >= 3 => {
            let (files, prefix) = match args.iter().position(|a| a == "--id-prefix") {
                Some(i) if i + 2 == args.len() && i >= 3 => (&args[2..i], args[i + 1].as_str()),
                Some(_) => return eprintln!("{usage}"),
                None => (&args[2..], "P5B-G6-"),
            };
            sample(Path::new(&args[1]), files, prefix).map(|_| true)
        }
        Some("batches") if args.len() == 5 => args[4]
            .parse()
            .map_err(|_| usage.to_owned())
            .and_then(|n| {
                batches(
                    Path::new(&args[1]),
                    Path::new(&args[2]),
                    Path::new(&args[3]),
                    n,
                )
            })
            .map(|_| true),
        Some("score") if args.len() >= 3 => {
            let mut predictions = None;
            let mut moves = false;
            let mut rest = args[3..].iter();
            let mut ok = true;
            while let Some(a) = rest.next() {
                match a.as_str() {
                    "--predictions" => match rest.next() {
                        Some(p) => predictions = Some(Path::new(p)),
                        None => ok = false,
                    },
                    "--moves-invalidate" => moves = true,
                    _ => ok = false,
                }
            }
            if ok {
                score(Path::new(&args[1]), Path::new(&args[2]), predictions, moves)
            } else {
                Err(usage.to_owned())
            }
        }
        Some("predict") if args.len() == 4 => predict(
            Path::new(&args[1]),
            Path::new(&args[2]),
            Path::new(&args[3]),
        )
        .map(|_| true),
        _ => Err(usage.to_owned()),
    };
    match result {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("symbol_replay: {e}");
            std::process::exit(2);
        }
    }
}
