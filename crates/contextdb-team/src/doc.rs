//! The team document (ADR 0018 §9.1, SPEC SD21.4, SD21.6): members and
//! their keys, admins, lanes with writers and curators, the replica's A5
//! policy and the state limit. Written by humans as TOML; carried on the
//! chain, hashed into the digest and stored in checkpoints in one canonical
//! binary form.

use crate::enc::{DecodeError, Reader, Writer, strictly_sorted};
use crate::sshsig::PublicKey;
use contextdb_model::RecordKind;
use std::collections::{BTreeMap, BTreeSet};

pub const DOC_VERSION: u16 = 1;
/// The canonical encoding's bound (SD21.6); larger → `TeamDocTooLarge`.
pub const MAX_DOC_BYTES: usize = 512 * 1024;
pub const DEFAULT_STATE_BYTES: u64 = 1 << 30;
pub const MAX_STATE_BYTES: u64 = 4 << 30;
pub const MAX_NAME: usize = 64;
const MAGIC: &[u8; 4] = b"CXTD";

/// The kinds a team record may have (SD21.4).
pub const TEAM_KINDS: [RecordKind; 5] = [
    RecordKind::Decision,
    RecordKind::Plan,
    RecordKind::Fact,
    RecordKind::Convention,
    RecordKind::Evidence,
];

pub fn kind_name(k: RecordKind) -> Option<&'static str> {
    Some(match k {
        RecordKind::Decision => "decision",
        RecordKind::Plan => "plan",
        RecordKind::Fact => "fact",
        RecordKind::Convention => "convention",
        RecordKind::Evidence => "evidence",
        _ => return None,
    })
}

pub fn kind_from_name(s: &str) -> Option<RecordKind> {
    TEAM_KINDS.into_iter().find(|k| kind_name(*k) == Some(s))
}

/// The one-byte code of a team kind in every canonical encoding.
pub fn kind_code(k: RecordKind) -> Option<u8> {
    TEAM_KINDS.iter().position(|t| *t == k).map(|i| i as u8)
}

