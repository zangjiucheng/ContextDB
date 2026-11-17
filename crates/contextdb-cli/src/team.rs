//! `ctx team` (ADR 0018 §7–9): create and join a team, show it, sync it,
//! and publish its queued proposals. Everything that reads or writes the
//! store is a request to `ctxd`; this process only runs git, signs with the
//! bound key and pushes, in the developer's environment (§7.1). A join
//! builds the replica here once, before `ctxd` opens the team.
//!
//! ```text
//! ctx team create  <dir> --repo <work tree> --key <path> --member <name>
//!                  [--remote origin] [--lane main]…
//! ctx team join    <dir> --repo <work tree> --genesis <oid> --key <path>
//!                  [--remote origin]
//! ctx team status  <dir> [--team <hex>]
//! ctx team sync    <dir> [--team <hex>]
//! ctx team publish <dir> [--team <hex>] [--yes <token>] [--confirm]
//! ```

use crate::p4::{call, connect, flag};
use contextdb_daemon::team::{TEAMS_DIR, TeamConfig};
use contextdb_proto::proto::*;
use contextdb_team::commit::{self, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::git::Repo;
use contextdb_team::ops::{TeamBatch, TeamOp};
use contextdb_team::store::{Status, TeamStore};
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "ctx team create|join|status|sync|publish <dir> …";
/// The fetch refspec a join adds (§7.5).
fn refspec(remote: &str) -> String {
    format!("+refs/contextdb/team/*:refs/contextdb/remotes/{remote}/team/*")
}

fn flags<'a>(args: &'a [String], name: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|w| w[0] == name)
        .map(|w| w[1].as_str())
        .collect()
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The team's directory under the data directory.
fn team_dir(data: &Path, team: &[u8; 16]) -> PathBuf {
    data.join(TEAMS_DIR).join(hex(team))
}

/// `--team <hex>`, or the only joined team.
fn pick(data: &Path, args: &[String]) -> Result<[u8; 16], String> {
    if let Some(h) = flag(args, "--team") {
        return commit::parse_id128(h).ok_or_else(|| format!("--team {h}: not a team id"));
    }
    let mut ids: Vec<[u8; 16]> = std::fs::read_dir(data.join(TEAMS_DIR))
        .map(|d| {
            d.flatten()
                .filter_map(|e| commit::parse_id128(e.file_name().to_str()?))
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    match ids.as_slice() {
        [one] => Ok(*one),
        [] => Err("no team is joined".into()),
        _ => Err("several teams are joined: name one with --team".into()),
    }
}

/// A field of the team's `anchor` file, read without opening the replica
/// (`ctxd` owns it).
fn anchor_field(dir: &Path, key: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(dir.join("anchor")).map_err(|e| e.to_string())?;
    text.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix(' ')))
        .map(str::to_owned)
        .ok_or_else(|| format!("anchor: no {key}"))
}

