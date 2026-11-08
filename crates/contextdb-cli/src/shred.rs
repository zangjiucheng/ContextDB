//! `ctx shred <dir> <key>[@rev] [--with-reviews] [--yes]` (ADR 0017 §8.3):
//! lists the payloads a shred would remove, and with `--yes` commits it
//! through the admin socket.
use crate::p4::{call, connect};
use contextdb_model::*;
use contextdb_proto::proto::*;
use std::path::Path;

/// What a shred does not cover (ADR 0017 §2.3, §8.6).
const LIMITS: &str = "\
A shred makes the payload unreadable and, once the purge completes, unrecoverable
from the files the daemon controls. It does not reach:
  - the session log and the outbox (handoff summaries, supersede reasons, tool
    facts), which may quote the body; there is no supported way to remove them
    in P9;
  - text that agents copied into other records;
  - review payloads and notes, unless --with-reviews;
  - copies made earlier: backups, `ctx export` files, git mirror refs (see
    `ctx mirror export --rebuild`), validators, embedders or anyone it was sent to.
Topology and statuses are unchanged; anyone holding the exact bytes can still
confirm the content hash.";

fn hex(h: &ContentHash) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn run(dir: &Path, args: &[String]) -> Result<u8, String> {
    let spec = args
        .first()
        .ok_or("usage: ctx shred <dir> <key>[@rev] [--with-reviews] [--yes]")?;
    let mut with_reviews = false;
    let mut confirm = false;
    for a in &args[1..] {
        match a.as_str() {
            "--with-reviews" => with_reviews = true,
            "--yes" => confirm = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let (key, rev) = match spec.rsplit_once('@') {
        Some((k, r)) => (
            k.to_owned(),
            Some(RevisionId(
                r.parse()
                    .map_err(|_| format!("{spec}: expected <key>@<rev>"))?,
            )),
        ),
        None => (spec.clone(), None),
    };
    let mut c = connect(dir, "admin.sock")?;
    let env = call(
        &mut c,
        Request::Shred {
            key,
            rev,
            with_reviews,
            confirm,
            client_txn_id: None,
        },
    )?;
    let Response::Shred {
        targets,
        epoch,
        purged,
    } = env.response
    else {
        return Err("unexpected response".into());
    };
    for t in &targets {
        println!(
            "{:<10} {}@{} on {}  {}",
            t.what,
            t.key,
            t.rev.0,
            t.branch,
            &hex(&t.content)[..16]
        );
    }
    let contents: std::collections::BTreeSet<_> = targets.iter().map(|t| t.content).collect();
    println!();
    println!("{LIMITS}");
    println!();
    match epoch {
        None => {
            println!(
                "{} payload(s) would be shredded; run again with --yes to shred them",
                contents.len()
            );
        }
        Some(e) if purged => println!(
            "shredded {} payload(s) at epoch {}: purged",
            contents.len(),
            e.0
        ),
        Some(e) => println!(
            "shredded {} payload(s) at epoch {}: committed, purge pending",
            contents.len(),
            e.0
        ),
    }
    Ok(0)
}
