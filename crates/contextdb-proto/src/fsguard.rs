//! File-system isolation (ADR 0017 §3.2, G-FS): ownership, modes, ACLs and
//! the data directory's ancestors. Shared by `ctxd` (the start-up walk, the
//! ancestor rules) and its clients (the ancestor ownership rule before
//! trusting a server, `ServerNotOwner`).
//!
//! The rules hold only in multi-user mode (some principal other than the
//! store owner is mapped). In single-user mode nothing here runs.
use std::{
    fmt, io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// Why a data directory or one of its ancestors breaks isolation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    /// An entry in `<data>` (or `<data>` itself) is owned by another uid.
    ForeignOwner {
        path: PathBuf,
        uid: u32,
    },
    /// An entry grants group or other more than its role allows.
    Mode {
        path: PathBuf,
        mode: u32,
        allowed: u32,
    },
    /// An entry carries an ACL entry (allow or deny, explicit or inherited).
    Acl {
        path: PathBuf,
    },
    /// An ancestor is owned by someone other than root or the store owner.
    AncestorOwner {
        path: PathBuf,
        uid: u32,
    },
    /// An ancestor is group- or other-writable and not sticky.
    AncestorWritable {
        path: PathBuf,
        mode: u32,
    },
    /// A mapped uid cannot search this directory (a mode check; ACLs on
    /// ancestors are left to the real two-user run, G6).
    NotSearchable {
        path: PathBuf,
        uid: u32,
    },
    /// The socket directory is outside `<data>` (the long-path fallback).
    SocketsOutsideData {
        path: PathBuf,
    },
    Io {
        path: PathBuf,
        error: String,
    },
}
impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignOwner { path, uid } => {
                write!(
                    f,
                    "{} is owned by uid {uid}, not the store owner",
                    path.display()
                )
            }
            Self::Mode {
                path,
                mode,
                allowed,
            } => write!(
                f,
                "{} has mode {mode:04o}; group and other may have at most {allowed:04o}",
                path.display()
            ),
            Self::Acl { path } => write!(
                f,
                "{} carries an ACL; no ACL is allowed inside the data directory (ctx doctor --fix-modes)",
                path.display()
            ),
            Self::AncestorOwner { path, uid } => write!(
                f,
                "ancestor {} is owned by uid {uid}; it must be owned by root or the store owner",
                path.display()
            ),
            Self::AncestorWritable { path, mode } => write!(
                f,
                "ancestor {} has mode {mode:04o}: writable by others without the sticky bit",
                path.display()
            ),
            Self::NotSearchable { path, uid } => {
                write!(f, "uid {uid} cannot search {}", path.display())
            }
            Self::SocketsOutsideData { path } => write!(
                f,
                "the socket directory {} is outside the data directory (path too long); use a shorter data directory",
                path.display()
            ),
            Self::Io { path, error } => write!(f, "{}: {error}", path.display()),
        }
    }
}
fn io_violation(path: &Path, e: &io::Error) -> Violation {
    Violation::Io {
        path: path.to_path_buf(),
        error: e.to_string(),
    }
}

/// The group/other bits each public entry may have; everything else in
/// `<data>` may have none (0700 directories, 0600 files).
pub fn public_entries(data: &Path) -> Vec<(PathBuf, u32)> {
    let shared = data.join("run").join("shared");
    vec![
        (data.to_path_buf(), 0o011),
        (data.join("run"), 0o011),
        (shared.clone(), 0o011),
        (shared.join(crate::lease::ENDPOINT), 0o044),
        (shared.join("agent.sock"), 0o066),
    ]
}

