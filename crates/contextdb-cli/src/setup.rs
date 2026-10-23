//! `ctx setup` and `ctx service`: one-command onboarding of a repository and a
//! per-data-dir user service that keeps `ctxd` running. Operational only: no
//! semantic change, and an existing `ctxd` that speaks another protocol is
//! never touched (e.g. a long-running dogfood daemon).
use contextdb_proto::{lease, proto::PROTOCOL_VERSION};
use serde_json::{Value, json};
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].as_str())
}
/// `ctxd`, `ctx-mcp` and `ctx-hook` installed next to this `ctx`.
fn sibling(name: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let path = exe.with_file_name(name);
    if path.is_file() {
        path.canonicalize().map_err(|e| e.to_string())
    } else {
        Err(format!(
            "{name} not found next to {}: install all four binaries together (scripts/install.sh)",
            exe.display()
        ))
    }
}
fn utf8(p: &Path) -> Result<&str, String> {
    p.to_str()
        .ok_or_else(|| format!("non-UTF8 path {}", p.display()))
}
/// Replace a file whole (tmp + rename), so an interrupted write never leaves
/// a truncated harness config behind.
fn replace(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("ctx-tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// A live `ctxd` for `dir` speaking this build's protocol, started detached
/// (own process group, output to `<dir>/ctxd.log`) if none is running.
fn ensure_daemon(dir: &Path, ctxd: &Path) -> Result<&'static str, String> {
    let live = || lease::read_lease(&dir.join("ctxd.lease")).filter(lease::Lease::is_live);
    if let Some(l) = live() {
        return if l.protocol == PROTOCOL_VERSION {
            Ok("already running")
        } else {
            Err(format!(
                "the ctxd serving {} speaks protocol {}, this ctx {}: stop it and start the matching build (not changed)",
                dir.display(),
                l.protocol,
                PROTOCOL_VERSION
            ))
        };
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ctxd.log"))
        .map_err(|e| e.to_string())?;
    Command::new(ctxd)
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("start ctxd: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if live().is_some_and(|l| l.protocol == PROTOCOL_VERSION) {
            return Ok("started");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "ctxd did not come up within 10 s; see {}",
        dir.join("ctxd.log").display()
    ))
}

/// The repository's `.git/info/exclude` (local, never committed); None
/// outside a Git repository.
fn exclude_file(project: &Path) -> Result<Option<PathBuf>, String> {
    let out = Command::new("git")
        .current_dir(project)
        .args(["rev-parse", "--git-path", "info/exclude"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Ok(None);
    }
    let file = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Ok(Some(if file.is_absolute() {
        file
    } else {
        project.join(file)
    }))
}
/// Keep the data dir out of Git when it lives inside the repository.
fn exclude(project: &Path, dir: &Path) -> Result<Option<String>, String> {
    let Ok(rel) = dir.strip_prefix(project) else {
        return Ok(None);
    };
    let Some(file) = exclude_file(project)? else {
        return Ok(None); // not a Git repository
    };
    append_pattern(&file, &format!("/{}/", utf8(rel)?))
}
/// The files `ctx setup` writes hold machine-specific absolute paths, so they
/// are kept out of Git: untracked ones are added to `.git/info/exclude`
/// (returned first); tracked ones cannot be excluded and are returned second
/// so the caller can warn.
fn exclude_generated(project: &Path, files: &[&str]) -> Result<(Vec<String>, Vec<String>), String> {
    let Some(exclude) = exclude_file(project)? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let (mut added, mut tracked) = (Vec::new(), Vec::new());
    for f in files {
        let is_tracked = Command::new("git")
            .current_dir(project)
            .args(["ls-files", "--error-unmatch", "--", f])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| e.to_string())?
            .success();
        if is_tracked {
            tracked.push((*f).to_owned());
        } else if let Some(p) = append_pattern(&exclude, &format!("/{f}"))? {
            added.push(p);
        }
    }
    Ok((added, tracked))
}
fn append_pattern(file: &Path, pattern: &str) -> Result<Option<String>, String> {
    let pattern = pattern.to_owned();
    let text = std::fs::read_to_string(file).unwrap_or_default();
    if text.lines().any(|l| l.trim() == pattern) {
        return Ok(None);
    }
    let mut text = text;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&pattern);
    text.push('\n');
    replace(file, &text)?;
    Ok(Some(pattern))
}

