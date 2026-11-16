//! In-process verification of OpenSSH `sshsig` signatures (PROTOCOL.sshsig)
//! as `git` writes them with `gpg.format=ssh` (ADR 0018 §3.1, SPEC SD21.3).
//!
//! Only plain `ssh-ed25519` keys are accepted (security keys and every other
//! algorithm are refused), the namespace must be `git`, the reserved field
//! empty and the hash `sha256` or `sha512`. Nothing depends on time: there
//! are no validity windows or principals files, so a verdict is a pure
//! function of the bytes and the key list.

use crate::b64;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256, Sha512};

const KEY_TYPE: &str = "ssh-ed25519";
const MAGIC: &[u8; 6] = b"SSHSIG";
const BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";
const END: &str = "-----END SSH SIGNATURE-----";

/// An ed25519 public key as a team document lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicKey(pub [u8; 32]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SigError {
    /// Not `ssh-ed25519 <base64> [comment]`, or another key type.
    BadPublicKey,
    /// The armor, wire encoding or a field is malformed or non-canonical.
    Malformed(&'static str),
    UnsupportedKey,
    WrongNamespace,
    UnsupportedHash,
    /// The signing key is not one of the allowed keys.
    UnknownKey,
    /// The signature does not verify.
    BadSignature,
}

impl PublicKey {
    /// Parses an OpenSSH public key line: `ssh-ed25519 <base64> [comment]`.
    pub fn parse(line: &str) -> Result<Self, SigError> {
        let mut parts = line.split(' ');
        let (Some(ty), Some(data)) = (parts.next(), parts.next()) else {
            return Err(SigError::BadPublicKey);
        };
        if ty != KEY_TYPE {
            return Err(SigError::BadPublicKey);
        }
        let blob = b64::decode(data.as_bytes()).ok_or(SigError::BadPublicKey)?;
        Self::from_wire(&blob).map_err(|_| SigError::BadPublicKey)
    }

    /// The SSH wire form: `string "ssh-ed25519", string key(32)`.
    fn from_wire(blob: &[u8]) -> Result<Self, SigError> {
        let mut r = Reader(blob);
        if r.string()? != KEY_TYPE.as_bytes() {
            return Err(SigError::UnsupportedKey);
        }
        let key: [u8; 32] = r
            .string()?
            .try_into()
            .map_err(|_| SigError::Malformed("key length"))?;
        r.end()?;
        Ok(Self(key))
    }

    pub fn to_wire(self) -> Vec<u8> {
        let mut out = Vec::new();
        put_string(&mut out, KEY_TYPE.as_bytes());
        put_string(&mut out, &self.0);
        out
    }

