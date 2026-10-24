//! Certificate records (SPEC §6.4, ADR 0008 §6). The certificate is a durable
//! record in the session log, so it has a canonical Wire encoding; `cert_id`
//! hashes the encoding of every other field (`body_bytes`).
use crate::{
    BranchId, ContentHash, Decoder, Epoch, LogicalId, PendingReason, PrincipalId, RevisionId,
    RuleId, StaleReason, Summary, Wire, WireError,
};

/// How an entry was certified: at its head, or (after recertification) with a
/// changed witness closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Via {
    Head,
    Alternative,
}
/// One read entry. `since_epoch` is the start of its strict window: the
/// `issued_epoch` of the first certificate that covered it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertItem {
    pub logical: LogicalId,
    pub rev: RevisionId,
    pub required: bool,
    pub via: Via,
    pub since_epoch: Epoch,
}
/// One element of the certificate vector. `since_epoch` is the earliest
/// `since_epoch` among the required entries whose closure contains it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct VectorItem {
    pub logical: LogicalId,
    pub rev: RevisionId,
    pub since_epoch: Epoch,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Certificate {
    pub cert_id: ContentHash,
    pub session_id: [u8; 16],
    pub principal: PrincipalId,
    pub branch: BranchId,
    pub issued_epoch: Epoch,
    pub log_checksum_at_issue: ContentHash,
    pub code_commit: String,
    /// `(scope_lane, committed position)` of every CodeAnchor scope in the vector.
    pub watermarks: Vec<(String, Vec<u8>)>,
    pub entries: Vec<CertItem>,
    /// Sorted by logical; one effective revision per logical.
    pub vector: Vec<VectorItem>,
    pub strict_no_aba: bool,
}
impl Certificate {
    /// Canonical encoding of every field except `cert_id`; `cert_id` is
    /// blake3 of these bytes (computed in core, the model has no dependencies).
    pub fn body_bytes(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.encode_body(&mut o);
        o
    }
    fn encode_body(&self, o: &mut Vec<u8>) {
        self.session_id.encode(o);
        self.principal.encode(o);
        self.branch.encode(o);
        self.issued_epoch.encode(o);
        self.log_checksum_at_issue.encode(o);
        self.code_commit.encode(o);
        self.watermarks.encode(o);
        self.entries.encode(o);
        self.vector.encode(o);
        self.strict_no_aba.encode(o);
    }
}
/// Why issuance failed (ADR 0008 §6.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueFailure {
    /// A required entry is not CERTIFIED (SPEC §6.4 "签发").
    NotCertified {
        logical: LogicalId,
        rev: RevisionId,
        summary: Summary,
    },
    /// Two effective revisions of one logical among the required entries or in the vector (T21).
    Ambiguous { logical: LogicalId },
    /// No required entry.
    Empty,
}
/// A change reported by validation or recertification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertDiff {
    pub logical: LogicalId,
    /// Revision in the certificate (entry or vector), if any.
    pub cert_rev: Option<RevisionId>,
    /// Current head (entries) or current effective revision (vector), if any.
    pub current: Option<RevisionId>,
    /// SD13 summary of `current`.
    pub summary: Option<Summary>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidReason {
    /// A required entry is STALE or no longer the head.
    Changed,
    /// `strict_no_aba` and a head move, base flip or watermark advance in the window.
    StrictChange,
    /// The op_log frame at `issued_epoch` no longer matches (ADR 0008 §6.4 step 0).
    LogMismatch,
    /// Presented on a branch other than the one it was issued on (ADR 0013 §7).
    BranchMismatch,
}
/// Result of `validate_certificate` (SPEC §6.4), plus `Empty` (SPEC v1.5.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Validation {
    Denied,
    Invalid {
        reason: InvalidReason,
        diffs: Vec<CertDiff>,
    },
    Conflicted {
        entries: Vec<LogicalId>,
    },
    Ambiguous {
        logical: LogicalId,
    },
    Unknown {
        lag: Vec<String>,
    },
    Pending {
        rules: Vec<RuleId>,
    },
    Empty,
    Valid,
    ValidRecertified {
        new_cert: Box<Certificate>,
        diffs: Vec<CertDiff>,
    },
}

