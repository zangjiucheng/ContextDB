//! The access document and `Authorized(p, l, b)` (ADR 0017 §3.1, §4).
//!
//! `AccessState` is global kernel state: the current document and every
//! principal id ever assigned. Ids are assigned from 16 upwards in order of
//! first appearance across the document history (names in sorted order
//! within one document); a name keeps its first id forever. The store
//! owner's entry maps to principal 0.
use contextdb_model::*;
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

/// The committed access document and the ids it has assigned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessState {
    pub doc: AccessDoc,
    /// Every name ever listed → its id (owner names → 0).
    pub ids: BTreeMap<String, PrincipalId>,
}

/// The decisive step of `Authorized` (ADR 0017 §4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Step 1: the store owner.
    StoreOwner,
    /// Step 1: the logical's owner.
    LogicalOwner,
    /// Step 2: not a member of the lane's entry.
    Membership,
    /// Step 3: another principal's personal lane.
    Visibility,
    /// Step 4: a dir grant that covers none of the logical's dirs.
    Role,
    /// Step 5: a key-prefix deny.
    Acl,
    /// Every step passed.
    Allowed,
}
impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Self::StoreOwner => "owner",
            Self::LogicalOwner => "logical-owner",
            Self::Membership => "membership",
            Self::Visibility => "visibility",
            Self::Role => "role",
            Self::Acl => "acl",
            Self::Allowed => "allowed",
        }
    }
}
/// `Authorized`'s answer and the step that made it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    pub step: Step,
}
/// What `Authorized` reads about a logical: its key, lane and owner
/// (global, immutable) and its `dirs` on the read's branch.
#[derive(Clone, Copy, Debug)]
pub struct LogicalFacts<'a> {
    pub key: &'a str,
    pub lane: &'a str,
    pub owner: PrincipalId,
    pub dirs: &'a BTreeSet<String>,
}

const PERSONAL: &str = "personal:";
const CODE: &str = "code:";

/// A principal name (ADR 0017 §4.2): 1 to 32 of `a-z`, `0-9`, `_`, `-`.
pub fn valid_name(n: &str) -> bool {
    (1..=32).contains(&n.len())
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}
fn bad(reason: impl Into<String>) -> TxError {
    TxError::InvalidAccess {
        reason: reason.into(),
    }
}

