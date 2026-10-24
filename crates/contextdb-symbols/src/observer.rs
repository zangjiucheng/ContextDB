//! The symbol-aware observer (ADR 0012 §5): one scan captures every file and
//! host file, parses each host file once, and submits file and symbol anchor
//! changes with one watermark CAS, exactly like the P5a observer.
use crate::{ParsedFile, normaliser_version, parse};
use contextdb_anchors::{
    AnchorDescriptor, AnchorObservation, Language, Observation, Repo, SymbolDescriptor,
    Unavailable,
    capture::{git, position},
    observer::{state_by_key_on, submit_on},
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
        let (branch, prepared) = scan(client, repo, added, mode, deadline)?;
        match submit_on(client, prepared, branch) {
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

/// What one scan observes: file anchors by path, symbol anchors by logical
/// key → (host path, symbol).
pub type Targets = (BTreeSet<String>, BTreeMap<String, (String, String)>);

/// The committed anchors plus `added`, refusing (in `Mode::Scan`) a committed
/// symbol head made with another normaliser version (ADR 0012 §4).
pub fn targets(
    scope: &str,
    committed: &BTreeMap<String, (AnchorDescriptor, contextdb_anchors::AnchorState)>,
    added: &Added,
    mode: Mode,
) -> Result<Targets, Unavailable> {
    let mut files: BTreeSet<String> = added.files.clone();
    let mut symbols: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (key, (d, _)) in committed {
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
        // Legacy file paths may contain '#'. A matching logical key belongs
        // to that file forever; never revise it into a different anchor kind.
        if committed
            .get(&key)
            .is_some_and(|(d, _)| matches!(d, AnchorDescriptor::File(_)))
        {
            return Err(unavailable(format!(
                "LogicalExists: {key} belongs to a legacy file anchor; rename that file"
            )));
        }
        symbols.insert(key, (path.clone(), symbol.clone()));
    }
    Ok((files, symbols))
}

/// Whether a host's captured bytes differ from its blob at `HEAD`.
pub type Edited<'a> = dyn FnMut(&str, &[u8]) -> Result<bool, Unavailable> + 'a;

/// Host paths of the symbol targets.
pub fn hosts(targets: &Targets) -> BTreeSet<String> {
    targets.1.values().map(|(h, _)| h.clone()).collect()
}

/// The pure half of a scan (ADR 0012 §4–§6): from a capture of every target
/// path (`entries`, at `head`) and the kept host bytes, parse each host once,
/// derive every observation, rebind candidates and the position, and prepare
/// the batch. `edited` answers whether a host is modified relative to `HEAD`.
#[allow(clippy::too_many_arguments)]
pub fn prepare_scan(
    repo: &str,
    targets: &Targets,
    head: &[u8],
    entries: &BTreeMap<String, Observation>,
    bytes: &BTreeMap<String, Vec<u8>>,
    committed: BTreeMap<String, contextdb_anchors::AnchorState>,
    watermark: Option<Vec<u8>>,
    edited: &mut Edited,
    deadline: Instant,
) -> Result<contextdb_anchors::Prepared, Unavailable> {
    let (files, symbols) = targets;
    let hosts = hosts(targets);
    let hosts_only: BTreeSet<String> = hosts.difference(files).cloned().collect();

    // Parse each distinct host content once, within the byte budget (checked
    // before any parse) and the deadline.
    let language = |path: &String| Language::of_path(path).expect("hosts have a language");
    let distinct: BTreeSet<(Language, &[u8])> = bytes
        .iter()
        .map(|(path, content)| (language(path), content.as_slice()))
        .collect();
    if distinct.iter().map(|(_, c)| c.len()).sum::<usize>() > PARSE_BUDGET {
        return Err(unavailable("parse byte budget"));
    }
    let mut by_content: BTreeMap<(Language, &[u8]), ParsedFile> = BTreeMap::new();
    for (language, content) in distinct {
        let file = parse(language, content, deadline).map_err(|e| unavailable(e.0))?;
        by_content.insert((language, content), file);
    }
    let parsed: BTreeMap<String, ParsedFile> = bytes
        .iter()
        .map(|(path, content)| {
            let file = &by_content[&(language(path), content.as_slice())];
            (path.clone(), file.clone())
        })
        .collect();

    let mut observations: BTreeMap<String, (String, AnchorObservation)> = BTreeMap::new();
    for path in files {
        let key = format!("code:{repo}/{path}");
        let obs = match &entries[path] {
            Observation::Present(d) => {
                AnchorObservation::Present(AnchorDescriptor::File(d.clone()))
            }
            Observation::Missing => AnchorObservation::Missing,
            Observation::Broken(r) => AnchorObservation::Broken(r.clone()),
        };
        observations.insert(key, (path.clone(), obs));
    }
    let mut versions = BTreeMap::new();
    for (key, (host, symbol)) in symbols {
        let language = Language::of_path(host).expect("symbol hosts have a language");
        versions.insert(language, normaliser_version(language));
        let label = format!("{host}#{symbol}");
        let obs = match (&entries[host], parsed.get(host)) {
            (Observation::Missing, _) => AnchorObservation::Missing,
            (Observation::Broken(r), _) => AnchorObservation::Broken(r.clone()),
            (Observation::Present(_), Some(file)) => match file.symbols.get(symbol) {
                Some(s) if !s.error => {
                    AnchorObservation::Present(AnchorDescriptor::Symbol(SymbolDescriptor {
                        repo: repo.to_owned(),
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
                    if edited(host, &bytes[host])? {
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
    let position = position(repo, head, entries, &hosts_only, &versions);
    prepare_anchors(
        repo,
        &observations,
        &committed,
        &candidates,
        watermark,
        position,
    )
    .map_err(Unavailable)
}

fn scan(
    client: &mut Client,
    repo: &Repo,
    added: &Added,
    mode: Mode,
    deadline: Instant,
) -> Result<(contextdb_model::BranchId, contextdb_anchors::Prepared), Unavailable> {
    let scope = repo.scope();
    // On the branch of the worktree's checked-out git branch (ADR 0013 §7).
    let git_branch = contextdb_proto::git::checked_out_branch(&repo.root);
    let (branch, (watermark, committed)) = state_by_key_on(client, &scope, git_branch.as_deref())?;
    let targets = targets(&scope, &committed, added, mode)?;
    let hosts = hosts(&targets);
    let all: BTreeSet<String> = targets.0.union(&hosts).cloned().collect();
    let (captured, bytes) = repo.capture_keeping(&all, &hosts, deadline)?;
    let prior = committed
        .into_iter()
        .map(|(k, (_, state))| (k, state))
        .collect();
    prepare_scan(
        &repo.id,
        &targets,
        &captured.head,
        &captured.entries,
        &bytes,
        prior,
        watermark,
        &mut |host, content| edited(repo, host, content, deadline),
        deadline,
    )
    .map(|prepared| (branch, prepared))
}
