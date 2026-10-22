//! Pure observer classification; allocation references remain valid on dedup hits.
use crate::Descriptor;
use contextdb_model::*;
use std::collections::BTreeMap;

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
    pub oid: Vec<u8>,
    pub base: bool,
    pub breaks: Vec<ArtifactId>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prepared {
    pub blobs: Vec<Vec<u8>>,
    pub ops: Vec<Op>,
}
/// A whole scope is prepared before any durable submission. Missing new anchors
/// and inconsistent descriptors reject the scan, never publishing a partial one.
pub fn prepare(
    repo: &str,
    observations: &BTreeMap<String, Observation>,
    prior: &BTreeMap<String, AnchorState>,
    expected: Option<Vec<u8>>,
    position: Vec<u8>,
) -> Result<Prepared, String> {
    let mut out = Prepared::default();
    let mut next_revision = 0u16;
    for (path, observation) in observations {
        crate::validate_path(repo, path).map_err(|e| e.to_string())?;
        let old = prior.get(path);
        match observation {
            Observation::Present(d) => {
                d.validate().map_err(|e| e.to_string())?;
                if d.repo != repo || d.path != *path {
                    return Err("descriptor routing mismatch".into());
                }
                let content = d.content();
                match old {
                    None => {
                        out.blobs.push(d.canonical_bytes());
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
                        out.blobs.push(d.canonical_bytes());
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
            Observation::Missing | Observation::Broken(_) => {
                let s = old.ok_or_else(|| format!("cannot register missing/broken path {path}"))?;
                if s.base {
                    out.ops.push(Op::RetractBase { rev: s.head.into() });
                }
                if s.breaks.is_empty() {
                    let reason = match observation {
                        Observation::Missing => "missing",
                        Observation::Broken(r) => r,
                        _ => unreachable!(),
                    };
                    let candidates: Vec<_> = observations
                        .iter()
                        .filter_map(|(candidate, obs)| match obs {
                            Observation::Present(d) if candidate != path && d.oid == s.oid => {
                                Some(candidate.as_str())
                            }
                            _ => None,
                        })
                        .collect();
                    let payload = format!(
                        "anchor-broken:{path}:{reason};rebind=explicit;candidates={}",
                        candidates.join(",")
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
    if prior.keys().any(|p| !observations.contains_key(p)) {
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
