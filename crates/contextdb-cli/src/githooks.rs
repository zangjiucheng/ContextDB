//! The git hook dispatcher (ADR 0018 §7.5): one ContextDB script per hook
//! name runs `ctx git-hook <name> <hooks dir> …`, which chains the user's
//! original hook (kept as `<name>.contextdb-original`) with the same
//! arguments and input, then runs the registered components in a fixed
//! order. The registry is `<hooks dir>/contextdb.components`, one
//! `<component> <data dir> [<repo root>]` per line:
//!
//! - `anchors` (P5a): `post-commit`, `post-checkout`, `post-merge` scan;
//! - `anchors-gate` (P5a): the `pre-push` gate;
//! - `team` (P10): `reference-transaction` (announce, withdraw, notify),
//!   `post-merge` (notify), `pre-push` (bounded sync, then publish).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const REGISTRY: &str = "contextdb.components";
const HEADER: &str = "#!/bin/sh\n# ContextDB dispatcher\n";
const LEGACY: &[u8] = b"#!/bin/sh\n# ContextDB P5a\n";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Component {
    pub name: String,
    pub data: PathBuf,
    pub repo: Option<PathBuf>,
}

fn hooks_of(component: &str) -> &'static [&'static str] {
    match component {
        "anchors" => &["post-commit", "post-checkout", "post-merge"],
        "anchors-gate" => &["pre-push"],
        "team" => &["reference-transaction", "post-merge", "pre-push"],
        _ => &[],
    }
}

/// Hooks git feeds on standard input.
fn takes_input(hook: &str) -> bool {
    matches!(hook, "pre-push" | "reference-transaction")
}

pub fn read_registry(hooks: &Path) -> Vec<Component> {
    let text = std::fs::read_to_string(hooks.join(REGISTRY)).unwrap_or_default();
    let mut out: Vec<Component> = text
        .lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, ' ');
            Some(Component {
                name: p.next()?.to_owned(),
                data: PathBuf::from(p.next()?),
                repo: p.next().map(PathBuf::from),
            })
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_file_name(format!(
        ".{}.contextdb-tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("hook")
    ));
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|e| e.to_string())?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    if let Some(d) = path.parent() {
        std::fs::File::open(d)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The hooks directory of the work tree `repo`.
pub fn hooks_dir(repo: &Path) -> Result<PathBuf, String> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .current_dir(repo)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Ok(if p.is_absolute() { p } else { repo.join(p) })
}

/// Registers (or with `remove`, unregisters) `component` and installs the
/// dispatcher for every hook a registered component uses; a hook no
/// component uses any more gets its original back.
pub fn register(hooks: &Path, component: Component, remove: bool) -> Result<(), String> {
    std::fs::create_dir_all(hooks).map_err(|e| e.to_string())?;
    let mut reg = read_registry(hooks);
    reg.retain(|c| !(c.name == component.name && c.data == component.data));
    if !remove {
        reg.push(component);
    }
    reg.sort();
    reg.dedup();
    let text: String = reg
        .iter()
        .map(|c| match &c.repo {
            Some(r) => format!("{} {} {}\n", c.name, c.data.display(), r.display()),
            None => format!("{} {}\n", c.name, c.data.display()),
        })
        .collect();
    write_atomic(&hooks.join(REGISTRY), text.as_bytes(), 0o644)?;
    let used: std::collections::BTreeSet<&str> = reg
        .iter()
        .flat_map(|c| hooks_of(&c.name).iter().copied())
        .collect();
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let quote = |p: &Path| crate::p4::shell_quote(p.to_str().ok_or("non-UTF8 hook path")?);
    for name in [
        "post-commit",
        "post-checkout",
        "post-merge",
        "pre-push",
        "reference-transaction",
    ] {
        let path = hooks.join(name);
        let backup = hooks.join(format!("{name}.contextdb-original"));
        let existing = std::fs::read(&path).ok();
        let ours = existing
            .as_ref()
            .is_some_and(|b| b.starts_with(HEADER.as_bytes()) || b.starts_with(LEGACY));
        if !used.contains(name) {
            if ours {
                std::fs::remove_file(&path).map_err(|e| e.to_string())?;
                if backup.exists() {
                    std::fs::rename(&backup, &path).map_err(|e| e.to_string())?;
                }
            }
            continue;
        }
        // `reference-transaction` runs for every ref update, three times per
        // transaction: the script itself chains the user's hook and starts
        // `ctx` only for a team tracking ref in a state that matters (G5).
        let script = if name == "reference-transaction" {
            format!(
                "{HEADER}input=$(cat)\n\
                 if [ -x {b} ]; then printf '%s\\n' \"$input\" | {b} \"$@\" || exit $?; fi\n\
                 case \"$1\" in prepared|aborted|committed) ;; *) exit 0 ;; esac\n\
                 case \"$input\" in *refs/contextdb/remotes/*) ;; *) exit 0 ;; esac\n\
                 printf '%s\\n' \"$input\" | exec {exe} git-hook {name} {hooks} --chained \"$@\"\n",
                b = quote(&backup)?,
                exe = quote(&exe)?,
                hooks = quote(hooks)?,
            )
        } else {
            format!(
                "{HEADER}exec {} git-hook {name} {} \"$@\"\n",
                quote(&exe)?,
                quote(hooks)?
            )
        };
        if existing.as_deref() == Some(script.as_bytes()) {
            continue;
        }
        if existing.is_some() && !ours {
            if backup.exists() {
                return Err(format!("backup exists: {}", backup.display()));
            }
            // Hard-link the original before the atomic replacement: an
            // interruption never leaves the active hook missing.
            std::fs::hard_link(&path, &backup).map_err(|e| e.to_string())?;
        }
        write_atomic(&path, script.as_bytes(), 0o755)?;
    }
    Ok(())
}

