//! Canonical little-endian codec. Collections use u32 lengths and sorted map keys.
use crate::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    InvalidEncoding,
    UnknownVersion,
    TrailingBytes,
    LimitExceeded,
}
impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for WireError {}
const MAX_BYTES: usize = 16 * 1024 * 1024;
pub trait Wire: Sized {
    fn encode(&self, out: &mut Vec<u8>);
    fn decode(input: &mut &[u8]) -> Result<Self, WireError>;
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.encode(&mut v);
        v
    }
    fn from_bytes(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() > MAX_BYTES {
            return Err(WireError::LimitExceeded);
        }
        let mut input = bytes;
        let value = Self::decode(&mut input)?;
        if !input.is_empty() {
            return Err(WireError::TrailingBytes);
        }
        Ok(value)
    }
}
fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8], WireError> {
    if n > input.len() {
        return Err(WireError::InvalidEncoding);
    }
    let (a, b) = input.split_at(n);
    *input = b;
    Ok(a)
}
macro_rules! number {($($t:ty),*)=>{$(impl Wire for $t {fn encode(&self,o:&mut Vec<u8>){o.extend(self.to_le_bytes())}fn decode(i:&mut &[u8])->Result<Self,WireError>{Ok(Self::from_le_bytes(take(i,std::mem::size_of::<Self>())?.try_into().map_err(|_|WireError::InvalidEncoding)?))}})*};}
number!(u8, u16, u32, u64);
impl Wire for bool {
    fn encode(&self, o: &mut Vec<u8>) {
        u8::from(*self).encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl<const N: usize> Wire for [u8; N] {
    fn encode(&self, o: &mut Vec<u8>) {
        o.extend(self)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        take(i, N)?
            .try_into()
            .map_err(|_| WireError::InvalidEncoding)
    }
}
impl<T: Wire> Wire for Vec<T> {
    fn encode(&self, o: &mut Vec<u8>) {
        u32::try_from(self.len())
            .expect("wire length capacity")
            .encode(o);
        for x in self {
            x.encode(o)
        }
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        let n = u32::decode(i)? as usize;
        if n > 1_000_000 || n > i.len() {
            return Err(WireError::LimitExceeded);
        }
        (0..n).map(|_| T::decode(i)).collect()
    }
}
impl Wire for String {
    fn encode(&self, o: &mut Vec<u8>) {
        self.as_bytes().to_vec().encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        String::from_utf8(Vec::<u8>::decode(i)?).map_err(|_| WireError::InvalidEncoding)
    }
}
impl<T: Wire> Wire for Option<T> {
    fn encode(&self, o: &mut Vec<u8>) {
        self.is_some().encode(o);
        if let Some(x) = self {
            x.encode(o)
        }
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        if bool::decode(i)? {
            Ok(Some(T::decode(i)?))
        } else {
            Ok(None)
        }
    }
}
impl<A: Wire, B: Wire> Wire for (A, B) {
    fn encode(&self, o: &mut Vec<u8>) {
        self.0.encode(o);
        self.1.encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok((A::decode(i)?, B::decode(i)?))
    }
}
impl<T: Wire + Ord> Wire for BTreeSet<T> {
    fn encode(&self, o: &mut Vec<u8>) {
        u32::try_from(self.len())
            .expect("wire length capacity")
            .encode(o);
        for x in self {
            x.encode(o)
        }
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        let v = Vec::<T>::decode(i)?;
        if v.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(WireError::InvalidEncoding);
        }
        let n = v.len();
        let set: Self = v.into_iter().collect();
        if set.len() != n {
            return Err(WireError::InvalidEncoding);
        }
        Ok(set)
    }
}
impl<K: Wire + Ord, V: Wire> Wire for BTreeMap<K, V> {
    fn encode(&self, o: &mut Vec<u8>) {
        u32::try_from(self.len())
            .expect("wire length capacity")
            .encode(o);
        for pair in self {
            pair.0.encode(o);
            pair.1.encode(o)
        }
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        let v = Vec::<(K, V)>::decode(i)?;
        if v.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(WireError::InvalidEncoding);
        }
        let n = v.len();
        let map: Self = v.into_iter().collect();
        if map.len() != n {
            return Err(WireError::InvalidEncoding);
        }
        Ok(map)
    }
}
impl Wire for LogicalId {
    fn encode(&self, o: &mut Vec<u8>) {
        self.0.encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self(u32::decode(i)?))
    }
}
impl Wire for RevisionId {
    fn encode(&self, o: &mut Vec<u8>) {
        self.0.encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self(u32::decode(i)?))
    }
}
impl Wire for RuleId {
    fn encode(&self, o: &mut Vec<u8>) {
        self.0.encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self(u32::decode(i)?))
    }
}
impl Wire for ArtifactId {
    fn encode(&self, o: &mut Vec<u8>) {
        self.0.encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self(u32::decode(i)?))
    }
}
impl Wire for IncId {
    fn encode(&self, o: &mut Vec<u8>) {
        self.0.encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self(u32::decode(i)?))
    }
}
impl Wire for BranchId {
    fn encode(&self, o: &mut Vec<u8>) {
        self.0.encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self(u16::decode(i)?))
    }
}
impl Wire for PrincipalId {
    fn encode(&self, o: &mut Vec<u8>) {
        self.0.encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self(u32::decode(i)?))
    }
}
impl Wire for Epoch {
    fn encode(&self, o: &mut Vec<u8>) {
        self.0.encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self(u64::decode(i)?))
    }
}
impl Wire for EdgeKind {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Ref),
            1 => Ok(Self::Pin),
            2 => Ok(Self::Sem),
            3 => Ok(Self::Cur),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for RecordKind {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Fact),
            1 => Ok(Self::Assumption),
            2 => Ok(Self::Decision),
            3 => Ok(Self::Plan),
            4 => Ok(Self::Convention),
            5 => Ok(Self::Procedure),
            6 => Ok(Self::Artifact),
            7 => Ok(Self::Evidence),
            8 => Ok(Self::CodeAnchor),
            9 => Ok(Self::Summary),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for Channel {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Agent),
            1 => Ok(Self::Observer),
            2 => Ok(Self::Admin),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for Verdict {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Verified),
            1 => Ok(Self::Rejected),
            2 => Ok(Self::Pending),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for ArtifactType {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Conflict),
            1 => Ok(Self::PendingReview),
            2 => Ok(Self::ImportBroken),
            3 => Ok(Self::AnchorBroken),
            4 => Ok(Self::DriftReport),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for SemState {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::NotRequired),
            1 => Ok(Self::Verified),
            2 => Ok(Self::Pending),
            3 => Ok(Self::Rejected),
            4 => Ok(Self::NotApplicable),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for PendingReason {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Semantic),
            1 => Ok(Self::StructuralUnknown),
            2 => Ok(Self::ObservationLag),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for Structural {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Supported),
            1 => Ok(Self::Unsupported),
            2 => Ok(Self::Unknown),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for Currency {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Current),
            1 => Ok(Self::Superseded),
            2 => Ok(Self::BranchOnly),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for Observation {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::NotApplicable),
            1 => Ok(Self::Fresh),
            2 => Ok(Self::Lagging),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for PolicyDoc {
    fn encode(&self, o: &mut Vec<u8>) {
        self.version.encode(o);
        self.require_deps.encode(o);
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            version: Wire::decode(i)?,
            require_deps: Wire::decode(i)?,
        })
    }
}
impl Wire for Batch {
    fn encode(&self, o: &mut Vec<u8>) {
        self.branch.encode(o);
        self.principal.encode(o);
        self.channel.encode(o);
        self.client_txn_id.encode(o);
        self.ops.encode(o);
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            branch: Wire::decode(i)?,
            principal: Wire::decode(i)?,
            channel: Wire::decode(i)?,
            client_txn_id: Wire::decode(i)?,
            ops: Wire::decode(i)?,
        })
    }
}
impl Wire for Logical {
    fn encode(&self, o: &mut Vec<u8>) {
        self.key.encode(o);
        self.kind.encode(o);
        self.scope_lane.encode(o);
        self.owner.encode(o);
        self.origin_branch.encode(o);
        self.origin_epoch.encode(o);
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            key: Wire::decode(i)?,
            kind: Wire::decode(i)?,
            scope_lane: Wire::decode(i)?,
            owner: Wire::decode(i)?,
            origin_branch: Wire::decode(i)?,
            origin_epoch: Wire::decode(i)?,
        })
    }
}
impl Wire for Revision {
    fn encode(&self, o: &mut Vec<u8>) {
        self.logical.encode(o);
        self.content.encode(o);
        self.author.encode(o);
        self.origin_branch.encode(o);
        self.origin_epoch.encode(o);
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            logical: Wire::decode(i)?,
            content: Wire::decode(i)?,
            author: Wire::decode(i)?,
            origin_branch: Wire::decode(i)?,
            origin_epoch: Wire::decode(i)?,
        })
    }
}
impl Wire for Incidence {
    fn encode(&self, o: &mut Vec<u8>) {
        self.premise.encode(o);
        self.kind.encode(o);
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            premise: Wire::decode(i)?,
            kind: Wire::decode(i)?,
        })
    }
}
impl Wire for Rule {
    fn encode(&self, o: &mut Vec<u8>) {
        self.conclusion.encode(o);
        self.premises.encode(o);
        self.references.encode(o);
        self.enabled.encode(o);
        self.origin_branch.encode(o);
        self.origin_epoch.encode(o);
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            conclusion: Wire::decode(i)?,
            premises: Wire::decode(i)?,
            references: Wire::decode(i)?,
            enabled: Wire::decode(i)?,
            origin_branch: Wire::decode(i)?,
            origin_epoch: Wire::decode(i)?,
        })
    }
}
impl Wire for ValidationRecord {
    fn encode(&self, o: &mut Vec<u8>) {
        self.verdict.encode(o);
        self.validator.encode(o);
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            verdict: Wire::decode(i)?,
            validator: Wire::decode(i)?,
        })
    }
}
impl Wire for Metadata {
    fn encode(&self, o: &mut Vec<u8>) {
        self.tags.encode(o);
        self.dirs.encode(o);
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            tags: Wire::decode(i)?,
            dirs: Wire::decode(i)?,
        })
    }
}
impl Wire for Artifact {
    fn encode(&self, o: &mut Vec<u8>) {
        self.ty.encode(o);
        self.target.encode(o);
        self.payload_hash.encode(o);
        self.open.encode(o);
        self.author.encode(o);
        self.origin_branch.encode(o);
        self.origin_epoch.encode(o);
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            ty: Wire::decode(i)?,
            target: Wire::decode(i)?,
            payload_hash: Wire::decode(i)?,
            open: Wire::decode(i)?,
            author: Wire::decode(i)?,
            origin_branch: Wire::decode(i)?,
            origin_epoch: Wire::decode(i)?,
        })
    }
}
impl Wire for Inputs {
    fn encode(&self, o: &mut Vec<u8>) {
        self.logicals.encode(o);
        self.revisions.encode(o);
        self.heads.encode(o);
        self.base.encode(o);
        self.rules.encode(o);
        self.validations.encode(o);
        self.artifacts.encode(o);
        self.metadata.encode(o);
        self.watermarks.encode(o);
        self.policy.encode(o);
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            logicals: Wire::decode(i)?,
            revisions: Wire::decode(i)?,
            heads: Wire::decode(i)?,
            base: Wire::decode(i)?,
            rules: Wire::decode(i)?,
            validations: Wire::decode(i)?,
            artifacts: Wire::decode(i)?,
            metadata: Wire::decode(i)?,
            watermarks: Wire::decode(i)?,
            policy: Wire::decode(i)?,
        })
    }
}
impl Wire for Dimensions {
    fn encode(&self, o: &mut Vec<u8>) {
        self.authorized.encode(o);
        self.conflicted.encode(o);
        self.currency.encode(o);
        self.structural.encode(o);
        self.l1.encode(o);
        self.l2.encode(o);
        self.semantic.encode(o);
        self.stale_reason.encode(o);
        self.observation.encode(o);
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            authorized: Wire::decode(i)?,
            conflicted: Wire::decode(i)?,
            currency: Wire::decode(i)?,
            structural: Wire::decode(i)?,
            l1: Wire::decode(i)?,
            l2: Wire::decode(i)?,
            semantic: Wire::decode(i)?,
            stale_reason: Wire::decode(i)?,
            observation: Wire::decode(i)?,
        })
    }
}
impl Wire for Allocations {
    fn encode(&self, o: &mut Vec<u8>) {
        self.new_logicals.encode(o);
        self.new_revisions.encode(o);
        self.new_rules.encode(o);
        self.new_artifacts.encode(o);
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            new_logicals: Wire::decode(i)?,
            new_revisions: Wire::decode(i)?,
            new_rules: Wire::decode(i)?,
            new_artifacts: Wire::decode(i)?,
        })
    }
}
impl Wire for LogicalRef {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Existing(value) => {
                0u8.encode(o);
                value.encode(o);
            }
            Self::Key(value) => {
                1u8.encode(o);
                value.encode(o);
            }
            Self::New(value) => {
                2u8.encode(o);
                value.encode(o);
            }
        }
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Existing(Wire::decode(i)?)),
            1 => Ok(Self::Key(Wire::decode(i)?)),
            2 => Ok(Self::New(Wire::decode(i)?)),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for RevRef {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Existing(value) => {
                0u8.encode(o);
                value.encode(o);
            }
            Self::New(value) => {
                1u8.encode(o);
                value.encode(o);
            }
        }
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Existing(Wire::decode(i)?)),
            1 => Ok(Self::New(Wire::decode(i)?)),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for RuleRef {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Existing(value) => {
                0u8.encode(o);
                value.encode(o);
            }
            Self::New(value) => {
                1u8.encode(o);
                value.encode(o);
            }
        }
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Existing(Wire::decode(i)?)),
            1 => Ok(Self::New(Wire::decode(i)?)),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for ArtifactRef {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Existing(value) => {
                0u8.encode(o);
                value.encode(o);
            }
            Self::New(value) => {
                1u8.encode(o);
                value.encode(o);
            }
        }
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Existing(Wire::decode(i)?)),
            1 => Ok(Self::New(Wire::decode(i)?)),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for Witness {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Base => {
                0u8.encode(o);
            }
            Self::Rule(value) => {
                1u8.encode(o);
                value.encode(o);
            }
        }
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::Base),
            1 => Ok(Self::Rule(Wire::decode(i)?)),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for StaleReason {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::HeadSuperseded => {
                0u8.encode(o);
            }
            Self::Unsupported => {
                1u8.encode(o);
            }
            Self::Rejected => {
                2u8.encode(o);
            }
            Self::SupersededPinnedPremise(value) => {
                3u8.encode(o);
                value.encode(o);
            }
        }
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::HeadSuperseded),
            1 => Ok(Self::Unsupported),
            2 => Ok(Self::Rejected),
            3 => Ok(Self::SupersededPinnedPremise(Wire::decode(i)?)),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}
