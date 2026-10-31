//! `ctx`: operator commands over a durable store (SPEC §5 P2, ADR 0006) and,
//! with a running `ctxd`, admin commands over its socket (ADR 0008 §8.3).
mod anchors;
mod daemon;
mod p4;
mod setup;
mod ui;

use contextdb_model::*;
use contextdb_store::{OpenOptions, ProjectionFile, StdDisk, Store, doctor, migrate};
use std::{path::Path, process::ExitCode};

const HELP: &str = "ContextDB CLI

Usage (offline; refused while ctxd runs):
  ctx replay <dir>                     open the store, replay, print the open report
  ctx doctor <dir> [--rebuild]         verify chain, replay, full oracle check, blobs;
                                       --rebuild discards and rebuilds projections
  ctx doctor <dir> --truncate-before <epoch> [--yes]
                                       repair a broken chain: drop frames from <epoch>;
                                       without --yes only prints what would be lost
  ctx snapshot <dir>                   write an input-layer snapshot
  ctx migrate <dir>                    migrate an older op_log format (keeps a backup)
  ctx export <dir>                     print the op_log as EventLog JSONL (v0 lines unless a batch needs v1)

Usage (via a running ctxd, admin socket):
  ctx status <dir> [--branch B]        epoch, logicals, revisions, sessions
                                       (--branch on status, why, why-not, impact,
                                       diff and validate reads that branch)
  ctx why <dir> <key[@rev]>            base, active rules, premises
  ctx why-not <dir> <key[@rev]>        failing incidences per rule
  ctx impact <dir> <key[@rev]>         dependents (reachable impact)
  ctx diff <dir> <from> <to>           status changes between epochs
  ctx validate <dir> <key@rev> verified|rejected [--yes]
                                       controlled validation of Sem rules (shows the
                                       vectors, records exactly what was shown)
  ctx shutdown <dir>                   stop ctxd
  ctx branch create <dir> <name> [--from B] | list <dir> | archive <dir> <name>
                                       branches (sessions bind to the branch named
                                       like their git branch)
  ctx merge <dir> <source> [--into B] [--no-require-certified] [--certify K ...]
                                       merge a branch (default into main)

Hooks and gating (P4, ADR 0010):
  ctx check <dir> (--session HEX | --harness H --key K) [--agent A] [--strict]
            [--action-tool T --action-path P ...] [--dry-run] [--fail-on-warn]
                                       verdict and gate decision; exit 0 allow
                                       (warn too unless --fail-on-warn), 1 deny,
                                       2 error, 3 daemon unavailable
  ctx check <dir> --all                exit 1 if any head is STALE or CONFLICTED
  ctx policy show|apply <dir> [--file .contextdb/policy.toml]
                                       commit a policy (admin; kernel-enforced)
  ctx gating apply <dir> [--file .contextdb/gating.toml]
                                       compile gating for the hooks
  ctx hooks install|uninstall --harness claude|codex|opencode [--project ROOT]
            [--dir DATA] [--ctx-hook PATH] [--mcp-name NAME]
  ctx hooks flush <dir>                deliver every pending outbox fact now
  ctx gate-report P4 <dir> [--out FILE]
                                       dogfood numbers (false-deny, coverage, ...)
  ctx dogfood review <dir>             label warn/deny decisions and sampled allows
Setup (one repository; binaries installed together, scripts/install.sh):
  ctx setup [--project ROOT] [--dir DATA] [--harness claude|codex|opencode|both|all] [--no-start]
                                       data dir, ctxd, project MCP config, hooks
  ctx service install|uninstall|restart|status <dir> [--no-load]
                                       launchd (macOS) / systemd --user service
                                       keeping ctxd running
Viewer:
  ctx ui <dir> [--port N]              read-only context graph in the browser
                                       (127.0.0.1 only, default port 7787)
  ctx --help | --version

Specification: SPEC.md; durability: docs/adr/0006-p2-durable-log.md";

