//! Portable team ops and the `CXTB` batch format (ADR 0018 §4.1, SPEC
//! SD21.3–4). A team batch names nothing by local id: records by key path
//! (the part after `team/<team-id>/`), revisions by content hash, rules by
//! their canonical form. Every state change carries its expected value.

use crate::doc::{TeamDoc, kind_code, kind_from_code, valid_name};
use crate::enc::{DecodeError, Reader, Writer, strictly_sorted};
use contextdb_model::RecordKind;
use std::collections::BTreeSet;

pub type Hash = [u8; 32];

const MAGIC: &[u8; 4] = b"CXTB";
pub const CXTB_VERSION: u16 = 1;
/// Ordinary entry limits (§3.1), checked before decoding.
pub const MAX_BATCH_BYTES: usize = 1 << 20;
pub const MAX_OPS: usize = 1000;
pub const MAX_BLOB_BYTES: usize = 1 << 20;
pub const MAX_ENTRY_BYTES: usize = 8 << 20;
/// Item bounds that make every checkpoint item fit a chunk (§8.6).
pub const MAX_KEY: usize = 256;
pub const MAX_TAGS_BYTES: usize = 64 * 1024;
pub const MAX_INCIDENCES: usize = 1024;

/// A rule premise or reference, naming the revision by content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Edge {
    /// Pin-strict on this revision.
    Pin(Hash),
    /// Current-head premise; the hash is the head content the proposer saw,
    /// and must still be the head when the root decides (N7).
    Cur(Hash),
    /// A reference to this revision; never supports anything.
    Ref(Hash),
}

impl Edge {
    pub fn content(self) -> Hash {
        match self {
            Self::Pin(h) | Self::Cur(h) | Self::Ref(h) => h,
        }
    }
}

/// A team rule in canonical form: incidences sorted by `(path, edge)`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RuleSpec {
    pub conclusion: (String, Hash),
    pub incidences: Vec<(String, Edge)>,
}

