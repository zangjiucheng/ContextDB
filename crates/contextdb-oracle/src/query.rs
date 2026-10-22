use crate::{asserted_vector, current_vector, jacobi, summarize};
use contextdb_model::*;
use std::collections::BTreeMap;

pub fn why(i: &Inputs, d: &DerivedSnapshot, r: RevisionId) -> Result<Explanation, TxError> {
    let node = d.nodes.get(r.index()).ok_or(TxError::UnknownRef)?;
    let active_rules = i
        .rules
        .iter()
        .enumerate()
        .filter(|(n, j)| j.conclusion == r && d.active[*n])
        .map(|(n, _)| RuleId(n as u32))
        .collect();
    let references = i
        .rules
        .iter()
        .enumerate()
        .filter(|(_, j)| j.conclusion == r)
        .flat_map(|(n, j)| j.references.iter().map(move |r| (RuleId(n as u32), *r)))
        .collect();
    Ok(Explanation {
        revision: r,
        base: i.base[r.index()],
        active_rules,
        witness: node.witness[0],
        references,
    })
}
pub fn why_not(
    i: &Inputs,
    d: &DerivedSnapshot,
    r: RevisionId,
    depth: usize,
) -> Result<WhyNot, TxError> {
    d.nodes.get(r.index()).ok_or(TxError::UnknownRef)?;
    let mut rules = Vec::new();
    for (n, j) in i
        .rules
        .iter()
        .enumerate()
        .filter(|(_, j)| j.conclusion == r)
    {
        let mut incidences = Vec::new();
        let mut upstream = Vec::new();
        for p in &j.premises {
            let l = i.revisions[p.premise.index()].logical.index();
            let h = i.heads[l];
            let node = &d.nodes[p.premise.index()];
            if !node.l0 {
                incidences.push(IncidenceFailure::Unsupported(p.premise));
            }
            if p.kind == EdgeKind::Cur && h != p.premise {
                incidences.push(IncidenceFailure::NotCurrent {
                    premise: p.premise,
                    head: h,
                });
            }
            if p.kind == EdgeKind::Pin
                && h != p.premise
                && i.logicals[l].kind != RecordKind::Evidence
            {
                incidences.push(IncidenceFailure::SupersededPinned {
                    premise: p.premise,
                    head: h,
                });
            }
            if p.kind == EdgeKind::Sem && !d.nodes[h.index()].l1 {
                incidences.push(IncidenceFailure::SemanticHeadUnsupported(h));
            }
            if depth > 0 && !node.l2 {
                upstream.push(why_not(i, d, p.premise, depth - 1)?);
            }
            if depth > 0 && p.kind == EdgeKind::Sem && h != p.premise && !d.nodes[h.index()].l2 {
                upstream.push(why_not(i, d, h, depth - 1)?);
            }
        }
        rules.push(RuleFailure {
            rule: RuleId(n as u32),
            disabled: !j.enabled,
            gate: d.sem[n],
            asserted: asserted_vector(i, j),
            current: current_vector(i, j),
            incidences,
            upstream,
        });
    }
    Ok(WhyNot { revision: r, rules })
}
/// P0 pure required-entry verdict. No epoch parameter or epoch comparison.
/// Full certificate closures, strict history and recertification arrive in P3.
pub fn cert_verdict(
    vector: &[CertEntry],
    i: &Inputs,
    live_pos: &BTreeMap<String, Vec<u8>>,
) -> CertVerdict {
    let d = jacobi(i);
    let mut priority = 7;
    let mut logicals = BTreeMap::new();
    for e in vector.iter().filter(|e| e.required) {
        let p = match i.revisions.get(e.revision.index()) {
            None => 1,
            Some(rev) if rev.logical != e.logical => 1,
            Some(_) => {
                let mut dim = d.nodes[e.revision.index()].dimensions.clone();
                if let Some(logical) = i.logicals.get(e.logical.index())
                    && logical.kind == RecordKind::CodeAnchor
                    && live_pos
                        .get(&logical.scope_lane)
                        .is_none_or(|v| i.watermarks.get(&logical.scope_lane) != Some(v))
                {
                    dim.observation = Observation::Lagging;
                }
                match summarize(&dim) {
                    Summary::Unavailable => 0,
                    Summary::Stale(_) => 1,
                    Summary::Conflicted => 2,
                    Summary::Pending(PendingReason::ObservationLag) => 4,
                    Summary::Pending(_) => 5,
                    Summary::Certified => 7,
                }
            }
        };
        priority = priority.min(p);
        if let Some(old) = logicals.insert(e.logical, e.revision)
            && old != e.revision
        {
            priority = priority.min(3);
        }
    }
    match priority {
        0 => CertVerdict::Denied,
        1 => CertVerdict::Invalid,
        2 => CertVerdict::Conflicted,
        3 => CertVerdict::Ambiguous,
        4 => CertVerdict::Unknown,
        5 => CertVerdict::Pending,
        _ => CertVerdict::Valid,
    }
}
