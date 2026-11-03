//! P7 blob documents (ADR 0015 §2, §3.2): validation metadata, review,
//! conflict and resolution documents. Each is a canonical wire encoding,
//! stored as an ordinary store blob shipped with its batch; the kernel keeps
//! only the hash and never reads the content.
use contextdb_model::*;

/// Largest blob document (ADR 0015 §3.2).
pub const MAX_DOC_BYTES: usize = 64 * 1024;

macro_rules! wire_struct {
    ($name:ident { $($field:ident),* $(,)? }) => {
        impl Wire for $name {
            fn encode(&self, o: &mut Vec<u8>) {
                $(self.$field.encode(o);)*
            }
            fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
                Ok(Self { $($field: Wire::decode(i)?,)* })
            }
        }
    };
}
macro_rules! wire_enum {
    ($name:ident { $($variant:ident = $tag:literal),* $(,)? }) => {
        impl Wire for $name {
            fn encode(&self, o: &mut Vec<u8>) {
                (*self as u8).encode(o)
            }
            fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
                match u8::decode(i)? {
                    $($tag => Ok(Self::$variant),)*
                    _ => Err(WireError::InvalidEncoding),
                }
            }
        }
    };
}

/// Who produced a validation outcome (ADR 0015 §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ValidatorKind {
    Deterministic = 0,
    Admin = 1,
    Llm = 2,
}
wire_enum!(ValidatorKind {
    Deterministic = 0,
    Admin = 1,
    Llm = 2
});

/// A validator's label (SPEC P7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Label {
    Reinforcement = 0,
    None = 1,
    Conflict = 2,
    BoundaryChange = 3,
    NewRule = 4,
}
wire_enum!(Label {
    Reinforcement = 0,
    None = 1,
    Conflict = 2,
    BoundaryChange = 3,
    NewRule = 4
});
impl Label {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "reinforcement" => Self::Reinforcement,
            "none" => Self::None,
            "conflict" => Self::Conflict,
            "boundary_change" => Self::BoundaryChange,
            "new_rule" => Self::NewRule,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Reinforcement => "reinforcement",
            Self::None => "none",
            Self::Conflict => "conflict",
            Self::BoundaryChange => "boundary_change",
            Self::NewRule => "new_rule",
        }
    }
    /// The trusted mapping (ADR 0015 §4.2): Some(verdict) records it;
    /// None leaves the rule Pending.
    pub fn trusted_verdict(self) -> Option<Verdict> {
        match self {
            Self::Reinforcement | Self::None => Some(Verdict::Verified),
            Self::Conflict => Some(Verdict::Rejected),
            Self::BoundaryChange | Self::NewRule => None,
        }
    }
}

/// `ValidationMeta` (ADR 0015 §3.2). `evidence` holds `key@rev` handles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationMeta {
    pub v: u8,
    pub kind: ValidatorKind,
    pub validator: String,
    pub version: String,
    pub model: Option<String>,
    pub prompt_sha256: Option<ContentHash>,
    pub label: Label,
    pub rationale: String,
    pub evidence: Vec<String>,
    pub tokens_in: Option<u32>,
    pub tokens_out: Option<u32>,
    pub job: Option<[u8; 16]>,
    pub based_on: Option<ArtifactId>,
    pub at: u64,
}
wire_struct!(ValidationMeta {
    v,
    kind,
    validator,
    version,
    model,
    prompt_sha256,
    label,
    rationale,
    evidence,
    tokens_in,
    tokens_out,
    job,
    based_on,
    at
});

/// What an LLM suggests through a PendingReview (ADR 0015 §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Suggestion {
    Verify = 0,
    Reject = 1,
    Investigate = 2,
}
wire_enum!(Suggestion {
    Verify = 0,
    Reject = 1,
    Investigate = 2
});

/// The PendingReview payload (ADR 0015 §3.2). `rule`/`curvec` name the job
/// for validator reviews; `sides` names a candidate pair for contradiction
/// reviews.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewDoc {
    pub v: u8,
    pub suggestion: Suggestion,
    pub branch: BranchId,
    pub rule: Option<RuleId>,
    pub curvec: HeadVector,
    pub sides: Vec<RevisionId>,
    pub meta: ContentHash,
}
wire_struct!(ReviewDoc {
    v,
    suggestion,
    branch,
    rule,
    curvec,
    sides,
    meta
});