/// Claude Code project MCP config: only the `contextdb` server is written.
fn claude_mcp(project: &Path, mcp: &Path, dir: &Path) -> Result<&'static str, String> {
    let file = project.join(".mcp.json");
    let mut config: Value = match std::fs::read_to_string(&file) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: {e}", file.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    let entry = json!({ "command": utf8(mcp)?, "args": [utf8(dir)?] });
    let servers = config
        .as_object_mut()
        .ok_or(".mcp.json is not an object")?
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or(".mcp.json mcpServers is not an object")?;
    if servers.get("contextdb") == Some(&entry) {
        return Ok("unchanged");
    }
    let verb = if servers.contains_key("contextdb") {
        "updated"
    } else {
        "added"
    };
    servers.insert("contextdb".into(), entry);
    replace(
        &file,
        &(serde_json::to_string_pretty(&config).map_err(|e| e.to_string())? + "\n"),
    )?;
    Ok(verb)
}

/// Codex project MCP config: the `[mcp_servers.contextdb]` table is added or
/// rewritten in place; every other line (comments included) is kept.
fn codex_mcp(project: &Path, mcp: &Path, dir: &Path) -> Result<&'static str, String> {
    let file = project.join(".codex/config.toml");
    let text = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    text.parse::<toml::Table>()
        .map_err(|e| format!("{}: {e}", file.display()))?;
    let quote = |s: &str| serde_json::to_string(s).map_err(|e| e.to_string());
    let section = format!(
        "[mcp_servers.contextdb]\ncommand = {}\nargs = [{}]\n",
        quote(utf8(mcp)?)?,
        quote(utf8(dir)?)?
    );
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim() == "[mcp_servers.contextdb]");
    let out = match start {
        None => {
            let mut out = text.clone();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out + &section
        }
        Some(i) => {
            let end = lines[i + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map_or(lines.len(), |k| i + 1 + k);
            let old = lines[i..end].join("\n");
            if old.trim_end() == section.trim_end() {
                return Ok("unchanged");
            }
            let mut out: Vec<String> = lines[..i].iter().map(|l| l.to_string()).collect();
            out.push(section.trim_end().to_owned());
            if end < lines.len() {
                out.push(String::new());
            }
            out.extend(lines[end..].iter().map(|l| l.to_string()));
            out.join("\n") + "\n"
        }
    };
    out.parse::<toml::Table>()
        .map_err(|e| format!("generated .codex/config.toml does not parse: {e}"))?;
    replace(&file, &out)?;
    Ok(if start.is_some() { "updated" } else { "added" })
}

const SETUP_USAGE: &str =
    "usage: ctx setup [--project ROOT] [--dir DATA] [--harness claude|codex|both] [--no-start]";

