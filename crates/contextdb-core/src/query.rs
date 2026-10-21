use crate::{
    Indexes, InputView,
    engine::CountingState,
    layers::{LayerColumns, semantic, vectors},
};
use contextdb_model::*;

pub(crate) fn why(
    i: &impl InputView,
    c: &CountingState,
    index: &Indexes,
    r: RevisionId,
) -> Result<Explanation, TxError> {
    if r.index() >= i.revision_count() {
        return Err(TxError::UnknownRef);
    }
    Ok(Explanation {
        revision: r,
        base: i.base(r),
        active_rules: index
            .rules_by_concl
            .row(r.index())
            .iter()
            .filter(|j| c.counted[j.index()])
            .copied()
            .collect(),
        witness: c.witness[r.index()],
        references: index
            .rules_by_concl
            .row(r.index())
            .iter()
            .flat_map(|j| i.references(*j).iter().map(move |r| (*j, *r)))
            .collect(),
    })
}
pub(crate) fn why_not(
    i: &impl InputView,
    c: &CountingState,
    index: &Indexes,
    layers: &LayerColumns,
    r: RevisionId,
    depth: usize,
) -> Result<WhyNot, TxError> {
    if r.index() >= i.revision_count() {
        return Err(TxError::UnknownRef);
    }
    let mut rules = Vec::new();
    for j in index.rules_by_concl.row(r.index()) {
        let (asserted, current) = vectors(i, *j);
        let mut incidences = Vec::new();
        let mut upstream = Vec::new();
        for p in i.premises(*j) {
            let logical = i.revision_logical(p.premise);
            let head = i.head(logical);
            let node = layers.node_state(i, p.premise, c.row(p.premise));
            if !node.l0 {
                incidences.push(IncidenceFailure::Unsupported(p.premise));
            }
            if p.kind == EdgeKind::Cur && head != p.premise {
                incidences.push(IncidenceFailure::NotCurrent {
                    premise: p.premise,
                    head,
                });
            }
            if p.kind == EdgeKind::Pin
                && head != p.premise
                && i.logical_kind(logical) != RecordKind::Evidence
            {
                incidences.push(IncidenceFailure::SupersededPinned {
                    premise: p.premise,
                    head,
                });
            }
            let head_l2 = if p.kind == EdgeKind::Sem {
                let h = layers.node_state(i, head, c.row(head));
                if !h.l1 {
                    incidences.push(IncidenceFailure::SemanticHeadUnsupported(head));
                }
                h.l2
            } else {
                true
            };
            if depth > 0 && !node.l2 {
                upstream.push(why_not(i, c, index, layers, p.premise, depth - 1)?);
            }
            if depth > 0 && p.kind == EdgeKind::Sem && head != p.premise && !head_l2 {
                upstream.push(why_not(i, c, index, layers, head, depth - 1)?);
            }
        }
        rules.push(RuleFailure {
            rule: *j,
            disabled: !i.enabled(*j),
            gate: semantic(i, *j),
            asserted,
            current,
            incidences,
            upstream,
        });
    }
    Ok(WhyNot { revision: r, rules })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proof {
    pub explanation: Explanation,
    pub premises: Vec<Proof>,
    pub truncated: bool,
}
pub(crate) fn proof(
    i: &impl InputView,
    c: &CountingState,
    index: &Indexes,
    r: RevisionId,
    depth: usize,
) -> Result<Proof, TxError> {
    let explanation = why(i, c, index, r)?;
    let mut premises = Vec::new();
    let mut truncated = false;
    if let Some(Witness::Rule(j)) = explanation.witness {
        if depth == 0 {
            truncated = true;
        } else {
            for p in i.premises(j) {
                premises.push(proof(i, c, index, p.premise, depth - 1)?);
            }
        }
    }
    Ok(Proof {
        explanation,
        premises,
        truncated,
    })
}
impl Wire for Proof {
    fn encode(&self, o: &mut Vec<u8>) {
        self.explanation.encode(o);
        self.premises.encode(o);
        self.truncated.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            explanation: Wire::decode(i)?,
            premises: Wire::decode(i)?,
            truncated: Wire::decode(i)?,
        })
    }
}
