use contextdb_model::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}
impl ObjectFormat {
    pub fn oid_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub repo: String,
    pub path: String,
    pub format: ObjectFormat,
    pub oid: Vec<u8>,
}
pub fn validate_path(repo: &str, path: &str) -> Result<(), WireError> {
    if repo.len() != 32
        || !repo
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || path.is_empty()
        || format!("code:{repo}/{path}").len() > 256
        || !path
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._/#-".contains(&b))
        || path
            .split('/')
            .any(|p| p.is_empty() || matches!(p, "." | ".." | ".git" | ".contextdb"))
    {
        return Err(WireError::InvalidEncoding);
    }
    Ok(())
}
impl Descriptor {
    pub fn validate(&self) -> Result<(), WireError> {
        validate_path(&self.repo, &self.path)?;
        if self.oid.len() != self.format.oid_len() {
            return Err(WireError::InvalidEncoding);
        }
        Ok(())
    }
    pub fn key(&self) -> String {
        format!("code:{}/{}", self.repo, self.path)
    }
    pub fn scope(&self) -> String {
        format!("code:{}", self.repo)
    }
    pub fn content(&self) -> ContentHash {
        *blake3::hash(&self.canonical_bytes()).as_bytes()
    }
}
impl Wire for Descriptor {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"CTXANC01");
        1u32.encode(out);
        self.repo.encode(out);
        self.path.encode(out);
        (match self.format {
            ObjectFormat::Sha1 => 1u8,
            ObjectFormat::Sha256 => 2u8,
        })
        .encode(out);
        self.oid.encode(out);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        if i.take_bytes(8)? != b"CTXANC01" {
            return Err(WireError::InvalidEncoding);
        }
        if u32::decode(i)? != 1 {
            return Err(WireError::UnknownVersion);
        }
        let repo = String::decode(i)?;
        let path = String::decode(i)?;
        let format = match u8::decode(i)? {
            1 => ObjectFormat::Sha1,
            2 => ObjectFormat::Sha256,
            _ => return Err(WireError::InvalidEncoding),
        };
        let oid = Vec::<u8>::decode(i)?;
        let d = Self {
            repo,
            path,
            format,
            oid,
        };
        d.validate()?;
        Ok(d)
    }
}
pub fn unhex(bytes: &[u8]) -> Result<Vec<u8>, WireError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(WireError::InvalidEncoding);
    }
    bytes
        .chunks_exact(2)
        .map(|c| {
            let digit = |b| match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                _ => Err(WireError::InvalidEncoding),
            };
            Ok(digit(c[0])? * 16 + digit(c[1])?)
        })
        .collect()
}