impl Wire for Op {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::CreateLogical {
                key,
                kind,
                scope_lane,
                content,
                adopt_as_base,
            } => {
                0u8.encode(o);
                key.encode(o);
                kind.encode(o);
                scope_lane.encode(o);
                content.encode(o);
                adopt_as_base.encode(o);
            }
            Self::AddRevision {
                logical,
                content,
                adopt_as_base,
            } => {
                1u8.encode(o);
                logical.encode(o);
                content.encode(o);
                adopt_as_base.encode(o);
            }
            Self::SetHead {
                logical,
                expected,
                new,
            } => {
                2u8.encode(o);
                logical.encode(o);
                expected.encode(o);
                new.encode(o);
            }
            Self::AssertBase { rev } => {
                3u8.encode(o);
                rev.encode(o);
            }
            Self::RetractBase { rev } => {
                4u8.encode(o);
                rev.encode(o);
            }
            Self::AddRule {
                conclusion,
                incidences,
                enabled,
            } => {
                5u8.encode(o);
                conclusion.encode(o);
                incidences.encode(o);
                enabled.encode(o);
            }
            Self::SetRuleEnabled { rule, enabled } => {
                6u8.encode(o);
                rule.encode(o);
                enabled.encode(o);
            }
            Self::Rebind { rule, from, to } => {
                7u8.encode(o);
                rule.encode(o);
                from.encode(o);
                to.encode(o);
            }
            Self::RecordValidation {
                rule,
                curvec_seen,
                verdict,
                validator,
            } => {
                8u8.encode(o);
                rule.encode(o);
                curvec_seen.encode(o);
                verdict.encode(o);
                validator.encode(o);
            }
            Self::SetPolicy { policy } => {
                9u8.encode(o);
                policy.encode(o);
            }
            Self::ObserverWatermark {
                scope,
                expected,
                position,
            } => {
                10u8.encode(o);
                scope.encode(o);
                expected.encode(o);
                position.encode(o);
            }
            Self::SetMetadata {
                logical,
                tags,
                dirs,
            } => {
                11u8.encode(o);
                logical.encode(o);
                tags.encode(o);
                dirs.encode(o);
            }
            Self::OpenArtifact {
                ty,
                target,
                payload_hash,
            } => {
                12u8.encode(o);
                ty.encode(o);
                target.encode(o);
                payload_hash.encode(o);
            }
            Self::ResolveArtifact { id } => {
                13u8.encode(o);
                id.encode(o);
            }
        }
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        match u8::decode(i)? {
            0 => Ok(Self::CreateLogical {
                key: Wire::decode(i)?,
                kind: Wire::decode(i)?,
                scope_lane: Wire::decode(i)?,
                content: Wire::decode(i)?,
                adopt_as_base: Wire::decode(i)?,
            }),
            1 => Ok(Self::AddRevision {
                logical: Wire::decode(i)?,
                content: Wire::decode(i)?,
                adopt_as_base: Wire::decode(i)?,
            }),
            2 => Ok(Self::SetHead {
                logical: Wire::decode(i)?,
                expected: Wire::decode(i)?,
                new: Wire::decode(i)?,
            }),
            3 => Ok(Self::AssertBase {
                rev: Wire::decode(i)?,
            }),
            4 => Ok(Self::RetractBase {
                rev: Wire::decode(i)?,
            }),
            5 => Ok(Self::AddRule {
                conclusion: Wire::decode(i)?,
                incidences: Wire::decode(i)?,
                enabled: Wire::decode(i)?,
            }),
            6 => Ok(Self::SetRuleEnabled {
                rule: Wire::decode(i)?,
                enabled: Wire::decode(i)?,
            }),
            7 => Ok(Self::Rebind {
                rule: Wire::decode(i)?,
                from: Wire::decode(i)?,
                to: Wire::decode(i)?,
            }),
            8 => Ok(Self::RecordValidation {
                rule: Wire::decode(i)?,
                curvec_seen: Wire::decode(i)?,
                verdict: Wire::decode(i)?,
                validator: Wire::decode(i)?,
            }),
            9 => Ok(Self::SetPolicy {
                policy: Wire::decode(i)?,
            }),
            10 => Ok(Self::ObserverWatermark {
                scope: Wire::decode(i)?,
                expected: Wire::decode(i)?,
                position: Wire::decode(i)?,
            }),
            11 => Ok(Self::SetMetadata {
                logical: Wire::decode(i)?,
                tags: Wire::decode(i)?,
                dirs: Wire::decode(i)?,
            }),
            12 => Ok(Self::OpenArtifact {
                ty: Wire::decode(i)?,
                target: Wire::decode(i)?,
                payload_hash: Wire::decode(i)?,
            }),
            13 => Ok(Self::ResolveArtifact {
                id: Wire::decode(i)?,
            }),
            _ => Err(WireError::InvalidEncoding),
        }
    }
}