/// The document checks of ADR 0017 §4.2 that need no daemon state (the
/// owner uid is checked by the daemon, unknown fields by the parser).
pub fn validate(doc: &AccessDoc) -> Result<(), TxError> {
    if doc.version != 1 {
        return Err(bad(format!("unknown version {}", doc.version)));
    }
    if !doc.principals.contains_key(&doc.owner) {
        return Err(bad(format!("owner {} has no principal entry", doc.owner)));
    }
    let mut uids = BTreeSet::new();
    for (name, uid) in &doc.principals {
        if !valid_name(name) {
            return Err(bad(format!("invalid principal name {name:?}")));
        }
        if !uids.insert(*uid) {
            return Err(bad(format!("uid {uid} is mapped twice")));
        }
    }
    for (g, members) in &doc.groups {
        if !valid_name(g) {
            return Err(bad(format!("invalid group name {g:?}")));
        }
        for m in members {
            if m.starts_with('@') {
                return Err(bad(format!("group {g}: nested group {m}")));
            }
            if !doc.principals.contains_key(m) {
                return Err(bad(format!("group {g}: unknown principal {m}")));
            }
        }
    }
    for (lane, e) in &doc.lanes {
        if lane.is_empty() {
            return Err(bad("empty lane name"));
        }
        if lane.starts_with(PERSONAL) {
            return Err(bad(format!("lane {lane}: personal lanes are implicit")));
        }
        entry_ok(doc, lane, e, lane.starts_with(CODE))?;
    }
    entry_ok(doc, "code_lanes", &doc.code_lanes, true)?;
    entry_ok(doc, "default_lane", &doc.default_lane, false)
}
fn entry_ok(doc: &AccessDoc, lane: &str, e: &LaneEntry, code: bool) -> Result<(), TxError> {
    for (role, list) in [
        ("members", &e.members),
        ("writers", &e.writers),
        ("curators", &e.curators),
    ] {
        for t in list {
            let ok = match t.as_str() {
                "*members" | "*owner" => true,
                "*writers" => role == "curators",
                g if g.starts_with('@') => doc.groups.contains_key(&g[1..]),
                p => doc.principals.contains_key(p),
            };
            if !ok {
                return Err(bad(format!("lane {lane}: {role} names {t:?}")));
            }
        }
    }
    if code
        && expand(doc, &e.writers, None)
            .iter()
            .any(|w| *w != doc.owner)
    {
        return Err(bad(format!(
            "lane {lane}: code lanes are written by *owner only"
        )));
    }
    let members = expand(doc, &e.members, None);
    let who_ok = |who: &str| -> Result<(), TxError> {
        let named = subjects(doc, who);
        if named.is_empty() || named.iter().any(|p| !members.contains(*p)) {
            return Err(bad(format!("lane {lane}: {who} is not a member")));
        }
        Ok(())
    };
    for (who, dirs) in &e.grants {
        who_ok(who)?;
        for d in dirs {
            if d.split('/')
                .any(|s| s.is_empty() || s == "." || s == ".." || s == "*")
            {
                return Err(bad(format!("lane {lane}: grant dir {d:?}")));
            }
        }
    }
    for (who, prefix) in &e.deny {
        who_ok(who)?;
        if prefix.is_empty() {
            return Err(bad(format!("lane {lane}: empty deny prefix")));
        }
    }
    Ok(())
}
/// The principals a grant or deny subject names: a principal or `@group`.
fn subjects<'a>(doc: &'a AccessDoc, who: &str) -> Vec<&'a str> {
    match who.strip_prefix('@') {
        Some(g) => doc
            .groups
            .get(g)
            .map(|m| m.iter().map(String::as_str).collect())
            .unwrap_or_default(),
        None => doc
            .principals
            .get_key_value(who)
            .map(|(k, _)| vec![k.as_str()])
            .unwrap_or_default(),
    }
}
/// A role list's principals; `writers` resolves `*writers`.
fn expand<'a>(
    doc: &'a AccessDoc,
    list: &'a BTreeSet<String>,
    writers: Option<&'a BTreeSet<String>>,
) -> BTreeSet<&'a str> {
    let mut out = BTreeSet::new();
    for t in list {
        match t.as_str() {
            "*members" => out.extend(doc.principals.keys().map(String::as_str)),
            "*owner" => {
                out.insert(doc.owner.as_str());
            }
            "*writers" => {
                if let Some(w) = writers {
                    out.extend(expand(doc, w, None));
                }
            }
            g if g.starts_with('@') => {
                if let Some(m) = doc.groups.get(&g[1..]) {
                    out.extend(m.iter().map(String::as_str));
                }
            }
            p => {
                out.insert(p);
            }
        }
    }
    out
}

