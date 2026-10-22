//! P5a G6 commit replay (ADR 0011 §11). Prepares anchor/change pairs from real
//! repository history, then scores agent-labelled evidence with explicit provenance.
//! Scoring never writes labels; historical human reviews remain a separate input.
//!
//!   anchor_replay prepare <clone> <out.tsv> [--commits N] [--anchors K]
//!   anchor_replay score <labelled.tsv>...
//!   anchor_replay label <repos dir> <labeler> <file.tsv>...
//!   anchor_replay score --assisted <proposals.tsv> <file.tsv>...
//!
//! `prepare` makes a `--shared --no-checkout` clone in a temporary directory
//! (the source clone is never modified), starts a development daemon there
//! (never `.contextdb`), and replays the last N first-parent commits of HEAD
//! with the real capture/classify/commit path. Selection rule, fixed before
//! any replay: the K supported regular files at the first replayed commit with
//! the smallest `git hash-object` of their path string. Every observer event
//! on an anchor becomes a `fired` pair; candidate misses are commits that
//! changed another file in an anchor's directory without firing, keeping the
//! same number as fired pairs, smallest `git hash-object` of `commit:path`.
use contextdb_anchors::{
    ObjectFormat, Repo,
    capture::{OBSERVER_TIMEOUT, blob_oid, git},
    observer, validate_path,
};
use contextdb_daemon::server::{Daemon, Options};
use contextdb_model::*;
use contextdb_proto::Client;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
    time::Instant,
};

const HEADER: &str =
    "repo_url\tcommit\tparent\tpath\tobserver\tadded\tremoved\tsubject\tinvalidates\tlabeler\tnote";