pub fn run(data: &Path, args: &[String]) -> Result<u8, String> {
    match args.first().map(String::as_str) {
        Some("create") => create(data, &args[1..]),
        Some("join") => {
            let repo = PathBuf::from(flag(args, "--repo").ok_or("--repo <work tree>")?);
            let genesis = flag(args, "--genesis").ok_or("--genesis <oid> (from an admin)")?;
            let key = PathBuf::from(flag(args, "--key").ok_or("--key <ssh key path>")?);
            let remote = flag(args, "--remote").unwrap_or("origin");
            join(data, &repo, genesis, &key, remote)
        }
        Some("status") => {
            let team = flag(args, "--team").map(|_| pick(data, args)).transpose()?;
            let mut c = connect(data, "admin.sock")?;
            lines(call(&mut c, Request::TeamStatus { team })?.response)
        }
        Some("sync") => {
            let team = flag(args, "--team").map(|_| pick(data, args)).transpose()?;
            let mut c = connect(data, "admin.sock")?;
            lines(call(&mut c, Request::TeamSync { team })?.response)
        }
        // Detached from `reference-transaction` in `committed` (§8.1 step 2):
        // `fetch-missing <dir> <remote> <repo> <team hex> <old>:<new>…`.
        Some("fetch-missing") => {
            let (Some(remote), Some(repo), Some(team)) = (args.get(1), args.get(2), args.get(3))
            else {
                return Err("fetch-missing <dir> <remote> <repo> <team> <old>:<new>…".into());
            };
            let team = commit::parse_id128(team).ok_or("not a team id")?;
            let ranges: Vec<(&str, &str)> =
                args[4..].iter().filter_map(|r| r.split_once(':')).collect();
            let tips: Vec<&str> = ranges.iter().map(|(_, n)| *n).collect();
            let not: Vec<&str> = ranges
                .iter()
                .map(|(o, _)| *o)
                .filter(|o| !o.bytes().all(|b| b == b'0'))
                .collect();
            let n = fetch_missing(Path::new(repo), remote, &tips, &not)?;
            if n > 0
                && let Some(mut c) = connect_within(data, std::time::Duration::from_secs(5))
            {
                let _ = c.call(&Request::TeamNotify { team });
            }
            Ok(0)
        }
        Some("leave") => {
            let team = pick(data, args)?;
            if !args.iter().any(|a| a == "--yes") {
                println!(
                    "leaving team {} retracts every mirror's base on every open branch and \
                     deletes the replica and the proposal queue; confirm with --yes",
                    hex(&team)
                );
                return Ok(1);
            }
            leave(data, team)
        }
        Some("publish") => {
            let team = pick(data, args)?;
            let yes = match flag(args, "--yes") {
                None => None,
                Some(t) => Some(parse_token(t)?),
            };
            publish(data, team, yes, args.iter().any(|a| a == "--confirm"))
        }
        _ => Err(USAGE.into()),
    }
}

fn lines(r: Response) -> Result<u8, String> {
    match r {
        Response::TeamLines { lines } => {
            for l in lines {
                println!("{l}");
            }
            Ok(0)
        }
        other => Err(format!("unexpected response: {other:?}")),
    }
}

fn parse_token(t: &str) -> Result<[u8; 32], String> {
    if t.len() != 64 || !t.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("--yes {t}: not a listing token"));
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&t[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Signs `payload` with `key`, writes the commit object, returns its id.
fn sign_commit(repo: &Repo, key: &Path, payload: &[u8]) -> Result<String, String> {
    let armored = contextdb_team::sign::sign(key, payload)?;
    let raw = commit::with_signature(payload, &armored, repo.format());
    repo.write_commit(&raw).map_err(|e| format!("{e:?}"))
}

/// Pushes `oid` to `refname` without running hooks (§7.2 step 8); `Ok(false)`
/// for a non-fast-forward refusal.
fn push(dir: &Path, remote: &str, oid: &str, refname: &str) -> Result<bool, String> {
    let out = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "push",
            "--porcelain",
            remote,
        ])
        .arg(format!("{oid}:{refname}"))
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git push: {e}"))?;
    if out.status.success() {
        return Ok(true);
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if text.contains("non-fast-forward")
        || text.contains("fetch first")
        || text.contains("[rejected]")
    {
        return Ok(false);
    }
    Err(format!("git push: {}", text.trim()))
}

