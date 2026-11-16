//! The canonical binary encoding shared by `CXTB`, the team document and the
//! digest: big-endian integers, `u32`-length-prefixed byte strings, UTF-8
//! strings. Decoders re-check canonical form (sorted sets, no trailing bytes)
//! so that every accepted value has exactly one encoding.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(u8::from(v))
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn raw(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(u32::try_from(v.len()).expect("length fits u32"));
        self.raw(v)
    }
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
    pub fn count(&mut self, n: usize) -> &mut Self {
        self.u32(u32::try_from(n).expect("count fits u32"))
    }
}

pub struct Reader<'a>(pub &'a [u8]);

impl<'a> Reader<'a> {
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < n {
            return Err(DecodeError("truncated"));
        }
        let (h, t) = self.0.split_at(n);
        self.0 = t;
        Ok(h)
    }
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError("bool")),
        }
    }
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub fn str(&mut self) -> Result<&'a str, DecodeError> {
        std::str::from_utf8(self.bytes()?).map_err(|_| DecodeError("utf-8"))
    }
    /// A count no larger than the bytes left could hold at `min` bytes per
    /// element, so a forged count cannot make the decoder allocate.
    pub fn count(&mut self, min: usize) -> Result<usize, DecodeError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min.max(1)) > self.0.len() {
            return Err(DecodeError("count"));
        }
        Ok(n)
    }
    pub fn end(&self) -> Result<(), DecodeError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(DecodeError("trailing bytes"))
        }
    }
}

/// Elements in strictly increasing order (a canonical set).
pub fn strictly_sorted<T: Ord>(v: &[T]) -> bool {
    v.windows(2).all(|w| w[0] < w[1])
}
