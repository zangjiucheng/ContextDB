//! Signing an entry with the bound member's SSH key (ADR 0018 §7.1, §9.2),
//! in the developer's environment: `ssh-keygen -Y sign -n git`, as git's own
//! SSH signing does, whatever the user's git signing configuration says (the
//! owner's global configuration is OpenPGP, N8). A key path ending in
//! `.pub` names a key held by the SSH agent (`-U`). The signature is checked
//! in-process against the public key before it is used, so a wrong key fails
//! here, never at the root.

use crate::sshsig::{self, PublicKey};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The public key of `key` (its `.pub` line).
pub fn public_key(key: &Path) -> Result<PublicKey, String> {
    let pubpath: PathBuf = if key.extension().is_some_and(|e| e == "pub") {
        key.to_owned()
    } else {
        let mut p = key.as_os_str().to_owned();
        p.push(".pub");
        p.into()
    };
    let line =
        std::fs::read_to_string(&pubpath).map_err(|e| format!("{}: {e}", pubpath.display()))?;
    PublicKey::parse(line.trim()).map_err(|e| format!("{}: {e:?}", pubpath.display()))
}

/// Signs `payload` (namespace `git`) with `key`; returns the armored
/// signature.
pub fn sign(key: &Path, payload: &[u8]) -> Result<String, String> {
    let public = public_key(key)?;
    let dir = std::env::temp_dir().join(format!(
        "ctx-team-sign-{}-{}",
        std::process::id(),
        crate::proposals::fresh_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ));
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| e.to_string())?;
    }
    let file = dir.join("entry");
    let out = (|| {
        std::fs::write(&file, payload).map_err(|e| e.to_string())?;
        let mut cmd = Command::new("ssh-keygen");
        cmd.args(["-Y", "sign", "-n", "git", "-f"]).arg(key);
        if key.extension().is_some_and(|e| e == "pub") {
            cmd.arg("-U");
        }
        let run = cmd
            .arg(&file)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("ssh-keygen: {e}"))?;
        if !run.status.success() {
            return Err(format!(
                "ssh-keygen -Y sign: {}",
                String::from_utf8_lossy(&run.stderr).trim()
            ));
        }
        std::fs::read_to_string(dir.join("entry.sig")).map_err(|e| e.to_string())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    let armored = out?;
    sshsig::verify(&armored, "git", payload, &[public]).map_err(|e| {
        format!(
            "the signature does not verify with {}: {e:?}",
            key.display()
        )
    })?;
    Ok(armored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_keygen_signatures_verify_in_process() {
        if Command::new("ssh-keygen").arg("-?").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ctx-sign-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("id_ed25519");
        let ok = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "test", "-f"])
            .arg(&key)
            .status()
            .unwrap();
        assert!(ok.success());
        let payload = b"tree 0\n\nctx team: entry\n";
        let armored = sign(&key, payload).unwrap();
        let public = public_key(&key).unwrap();
        assert_eq!(
            sshsig::verify(&armored, "git", payload, &[public]).unwrap(),
            public
        );
        // Another key's public half does not verify it.
        let other = dir.join("other");
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "o", "-f"])
            .arg(&other)
            .status()
            .unwrap();
        assert!(sshsig::verify(&armored, "git", payload, &[public_key(&other).unwrap()]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
