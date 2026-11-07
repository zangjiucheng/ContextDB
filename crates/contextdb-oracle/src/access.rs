//! Independent reference for the access document (ADR 0017 §3.1, §4):
//! validation, id assignment and `Authorized` as a decision table. It shares
//! nothing with `contextdb-core::access` but the model types.
use contextdb_model::*;
use std::collections::{BTreeMap, BTreeSet};

/// The reference access state: the document and every id ever assigned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessModel {
    pub doc: AccessDoc,
    pub ids: BTreeMap<String, u32>,
}

fn refuse(why: &str) -> TxError {
    TxError::InvalidAccess {
        reason: why.to_owned(),
    }
}
fn name_ok(n: &str) -> bool {
    let n = n.as_bytes();
    !n.is_empty()
        && n.len() <= 32
        && n.iter()
            .all(|c| b"abcdefghijklmnopqrstuvwxyz0123456789_-".contains(c))
}
/// Does role token `t` include principal `who`? (`writers` for `*writers`.)
fn token_has(doc: &AccessDoc, t: &str, who: &str, writers: &BTreeSet<String>) -> bool {
    if t == "*members" {
        return doc.principals.contains_key(who);
    }
    if t == "*owner" {
        return doc.owner == who;
    }
    if t == "*writers" {
        return writers
            .iter()
            .any(|w| w != "*writers" && token_has(doc, w, who, writers));
    }
    if let Some(g) = t.strip_prefix('@') {
        return doc.groups.get(g).is_some_and(|m| m.contains(who));
    }
    t == who
}
fn list_has(
    doc: &AccessDoc,
    list: &BTreeSet<String>,
    who: &str,
    writers: &BTreeSet<String>,
) -> bool {
    list.iter().any(|t| token_has(doc, t, who, writers))
}
/// The principals a grant/deny subject stands for.
fn subject_has(doc: &AccessDoc, subject: &str, who: &str) -> bool {
    match subject.strip_prefix('@') {
        Some(g) => doc.groups.get(g).is_some_and(|m| m.contains(who)),
        None => subject == who,
    }
}
fn check_entry(doc: &AccessDoc, e: &LaneEntry, code: bool) -> Result<(), TxError> {
    let roles = [
        (&e.members, false),
        (&e.writers, false),
        (&e.curators, true),
    ];
    for (list, curators) in roles {
        for t in list {
            let known = match t.as_str() {
                "*members" | "*owner" => true,
                "*writers" => curators,
                _ if t.starts_with('@') => doc.groups.contains_key(&t[1..]),
                _ => doc.principals.contains_key(t),
            };
            if !known {
                return Err(refuse("unknown role token"));
            }
        }
    }
    if code {
        let empty = BTreeSet::new();
        let others = doc
            .principals
            .keys()
            .any(|p| *p != doc.owner && list_has(doc, &e.writers, p, &empty));
        if others {
            return Err(refuse("code lane writers beyond *owner"));
        }
    }
    let empty = BTreeSet::new();
    let subject_ok = |s: &str| {
        let named: Vec<&String> = (doc.principals.keys())
            .filter(|p| subject_has(doc, s, p))
            .collect();
        !named.is_empty() && named.iter().all(|p| list_has(doc, &e.members, p, &empty))
    };
    for (s, dirs) in &e.grants {
        if !subject_ok(s) {
            return Err(refuse("grant for a non-member"));
        }
        for d in dirs {
            for seg in d.split('/') {
                if matches!(seg, "" | "." | ".." | "*") {
                    return Err(refuse("bad grant dir"));
                }
            }
        }
    }
    for (s, prefix) in &e.deny {
        if !subject_ok(s) {
            return Err(refuse("deny for a non-member"));
        }
        if prefix.is_empty() {
            return Err(refuse("empty deny prefix"));
        }
    }
    Ok(())
}
fn check(doc: &AccessDoc) -> Result<(), TxError> {
    if doc.version != 1 {
        return Err(refuse("version"));
    }
    if !doc.principals.contains_key(&doc.owner) {
        return Err(refuse("no owner entry"));
    }
    if doc.principals.keys().any(|n| !name_ok(n)) {
        return Err(refuse("principal name"));
    }
    let uids: BTreeSet<u32> = doc.principals.values().copied().collect();
    if uids.len() != doc.principals.len() {
        return Err(refuse("uid mapped twice"));
    }
    for (g, members) in &doc.groups {
        if !name_ok(g) {
            return Err(refuse("group name"));
        }
        if members.iter().any(|m| m.starts_with('@')) {
            return Err(refuse("nested group"));
        }
        if members.iter().any(|m| !doc.principals.contains_key(m)) {
            return Err(refuse("unknown group member"));
        }
    }
    for (lane, e) in &doc.lanes {
        if lane.is_empty() || lane.starts_with("personal:") {
            return Err(refuse("lane name"));
        }
        check_entry(doc, e, lane.starts_with("code:"))?;
    }
    check_entry(doc, &doc.code_lanes, true)?;
    check_entry(doc, &doc.default_lane, false)
}