fn create(data: &Path, args: &[String]) -> Result<u8, String> {
    let repo_dir = PathBuf::from(flag(args, "--repo").ok_or("--repo <work tree>")?);
    let key = PathBuf::from(flag(args, "--key").ok_or("--key <ssh key path>")?);
    let member = flag(args, "--member").ok_or("--member <name>")?;
    let remote = flag(args, "--remote").unwrap_or("origin");
    let mut lanes = flags(args, "--lane");
    if lanes.is_empty() {
        lanes.push("main");
    }
    let public = contextdb_team::sign::public_key(&key)?;
    let team = contextdb_team::proposals::fresh_id();
    let mut src = format!(
        "version = 1\naccept_version = 1\nid = \"{}\"\nadmins = [\"{member}\"]\n\
         [members.{member}]\nkeys = [\"{}\"]\n",
        hex(&team),
        public.to_line()
    );
    for l in &lanes {
        src.push_str(&format!(
            "[lanes.{l}]\nwriters = [\"*members\"]\ncurators = [\"*admins\"]\n"
        ));
    }
    let doc = TeamDoc::from_toml(&src).map_err(|e| format!("team document: {e:?}"))?;
    let repo = Repo::open(&repo_dir).map_err(|e| format!("{e:?}"))?;
    let batch = TeamBatch {
        proposals: vec![vec![TeamOp::SetTeamAccess { doc: Box::new(doc) }]],
    }
    .encode();
    let trailers: Vec<(&str, String)> = vec![
        ("Ctx-Team", hex(&team)),
        ("Ctx-Team-Seq", "0".into()),
        ("Ctx-Author", member.into()),
        ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
        ("Ctx-Accept-Version", "1".into()),
        ("Ctx-Proposal", hex(&contextdb_team::proposals::fresh_id())),
    ];
    let tree = repo
        .write_tree(&[("batch".to_owned(), batch)])
        .map_err(|e| format!("{e:?}"))?;
    let payload = commit::build(&tree, &[], "ctx team: genesis", &trailers);
    let oid = sign_commit(&repo, &key, &payload)?;
    let refname = format!("refs/contextdb/team/{}/g0", hex(&team));
    if !push(&repo_dir, remote, &oid, &refname)? {
        return Err(format!("{refname} already exists on {remote}"));
    }
    println!("team {} created; genesis {oid}", hex(&team));
    println!(
        "members join with: ctx team join <dir> --repo <work tree> --genesis {oid} --key <their key>"
    );
    join(data, &repo_dir, &oid, &key, remote)
}