/// The start-up walk (ADR 0017 §3.2): every entry under `data` (not
/// following symlinks) must be owned by `uid`, carry no ACL, and grant group
/// and other no more than `public_entries` allows. Returns every violation
/// found, in walk order, up to `limit`.
pub fn walk(data: &Path, uid: u32, limit: usize) -> Vec<Violation> {
    let public = public_entries(data);
    let mut out = Vec::new();
    let mut stack = vec![data.to_path_buf()];
    while let Some(p) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        let m = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            // Removed while walking (a temp file): nothing to check.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                out.push(io_violation(&p, &e));
                continue;
            }
        };
        if m.uid() != uid {
            out.push(Violation::ForeignOwner {
                path: p.clone(),
                uid: m.uid(),
            });
        }
        if !m.file_type().is_symlink() {
            let allowed = public.iter().find(|(q, _)| *q == p).map_or(0, |(_, a)| *a);
            let mode = m.permissions().mode() & 0o7777;
            if mode & 0o077 & !allowed != 0 {
                out.push(Violation::Mode {
                    path: p.clone(),
                    mode,
                    allowed,
                });
            }
        }
        match has_acl(&p) {
            Ok(true) => out.push(Violation::Acl { path: p.clone() }),
            Ok(false) => {}
            Err(e) => out.push(io_violation(&p, &e)),
        }
        if m.is_dir() {
            match std::fs::read_dir(&p) {
                Ok(entries) => {
                    let mut children: Vec<PathBuf> =
                        entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
                    children.sort();
                    stack.extend(children.into_iter().rev());
                }
                Err(e) => out.push(io_violation(&p, &e)),
            }
        }
    }
    out.truncate(limit);
    out
}

/// `ctx doctor --fix-modes`: removes group/other bits beyond the public
/// entries' and strips every ACL under `data`. It never changes an owner;
/// foreign-owned entries are returned for the store owner to resolve.
pub fn fix_modes(data: &Path, uid: u32) -> io::Result<(usize, Vec<Violation>)> {
    let mut fixed = 0;
    let mut foreign = Vec::new();
    for v in walk(data, uid, usize::MAX) {
        match v {
            Violation::Mode {
                path,
                mode,
                allowed,
            } => {
                let new = mode & !(0o077 & !allowed);
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(new))?;
                fixed += 1;
            }
            Violation::Acl { path } => {
                strip_acl(&path)?;
                fixed += 1;
            }
            other => foreign.push(other),
        }
    }
    Ok((fixed, foreign))
}

/// The ancestor ownership rule (ssh `StrictModes` style, ADR 0017 §3.2):
/// every ancestor of `data` is owned by root or `owner`, and is not group-
/// or other-writable unless it is sticky (the next component is itself
/// checked to be owned by root or `owner`). `data` must be canonical.
pub fn check_ancestors(data: &Path, owner: u32) -> Result<(), Violation> {
    let mut ancestors: Vec<&Path> = data.ancestors().skip(1).collect();
    ancestors.reverse();
    for a in ancestors {
        let m = std::fs::metadata(a).map_err(|e| io_violation(a, &e))?;
        if m.uid() != 0 && m.uid() != owner {
            return Err(Violation::AncestorOwner {
                path: a.to_path_buf(),
                uid: m.uid(),
            });
        }
        let mode = m.permissions().mode() & 0o7777;
        if mode & 0o022 != 0 && mode & 0o1000 == 0 {
            return Err(Violation::AncestorWritable {
                path: a.to_path_buf(),
                mode,
            });
        }
    }
    let m = std::fs::metadata(data).map_err(|e| io_violation(data, &e))?;
    if m.uid() != owner {
        return Err(Violation::ForeignOwner {
            path: data.to_path_buf(),
            uid: m.uid(),
        });
    }
    Ok(())
}

/// Every ancestor of `data`, and `data` itself, must be searchable by `uid`
/// through its mode bits (other `x`, group `x` with one of `uid`'s groups,
/// or owner `x`). ACL denies are not evaluated: the real two-user run is the
/// authority (G6).
pub fn check_searchable(data: &Path, uid: u32) -> Result<(), Violation> {
    let groups = groups_of(uid);
    for a in data.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let m = std::fs::metadata(a).map_err(|e| io_violation(a, &e))?;
        let mode = m.permissions().mode();
        let ok = (m.uid() == uid && mode & 0o100 != 0)
            || (groups.contains(&m.gid()) && mode & 0o010 != 0)
            || mode & 0o001 != 0
            || uid == 0;
        if !ok {
            return Err(Violation::NotSearchable {
                path: a.to_path_buf(),
                uid,
            });
        }
    }
    Ok(())
}

