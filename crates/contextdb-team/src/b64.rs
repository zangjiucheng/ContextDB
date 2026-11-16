//! Standard base64 (RFC 4648 §4) with mandatory padding, as OpenSSH writes
//! public keys and armored signatures. Strict: any other byte, missing or
//! misplaced padding, or non-zero trailing bits is an error, so every
//! accepted string has exactly one decoding and one encoding.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn value(c: u8) -> Option<u32> {
    ALPHABET.iter().position(|&a| a == c).map(|i| i as u32)
}

/// Decodes `s`; `None` if it is not canonical padded base64.
pub fn decode(s: &[u8]) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for (i, quad) in s.chunks(4).enumerate() {
        let last = i == s.len() / 4 - 1;
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            n = (n << 6) | value(c)?;
        }
        n <<= 6 * pad as u32;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        // Non-zero bits under the padding would give a second encoding.
        if pad == 1 && bytes[2] != 0 || pad == 2 && (bytes[1] != 0 || bytes[2] != 0) {
            return None;
        }
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let mut b = [0u8; 3];
        b[..chunk.len()].copy_from_slice(chunk);
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors_round_trip() {
        for (plain, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), enc);
            assert_eq!(decode(enc.as_bytes()).as_deref(), Some(plain.as_bytes()));
        }
    }

    #[test]
    fn non_canonical_is_refused() {
        for bad in [
            "Zg", "Zh==", "Zm9=", "Zg=a", "Zg==Zg==", "Z===", "Zm9v\n", "Zm-v",
        ] {
            assert_eq!(decode(bad.as_bytes()), None, "{bad}");
        }
    }
}