/// `ctx setup`: data dir, daemon, MCP registration and hooks in one step.
pub fn setup(args: &[String]) -> Result<u8, String> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{SETUP_USAGE}");
        return Ok(0);
    }
    let project = match flag(args, "--project") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    }
    .canonicalize()
    .map_err(|e| format!("project: {e}"))?;
    let harnesses: &[&str] = match flag(args, "--harness").unwrap_or("both") {
        "claude" => &["claude"],
        "codex" => &["codex"],
        "both" => &["claude", "codex"],
        _ => return Err(SETUP_USAGE.into()),
    };
    let dir = flag(args, "--dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| project.join(".contextdb/db"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let dir = dir.canonicalize().map_err(|e| e.to_string())?;
    let (ctxd, mcp, hook) = (sibling("ctxd")?, sibling("ctx-mcp")?, sibling("ctx-hook")?);
    println!("ContextDB setup for {}", project.display());
    println!("  data dir   {}", dir.display());
    if let Some(p) = exclude(&project, &dir)? {
        println!("  git        added {p} to .git/info/exclude");
    }
    if args.iter().any(|a| a == "--no-start") {
        println!("  ctxd       not started (--no-start)");
    } else {
        println!("  ctxd       {}", ensure_daemon(&dir, &ctxd)?);
    }
    for h in harnesses {
        let verb = match *h {
            "claude" => claude_mcp(&project, &mcp, &dir)?,
            _ => codex_mcp(&project, &mcp, &dir)?,
        };
        let file = if *h == "claude" {
            ".mcp.json"
        } else {
            ".codex/config.toml"
        };
        println!("  {h:10} MCP server contextdb {verb} in {file}");
        let install: Vec<String> = [
            "install",
            "--harness",
            h,
            "--project",
            utf8(&project)?,
            "--dir",
            utf8(&dir)?,
            "--ctx-hook",
            utf8(&hook)?,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        crate::p4::hooks(&install)?;
    }
    let mut generated = vec![".contextdb/dir.wire"];
    if harnesses.contains(&"claude") {
        generated.extend([".mcp.json", ".claude/settings.local.json"]);
    }
    if harnesses.contains(&"codex") {
        generated.extend([".codex/config.toml", ".codex/hooks.json"]);
    }
    let (added, tracked) = exclude_generated(&project, &generated)?;
    if !added.is_empty() {
        println!(
            "  git        added {} to .git/info/exclude",
            added.join(" ")
        );
    }
    for f in &tracked {
        println!(
            "  warning    {f} is tracked by Git and now holds machine-specific paths; do not commit those lines"
        );
    }
    println!("\nNext:");
    if harnesses.contains(&"claude") {
        println!(
            "  - Claude Code: approve the project MCP server `contextdb` when asked on the next start."
        );
    }
    if harnesses.contains(&"codex") {
        println!("  - Codex: open /hooks (or accept the startup review) to trust the new hooks.");
    }
    println!(
        "  - Keep ctxd running across logins: ctx service install {}",
        dir.display()
    );
    println!("  - Default gating is warn; see README (Hooks 与 action gating) to tune it.");
    Ok(0)
}

/// Stable per-data-dir service name.
fn label(dir: &Path) -> Result<String, String> {
    let hash = blake3::hash(utf8(dir)?.as_bytes());
    Ok(format!("com.contextdb.ctxd.{}", &hash.to_hex()[..12]))
}
fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
/// Unit file for this platform and where it goes.
fn unit(home: &Path, dir: &Path, ctxd: &Path) -> Result<(PathBuf, String), String> {
    let name = label(dir)?;
    let log = dir.join("ctxd.log");
    if cfg!(target_os = "macos") {
        // KeepAlive only after a failure: `ctx shutdown` (exit 0) stays down.
        let text = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{name}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>{}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
            xml(utf8(ctxd)?),
            xml(utf8(dir)?),
            xml(utf8(&log)?),
            xml(utf8(&log)?)
        );
        Ok((
            home.join(format!("Library/LaunchAgents/{name}.plist")),
            text,
        ))
    } else {
        let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let text = format!(
            "[Unit]\nDescription=ContextDB daemon for {dir}\n\n[Service]\nExecStart={} {}\nRestart=on-failure\nRestartSec=30\nStandardOutput=append:{log}\nStandardError=append:{log}\n\n[Install]\nWantedBy=default.target\n",
            quote(utf8(ctxd)?),
            quote(utf8(dir)?),
            dir = utf8(dir)?,
            log = utf8(&log)?
        );
        Ok((
            home.join(format!(".config/systemd/user/{name}.service")),
            text,
        ))
    }
}
fn run(cmd: &str, args: &[&str]) -> Result<bool, String> {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .map_err(|e| format!("{cmd}: {e}"))
}
fn uid() -> Result<String, String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

const SERVICE_USAGE: &str = "usage: ctx service install|uninstall|restart|status <dir> [--no-load]";

