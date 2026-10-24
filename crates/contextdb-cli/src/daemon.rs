//! `ctx` commands served by a running `ctxd` over its admin socket (SPEC §6.6,
//! ADR 0008 §8.3). Offline operator commands open the store directly and are
//! refused (`Locked`) while a daemon runs.
use contextdb_daemon::{lease, proto::*, server::Client};
use contextdb_model::*;
use std::{io::BufRead, path::Path};

fn connect(dir: &Path) -> Result<Client, String> {
    let l = lease::read_lease(&dir.join("ctxd.lease"))
        .filter(lease::Lease::is_live)
        .ok_or_else(|| format!("ctxd is not running for {}", dir.display()))?;
    let admin = l
        .sockets
        .iter()
        .find(|s| s.ends_with("admin.sock"))
        .ok_or("lease names no admin socket")?;
    Client::connect(admin).map_err(|e| format!("connect: {e}"))
}
fn call(c: &mut Client, r: Request) -> Result<Envelope, String> {
    let env = c.call(&r).map_err(|e| format!("{e:?}"))?;
    if let Response::Error { code, detail } = &env.response {
        return Err(format!("{code}: {detail}"));
    }
    Ok(env)
}
/// `key`, `key@rev`.
fn target(s: &str) -> Result<Target, String> {
    match s.rsplit_once('@') {
        Some((key, rev)) => Ok(Target::KeyRev {
            key: key.into(),
            rev: RevisionId(
                rev.trim_start_matches('r')
                    .parse()
                    .map_err(|_| format!("bad revision in {s}"))?,
            ),
        }),
        None => Ok(Target::Key(s.into())),
    }
}
/// Removes `--branch B` from `args`, returning B.
fn take_branch(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut rest = Vec::new();
    let mut branch = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--branch" {
            branch = Some(it.next().ok_or("--branch needs a name")?.clone());
        } else {
            rest.push(a.clone());
        }
    }
    Ok((branch, rest))
}
/// `r` on `branch` (protocol 4), or on `main`.
fn on(branch: &Option<String>, r: Request) -> Request {
    match branch {
        Some(b) => Request::OnBranch {
            branch: BranchSel::Named(b.clone()),
            request: Box::new(r),
        },
        None => r,
    }
}
pub fn run(cmd: &str, dir: &Path, args: &[String]) -> Result<(), String> {
    let (branch, args) = take_branch(args)?;
    let args = &args[..];
    let mut c = connect(dir)?;
    match cmd {
        "status" => {
            let env = call(&mut c, on(&branch, Request::Status))?;
            if let Response::Status {
                revisions,
                logicals,
                sessions,
            } = env.response
            {
                println!(
                    "epoch={} logicals={logicals} revisions={revisions} sessions={sessions}",
                    env.epoch.0
                );
            }
        }
        "why" | "why-not" | "impact" => {
            let t = target(args.first().ok_or("missing target")?)?;
            let r = match cmd {
                "why" => Request::Why {
                    target: t,
                    depth: 3,
                },
                "why-not" => Request::WhyNot {
                    target: t,
                    depth: 3,
                },
                _ => Request::Impact { target: t },
            };
            let env = call(&mut c, on(&branch, r))?;
            match env.response {
                Response::Why { proof } => println!("{proof:#?}"),
                Response::WhyNot { why_not } => println!("{why_not:#?}"),
                Response::Impact { impact } => println!("{impact:#?}"),
                other => println!("{other:?}"),
            }
        }
        "diff" => {
            let parse = |n: usize| -> Result<Epoch, String> {
                args.get(n)
                    .and_then(|v| v.parse().ok())
                    .map(Epoch)
                    .ok_or_else(|| "usage: ctx diff <dir> <from> <to>".into())
            };
            let env = call(
                &mut c,
                on(
                    &branch,
                    Request::Diff {
                        from: parse(0)?,
                        to: parse(1)?,
                    },
                ),
            )?;
            if let Response::Diff { diff } = env.response {
                for ch in diff.changes {
                    println!(
                        "r{}: {:?} -> {:?}",
                        ch.revision.0,
                        ch.before.map(|s| s.l2),
                        ch.after.map(|s| s.l2)
                    );
                }
            }
        }
        "validate" => validate(&mut c, args, &branch)?,
        "shutdown" => {
            call(&mut c, Request::Shutdown)?;
            println!("ctxd stopping");
        }
        other => return Err(format!("unknown command {other}")),
    }
    Ok(())
}
/// `ctx validate <dir> <key@rev> verified|rejected [--yes]`: shows every Sem
/// rule concluding the revision with its current vector, asks for
/// confirmation, then records exactly the vector shown (CurvecMoved if it
/// changed meanwhile).
fn validate(c: &mut Client, args: &[String], branch: &Option<String>) -> Result<(), String> {
    let t = target(
        args.first()
            .ok_or("usage: ctx validate <dir> <key@rev> verified|rejected [--yes]")?,
    )?;
    let verdict = match args.get(1).map(String::as_str) {
        Some("verified") => Verdict::Verified,
        Some("rejected") => Verdict::Rejected,
        _ => return Err("verdict must be verified or rejected".into()),
    };
    let yes = args.iter().any(|a| a == "--yes");
    let env = call(
        c,
        on(
            branch,
            Request::WhyNot {
                target: t,
                depth: 0,
            },
        ),
    )?;
    let Response::WhyNot { why_not } = env.response else {
        return Err("unexpected response".into());
    };
    let rules: Vec<_> = why_not
        .rules
        .iter()
        .filter(|r| !r.disabled && !r.asserted.is_empty())
        .collect();
    if rules.is_empty() {
        return Err("no enabled rule with a Sem incidence concludes this revision".into());
    }
    for r in &rules {
        println!(
            "rule {}: gate {:?}, asserted {:?}, current {:?}",
            r.rule.0, r.gate, r.asserted, r.current
        );
    }
    if !yes {
        print!("record {verdict:?} for these vectors? [y/N] ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if !line.trim().eq_ignore_ascii_case("y") {
            return Err("not confirmed".into());
        }
    }
    let validator = std::env::var("USER").unwrap_or_else(|_| "human".into());
    for r in rules {
        let env = call(
            c,
            on(
                branch,
                Request::Validate {
                    rule: r.rule,
                    verdict,
                    curvec_seen: r.current.clone(),
                    validator: validator.clone(),
                },
            ),
        )?;
        if let Response::Committed { result } = env.response {
            println!("rule {}: recorded at epoch {}", r.rule.0, result.epoch.0);
        }
    }
    Ok(())
}
/// `ctx branch create|list|archive <dir> …` (ADR 0013 §10).
pub fn branch(sub: &str, dir: &Path, args: &[String]) -> Result<(), String> {
    let mut c = connect(dir)?;
    let name = || args.first().cloned().ok_or("missing branch name");
    let env = match sub {
        "create" => {
            let from = args
                .windows(2)
                .find(|w| w[0] == "--from")
                .map(|w| w[1].clone());
            call(
                &mut c,
                Request::BranchCreate {
                    name: name()?,
                    from,
                },
            )?
        }
        "archive" => call(&mut c, Request::BranchArchive { name: name()? })?,
        "list" => call(&mut c, Request::BranchList)?,
        _ => return Err("usage: ctx branch create|list|archive <dir> [name] [--from B]".into()),
    };
    match env.response {
        Response::Branches { branches } => {
            for b in branches {
                println!(
                    "{}\tparent={}\tforked={}{}",
                    b.name,
                    b.parent.map_or("-".to_owned(), |p| p.0.to_string()),
                    b.fork_epoch.0,
                    if b.archived { "\tarchived" } else { "" }
                );
            }
        }
        Response::Committed { result } => println!("{sub}d at epoch {}", result.epoch.0),
        other => println!("{other:?}"),
    }
    Ok(())
}
/// `ctx merge <dir> <source> [--into B] [--no-require-certified] [--certify K …]`.
pub fn merge(dir: &Path, args: &[String]) -> Result<(), String> {
    let mut c = connect(dir)?;
    let source = args.first().cloned().ok_or(
        "usage: ctx merge <dir> <source> [--into B] [--no-require-certified] [--certify K ...]",
    )?;
    let (mut into, mut require_certified, mut certify) = (None, true, Vec::new());
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--into" => into = Some(it.next().ok_or("--into needs a branch")?.clone()),
            "--no-require-certified" => require_certified = false,
            "--certify" => certify.push(it.next().ok_or("--certify needs a key")?.clone()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let env = call(
        &mut c,
        Request::Merge {
            binding: None,
            source,
            into,
            require_certified,
            certify,
        },
    )?;
    if let Response::Merged { result, report } = env.response {
        println!(
            "merged at epoch {}: {} heads moved, {} conflicts, newly visible {:?}, {} convergent",
            result.epoch.0,
            report.moved_heads.len(),
            report.conflicts.len(),
            report.newly_visible,
            report.convergent
        );
        for c in report.conflicts {
            println!(
                "  conflict {:?} on r{} (artifact {})",
                c.kind, c.target.0, c.artifact.0
            );
        }
        for (l, f) in report.metadata_conflicts {
            println!("  metadata {f:?} differs on logical {}", l.0);
        }
    }
    Ok(())
}
