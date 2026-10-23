//! Symbol anchors' pure formats (ADR 0012 §2–§4): the symbol path syntax, its
//! reversible key encoding, and the `CTXANC02` descriptor. No parsing here:
//! grammars live in `contextdb-symbols`, which `ctxd` never links.
use crate::descriptor::validate_path;
use contextdb_model::*;

/// Languages with symbol anchors (ADR 0012 §2), by file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Language {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
}
impl Language {
    pub fn of_path(path: &str) -> Option<Self> {
        let ext = path.rsplit_once('.')?.1;
        Some(match ext {
            "rs" => Self::Rust,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "py" => Self::Python,
            "go" => Self::Go,
            _ => return None,
        })
    }
    fn code(self) -> u8 {
        match self {
            Self::Rust => 1,
            Self::TypeScript => 2,
            Self::Tsx => 3,
            Self::JavaScript => 4,
            Self::Python => 5,
            Self::Go => 6,
        }
    }
    fn from_code(c: u8) -> Result<Self, WireError> {
        Ok(match c {
            1 => Self::Rust,
            2 => Self::TypeScript,
            3 => Self::Tsx,
            4 => Self::JavaScript,
            5 => Self::Python,
            6 => Self::Go,
            _ => return Err(WireError::InvalidEncoding),
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Go => "go",
        }
    }
}

fn ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}
fn is_hex8(s: &str) -> bool {
    s.len() == 8
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
/// One segment of a symbol path (ADR 0012 §2–§3, normative forms):
/// `ident`, `ident#Trait`, `ident#Trait#hex8`, `#hex8`, `#hex8#Trait`,
/// `#hex8#Trait#hex8` (non-path self type), or the marker `#private`.
fn valid_segment(seg: &str) -> bool {
    if seg == "#private" {
        return true;
    }
    let ident = |s: &str| !s.is_empty() && s.bytes().all(ident_char);
    let parts: Vec<&str> = seg.split('#').collect();
    match parts.as_slice() {
        [a] => ident(a),
        [a, t] => (ident(a) && ident(t)) || (a.is_empty() && is_hex8(t)),
        [a, t, h] => {
            (ident(a) && ident(t) && is_hex8(h)) || (a.is_empty() && is_hex8(t) && ident(h))
        }
        [a, s, t, h] => a.is_empty() && is_hex8(s) && ident(t) && is_hex8(h),
        _ => false,
    }
}
/// A symbol path written with its real spelling, e.g. `Store::open`,
/// `T#From#3fa1c2d0::from`, `C::#private::p`.
pub fn validate_symbol(symbol: &str) -> Result<(), WireError> {
    if symbol.is_empty() || symbol.contains(":::") {
        return Err(WireError::InvalidEncoding);
    }
    let segments: Vec<&str> = symbol.split("::").collect();
    let ok = segments.iter().all(|s| valid_segment(s))
        // `#private` marks the member segment that follows it.
        && segments
            .iter()
            .enumerate()
            .all(|(i, s)| *s != "#private" || (i + 1 < segments.len() && segments[i + 1] != "#private"));
    if ok {
        Ok(())
    } else {
        Err(WireError::InvalidEncoding)
    }
}
/// Reversible encoding into the lowercase key alphabet (ADR 0012 §3).
pub fn encode_symbol(symbol: &str) -> Result<String, WireError> {
    validate_symbol(symbol)?;
    let mut out = String::with_capacity(symbol.len() + 8);
    let mut chars = symbol.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            'a'..='z' | '0'..='9' | '#' => out.push(c),
            'A'..='Z' => {
                out.push('-');
                out.push(c.to_ascii_lowercase());
            }
            '_' => out.push('.'),
            '$' => out.push_str("-."),
            ':' if chars.peek() == Some(&':') => {
                chars.next();
                out.push(':');
            }
            _ => return Err(WireError::InvalidEncoding),
        }
    }
    Ok(out)
}
pub fn decode_symbol(encoded: &str) -> Result<String, WireError> {
    let mut out = String::with_capacity(encoded.len());
    let mut chars = encoded.chars();
    while let Some(c) = chars.next() {
        match c {
            'a'..='z' | '0'..='9' | '#' => out.push(c),
            '.' => out.push('_'),
            ':' => out.push_str("::"),
            '-' => match chars.next() {
                Some(l @ 'a'..='z') => out.push(l.to_ascii_uppercase()),
                Some('.') => out.push('$'),
                _ => return Err(WireError::InvalidEncoding),
            },
            _ => return Err(WireError::InvalidEncoding),
        }
    }
    validate_symbol(&out)?;
    Ok(out)
}
/// Frozen key hash (ADR 0012 §2, SD18.1): never depends on a normaliser.
pub fn key_hash(source: &str) -> String {
    let stripped: Vec<u8> = source
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    blake3::hash(&stripped).to_hex()[..8].to_owned()
}

