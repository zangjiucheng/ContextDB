//! Pure observer classification; allocation references remain valid on dedup hits.
use crate::{AnchorDescriptor, Descriptor};
use contextdb_model::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    Present(Descriptor),
    Missing,
    Broken(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorState {
    pub logical: LogicalId,
    pub head: RevisionId,
    pub content: ContentHash,
    /// Rebind match id of the head: a file's OID, a symbol's masked hash.
    pub oid: Vec<u8>,
    pub base: bool,
    pub breaks: Vec<ArtifactId>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prepared {
    pub blobs: Vec<Vec<u8>>,
    pub ops: Vec<Op>,
}
/// One anchor's observation in a scan, file or symbol (ADR 0012 §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorObservation {
    Present(AnchorDescriptor),
    Missing,
    Broken(String),
}
/// File-level scan (ADR 0011): observations and prior state keyed by path.
pub fn prepare(
    repo: &str,
    observations: &BTreeMap<String, Observation>,
    prior: &BTreeMap<String, AnchorState>,
    expected: Option<Vec<u8>>,
    position: Vec<u8>,
) -> Result<Prepared, String> {
    let mut by_key = BTreeMap::new();
    for (path, observation) in observations {
        crate::validate_legacy_path(repo, path).map_err(|e| e.to_string())?;
        let key = format!("code:{repo}/{path}");
        let observation = match observation {
            Observation::Present(d) => {
                if d.repo != repo || d.path != *path {
                    return Err("descriptor routing mismatch".into());
                }
                AnchorObservation::Present(AnchorDescriptor::File(d.clone()))
            }
            Observation::Missing => AnchorObservation::Missing,
            Observation::Broken(r) => AnchorObservation::Broken(r.clone()),
        };
        by_key.insert(key, (path.clone(), observation));
    }
    let prior = prior
        .iter()
        .map(|(path, s)| (format!("code:{repo}/{path}"), s.clone()))
        .collect();
    prepare_anchors(repo, &by_key, &prior, &BTreeMap::new(), expected, position)
}
/// A whole scope is prepared before any durable submission. Missing new anchors
/// and inconsistent descriptors reject the scan, never publishing a partial one.
/// Observations and prior state are keyed by logical key; each observation
/// carries the label a broken-anchor payload names it by (the path, or
/// `path#symbol`). `extra_candidates` maps a match id (file OID or symbol
/// name-masked hash) to unregistered rebind candidates the observer found.
pub fn prepare_anchors(
    repo: &str,
    observations: &BTreeMap<String, (String, AnchorObservation)>,
    prior: &BTreeMap<String, AnchorState>,
    extra_candidates: &BTreeMap<Vec<u8>, BTreeSet<String>>,
    expected: Option<Vec<u8>>,
    position: Vec<u8>,
) -> Result<Prepared, String> {
    let mut out = Prepared::default();
    let mut next_revision = 0u16;
    for (key, (label, observation)) in observations {
        let old = prior.get(key);
        match observation {
            AnchorObservation::Present(d) => {
                d.validate().map_err(|e| e.to_string())?;
                if d.scope() != format!("code:{repo}") || d.key() != *key {
                    return Err("descriptor routing mismatch".into());
                }
                let content = d.content();
                match old {
                    None => {
                        out.blobs.push(d.canonical());
                        out.ops.push(Op::CreateLogical {
                            key: d.key(),
                            kind: RecordKind::CodeAnchor,
                            scope_lane: d.scope(),
                            content,
                            adopt_as_base: true,
                        });
                        next_revision = next_revision.checked_add(1).ok_or("too many anchors")?;
                    }
                    Some(s) if s.content != content => {
                        out.blobs.push(d.canonical());
                        out.ops.push(Op::AddRevision {
                            logical: s.logical.into(),
                            content,
                            adopt_as_base: true,
                        });
                        let new = RevRef::New(next_revision);
                        next_revision = next_revision.checked_add(1).ok_or("too many anchors")?;
                        out.ops.push(Op::SetHead {
                            logical: s.logical.into(),
                            expected: s.head.into(),
                            new,
                        });
                    }
                    Some(s) if !s.base => out.ops.push(Op::AssertBase { rev: s.head.into() }),
                    Some(_) => {}
                }
                if let Some(s) = old {
                    for id in &s.breaks {
                        out.ops.push(Op::ResolveArtifact { id: (*id).into() });
                    }
                }
            }
            AnchorObservation::Missing | AnchorObservation::Broken(_) => {
                let s =
                    old.ok_or_else(|| format!("cannot register missing/broken anchor {label}"))?;
                if s.base {
                    out.ops.push(Op::RetractBase { rev: s.head.into() });
                }
                if s.breaks.is_empty() {
                    let reason = match observation {
                        AnchorObservation::Missing => "missing",
                        AnchorObservation::Broken(r) => r,
                        _ => unreachable!(),
                    };
                    let mut candidates: BTreeSet<String> = observations
                        .iter()
                        .filter_map(|(candidate, (l, obs))| match obs {
                            AnchorObservation::Present(d)
                                if candidate != key && d.match_id() == s.oid.as_slice() =>
                            {
                                Some(l.clone())
                            }
                            _ => None,
                        })
                        .collect();
                    if let Some(extra) = extra_candidates.get(&s.oid) {
                        candidates.extend(extra.iter().filter(|c| *c != label).cloned());
                    }
                    let payload = format!(
                        "anchor-broken:{label}:{reason};rebind=explicit;candidates={}",
                        candidates.into_iter().collect::<Vec<_>>().join(",")
                    )
                    .into_bytes();
                    let payload_hash = *blake3::hash(&payload).as_bytes();
                    out.blobs.push(payload);
                    out.ops.push(Op::OpenArtifact {
                        ty: ArtifactType::AnchorBroken,
                        target: s.head.into(),
                        payload_hash,
                    });
                }
            }
        }
    }
    if prior.keys().any(|k| !observations.contains_key(k)) {
        return Err("incomplete scope capture".into());
    }
    // The watermark CAS goes first: a racing observer then always loses on
    // WatermarkConflict (retried after a reload), never on a later content op
    // such as resolving a break the winner already resolved.
    if expected.as_ref() != Some(&position) {
        out.ops.insert(
            0,
            Op::ObserverWatermark {
                scope: format!("code:{repo}"),
                expected,
                position,
            },
        );
    }
    Ok(out)
}