/// Operator commands never create a store: a mistyped path is an error.
fn options(dir: &Path) -> OpenOptions {
    OpenOptions {
        projections: ProjectionFile::Path(dir.join(contextdb_store::PROJECTIONS_FILE)),
        create: false,
        ..OpenOptions::default()
    }
}
fn hex(id: &[u8]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}
fn open(dir: &Path) -> Result<Store<StdDisk>, String> {
    open_with(dir, &options(dir))
}
fn open_with(dir: &Path, options: &OpenOptions) -> Result<Store<StdDisk>, String> {
    let (store, report) =
        Store::open(StdDisk::default(), dir, options).map_err(|e| format!("open: {e:?}"))?;
    println!(
        "epoch={} replayed={} snapshot={:?} projection_watermark={} self_checked={}",
        store.epoch().0,
        report.replayed,
        report.snapshot_epoch.map(|e| e.0),
        report.projection_watermark.0,
        report.self_checked
    );
    if let Some(t) = &report.truncated_tail {
        println!(
            "TRUNCATED TAIL after epoch {}: {} bytes ({:?})",
            t.after_epoch.0, t.bytes, t.reason
        );
    }
    if let Some(seg) = &report.removed_segment {
        println!("removed empty tail segment {}", seg.display());
    }
    for s in &report.rejected_snapshots {
        println!("ignored snapshot {s}");
    }
    Ok(store)
}
fn open_quiet(dir: &Path) -> Result<Store<StdDisk>, String> {
    Store::open(StdDisk::default(), dir, &options(dir))
        .map(|(store, _)| store)
        .map_err(|e| format!("open: {e:?}"))
}
fn doctor(dir: &Path, rebuild: bool) -> Result<(), String> {
    // Projections are rebuildable: --rebuild clears them under the store lock.
    let mut store = open_with(
        dir,
        &OpenOptions {
            rebuild_projections: rebuild,
            ..options(dir)
        },
    )?;
    let mark = store
        .catch_up_projections()
        .map_err(|e| format!("projections: {e:?}"))?;
    println!("projections complete through epoch {}", mark.0);
    // Every branch (ADR 0013): each database is checked under its own ids.
    let kernel = store.kernel().map_err(|e| format!("{e:?}"))?;
    let mut contents = std::collections::BTreeSet::new();
    let mut revisions = 0;
    for info in kernel.branches() {
        let db = kernel.branch(info.id).expect("listed").db();
        let inputs = db.inputs();
        if DerivedView::derived(db) != &contextdb_oracle::jacobi(&inputs) {
            return Err(format!(
                "{}: derived state differs from the oracle",
                info.name
            ));
        }
        revisions += inputs.revisions.len();
        contents.extend(inputs.revisions.iter().map(|r| r.content));
    }
    println!(
        "oracle check: derived state == jacobi(inputs) on {} branches ({revisions} revisions)",
        kernel.branches().count()
    );
    let mut problems = 0;
    for content in &contents {
        if let contextdb_store::Payload::Unavailable(why) =
            store.get_blob(content).map_err(|e| format!("{e:?}"))?
        {
            println!("payload {why:?} ({})", hex(content));
            problems += 1;
        }
    }
    println!("payloads: {problems} unavailable");
    Ok(())
}
fn truncate(dir: &Path, epoch: u64, yes: bool) -> Result<(), String> {
    let disk = StdDisk::default();
    let plan = doctor::plan_truncate_before(&disk, dir, Epoch(epoch))
        .map_err(|e| format!("plan: {e:?}"))?;
    println!("new tail: epoch {}", plan.keep_through.0);
    println!(
        "ACKED DATA LOSS: {} readable epochs {:?}{}",
        plan.dropped_epochs.len(),
        plan.dropped_epochs.iter().map(|e| e.0).collect::<Vec<_>>(),
        if plan.unreadable_tail {
            " plus unreadable bytes that may hold more acked epochs"
        } else {
            ""
        }
    );
    for id in &plan.dropped_txn_ids {
        println!("client_txn_id {} becomes NotCommitted", hex(id));
    }
    if !yes {
        println!("dry run: re-run with --yes to truncate");
        return Ok(());
    }
    // Projections above the new tail no longer name the log; open clears them.
    doctor::truncate_before(&disk, dir, &plan).map_err(|e| format!("truncate: {e:?}"))?;
    println!("truncated");
    Ok(())
}
fn run(args: &[String]) -> Result<u8, String> {
    let dir = |n: usize| args.get(n).map(Path::new).ok_or("missing <dir>");
    match args.first().map(String::as_str) {
        Some("check") if args.iter().any(|a| a == "--range") => anchors::range(dir(1)?, &args[2..]),
        Some("check") => p4::check(dir(1)?, &args[2..]),
        Some("anchors") => anchors::run(&args[1..]),
        Some("anchors-pre-push") => anchors::pre_push(dir(1)?, dir(2)?),
        Some("policy") => {
            let sub = args.get(1).cloned().unwrap_or_default();
            let mut rest = vec![sub];
            rest.extend(args.iter().skip(3).cloned());
            p4::policy(dir(2)?, &rest)
        }
        Some("gating") => {
            let sub = args.get(1).cloned().unwrap_or_default();
            let mut rest = vec![sub];
            rest.extend(args.iter().skip(3).cloned());
            p4::gating(dir(2)?, &rest)
        }
        Some("hooks") => p4::hooks(&args[1..]),
        Some("setup") => setup::setup(&args[1..]),
        Some("service") => setup::service(&args[1..]),
        Some("ui") => ui::run(&args[1..]),
        Some("gate-report") => p4::gate_report(&args[1..]),
        Some("dogfood") => p4::dogfood(&args[1..]),
        Some("branch") => {
            let sub = args.get(1).cloned().unwrap_or_default();
            daemon::branch(&sub, dir(2)?, &args[3.min(args.len())..]).map(|()| 0)
        }
        Some("merge") => daemon::merge(dir(1)?, &args[2.min(args.len())..]).map(|()| 0),
        _ => run_p3(args).map(|()| 0),
    }
}
fn run_p3(args: &[String]) -> Result<(), String> {
    let dir = |n: usize| args.get(n).map(Path::new).ok_or("missing <dir>");
    match args.first().map(String::as_str) {
        Some(
            cmd @ ("status" | "why" | "why-not" | "impact" | "diff" | "validate" | "shutdown"),
        ) => daemon::run(cmd, dir(1)?, &args[2..]),
        Some("replay") => open(dir(1)?).map(|_| ()),
        Some("snapshot") => {
            let mut store = open(dir(1)?)?;
            let e = store.snapshot().map_err(|e| format!("snapshot: {e:?}"))?;
            println!("snapshot at epoch {}", e.0);
            Ok(())
        }
        Some("export") => {
            let store = open_quiet(dir(1)?)?;
            let mut log = EventLog::default();
            for e in 1..=store.epoch().0 {
                let row = store
                    .read_row(Epoch(e))
                    .map_err(|e| format!("read: {e:?}"))?;
                // EventLog keeps its import limits (ADR 0006 review R4).
                let batch = Batch::from_bytes(&row.batch_blob).map_err(|_| {
                    format!("ExportLimit {{ epoch: {e} }}: batch exceeds EventLog limits")
                })?;
                log.batches.push(batch);
            }
            print!("{}", log.to_jsonl());
            Ok(())
        }
        Some("migrate") => {
            let r = migrate::migrate(&StdDisk::default(), dir(1)?)
                .map_err(|e| format!("migrate: {e:?}"))?;
            println!(
                "migrated format {} -> {} ({} frames); backup {}",
                r.from, r.to, r.frames, r.backup
            );
            if !r.changed.is_empty() {
                let epochs: Vec<String> = r.changed.iter().map(|e| e.0.to_string()).collect();
                println!(
                    "{} results changed under the new kernel (originally acked results kept): epochs {}",
                    r.changed.len(),
                    epochs.join(", ")
                );
            }
            Ok(())
        }
        Some("doctor") => {
            let d = dir(1)?;
            match args.get(2).map(String::as_str) {
                None => doctor(d, false),
                Some("--rebuild") => doctor(d, true),
                Some("--truncate-before") => {
                    let epoch = args
                        .get(3)
                        .and_then(|e| e.parse().ok())
                        .ok_or("missing <epoch>")?;
                    truncate(d, epoch, args.get(4).map(String::as_str) == Some("--yes"))
                }
                Some(other) => Err(format!("unknown doctor option {other}")),
            }
        }
        Some(other) => Err(format!("unknown command {other}; see ctx --help")),
        None => Err("no command".into()),
    }
}
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Some("--version") | Some("-V") => {
            println!("ctx {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("ctx: {e}");
            ExitCode::from(p4::EXIT_ERROR)
        }
    }
}
