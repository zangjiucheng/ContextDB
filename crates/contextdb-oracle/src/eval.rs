use crate::input::{cycle_graph, semantic};
use contextdb_model::*;
use std::collections::{BTreeMap, BTreeSet};

/// SD11 `Conflicted(r)`: an open `Conflict` artifact targets `r` (the P6
/// gate, ADR 0013 §6.4).
pub fn conflicted(i: &Inputs) -> Vec<bool> {
    let mut out = vec![false; i.revisions.len()];
    for a in &i.artifacts {
        if a.ty == ArtifactType::Conflict && a.open {
            out[a.target.index()] = true;
        }
    }
    out
}
/// Simultaneous positive least fixed point, including for raw cyclic inputs.
pub fn jacobi(i: &Inputs) -> DerivedSnapshot {
    let gate = conflicted(i);
    let mut layers = vec![[false; 3]; i.revisions.len()];
    loop {
        let mut next: Vec<_> = i.base.iter().map(|b| [*b; 3]).collect();
        for (n, j) in i.rules.iter().enumerate() {
            if !j.enabled {
                continue;
            }
            let sem = semantic(i, RuleId(n as u32));
            for layer in 0..3 {
                if (layer == 1 && sem == SemState::Rejected)
                    || (layer == 2 && !matches!(sem, SemState::Verified | SemState::NotRequired))
                {
                    continue;
                }
                let all = j.premises.iter().all(|p| {
                    let l = i.revisions[p.premise.index()].logical.index();
                    layers[p.premise.index()][layer]
                        && (p.kind != EdgeKind::Cur || i.heads[l] == p.premise)
                        && (layer == 0
                            || p.kind != EdgeKind::Pin
                            || i.heads[l] == p.premise
                            || i.logicals[l].kind == RecordKind::Evidence)
                        && (layer == 0
                            || p.kind != EdgeKind::Sem
                            || layers[i.heads[l].index()][layer])
                });
                next[j.conclusion.index()][layer] |= all;
            }
        }
        for (n, gated) in gate.iter().enumerate() {
            next[n][2] &= !gated;
        }
        if next == layers {
            return snapshot(i, layers);
        }
        layers = next;
    }
}

/// Independently evaluates in Kahn rank order. Cycles have no topological order.
pub fn kahn(i: &Inputs) -> Result<DerivedSnapshot, Vec<CycleNode>> {
    let graph = cycle_graph(i);
    let mut indegree: BTreeMap<_, usize> = graph.keys().cloned().map(|n| (n, 0)).collect();
    for edges in graph.values() {
        for n in edges {
            *indegree.entry(n.clone()).or_default() += 1;
        }
    }
    let mut ready: BTreeSet<_> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| n.clone())
        .collect();
    let mut count = 0;
    let gate = conflicted(i);
    let mut truth = vec![[false; 3]; i.revisions.len()];
    while let Some(node) = ready.pop_first() {
        count += 1;
        if let CycleNode::Revision(r) = node {
            let mut result = [i.base[r.index()]; 3];
            for (id, rule) in i
                .rules
                .iter()
                .enumerate()
                .filter(|(_, j)| j.conclusion == r && j.enabled)
            {
                let gate = semantic(i, RuleId(id as u32));
                let mut succeeds = [true; 3];
                succeeds[1] = gate != SemState::Rejected;
                succeeds[2] = matches!(gate, SemState::NotRequired | SemState::Verified);
                for inc in &rule.premises {
                    let p = inc.premise.index();
                    let l = i.revisions[p].logical.index();
                    let head = i.heads[l];
                    for level in 0..3 {
                        succeeds[level] &= truth[p][level];
                        match inc.kind {
                            EdgeKind::Cur => succeeds[level] &= head == inc.premise,
                            EdgeKind::Pin if level > 0 => {
                                succeeds[level] &= head == inc.premise
                                    || i.logicals[l].kind == RecordKind::Evidence
                            }
                            EdgeKind::Sem if level > 0 => {
                                succeeds[level] &= truth[head.index()][level]
                            }
                            _ => {}
                        }
                    }
                }
                for level in 0..3 {
                    result[level] |= succeeds[level];
                }
            }
            result[2] &= !gate[r.index()];
            truth[r.index()] = result;
        }
        for next in &graph[&node] {
            let d = indegree.get_mut(next).expect("graph contains destinations");
            *d -= 1;
            if *d == 0 {
                ready.insert(next.clone());
            }
        }
    }
    if count != graph.len() {
        return Err(crate::input::cycle_path(i).expect("non-DAG"));
    }
    Ok(snapshot(i, truth))
}

