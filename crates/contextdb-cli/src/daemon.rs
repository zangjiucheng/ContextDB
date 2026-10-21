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
pub fn run(cmd: &str, dir: &Path, args: &[String]) -> Result<(), String> {
    let mut c = connect(dir)?;
    match cmd {
        "status" => {
            let env = call(&mut c, Request::Status)?;
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
            let env = call(&mut c, r)?;
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
                Request::Diff {
                    from: parse(0)?,
                    to: parse(1)?,
                },
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
        "validate" => validate(&mut c, args)?,
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
fn validate(c: &mut Client, args: &[String]) -> Result<(), String> {
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
        Request::WhyNot {
            target: t,
            depth: 0,
        },
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
            Request::Validate {
                rule: r.rule,
                verdict,
                curvec_seen: r.current.clone(),
                validator: validator.clone(),
            },
        )?;
        if let Response::Committed { result } = env.response {
            println!("rule {}: recorded at epoch {}", r.rule.0, result.epoch.0);
        }
    }
    Ok(())
}