fn run(root: &Path, args: &[&str], cap: usize) -> Result<Vec<u8>, String> {
    let (ok, out) = git(
        root,
        args,
        &[],
        cap,
        Instant::now() + std::time::Duration::from_secs(120),
    )
    .map_err(|e| e.to_string())?;
    if ok {
        Ok(out)
    } else {
        Err(format!("git {} failed", args.join(" ")))
    }
}
fn text(root: &Path, args: &[&str]) -> Result<String, String> {
    String::from_utf8(run(root, args, 64 << 20)?)
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

fn prepare(src: &Path, out: &Path, commits: usize, anchors: usize) -> Result<(), String> {
    let src = src.canonicalize().map_err(|e| e.to_string())?;
    let url = text(&src, &["remote", "get-url", "origin"]).unwrap_or_else(|_| "local".into());
    let list = text(
        &src,
        &[
            "rev-list",
            "--first-parent",
            "--reverse",
            "-n",
            &(commits + 1).to_string(),
            "HEAD",
        ],
    )?;
    let list: Vec<&str> = list.lines().collect();
    if list.len() < commits + 1 {
        return Err(format!(
            "{} has {} first-parent commits; {commits} replayed commits need {}",
            src.display(),
            list.len(),
            commits + 1
        ));
    }
    let tmp = std::env::temp_dir().join(format!("anchor-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let work = tmp.join("work");
    let data = tmp.join("db");
    std::fs::create_dir_all(&data).map_err(|e| e.to_string())?;
    run(
        &tmp,
        &[
            "clone",
            "-q",
            "--shared",
            "--no-checkout",
            src.to_str().ok_or("non-UTF8 clone path")?,
            "work",
        ],
        1 << 20,
    )?;
    run(&work, &["checkout", "-q", "--detach", list[0]], 1 << 20)?;
    let repo = Repo::open(&work, Instant::now() + OBSERVER_TIMEOUT).map_err(|e| e.to_string())?;
    // Selection rule, fixed before any replay (see the module comment).
    let tree = run(&work, &["ls-tree", "-r", "-z", list[0]], 64 << 20)?;
    let mut candidates: Vec<(Vec<u8>, String)> = tree
        .split(|b| *b == 0)
        .filter_map(|entry| {
            let (meta, path) = std::str::from_utf8(entry).ok()?.split_once('\t')?;
            let mut fields = meta.split(' ');
            let mode = fields.next()?;
            (matches!(mode, "100644" | "100755")
                && validate_path(&repo.id, path).is_ok()
                && std::fs::symlink_metadata(work.join(path)).is_ok_and(|m| m.is_file()))
            .then(|| (order(path), path.to_owned()))
        })
        .collect();
    candidates.sort();
    let selected: BTreeSet<String> = candidates
        .into_iter()
        .take(anchors)
        .map(|(_, p)| p)
        .collect();
    if selected.len() < anchors {
        return Err(format!("only {} supported files", selected.len()));
    }
    std::fs::write(
        data.join("anchors.routes"),
        vec![(repo.scope(), repo.root.to_str().unwrap().to_owned())].canonical_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let daemon = Daemon::start(&data, &Options::default()).map_err(|e| format!("{e:?}"))?;
    let mut client =
        Client::connect(daemon.socket(Channel::Observer)).map_err(|e| e.to_string())?;
    let reconcile = |client: &mut Client, added: &BTreeSet<String>| {
        observer::reconcile(client, &repo, added, Instant::now() + OBSERVER_TIMEOUT)
            .map_err(|e| e.to_string())
    };
    reconcile(&mut client, &selected)?;
    let state =
        |client: &mut Client| -> Result<BTreeMap<String, (RevisionId, bool, usize)>, String> {
            let (_, s) = observer::state(client, &repo.scope()).map_err(|e| e.to_string())?;
            Ok(s.into_iter()
                .map(|(p, a)| (p, (a.head, a.base, a.breaks.len())))
                .collect())
        };
    let mut before = state(&mut client)?;
    let dirs: BTreeSet<String> = selected
        .iter()
        .map(|p| {
            p.rsplit_once('/')
                .map_or(String::new(), |(d, _)| d.to_owned())
        })
        .collect();
    let mut fired = Vec::new();
    let mut quiet = Vec::new();
    // Automatic oracle (not a label): an anchor fires iff Git reports its
    // blob changed, added or deleted between parent and commit.
    let mut disagreements = Vec::new();
    for pair in list.windows(2) {
        let (parent, commit) = (pair[0], pair[1]);
        run(&work, &["checkout", "-q", "--detach", commit], 1 << 20)?;
        reconcile(&mut client, &BTreeSet::new())?;
        let after = state(&mut client)?;
        let subject = tsv(&text(&work, &["log", "-1", "--format=%s", commit])?);
        let stat = text(
            &work,
            &["diff", "--numstat", "--no-renames", "-z", parent, commit],
        )?;
        let mut changed = BTreeMap::new();
        for entry in stat.split('\0').filter(|e| !e.is_empty()) {
            let mut f = entry.splitn(3, '\t');
            if let (Some(a), Some(r), Some(p)) = (f.next(), f.next(), f.next()) {
                changed.insert(p.to_owned(), (a.to_owned(), r.to_owned()));
            }
        }
        for path in &selected {
            let (b, a) = (before.get(path), after.get(path));
            let event = match (b, a) {
                (Some(b), Some(a)) if b == a => None,
                (Some((_, true, _)), Some((_, false, _))) => Some("broken"),
                (Some((_, false, _)), Some((_, true, _))) => Some("restored"),
                (Some((h, ..)), Some((g, ..))) if h != g => Some("changed"),
                (_, _) => Some("other"),
            };
            let (added, removed) = changed
                .get(path)
                .cloned()
                .unwrap_or_else(|| ("0".into(), "0".into()));
            let row = |obs: &str| {
                format!(
                    "{}\t{commit}\t{parent}\t{}\t{obs}\t{added}\t{removed}\t{subject}\t\t\t",
                    tsv(&url),
                    tsv(path)
                )
            };
            if event.is_some() != changed.contains_key(path) {
                disagreements.push(format!("{commit} {path} {event:?}"));
            }
            if let Some(event) = event {
                fired.push(row(event));
            } else if !changed.contains_key(path) {
                let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
                let near = changed.keys().any(|p| {
                    p.rsplit_once('/').map_or("", |(d, _)| d) == dir && dirs.contains(dir)
                });
                if near {
                    quiet.push((order(&format!("{commit}:{path}")), row("not-fired")));
                }
            }
        }
        before = after;
    }
    quiet.sort();
    let candidates = quiet.len();
    quiet.truncate(fired.len());
    let mut f = std::fs::File::create(out).map_err(|e| e.to_string())?;
    let head = |f: &mut std::fs::File| -> std::io::Result<()> {
        writeln!(f, "# P5a G6 replay: unlabelled pairs for HUMAN labelling")?;
        writeln!(f, "# repo_url={url}")?;
        writeln!(
            f,
            "# replayed={commits} first={} last={} anchors={anchors}",
            list[1], list[commits]
        )?;
        writeln!(
            f,
            "# selection: {anchors} supported regular files at {} with smallest git hash-object(path)",
            list[0]
        )?;
        writeln!(
            f,
            "# misses: not-fired same-directory changes, {} of {candidates} kept by smallest git hash-object(commit:path)",
            quiet.len()
        )?;
        writeln!(
            f,
            "# invalidates: y if a decision resting on this file at the parent commit must be re-validated after this commit, n otherwise; leave empty if unsure"
        )?;
        writeln!(
            f,
            "# git_diff_disagreements={} {}",
            disagreements.len(),
            disagreements.join("; ")
        )?;
        writeln!(
            f,
            "# anchors: {}",
            selected.iter().cloned().collect::<Vec<_>>().join(" ")
        )?;
        writeln!(f, "{HEADER}")
    };
    head(&mut f).map_err(|e| e.to_string())?;
    for row in fired.iter().chain(quiet.iter().map(|(_, r)| r)) {
        writeln!(f, "{row}").map_err(|e| e.to_string())?;
    }
    println!(
        "{}: {commits} commits, {} anchors, {} fired, {} not-fired candidates, {} git-diff disagreements -> {}",
        url,
        selected.len(),
        fired.len(),
        quiet.len(),
        disagreements.len(),
        out.display()
    );
    drop(daemon);
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

fn score(files: &[String], list: bool) -> Result<bool, String> {
    let (mut fp, mut tp, mut fneg, mut tn) = (0u32, 0u32, 0u32, 0u32);
    let mut unlabelled = Vec::new();
    let mut repos = BTreeSet::new();
    for file in files {
        let body = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
        let mut lines = body.lines().filter(|l| !l.starts_with('#'));
        if lines.next() != Some(HEADER) {
            return Err(format!("{file}: not an anchor_replay file"));
        }
        for line in lines {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 11 {
                return Err(format!("{file}: malformed row {line:?}"));
            }
            repos.insert(f[0].to_owned());
            let fired = f[4] != "not-fired";
            match (fired, f[8].trim(), f[9].trim().is_empty()) {
                (_, "y" | "n", true) => {
                    return Err(format!("{file}: label without labeler {line:?}"));
                }
                (true, "y", false) => tp += 1,
                (true, "n", false) => fp += 1,
                (false, "y", false) => fneg += 1,
                (false, "n", false) => tn += 1,
                (_, "", _) => unlabelled.push(format!("{}@{} {}", f[0], f[1], f[3])),
                (_, other, _) => return Err(format!("{file}: label {other:?} is not y/n")),
            }
        }
    }
    let labelled = tp + fp + fneg + tn;
    let rate = |a: u32, b: u32| {
        if b == 0 {
            "n/a".to_owned()
        } else {
            format!("{:.1}%", 100.0 * a as f64 / b as f64)
        }
    };
    println!(
        "repos={} labelled={labelled} unlabelled={}",
        repos.len(),
        unlabelled.len()
    );
    println!(
        "false_invalidation={} ({fp} of {} fired)",
        rate(fp, tp + fp),
        tp + fp
    );
    println!(
        "missed_invalidation={} ({fneg} of {} invalidating)",
        rate(fneg, tp + fneg),
        tp + fneg
    );
    if list {
        for u in &unlabelled {
            println!("unlabelled {u}");
        }
    }
    // ADR 0011 §7 minimums; P5a fixes no rate threshold (P5b compares to it).
    let complete = repos.len() >= 3 && labelled >= 200;
    println!("p5a_g6_evidence_complete={complete}");
    Ok(complete)
}

/// Interactive labelling in a terminal: shows each unlabelled pair (commit
/// message and the relevant diff from a clone under `<repos dir>/<name>`) and
/// records the human's y/n with the labeler's name. Pairs are presented in a
/// fixed mixed order (smallest `git hash-object` of `commit:path`) across all
/// files, so any prefix covers every repository and both fired and not-fired
/// pairs. Each answer is saved immediately; `q` stops, `s` skips.
fn label(repos: &Path, labeler: &str, files: &[String]) -> Result<bool, String> {
    use std::io::BufRead;
    if labeler.is_empty() || labeler.contains(['\t', '\n']) {
        return Err("labeler name required".into());
    }
    let mut docs: Vec<Vec<String>> = Vec::new();
    let mut queue = Vec::new();
    for (k, file) in files.iter().enumerate() {
        let body = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
        let lines: Vec<String> = body.lines().map(str::to_owned).collect();
        for (i, line) in lines.iter().enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            if !line.starts_with('#') && line != HEADER && f.len() == 11 && f[8].is_empty() {
                queue.push((order(&format!("{}:{}", f[1], f[3])), k, i));
            }
        }
        docs.push(lines);
    }
    queue.sort();
    let total = queue.len();
    println!(
        "{total} unlabelled pairs. For each: would a decision or plan that relied on this file at the parent commit have to be re-checked after this commit?\n  y = yes (behaviour, API, contract or meaning changed)\n  n = no (formatting, comments, typos, unrelated refactor, or nothing relevant changed)\n  s = skip (unsure)   q = quit (answers so far are saved)"
    );
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut done = 0;
    for (n, (_, k, i)) in queue.into_iter().enumerate() {
        let f: Vec<String> = docs[k][i].split('\t').map(str::to_owned).collect();
        let name = f[0]
            .trim_end_matches(".git")
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        let clone = repos.join(&name);
        let (commit, parent, path, event) = (&f[1], &f[2], &f[3], &f[4]);
        println!(
            "\n==== {}/{total}  {name}  {}  {path}  [{event}]",
            n + 1,
            &commit[..12]
        );
        let message = text(&clone, &["log", "-1", "--format=%B", commit]).unwrap_or_default();
        println!(
            "{}",
            message.lines().take(12).collect::<Vec<_>>().join("\n")
        );
        // Fired: the anchor's own diff. Not fired: what changed beside it.
        let diff = if event == "not-fired" {
            let dir = path.rsplit_once('/').map_or(".", |(d, _)| d);
            text(&clone, &["diff", "--stat", parent, commit, "--", dir]).and_then(|stat| {
                text(
                    &clone,
                    &["diff", "--no-color", "-U2", parent, commit, "--", dir],
                )
                .map(|d| format!("(anchor {path} unchanged; same-directory changes)\n{stat}\n{d}"))
            })
        } else {
            text(
                &clone,
                &["diff", "--no-color", "-U3", parent, commit, "--", path],
            )
        }
        .unwrap_or_else(|e| {
            format!(
                "(diff unavailable: {e}; clone {} missing?)",
                clone.display()
            )
        });
        let lines: Vec<&str> = diff.lines().collect();
        println!(
            "{}",
            lines
                .iter()
                .take(120)
                .copied()
                .collect::<Vec<_>>()
                .join("\n")
        );
        if lines.len() > 120 {
            println!(
                "... {} more lines (git -C {} show {commit} -- {path})",
                lines.len() - 120,
                clone.display()
            );
        }
        let answer = loop {
            print!("invalidates? [y/n/s/q] ");
            std::io::stdout().flush().map_err(|e| e.to_string())?;
            let mut line = String::new();
            if input.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                break "q".to_owned();
            }
            let a = line.trim().to_lowercase();
            if ["y", "n", "s", "q"].contains(&a.as_str()) {
                break a;
            }
        };
        match answer.as_str() {
            "q" => break,
            "s" => continue,
            _ => {}
        }
        let mut row = f.clone();
        row[8] = answer;
        row[9] = labeler.to_owned();
        docs[k][i] = row.join("\t");
        let tmp = format!("{}.tmp", files[k]);
        std::fs::write(&tmp, docs[k].join("\n") + "\n").map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &files[k]).map_err(|e| e.to_string())?;
        done += 1;
    }
    println!("\nlabelled {done} pairs this session");
    score(files, false)
}

/// (replay file name, repo url, commit, parent, path, observer event).
type PairKey = (String, String, String, String, String, String);
/// One sample stratum: repository and whether the observer fired.
type Stratum = (String, bool);
/// Per stratum: final y/n counts for (agreed + AI-only), the human-only
/// labels, the contested pairs (human, ai) and the files involved.
#[derive(Default)]
struct Cell {
    labels: [u32; 2],
    human: [u32; 2],
    contested: Vec<(String, String, String)>,
    files: BTreeSet<String>,
    ids: Vec<(String, bool)>,
}
fn pct(a: u32, b: u32) -> String {
    if b == 0 {
        "n/a".to_owned()
    } else {
        format!("{:.1}%", 100. * a as f64 / b as f64)
    }
}
/// ADR 0011 §11 (r0.6): G6 from agent labels on a pre-registered stratified
/// sample. Human audits are optional diagnostics, never completion criteria.
/// Historical human labels are read from the replay TSVs (never written
/// there from the AI column); disagreements are contested and only enter a
/// sensitivity range.
fn assisted(proposals: &Path, files: &[String]) -> Result<bool, String> {
    let body = std::fs::read_to_string(proposals).map_err(|e| e.to_string())?;
    let mut lines = body.lines();
    let header: Vec<&str> = lines.next().ok_or("empty proposals")?.split('\t').collect();
    let col = |name: &str| {
        header
            .iter()
            .position(|h| *h == name)
            .ok_or(format!("proposals: no column {name}"))
    };
    let cols = [
        col("id")?,
        col("source")?,
        col("repo_url")?,
        col("commit")?,
        col("parent")?,
        col("path")?,
        col("observer")?,
        col("ai_invalidates")?,
        col("rationale")?,
        col("review_focus")?,
        col("provenance")?,
    ];
    let width = cols.iter().max().copied().unwrap_or(0) + 1;
    let mut human: BTreeMap<PairKey, Vec<(String, String)>> = BTreeMap::new();
    let mut replay_ok = true;
    for file in files {
        let name = Path::new(file)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("bad file name")?
            .to_owned();
        let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
        let commits = text.lines().find_map(|line| {
            line.strip_prefix("# replayed=")?
                .split_whitespace()
                .next()?
                .parse::<usize>()
                .ok()
        });
        let disagreements = text.lines().find_map(|line| {
            line.strip_prefix("# git_diff_disagreements=")?
                .split_whitespace()
                .next()?
                .parse::<usize>()
                .ok()
        });
        replay_ok &= commits.is_some_and(|n| n >= 1000) && disagreements == Some(0);
        for line in text.lines().filter(|l| !l.starts_with('#') && *l != HEADER) {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 11 {
                return Err(format!("{file}: malformed row"));
            }
            human
                .entry((
                    name.clone(),
                    f[0].into(),
                    f[1].into(),
                    f[2].into(),
                    f[3].into(),
                    f[4].into(),
                ))
                .or_default()
                .push((f[8].trim().to_owned(), f[9].trim().to_owned()));
        }
    }
    let mut cells: BTreeMap<Stratum, Cell> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let (mut total, mut audited, mut agreed) = (0u32, 0u32, 0u32);
    for line in lines.filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < width {
            return Err(format!("proposals: short row {line:?}"));
        }
        let [
            c_id,
            c_src,
            c_url,
            c_commit,
            c_parent,
            c_path,
            c_obs,
            c_ai,
            c_reason,
            c_focus,
            c_provenance,
        ] = cols;
        if [c_reason, c_focus, c_provenance]
            .iter()
            .any(|&i| f[i].trim().is_empty())
        {
            return Err(format!(
                "{}: missing rationale, review focus or agent provenance",
                f[c_id]
            ));
        }
        let key: PairKey = (
            f[c_src].into(),
            f[c_url].into(),
            f[c_commit].into(),
            f[c_parent].into(),
            f[c_path].into(),
            f[c_obs].into(),
        );
        if !seen.insert(key.clone()) {
            return Err(format!("{}: duplicate proposal", f[c_id]));
        }
        let rows = human
            .get(&key)
            .ok_or(format!("{}: no replay row", f[c_id]))?;
        if rows.len() != 1 {
            return Err(format!("{}: {} replay rows match", f[c_id], rows.len()));
        }
        let ai = f[c_ai].trim();
        if !matches!(ai, "y" | "n") {
            return Err(format!("{}: AI label {ai:?}", f[c_id]));
        }
        let repo = f[c_url]
            .trim_end_matches(".git")
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_owned();
        let cell = cells.entry((repo, f[c_obs] != "not-fired")).or_default();
        cell.files.insert(f[c_path].to_owned());
        total += 1;
        let (label, labeler) = &rows[0];
        let slot = |l: &str| usize::from(l == "n");
        match (label.as_str(), labeler.is_empty()) {
            ("y" | "n", false) => {
                audited += 1;
                cell.ids.push((f[c_id].to_owned(), true));
                cell.human[slot(label)] += 1;
                if label == ai {
                    agreed += 1;
                    cell.labels[slot(label)] += 1;
                } else {
                    cell.contested
                        .push((f[c_id].to_owned(), label.clone(), ai.to_owned()));
                }
            }
            ("", true) => {
                cell.ids.push((f[c_id].to_owned(), false));
                cell.labels[slot(ai)] += 1;
            }
            (other, _) => {
                return Err(format!(
                    "{}: human label {other:?} needs y/n and a labeler",
                    f[c_id]
                ));
            }
        }
    }
    let agreement = if audited == 0 {
        0.0
    } else {
        agreed as f64 / audited as f64
    };
    // Wilson score interval, 95 %.
    let wilson = if audited == 0 {
        0.0
    } else {
        let (n, p, z) = (audited as f64, agreement, 1.96f64);
        (p + z * z / (2. * n) - z * ((p * (1. - p) + z * z / (4. * n)) / n).sqrt())
            / (1. + z * z / n)
    };
    let contested: usize = cells.values().map(|c| c.contested.len()).sum();
    println!(
        "G6 agent-labelled (ADR 0011 §11): {total} pairs, {audited} historical human reviews, {} AI-only, {contested} contested",
        total - audited
    );
    if audited == 0 {
        println!("  optional human agreement=n/a (no human review required)");
    } else {
        println!(
            "  optional human agreement {agreed}/{audited} = {:.1}% (Wilson 95% lower bound {:.1}%; non-blind, not a gate criterion)",
            100. * agreement,
            100. * wilson
        );
    }
    println!(
        "  per stratum: audited | final y/n (agreed + AI-only; contested out) | human-only y/n | contested -> human / AI | files"
    );
    let (mut pool, mut pool_h, mut pool_a) = ([[0u32; 2]; 2], [[0u32; 2]; 2], [[0u32; 2]; 2]);
    for ((repo, fired), c) in &cells {
        let mut ids = c.ids.clone();
        ids.sort();
        let n_aud = ids.iter().filter(|(_, a)| *a).count();
        let (mut h, mut a) = (c.labels, c.labels);
        for (_, hl, al) in &c.contested {
            h[usize::from(hl == "n")] += 1;
            a[usize::from(al == "n")] += 1;
        }
        let rate = |v: [u32; 2]| {
            if *fired {
                pct(v[1], v[0] + v[1])
            } else {
                pct(v[0], v[0] + v[1])
            }
        };
        println!(
            "    {repo:8} {:9} audited {n_aud:2}/{:2} | y={:3} n={:3} {}={:6} | human y={} n={} | {} / {} | {} files",
            if *fired { "fired" } else { "not-fired" },
            ids.len(),
            c.labels[0],
            c.labels[1],
            if *fired { "false_inv" } else { "inval_nf" },
            rate(c.labels),
            c.human[0],
            c.human[1],
            rate(h),
            rate(a),
            c.files.len()
        );
        let k = usize::from(*fired);
        for i in 0..2 {
            pool[k][i] += c.labels[i];
            pool_h[k][i] += h[i];
            pool_a[k][i] += a[i];
        }
        for (id, hl, al) in &c.contested {
            println!("      contested {id} human={hl} ai={al}");
        }
    }
    let show = |p: [[u32; 2]; 2]| {
        format!(
            "false_invalidation={} invalidating_not_fired={}",
            pct(p[1][1], p[1][0] + p[1][1]),
            pct(p[0][0], p[0][0] + p[0][1])
        )
    };
    println!(
        "  pooled over the stratified sample (NOT population rates): {}",
        show(pool)
    );
    println!("    contested resolved the human way: {}", show(pool_h));
    println!("    contested resolved the AI way:    {}", show(pool_a));
    let repos: BTreeSet<_> = cells.keys().map(|(r, _)| r.clone()).collect();
    let coverage_ok = repos.iter().all(|repo| {
        [false, true]
            .iter()
            .all(|&fired| cells.contains_key(&(repo.clone(), fired)))
    });
    let complete = repos.len() >= 3 && total >= 200 && coverage_ok && replay_ok;
    println!(
        "  criteria: repos={} (>=3) pairs={total} (>=200) both strata per repo={coverage_ok} replay>=1000 with zero Git disagreements={replay_ok}; human audit optional",
        repos.len(),
    );
    println!("p5a_g6_evidence_complete={complete}");
    Ok(complete)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str, default: usize| {
        args.windows(2)
            .find(|w| w[0] == name)
            .and_then(|w| w[1].parse().ok())
            .unwrap_or(default)
    };
    let result = match args.first().map(String::as_str) {
        Some("prepare") if args.len() >= 3 => prepare(
            Path::new(&args[1]),
            Path::new(&args[2]),
            flag("--commits", 1000),
            flag("--anchors", 20),
        )
        .map(|()| true),
        Some("score") if args.len() >= 4 && args[1] == "--assisted" => {
            assisted(Path::new(&args[2]), &args[3..])
        }
        Some("score") if args.len() >= 2 => score(&args[1..], true),
        Some("label") if args.len() >= 4 => label(Path::new(&args[1]), &args[2], &args[3..]),
        _ => Err("usage: anchor_replay prepare <clone> <out.tsv> [--commits N] [--anchors K] | score [--assisted <proposals.tsv>] <labelled.tsv>... | label <repos dir> <labeler> <file.tsv>...".into()),
    };
    match result {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("anchor_replay: {e}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_evidence_needs_no_human_audit_but_still_requires_replay_coverage_and_provenance() {
        let root = std::env::temp_dir().join(format!("agent-g6-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let proposals = root.join("proposals.tsv");
        let mut proposal_text = "id\tsource\trepo_url\tcommit\tparent\tpath\tobserver\tai_invalidates\trationale\treview_focus\tprovenance\n".to_owned();
        let mut files = Vec::new();
        for repo in 0..3 {
            let name = format!("repo{repo}.tsv");
            let mut text = format!("# replayed=1000\n# git_diff_disagreements=0\n{HEADER}\n");
            for i in 0..70 {
                let event = if i < 35 { "changed" } else { "not-fired" };
                let commit = format!("{:040x}", repo * 70 + i + 1);
                let url = format!("https://example.org/repo{repo}.git");
                text.push_str(&format!(
                    "{url}\t{commit}\tparent\tfile{i}.rs\t{event}\t0\t0\tsubject\t\t\t\n"
                ));
                proposal_text.push_str(&format!("G6-{}\t{name}\t{url}\t{commit}\tparent\tfile{i}.rs\t{event}\tn\treason\tfocus\tagent:test\n", repo * 70 + i + 1));
            }
            let file = root.join(name);
            std::fs::write(&file, text).unwrap();
            files.push(file.to_str().unwrap().to_owned());
        }
        std::fs::write(&proposals, &proposal_text).unwrap();
        assert!(
            assisted(&proposals, &files).unwrap(),
            "zero human reviews are allowed"
        );

        std::fs::write(&proposals, proposal_text.replacen("agent:test", "", 1)).unwrap();
        assert!(
            assisted(&proposals, &files)
                .unwrap_err()
                .contains("provenance")
        );
        std::fs::write(&proposals, &proposal_text).unwrap();

        let original = std::fs::read_to_string(&files[0]).unwrap();
        std::fs::write(&files[0], original.replace("replayed=1000", "replayed=999")).unwrap();
        assert!(
            !assisted(&proposals, &files).unwrap(),
            "real replay minimum remains"
        );
        std::fs::write(&files[0], &original).unwrap();

        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            std::fs::write(file, text.replace("not-fired", "changed")).unwrap();
        }
        std::fs::write(&proposals, proposal_text.replace("not-fired", "changed")).unwrap();
        assert!(
            !assisted(&proposals, &files).unwrap(),
            "both sample strata remain required"
        );
    }
}