/// `ctx git-hook <name> <hooks dir> <args…>`: the dispatcher.
pub fn run(args: &[String]) -> Result<u8, String> {
    let (Some(name), Some(hooks)) = (args.first(), args.get(1)) else {
        return Err("git-hook <name> <hooks dir> …".into());
    };
    let hooks = Path::new(hooks);
    // `--chained`: the script already ran the user's hook (and it passed).
    let chained = args.get(2).is_some_and(|a| a == "--chained");
    let rest = &args[if chained { 3 } else { 2 }..];
    let mut input = Vec::new();
    if takes_input(name) {
        std::io::stdin()
            .read_to_end(&mut input)
            .map_err(|e| e.to_string())?;
    }
    // The user's hook first, with the same arguments and input.
    let backup = hooks.join(format!("{name}.contextdb-original"));
    let original = if !chained && is_executable(&backup) {
        let mut child = Command::new(&backup)
            .args(rest)
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        if let Some(mut s) = child.stdin.take() {
            let _ = s.write_all(&input);
        }
        child.wait().map_err(|e| e.to_string())?.code().unwrap_or(1)
    } else {
        0
    };
    let components = read_registry(hooks);
    let input = String::from_utf8_lossy(&input).into_owned();
    match name.as_str() {
        "post-commit" | "post-checkout" | "post-merge" => {
            // Never fail the operation: a failed scan only delays freshness.
            for c in &components {
                match (c.name.as_str(), &c.repo) {
                    ("anchors", Some(repo)) => {
                        let a = [
                            "scan".to_owned(),
                            c.data.display().to_string(),
                            repo.display().to_string(),
                        ];
                        let _ = crate::anchors::run(&a);
                    }
                    ("team", _) if name == "post-merge" => crate::team::notify_all(&c.data),
                    _ => {}
                }
            }
            Ok(u8::try_from(original).unwrap_or(1))
        }
        "reference-transaction" => {
            if original != 0 {
                return Ok(u8::try_from(original).unwrap_or(1));
            }
            let state = rest.first().map(String::as_str).unwrap_or("");
            for c in components.iter().filter(|c| c.name == "team") {
                if let Err(e) = crate::team::reference_transaction(&c.data, state, &input) {
                    // Only an announce that cannot be made durable vetoes.
                    eprintln!("ContextDB: {e}");
                    return Ok(1);
                }
            }
            Ok(0)
        }
        "pre-push" => {
            if original != 0 {
                return Ok(u8::try_from(original).unwrap_or(1));
            }
            let (remote, url) = (
                rest.first().map(String::as_str).unwrap_or(""),
                rest.get(1).map(String::as_str).unwrap_or(""),
            );
            let teams: Vec<&Component> = components.iter().filter(|c| c.name == "team").collect();
            for c in &teams {
                if let Err(e) = crate::team::refuse_team_refs(&c.data, &input) {
                    eprintln!("ContextDB: {e}");
                    return Ok(1);
                }
                // Step 1: a bounded sync, so the gate does not see lag a
                // sync would remove.
                crate::team::bounded_sync(&c.data, remote);
            }
            let mut gate = 0u8;
            for c in components.iter().filter(|c| c.name == "anchors-gate") {
                if let Some(repo) = &c.repo {
                    match crate::anchors::pre_push_input(&c.data, repo, &input) {
                        Ok(code) => gate = gate.max(code),
                        Err(e) => {
                            eprintln!("ContextDB gate: {e}");
                            gate = gate.max(1);
                        }
                    }
                }
            }
            if gate != 0 {
                return Ok(gate);
            }
            let mut code = 0u8;
            for c in &teams {
                code = code.max(crate::team::pre_push_publish(&c.data, remote, url));
            }
            Ok(code)
        }
        _ => Ok(u8::try_from(original).unwrap_or(1)),
    }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The `git push` arguments of the process that runs this hook, found by
/// walking up from the hook's parent (§7.2 step 3); `None` when they cannot
/// be read.
pub fn push_args() -> Option<Vec<String>> {
    let mut pid = nix::unistd::getppid().as_raw() as u32;
    for _ in 0..8 {
        let args = process_args(pid)?;
        let is_git = args
            .first()
            .is_some_and(|a| Path::new(a).file_name().is_some_and(|n| n == "git"));
        if is_git {
            return Some(args);
        }
        pid = parent_of(pid)?;
        if pid <= 1 {
            return None;
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn process_args(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid …`; comm may hold spaces and parentheses.
    let after = &stat[stat.rfind(')')? + 2..];
    after.split(' ').nth(1)?.parse().ok()
}

/// macOS: `ps` gives the command line joined by spaces (`unsafe` is
/// forbidden here, so no `KERN_PROCARGS2`): an argument with a space splits
/// in two. In git's push grammar that can only make a later word read as an
/// option (more dry runs, publish skipped), never hide `-n`.
#[cfg(target_os = "macos")]
fn process_args(pid: u32) -> Option<Vec<String>> {
    let out = Command::new("ps")
        .args(["-ww", "-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let args: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
    (!args.is_empty()).then_some(args)
}

#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<u32> {
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_args(_: u32) -> Option<Vec<String>> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn parent_of(_: u32) -> Option<u32> {
    None
}

/// Whether `git <args>` is a dry-run push, by git's option grammar:
/// `Some(true)` dry, `Some(false)` a real push, `None` when the command is
/// not literally `push` (an alias) or cannot be parsed (§7.2 step 3).
pub fn is_dry_run(args: &[String]) -> Option<bool> {
    // Global options before the subcommand; those taking a value.
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path"
            | "--config-env" | "--super-prefix" | "--list-cmds" => i += 2,
            _ if a.starts_with("--") || (a.starts_with('-') && a.len() > 1) => i += 1,
            _ => break,
        }
    }
    if args.get(i).map(String::as_str) != Some("push") {
        return None;
    }
    let mut dry = false;
    let mut j = i + 1;
    while j < args.len() {
        let a = args[j].as_str();
        if a == "--" {
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or("");
            if name == "no-dry-run" {
                dry = false;
            } else if name.len() >= 3 && "dry-run".starts_with(name) {
                // `--dry`, `--dry-r`, … : git's unique abbreviations.
                dry = true;
            } else if matches!(
                name,
                "repo" | "receive-pack" | "exec" | "push-option" | "signed"
            ) && !long.contains('=')
            {
                j += 1;
            }
        } else if let Some(short) = a.strip_prefix('-').filter(|s| !s.is_empty()) {
            for c in short.chars() {
                match c {
                    'n' => dry = true,
                    // `-o <option>`: the rest of the group, or the next
                    // argument, is its value.
                    'o' => {
                        if short.ends_with('o') {
                            j += 1;
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
        j += 1;
    }
    Some(dry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn dry_runs_are_recognised_by_git_grammar() {
        for (cmd, want) in [
            ("git push origin main", Some(false)),
            ("git push -n origin main", Some(true)),
            ("git push --dry-run origin", Some(true)),
            ("git push --dry origin", Some(true)),
            ("git push -nv origin", Some(true)),
            ("git push -vn origin", Some(true)),
            ("git push -n --no-dry-run origin", Some(false)),
            ("git push -- -n", Some(false)),
            ("git push -o n origin", Some(false)),
            ("git push -on origin", Some(false)),
            ("git -c core.x=1 -C /tmp push -n", Some(true)),
            ("git p -n", None),
            ("git --dry-run", None),
        ] {
            assert_eq!(is_dry_run(&args(cmd)), want, "{cmd}");
        }
    }

    #[test]
    fn the_registry_installs_chains_and_restores() {
        let dir = std::env::temp_dir().join(format!("ctx-githooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(dir.join("post-merge"), b"#!/bin/sh\necho theirs\n").unwrap();
        std::fs::set_permissions(
            dir.join("post-merge"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let team = Component {
            name: "team".into(),
            data: PathBuf::from("/data"),
            repo: None,
        };
        register(&dir, team.clone(), false).unwrap();
        let once = std::fs::read(dir.join("post-merge")).unwrap();
        assert!(once.starts_with(HEADER.as_bytes()));
        assert!(dir.join("post-merge.contextdb-original").exists());
        assert!(dir.join("reference-transaction").exists());
        assert!(!dir.join("post-commit").exists());
        register(&dir, team.clone(), false).unwrap();
        assert_eq!(std::fs::read(dir.join("post-merge")).unwrap(), once);
        assert_eq!(read_registry(&dir), vec![team.clone()]);
        register(&dir, team, true).unwrap();
        assert_eq!(
            std::fs::read(dir.join("post-merge")).unwrap(),
            b"#!/bin/sh\necho theirs\n"
        );
        assert!(!dir.join("reference-transaction").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
