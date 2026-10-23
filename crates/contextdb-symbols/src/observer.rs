//! The symbol-aware observer (ADR 0012 §5): one scan captures every file and
//! host file, parses each host file once, and submits file and symbol anchor
//! changes with one watermark CAS, exactly like the P5a observer.
use crate::{ParsedFile, normaliser_version, parse};
use contextdb_anchors::{
    AnchorDescriptor, AnchorObservation, Language, Observation, Repo, SymbolDescriptor,
    Unavailable,
    capture::{git, position},
    observer::{state_by_key, submit},
    prepare_anchors,
    symbol::validate_symbol,
    validate_path,
};
use contextdb_model::CommitResult;
use contextdb_proto::Client;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

/// Host-file bytes parsed per scan (ADR 0012 §4): ≈ 3 s at ≈ 9 ms per 100 KB.
pub const PARSE_BUDGET: usize = 32 << 20;

/// What a scan may add to the committed anchors.
#[derive(Clone, Debug, Default)]
pub struct Added {
    pub files: BTreeSet<String>,
    /// `(host path, symbol)` with the symbol's real spelling.
    pub symbols: BTreeSet<(String, String)>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Normal scan: a committed symbol head made with another normaliser
    /// version makes the scope unavailable.
    Scan,
    /// `ctx anchors migrate`: crosses normaliser versions.
    Migrate,
}
fn unavailable(e: impl std::fmt::Display) -> Unavailable {
    Unavailable(e.to_string())
}

/// Whether a tracked host file is being edited: the filtered hash of the
/// captured bytes differs from the path's blob at HEAD (ADR 0012 §4). No stat
/// cache or mtime; Git never writes the index.
fn edited(repo: &Repo, path: &str, bytes: &[u8], deadline: Instant) -> Result<bool, Unavailable> {
    let (ok, head) = git(
        &repo.root,
        &["--no-optional-locks", "ls-tree", "-z", "HEAD", "--", path],
        &[],
        16 << 10,
        deadline,
    )?;
    let head_oid = head
        .split(|b| *b == b'\t')
        .next()
        .and_then(|meta| std::str::from_utf8(meta).ok())
        .and_then(|meta| meta.split(' ').nth(2))
        .map(str::to_owned);
    let Some(head_oid) = head_oid.filter(|_| ok) else {
        return Ok(false); // untracked or unborn HEAD: not "being edited"
    };
    let (ok, worktree) = git(
        &repo.root,
        &[
            "--no-optional-locks",
            "hash-object",
            &format!("--path={path}"),
            "--stdin",
        ],
        bytes,
        256,
        deadline,
    )?;
    if !ok {
        return Err(unavailable("hash-object failed"));
    }
    Ok(String::from_utf8_lossy(&worktree).trim() != head_oid)
}

/// One reconcile with the committed state, retried after a CAS conflict by
/// reloading and capturing again (never resubmitting an old capture).
pub fn reconcile(
    client: &mut Client,
    repo: &Repo,
    added: &Added,
    mode: Mode,
    deadline: Instant,
) -> Result<Option<CommitResult>, Unavailable> {
    for path in &added.files {
        validate_path(&repo.id, path).map_err(unavailable)?;
    }
    for (path, symbol) in &added.symbols {
        validate_path(&repo.id, path).map_err(unavailable)?;
        validate_symbol(symbol)
            .map_err(|_| unavailable(format!("{path}#{symbol}: not a symbol path")))?;
        if Language::of_path(path).is_none() {
            return Err(unavailable(format!(
                "{path}: no symbol anchors for this file type"
            )));
        }
    }
    for _ in 0..3 {
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| unavailable("observer deadline"))?;
        client.set_timeout(timeout).map_err(unavailable)?;
        let prepared = scan(client, repo, added, mode, deadline)?;
        match submit(client, prepared) {
            Ok(result) => return Ok(result),
            Err(e)
                if ["HeadConflict", "WatermarkConflict", "LogicalExists"]
                    .iter()
                    .any(|c| e.0.contains(c)) => {}
            Err(e) => return Err(e),
        }
    }
    Err(unavailable("observer CAS retry limit"))
}

