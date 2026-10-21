//! Daemon-side lease helpers; the discovery record itself is shared with
//! clients in `contextdb_proto::lease`.
pub use contextdb_proto::lease::*;
use std::{io, path::Path};

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