impl AccessState {
    /// The state after committing `doc` over `prev`: validation, then id
    /// assignment. A name keeps its first id, so a former owner entry cannot
    /// come back as another principal, nor a mapped name become the owner.
    pub fn apply(prev: Option<&Self>, doc: &AccessDoc) -> Result<Self, TxError> {
        validate(doc)?;
        let mut ids = prev.map(|s| s.ids.clone()).unwrap_or_default();
        let mut next = ids
            .values()
            .map(|p| p.0 + 1)
            .max()
            .unwrap_or(0)
            .max(PrincipalId::FIRST_MAPPED.0);
        for name in doc.principals.keys() {
            let owner = *name == doc.owner;
            match ids.get(name) {
                Some(p) if p.is_store_owner() != owner => {
                    return Err(bad(format!(
                        "principal {name} was {} and cannot become {}",
                        if owner {
                            "another principal"
                        } else {
                            "the store owner"
                        },
                        if owner {
                            "the store owner"
                        } else {
                            "another principal"
                        },
                    )));
                }
                Some(_) => {}
                None if owner => {
                    ids.insert(name.clone(), PrincipalId::STORE_OWNER);
                }
                None => {
                    let id = PrincipalId(next);
                    if id.is_reserved() {
                        return Err(bad("principal ids exhausted"));
                    }
                    ids.insert(name.clone(), id);
                    next += 1;
                }
            }
        }
        Ok(Self {
            doc: doc.clone(),
            ids,
        })
    }
    /// The id of a principal the current document lists.
    pub fn principal_of_name(&self, name: &str) -> Option<PrincipalId> {
        self.doc
            .principals
            .contains_key(name)
            .then(|| self.ids[name])
    }
    /// The current document's name for a non-owner principal.
    pub fn name_of(&self, p: PrincipalId) -> Option<&str> {
        if p.is_store_owner() {
            return Some(&self.doc.owner);
        }
        self.doc
            .principals
            .keys()
            .find(|n| self.ids.get(*n) == Some(&p))
            .map(String::as_str)
    }
    /// `(name, uid, id)` of every listed principal other than the owner.
    pub fn mapped(&self) -> impl Iterator<Item = (&str, u32, PrincipalId)> {
        self.doc
            .principals
            .iter()
            .filter(|(n, _)| **n != self.doc.owner)
            .map(|(n, u)| (n.as_str(), *u, self.ids[n]))
    }
    /// The one entry of a lane (ADR 0017 §4.3).
    pub fn entry(&self, lane: &str) -> Cow<'_, LaneEntry> {
        if let Some(e) = self.doc.lanes.get(lane) {
            return Cow::Borrowed(e);
        }
        if let Some(name) = lane.strip_prefix(PERSONAL) {
            let only: BTreeSet<String> = if self.doc.principals.contains_key(name) {
                [name.to_owned()].into()
            } else {
                BTreeSet::new()
            };
            return Cow::Owned(LaneEntry {
                members: only.clone(),
                writers: only.clone(),
                curators: only,
                egress: self.doc.personal_egress,
                grants: BTreeMap::new(),
                deny: BTreeSet::new(),
            });
        }
        if lane.starts_with(CODE) {
            return Cow::Borrowed(&self.doc.code_lanes);
        }
        Cow::Borrowed(&self.doc.default_lane)
    }
    /// Whether `name` is in `group` (no nesting).
    fn in_group(&self, name: &str, who: &str) -> bool {
        who == name
            || who
                .strip_prefix('@')
                .and_then(|g| self.doc.groups.get(g))
                .is_some_and(|m| m.contains(name))
    }
    /// `Authorized(p, l, b)` (ADR 0017 §4.4), first decisive step wins.
    pub fn authorized(&self, p: PrincipalId, l: &LogicalFacts<'_>) -> Decision {
        let decide = |allowed, step| Decision { allowed, step };
        if p.is_store_owner() {
            return decide(true, Step::StoreOwner);
        }
        if l.owner == p {
            return decide(true, Step::LogicalOwner);
        }
        let Some(name) = self.name_of(p) else {
            return decide(false, Step::Membership);
        };
        let entry = self.entry(l.lane);
        if !expand(&self.doc, &entry.members, None).contains(name) {
            return decide(false, Step::Membership);
        }
        if l.lane.strip_prefix(PERSONAL).is_some_and(|o| o != name) {
            return decide(false, Step::Visibility);
        }
        let grants: Vec<&String> = (entry.grants.iter())
            .filter(|(who, _)| self.in_group(name, who))
            .flat_map(|(_, dirs)| dirs)
            .collect();
        if !grants.is_empty() && !l.dirs.iter().any(|d| grants.iter().any(|g| covers(g, d))) {
            return decide(false, Step::Role);
        }
        if (entry.deny.iter())
            .any(|(who, prefix)| self.in_group(name, who) && l.key.starts_with(prefix.as_str()))
        {
            return decide(false, Step::Acl);
        }
        decide(true, Step::Allowed)
    }
}
/// The lanes a principal may read anything in, besides the logicals it
/// owns ([`AccessState::read_lanes`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadLanes {
    /// Lanes by name: the document's lanes it is a member of and its own
    /// personal lane.
    pub lanes: BTreeSet<String>,
    /// Every `code:` lane the document does not list (`code_lanes`).
    pub code: bool,
}
impl ReadLanes {
    pub fn contains(&self, lane: &str) -> bool {
        self.lanes.contains(lane) || (self.code && lane.starts_with(CODE))
    }
    /// The prefix [`Self::code`] stands for.
    pub const CODE_PREFIX: &'static str = CODE;
}
impl AccessState {
    /// The lanes outside which `p` reads nothing but the logicals it owns,
    /// when they can be listed: `None` for the store owner and for a member
    /// of the default lane (any unlisted lane may then be readable). A
    /// superset of what [`Self::authorized`] allows: grants and denies only
    /// narrow it. Recall uses it to skip, inside the index, the rows `p`
    /// could never read (ADR 0017 §5.3, G5).
    pub fn read_lanes(&self, p: PrincipalId) -> Option<ReadLanes> {
        if p.is_store_owner() {
            return None;
        }
        let Some(name) = self.name_of(p) else {
            return Some(ReadLanes::default());
        };
        let member = |e: &LaneEntry| expand(&self.doc, &e.members, None).contains(name);
        if member(&self.doc.default_lane) {
            return None;
        }
        let mut lanes: BTreeSet<String> = (self.doc.lanes.iter())
            .filter(|(_, e)| member(e))
            .map(|(l, _)| l.clone())
            .collect();
        lanes.insert(format!("{PERSONAL}{name}"));
        Some(ReadLanes {
            lanes,
            code: member(&self.doc.code_lanes),
        })
    }
}
/// [`AccessState::read_lanes`]; without an access document a non-owner
/// reads only the logicals it owns.
pub fn read_lanes(state: Option<&AccessState>, p: PrincipalId) -> Option<ReadLanes> {
    match state {
        Some(s) => s.read_lanes(p),
        None if p.is_store_owner() => None,
        None => Some(ReadLanes::default()),
    }
}
/// A write role of a lane entry (ADR 0017 §4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Writer,
    Curator,
}
impl AccessState {
    /// Whether `p` holds `role` in `entry(lane)` (ADR 0017 §6.2). The store
    /// owner holds every role; `*writers` among curators expands to the
    /// entry's writers.
    pub fn has_role(&self, p: PrincipalId, lane: &str, role: Role) -> bool {
        if p.is_store_owner() {
            return true;
        }
        let Some(name) = self.name_of(p) else {
            return false;
        };
        let e = self.entry(lane);
        match role {
            Role::Writer => expand(&self.doc, &e.writers, None).contains(name),
            Role::Curator => expand(&self.doc, &e.curators, Some(&e.writers)).contains(name),
        }
    }
    /// Whether `lane` names a personal lane of a principal the document
    /// does not list (ADR 0017 §6.2 rule 1: `InvalidLane`).
    pub fn unknown_personal(&self, lane: &str) -> bool {
        lane.strip_prefix(PERSONAL)
            .is_some_and(|n| !self.doc.principals.contains_key(n))
    }
}
impl AccessState {
    /// Whether `lane`'s bodies may leave the machine (ADR 0017 §7): its
    /// entry's `egress` (personal lanes: `[personal] egress`, F6).
    pub fn egress(&self, lane: &str) -> bool {
        self.entry(lane).egress
    }
    /// Whether the document maps a principal besides the store owner.
    pub fn multi_user(&self) -> bool {
        self.mapped().next().is_some()
    }
    /// Whether every principal may read every logical of `lane` (ADR 0017
    /// §7, the mirror): members `*members`, no grants and no denies.
    pub fn readable_by_all(&self, lane: &str) -> bool {
        let e = self.entry(lane);
        e.members.contains("*members") && e.grants.is_empty() && e.deny.is_empty()
    }
    /// Whether the git mirror exports `lane` (ADR 0017 §7): egress, and in
    /// multi-user mode readable by every principal.
    pub fn mirrored(&self, lane: &str) -> bool {
        self.egress(lane) && (!self.multi_user() || self.readable_by_all(lane))
    }
}
/// `egress` without an access document: every lane, as before P9.
pub fn egress(state: Option<&AccessState>, lane: &str) -> bool {
    state.is_none_or(|s| s.egress(lane))
}
/// `mirrored` without an access document: every lane (single-user, as P8).
pub fn mirrored(state: Option<&AccessState>, lane: &str) -> bool {
    state.is_none_or(|s| s.mirrored(lane))
}
/// `has_role` without an access document: only the store owner (T106).
pub fn has_role(state: Option<&AccessState>, p: PrincipalId, lane: &str, role: Role) -> bool {
    match state {
        Some(s) => s.has_role(p, lane, role),
        None => p.is_store_owner(),
    }
}
/// Whether grant dir `g` covers metadata dir `d`: equal, or an ancestor.
fn covers(g: &str, d: &str) -> bool {
    let (g, d) = (g.trim_end_matches('/'), d.trim_end_matches('/'));
    d == g || d.strip_prefix(g).is_some_and(|rest| rest.starts_with('/'))
}
/// `Authorized` without an access document: only the store owner exists (T106).
pub fn authorized(state: Option<&AccessState>, p: PrincipalId, l: &LogicalFacts<'_>) -> Decision {
    match state {
        Some(s) => s.authorized(p, l),
        None if p.is_store_owner() => Decision {
            allowed: true,
            step: Step::StoreOwner,
        },
        None if l.owner == p => Decision {
            allowed: true,
            step: Step::LogicalOwner,
        },
        None => Decision {
            allowed: false,
            step: Step::Membership,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn set(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|x| (*x).to_owned()).collect()
    }
    fn entry(members: &[&str]) -> LaneEntry {
        LaneEntry {
            members: set(members),
            writers: set(members),
            ..LaneEntry::owner_only()
        }
    }
    /// Code lanes are written by the store owner only.
    fn code(members: &[&str]) -> LaneEntry {
        LaneEntry {
            members: set(members),
            ..LaneEntry::owner_only()
        }
    }
    /// `read_lanes` is a superset of `authorized` (ADR 0017 §5.3, G5):
    /// whatever `p` may read lies in its lanes or is its own.
    #[test]
    fn read_lanes_cover_every_authorized_logical() {
        let principals: BTreeMap<String, u32> = [("o", 501), ("a", 1001), ("b", 1002), ("c", 1003)]
            .into_iter()
            .map(|(n, u)| (n.to_owned(), u))
            .collect();
        let mut narrowed = entry(&["a", "b"]);
        narrowed.grants.insert("a".into(), set(&["src/x"]));
        narrowed.deny.insert(("b".into(), "secret.".into()));
        let lanes: BTreeMap<String, LaneEntry> = [
            ("team", entry(&["a", "b"])),
            ("all", entry(&["*members"])),
            ("grp", entry(&["@g"])),
            ("narrow", narrowed),
            ("code:pinned", code(&["b"])),
        ]
        .into_iter()
        .map(|(l, e)| (l.to_owned(), e))
        .collect();
        let docs = [
            (LaneEntry::owner_only(), LaneEntry::code_default()),
            (LaneEntry::owner_only(), LaneEntry::owner_only()),
            (entry(&["c"]), code(&["a"])),
        ];
        let probe = [
            "team",
            "all",
            "grp",
            "narrow",
            "code:pinned",
            "code:repo",
            "personal:a",
            "personal:b",
            "personal:zed",
            "elsewhere",
        ];
        for (default_lane, code_lanes) in docs {
            let doc = AccessDoc {
                version: 1,
                owner: "o".into(),
                principals: principals.clone(),
                groups: [("g".to_owned(), set(&["b", "c"]))].into(),
                lanes: lanes.clone(),
                code_lanes,
                personal_egress: false,
                default_lane,
            };
            let st = AccessState::apply(None, &doc).unwrap();
            let ids: Vec<PrincipalId> = ["o", "a", "b", "c"]
                .iter()
                .map(|n| st.principal_of_name(n).unwrap())
                .chain([PrincipalId(999)])
                .collect();
            for &p in &ids {
                let Some(r) = st.read_lanes(p) else {
                    continue;
                };
                for lane in probe {
                    for owner in &ids {
                        for key in ["k", "secret.k"] {
                            for dirs in [set(&[]), set(&["src/x/y"]), set(&["other"])] {
                                let facts = LogicalFacts {
                                    key,
                                    lane,
                                    owner: *owner,
                                    dirs: &dirs,
                                };
                                if st.authorized(p, &facts).allowed {
                                    assert!(
                                        *owner == p || r.contains(lane),
                                        "{p:?} reads {lane} outside {r:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        // Without a document a non-owner reads only what it owns.
        assert_eq!(
            read_lanes(None, PrincipalId(999)),
            Some(ReadLanes::default())
        );
        assert_eq!(read_lanes(None, PrincipalId::STORE_OWNER), None);
    }
}