fn scan(
    client: &mut Client,
    repo: &Repo,
    added: &Added,
    mode: Mode,
    deadline: Instant,
) -> Result<contextdb_anchors::Prepared, Unavailable> {
    let scope = repo.scope();
    let (watermark, committed) = state_by_key(client, &scope)?;
    let mut files: BTreeSet<String> = added.files.clone();
    let mut symbols: BTreeMap<String, (String, String)> = BTreeMap::new(); // key → (host, symbol)
    for (key, (d, _)) in &committed {
        match d {
            AnchorDescriptor::File(f) => {
                files.insert(f.path.clone());
            }
            AnchorDescriptor::Symbol(s) => {
                if mode == Mode::Scan && s.normaliser != normaliser_version(s.language) {
                    return Err(unavailable(format!(
                        "{key} was observed with normaliser {} but this binary uses {}: run ctx anchors migrate",
                        s.normaliser,
                        normaliser_version(s.language)
                    )));
                }
                symbols.insert(key.clone(), (s.path.clone(), s.symbol.clone()));
            }
        }
    }
    for (path, symbol) in &added.symbols {
        let key = format!(
            "{scope}/{path}#{}",
            contextdb_anchors::symbol::encode_symbol(symbol).map_err(unavailable)?
        );
        symbols.insert(key, (path.clone(), symbol.clone()));
    }
    let hosts: BTreeSet<String> = symbols.values().map(|(h, _)| h.clone()).collect();
    let hosts_only: BTreeSet<String> = hosts.difference(&files).cloned().collect();
    let all: BTreeSet<String> = files.union(&hosts).cloned().collect();
    let (captured, bytes) = repo.capture_keeping(&all, &hosts, deadline)?;

    // Parse each host once, within the byte budget and the deadline.
    let mut parsed: BTreeMap<String, ParsedFile> = BTreeMap::new();
    let mut budget = PARSE_BUDGET;
    for (path, content) in &bytes {
        budget = budget
            .checked_sub(content.len())
            .ok_or_else(|| unavailable("parse byte budget"))?;
        let language = Language::of_path(path).expect("hosts have a language");
        parsed.insert(
            path.clone(),
            parse(language, content, deadline).map_err(|e| unavailable(e.0))?,
        );
    }

    let mut observations: BTreeMap<String, (String, AnchorObservation)> = BTreeMap::new();
    for path in &files {
        let key = format!("{scope}/{path}");
        let obs = match &captured.entries[path] {
            Observation::Present(d) => {
                AnchorObservation::Present(AnchorDescriptor::File(d.clone()))
            }
            Observation::Missing => AnchorObservation::Missing,
            Observation::Broken(r) => AnchorObservation::Broken(r.clone()),
        };
        observations.insert(key, (path.clone(), obs));
    }
    let mut versions = BTreeMap::new();
    for (key, (host, symbol)) in &symbols {
        let language = Language::of_path(host).expect("symbol hosts have a language");
        versions.insert(language, normaliser_version(language));
        let label = format!("{host}#{symbol}");
        let obs = match (&captured.entries[host], parsed.get(host)) {
            (Observation::Missing, _) => AnchorObservation::Missing,
            (Observation::Broken(r), _) => AnchorObservation::Broken(r.clone()),
            (Observation::Present(_), Some(file)) => match file.symbols.get(symbol) {
                Some(s) if !s.error => {
                    AnchorObservation::Present(AnchorDescriptor::Symbol(SymbolDescriptor {
                        repo: repo.id.clone(),
                        path: host.clone(),
                        symbol: symbol.clone(),
                        language,
                        normaliser: normaliser_version(language),
                        hash: s.hash.to_vec(),
                        masked: s.masked.to_vec(),
                    }))
                }
                None if !file.has_error => AnchorObservation::Missing,
                // The span or its enclosing chain has errors, or the symbol is
                // not found in a file with errors (ADR 0012 §4).
                _ => {
                    if edited(repo, host, &bytes[host], deadline)? {
                        return Err(unavailable(format!(
                            "{host} is being edited and does not parse"
                        )));
                    }
                    AnchorObservation::Broken("parse-error".into())
                }
            },
            (Observation::Present(_), None) => return Err(unavailable("host bytes not kept")),
        };
        observations.insert(key.clone(), (label, obs));
    }
    // Rebind suggestions: every symbol of the parsed host files by its
    // name-masked hash (ADR 0012 §5).
    let mut candidates: BTreeMap<Vec<u8>, BTreeSet<String>> = BTreeMap::new();
    for (path, file) in &parsed {
        for (symbol, s) in &file.symbols {
            candidates
                .entry(s.masked.to_vec())
                .or_default()
                .insert(format!("{path}#{symbol}"));
        }
    }
    let position = position(
        &repo.id,
        &captured.head,
        &captured.entries,
        &hosts_only,
        &versions,
    );
    let prior = committed
        .into_iter()
        .map(|(k, (_, state))| (k, state))
        .collect();
    prepare_anchors(
        &repo.id,
        &observations,
        &prior,
        &candidates,
        watermark,
        position,
    )
    .map_err(Unavailable)
}
