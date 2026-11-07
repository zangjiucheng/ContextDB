//! `ctx access` (ADR 0017 §4.1): the access document from
//! `.contextdb/access.toml`, committed through the admin socket.
use crate::p4::{call, connect, flag, only_keys, table};
use contextdb_model::*;
use contextdb_proto::proto::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

fn strings(v: &toml::Value, what: &str) -> Result<BTreeSet<String>, String> {
    let a = v
        .as_array()
        .ok_or_else(|| format!("{what} must be an array of strings"))?;
    a.iter()
        .map(|x| {
            x.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{what} must be an array of strings"))
        })
        .collect()
}
fn subtable<'a>(v: &'a toml::Value, what: &str) -> Result<&'a toml::Table, String> {
    v.as_table()
        .ok_or_else(|| format!("{what} must be a table"))
}
/// A lane entry; fields left out take `default`'s value.
fn entry(t: &toml::Table, what: &str, default: &LaneEntry) -> Result<LaneEntry, String> {
    only_keys(
        t,
        &["members", "writers", "curators", "egress", "grants", "deny"],
        what,
    )?;
    let mut e = default.clone();
    for (key, slot) in [
        ("members", &mut e.members),
        ("writers", &mut e.writers),
        ("curators", &mut e.curators),
    ] {
        if let Some(v) = t.get(key) {
            *slot = strings(v, &format!("{what}.{key}"))?;
        }
    }
    if let Some(v) = t.get("egress") {
        e.egress = v
            .as_bool()
            .ok_or_else(|| format!("{what}.egress must be true or false"))?;
    }
    if let Some(v) = t.get("grants") {
        let list = v
            .as_array()
            .ok_or_else(|| format!("{what}.grants must be an array of tables"))?;
        for g in list {
            let g = subtable(g, &format!("{what}.grants"))?;
            only_keys(g, &["principal", "dirs"], &format!("{what}.grants"))?;
            let who = g
                .get("principal")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| format!("{what}.grants: principal is required"))?;
            let dirs = strings(
                g.get("dirs")
                    .ok_or_else(|| format!("{what}.grants: dirs is required"))?,
                &format!("{what}.grants.dirs"),
            )?;
            e.grants.entry(who.to_owned()).or_default().extend(dirs);
        }
    }
    if let Some(v) = t.get("deny") {
        let list = v
            .as_array()
            .ok_or_else(|| format!("{what}.deny must be an array of tables"))?;
        for d in list {
            let d = subtable(d, &format!("{what}.deny"))?;
            only_keys(d, &["principal", "prefix"], &format!("{what}.deny"))?;
            let get = |k: &str| {
                d.get(k)
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{what}.deny: {k} is required"))
            };
            e.deny.insert((get("principal")?, get("prefix")?));
        }
    }
    Ok(e)
}
/// `access.toml` → `AccessDoc`. The owner entry is the principal whose uid
/// is `owner_uid` (the uid `ctx` and the daemon run as). Unknown keys are
/// refused; the kernel validates the rest (ADR 0017 §4.2).
pub fn access_doc(src: &str, owner_uid: u32) -> Result<AccessDoc, String> {
    let t = table(src)?;
    only_keys(
        &t,
        &[
            "version",
            "principals",
            "groups",
            "lanes",
            "code_lanes",
            "personal",
            "default_lane",
        ],
        "access.toml",
    )?;
    let version = t
        .get("version")
        .and_then(toml::Value::as_integer)
        .ok_or("access.toml: version = 1 is required")?;
    if version != 1 {
        return Err(format!("access.toml: unsupported version {version}"));
    }
    let mut principals = BTreeMap::new();
    if let Some(v) = t.get("principals") {
        for (name, p) in subtable(v, "principals")? {
            let p = subtable(p, &format!("principals.{name}"))?;
            only_keys(p, &["uid"], &format!("principals.{name}"))?;
            let uid = p
                .get("uid")
                .and_then(toml::Value::as_integer)
                .and_then(|u| u32::try_from(u).ok())
                .ok_or_else(|| format!("principals.{name}: uid must be a uid"))?;
            principals.insert(name.clone(), uid);
        }
    }
    let owner = principals
        .iter()
        .find(|(_, u)| **u == owner_uid)
        .map(|(n, _)| n.clone())
        .ok_or_else(|| format!("access.toml: no principal has the owner's uid {owner_uid}"))?;
    let mut groups = BTreeMap::new();
    if let Some(v) = t.get("groups") {
        for (name, members) in subtable(v, "groups")? {
            groups.insert(name.clone(), strings(members, &format!("groups.{name}"))?);
        }
    }
    let listed = LaneEntry {
        members: BTreeSet::new(),
        writers: BTreeSet::new(),
        curators: BTreeSet::new(),
        ..LaneEntry::owner_only()
    };
    let mut lanes = BTreeMap::new();
    if let Some(v) = t.get("lanes") {
        for (name, e) in subtable(v, "lanes")? {
            let what = format!("lanes.{name:?}");
            lanes.insert(name.clone(), entry(subtable(e, &what)?, &what, &listed)?);
        }
    }
    let code_lanes = match t.get("code_lanes") {
        Some(v) => entry(
            subtable(v, "code_lanes")?,
            "code_lanes",
            &LaneEntry::code_default(),
        )?,
        None => LaneEntry::code_default(),
    };
    let default_lane = match t.get("default_lane") {
        Some(v) => entry(
            subtable(v, "default_lane")?,
            "default_lane",
            &LaneEntry::owner_only(),
        )?,
        None => LaneEntry::owner_only(),
    };
    let mut personal_egress = false;
    if let Some(v) = t.get("personal") {
        let p = subtable(v, "personal")?;
        only_keys(p, &["egress"], "personal")?;
        if let Some(e) = p.get("egress") {
            personal_egress = e.as_bool().ok_or("personal.egress must be true or false")?;
        }
    }
    Ok(AccessDoc {
        version: 1,
        owner,
        principals,
        groups,
        lanes,
        code_lanes,
        personal_egress,
        default_lane,
    })
}
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
