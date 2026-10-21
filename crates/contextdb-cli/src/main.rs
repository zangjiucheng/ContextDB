//! `ctx`: operator commands over a durable store (SPEC §5 P2, ADR 0006) and,
//! with a running `ctxd`, admin commands over its socket (ADR 0008 §8.3).
mod daemon;
mod p4;

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
  ctx export <dir>                     print the op_log as EventLog v0 JSONL

Usage (via a running ctxd, admin socket):
  ctx status <dir>                     epoch, logicals, revisions, sessions
  ctx why <dir> <key[@rev]>            base, active rules, premises
  ctx why-not <dir> <key[@rev]>        failing incidences per rule
  ctx impact <dir> <key[@rev]>         dependents (reachable impact)
  ctx diff <dir> <from> <to>           status changes between epochs
  ctx validate <dir> <key@rev> verified|rejected [--yes]
                                       human validation of Sem rules (shows the
                                       vectors, records exactly what was shown)
  ctx shutdown <dir>                   stop ctxd

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
  ctx hooks install|uninstall --harness claude|codex [--project ROOT]
            [--dir DATA] [--ctx-hook PATH] [--mcp-name NAME]
  ctx hooks flush <dir>                deliver every pending outbox fact now
  ctx gate-report P4 <dir> [--out FILE]
                                       dogfood numbers (false-deny, coverage, ...)
  ctx dogfood review <dir>             label warn/deny decisions and sampled allows
  ctx --help | --version

Specification: SPEC.md; durability: docs/adr/0006-p2-durable-log.md";

/// Operator commands never create a store: a mistyped path is an error.
fn options(dir: &Path) -> OpenOptions {
    OpenOptions {
        projections: ProjectionFile::Path(dir.join("projections.sqlite")),
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
    let db = store.db().map_err(|e| format!("{e:?}"))?;
    let inputs = db.inputs();
    let oracle = contextdb_oracle::jacobi(&inputs);
    if DerivedView::derived(db) != &oracle {
        return Err("derived state differs from the oracle".into());
    }
    println!(
        "oracle check: derived state == jacobi(inputs) for {} revisions",
        inputs.revisions.len()
    );
    let mut problems = 0;
    for (n, r) in inputs.revisions.iter().enumerate() {
        if let contextdb_store::Payload::Unavailable(why) =
            store.get_blob(&r.content).map_err(|e| format!("{e:?}"))?
        {
            println!("revision {n}: payload {why:?} ({})", hex(&r.content));
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
        Some("check") => p4::check(dir(1)?, &args[2..]),
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
        Some("gate-report") => p4::gate_report(&args[1..]),
        Some("dogfood") => p4::dogfood(&args[1..]),
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
                // EventLog v0 keeps its import limits (ADR 0006 review R4).
                let batch = Batch::from_bytes(&row.batch_blob).map_err(|_| {
                    format!("ExportLimit {{ epoch: {e} }}: batch exceeds EventLog v0 limits")
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