/// The groups of `uid` (primary and supplementary), from `id -G`, which
/// reads the same user database the kernel's access checks use.
pub fn groups_of(uid: u32) -> Vec<u32> {
    let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) else {
        return Vec::new();
    };
    let mut out = vec![user.gid.as_raw()];
    for id in ["/usr/bin/id", "/bin/id", "id"] {
        if let Ok(o) = std::process::Command::new(id)
            .arg("-G")
            .arg(&user.name)
            .env("LC_ALL", "C")
            .output()
            && o.status.success()
        {
            out.extend(
                String::from_utf8_lossy(&o.stdout)
                    .split_whitespace()
                    .filter_map(|g| g.parse::<u32>().ok()),
            );
            break;
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(target_os = "macos")]
mod acl {
    use exacl::AclOption;
    use std::{io, path::Path};
    pub fn has_acl(p: &Path) -> io::Result<bool> {
        Ok(!exacl::getfacl(p, AclOption::SYMLINK_ACL)?.is_empty())
    }
    pub fn strip_acl(p: &Path) -> io::Result<()> {
        exacl::setfacl(&[p], &[], AclOption::SYMLINK_ACL)
    }
}
#[cfg(not(target_os = "macos"))]
mod acl {
    use std::{io, path::Path};
    /// POSIX ACLs live in these xattrs; a file whose access ACL is only the
    /// mode bits stores none.
    const NAMES: [&str; 2] = ["system.posix_acl_access", "system.posix_acl_default"];
    fn absent(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(61) | Some(95)) // ENODATA, EOPNOTSUPP
    }
    pub fn has_acl(p: &Path) -> io::Result<bool> {
        if std::fs::symlink_metadata(p)?.file_type().is_symlink() {
            return Ok(false); // Linux symlinks carry no ACL
        }
        for n in NAMES {
            match xattr::get_deref(p, n) {
                Ok(Some(_)) => return Ok(true),
                Ok(None) => {}
                Err(e) if absent(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }
    pub fn strip_acl(p: &Path) -> io::Result<()> {
        if std::fs::symlink_metadata(p)?.file_type().is_symlink() {
            return Ok(());
        }
        for n in NAMES {
            match xattr::remove_deref(p, n) {
                Ok(()) => {}
                Err(e) if absent(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
/// True when `p` (not followed if a symlink) carries any ACL entry.
pub fn has_acl(p: &Path) -> io::Result<bool> {
    acl::has_acl(p)
}
/// Removes every ACL entry of `p` (not followed if a symlink).
pub fn strip_acl(p: &Path) -> io::Result<()> {
    acl::strip_acl(p)
}

/// The calling process's real uid.
pub fn current_uid() -> u32 {
    nix::unistd::getuid().as_raw()
}
/// Sets the process umask to 077 (ADR 0017 §3.2): every binary that may
/// write the data directory calls this first, so files other code creates
/// there (SQLite's `-wal`, `-shm` and journals, the lease) are private too.
pub fn private_umask() {
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
}
/// The peer uid of a connected Unix socket (`getpeereid` / `SO_PEERCRED`).
pub fn peer_uid(stream: &std::os::unix::net::UnixStream) -> io::Result<u32> {
    #[cfg(target_os = "macos")]
    {
        let (uid, _) = nix::unistd::getpeereid(stream).map_err(io::Error::from)?;
        Ok(uid.as_raw())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let cred = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
            .map_err(io::Error::from)?;
        Ok(cred.uid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ctx-fsguard-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn chmod(p: &Path, m: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    }
    #[test]
    fn walk_accepts_private_entries_and_flags_modes() {
        let uid = nix::unistd::getuid().as_raw();
        let d = tmp("walk");
        chmod(&d, 0o711);
        std::fs::create_dir_all(d.join("run/shared")).unwrap();
        chmod(&d.join("run"), 0o711);
        chmod(&d.join("run/shared"), 0o711);
        std::fs::write(d.join("run/shared/endpoint"), "x").unwrap();
        chmod(&d.join("run/shared/endpoint"), 0o644);
        std::fs::write(d.join("blob"), "x").unwrap();
        chmod(&d.join("blob"), 0o600);
        assert_eq!(walk(&d, uid, 10), vec![]);
        chmod(&d.join("blob"), 0o640);
        chmod(&d.join("run/shared/endpoint"), 0o666);
        let v = walk(&d, uid, 10);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(matches!(&v[0], Violation::Mode { path, .. } if path.ends_with("blob")));
        assert!(matches!(&v[1], Violation::Mode { path, .. } if path.ends_with("endpoint")));
        let (fixed, foreign) = fix_modes(&d, uid).unwrap();
        assert_eq!((fixed, foreign), (2, vec![]));
        assert_eq!(walk(&d, uid, 10), vec![]);
        assert!(matches!(
            walk(&d, uid + 1, 1).as_slice(),
            [Violation::ForeignOwner { .. }]
        ));
        let _ = std::fs::remove_dir_all(&d);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn acls_are_found_and_stripped_including_inherited_ones() {
        let uid = nix::unistd::getuid().as_raw();
        let d = tmp("acl");
        chmod(&d, 0o700);
        let sub = d.join("sub");
        std::fs::create_dir(&sub).unwrap();
        chmod(&sub, 0o700);
        assert!(!has_acl(&sub).unwrap());
        // An inheritable allow entry for everyone: copied onto new files.
        let ok = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read,file_inherit,directory_inherit"])
            .arg(&sub)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        std::fs::write(sub.join("f"), "x").unwrap();
        chmod(&sub.join("f"), 0o600);
        assert!(has_acl(&sub).unwrap());
        assert!(has_acl(&sub.join("f")).unwrap(), "inherited");
        let v = walk(&d, uid, 10);
        assert_eq!(
            v.iter()
                .filter(|v| matches!(v, Violation::Acl { .. }))
                .count(),
            2,
            "{v:?}"
        );
        // A deny-only entry is refused too.
        strip_acl(&sub.join("f")).unwrap();
        strip_acl(&sub).unwrap();
        assert_eq!(walk(&d, uid, 10), vec![]);
        let ok = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone deny delete"])
            .arg(&sub)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert!(matches!(
            walk(&d, uid, 10).as_slice(),
            [Violation::Acl { .. }]
        ));
        let (fixed, _) = fix_modes(&d, uid).unwrap();
        assert_eq!(fixed, 1);
        assert!(!has_acl(&sub).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn ancestors_follow_strict_modes() {
        let uid = nix::unistd::getuid().as_raw();
        let d = tmp("anc");
        let data = d.join("a").join("data");
        std::fs::create_dir_all(&data).unwrap();
        chmod(&d, 0o755);
        chmod(&d.join("a"), 0o755);
        // The temp dir itself is sticky (macOS per-user temp is 0700 and
        // owned by the user): either way the chain passes.
        assert_eq!(check_ancestors(&data, uid), Ok(()));
        chmod(&d.join("a"), 0o777);
        assert!(matches!(
            check_ancestors(&data, uid),
            Err(Violation::AncestorWritable { .. })
        ));
        chmod(&d.join("a"), 0o1777);
        assert_eq!(check_ancestors(&data, uid), Ok(()), "sticky is allowed");
        chmod(&d.join("a"), 0o755);
        assert!(matches!(
            check_ancestors(&data, uid + 1),
            Err(Violation::AncestorOwner { .. } | Violation::ForeignOwner { .. })
        ));
        assert!(groups_of(uid).contains(&nix::unistd::getgid().as_raw()));
        let _ = std::fs::remove_dir_all(&d);
    }
}
