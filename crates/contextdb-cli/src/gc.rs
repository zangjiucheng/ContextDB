//! `ctx gc <dir> [--apply] [--keep-epochs N]` through the admin socket, and
//! `ctx gc <dir> --compact` offline (ADR 0019 §5.2, §5.5, §5.7).
use crate::p4::{call, connect};
use contextdb_proto::proto::*;
use contextdb_store::{StdDisk, StoreError};
use std::path::Path;

const USAGE: &str = "usage: ctx gc <dir> [--apply] [--keep-epochs N] | ctx gc <dir> --compact";

pub fn run(dir: &Path, args: &[String]) -> Result<u8, String> {
    let (mut apply, mut compact, mut keep_epochs) = (false, false, None);
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--apply" => apply = true,
            "--compact" => compact = true,
            "--keep-epochs" => {
                let n = rest.next().ok_or(USAGE)?;
                keep_epochs = Some(
                    n.parse::<u64>()
                        .map_err(|_| format!("--keep-epochs {n}: expected a number"))?,
                );
            }
            other => return Err(format!("unknown argument {other}; {USAGE}")),
        }
    }
    if compact {
        if apply || keep_epochs.is_some() {
            return Err(format!("--compact takes no other option; {USAGE}"));
        }
        return compact_offline(dir);
    }
    let mut c = connect(dir, "admin.sock")?;
    let Response::Gc { report: r } = call(&mut c, Request::Gc { keep_epochs, apply })?.response
    else {
        return Err("unexpected response".into());
    };
    let cause = match r.cause {
        GcCause::KeepEpochs => format!("keep_epochs {}", r.keep_epochs),
        GcCause::HeldSnapshot => "a snapshot a reader holds".to_owned(),
    };
    if r.applied {
        println!(
            "collected {} payload(s), {} bytes, at epoch {} (generation {})",
            r.payloads, r.bytes, r.now.0, r.generation
        );
    } else {
        println!(
            "dry run at epoch {}: {} payload(s) collectable, {} bytes",
            r.e0.0, r.payloads, r.bytes
        );
    }
    println!("cutoff epoch {}, set by {cause}", r.cutoff.0);
    println!(
        "kept: K1 {} live, K2 {} documents, K3 {} named after the cutoff, K4 {} pinned by \
         certificates; K5: the oldest held snapshot is at epoch {}",
        r.kept_k1, r.kept_k2, r.kept_k3, r.kept_k4, r.held.0
    );
    if !r.applied {
        println!("run again with --apply to collect them");
        return Ok(0);
    }
    if r.rechecked_out > 0 {
        println!(
            "{} candidate(s) kept by the re-check (named, pinned or staged again since the plan)",
            r.rechecked_out
        );
    }
    println!(
        "{}",
        if r.purged {
            "projections purged"
        } else {
            "projection purge pending: retried at the next ctxd start"
        }
    );
    println!("disk space returns with `ctx gc <dir> --compact` while ctxd is stopped");
    Ok(0)
}

/// `ctx gc <dir> --compact`: under the store lock, so never while ctxd runs.
fn compact_offline(dir: &Path) -> Result<u8, String> {
    let r = match contextdb_store::gc::compact(StdDisk::default(), dir) {
        Ok(r) => r,
        Err(StoreError::Locked) => {
            return Err(format!(
                "ctxd holds the store lock for {}: stop it first (ctx shutdown <dir>), then \
                 run --compact again",
                dir.display()
            ));
        }
        Err(e) => return Err(crate::store_err("compact", &e)),
    };
    println!(
        "rewrote {} pack segment(s): {} record(s) dropped, {} bytes reclaimed",
        r.segments_rewritten, r.records_dropped, r.bytes_reclaimed
    );
    println!(
        "ledger: {} entries before, {} after",
        r.ledger_before, r.ledger_after
    );
    for s in &r.snapshots_removed {
        println!("removed snapshot {s}");
    }
    println!("replay check: the reopened store equals a full replay");
    Ok(0)
}
