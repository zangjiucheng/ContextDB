//! Principals and their OS uids (ADR 0017 §3.1, §3.3).
//!
//! The store owner is the uid the daemon runs as: principals 0, 1 and 2 and
//! every other reserved id act for it. Other principals come from the
//! committed access document ([`map_of`]); until one is mapped the daemon
//! is in single-user mode and behaves as in P8 (T106).
use contextdb_model::PrincipalId;
use contextdb_proto::fsguard::{self, Violation};
use std::collections::BTreeMap;

/// The uid → principal mapping of the current access document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrincipalMap {
    by_uid: BTreeMap<u32, (PrincipalId, String)>,
    by_name: BTreeMap<String, PrincipalId>,
}
impl PrincipalMap {
    /// Builds a map from `(name, uid, id)` entries for principals other than
    /// the store owner, and the store owner's own name (mapped to principal
    /// 0, never to a uid here: the owner's uid is the daemon's).
    pub fn new(
        owner_name: Option<&str>,
        owner_uid: u32,
        entries: impl IntoIterator<Item = (String, u32, PrincipalId)>,
    ) -> Result<Self, String> {
        let mut map = Self::default();
        if let Some(n) = owner_name {
            map.by_name.insert(n.to_owned(), PrincipalId::STORE_OWNER);
        }
        for (name, uid, id) in entries {
            if id.is_reserved() {
                return Err(format!("principal {name}: id {} is reserved", id.0));
            }
            if uid == owner_uid {
                return Err(format!(
                    "principal {name}: uid {uid} is the store owner's; name the owner instead"
                ));
            }
            if map.by_name.insert(name.clone(), id).is_some() {
                return Err(format!("principal {name} is listed twice"));
            }
            if map.by_uid.insert(uid, (id, name.clone())).is_some() {
                return Err(format!("uid {uid} is mapped twice"));
            }
        }
        Ok(map)
    }
    /// The principal a non-owner uid maps to.
    pub fn principal_of_uid(&self, uid: u32) -> Option<PrincipalId> {
        self.by_uid.get(&uid).map(|(p, _)| *p)
    }
    /// The principal a name maps to (the store owner's name maps to 0).
    pub fn principal_of_name(&self, name: &str) -> Option<PrincipalId> {
        self.by_name.get(name).copied()
    }
    /// True when some principal other than the store owner is mapped.
    pub fn is_multi_user(&self) -> bool {
        !self.by_uid.is_empty()
    }
    /// The mapped non-owner uids.
    pub fn uids(&self) -> impl Iterator<Item = u32> + '_ {
        self.by_uid.keys().copied()
    }
}

/// The mapping of the committed access document (ADR 0017 §3.1): every
/// listed principal but the owner, whose entry must carry the daemon's uid.
pub fn map_of(
    access: Option<&contextdb_core::access::AccessState>,
    owner_uid: u32,
) -> Result<PrincipalMap, String> {
    let Some(a) = access else {
        return Ok(PrincipalMap::default());
    };
    if a.doc.principals.get(&a.doc.owner) != Some(&owner_uid) {
        return Err(format!(
            "the owner entry {} does not carry the daemon's uid {owner_uid}",
            a.doc.owner
        ));
    }
    PrincipalMap::new(
        Some(&a.doc.owner),
        owner_uid,
        a.mapped().map(|(n, u, p)| (n.to_owned(), u, p)),
    )
}

/// The file-system side of multi-user mode (ADR 0017 §3.2): the daemon
/// gives the service one when multi-user mode may be entered.
#[derive(Clone, Debug)]
pub struct MultiUser {
    pub dir: std::path::PathBuf,
    /// The socket directory.
    pub run: std::path::PathBuf,
    pub agent_socket: std::path::PathBuf,
    pub owner_uid: u32,
}
impl MultiUser {
    /// The rules a map with other principals needs: the sockets inside
    /// `<data>`, every entry owned by the owner with private modes and no
    /// ACL, safe ancestors, and each uid able to search down to the socket.
    pub fn check(&self, uids: impl Iterator<Item = u32>) -> Vec<Violation> {
        if !self.run.starts_with(&self.dir) {
            return vec![Violation::SocketsOutsideData {
                path: self.run.clone(),
            }];
        }
        let mut v = fsguard::walk(&self.dir, self.owner_uid, 32);
        if let Err(a) = fsguard::check_ancestors(&self.dir, self.owner_uid) {
            v.push(a);
        }
        let shared = self.agent_socket.parent().unwrap_or(&self.dir);
        for u in uids {
            if let Err(e) = fsguard::check_searchable(shared, u) {
                v.push(e);
            }
        }
        v
    }
    /// The agent socket's mode: 0666 with other principals, else 0600.
    pub fn chmod(&self, multi: bool) -> Result<(), Violation> {
        use std::os::unix::fs::PermissionsExt;
        let mode = if multi { 0o666 } else { 0o600 };
        std::fs::set_permissions(&self.agent_socket, std::fs::Permissions::from_mode(mode)).map_err(
            |e| Violation::Io {
                path: self.agent_socket.clone(),
                error: e.to_string(),
            },
        )
    }
}

/// The principal a session, handoff or binding belongs to: every principal
/// acting for the store owner is one class (its agent, observer and admin
/// channels, the validator workers), so the owner's own tools keep sharing
/// sessions as before P9.
pub fn class(p: PrincipalId) -> PrincipalId {
    if p.is_store_owner() {
        PrincipalId::STORE_OWNER
    } else {
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reserved_ids_and_the_owner_uid_are_refused() {
        assert!(PrincipalMap::new(None, 501, [("bob".into(), 502, PrincipalId(3))]).is_err());
        assert!(
            PrincipalMap::new(None, 501, [("bob".into(), 502, PrincipalId(0x5641_0001))]).is_err()
        );
        assert!(PrincipalMap::new(None, 501, [("bob".into(), 501, PrincipalId(16))]).is_err());
        assert!(
            PrincipalMap::new(
                None,
                501,
                [
                    ("bob".into(), 502, PrincipalId(16)),
                    ("carol".into(), 502, PrincipalId(17))
                ]
            )
            .is_err()
        );
        let m =
            PrincipalMap::new(Some("alice"), 501, [("bob".into(), 502, PrincipalId(16))]).unwrap();
        assert_eq!(m.principal_of_uid(502), Some(PrincipalId(16)));
        assert_eq!(m.principal_of_uid(501), None);
        assert_eq!(m.principal_of_name("alice"), Some(PrincipalId::STORE_OWNER));
        assert!(m.is_multi_user());
        assert!(!PrincipalMap::default().is_multi_user());
        assert_eq!(class(PrincipalId(1)), PrincipalId(0));
        assert_eq!(class(PrincipalId(0x5641_0002)), PrincipalId(0));
        assert_eq!(class(PrincipalId(16)), PrincipalId(16));
    }
}