impl Wire for Via {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Head),
            1 => Ok(Self::Alternative),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for CertItem {
    fn encode(&self, o: &mut Vec<u8>) {
        self.logical.encode(o);
        self.rev.encode(o);
        self.required.encode(o);
        self.via.encode(o);
        self.since_epoch.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            logical: Wire::decode(i)?,
            rev: Wire::decode(i)?,
            required: Wire::decode(i)?,
            via: Wire::decode(i)?,
            since_epoch: Wire::decode(i)?,
        })
    }
}
impl Wire for VectorItem {
    fn encode(&self, o: &mut Vec<u8>) {
        self.logical.encode(o);
        self.rev.encode(o);
        self.since_epoch.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            logical: Wire::decode(i)?,
            rev: Wire::decode(i)?,
            since_epoch: Wire::decode(i)?,
        })
    }
}
impl Wire for Certificate {
    fn encode(&self, o: &mut Vec<u8>) {
        self.cert_id.encode(o);
        self.encode_body(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            cert_id: Wire::decode(i)?,
            session_id: Wire::decode(i)?,
            principal: Wire::decode(i)?,
            branch: Wire::decode(i)?,
            issued_epoch: Wire::decode(i)?,
            log_checksum_at_issue: Wire::decode(i)?,
            code_commit: Wire::decode(i)?,
            watermarks: Wire::decode(i)?,
            entries: Wire::decode(i)?,
            vector: Wire::decode(i)?,
            strict_no_aba: Wire::decode(i)?,
        })
    }
}
impl Wire for Summary {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Unavailable => 0u8.encode(o),
            Self::Conflicted => 1u8.encode(o),
            Self::Stale(r) => {
                2u8.encode(o);
                r.encode(o);
            }
            Self::Pending(r) => {
                3u8.encode(o);
                r.encode(o);
            }
            Self::Certified => 4u8.encode(o),
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Unavailable,
            1 => Self::Conflicted,
            2 => Self::Stale(StaleReason::decode(i)?),
            3 => Self::Pending(PendingReason::decode(i)?),
            4 => Self::Certified,
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
impl Wire for IssueFailure {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::NotCertified {
                logical,
                rev,
                summary,
            } => {
                0u8.encode(o);
                logical.encode(o);
                rev.encode(o);
                summary.encode(o);
            }
            Self::Ambiguous { logical } => {
                1u8.encode(o);
                logical.encode(o);
            }
            Self::Empty => 2u8.encode(o),
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::NotCertified {
                logical: Wire::decode(i)?,
                rev: Wire::decode(i)?,
                summary: Wire::decode(i)?,
            },
            1 => Self::Ambiguous {
                logical: Wire::decode(i)?,
            },
            2 => Self::Empty,
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
impl Wire for CertDiff {
    fn encode(&self, o: &mut Vec<u8>) {
        self.logical.encode(o);
        self.cert_rev.encode(o);
        self.current.encode(o);
        self.summary.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            logical: Wire::decode(i)?,
            cert_rev: Wire::decode(i)?,
            current: Wire::decode(i)?,
            summary: Wire::decode(i)?,
        })
    }
}
impl Wire for InvalidReason {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Changed),
            1 => Ok(Self::StrictChange),
            2 => Ok(Self::LogMismatch),
            3 => Ok(Self::BranchMismatch),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for Validation {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Denied => 0u8.encode(o),
            Self::Invalid { reason, diffs } => {
                1u8.encode(o);
                reason.encode(o);
                diffs.encode(o);
            }
            Self::Conflicted { entries } => {
                2u8.encode(o);
                entries.encode(o);
            }
            Self::Ambiguous { logical } => {
                3u8.encode(o);
                logical.encode(o);
            }
            Self::Unknown { lag } => {
                4u8.encode(o);
                lag.encode(o);
            }
            Self::Pending { rules } => {
                5u8.encode(o);
                rules.encode(o);
            }
            Self::Empty => 6u8.encode(o),
            Self::Valid => 7u8.encode(o),
            Self::ValidRecertified { new_cert, diffs } => {
                8u8.encode(o);
                new_cert.encode(o);
                diffs.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Denied,
            1 => Self::Invalid {
                reason: Wire::decode(i)?,
                diffs: Wire::decode(i)?,
            },
            2 => Self::Conflicted {
                entries: Wire::decode(i)?,
            },
            3 => Self::Ambiguous {
                logical: Wire::decode(i)?,
            },
            4 => Self::Unknown {
                lag: Wire::decode(i)?,
            },
            5 => Self::Pending {
                rules: Wire::decode(i)?,
            },
            6 => Self::Empty,
            7 => Self::Valid,
            8 => Self::ValidRecertified {
                new_cert: Box::new(Wire::decode(i)?),
                diffs: Wire::decode(i)?,
            },
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
