//! P5a G6 commit replay (ADR 0011 §7). Prepares anchor/change pairs from real
//! repository history for HUMAN labelling, then scores labelled files. It never
//! writes a label: an unlabelled pair stays unlabelled and is reported.
//!
//!   anchor_replay prepare <clone> <out.tsv> [--commits N] [--anchors K]
//!   anchor_replay score <labelled.tsv>...
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

fn score(files: &[String]) -> Result<bool, String> {
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
    for u in &unlabelled {
        println!("unlabelled {u}");
    }
    // ADR 0011 §7 minimums; P5a fixes no rate threshold (P5b compares to it).
    let complete = repos.len() >= 3 && labelled >= 200;
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
        Some("score") if args.len() >= 2 => score(&args[1..]),
        _ => Err("usage: anchor_replay prepare <clone> <out.tsv> [--commits N] [--anchors K] | score <labelled.tsv>...".into()),
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
