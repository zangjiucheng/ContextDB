//! P5a operator CLI; the running P4 daemon is never upgraded automatically.
use contextdb_anchors::{
    AnchorDescriptor, Repo,
    capture::{OBSERVER_TIMEOUT, git},
    descriptor::unhex,
    observer,
};
use contextdb_model::Wire;
use contextdb_proto::{
    Client, lease,
    proto::{Request, Response},
};
use contextdb_symbols::observer::{self as symbol_observer, Added, Mode};
use std::{
    collections::BTreeSet,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
fn connect(dir: &Path, socket: &str) -> Result<Client, String> {
    let lease = lease::read_lease(&dir.join("ctxd.lease"))
        .filter(lease::Lease::is_live)
        .ok_or("ctxd is not running")?;
    let socket = lease
        .sockets
        .iter()
        .find(|s| s.ends_with(socket))
        .ok_or("socket not in lease")?;
    Client::connect_timeout(socket, OBSERVER_TIMEOUT).map_err(|e| e.to_string())
}
fn stable_lock(path: &Path) -> Result<std::fs::File, String> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + OBSERVER_TIMEOUT;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}
/// Refuses anchors on runtime data. New registrations use the current path
/// rule (no `#`); committed paths only the legacy one, so a legacy `#` file
/// anchor is observed Broken instead of blocking every scan.
fn reject_runtime(
    dir: &Path,
    repo: &Repo,
    paths: &BTreeSet<String>,
    committed: bool,
) -> Result<(), String> {
    let data = dir.canonicalize().map_err(|e| e.to_string())?;
    for path in paths {
        if committed {
            contextdb_anchors::validate_legacy_path(&repo.id, path)
        } else {
            contextdb_anchors::validate_path(&repo.id, path)
        }
        .map_err(|e| format!("{path}: {e}"))?;
        let joined = repo.root.join(path);
        if joined.starts_with(&data) || joined.canonicalize().is_ok_and(|p| p.starts_with(&data)) {
            return Err("runtime data paths cannot be anchors".into());
        }
    }
    Ok(())
}
fn route(dir: &Path, repo: &Repo) -> Result<(), String> {
    let _lock = stable_lock(&dir.join("anchors.routes.lock"))?;
    let path = dir.join("anchors.routes");
    let mut routes = match std::fs::read(&path) {
        Ok(b) => Vec::<(String, String)>::from_bytes(&b).map_err(|e| e.to_string())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.to_string()),
    };
    let root = repo.root.to_str().ok_or("non-UTF8 root")?.to_owned();
    if let Some((_, existing)) = routes.iter().find(|(scope, _)| *scope == repo.scope()) {
        return if *existing == root {
            Ok(())
        } else {
            Err("repo-id routing collision".into())
        };
    }
    routes.push((repo.scope(), root));
    routes.sort();
    let tmp = dir.join(format!("anchors.routes.{}.tmp", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    let result = (|| {
        f.write_all(&routes.canonical_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        std::fs::File::open(dir)?.sync_all()
    })();
    let _ = std::fs::remove_file(tmp);
    result.map_err(|e| e.to_string())
}
const USAGE: &str = "anchors add <dir> <repo> <path>[#<symbol>]... | scan|watch|status|migrate <dir> <repo> | hooks <dir> <repo> install|uninstall [--pre-push] | symbols <repo> <path> | repair-announce <dir>";

/// Paths of the committed anchors (file anchors and symbol host files), from
/// their descriptors (ADR 0012 §7).
fn committed_paths(client: &mut Client, repo: &Repo) -> Result<BTreeSet<String>, String> {
    let (_, state) = observer::state_by_key(client, &repo.scope()).map_err(|e| e.to_string())?;
    Ok(state.values().map(|(d, _)| d.path().to_owned()).collect())
}
pub fn run(args: &[String]) -> Result<u8, String> {
    let sub = args.first().map(String::as_str).ok_or(USAGE)?;
    if sub == "symbols" {
        // ADR 0012 §8: a file's symbols with their keys.
        let repo = Repo::open(
            Path::new(args.get(1).ok_or(USAGE)?),
            Instant::now() + OBSERVER_TIMEOUT,
        )
        .map_err(|e| e.to_string())?;
        let path = args.get(2).ok_or(USAGE)?;
        let language = contextdb_anchors::Language::of_path(path)
            .ok_or(format!("{path}: no symbol anchors for this file type"))?;
        let bytes = std::fs::read(repo.root.join(path)).map_err(|e| format!("{path}: {e}"))?;
        let file = contextdb_symbols::parse(language, &bytes, Instant::now() + OBSERVER_TIMEOUT)
            .map_err(|e| e.to_string())?;
        for (symbol, s) in &file.symbols {
            let key =
                contextdb_anchors::symbol::encode_symbol(symbol).map_err(|e| e.to_string())?;
            println!(
                "{path}#{symbol}\tcode:{}/{path}#{key}{}",
                repo.id,
                if s.error { "\t(parse error)" } else { "" }
            );
        }
        if file.has_error {
            eprintln!("note: {path} has parse errors");
        }
        return Ok(0);
    }
    let dir = Path::new(args.get(1).ok_or("missing data dir")?);
    if sub == "repair-announce" {
        // ADR 0011 §9.4: after fixing what made an announce fail.
        contextdb_proto::outbox::Outbox::new(dir)
            .repair_announce()
            .map_err(|e| e.to_string())?;
        println!("announce record repaired");
        return Ok(0);
    }
    let repo = Repo::open(
        Path::new(args.get(2).ok_or("missing repo root")?),
        Instant::now() + OBSERVER_TIMEOUT,
    )
    .map_err(|e| e.to_string())?;
    if sub == "hooks" {
        return hooks(dir, &repo, &args[3..]);
    }
    let mut client = connect(dir, "observer.sock")?;
    match sub {
        "status" => {
            let (watermark, state) =
                observer::state_by_key(&mut client, &repo.scope()).map_err(|e| e.to_string())?;
            println!(
                "scope={} anchors={} watermark={watermark:?}",
                repo.scope(),
                state.len()
            );
            for (d, a) in state.values() {
                let note = match d {
                    AnchorDescriptor::Symbol(s)
                        if s.normaliser != contextdb_symbols::normaliser_version(s.language) =>
                    {
                        " (other normaliser version: run ctx anchors migrate)"
                    }
                    AnchorDescriptor::File(f) if f.path.contains('#') => {
                        " (legacy path with '#': rename the file)"
                    }
                    _ => "",
                };
                println!(
                    "{} r{} base={} breaks={:?}{note}",
                    d.label(),
                    a.head.0,
                    a.base,
                    a.breaks
                );
            }
        }
        "add" | "scan" | "watch" | "migrate" => {
            let mut added = Added::default();
            if sub == "add" {
                if args.len() < 4 {
                    return Err("anchors add needs <path> or <path>#<symbol> arguments".into());
                }
                for arg in &args[3..] {
                    match arg.split_once('#') {
                        Some((path, symbol)) => {
                            added.symbols.insert((path.to_owned(), symbol.to_owned()));
                        }
                        None => {
                            added.files.insert(arg.clone());
                        }
                    }
                }
            } else if args.len() != 3 {
                return Err(format!("{sub} takes no paths"));
            }
            let new_paths: BTreeSet<String> = added
                .files
                .iter()
                .cloned()
                .chain(added.symbols.iter().map(|(p, _)| p.clone()))
                .collect();
            reject_runtime(dir, &repo, &new_paths, false)?;
            for path in &new_paths {
                if !contextdb_anchors::capture::exact_spelling(&repo.root, path)
                    .map_err(|e| e.to_string())?
                {
                    return Err(format!(
                        "{path}: no file with exactly this spelling (case alias or missing)"
                    ));
                }
            }
            // A symbol must exist in its file (listed on a miss).
            for (path, symbol) in &added.symbols {
                let language = contextdb_anchors::Language::of_path(path)
                    .ok_or(format!("{path}: no symbol anchors for this file type"))?;
                let bytes =
                    std::fs::read(repo.root.join(path)).map_err(|e| format!("{path}: {e}"))?;
                let file =
                    contextdb_symbols::parse(language, &bytes, Instant::now() + OBSERVER_TIMEOUT)
                        .map_err(|e| e.to_string())?;
                if !file.symbols.contains_key(symbol) {
                    let known: Vec<_> = file.symbols.keys().map(String::as_str).collect();
                    return Err(format!(
                        "{path}#{symbol}: no such symbol; the file has: {}",
                        known.join(", ")
                    ));
                }
            }
            reject_runtime(dir, &repo, &committed_paths(&mut client, &repo)?, true)?;
            // Route config is trusted operator data and survives daemon restart.
            route(dir, &repo)?;
            let mode = if sub == "migrate" {
                Mode::Migrate
            } else {
                Mode::Scan
            };
            loop {
                let result = symbol_observer::reconcile(
                    &mut client,
                    &repo,
                    &added,
                    mode,
                    Instant::now() + OBSERVER_TIMEOUT,
                );
                match result {
                    Ok(Some(r)) => println!(
                        "anchors reconciled epoch={} changed={}",
                        r.epoch.0,
                        r.status_diff.len()
                    ),
                    Ok(None) => {
                        if sub != "watch" {
                            println!("anchors unchanged");
                        }
                    }
                    Err(e) if sub == "watch" => {
                        eprintln!("anchors unavailable: {e}");
                        // A daemon restart invalidates the old socket. Retry
                        // discovery while continuing periodic complete scans.
                        if let Ok(fresh) = connect(dir, "observer.sock") {
                            client = fresh;
                        }
                    }
                    Err(e) if e.0.contains("LogicalExists") => {
                        return Err(format!(
                            "{e}: a key is taken (a legacy file anchor whose path contains '#' can encode like a symbol key; rename that file)"
                        ));
                    }
                    Err(e) => return Err(e.to_string()),
                }
                if sub != "watch" {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        _ => return Err(USAGE.into()),
    }
    Ok(0)
}
fn output(repo: &Repo, args: &[&str], cap: usize, deadline: Instant) -> Result<Vec<u8>, String> {
    let (ok, out) = git(&repo.root, args, &[], cap, deadline).map_err(|e| e.to_string())?;
    if !ok {
        return Err(format!("Git failed: {}", args[0]));
    }
    Ok(out)
}
pub fn range(dir: &Path, args: &[String]) -> Result<u8, String> {
    let at = args
        .iter()
        .position(|a| a == "--range")
        .ok_or("missing --range")?;
    let range = args.get(at + 1).ok_or("missing a..b")?;
    if !args.iter().any(|a| a == "--strict") {
        return Err("range checks require --strict (point-in-time window)".into());
    }
    let repo_path = args
        .windows(2)
        .find(|p| p[0] == "--repo")
        .map(|p| p[1].as_str())
        .unwrap_or(".");
    let repo = Repo::open(Path::new(repo_path), Instant::now() + OBSERVER_TIMEOUT)
        .map_err(|e| e.to_string())?;
    let (base, tip) = range
        .split_once("..")
        .filter(|(a, b)| {
            !a.is_empty()
                && !b.is_empty()
                && !b.contains("..")
                && !a.starts_with('-')
                && !b.starts_with('-')
        })
        .ok_or("expected a..b")?;
    check_range(dir, &repo, base, tip)
}
fn resolve(repo: &Repo, name: &str, deadline: Instant) -> Result<String, String> {
    if name.starts_with('-') {
        return Err("invalid ref".into());
    }
    let out = output(
        repo,
        &["rev-parse", "--verify", &format!("{name}^{{commit}}")],
        128,
        deadline,
    )?;
    let text = String::from_utf8(out).map_err(|e| e.to_string())?;
    let text = text.trim();
    if unhex(text.as_bytes()).map_err(|e| e.to_string())?.len() != repo.format.oid_len() {
        return Err("wrong OID".into());
    }
    Ok(text.into())
}
fn check_range(dir: &Path, repo: &Repo, base: &str, tip: &str) -> Result<u8, String> {
    let deadline = Instant::now() + OBSERVER_TIMEOUT;
    let head = resolve(repo, "HEAD", deadline)?;
    let tip = resolve(repo, tip, deadline)?;
    if tip != head {
        return Err("Unknown: pushed tip is not current HEAD".into());
    }
    let mut client = connect(dir, "observer.sock")?;
    symbol_observer::reconcile(&mut client, repo, &Added::default(), Mode::Scan, deadline)
        .map_err(|e| e.to_string())?;
    let (_, prior) =
        observer::state_by_key(&mut client, &repo.scope()).map_err(|e| e.to_string())?;
    if prior.is_empty() {
        return Err("Unknown: no registered anchors".into());
    }
    // Registered files and symbol host files, from descriptors; the position
    // is encoded as the daemon encodes it (ADR 0012 §6).
    let mut files = BTreeSet::new();
    let mut hosts = BTreeSet::new();
    let mut versions = std::collections::BTreeMap::new();
    for (d, state) in prior.values() {
        match d {
            AnchorDescriptor::File(f) => {
                files.insert(f.path.clone());
            }
            AnchorDescriptor::Symbol(s) => {
                hosts.insert(s.path.clone());
                if state.base {
                    versions.insert(s.language, s.normaliser.clone());
                }
            }
        }
    }
    let paths: BTreeSet<_> = files.union(&hosts).cloned().collect();
    let hosts_only: BTreeSet<_> = hosts.difference(&files).cloned().collect();
    let mut captured = repo.capture(&paths, deadline).map_err(|e| e.to_string())?;
    captured.position = contextdb_anchors::capture::position(
        &repo.id,
        &captured.head,
        &captured.entries,
        &hosts_only,
        &versions,
    );
    let expected_head = unhex(tip.as_bytes()).map_err(|e| e.to_string())?;
    if captured.head != expected_head {
        return Err("Unknown: HEAD changed during range capture".into());
    }
    // Full raw pushed-tree agreement, including types, even if the range
    // changes only a subset of the registered anchors.
    for (path, observation) in &captured.entries {
        let tree = output(
            repo,
            &["ls-tree", "-z", &tip, "--", path],
            16 << 10,
            deadline,
        )?;
        let agrees = match observation {
            contextdb_anchors::Observation::Present(d) => {
                let (header, tree_path) = tree
                    .split(|b| *b == b'\t')
                    .collect::<Vec<_>>()
                    .split_first()
                    .and_then(|(h, t)| (t.len() == 1).then_some((*h, t[0])))
                    .ok_or("Unknown: tree entry unavailable")?;
                let text = std::str::from_utf8(header).map_err(|e| e.to_string())?;
                let fields: Vec<_> = text.split(' ').collect();
                fields.len() == 3
                    && matches!(fields[0], "100644" | "100755")
                    && fields[1] == "blob"
                    && unhex(fields[2].as_bytes()).map_err(|e| e.to_string())? == d.oid
                    && tree_path.strip_suffix(&[0]) == Some(path.as_bytes())
            }
            contextdb_anchors::Observation::Missing => tree.is_empty(),
            contextdb_anchors::Observation::Broken(_) => false,
        };
        if !agrees {
            return Err(format!(
                "Unknown: registered worktree differs from pushed commit at {path}"
            ));
        }
    }
    let base = if base.bytes().all(|b| b == b'0') && base.len() == 2 * repo.format.oid_len() {
        // Empty-tree OID can be computed without writing an object.
        let (ok, out) = git(
            &repo.root,
            &["hash-object", "-t", "tree", "--stdin"],
            &[],
            128,
            deadline,
        )
        .map_err(|e| e.to_string())?;
        if !ok {
            return Err("empty tree hash unavailable".into());
        }
        String::from_utf8(out)
            .map_err(|e| e.to_string())?
            .trim()
            .to_owned()
    } else {
        resolve(repo, base, deadline)?
    };
    let changed = output(
        repo,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            &base,
            &tip,
            "--",
        ],
        1 << 20,
        deadline,
    )?;
    let changed: Vec<_> = changed
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| {
            std::str::from_utf8(p)
                .map(str::to_owned)
                .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?;
    let mut admin = connect(dir, "admin.sock")?;
    let env = admin
        .call(&Request::CheckAnchors {
            scope: repo.scope(),
            paths: changed,
            expected_head,
            expected_position: captured.position,
        })
        .map_err(|e| format!("{e:?}"))?;
    if let Response::Check { validation, .. } = env.response {
        println!(
            "point-in-time range check epoch={} verdict={validation:?}; strict window starts now",
            env.epoch.0
        );
        Ok(
            if matches!(
                validation,
                contextdb_model::Validation::Valid
                    | contextdb_model::Validation::ValidRecertified { .. }
            ) {
                0
            } else {
                1
            },
        )
    } else {
        Err(format!("{:?}", env.response))
    }
}
pub fn pre_push(dir: &Path, root: &Path) -> Result<u8, String> {
    let repo = Repo::open(root, Instant::now() + OBSERVER_TIMEOUT).map_err(|e| e.to_string())?;
    let mut code = 0;
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() != 4 {
            return Err("invalid pre-push ref pair".into());
        }
        for oid in [parts[1], parts[3]] {
            if unhex(oid.as_bytes()).map_err(|e| e.to_string())?.len() != repo.format.oid_len() {
                return Err("invalid pre-push OID".into());
            }
        }
        if parts[1].bytes().all(|b| b == b'0') {
            continue;
        }
        if parts[1] != resolve(&repo, "HEAD", Instant::now() + OBSERVER_TIMEOUT)? {
            return Err(
                "Unknown: non-deletion local OID must equal current HEAD (tags are not peeled)"
                    .into(),
            );
        }
        code = code.max(check_range(dir, &repo, parts[3], parts[1])?);
    }
    Ok(code)
}
fn hooks(dir: &Path, repo: &Repo, args: &[String]) -> Result<u8, String> {
    let install = match args.first().map(String::as_str) {
        Some("install") => true,
        Some("uninstall") => false,
        _ => return Err("anchors hooks <dir> <repo> install|uninstall [--pre-push]".into()),
    };
    let hook_dir = output(
        repo,
        &["rev-parse", "--git-path", "hooks"],
        16 << 10,
        Instant::now() + OBSERVER_TIMEOUT,
    )?;
    let hook_dir = PathBuf::from(
        String::from_utf8(hook_dir)
            .map_err(|e| e.to_string())?
            .trim(),
    );
    let hook_dir = if hook_dir.is_absolute() {
        hook_dir
    } else {
        repo.root.join(hook_dir)
    };
    std::fs::create_dir_all(&hook_dir).map_err(|e| e.to_string())?;
    let _lock = stable_lock(&hook_dir.join(".contextdb-install.lock"))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let quote = |p: &Path| crate::p4::shell_quote(p.to_str().ok_or("non-UTF8 hook path")?);
    for name in ["post-commit", "post-checkout", "post-merge", "pre-push"] {
        if name == "pre-push" && install && !args.iter().any(|a| a == "--pre-push") {
            continue;
        }
        let path = hook_dir.join(name);
        let backup = hook_dir.join(format!("{name}.contextdb-original"));
        let existing = std::fs::read(&path).ok();
        let ours = existing
            .as_ref()
            .is_some_and(|b| b.starts_with(b"#!/bin/sh\n# ContextDB P5a\n"));
        if !install {
            if ours {
                std::fs::remove_file(&path).map_err(|e| e.to_string())?;
                if backup.exists() {
                    std::fs::rename(&backup, &path).map_err(|e| e.to_string())?;
                }
            }
            continue;
        }
        let command = if name == "pre-push" {
            format!(
                "{} anchors-pre-push {} {}",
                quote(&exe)?,
                quote(&dir.canonicalize().map_err(|e| e.to_string())?)?,
                quote(&repo.root)?
            )
        } else {
            format!(
                "{} anchors scan {} {}",
                quote(&exe)?,
                quote(&dir.canonicalize().map_err(|e| e.to_string())?)?,
                quote(&repo.root)?
            )
        };
        // stdin is buffered once for pre-push so both the previous hook and
        // ContextDB receive every ref pair. No ignored exit status.
        let script = if name == "pre-push" {
            format!(
                "#!/bin/sh\n# ContextDB P5a\ninput=$(mktemp) || exit 1\ntrap 'rm -f \"$input\"' EXIT HUP INT TERM\ncat > \"$input\" || exit 1\nif [ -x {} ]; then {} \"$@\" < \"$input\" || exit $?; fi\n{command} < \"$input\"\n",
                quote(&backup)?,
                quote(&backup)?
            )
        } else {
            format!(
                // Git reports post-checkout's status as the checkout's own;
                // a failed scan (ctxd down) only delays freshness, so it must
                // not fail the checkout. The original hook's status is kept.
                "#!/bin/sh\n# ContextDB P5a\nif [ -x {} ]; then {} \"$@\" || exit $?; fi\n{command} || true\n",
                quote(&backup)?,
                quote(&backup)?
            )
        };
        let tmp = hook_dir.join(format!(".{name}.contextdb-tmp"));
        std::fs::write(&tmp, script).map_err(|e| e.to_string())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        std::fs::File::open(&tmp)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        if existing.is_some() && !ours {
            if backup.exists() {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("backup exists: {}", backup.display()));
            }
            // Hard-link the original before atomic replacement: interruption
            // never leaves the active hook missing. Keep its original mode.
            std::fs::hard_link(&path, &backup).map_err(|e| e.to_string())?;
            std::fs::File::open(&hook_dir)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        std::fs::File::open(&hook_dir)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    Ok(0)
}