/// `CTXANC02` (ADR 0012 §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolDescriptor {
    pub repo: String,
    pub path: String,
    pub symbol: String,
    pub language: Language,
    pub normaliser: String,
    pub hash: Vec<u8>,
    /// The normalised hash with the declared name masked: identifies a
    /// renamed or moved symbol for rebind suggestions (ADR 0012 §5).
    pub masked: Vec<u8>,
}
impl SymbolDescriptor {
    pub fn validate(&self) -> Result<(), WireError> {
        validate_path(&self.repo, &self.path)?;
        if Language::of_path(&self.path) != Some(self.language)
            || self.normaliser.is_empty()
            || self.normaliser.len() > 256
            || self.hash.len() != 32
            || self.masked.len() != 32
        {
            return Err(WireError::InvalidEncoding);
        }
        let key = self.key_checked()?;
        if key.len() > 256 {
            return Err(WireError::InvalidEncoding);
        }
        Ok(())
    }
    fn key_checked(&self) -> Result<String, WireError> {
        Ok(format!(
            "code:{}/{}#{}",
            self.repo,
            self.path,
            encode_symbol(&self.symbol)?
        ))
    }
    pub fn key(&self) -> String {
        self.key_checked().expect("validated symbol")
    }
    pub fn scope(&self) -> String {
        format!("code:{}", self.repo)
    }
    pub fn content(&self) -> ContentHash {
        *blake3::hash(&self.canonical_bytes()).as_bytes()
    }
}
impl Wire for SymbolDescriptor {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"CTXANC02");
        1u32.encode(out);
        self.repo.encode(out);
        self.path.encode(out);
        self.symbol.encode(out);
        self.language.code().encode(out);
        self.normaliser.encode(out);
        self.hash.encode(out);
        self.masked.encode(out);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        if i.take_bytes(8)? != b"CTXANC02" {
            return Err(WireError::InvalidEncoding);
        }
        if u32::decode(i)? != 1 {
            return Err(WireError::UnknownVersion);
        }
        let d = Self {
            repo: String::decode(i)?,
            path: String::decode(i)?,
            symbol: String::decode(i)?,
            language: Language::from_code(u8::decode(i)?)?,
            normaliser: String::decode(i)?,
            hash: Vec::<u8>::decode(i)?,
            masked: Vec::<u8>::decode(i)?,
        };
        d.validate()?;
        Ok(d)
    }
}

/// Any committed anchor descriptor. Key-to-path mapping always goes through
/// this (ADR 0012 §7), never by splitting keys: a legacy file path with `#`
/// stays one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorDescriptor {
    File(crate::Descriptor),
    Symbol(SymbolDescriptor),
}
impl AnchorDescriptor {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WireError> {
        match bytes.get(..8) {
            Some(b"CTXANC01") => crate::Descriptor::from_bytes(bytes).map(Self::File),
            Some(b"CTXANC02") => SymbolDescriptor::from_bytes(bytes).map(Self::Symbol),
            _ => Err(WireError::InvalidEncoding),
        }
    }
    pub fn validate(&self) -> Result<(), WireError> {
        match self {
            Self::File(d) => d.validate(),
            Self::Symbol(d) => d.validate(),
        }
    }
    /// The file this anchor observes (the host file for a symbol).
    pub fn path(&self) -> &str {
        match self {
            Self::File(d) => &d.path,
            Self::Symbol(d) => &d.path,
        }
    }
    pub fn key(&self) -> String {
        match self {
            Self::File(d) => d.key(),
            Self::Symbol(d) => d.key(),
        }
    }
    pub fn scope(&self) -> String {
        match self {
            Self::File(d) => d.scope(),
            Self::Symbol(d) => d.scope(),
        }
    }
    pub fn content(&self) -> ContentHash {
        match self {
            Self::File(d) => d.content(),
            Self::Symbol(d) => d.content(),
        }
    }
    pub fn canonical(&self) -> Vec<u8> {
        match self {
            Self::File(d) => d.canonical_bytes(),
            Self::Symbol(d) => d.canonical_bytes(),
        }
    }
    /// What a rebind suggestion matches on: a file's OID, a symbol's
    /// name-masked hash.
    pub fn match_id(&self) -> &[u8] {
        match self {
            Self::File(d) => &d.oid,
            Self::Symbol(d) => &d.masked,
        }
    }
    /// How a broken-anchor payload names it: the path, or `path#symbol`.
    pub fn label(&self) -> String {
        match self {
            Self::File(d) => d.path.clone(),
            Self::Symbol(d) => format!("{}#{}", d.path, d.symbol),
        }
    }
}