impl AccessModel {
    /// `SetAccess` over `prev`.
    pub fn apply(prev: Option<&Self>, doc: &AccessDoc) -> Result<Self, TxError> {
        check(doc)?;
        let mut ids = prev.map(|m| m.ids.clone()).unwrap_or_default();
        for (name, _) in doc.principals.iter() {
            let is_owner = *name == doc.owner;
            if let Some(&id) = ids.get(name) {
                let was_owner = id < 16 || (0x5641_0000..=0x5641_FFFF).contains(&id);
                if was_owner != is_owner {
                    return Err(refuse("a name changes between owner and principal"));
                }
                continue;
            }
            let id = if is_owner {
                0
            } else {
                let top = ids.values().copied().filter(|&v| v >= 16).max();
                let id = top.map_or(16, |t| t + 1);
                if (0x5641_0000..=0x5641_FFFF).contains(&id) {
                    return Err(refuse("ids exhausted"));
                }
                id
            };
            ids.insert(name.clone(), id);
        }
        Ok(Self {
            doc: doc.clone(),
            ids,
        })
    }
    fn name_of(&self, p: PrincipalId) -> Option<&String> {
        self.doc.principals.keys().find(|n| self.ids[*n] == p.0)
    }
    /// The entry of `lane`, materialised (ADR 0017 §4.3).
    fn entry(&self, lane: &str) -> LaneEntry {
        if let Some(e) = self.doc.lanes.get(lane) {
            return e.clone();
        }
        if let Some(n) = lane.strip_prefix("personal:") {
            let set: BTreeSet<String> = self
                .doc
                .principals
                .keys()
                .filter(|p| p.as_str() == n)
                .cloned()
                .collect();
            return LaneEntry {
                members: set.clone(),
                writers: set.clone(),
                curators: set,
                egress: self.doc.personal_egress,
                grants: BTreeMap::new(),
                deny: BTreeSet::new(),
            };
        }
        if lane.starts_with("code:") {
            self.doc.code_lanes.clone()
        } else {
            self.doc.default_lane.clone()
        }
    }
    /// `Authorized(p, l, b)` as a decision table: every predicate first,
    /// then the first matching row. Returns the answer and the step name
    /// (`contextdb_core::access::Step::name`).
    pub fn authorized(
        &self,
        p: PrincipalId,
        key: &str,
        lane: &str,
        owner: PrincipalId,
        dirs: &BTreeSet<String>,
    ) -> (bool, &'static str) {
        let store_owner = p.0 < 16 || (0x5641_0000..=0x5641_FFFF).contains(&p.0);
        let logical_owner = owner == p;
        let name = self.name_of(p);
        let e = self.entry(lane);
        let empty = BTreeSet::new();
        let member = name.is_some_and(|n| list_has(&self.doc, &e.members, n, &empty));
        let foreign_personal = match (lane.strip_prefix("personal:"), name) {
            (Some(o), Some(n)) => o != n,
            (Some(_), None) => true,
            _ => false,
        };
        let my_grants: Vec<&String> = match name {
            Some(n) => (e.grants.iter())
                .filter(|(s, _)| subject_has(&self.doc, s, n))
                .flat_map(|(_, d)| d.iter())
                .collect(),
            None => Vec::new(),
        };
        let norm = |s: &str| s.trim_end_matches('/').to_owned();
        let covered = dirs.iter().any(|d| {
            my_grants.iter().any(|g| {
                let (g, d) = (norm(g), norm(d));
                d == g || d.starts_with(&format!("{g}/"))
            })
        });
        let narrowed_out = !my_grants.is_empty() && !covered;
        let denied = name.is_some_and(|n| {
            (e.deny.iter())
                .any(|(s, prefix)| subject_has(&self.doc, s, n) && key.starts_with(prefix.as_str()))
        });
        let table: [(bool, bool, &'static str); 7] = [
            (store_owner, true, "owner"),
            (logical_owner, true, "logical-owner"),
            (!member, false, "membership"),
            (foreign_personal, false, "visibility"),
            (narrowed_out, false, "role"),
            (denied, false, "acl"),
            (true, true, "allowed"),
        ];
        let (_, allow, step) = table
            .into_iter()
            .find(|(hit, _, _)| *hit)
            .expect("last row");
        (allow, step)
    }
}