impl Wire for NodeState {
    fn encode(&self, o: &mut Vec<u8>) {
        self.l0.encode(o);
        self.l1.encode(o);
        self.l2.encode(o);
        self.support.encode(o);
        for w in self.witness {
            w.encode(o)
        }
        self.dimensions.encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        Ok(Self {
            l0: Wire::decode(i)?,
            l1: Wire::decode(i)?,
            l2: Wire::decode(i)?,
            support: Wire::decode(i)?,
            witness: [Wire::decode(i)?, Wire::decode(i)?, Wire::decode(i)?],
            dimensions: Wire::decode(i)?,
        })
    }
}
impl Wire for DerivedSnapshot {
    fn encode(&self, o: &mut Vec<u8>) {
        self.nodes.encode(o);
        self.sat.encode(o);
        self.unsat.encode(o);
        self.active.encode(o);
        u32::try_from(self.layer_active.len())
            .expect("rule capacity")
            .encode(o);
        for a in &self.layer_active {
            a[0].encode(o);
            a[1].encode(o)
        }
        self.sem.encode(o)
    }
    fn decode(i: &mut &[u8]) -> Result<Self, WireError> {
        let nodes = Wire::decode(i)?;
        let sat = Wire::decode(i)?;
        let unsat = Wire::decode(i)?;
        let active = Wire::decode(i)?;
        let n = u32::decode(i)? as usize;
        if n > i.len() / 2 {
            return Err(WireError::InvalidEncoding);
        }
        let layer_active = (0..n)
            .map(|_| Ok([bool::decode(i)?, bool::decode(i)?]))
            .collect::<Result<_, WireError>>()?;
        Ok(Self {
            nodes,
            sat,
            unsat,
            active,
            layer_active,
            sem: Wire::decode(i)?,
        })
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventLog {
    pub batches: Vec<Batch>,
}
impl EventLog {
    pub const VERSION: u16 = 0;
    pub fn to_jsonl(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for batch in &self.batches {
            out.push_str("{\"version\":0,\"batch_hex\":\"");
            for b in batch.canonical_bytes() {
                write!(out, "{b:02x}").expect("string write")
            }
            out.push_str("\"}\n")
        }
        out
    }
    pub fn from_jsonl(text: &str) -> Result<Self, WireError> {
        let mut batches = Vec::new();
        if text.len() > MAX_BYTES * 2 {
            return Err(WireError::LimitExceeded);
        }
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let rest = line
                .strip_prefix("{\"version\":")
                .ok_or(WireError::InvalidEncoding)?;
            let (v, rest) = rest
                .split_once(",\"batch_hex\":\"")
                .ok_or(WireError::InvalidEncoding)?;
            if v != "0" {
                return Err(WireError::UnknownVersion);
            }
            let hex = rest.strip_suffix("\"}").ok_or(WireError::InvalidEncoding)?;
            if hex.len() % 2 != 0 || !hex.is_ascii() {
                return Err(WireError::InvalidEncoding);
            }
            let bytes = hex
                .as_bytes()
                .chunks_exact(2)
                .map(|c| {
                    let s = std::str::from_utf8(c).map_err(|_| WireError::InvalidEncoding)?;
                    u8::from_str_radix(s, 16).map_err(|_| WireError::InvalidEncoding)
                })
                .collect::<Result<Vec<_>, _>>()?;
            batches.push(Batch::from_bytes(&bytes)?)
        }
        Ok(Self { batches })
    }
}