pub fn kind_from_code(c: u8) -> Option<RecordKind> {
    TEAM_KINDS.get(c as usize).copied()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamLane {
    /// Member names, `*members` or `*admins`.
    pub writers: BTreeSet<String>,
    pub curators: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamDoc {
    pub version: u16,
    pub accept_version: u32,
    pub id: [u8; 16],
    /// Display only.
    pub name: String,
    pub admins: BTreeSet<String>,
    pub members: BTreeMap<String, BTreeSet<PublicKey>>,
    pub lanes: BTreeMap<String, TeamLane>,
    /// The replica's A5 policy.
    pub require_deps: BTreeSet<RecordKind>,
    pub state_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocError {
    Toml(String),
    Invalid(String),
    TooLarge(usize),
    Decode(&'static str),
}

impl From<DecodeError> for DocError {
    fn from(e: DecodeError) -> Self {
        Self::Decode(e.0)
    }
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, DocError> {
    Err(DocError::Invalid(msg.into()))
}

/// Member and lane names: `[a-z0-9][a-z0-9_-]*`, at most 64 bytes.
pub fn valid_name(s: &str) -> bool {
    let mut b = s.bytes();
    s.len() <= MAX_NAME
        && b.next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

impl TeamDoc {
    pub fn is_admin(&self, member: &str) -> bool {
        self.admins.contains(member)
    }

    /// The member a key belongs to (each key belongs to one member only).
    pub fn member_of(&self, key: &PublicKey) -> Option<&str> {
        self.members
            .iter()
            .find(|(_, keys)| keys.contains(key))
            .map(|(m, _)| m.as_str())
    }

    pub fn keys_of(&self, member: &str) -> Vec<PublicKey> {
        self.members
            .get(member)
            .map(|k| k.iter().copied().collect())
            .unwrap_or_default()
    }

    fn holds(&self, role: &BTreeSet<String>, member: &str) -> bool {
        self.members.contains_key(member)
            && (role.contains(member)
                || role.contains("*members")
                || (role.contains("*admins") && self.is_admin(member)))
    }

    pub fn can_write(&self, member: &str, lane: &str) -> bool {
        self.lanes
            .get(lane)
            .is_some_and(|l| self.holds(&l.writers, member))
    }

    pub fn can_curate(&self, member: &str, lane: &str) -> bool {
        self.lanes
            .get(lane)
            .is_some_and(|l| self.holds(&l.curators, member))
    }

    /// The structural rules of §9.1; the rules that depend on the replica
    /// (a lane with live records, the state limit) are checked by `accept`.
    pub fn validate(&self) -> Result<(), DocError> {
        if self.version != DOC_VERSION {
            return invalid(format!("version {} (expected {DOC_VERSION})", self.version));
        }
        if self.accept_version == 0 {
            return invalid("accept_version must be ≥ 1");
        }
        if self.name.len() > 256 || self.name.contains('\n') {
            return invalid("name");
        }
        if self.state_bytes == 0 || self.state_bytes > MAX_STATE_BYTES {
            return invalid(format!("limits.state_bytes must be 1..={MAX_STATE_BYTES}"));
        }
        if self.admins.is_empty() {
            return invalid("at least one admin");
        }
        let mut owners: BTreeMap<PublicKey, &str> = BTreeMap::new();
        for (m, keys) in &self.members {
            if !valid_name(m) {
                return invalid(format!("member name {m:?}"));
            }
            if keys.is_empty() {
                return invalid(format!("member {m} has no key"));
            }
            for k in keys {
                if let Some(other) = owners.insert(*k, m) {
                    return invalid(format!("a key is listed for both {other} and {m}"));
                }
            }
        }
        for a in &self.admins {
            if !self.members.contains_key(a) {
                return invalid(format!("admin {a} is not a member"));
            }
        }
        for (name, lane) in &self.lanes {
            if !valid_name(name) {
                return invalid(format!("lane name {name:?}"));
            }
            for who in lane.writers.iter().chain(&lane.curators) {
                if who != "*members" && who != "*admins" && !self.members.contains_key(who) {
                    return invalid(format!("lane {name}: {who} is not a member"));
                }
            }
        }
        let size = self.encode().len();
        if size > MAX_DOC_BYTES {
            return Err(DocError::TooLarge(size));
        }
        Ok(())
    }

    /// The canonical binary form.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.raw(MAGIC)
            .u16(self.version)
            .u32(self.accept_version)
            .raw(&self.id)
            .str(&self.name);
        w.count(self.admins.len());
        for a in &self.admins {
            w.str(a);
        }
        w.count(self.members.len());
        for (m, keys) in &self.members {
            w.str(m).count(keys.len());
            for k in keys {
                w.raw(&k.0);
            }
        }
        w.count(self.lanes.len());
        for (name, lane) in &self.lanes {
            w.str(name);
            for set in [&lane.writers, &lane.curators] {
                w.count(set.len());
                for s in set {
                    w.str(s);
                }
            }
        }
        w.count(self.require_deps.len());
        for k in &self.require_deps {
            w.u8(kind_code(*k).expect("validated team kind"));
        }
        w.u64(self.state_bytes);
        w.0
    }

    /// Decodes and validates; the bytes must be the canonical encoding.
    pub fn decode(bytes: &[u8]) -> Result<Self, DocError> {
        if bytes.len() > MAX_DOC_BYTES {
            return Err(DocError::TooLarge(bytes.len()));
        }
        let mut r = Reader(bytes);
        if r.take(4)? != MAGIC {
            return Err(DocError::Decode("magic"));
        }
        let version = r.u16()?;
        let accept_version = r.u32()?;
        let id = r.array()?;
        let name = r.str()?.to_owned();
        let strings = |r: &mut Reader| -> Result<BTreeSet<String>, DocError> {
            let n = r.count(4)?;
            let v: Vec<String> = (0..n)
                .map(|_| r.str().map(str::to_owned))
                .collect::<Result<_, _>>()?;
            if !strictly_sorted(&v) {
                return Err(DocError::Decode("unsorted set"));
            }
            Ok(v.into_iter().collect())
        };
        let admins = strings(&mut r)?;
        let n = r.count(8)?;
        let mut members = BTreeMap::new();
        let mut last: Option<String> = None;
        for _ in 0..n {
            let m = r.str()?.to_owned();
            if last.as_ref().is_some_and(|l| *l >= m) {
                return Err(DocError::Decode("unsorted members"));
            }
            let k = r.count(32)?;
            let keys: Vec<PublicKey> = (0..k)
                .map(|_| r.array().map(PublicKey))
                .collect::<Result<_, _>>()?;
            if !strictly_sorted(&keys) {
                return Err(DocError::Decode("unsorted keys"));
            }
            last = Some(m.clone());
            members.insert(m, keys.into_iter().collect());
        }
        let n = r.count(12)?;
        let mut lanes = BTreeMap::new();
        let mut last: Option<String> = None;
        for _ in 0..n {
            let name = r.str()?.to_owned();
            if last.as_ref().is_some_and(|l| *l >= name) {
                return Err(DocError::Decode("unsorted lanes"));
            }
            let writers = strings(&mut r)?;
            let curators = strings(&mut r)?;
            last = Some(name.clone());
            lanes.insert(name, TeamLane { writers, curators });
        }
        let n = r.count(1)?;
        let codes: Vec<u8> = (0..n).map(|_| r.u8()).collect::<Result<_, _>>()?;
        if !strictly_sorted(&codes) {
            return Err(DocError::Decode("unsorted kinds"));
        }
        let require_deps = codes
            .into_iter()
            .map(|c| kind_from_code(c).ok_or(DocError::Decode("kind")))
            .collect::<Result<_, _>>()?;
        let state_bytes = r.u64()?;
        r.end()?;
        let doc = Self {
            version,
            accept_version,
            id,
            name,
            admins,
            members,
            lanes,
            require_deps,
            state_bytes,
        };
        doc.validate()?;
        Ok(doc)
    }

    /// Parses the TOML a human writes (ADR 0018 §9.1). Unknown keys reject.
    pub fn from_toml(src: &str) -> Result<Self, DocError> {
        let t: toml::Table = src.parse().map_err(|e| DocError::Toml(format!("{e}")))?;
        only(
            &t,
            &[
                "version",
                "accept_version",
                "id",
                "name",
                "admins",
                "members",
                "lanes",
                "policy",
                "limits",
            ],
            "team document",
        )?;
        let int = |t: &toml::Table, k: &str| -> Result<Option<i64>, DocError> {
            match t.get(k) {
                None => Ok(None),
                Some(v) => v
                    .as_integer()
                    .map(Some)
                    .ok_or(DocError::Invalid(format!("{k} must be an integer"))),
            }
        };
        let version = int(&t, "version")?.ok_or(DocError::Invalid("version is required".into()))?;
        let accept_version = int(&t, "accept_version")?
            .ok_or(DocError::Invalid("accept_version is required".into()))?;
        let id = t
            .get("id")
            .and_then(toml::Value::as_str)
            .and_then(crate::commit::parse_id128)
            .ok_or(DocError::Invalid(
                "id must be 32 lowercase hex digits".into(),
            ))?;
        let name = t
            .get("name")
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_owned();
        let admins = str_set(t.get("admins"), "admins")?;
        let mut members = BTreeMap::new();
        if let Some(v) = t.get("members") {
            let mt = v
                .as_table()
                .ok_or(DocError::Invalid("members must be a table".into()))?;
            for (m, entry) in mt {
                let e = entry
                    .as_table()
                    .ok_or(DocError::Invalid(format!("members.{m} must be a table")))?;
                only(e, &["keys"], &format!("members.{m}"))?;
                let mut keys = BTreeSet::new();
                for line in str_set(e.get("keys"), &format!("members.{m}.keys"))? {
                    let k = PublicKey::parse(&line).map_err(|_| {
                        DocError::Invalid(format!("members.{m}: not an ssh-ed25519 key"))
                    })?;
                    if !keys.insert(k) {
                        return invalid(format!("members.{m}: duplicate key"));
                    }
                }
                members.insert(m.clone(), keys);
            }
        }
        let mut lanes = BTreeMap::new();
        if let Some(v) = t.get("lanes") {
            let lt = v
                .as_table()
                .ok_or(DocError::Invalid("lanes must be a table".into()))?;
            for (name, entry) in lt {
                let e = entry
                    .as_table()
                    .ok_or(DocError::Invalid(format!("lanes.{name} must be a table")))?;
                only(e, &["writers", "curators"], &format!("lanes.{name}"))?;
                lanes.insert(
                    name.clone(),
                    TeamLane {
                        writers: str_set(e.get("writers"), "writers")?,
                        curators: str_set(e.get("curators"), "curators")?,
                    },
                );
            }
        }
        let mut require_deps = BTreeSet::new();
        if let Some(v) = t.get("policy") {
            let p = v
                .as_table()
                .ok_or(DocError::Invalid("policy must be a table".into()))?;
            only(p, &["require_deps"], "policy")?;
            for k in str_set(p.get("require_deps"), "policy.require_deps")? {
                require_deps.insert(
                    kind_from_name(&k)
                        .ok_or(DocError::Invalid(format!("{k} is not a team kind")))?,
                );
            }
        }
        let mut state_bytes = DEFAULT_STATE_BYTES;
        if let Some(v) = t.get("limits") {
            let l = v
                .as_table()
                .ok_or(DocError::Invalid("limits must be a table".into()))?;
            only(l, &["state_bytes"], "limits")?;
            if let Some(n) = int(l, "state_bytes")? {
                state_bytes =
                    u64::try_from(n).map_err(|_| DocError::Invalid("state_bytes".into()))?;
            }
        }
        let doc = Self {
            version: u16::try_from(version).map_err(|_| DocError::Invalid("version".into()))?,
            accept_version: u32::try_from(accept_version)
                .map_err(|_| DocError::Invalid("accept_version".into()))?,
            id,
            name,
            admins,
            members,
            lanes,
            require_deps,
            state_bytes,
        };
        doc.validate()?;
        Ok(doc)
    }
}

fn only(t: &toml::Table, allowed: &[&str], what: &str) -> Result<(), DocError> {
    match t.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => invalid(format!("{what}: unknown key {k}")),
        None => Ok(()),
    }
}

fn str_set(v: Option<&toml::Value>, what: &str) -> Result<BTreeSet<String>, DocError> {
    let Some(v) = v else {
        return Ok(BTreeSet::new());
    };
    let arr = v
        .as_array()
        .ok_or(DocError::Invalid(format!("{what} must be an array")))?;
    let mut out = BTreeSet::new();
    for s in arr {
        let s = s
            .as_str()
            .ok_or(DocError::Invalid(format!("{what} holds strings")))?;
        if !out.insert(s.to_owned()) {
            return invalid(format!("{what}: duplicate {s}"));
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sshsig::public_key;

    pub fn sample_toml() -> String {
        format!(
            r#"version = 1
accept_version = 1
id = "00112233445566778899aabbccddeeff"
name = "acme-backend"
admins = ["alice"]

[members.alice]
keys = ["{} alice@laptop"]
[members.bob]
keys = ["{}"]

[lanes.conventions]
writers = ["*members"]
curators = ["alice"]
[lanes.architecture]
writers = ["alice", "bob"]
curators = ["*admins"]

[policy]
require_deps = []
"#,
            public_key(&[1; 32]).to_line(),
            public_key(&[2; 32]).to_line()
        )
    }

    #[test]
    fn toml_parse_roles_and_canonical_round_trip() {
        let d = TeamDoc::from_toml(&sample_toml()).unwrap();
        assert_eq!(d.state_bytes, DEFAULT_STATE_BYTES);
        assert_eq!(d.member_of(&public_key(&[2; 32])), Some("bob"));
        assert!(d.can_write("bob", "conventions") && d.can_write("bob", "architecture"));
        assert!(!d.can_curate("bob", "architecture") && d.can_curate("alice", "architecture"));
        assert!(!d.can_write("carol", "conventions") && !d.can_write("bob", "nope"));
        let bytes = d.encode();
        assert_eq!(TeamDoc::decode(&bytes).unwrap(), d);
        // A trailing byte, or any other encoding, is refused.
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(TeamDoc::decode(&extra).is_err());
    }

    #[test]
    fn structural_rules() {
        let base = sample_toml();
        let shared = base.replace(
            &public_key(&[2; 32]).to_line(),
            &public_key(&[1; 32]).to_line(),
        );
        for (name, src) in [
            ("shared key", shared),
            (
                "no admin",
                base.replace(r#"admins = ["alice"]"#, "admins = []"),
            ),
            (
                "admin not member",
                base.replace(r#"admins = ["alice"]"#, r#"admins = ["carol"]"#),
            ),
            (
                "unknown writer",
                base.replace(r#"writers = ["alice", "bob"]"#, r#"writers = ["carol"]"#),
            ),
            ("unknown key", format!("{base}\nextra = 1\n")),
            (
                "bad kind",
                base.replace("require_deps = []", r#"require_deps = ["summary"]"#),
            ),
            (
                "huge limit",
                format!("{base}\n[limits]\nstate_bytes = 5000000000\n"),
            ),
            (
                "bad id",
                base.replace("00112233445566778899aabbccddeeff", "XYZ"),
            ),
            (
                "bad lane name",
                base.replace("[lanes.conventions]", "[lanes.Conventions]"),
            ),
        ] {
            assert!(TeamDoc::from_toml(&src).is_err(), "{name} accepted");
        }
    }
}