fn join(
    data: &Path,
    repo_dir: &Path,
    genesis: &str,
    key: &Path,
    remote: &str,
) -> Result<u8, String> {
    let public = contextdb_team::sign::public_key(key)?;
    let repo = Repo::open(repo_dir).map_err(|e| format!("{e:?}"))?;
    let root = repo
        .dir()
        .canonicalize()
        .map_err(|e| format!("{}: {e}", repo.dir().display()))?;
    // The refspec, then a fetch in this environment (credentials).
    let spec = refspec(remote);
    let have = git(
        repo_dir,
        &["config", "--get-all", &format!("remote.{remote}.fetch")],
    )
    .unwrap_or_default();
    if !have.lines().any(|l| l.trim() == spec) {
        git(
            repo_dir,
            &["config", "--add", &format!("remote.{remote}.fetch"), &spec],
        )?;
    }
    git(repo_dir, &["fetch", "-q", remote])?;
    // A partial clone holds the genesis commit, not its tree's blobs.
    fetch_missing(repo_dir, remote, &[genesis], &[])?;
    // The team id is the genesis entry's, which came out of band.
    let objs = repo.read_entry(genesis).map_err(|e| format!("{e:?}"))?;
    let c = commit::parse(&objs.commit, repo.format()).map_err(|e| format!("genesis: {e:?}"))?;
    let team = c
        .trailers
        .team
        .as_deref()
        .and_then(commit::parse_id128)
        .ok_or("the genesis entry names no team")?;
    let dir = team_dir(data, &team);
    if dir.exists() {
        return Err(format!("team {} is already joined", hex(&team)));
    }
    let built = (|| -> Result<String, String> {
        let mut s = TeamStore::create(&dir, team, repo.format(), remote, genesis)
            .map_err(|e| format!("{e:?}"))?;
        // A partial clone: the entries' blobs by OID first (§8.1, Q13).
        let mut attempts = 0;
        loop {
            match s.import(&repo).map_err(|e| format!("{e:?}"))? {
                Status::Imported(_) | Status::UpToDate => break,
                Status::Incomplete(missing) if attempts < 3 => {
                    attempts += 1;
                    let mut args = vec!["fetch", "-q", remote];
                    args.extend(missing.iter().map(String::as_str));
                    git(repo_dir, &args)?;
                }
                other => return Err(format!("import: {other:?}")),
            }
        }
        let doc = s.replica().doc().ok_or("the chain has no team document")?;
        let member = doc
            .member_of(&public)
            .ok_or_else(|| format!("NotTeamMember: {} is not listed", key.display()))?
            .to_owned();
        TeamConfig::new(root, member.clone(), key.display().to_string())
            .save(&dir)
            .map_err(|e| e.to_string())?;
        Ok(member)
    })();
    let member = match built {
        Ok(m) => m,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    println!("joined team {} as {member}", hex(&team));
    // The hooks: announce, notify, publish (§7.5).
    let hooks = crate::githooks::hooks_dir(repo_dir)?;
    crate::githooks::register(
        &hooks,
        crate::githooks::Component {
            name: "team".into(),
            data: data.canonicalize().map_err(|e| e.to_string())?,
            repo: None,
        },
        false,
    )?;
    // A running ctxd opens it now; otherwise at its next start.
    match connect(data, "admin.sock") {
        Ok(mut c) => {
            lines(call(&mut c, Request::TeamOpen { team })?.response)?;
        }
        Err(_) => println!("ctxd is not running: it opens the team at its next start"),
    }
    Ok(0)
}

/// The publish loop of §7.2 steps 5–10, by hand (`ctx team publish`) or
/// from `pre-push`: at most 3 attempts.
pub fn publish(
    data: &Path,
    team: [u8; 16],
    yes: Option<[u8; 32]>,
    confirm: bool,
) -> Result<u8, String> {
    let dir = team_dir(data, &team);
    let cfg = TeamConfig::load(&dir)?;
    let remote = anchor_field(&dir, "remote")?;
    let repo = Repo::open(&cfg.repo).map_err(|e| format!("{e:?}"))?;
    let mut c = connect(data, "admin.sock")?;
    for _ in 0..3 {
        // A fetch and import first: the entry's parent is the tip.
        git(&cfg.repo, &["fetch", "-q", &remote])?;
        call(&mut c, Request::TeamSync { team: Some(team) })?;
        let r = call(&mut c, Request::TeamPublishPrepare { team, yes, confirm })?.response;
        let Response::TeamPrepared {
            entry,
            listed,
            conflicted,
        } = r
        else {
            return Err(format!("unexpected response: {r:?}"));
        };
        for (id, reason) in &conflicted {
            println!("proposal {} conflicted: {reason}", hex(id));
        }
        if let Some((token, who)) = listed {
            println!(
                "not published: confirm with `ctx team publish --yes {}`",
                hex(&token)
            );
            for (id, author) in who {
                println!("  proposal {} by principal {}", hex(&id), author.0);
            }
            return Ok(0);
        }
        let Some(e) = entry else {
            println!("nothing to publish");
            return Ok(0);
        };
        let oid = sign_commit(&repo, Path::new(&cfg.key), &e.payload)?;
        match call(
            &mut c,
            Request::TeamPublishBegin {
                team,
                commit: oid.clone(),
            },
        ) {
            Ok(_) => {}
            // The policy or the queue moved since step 5: again.
            Err(e) if e.starts_with("PolicyChanged") || e.starts_with("QueueChanged") => continue,
            Err(e) => return Err(e),
        }
        match push(&cfg.repo, &remote, &oid, &e.refname) {
            Ok(true) => {
                git(&cfg.repo, &["fetch", "-q", &remote])?;
                call(&mut c, Request::TeamSync { team: Some(team) })?;
                println!("published {} proposal(s) in {oid}", e.proposals.len());
                return lines(call(&mut c, Request::TeamStatus { team: Some(team) })?.response);
            }
            Ok(false) => {
                call(&mut c, Request::TeamPublishRefused { team })?;
            }
            Err(err) => {
                // The outcome is unknown: `published` until a fetch shows
                // whether the chain holds it (§7.4).
                return Err(err);
            }
        }
    }
    Err("the team ref kept moving: not published after 3 attempts".into())
}

// ----- the team component of the hook dispatcher (§7.2, §7.5, §8.1, §8.3) -----

/// Every joined team of `data`: `(id, its directory)`.
fn joined(data: &Path) -> Vec<([u8; 16], PathBuf)> {
    let mut v: Vec<([u8; 16], PathBuf)> = std::fs::read_dir(data.join(TEAMS_DIR))
        .map(|d| {
            d.flatten()
                .filter_map(|e| Some((commit::parse_id128(e.file_name().to_str()?)?, e.path())))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// The admin socket of a running `ctxd`, with every read and write bounded
/// by `timeout`; `None` when no `ctxd` runs for `data`.
fn connect_within(
    data: &Path,
    timeout: std::time::Duration,
) -> Option<contextdb_proto::client::Client> {
    let l = contextdb_proto::lease::read_lease(&data.join("ctxd.lease"))
        .filter(contextdb_proto::lease::Lease::is_live)?;
    let path = l.sockets.iter().find(|s| s.ends_with("admin.sock"))?;
    contextdb_proto::client::Client::connect_timeout(path, timeout).ok()
}

/// `refs/contextdb/remotes/<remote>/team/<hex>/g<n>` → the team id.
fn tracking_team(refname: &str) -> Option<[u8; 16]> {
    let rest = refname.strip_prefix("refs/contextdb/remotes/")?;
    let (_, rest) = rest.split_once("/team/")?;
    let (hex, generation) = rest.split_once('/')?;
    generation.strip_prefix('g')?.parse::<u32>().ok()?;
    commit::parse_id128(hex)
}

/// `reference-transaction` (§8.1, §8.3): in `prepared` announce every new
/// team OID, durably and then to `ctxd`; in `aborted` withdraw it; in
/// `committed` ask for an import. `Err` only when an announce cannot be
/// made durable, or `ctxd` accepted it but did not acknowledge in time.
pub fn reference_transaction(data: &Path, state: &str, input: &str) -> Result<(), String> {
    let teams = joined(data);
    let zero = |o: &str| o.bytes().all(|b| b == b'0');
    let moves: Vec<([u8; 16], &Path, &str, &str)> = input
        .lines()
        .filter_map(|l| {
            let mut p = l.split(' ');
            let (_old, new, refname) = (p.next()?, p.next()?, p.next()?);
            let team = tracking_team(refname)?;
            let dir = teams.iter().find(|(t, _)| *t == team)?.1.as_path();
            (!zero(new)).then_some((team, dir, refname, new))
        })
        .collect();
    if moves.is_empty() {
        return Ok(());
    }
    // The git process whose transaction this is.
    let pid = nix::unistd::getppid().as_raw() as u32;
    match state {
        "prepared" => {
            for (_, dir, refname, oid) in &moves {
                contextdb_daemon::team::append_announce(
                    dir,
                    &contextdb_daemon::team::Announce {
                        refname: (*refname).to_owned(),
                        oid: (*oid).to_owned(),
                        pid,
                    },
                )
                .map_err(|e| format!("team announce not durable: {e}"))?;
            }
            // No ctxd listening: the file alone is the fence.
            let Some(mut c) = connect_within(data, std::time::Duration::from_millis(500)) else {
                return Ok(());
            };
            for (team, _, refname, oid) in moves {
                let r = c
                    .call(&Request::TeamAnnounce {
                        team,
                        refname: refname.to_owned(),
                        oid: oid.to_owned(),
                        pid,
                    })
                    .map_err(|e| format!("ctxd did not acknowledge the team announce: {e:?}"))?;
                if let Response::Error { code, detail } = r.response {
                    return Err(format!("team announce refused: {code}: {detail}"));
                }
            }
            Ok(())
        }
        "aborted" => {
            if let Some(mut c) = connect_within(data, std::time::Duration::from_millis(500)) {
                for (team, _, refname, oid) in moves {
                    let _ = c.call(&Request::TeamWithdraw {
                        team,
                        refname: refname.to_owned(),
                        oid: oid.to_owned(),
                    });
                }
            }
            Ok(())
        }
        "committed" => {
            if let Some(mut c) = connect_within(data, std::time::Duration::from_millis(500)) {
                let mut seen = std::collections::BTreeSet::new();
                for (team, ..) in &moves {
                    if seen.insert(*team) {
                        let _ = c.call(&Request::TeamNotify { team: *team });
                    }
                }
            }
            // A partial clone has the commits, not their blobs: fetch them
            // by OID, detached, then notify again.
            let olds: std::collections::BTreeMap<&str, &str> = input
                .lines()
                .filter_map(|l| {
                    let mut p = l.split(' ');
                    let (old, _new, r) = (p.next()?, p.next()?, p.next()?);
                    Some((r, old))
                })
                .collect();
            let mut by_team: std::collections::BTreeMap<[u8; 16], (&Path, Vec<String>, String)> =
                Default::default();
            for (team, dir, refname, oid) in &moves {
                let remote = refname
                    .strip_prefix("refs/contextdb/remotes/")
                    .and_then(|r| r.split_once('/'))
                    .map(|(r, _)| r.to_owned())
                    .unwrap_or_default();
                let old = olds.get(refname).copied().unwrap_or("0");
                let e = by_team.entry(*team).or_insert((dir, Vec::new(), remote));
                e.1.push(format!("{old}:{oid}"));
            }
            for (team, (dir, ranges, remote)) in by_team {
                if let Ok(cfg) = TeamConfig::load(dir) {
                    spawn_fetch_missing(data, &team, &cfg.repo, &remote, &ranges);
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `post-merge` (a pull): import every joined team now.
pub fn notify_all(data: &Path) {
    if let Some(mut c) = connect_within(data, std::time::Duration::from_millis(500)) {
        for (team, _) in joined(data) {
            let _ = c.call(&Request::TeamNotify { team });
        }
    }
}

/// `pre-push` refuses pushes that delete a team ref or upload tracking refs
/// (§7.2 step 3).
pub fn refuse_team_refs(_data: &Path, input: &str) -> Result<(), String> {
    for line in input.lines() {
        let p: Vec<&str> = line.split(' ').collect();
        let [_local, local_oid, remote_ref, _] = p.as_slice() else {
            continue;
        };
        if remote_ref.starts_with("refs/contextdb/remotes/") {
            return Err(format!(
                "refusing to push {remote_ref}: tracking refs stay local"
            ));
        }
        if remote_ref.starts_with("refs/contextdb/team/") && local_oid.bytes().all(|b| b == b'0') {
            return Err(format!(
                "refusing to delete {remote_ref}: the team chain is append-only"
            ));
        }
    }
    Ok(())
}

/// Runs `git` with a deadline; `false` on failure or timeout (killed).
fn git_within(dir: &Path, args: &[&str], deadline: std::time::Instant) -> bool {
    let Ok(mut child) = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return s.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// `pre-push` step 1: a bounded team sync — fetch, import, catch up — so
/// the gate does not see lag a sync would remove. Never fails the push.
pub fn bounded_sync(data: &Path, _push_remote: &str) {
    for (team, dir) in joined(data) {
        let (Ok(cfg), Ok(remote)) = (TeamConfig::load(&dir), anchor_field(&dir, "remote")) else {
            continue;
        };
        let deadline = std::time::Instant::now() + cfg.push_timeout;
        if !git_within(&cfg.repo, &["fetch", "-q", &remote], deadline) {
            eprintln!(
                "ContextDB: team {}: fetch did not finish in time",
                hex(&team)
            );
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if let Some(mut c) = connect_within(data, left.max(std::time::Duration::from_millis(100))) {
            let _ = c.call(&Request::TeamSync { team: Some(team) });
        }
    }
}

/// `pre-push` steps 3–10 for every team whose remote this push goes to.
/// Publishing is irrevocable: it is skipped, with a warning, for a dry run
/// or when the push's arguments cannot be read. A failure fails the code
/// push only with `strict_push` (O5).
pub fn pre_push_publish(data: &Path, remote: &str, url: &str) -> u8 {
    let mut code = 0;
    for (team, dir) in joined(data) {
        let (Ok(cfg), Ok(team_remote)) = (TeamConfig::load(&dir), anchor_field(&dir, "remote"))
        else {
            continue;
        };
        if team_remote != remote {
            continue;
        }
        let urls = git(&cfg.repo, &["remote", "get-url", "--all", remote]).unwrap_or_default()
            + &git(&cfg.repo, &["remote", "get-url", "--push", "--all", remote])
                .unwrap_or_default();
        if !urls.lines().any(|u| u.trim() == url) {
            continue;
        }
        match crate::githooks::push_args()
            .as_deref()
            .map(crate::githooks::is_dry_run)
        {
            Some(Some(false)) => {}
            Some(Some(true)) => {
                println!("ContextDB: dry run: team proposals not published");
                continue;
            }
            _ => {
                eprintln!(
                    "ContextDB: could not read the push's arguments: team proposals not published \
                     (run `ctx team publish`)"
                );
                continue;
            }
        }
        // Step 4: the publish lock; the kernel releases a dead holder's.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("publish.lock"));
        let Ok(lock) = lock else { continue };
        let Ok(_held) = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusiveNonblock)
        else {
            eprintln!("ContextDB: team {}: another publish is running", hex(&team));
            continue;
        };
        if let Err(e) = publish(data, team, None, false) {
            eprintln!("ContextDB: team {}: publish failed: {e}", hex(&team));
            if cfg.strict_push {
                code = 1;
            }
        }
    }
    code
}

/// `ctx team leave` (§8.5): `ctxd` records `leaving`, retracts and deletes;
/// then the fetch refspec, the tracking refs and (with no team left) the
/// hook component go.
fn leave(data: &Path, team: [u8; 16]) -> Result<u8, String> {
    let dir = team_dir(data, &team);
    let cfg = TeamConfig::load(&dir)?;
    let remote = anchor_field(&dir, "remote")?;
    let mut c = connect(data, "admin.sock")?;
    lines(call(&mut c, Request::TeamLeave { team })?.response)?;
    let prefix = format!("refs/contextdb/remotes/{remote}/team/{}/", hex(&team));
    for r in git(&cfg.repo, &["for-each-ref", "--format=%(refname)", &prefix])?.lines() {
        git(&cfg.repo, &["update-ref", "-d", r])?;
    }
    if joined(data).is_empty() {
        let _ = git(
            &cfg.repo,
            &[
                "config",
                "--fixed-value",
                "--unset-all",
                &format!("remote.{remote}.fetch"),
                &refspec(&remote),
            ],
        );
        crate::githooks::register(
            &crate::githooks::hooks_dir(&cfg.repo)?,
            crate::githooks::Component {
                name: "team".into(),
                data: data.canonicalize().map_err(|e| e.to_string())?,
                repo: None,
            },
            true,
        )?;
    }
    println!("left team {}", hex(&team));
    Ok(0)
}

/// The objects reachable from `tips` (and not from `not`) that a partial
/// clone does not hold, without fetching them lazily.
fn missing_objects(repo: &Path, tips: &[&str], not: &[&str]) -> Vec<String> {
    let mut args = vec!["rev-list", "--objects", "--missing=print"];
    args.extend(tips.iter().copied());
    let excluded: Vec<String> = not.iter().map(|o| format!("^{o}")).collect();
    args.extend(excluded.iter().map(String::as_str));
    let Ok(out) = Command::new("git")
        .args(&args)
        .current_dir(repo)
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('?'))
        .map(str::to_owned)
        .collect()
}

/// Fetches by OID, through the promisor remote, every object a partial
/// clone lacks below `tips` (§8.1 step 2, Q13). Returns how many.
pub fn fetch_missing(
    repo: &Path,
    remote: &str,
    tips: &[&str],
    not: &[&str],
) -> Result<usize, String> {
    let missing = missing_objects(repo, tips, not);
    if missing.is_empty() {
        return Ok(0);
    }
    let mut args = vec!["fetch", "-q", remote];
    args.extend(missing.iter().map(String::as_str));
    git(repo, &args)?;
    Ok(missing.len())
}

/// Starts `ctx team fetch-missing` detached from the hook: git does not
/// wait for it (§8.1 step 2).
fn spawn_fetch_missing(data: &Path, team: &[u8; 16], repo: &Path, remote: &str, ranges: &[String]) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .arg("team")
        .arg("fetch-missing")
        .arg(data)
        .arg(remote)
        .arg(repo)
        .arg(hex(team))
        .args(ranges)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