/// `ctx service`: a user service (launchd on macOS, systemd --user elsewhere)
/// that starts `ctxd <dir>` at login and restarts it after a failure.
/// `--no-load` writes or removes the unit without asking the service manager.
pub fn service(args: &[String]) -> Result<u8, String> {
    let sub = args.first().map(String::as_str).ok_or(SERVICE_USAGE)?;
    let dir = PathBuf::from(args.get(1).ok_or(SERVICE_USAGE)?)
        .canonicalize()
        .map_err(|e| format!("data dir: {e}"))?;
    let load = !args.iter().any(|a| a == "--no-load");
    let name = label(&dir)?;
    let mac = cfg!(target_os = "macos");
    match sub {
        "install" => {
            let ctxd = sibling("ctxd")?;
            if let Some(l) =
                lease::read_lease(&dir.join("ctxd.lease")).filter(lease::Lease::is_live)
                && load
            {
                return Err(format!(
                    "a ctxd (pid {}) already serves {}: stop it first (ctx shutdown {}) so the service owns it",
                    l.pid,
                    dir.display(),
                    dir.display()
                ));
            }
            let (file, text) = unit(&home()?, &dir, &ctxd)?;
            replace(&file, &text)?;
            println!("wrote {}", file.display());
            if load {
                let ok = if mac {
                    let domain = format!("gui/{}", uid()?);
                    let _ = run("launchctl", &["bootout", &format!("{domain}/{name}")]);
                    run("launchctl", &["bootstrap", &domain, utf8(&file)?])?
                } else {
                    run("systemctl", &["--user", "daemon-reload"])?
                        && run(
                            "systemctl",
                            &["--user", "enable", "--now", &format!("{name}.service")],
                        )?
                };
                if !ok {
                    return Err(format!("the service manager refused {name}"));
                }
                println!("loaded {name}; ctxd now starts at login and restarts after a failure");
            }
        }
        "uninstall" => {
            let (file, _) = unit(&home()?, &dir, Path::new("ctxd"))?;
            if load {
                if mac {
                    let _ = run("launchctl", &["bootout", &format!("gui/{}/{name}", uid()?)]);
                } else {
                    let _ = run(
                        "systemctl",
                        &["--user", "disable", "--now", &format!("{name}.service")],
                    );
                }
            }
            match std::fs::remove_file(&file) {
                Ok(()) => println!("removed {}", file.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("no service for {}", dir.display())
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        "restart" => {
            // After an upgrade: the new ctxd binary is picked up by the
            // service manager; a ctxd not run by the service is left alone.
            let (file, _) = unit(&home()?, &dir, Path::new("ctxd"))?;
            if !file.exists() {
                return Err(format!(
                    "no service for {}: restart a manually started ctxd with ctx shutdown, then ctx setup",
                    dir.display()
                ));
            }
            let ok = if mac {
                run(
                    "launchctl",
                    &["kickstart", "-k", &format!("gui/{}/{name}", uid()?)],
                )?
            } else {
                run(
                    "systemctl",
                    &["--user", "restart", &format!("{name}.service")],
                )?
            };
            if !ok {
                return Err(format!("the service manager could not restart {name}"));
            }
            println!("restarted {name}");
        }
        "status" => {
            let (file, _) = unit(&home()?, &dir, Path::new("ctxd"))?;
            let lease = lease::read_lease(&dir.join("ctxd.lease")).filter(lease::Lease::is_live);
            println!(
                "service {name}: {}",
                if file.exists() {
                    "installed"
                } else {
                    "not installed"
                }
            );
            match lease {
                Some(l) => println!(
                    "ctxd: running (pid {}, protocol {}{})",
                    l.pid,
                    l.protocol,
                    if l.protocol == PROTOCOL_VERSION {
                        ""
                    } else {
                        ", differs from this ctx"
                    }
                ),
                None => println!("ctxd: not running"),
            }
        }
        _ => return Err(SERVICE_USAGE.into()),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ctx-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }
    #[test]
    fn claude_mcp_adds_updates_and_keeps_other_servers() {
        let p = temp("claude");
        std::fs::write(
            p.join(".mcp.json"),
            r#"{"mcpServers":{"other":{"command":"x"}},"keep":1}"#,
        )
        .unwrap();
        let (mcp, dir) = (Path::new("/bin/ctx-mcp"), Path::new("/data"));
        assert_eq!(claude_mcp(&p, mcp, dir).unwrap(), "added");
        assert_eq!(claude_mcp(&p, mcp, dir).unwrap(), "unchanged");
        assert_eq!(claude_mcp(&p, mcp, Path::new("/data2")).unwrap(), "updated");
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(p.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(v["keep"], 1);
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"]["contextdb"]["args"][0], "/data2");
        std::fs::write(p.join(".mcp.json"), "[1]").unwrap();
        assert!(claude_mcp(&p, mcp, dir).is_err(), "not an object: refused");
        let _ = std::fs::remove_dir_all(p);
    }
    #[test]
    fn codex_mcp_rewrites_only_its_table_and_keeps_comments() {
        let p = temp("codex");
        std::fs::create_dir_all(p.join(".codex")).unwrap();
        let file = p.join(".codex/config.toml");
        std::fs::write(
            &file,
            "# mine\nmodel = \"o\"\n[mcp_servers.other]\ncommand = \"x\"\n",
        )
        .unwrap();
        let mcp = Path::new("/bin/ctx-mcp");
        assert_eq!(codex_mcp(&p, mcp, Path::new("/a \"q\"")).unwrap(), "added");
        assert_eq!(
            codex_mcp(&p, mcp, Path::new("/a \"q\"")).unwrap(),
            "unchanged"
        );
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str("\n[profiles.x]\nmodel = \"y\"\n");
        std::fs::write(&file, text).unwrap();
        assert_eq!(codex_mcp(&p, mcp, Path::new("/b")).unwrap(), "updated");
        let text = std::fs::read_to_string(&file).unwrap();
        let t: toml::Table = text.parse().unwrap();
        assert_eq!(
            t["mcp_servers"]["contextdb"]["args"][0].as_str(),
            Some("/b")
        );
        assert_eq!(t["mcp_servers"]["other"]["command"].as_str(), Some("x"));
        assert_eq!(t["profiles"]["x"]["model"].as_str(), Some("y"));
        assert!(text.starts_with("# mine\n"));
        assert_eq!(text.matches("[mcp_servers.contextdb]").count(), 1);
        std::fs::write(&file, "not = [toml").unwrap();
        assert!(
            codex_mcp(&p, mcp, Path::new("/b")).is_err(),
            "invalid TOML untouched"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "not = [toml");
        let _ = std::fs::remove_dir_all(p);
    }
    #[test]
    fn exclude_adds_the_data_dir_once_and_only_inside_the_repo() {
        let p = temp("exclude");
        assert!(
            Command::new("git")
                .current_dir(&p)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        let dir = p.join(".contextdb/db");
        assert_eq!(
            exclude(&p, &dir).unwrap().as_deref(),
            Some("/.contextdb/db/")
        );
        assert_eq!(exclude(&p, &dir).unwrap(), None, "idempotent");
        assert_eq!(exclude(&p, Path::new("/elsewhere")).unwrap(), None);
        let text = std::fs::read_to_string(p.join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches("/.contextdb/db/").count(), 1);
        let _ = std::fs::remove_dir_all(p);
    }
    #[test]
    fn generated_files_are_excluded_unless_tracked() {
        let p = temp("generated");
        assert!(
            Command::new("git")
                .current_dir(&p)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(p.join(".mcp.json"), "{}").unwrap();
        assert!(
            Command::new("git")
                .current_dir(&p)
                .args(["add", ".mcp.json"])
                .status()
                .unwrap()
                .success()
        );
        let (added, tracked) = exclude_generated(
            &p,
            &[".mcp.json", ".codex/hooks.json", ".contextdb/dir.wire"],
        )
        .unwrap();
        assert_eq!(added, vec!["/.codex/hooks.json", "/.contextdb/dir.wire"]);
        assert_eq!(
            tracked,
            vec![".mcp.json"],
            "a tracked file cannot be excluded"
        );
        let (again, _) = exclude_generated(&p, &[".codex/hooks.json"]).unwrap();
        assert!(again.is_empty(), "idempotent");
        let _ = std::fs::remove_dir_all(p);
    }
    #[test]
    fn service_unit_restarts_only_after_failures() {
        let home = temp("home");
        let (file, text) = unit(&home, Path::new("/data <x>"), Path::new("/bin/ctxd")).unwrap();
        assert!(file.starts_with(&home));
        assert!(
            file.to_str()
                .unwrap()
                .contains(&label(Path::new("/data <x>")).unwrap())
        );
        if cfg!(target_os = "macos") {
            assert!(text.contains("<key>SuccessfulExit</key><false/>"));
            assert!(text.contains("/data &lt;x&gt;"), "XML-escaped");
        } else {
            assert!(text.contains("Restart=on-failure"));
        }
        assert_ne!(
            label(Path::new("/a")).unwrap(),
            label(Path::new("/b")).unwrap()
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
