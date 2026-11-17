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
        match s.import(&repo).map_err(|e| format!("{e:?}"))? {
            Status::Imported(_) | Status::UpToDate => {}
            other => return Err(format!("import: {other:?}")),
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
