//! `access.toml` → `AccessDoc` (ADR 0017 §4.1). In the library so the
//! fuzz targets reach it (G4).
use contextdb_model::*;
use std::collections::{BTreeMap, BTreeSet};

/// `src` as a TOML table.
pub fn table(src: &str) -> Result<toml::Table, String> {
    src.parse::<toml::Table>().map_err(|e| format!("toml: {e}"))
}
/// Refuses a key of `t` not in `allowed`.
pub fn only_keys(t: &toml::Table, allowed: &[&str], what: &str) -> Result<(), String> {
    match t.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!("{what}: unknown key {k}")),
        None => Ok(()),
    }
}
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
