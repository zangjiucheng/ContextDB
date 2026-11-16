//! Strict parsing of a root-chain entry's git commit object (ADR 0018 §3.1,
//! SPEC SD21.2–3): the headers, the signature header, the payload the
//! signature covers, and the `Ctx-*` trailers.
//!
//! The parser accepts exactly the commits ContextDB writes. Anything else —
//! an unknown header, a second signature, a non-canonical author line, an
//! unknown or duplicated trailer — is a structural rejection, which `accept`
//! turns into a deterministic `Rejected` outcome.

/// The object format of the repository holding the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    pub fn hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }
    /// The header that carries the signature for this format.
    pub fn sig_header(self) -> &'static str {
        match self {
            Self::Sha1 => "gpgsig",
            Self::Sha256 => "gpgsig-sha256",
        }
    }
}

/// The fixed identity of every entry (m-N7): no member's name or clock.
pub const IDENTITY: &str = "ContextDB <team@contextdb> 0 +0000";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: String,
    pub parents: Vec<String>,
    /// The armored signature, `None` if unsigned.
    pub signature: Option<String>,
    /// The bytes the signature covers: the object without its signature
    /// header.
    pub payload: Vec<u8>,
    pub trailers: Trailers,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trailers {
    pub team: Option<String>,
    pub seq: Option<u64>,
    pub author: Option<String>,
    pub batch_hash: Option<String>,
    pub accept_version: Option<u32>,
    pub proposals: Vec<[u8; 16]>,
    pub compacts: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitError {
    NotUtf8,
    Header(&'static str),
    Identity,
    Message,
    Trailer(String),
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Parses a raw commit object (`git cat-file commit <oid>`).
pub fn parse(raw: &[u8], format: ObjectFormat) -> Result<Commit, CommitError> {
    let text = std::str::from_utf8(raw).map_err(|_| CommitError::NotUtf8)?;
    let (head, message) = text
        .split_once("\n\n")
        .ok_or(CommitError::Header("no message"))?;
    let lines: Vec<&str> = head.split('\n').collect();
    let mut i = 0;
    let mut next = |want: &str| -> Option<&str> {
        let line = lines.get(i)?;
        let rest = line.strip_prefix(want)?.strip_prefix(' ')?;
        i += 1;
        Some(rest)
    };
    let tree = next("tree").ok_or(CommitError::Header("tree"))?.to_owned();
    let mut parents = Vec::new();
    while let Some(p) = next("parent") {
        parents.push(p.to_owned());
    }
    let author = next("author").ok_or(CommitError::Header("author"))?;
    let committer = next("committer").ok_or(CommitError::Header("committer"))?;
    if author != IDENTITY || committer != IDENTITY {
        return Err(CommitError::Identity);
    }
    if !is_hex(&tree, format.hex_len()) || !parents.iter().all(|p| is_hex(p, format.hex_len())) {
        return Err(CommitError::Header("object id"));
    }
    // Only the signature header may follow, with its continuation lines.
    let signed_end = i;
    let mut signature = None;
    if let Some(first) = lines.get(i) {
        let first = first
            .strip_prefix(format.sig_header())
            .and_then(|r| r.strip_prefix(' '))
            .ok_or(CommitError::Header("unexpected header"))?;
        let mut armored = format!("{first}\n");
        i += 1;
        while let Some(cont) = lines.get(i).and_then(|l| l.strip_prefix(' ')) {
            armored.push_str(cont);
            armored.push('\n');
            i += 1;
        }
        signature = Some(armored);
    }
    if i != lines.len() {
        return Err(CommitError::Header("unexpected header"));
    }
    let mut payload = lines[..signed_end].join("\n").into_bytes();
    payload.extend_from_slice(b"\n\n");
    payload.extend_from_slice(message.as_bytes());
    Ok(Commit {
        tree,
        parents,
        signature,
        payload,
        trailers: parse_trailers(message)?,
    })
}

/// The message is `<subject>\n\n<trailers>\n`: one subject line (free text,
/// display only) and one trailer paragraph of `Ctx-*` lines.
fn parse_trailers(message: &str) -> Result<Trailers, CommitError> {
    let body = message.strip_suffix('\n').ok_or(CommitError::Message)?;
    let (subject, block) = body.split_once("\n\n").ok_or(CommitError::Message)?;
    if subject.is_empty() || subject.contains('\n') || block.is_empty() {
        return Err(CommitError::Message);
    }
    let mut t = Trailers::default();
    let bad = |line: &str| CommitError::Trailer(line.to_owned());
    for line in block.split('\n') {
        let (key, value) = line.split_once(": ").ok_or_else(|| bad(line))?;
        if value.is_empty() || value.trim() != value {
            return Err(bad(line));
        }
        fn once<T>(slot: &mut Option<T>, v: T, line: &str) -> Result<(), CommitError> {
            if slot.replace(v).is_some() {
                return Err(CommitError::Trailer(line.to_owned()));
            }
            Ok(())
        }
        match key {
            "Ctx-Team" => once(&mut t.team, value.to_owned(), line)?,
            "Ctx-Team-Seq" => {
                let n = parse_decimal(value).ok_or_else(|| bad(line))?;
                once(&mut t.seq, n, line)?
            }
            "Ctx-Author" => once(&mut t.author, value.to_owned(), line)?,
            "Ctx-Batch-Hash" => once(&mut t.batch_hash, value.to_owned(), line)?,
            "Ctx-Accept-Version" => {
                let n = parse_decimal(value)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| bad(line))?;
                once(&mut t.accept_version, n, line)?
            }
            "Ctx-Proposal" => {
                let id = parse_id128(value).ok_or_else(|| bad(line))?;
                if t.proposals.contains(&id) {
                    return Err(bad(line));
                }
                t.proposals.push(id);
            }
            "Ctx-Compacts" => once(&mut t.compacts, value.to_owned(), line)?,
            _ => return Err(bad(line)),
        }
    }
    Ok(t)
}

/// Canonical decimal: no sign, no leading zeros (except `0`).
fn parse_decimal(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0'))
    {
        return None;
    }
    s.parse().ok()
}

/// 32 lowercase hex digits.
pub fn parse_id128(s: &str) -> Option<[u8; 16]> {
    if !is_hex(s, 32) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Builds the unsigned object for an entry; `sign` then inserts the
/// signature header exactly where `parse` expects it.
pub fn build(
    tree: &str,
    parents: &[String],
    subject: &str,
    trailers: &[(&str, String)],
) -> Vec<u8> {
    let mut s = format!("tree {tree}\n");
    for p in parents {
        s.push_str(&format!("parent {p}\n"));
    }
    s.push_str(&format!(
        "author {IDENTITY}\ncommitter {IDENTITY}\n\n{subject}\n\n"
    ));
    for (k, v) in trailers {
        s.push_str(&format!("{k}: {v}\n"));
    }
    s.into_bytes()
}

/// Inserts an armored signature into an unsigned object built by [`build`].
pub fn with_signature(unsigned: &[u8], armored: &str, format: ObjectFormat) -> Vec<u8> {
    let text = std::str::from_utf8(unsigned).expect("build writes utf-8");
    let (head, message) = text.split_once("\n\n").expect("build writes a message");
    let mut sig = String::new();
    for (i, line) in armored.trim_end_matches('\n').split('\n').enumerate() {
        sig.push_str(if i == 0 { format.sig_header() } else { "" });
        sig.push(' ');
        sig.push_str(line);
        sig.push('\n');
    }
    format!("{head}\n{sig}\n{message}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshsig;

    const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    fn trailers() -> Vec<(&'static str, String)> {
        vec![
            ("Ctx-Team", "00112233445566778899aabbccddeeff".into()),
            ("Ctx-Team-Seq", "0".into()),
            ("Ctx-Author", "alice".into()),
            ("Ctx-Accept-Version", "1".into()),
        ]
    }

    #[test]
    fn signed_round_trip() {
        let unsigned = build(TREE, &[], "ctx team: genesis", &trailers());
        let sk = [3u8; 32];
        let armored = sshsig::sign(&sk, "git", &unsigned);
        let raw = with_signature(&unsigned, &armored, ObjectFormat::Sha1);
        let c = parse(&raw, ObjectFormat::Sha1).unwrap();
        assert_eq!(c.payload, unsigned);
        assert_eq!(c.signature.as_deref(), Some(armored.as_str()));
        assert_eq!(c.trailers.seq, Some(0));
        assert_eq!(c.trailers.author.as_deref(), Some("alice"));
        let pk = sshsig::public_key(&sk);
        assert_eq!(
            sshsig::verify(c.signature.as_ref().unwrap(), "git", &c.payload, &[pk]),
            Ok(pk)
        );
        // In a SHA-1 repository a gpgsig-sha256 header is not the signature.
        assert!(
            parse(
                &with_signature(&unsigned, &armored, ObjectFormat::Sha256),
                ObjectFormat::Sha1
            )
            .is_err()
        );
    }

    #[test]
    fn strict_refusals() {
        let ok = build(TREE, &[], "s", &trailers());
        assert!(parse(&ok, ObjectFormat::Sha1).is_ok());
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("dup trailer", build(TREE, &[], "s", &[trailers(), vec![("Ctx-Author", "bob".into())]].concat())),
            ("unknown trailer", build(TREE, &[], "s", &[trailers(), vec![("Signed-off-by", "x".into())]].concat())),
            ("seq leading zero", build(TREE, &[], "s", &[("Ctx-Team-Seq", "01".into())])),
            ("bad proposal id", build(TREE, &[], "s", &[("Ctx-Proposal", "XYZ".into())])),
            ("short tree", build("abc", &[], "s", &trailers())),
            ("identity", String::from_utf8(ok.clone()).unwrap().replacen("0 +0000", "1 +0000", 1).into_bytes()),
            ("extra header", String::from_utf8(ok.clone()).unwrap().replacen("\n\n", "\nencoding UTF-8\n\n", 1).into_bytes()),
            ("no subject paragraph", b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor ContextDB <team@contextdb> 0 +0000\ncommitter ContextDB <team@contextdb> 0 +0000\n\nCtx-Team-Seq: 0\n".to_vec()),
        ];
        for (name, raw) in cases {
            assert!(parse(&raw, ObjectFormat::Sha1).is_err(), "{name} accepted");
        }
        let dup = [1u8; 16];
        let two = build(
            TREE,
            &[],
            "s",
            &[("Ctx-Proposal", hex(&dup)), ("Ctx-Proposal", hex(&dup))],
        );
        assert!(parse(&two, ObjectFormat::Sha1).is_err());
    }
}