/// Where a Conflict came from (SD20).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictSource {
    Agent,
    Admin,
    Candidate(u64),
    Review(ArtifactId),
}
impl Wire for ConflictSource {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Agent => 0u8.encode(o),
            Self::Admin => 1u8.encode(o),
            Self::Candidate(id) => {
                2u8.encode(o);
                id.encode(o);
            }
            Self::Review(a) => {
                3u8.encode(o);
                a.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Agent,
            1 => Self::Admin,
            2 => Self::Candidate(Wire::decode(i)?),
            3 => Self::Review(Wire::decode(i)?),
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}

/// The payload shared by both artifacts of a contradiction (SD20).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictDoc {
    pub v: u8,
    pub sides: Vec<RevisionId>,
    pub reason: String,
    pub source: ConflictSource,
    pub evidence: Vec<String>,
}
wire_struct!(ConflictDoc {
    v,
    sides,
    reason,
    source,
    evidence
});

/// A resolution note (`ResolveArtifact.note`, ADR 0015 §6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionDoc {
    pub v: u8,
    pub note: String,
    pub at: u64,
}
wire_struct!(ResolutionDoc { v, note, at });

/// A document's canonical bytes and their hash, refusing oversize documents.
pub fn blob<T: Wire>(doc: &T) -> Result<(ContentHash, Vec<u8>), String> {
    let bytes = doc.canonical_bytes();
    if bytes.len() > MAX_DOC_BYTES {
        return Err(format!(
            "document is {} bytes, over the {MAX_DOC_BYTES}-byte cap",
            bytes.len()
        ));
    }
    Ok((*blake3::hash(&bytes).as_bytes(), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_round_trip_and_cap() {
        let meta = ValidationMeta {
            v: 1,
            kind: ValidatorKind::Llm,
            validator: "claude".into(),
            version: "2.1.316".into(),
            model: Some("claude-haiku-5-5".into()),
            prompt_sha256: Some([3; 32]),
            label: Label::Conflict,
            rationale: "contradicts".into(),
            evidence: vec!["db@2".into()],
            tokens_in: Some(10),
            tokens_out: Some(2),
            job: Some([1; 16]),
            based_on: None,
            at: 7,
        };
        let (h, bytes) = blob(&meta).unwrap();
        assert_eq!(h, *blake3::hash(&bytes).as_bytes());
        assert_eq!(ValidationMeta::from_bytes(&bytes), Ok(meta.clone()));
        let review = ReviewDoc {
            v: 1,
            suggestion: Suggestion::Reject,
            branch: MAIN,
            rule: Some(RuleId(4)),
            curvec: vec![(LogicalId(0), RevisionId(2))],
            sides: vec![],
            meta: h,
        };
        assert_eq!(ReviewDoc::from_bytes(&review.canonical_bytes()), Ok(review));
        for source in [
            ConflictSource::Agent,
            ConflictSource::Admin,
            ConflictSource::Candidate(9),
            ConflictSource::Review(ArtifactId(3)),
        ] {
            let doc = ConflictDoc {
                v: 1,
                sides: vec![RevisionId(1), RevisionId(2)],
                reason: "r".into(),
                source,
                evidence: vec![],
            };
            assert_eq!(ConflictDoc::from_bytes(&doc.canonical_bytes()), Ok(doc));
        }
        let big = ResolutionDoc {
            v: 1,
            note: "x".repeat(MAX_DOC_BYTES),
            at: 0,
        };
        assert!(blob(&big).is_err());
        for l in [
            Label::Reinforcement,
            Label::None,
            Label::Conflict,
            Label::BoundaryChange,
            Label::NewRule,
        ] {
            assert_eq!(Label::parse(l.name()), Some(l));
        }
        assert_eq!(Label::Conflict.trusted_verdict(), Some(Verdict::Rejected));
        assert_eq!(Label::NewRule.trusted_verdict(), None);
    }
}
