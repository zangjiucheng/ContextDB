//! `ctx access` (ADR 0017 §4.1): the access document from
//! `.contextdb/access.toml`, committed through the admin socket.
use crate::p4::{call, connect, flag};
pub use contextdb_cli::access_doc::access_doc;
use contextdb_model::*;
use contextdb_proto::proto::*;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

fn show_entry(name: &str, e: &LaneEntry) {
    let list = |s: &BTreeSet<String>| s.iter().cloned().collect::<Vec<_>>().join(",");
    println!(
        "  {name}: members=[{}] writers=[{}] curators=[{}] egress={}",
        list(&e.members),
        list(&e.writers),
        list(&e.curators),
        e.egress
    );
    for (who, dirs) in &e.grants {
        println!("    grant {who}: {}", list(dirs));
    }
    for (who, prefix) in &e.deny {
        println!("    deny {who}: {prefix}");
    }
}
pub fn run(dir: &Path, args: &[String]) -> Result<u8, String> {
    match args.first().map(String::as_str) {
        Some("apply") => {
            let file = PathBuf::from(flag(args, "--file").unwrap_or(".contextdb/access.toml"));
            let src =
                std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let doc = access_doc(&src, contextdb_proto::fsguard::current_uid())?;
            let mut c = connect(dir, "admin.sock")?;
            let env = call(
                &mut c,
                Request::SetAccess {
                    doc,
                    client_txn_id: None,
                },
            )?;
            if let Response::Committed { result } = env.response {
                println!("access document committed at epoch {}", result.epoch.0);
            }
            Ok(0)
        }
        Some("show") => {
            let mut c = connect(dir, "admin.sock")?;
            let env = call(&mut c, Request::AccessShow)?;
            let Response::Access {
                doc,
                ids,
                multi_user,
            } = env.response
            else {
                return Err("unexpected response".into());
            };
            let Some(doc) = doc else {
                println!("no access document: single-user mode (only the store owner)");
                return Ok(0);
            };
            println!(
                "multi-user mode: {}",
                if multi_user {
                    "on"
                } else {
                    "off (the uid mapping is not in effect)"
                }
            );
            println!("owner: {}", doc.owner);
            for (name, id) in &ids {
                match doc.principals.get(name) {
                    Some(uid) => println!("principal {name}: uid {uid}, id {}", id.0),
                    None => println!("principal {name}: id {} (no longer listed)", id.0),
                }
            }
            for (g, m) in &doc.groups {
                println!(
                    "group {g}: {}",
                    m.iter().cloned().collect::<Vec<_>>().join(",")
                );
            }
            println!("lanes:");
            for (name, e) in &doc.lanes {
                show_entry(name, e);
            }
            show_entry("code_lanes", &doc.code_lanes);
            show_entry("default_lane", &doc.default_lane);
            println!("  personal: egress={}", doc.personal_egress);
            Ok(0)
        }
        Some("check") => {
            let (Some(principal), Some(key)) = (args.get(1), args.get(2)) else {
                return Err(USAGE.into());
            };
            let mut c = connect(dir, "admin.sock")?;
            let env = call(
                &mut c,
                Request::AccessCheck {
                    principal: principal.clone(),
                    key: key.clone(),
                    branch: flag(args, "--branch").map(str::to_owned),
                },
            )?;
            let Response::AccessDecision {
                principal: p,
                allowed,
                step,
            } = env.response
            else {
                return Err("unexpected response".into());
            };
            println!(
                "{principal} (id {}) {} {key}: {step}",
                p.0,
                if allowed { "may read" } else { "may not read" }
            );
            Ok(if allowed { 0 } else { 1 })
        }
        _ => Err(USAGE.into()),
    }
}
const USAGE: &str = "usage: ctx access apply <dir> [--file .contextdb/access.toml] | show <dir> | check <dir> <principal> <key> [--branch B]";

#[cfg(test)]
mod tests {
    use super::*;
    const EXAMPLE: &str = r#"
version = 1
[principals.alice]
uid = 501
[principals.bob]
uid = 502
[principals.carol]
uid = 503
[groups]
billing = ["bob", "carol"]
[lanes.repo]
members  = ["*members"]
writers  = ["*members"]
curators = ["alice"]
egress   = true
[lanes."billing-notes"]
members  = ["@billing"]
writers  = ["@billing"]
curators = ["bob"]
egress   = false
[[lanes."billing-notes".grants]]
principal = "carol"
dirs = ["src/billing/invoices"]
[[lanes.repo.deny]]
principal = "carol"
prefix = "secrets."
[personal]
egress = false
"#;
    #[test]
    fn the_adr_example_parses_with_defaults() {
        let d = access_doc(EXAMPLE, 501).unwrap();
        assert_eq!(d.owner, "alice");
        assert_eq!(d.principals.len(), 3);
        assert_eq!(d.code_lanes, LaneEntry::code_default());
        assert_eq!(d.default_lane, LaneEntry::owner_only());
        assert!(!d.personal_egress);
        let b = &d.lanes["billing-notes"];
        assert!(!b.egress);
        assert_eq!(b.grants["carol"].len(), 1);
        assert!(
            d.lanes["repo"]
                .deny
                .contains(&("carol".into(), "secrets.".into()))
        );
        assert!(contextdb_core::access::validate(&d).is_ok());
    }
    #[test]
    fn unknown_keys_and_a_missing_owner_are_refused() {
        assert!(access_doc(EXAMPLE, 999).unwrap_err().contains("owner"));
        let extra = format!("{EXAMPLE}\n[extra]\nx = 1\n");
        assert!(access_doc(&extra, 501).unwrap_err().contains("unknown key"));
        let bad_lane = EXAMPLE.replace("egress   = true", "egres = true");
        assert!(
            access_doc(&bad_lane, 501)
                .unwrap_err()
                .contains("unknown key")
        );
        assert!(access_doc("version = 2", 501).is_err());
    }
}