fn literal(i: &Inputs, t: &[[bool; 3]], p: &Incidence, layer: usize) -> bool {
    let l = i.revisions[p.premise.index()].logical.index();
    let h = i.heads[l];
    t[p.premise.index()][layer]
        && (p.kind != EdgeKind::Cur || p.premise == h)
        && (layer == 0
            || p.kind != EdgeKind::Pin
            || p.premise == h
            || i.logicals[l].kind == RecordKind::Evidence)
        && (layer == 0 || p.kind != EdgeKind::Sem || t[h.index()][layer])
}
fn snapshot(i: &Inputs, t: Vec<[bool; 3]>) -> DerivedSnapshot {
    let sem: Vec<_> = (0..i.rules.len())
        .map(|n| semantic(i, RuleId(n as u32)))
        .collect();
    let sat: Vec<Vec<bool>> = i
        .rules
        .iter()
        .map(|j| j.premises.iter().map(|p| literal(i, &t, p, 0)).collect())
        .collect();
    let unsat: Vec<_> = sat
        .iter()
        .map(|s| s.iter().filter(|v| !**v).count() as u32)
        .collect();
    let active: Vec<_> = i
        .rules
        .iter()
        .enumerate()
        .map(|(n, j)| j.enabled && unsat[n] == 0)
        .collect();
    let layer_active: Vec<_> = i
        .rules
        .iter()
        .enumerate()
        .map(|(n, j)| {
            [
                j.enabled
                    && sem[n] != SemState::Rejected
                    && j.premises.iter().all(|p| literal(i, &t, p, 1)),
                j.enabled
                    && matches!(sem[n], SemState::Verified | SemState::NotRequired)
                    && j.premises.iter().all(|p| literal(i, &t, p, 2)),
            ]
        })
        .collect();
    let gate = conflicted(i);
    let mut witnesses = vec![[None; 3]; t.len()];
    let mut support = vec![0; t.len()];
    for (r, base) in i.base.iter().enumerate() {
        if *base {
            witnesses[r] = [Some(Witness::Base); 3];
        }
    }
    for (n, j) in i.rules.iter().enumerate() {
        let r = j.conclusion.index();
        if active[n] {
            support[r] += 1;
        }
        for (layer, yes) in [active[n], layer_active[n][0], layer_active[n][1]]
            .into_iter()
            .enumerate()
        {
            if yes && witnesses[r][layer].is_none() {
                witnesses[r][layer] = Some(Witness::Rule(RuleId(n as u32)));
            }
        }
    }
    // A conflicted revision has no L2 witness.
    for (n, gated) in gate.iter().enumerate() {
        if *gated {
            witnesses[n][2] = None;
        }
    }
    fn reason(
        r: RevisionId,
        i: &Inputs,
        t: &[[bool; 3]],
        w: &[[Option<Witness>; 3]],
        s: &[SemState],
        seen: &mut BTreeSet<RevisionId>,
    ) -> StaleReason {
        let n = r.index();
        let l = i.revisions[n].logical.index();
        if i.heads[l] != r {
            return StaleReason::HeadSuperseded;
        }
        if !t[n][0] || !seen.insert(r) {
            return StaleReason::Unsupported;
        }
        if let Some(Witness::Rule(j)) = w[n][0] {
            if s[j.index()] == SemState::Rejected {
                return StaleReason::Rejected;
            }
            if let Some(p) = i.rules[j.index()]
                .premises
                .iter()
                .find(|p| !literal(i, t, p, 1))
            {
                let l = i.revisions[p.premise.index()].logical.index();
                let h = i.heads[l];
                if p.kind == EdgeKind::Pin
                    && p.premise != h
                    && i.logicals[l].kind != RecordKind::Evidence
                {
                    return StaleReason::SupersededPinnedPremise(p.premise);
                }
                let upstream = if p.kind == EdgeKind::Sem && h != p.premise && !t[h.index()][1] {
                    h
                } else {
                    p.premise
                };
                return reason(upstream, i, t, w, s, seen);
            }
        }
        StaleReason::Unsupported // ignored when L1 holds
    }
    let nodes =
        t.iter()
            .enumerate()
            .map(|(n, v)| {
                let semantic = if !v[0] {
                    SemState::NotApplicable
                } else {
                    witnesses[n].iter().rev().flatten().next().map_or(
                        SemState::NotApplicable,
                        |w| match w {
                            Witness::Base => SemState::NotRequired,
                            Witness::Rule(j) => sem[j.index()],
                        },
                    )
                };
                NodeState {
                    l0: v[0],
                    l1: v[1],
                    l2: v[2],
                    support: support[n],
                    witness: witnesses[n],
                    dimensions: Dimensions {
                        authorized: true,
                        conflicted: gate[n],
                        currency: if i.heads[i.revisions[n].logical.index()].index() == n {
                            Currency::Current
                        } else {
                            Currency::Superseded
                        },
                        structural: if v[0] {
                            Structural::Supported
                        } else {
                            Structural::Unsupported
                        },
                        l1: v[1],
                        l2: v[2],
                        semantic,
                        stale_reason: reason(
                            RevisionId(n as u32),
                            i,
                            &t,
                            &witnesses,
                            &sem,
                            &mut BTreeSet::new(),
                        ),
                        observation: Observation::NotApplicable,
                    },
                }
            })
            .collect();
    DerivedSnapshot {
        nodes,
        sat,
        unsat,
        active,
        layer_active,
        sem,
    }
}

pub fn summarize(d: &Dimensions) -> Summary {
    if !d.authorized {
        return Summary::Unavailable;
    }
    if d.conflicted {
        return Summary::Conflicted;
    }
    if d.currency != Currency::Current {
        return Summary::Stale(StaleReason::HeadSuperseded);
    }
    if d.structural == Structural::Unsupported {
        return Summary::Stale(StaleReason::Unsupported);
    }
    if d.structural == Structural::Supported && !d.l1 {
        return Summary::Stale(d.stale_reason.clone());
    }
    if d.observation == Observation::Lagging {
        return Summary::Pending(PendingReason::ObservationLag);
    }
    if d.structural == Structural::Unknown {
        return Summary::Pending(PendingReason::StructuralUnknown);
    }
    if !d.l2 {
        return Summary::Pending(PendingReason::Semantic);
    }
    Summary::Certified
}
