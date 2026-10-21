//! Single-instance lease (ADR 0008 §2, T37). The store flock is the only
//! exclusion; the lease file is a discovery record for clients.
use std::{
    io,
    path::{Path, PathBuf},
};

/// Filesystem families `ctxd` refuses (network or virtual). Linux reports a
/// magic number, macOS a type name; the check is pure so tests inject values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsId {
    Magic(u64),
    Name(String),
}
const LINUX_REJECTED: &[(u64, &str)] = &[
    (0x6969, "nfs"),
    (0x517B, "smb"),
    (0xFE53_4D42, "smb2"),
    (0xFF53_4D42, "cifs"),
    (0x6573_5546, "fuse"),
    (0x0102_1997, "9p"),
    (0x5346_414F, "afs"),
    (0x0027_E0EB, "cgroup"),
    (0x00C3_6400, "ceph"),
    (0x7375_7245, "coda"),
];
const NAME_REJECTED: &[&str] = &[
    "nfs", "smbfs", "cifs", "afpfs", "webdav", "macfuse", "osxfuse", "fusefs", "fuse", "fuseblk",
    "9p", "virtiofs", "drvfs", "ceph",
];
/// `Err(name)` when `fs` is a network or virtual filesystem not in `allow`.
pub fn check_filesystem(fs: &FsId, allow: &[String]) -> Result<(), String> {
    let name = match fs {
        FsId::Magic(m) => LINUX_REJECTED
            .iter()
            .find(|(magic, _)| magic == m)
            .map(|(_, n)| (*n).to_owned()),
        FsId::Name(n) => NAME_REJECTED
            .iter()
            .find(|r| n.eq_ignore_ascii_case(r) || n.starts_with(&format!("{r}.")))
            .map(|_| n.clone()),
    };
    match name {
        Some(n) if !allow.iter().any(|a| a.eq_ignore_ascii_case(&n)) => Err(n),
        _ => Ok(()),
    }
}
/// The filesystem identity of `dir`.
pub fn filesystem_of(dir: &Path) -> io::Result<FsId> {
    let st = nix::sys::statfs::statfs(dir).map_err(io::Error::from)?;
    #[cfg(target_os = "macos")]
    {
        Ok(FsId::Name(st.filesystem_type_name().to_owned()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(FsId::Magic(st.filesystem_type().0 as u64))
    }
}
/// Capability probe: a second open file description must not get the lock
/// while the store holds it (flock is per open file description).
pub fn probe_flock(lock: &Path) -> io::Result<()> {
    let file = std::fs::OpenOptions::new().write(true).open(lock)?;
    match file.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => Ok(()),
        Ok(()) => Err(io::Error::other(
            "flock does not exclude on this filesystem (capability probe)",
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}
/// Discovery record, written after the flock is held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub pid: u32,
    pub start_time: String,
    pub protocol: u16,
    pub sockets: Vec<PathBuf>,
}
impl Lease {
    pub fn render(&self) -> String {
        let mut s = format!(
            "pid={}\nstart_time={}\nprotocol={}\n",
            self.pid, self.start_time, self.protocol
        );
        for p in &self.sockets {
            s.push_str(&format!("socket={}\n", p.display()));
        }
        s
    }
    pub fn parse(text: &str) -> Option<Self> {
        let (mut pid, mut start, mut protocol, mut sockets) = (None, None, None, Vec::new());
        for line in text.lines() {
            let (k, v) = line.split_once('=')?;
            match k {
                "pid" => pid = v.parse().ok(),
                "start_time" => start = Some(v.to_owned()),
                "protocol" => protocol = v.parse().ok(),
                "socket" => sockets.push(PathBuf::from(v)),
                _ => {}
            }
        }
        Some(Self {
            pid: pid?,
            start_time: start?,
            protocol: protocol?,
            sockets,
        })
    }
    /// Live when the named pid exists with the recorded start time (pid reuse
    /// gives a different start time).
    pub fn is_live(&self) -> bool {
        process_start_time(self.pid).is_some_and(|t| t == self.start_time)
    }
}
/// A process's start time as reported by `ps` (portable across macOS and Linux).
pub fn process_start_time(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let t = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.status.success() && !t.is_empty()).then_some(t)
}
/// Writes `lease` durably via temp file, sync and rename.
pub fn write_lease(path: &Path, lease: &Lease) -> io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(lease.render().as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}
pub fn read_lease(path: &Path) -> Option<Lease> {
    Lease::parse(&std::fs::read_to_string(path).ok()?)
}
/// Platform `sun_path` limit (bytes, without the NUL).
pub const SUN_PATH_MAX: usize = if cfg!(target_os = "macos") { 103 } else { 107 };
/// Socket directory: `<dir>/run`, or a short per-uid directory when socket
/// paths would exceed `sun_path` (ADR 0008 §2.6).
pub fn socket_dir(dir: &Path, uid: u32) -> PathBuf {
    let run = dir.join("run");
    if run.join("observer.sock").as_os_str().len() <= SUN_PATH_MAX {
        return run;
    }
    let tmp = std::env::temp_dir();
    let hash = fnv_hex(dir.as_os_str().as_encoded_bytes());
    tmp.join(format!("ctxd-{uid}")).join(&hash[..16])
}
fn fnv_hex(bytes: &[u8]) -> String {
    // FNV-1a 64 twice with different seeds: short, stable, dependency-free.
    let h = |seed: u64| {
        bytes.iter().fold(seed, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
        })
    };
    format!(
        "{:016x}{:016x}",
        h(0xcbf2_9ce4_8422_2325),
        h(0x8422_2325_cbf2_9ce4)
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_and_virtual_filesystems_are_refused_unless_allowed() {
        assert_eq!(
            check_filesystem(&FsId::Magic(0x6969), &[]),
            Err("nfs".into())
        );
        assert_eq!(
            check_filesystem(&FsId::Magic(0x0102_1997), &[]),
            Err("9p".into())
        );
        assert_eq!(check_filesystem(&FsId::Magic(0xEF53), &[]), Ok(())); // ext4
        assert_eq!(
            check_filesystem(&FsId::Name("smbfs".into()), &[]),
            Err("smbfs".into())
        );
        assert_eq!(
            check_filesystem(&FsId::Name("macfuse".into()), &[]),
            Err("macfuse".into())
        );
        assert_eq!(check_filesystem(&FsId::Name("apfs".into()), &[]), Ok(()));
        assert_eq!(
            check_filesystem(&FsId::Magic(0x6969), &["nfs".into()]),
            Ok(()),
            "--allow-fs overrides"
        );
    }
    #[test]
    fn lease_round_trips_and_detects_dead_or_reused_pids() {
        let me = std::process::id();
        let lease = Lease {
            pid: me,
            start_time: process_start_time(me).unwrap(),
            protocol: 1,
            sockets: vec!["/tmp/a.sock".into()],
        };
        assert_eq!(Lease::parse(&lease.render()), Some(lease.clone()));
        assert!(lease.is_live());
        let reused = Lease {
            start_time: "Thu Jan  1 00:00:00 1970".into(),
            ..lease.clone()
        };
        assert!(!reused.is_live(), "pid reuse: different start time");
    }
    #[test]
    fn long_data_directories_get_short_socket_paths() {
        let deep = PathBuf::from(format!("/tmp/{}", "d".repeat(200)));
        let dir = socket_dir(&deep, 501);
        assert!(dir.join("observer.sock").as_os_str().len() <= SUN_PATH_MAX);
        assert_eq!(
            socket_dir(Path::new("/tmp/x"), 501),
            PathBuf::from("/tmp/x/run")
        );
    }
}
