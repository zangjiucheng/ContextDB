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
/// `ctx validate <dir> <key@rev> verified|rejected [--label L] [--rationale T]
/// [--evidence key@rev]... [--from-review ID] [--yes]`: shows every Sem rule
/// concluding the revision with its current vector, asks for confirmation,
/// then records exactly the vector shown (CurvecMoved if it changed
/// meanwhile). The daemon stores a metadata blob with the label, rationale
/// and evidence; `--from-review` converts an LLM suggestion and resolves the
/// review in the same batch (ADR 0015 §4.2; the independent evidence SPEC
/// §4.4 asks for is the caller's).
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
    let flag = |name: &str| -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == name)
            .map(|w| w[1].clone())
            .collect()
    };
    let label = match flag("--label").first() {
        Some(l) => Some(
            contextdb_proto::docs::Label::parse(l)
                .ok_or("--label: reinforcement|none|conflict|boundary_change|new_rule")?,
        ),
        None => None,
    };
    let rationale = flag("--rationale").first().cloned().unwrap_or_default();
    let evidence = flag("--evidence");
    let from_review = match flag("--from-review").first() {
        Some(a) => Some(ArtifactId(
            a.parse()
                .map_err(|_| "--from-review takes an artifact id")?,
        )),
        None => None,
    };
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
    let first = rules[0].rule;
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
                    label,
                    rationale: rationale.clone(),
                    evidence: evidence.clone(),
                    // One review resolves once, with the first rule.
                    from_review: if r.rule == first { from_review } else { None },
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
/// `ctx conflicts list|open|resolve <dir> …` (ADR 0015 §6), as admin.
/// `ctx changes <dir> [--since E] [--branch B] [--scope S] [--max N]
/// [--follow]` (ADR 0015 §8): the branch's per-epoch status changes after
/// `--since` (default 0). `--follow` long-polls and keeps printing.
pub fn changes(dir: &Path, args: &[String]) -> Result<(), String> {
    use contextdb_proto::proto::Response as R;
    let (branch, args) = take_branch(args)?;
    let flag = |name: &str| args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone());
    let follow = args.iter().any(|a| a == "--follow");
    let mut since = Epoch(
        flag("--since")
            .map(|e| e.parse().map_err(|_| "--since takes an epoch"))
            .transpose()?
            .unwrap_or(0),
    );
    let max_items = flag("--max").and_then(|n| n.parse().ok()).unwrap_or(0);
    let mut c = connect(dir)?;
    loop {
        let env = call(
            &mut c,
            Request::Changes {
                binding: None,
                branch: branch.clone(),
                since_epoch: since,
                scope: flag("--scope"),
                max_items,
                wait_ms: if follow { 30_000 } else { 0 },
            },
        )?;
        match env.response {
            R::Changes {
                items,
                reached,
                more,
            } => {
                for i in &items {
                    println!("{} {}@{} {:?}", i.epoch.0, i.key, i.rev.0, i.summary);
                }
                since = reached;
                if !more && !follow {
                    return Ok(());
                }
            }
            R::Truncated { earliest } => {
                println!(
                    "truncated: epochs after {} cannot be served; re-check, then follow from {}",
                    since.0, earliest.0
                );
                if !follow {
                    return Ok(());
                }
                since = earliest;
            }
            other => return Err(format!("unexpected response {other:?}")),
        }
    }
}
pub fn conflicts(sub: &str, dir: &Path, args: &[String]) -> Result<(), String> {
    use contextdb_proto::proto::{ConflictState, Response as R};
    let (branch, args) = take_branch(args)?;
    let flag = |name: &str| args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone());
    let positional: Vec<&String> = {
        let mut out = Vec::new();
        let mut skip = false;
        for a in &args {
            if skip {
                skip = false;
            } else if a.starts_with("--") {
                skip = true;
            } else {
                out.push(a);
            }
        }
        out
    };
    let mut c = connect(dir)?;
    let request = match sub {
        "list" => Request::Conflicts {
            binding: None,
            branch: branch.clone(),
            scope: flag("--scope"),
            state: match flag("--state").as_deref().unwrap_or("all") {
                "open" => ConflictState::Open,
                "review" => ConflictState::Review,
                "candidates" => ConflictState::Candidates,
                "all" => ConflictState::All,
                other => return Err(format!("unknown state {other}")),
            },
            limit: flag("--limit").and_then(|l| l.parse().ok()).unwrap_or(0),
        },
        "open" => Request::ConflictOpen {
            binding: None,
            branch: branch.clone(),
            a: target(positional.first().ok_or("missing <key@rev>")?)?,
            b: positional.get(1).map(|t| target(t)).transpose()?,
            reason: flag("--reason").ok_or("--reason is required")?,
            evidence: Vec::new(),
            from: match (flag("--from-candidate"), flag("--from-review")) {
                (Some(_), Some(_)) => {
                    return Err("--from-candidate or --from-review, not both".into());
                }
                (Some(c), None) => Some(ConflictFrom::Candidate(
                    c.parse()
                        .map_err(|_| "--from-candidate takes a candidate id")?,
                )),
                (None, Some(r)) => Some(ConflictFrom::Review(ArtifactId(
                    r.parse()
                        .map_err(|_| "--from-review takes an artifact id")?,
                ))),
                (None, None) => None,
            },
            expect_cert_valid: false,
            client_txn_id: None,
        },
        "resolve" => Request::Resolve {
            binding: None,
            branch: branch.clone(),
            artifacts: (positional.iter())
                .map(|a| {
                    a.parse()
                        .map(ArtifactId)
                        .map_err(|_| format!("bad artifact id {a}"))
                })
                .collect::<Result<_, _>>()?,
            note: flag("--note").unwrap_or_default(),
            expect_cert_valid: false,
            client_txn_id: None,
        },
        _ => return Err("usage: ctx conflicts list|open|resolve <dir> ...".into()),
    };
    let env = call(&mut c, request)?;
    match env.response {
        R::Conflicts { items, truncated } => {
            for i in &items {
                let ids: Vec<String> = i.artifacts.iter().map(|a| a.0.to_string()).collect();
                let sides: Vec<String> = (i.sides.iter())
                    .map(|s| {
                        format!(
                            "{}@{}{}",
                            s.key,
                            s.rev.0,
                            if s.current { "" } else { " (superseded)" }
                        )
                    })
                    .collect();
                println!(
                    "{} [{}] {} — {} ({}){}",
                    i.kind,
                    ids.join(","),
                    sides.join(" vs "),
                    i.reason,
                    i.origin,
                    if i.marks.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", i.marks.join(", "))
                    }
                );
            }
            if items.is_empty() {
                println!("nothing open");
            }
            if truncated {
                println!("(more; raise --limit)");
            }
        }
        R::Committed { result } => {
            let ids: Vec<String> = (result.allocations.new_artifacts.iter())
                .map(|a| a.0.to_string())
                .collect();
            println!(
                "committed at epoch {}{}",
                result.epoch.0,
                if ids.is_empty() {
                    String::new()
                } else {
                    format!("; artifacts {}", ids.join(","))
                }
            );
        }
        other => return Err(format!("unexpected response {other:?}")),
    }
    Ok(())
}
/// `ctx validators status|reload|check <dir>` and `ctx validators retry
/// <dir> <job-key>|--failed` (ADR 0015 §4.4).
pub fn validators(sub: &str, dir: &Path, args: &[String]) -> Result<(), String> {
    use contextdb_proto::proto::ValidatorsAction;
    let action = match sub {
        "status" => ValidatorsAction::Status,
        "reload" => ValidatorsAction::Reload,
        "check" => ValidatorsAction::Check,
        "retry" => match args.first().map(String::as_str) {
            Some("--failed") => ValidatorsAction::Retry {
                key: None,
                failed: true,
            },
            Some(hex) => ValidatorsAction::Retry {
                key: Some(parse_key(hex)?),
                failed: false,
            },
            None => return Err("usage: ctx validators retry <dir> <job-key>|--failed".into()),
        },
        _ => {
            return Err(
                "usage: ctx validators status|reload|check <dir> | retry <dir> <job-key>|--failed"
                    .into(),
            );
        }
    };
    let mut c = connect(dir)?;
    let env = call(&mut c, Request::Validators { action })?;
    let Response::Validators { report } = env.response else {
        return Err(format!("unexpected response {:?}", env.response));
    };
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let line = |v: &[contextdb_proto::proto::Counter]| {
        v.iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join(" ")
    };
    if report.validators.is_empty() {
        println!(
            "validators: none configured ({}/validators.toml)",
            dir.display()
        );
    }
    for v in &report.validators {
        println!("validator {} [{:?}] {}", v.name, v.kind, v.version);
    }
    if let Some(e) = &report.error {
        println!("config error (previous configuration kept): {e}");
    }
    println!("limits: {}", line(&report.limits));
    println!("jobs: {}", line(&report.jobs));
    println!("stats: {}", line(&report.stats));
    for j in &report.failed {
        println!(
            "failed {} branch={} rule={} gen={} attempts={}: {}",
            hex(&j.key),
            j.branch.0,
            j.rule.0,
            j.generation,
            j.attempts,
            j.last_error.as_deref().unwrap_or("")
        );
    }
    if sub == "retry" {
        println!("requeued {}", report.retried);
    }
    for l in &report.check {
        println!("check: {l}");
    }
    Ok(())
}
fn parse_key(hex: &str) -> Result<[u8; 32], String> {
    let bad = || format!("job key must be 64 hex digits: {hex}");
    if hex.len() != 64 {
        return Err(bad());
    }
    let mut out = [0u8; 32];
    for (n, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * n..2 * n + 2], 16).map_err(|_| bad())?;
    }
    Ok(out)
}
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
                    client_txn_id: None,
                },
            )?
        }
        "archive" => call(
            &mut c,
            Request::BranchArchive {
                name: name()?,
                client_txn_id: None,
            },
        )?,
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
            client_txn_id: None,
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