impl RuleSpec {
    /// The rule's canonical encoding, which names it in enable/disable.
    pub fn encode(&self, w: &mut Writer) {
        w.str(&self.conclusion.0).raw(&self.conclusion.1);
        w.count(self.incidences.len());
        for (path, e) in &self.incidences {
            w.str(path);
            let (tag, h) = match e {
                Edge::Pin(h) => (0, h),
                Edge::Cur(h) => (1, h),
                Edge::Ref(h) => (2, h),
            };
            w.u8(tag).raw(h);
        }
    }
    fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let conclusion = (path(r)?, r.array()?);
        let n = r.count(37)?;
        if n > MAX_INCIDENCES {
            return Err(DecodeError("too many incidences"));
        }
        let mut incidences = Vec::with_capacity(n);
        for _ in 0..n {
            let p = path(r)?;
            let tag = r.u8()?;
            let h = r.array()?;
            incidences.push((
                p,
                match tag {
                    0 => Edge::Pin(h),
                    1 => Edge::Cur(h),
                    2 => Edge::Ref(h),
                    _ => return Err(DecodeError("edge kind")),
                },
            ));
        }
        if !strictly_sorted(&incidences) {
            return Err(DecodeError("unsorted incidences"));
        }
        Ok(Self {
            conclusion,
            incidences,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeamOp {
    /// Creates `team/<id>/<path>` in lane `team:<id>/<lane>`.
    New {
        path: String,
        lane: String,
        kind: RecordKind,
        content: Hash,
        adopt_as_base: bool,
    },
    /// Adds a revision (it does not move the head).
    Revise {
        path: String,
        content: Hash,
        adopt_as_base: bool,
    },
    SetHead {
        path: String,
        expected: Hash,
        new: Hash,
    },
    /// Asserts (`base = true`) or retracts a revision's base; `expected` is
    /// the bit the proposer saw.
    SetBase {
        path: String,
        content: Hash,
        expected: bool,
        base: bool,
    },
    AddRule {
        rule: RuleSpec,
        enabled: bool,
    },
    SetRuleEnabled {
        rule: RuleSpec,
        expected: bool,
        enabled: bool,
    },
    /// Replaces the tags; `expected` is the BLAKE3 of the tags seen
    /// ([`tags_hash`]).
    SetTags {
        path: String,
        expected: Hash,
        tags: BTreeSet<String>,
    },
    /// Read set: the proposal relied on this head (M2).
    Expect {
        path: String,
        content: Hash,
    },
    SetTeamAccess {
        doc: Box<TeamDoc>,
    },
    TeamShred {
        content: Hash,
    },
    Compact {
        generation: u32,
        /// The checkpoint tree's raw object id (20 or 32 bytes).
        tree: Vec<u8>,
        digest: Hash,
    },
}

/// The canonical hash of a tag set, as `SetTags.expected` names it.
pub fn tags_hash(tags: &BTreeSet<String>) -> Hash {
    let mut w = Writer::default();
    w.count(tags.len());
    for t in tags {
        w.str(t);
    }
    *blake3::hash(&w.0).as_bytes()
}

/// A key path: the kernel's key charset (`[a-z0-9][a-z0-9._:/#-]*`), and
/// the whole key `team/<32 hex>/<path>` within 256 bytes.
pub fn valid_path(p: &str) -> bool {
    let mut b = p.bytes();
    // `team/` + 32 hex + `/` + path ≤ MAX_KEY.
    p.len() + "team/".len() + "/".len() + 32 <= MAX_KEY
        && b.next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._:/#-".contains(&c))
}

fn path(r: &mut Reader) -> Result<String, DecodeError> {
    let p = r.str()?;
    if !valid_path(p) {
        return Err(DecodeError("key path"));
    }
    Ok(p.to_owned())
}

fn lane(r: &mut Reader) -> Result<String, DecodeError> {
    let l = r.str()?;
    if !valid_name(l) {
        return Err(DecodeError("lane"));
    }
    Ok(l.to_owned())
}

impl TeamOp {
    fn encode(&self, w: &mut Writer) {
        match self {
            Self::New {
                path,
                lane,
                kind,
                content,
                adopt_as_base,
            } => {
                w.u8(0).str(path).str(lane);
                w.u8(kind_code(*kind).expect("team kind"));
                w.raw(content).bool(*adopt_as_base);
            }
            Self::Revise {
                path,
                content,
                adopt_as_base,
            } => {
                w.u8(1).str(path).raw(content).bool(*adopt_as_base);
            }
            Self::SetHead {
                path,
                expected,
                new,
            } => {
                w.u8(2).str(path).raw(expected).raw(new);
            }
            Self::SetBase {
                path,
                content,
                expected,
                base,
            } => {
                w.u8(3).str(path).raw(content).bool(*expected).bool(*base);
            }
            Self::AddRule { rule, enabled } => {
                w.u8(4);
                rule.encode(w);
                w.bool(*enabled);
            }
            Self::SetRuleEnabled {
                rule,
                expected,
                enabled,
            } => {
                w.u8(5);
                rule.encode(w);
                w.bool(*expected).bool(*enabled);
            }
            Self::SetTags {
                path,
                expected,
                tags,
            } => {
                w.u8(6).str(path).raw(expected).count(tags.len());
                for t in tags {
                    w.str(t);
                }
            }
            Self::Expect { path, content } => {
                w.u8(7).str(path).raw(content);
            }
            Self::SetTeamAccess { doc } => {
                w.u8(8).bytes(&doc.encode());
            }
            Self::TeamShred { content } => {
                w.u8(9).raw(content);
            }
            Self::Compact {
                generation,
                tree,
                digest,
            } => {
                w.u8(10).u32(*generation).bytes(tree).raw(digest);
            }
        }
    }

    fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(match r.u8()? {
            0 => Self::New {
                path: path(r)?,
                lane: lane(r)?,
                kind: kind_from_code(r.u8()?).ok_or(DecodeError("kind"))?,
                content: r.array()?,
                adopt_as_base: r.bool()?,
            },
            1 => Self::Revise {
                path: path(r)?,
                content: r.array()?,
                adopt_as_base: r.bool()?,
            },
            2 => Self::SetHead {
                path: path(r)?,
                expected: r.array()?,
                new: r.array()?,
            },
            3 => Self::SetBase {
                path: path(r)?,
                content: r.array()?,
                expected: r.bool()?,
                base: r.bool()?,
            },
            4 => Self::AddRule {
                rule: RuleSpec::decode(r)?,
                enabled: r.bool()?,
            },
            5 => Self::SetRuleEnabled {
                rule: RuleSpec::decode(r)?,
                expected: r.bool()?,
                enabled: r.bool()?,
            },
            6 => {
                let path = path(r)?;
                let expected = r.array()?;
                let n = r.count(4)?;
                let tags: Vec<String> = (0..n)
                    .map(|_| r.str().map(str::to_owned))
                    .collect::<Result<_, _>>()?;
                if !strictly_sorted(&tags) {
                    return Err(DecodeError("unsorted tags"));
                }
                let tags: BTreeSet<String> = tags.into_iter().collect();
                if tags.iter().map(|t| t.len() + 4).sum::<usize>() + 4 > MAX_TAGS_BYTES {
                    return Err(DecodeError("tags too large"));
                }
                Self::SetTags {
                    path,
                    expected,
                    tags,
                }
            }
            7 => Self::Expect {
                path: path(r)?,
                content: r.array()?,
            },
            8 => Self::SetTeamAccess {
                doc: Box::new(
                    TeamDoc::decode(r.bytes()?).map_err(|_| DecodeError("team document"))?,
                ),
            },
            9 => Self::TeamShred {
                content: r.array()?,
            },
            10 => {
                let generation = r.u32()?;
                let tree = r.bytes()?.to_vec();
                if tree.len() != 20 && tree.len() != 32 {
                    return Err(DecodeError("tree id"));
                }
                Self::Compact {
                    generation,
                    tree,
                    digest: r.array()?,
                }
            }
            _ => return Err(DecodeError("op tag")),
        })
    }

    /// Ops that must be the only op of their entry.
    pub fn is_sole(&self) -> bool {
        matches!(self, Self::SetTeamAccess { .. } | Self::Compact { .. })
    }
}

/// An entry's `batch`: one op list per proposal, in `Ctx-Proposal` order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TeamBatch {
    pub proposals: Vec<Vec<TeamOp>>,
}

impl TeamBatch {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.raw(MAGIC).u16(CXTB_VERSION);
        w.count(self.proposals.len());
        for ops in &self.proposals {
            w.count(ops.len());
            for op in ops {
                op.encode(&mut w);
            }
        }
        w.0
    }

