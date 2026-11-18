//! Where a team's chain lives on the remote (ADR 0018 O4, SPEC SD21.2):
//! custom refs `refs/contextdb/team/<id>/g<n>`, or, for a host that refuses
//! custom refs (or to use branch protection), orphan branches
//! `refs/heads/contextdb-team/<id>/g<n>`. Either way the fetch refspec
//! maps them to the same tracking refs,
//! `refs/contextdb/remotes/<remote>/team/<id>/g<n>`, so import and the
//! announce fence do not depend on the layout.

use crate::commit::hex;

/// Every remote prefix a team chain may use, whatever a store's layout.
pub const REMOTE_PREFIXES: [&str; 2] = ["refs/contextdb/team/", "refs/heads/contextdb-team/"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RefLayout {
    /// `refs/contextdb/team/<id>/g<n>` (the default).
    #[default]
    Refs,
    /// The orphan-branch fallback: `refs/heads/contextdb-team/<id>/g<n>`.
    Branch,
}

impl RefLayout {
    /// The remote prefix before `<id>/g<n>`.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Refs => REMOTE_PREFIXES[0],
            Self::Branch => REMOTE_PREFIXES[1],
        }
    }
    /// The remote ref of generation `g` of `team`.
    pub fn remote_ref(self, team: &[u8; 16], g: u32) -> String {
        format!("{}{}/g{g}", self.prefix(), hex(team))
    }
    /// The fetch refspec a join adds (§7.5).
    pub fn refspec(self, remote: &str) -> String {
        format!("+{}*:refs/contextdb/remotes/{remote}/team/*", self.prefix())
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Refs => "refs",
            Self::Branch => "branch",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "refs" => Some(Self::Refs),
            "branch" => Some(Self::Branch),
            _ => None,
        }
    }
}

/// `<id>/g<n>` of a team chain's remote ref, in either layout.
pub fn strip_remote_prefix(refname: &str) -> Option<&str> {
    REMOTE_PREFIXES.iter().find_map(|p| refname.strip_prefix(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_layouts_map_to_the_same_tracking_refs() {
        let t = [0xab; 16];
        let id = hex(&t);
        assert_eq!(
            RefLayout::Refs.remote_ref(&t, 2),
            format!("refs/contextdb/team/{id}/g2")
        );
        assert_eq!(
            RefLayout::Branch.remote_ref(&t, 2),
            format!("refs/heads/contextdb-team/{id}/g2")
        );
        for l in [RefLayout::Refs, RefLayout::Branch] {
            let spec = l.refspec("origin");
            let (src, dst) = spec.trim_start_matches('+').split_once(':').unwrap();
            assert_eq!(dst, "refs/contextdb/remotes/origin/team/*");
            assert!(l.remote_ref(&t, 0).starts_with(src.trim_end_matches('*')));
            assert_eq!(
                strip_remote_prefix(&l.remote_ref(&t, 0)),
                Some(&*format!("{id}/g0"))
            );
            assert_eq!(RefLayout::parse(l.name()), Some(l));
        }
        assert_eq!(strip_remote_prefix("refs/heads/main"), None);
    }
}
