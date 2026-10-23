//! Reference-corpus walk check (ADR 0012 §4, G2): prints every symbol's
//! normalised hash at a pinned commit, so two binaries' outputs can be diffed
//! to report how many symbols a normaliser-version change re-revisions.
//!
//!   symbol_hashes <clone> <commit>   →   path \t symbol \t version \t hash
use contextdb_anchors::Language;
use contextdb_symbols::{normaliser_version, parse};
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let o = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    o.status
        .success()
        .then_some(o.stdout)
        .ok_or_else(|| format!("git {} failed", args.join(" ")))
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [Some(clone), Some(commit)] = [args.first(), args.get(1)] else {
        return Err("usage: symbol_hashes <clone> <commit>".into());
    };
    let root = Path::new(clone);
    let listing = git(
        root,
        &["ls-tree", "-r", "-z", "--name-only", "--full-tree", commit],
    )?;
    for path in listing.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8_lossy(path).into_owned();
        let Some(language) = Language::of_path(&path) else {
            continue;
        };
        let src = git(root, &["cat-file", "blob", &format!("{commit}:{path}")])?;
        if src.len() > 1 << 20 {
            continue;
        }
        let file =
            parse(language, &src, Instant::now() + Duration::from_secs(30)).map_err(|e| e.0)?;
        for (symbol, s) in &file.symbols {
            let hash: String = s.hash.iter().map(|b| format!("{b:02x}")).collect();
            let state = if s.error { "error" } else { &hash };
            println!(
                "{path}\t{symbol}\t{}\t{state}",
                normaliser_version(language)
            );
        }
    }
    Ok(())
}