    /// Decodes a `batch` file. Size is checked before decoding; the bytes
    /// must be the canonical encoding.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() > MAX_BATCH_BYTES {
            return Err(DecodeError("batch too large"));
        }
        let mut r = Reader(bytes);
        if r.take(4)? != MAGIC {
            return Err(DecodeError("magic"));
        }
        if r.u16()? != CXTB_VERSION {
            return Err(DecodeError("version"));
        }
        let n = r.count(4)?;
        let mut proposals = Vec::with_capacity(n);
        let mut total = 0usize;
        for _ in 0..n {
            let k = r.count(1)?;
            total += k;
            if total > MAX_OPS {
                return Err(DecodeError("too many ops"));
            }
            if k == 0 {
                return Err(DecodeError("empty proposal"));
            }
            proposals.push(
                (0..k)
                    .map(|_| TeamOp::decode(&mut r))
                    .collect::<Result<_, _>>()?,
            );
        }
        r.end()?;
        let b = Self { proposals };
        debug_assert_eq!(b.encode(), bytes);
        Ok(b)
    }

    /// `Ctx-Batch-Hash`: BLAKE3 of the `batch` bytes, hex.
    pub fn hash_hex(bytes: &[u8]) -> String {
        crate::commit::hex(blake3::hash(bytes).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::TeamDoc;

    fn h(b: u8) -> Hash {
        [b; 32]
    }

    pub fn sample() -> TeamBatch {
        let rule = RuleSpec {
            conclusion: ("arch/db".into(), h(1)),
            incidences: vec![
                ("conv/naming".into(), Edge::Pin(h(2))),
                ("conv/naming".into(), Edge::Ref(h(2))),
                ("plan/q4".into(), Edge::Cur(h(3))),
            ],
        };
        let doc = TeamDoc::from_toml(&crate::doc::tests::sample_toml()).unwrap();
        TeamBatch {
            proposals: vec![
                vec![
                    TeamOp::New {
                        path: "arch/db".into(),
                        lane: "architecture".into(),
                        kind: RecordKind::Decision,
                        content: h(1),
                        adopt_as_base: true,
                    },
                    TeamOp::Revise {
                        path: "arch/db".into(),
                        content: h(4),
                        adopt_as_base: false,
                    },
                    TeamOp::SetHead {
                        path: "arch/db".into(),
                        expected: h(1),
                        new: h(4),
                    },
                    TeamOp::SetBase {
                        path: "arch/db".into(),
                        content: h(1),
                        expected: true,
                        base: false,
                    },
                    TeamOp::AddRule {
                        rule: rule.clone(),
                        enabled: true,
                    },
                    TeamOp::SetRuleEnabled {
                        rule,
                        expected: true,
                        enabled: false,
                    },
                    TeamOp::SetTags {
                        path: "arch/db".into(),
                        expected: tags_hash(&BTreeSet::new()),
                        tags: ["db".to_owned(), "storage".to_owned()].into(),
                    },
                    TeamOp::Expect {
                        path: "plan/q4".into(),
                        content: h(3),
                    },
                    TeamOp::TeamShred { content: h(9) },
                ],
                vec![TeamOp::SetTeamAccess { doc: Box::new(doc) }],
                vec![TeamOp::Compact {
                    generation: 1,
                    tree: vec![7; 20],
                    digest: h(8),
                }],
            ],
        }
    }

    #[test]
    fn cxtb_round_trip() {
        let b = sample();
        let bytes = b.encode();
        assert_eq!(TeamBatch::decode(&bytes).unwrap(), b);
    }

    #[test]
    fn cxtb_refusals() {
        let bytes = sample().encode();
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(TeamBatch::decode(&trailing).is_err());
        assert!(TeamBatch::decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(TeamBatch::decode(&vec![0; MAX_BATCH_BYTES + 1]).is_err());
        // Unsorted incidences are not canonical.
        let mut b = sample();
        if let TeamOp::AddRule { rule, .. } = &mut b.proposals[0][4] {
            rule.incidences.reverse();
        }
        assert!(TeamBatch::decode(&b.encode()).is_err());
        // Paths outside the key charset.
        let mut b = sample();
        if let TeamOp::Expect { path, .. } = &mut b.proposals[0][7] {
            *path = "Upper/Case".into();
        }
        assert!(TeamBatch::decode(&b.encode()).is_err());
        // Too many ops in all.
        let many = TeamBatch {
            proposals: vec![vec![TeamOp::TeamShred { content: h(1) }; MAX_OPS + 1]],
        };
        assert!(TeamBatch::decode(&many.encode()).is_err());
        // A forged count cannot make the decoder allocate.
        let mut forged = b"CXTB\x00\x01".to_vec();
        forged.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(TeamBatch::decode(&forged).is_err());
    }
}