    /// `ssh-ed25519 <base64>`, without a comment.
    pub fn to_line(self) -> String {
        format!("{KEY_TYPE} {}", b64::encode(&self.to_wire()))
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SigError> {
        if self.0.len() < n {
            return Err(SigError::Malformed("truncated"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u32(&mut self) -> Result<u32, SigError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn string(&mut self) -> Result<&'a [u8], SigError> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn end(&self) -> Result<(), SigError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(SigError::Malformed("trailing bytes"))
        }
    }
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

/// Strips the armor: the exact BEGIN line, base64 lines, the exact END line,
/// each terminated by `\n` (a final newline is optional).
fn unarmor(armored: &str) -> Result<Vec<u8>, SigError> {
    let body = armored.strip_suffix('\n').unwrap_or(armored);
    let mut lines = body.split('\n');
    if lines.next() != Some(BEGIN) {
        return Err(SigError::Malformed("armor begin"));
    }
    let mut b64 = Vec::new();
    let mut ended = false;
    for line in lines {
        if ended {
            return Err(SigError::Malformed("after armor end"));
        }
        if line == END {
            ended = true;
        } else {
            b64.extend_from_slice(line.as_bytes());
        }
    }
    if !ended {
        return Err(SigError::Malformed("armor end"));
    }
    b64::decode(&b64).ok_or(SigError::Malformed("base64"))
}

/// The message the key signs (PROTOCOL.sshsig): magic, namespace, reserved,
/// hash algorithm and the hash of the signed data.
fn signed_blob(namespace: &str, hash_alg: &str, message: &[u8]) -> Result<Vec<u8>, SigError> {
    let hash = match hash_alg {
        "sha256" => Sha256::digest(message).to_vec(),
        "sha512" => Sha512::digest(message).to_vec(),
        _ => return Err(SigError::UnsupportedHash),
    };
    let mut out = MAGIC.to_vec();
    put_string(&mut out, namespace.as_bytes());
    put_string(&mut out, b"");
    put_string(&mut out, hash_alg.as_bytes());
    put_string(&mut out, &hash);
    Ok(out)
}

/// Verifies an armored `sshsig` over `message` in `namespace` and returns
/// the signing key, which must be one of `allowed`.
pub fn verify(
    armored: &str,
    namespace: &str,
    message: &[u8],
    allowed: &[PublicKey],
) -> Result<PublicKey, SigError> {
    let blob = unarmor(armored)?;
    let mut r = Reader(&blob);
    if r.take(6)? != MAGIC {
        return Err(SigError::Malformed("magic"));
    }
    if r.u32()? != 1 {
        return Err(SigError::Malformed("version"));
    }
    let key = PublicKey::from_wire(r.string()?)?;
    if r.string()? != namespace.as_bytes() {
        return Err(SigError::WrongNamespace);
    }
    if !r.string()?.is_empty() {
        return Err(SigError::Malformed("reserved"));
    }
    let hash_alg = std::str::from_utf8(r.string()?)
        .map_err(|_| SigError::UnsupportedHash)?
        .to_owned();
    let mut sig = Reader(r.string()?);
    r.end()?;
    if sig.string()? != KEY_TYPE.as_bytes() {
        return Err(SigError::UnsupportedKey);
    }
    let raw: [u8; 64] = sig
        .string()?
        .try_into()
        .map_err(|_| SigError::Malformed("signature length"))?;
    sig.end()?;
    if !allowed.contains(&key) {
        return Err(SigError::UnknownKey);
    }
    let blob = signed_blob(namespace, &hash_alg, message)?;
    let vk = VerifyingKey::from_bytes(&key.0).map_err(|_| SigError::BadSignature)?;
    vk.verify_strict(&blob, &Signature::from_bytes(&raw))
        .map_err(|_| SigError::BadSignature)?;
    Ok(key)
}

/// Produces an armored `sshsig` as `ssh-keygen -Y sign` does (sha512, 70
/// columns). Used by tests and by tooling that holds the secret key in
/// process; publishing signs through git and `ssh-keygen` (§7.1).
pub fn sign(secret: &[u8; 32], namespace: &str, message: &[u8]) -> String {
    use ed25519_dalek::{Signer, SigningKey};
    let sk = SigningKey::from_bytes(secret);
    let key = PublicKey(sk.verifying_key().to_bytes());
    let blob = signed_blob(namespace, "sha512", message).expect("sha512 is supported");
    let sig = sk.sign(&blob).to_bytes();
    let mut inner = Vec::new();
    put_string(&mut inner, KEY_TYPE.as_bytes());
    put_string(&mut inner, &sig);
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&1u32.to_be_bytes());
    put_string(&mut out, &key.to_wire());
    put_string(&mut out, namespace.as_bytes());
    put_string(&mut out, b"");
    put_string(&mut out, b"sha512");
    put_string(&mut out, &inner);
    let text = b64::encode(&out);
    let mut armored = format!("{BEGIN}\n");
    for line in text.as_bytes().chunks(70) {
        armored.push_str(std::str::from_utf8(line).expect("base64 is ascii"));
        armored.push('\n');
    }
    armored.push_str(END);
    armored.push('\n');
    armored
}

/// The public key of an ed25519 secret key.
pub fn public_key(secret: &[u8; 32]) -> PublicKey {
    PublicKey(
        ed25519_dalek::SigningKey::from_bytes(secret)
            .verifying_key()
            .to_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SK: [u8; 32] = [7; 32];

    #[test]
    fn sign_verify_round_trip_and_refusals() {
        let pk = public_key(&SK);
        let sig = sign(&SK, "git", b"payload");
        assert_eq!(verify(&sig, "git", b"payload", &[pk]), Ok(pk));
        assert_eq!(
            verify(&sig, "git", b"payloaD", &[pk]),
            Err(SigError::BadSignature)
        );
        assert_eq!(
            verify(&sig, "file", b"payload", &[pk]),
            Err(SigError::WrongNamespace)
        );
        let other = public_key(&[9; 32]);
        assert_eq!(
            verify(&sig, "git", b"payload", &[other]),
            Err(SigError::UnknownKey)
        );
        assert!(
            verify(
                &sig.replace("-----END", "x-----END"),
                "git",
                b"payload",
                &[pk]
            )
            .is_err()
        );
    }

    #[test]
    fn public_key_line_round_trip() {
        let pk = public_key(&SK);
        assert_eq!(PublicKey::parse(&pk.to_line()), Ok(pk));
        assert_eq!(
            PublicKey::parse(&format!("{} me@host", pk.to_line())),
            Ok(pk)
        );
        let sk_line = pk
            .to_line()
            .replace("ssh-ed25519", "sk-ssh-ed25519@openssh.com");
        assert_eq!(PublicKey::parse(&sk_line), Err(SigError::BadPublicKey));
        assert_eq!(
            PublicKey::parse("ssh-rsa AAAA"),
            Err(SigError::BadPublicKey)
        );
    }
}
