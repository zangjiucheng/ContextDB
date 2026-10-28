//! Deterministic scenarios, replayable traces and P0 property input generation.
use contextdb_model::*;
use contextdb_oracle::{InputModel, current_vector};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::BTreeMap;

#[macro_export]
macro_rules! scenario {($($batch:expr),* $(,)?)=>{contextdb_model::EventLog{batches:vec![$($batch),*]}};}

#[derive(Clone, Debug, Default)]
pub struct Coverage {
    pub seeds: u64,
    pub commits: u64,
    pub rejects: u64,
    pub ops: BTreeMap<String, u64>,
    pub errors: BTreeMap<String, u64>,
}
impl Coverage {
    pub fn record(&mut self, b: &Batch, result: &Result<ApplyResult, TxError>) {
        for op in &b.ops {
            *self.ops.entry(op.name().into()).or_default() += 1;
        }
        match result {
            Ok(_) => self.commits += 1,
            Err(e) => {
                self.rejects += 1;
                *self
                    .errors
                    .entry(
                        format!("{e:?}")
                            .split(['{', '('])
                            .next()
                            .unwrap_or("error")
                            .trim()
                            .into(),
                    )
                    .or_default() += 1;
            }
        }
    }
}
pub fn create(key: &str, kind: RecordKind, base: bool, content: u8) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind,
        scope_lane: "repo".into(),
        content: [content; 32],
        adopt_as_base: base,
    }
}

/// PROP1 bounds: 6 logicals, 14 revisions, 10 rules, 12 batches, 1–5 Ops.
/// Seeds use ChaCha8, never thread_rng. Generation is adaptive to accepted inputs.
pub fn generate(seed: u64) -> EventLog {
    generate_inner(seed, false)
}

/// Stratified PROP1 cases include the six required event families in every seed.
/// The random tail uses the same weighted Op/edge generator as P0.
pub fn generate_p1(seed: u64) -> EventLog {
    generate_inner(seed, true)
}
fn generate_inner(seed: u64, p1: bool) -> EventLog {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut model = InputModel::default();
    let mut batches = Vec::new();
    let init = Batch::new(
        (0..4)
            .map(|n| {
                create(
                    &format!("n{n}"),
                    match n {
                        0 if seed.is_multiple_of(3) => RecordKind::Evidence,
                        2 if seed.is_multiple_of(2) => RecordKind::Summary,
                        3 => RecordKind::Plan,
                        _ => RecordKind::Fact,
                    },
                    n < 2,
                    n as u8,
                )
            })
            .collect(),
    );
    model.apply(&init).expect("valid initial state");
    batches.push(init);
    if p1 {
        // Replace the initial sample with a bounded fixture: Evidence + Summary,
        // two alternatives, a backward-rank insertion, and a new Sem premise.
        model = InputModel::default();
        batches.clear();
        let r = |n| RevRef::Existing(RevisionId(n));
        let add = |c, p, kind| Op::AddRule {
            conclusion: r(c),
            incidences: vec![(r(p), kind)],
            enabled: true,
        };
        let head = |expected, new| Op::SetHead {
            logical: LogicalId(0).into(),
            expected: r(expected),
            new: r(new),
        };
        let prefix = vec![
            Batch::new(
                (0..5)
                    .map(|n| {
                        create(
                            &format!("n{n}"),
                            match n {
                                3 => RecordKind::Evidence,
                                4 => RecordKind::Summary,
                                _ => RecordKind::Fact,
                            },
                            n != 2 && n != 4,
                            n as u8,
                        )
                    })
                    .collect(),
            ),
            Batch::new(vec![
                Op::AddRevision {
                    logical: LogicalId(0).into(),
                    content: [90; 32],
                    adopt_as_base: true,
                },
                add(2, 0, EdgeKind::Cur),
                add(2, 1, EdgeKind::Pin),
                add(1, 3, EdgeKind::Pin),
                add(4, 5, EdgeKind::Sem),
            ]),
            Batch::new(vec![head(0, 5), Op::RetractBase { rev: r(0) }]),
            Batch::new(vec![head(5, 0), Op::AssertBase { rev: r(0) }]),
            Batch::new(vec![add(0, 2, EdgeKind::Pin)]),
            Batch::new(vec![head(5, 5)]),
            Batch::new(vec![
                Op::SetRuleEnabled {
                    rule: RuleId(0).into(),
                    enabled: false,
                },
                Op::RetractBase { rev: r(0) },
            ]),
        ];
        for batch in prefix {
            let _ = model.apply(&batch);
            batches.push(batch);
        }
    }
    for tx in batches.len()..12 {
        let count = 1 + (rng.next_u32() % 5) as usize;
        let mut batch = Batch::new(Vec::new());
        let mut preview = model.clone();
        for k in 0..count {
            let i = preview.inputs();
            let n = i.revisions.len();
            let pick = (rng.next_u32() as usize) % n;
            let rev = RevisionId(pick as u32);
            let logical = i.revisions[pick].logical;
            let code = rng.next_u32() % 100;
            let op = match code {
                0..=9 if i.logicals.len() < 6 && n < 14 => create(
                    &format!("x{tx}-{k}"),
                    RecordKind::Fact,
                    rng.next_u32() % 2 == 0,
                    (tx * 5 + k) as u8,
                ),
                10..=23 if n < 14 => Op::AddRevision {
                    logical: logical.into(),
                    content: if rng.next_u32() % 3 == 0 {
                        i.revisions[pick].content
                    } else {
                        [(80 + tx * 5 + k) as u8; 32]
                    },
                    adopt_as_base: rng.next_u32() % 2 == 0,
                },
                24..=39 => {
                    let revisions: Vec<_> = i
                        .revisions
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| r.logical == logical)
                        .map(|(n, _)| RevisionId(n as u32))
                        .collect();
                    let to = revisions[rng.next_u32() as usize % revisions.len()];
                    Op::SetHead {
                        logical: logical.into(),
                        expected: if rng.next_u32() % 4 == 0 {
                            rev
                        } else {
                            i.heads[logical.index()]
                        }
                        .into(),
                        new: to.into(),
                    }
                }
                40..=49 => Op::AssertBase { rev: rev.into() },
                50..=59 => Op::RetractBase { rev: rev.into() },
                60..=77 if i.rules.len() < 10 => {
                    let m = 1 + rng.next_u32() % 3;
                    let incidences = (0..m)
                        .map(|_| {
                            let p = RevisionId(rng.next_u32() % n as u32);
                            let w = rng.next_u32() % 100;
                            let kind = match w {
                                0..=39 => EdgeKind::Pin,
                                40..=64 => EdgeKind::Cur,
                                65..=84 => EdgeKind::Sem,
                                _ => EdgeKind::Ref,
                            };
                            (p.into(), kind)
                        })
                        .collect();
                    Op::AddRule {
                        conclusion: rev.into(),
                        incidences,
                        enabled: rng.next_u32() % 4 != 0,
                    }
                }
                78..=85 if !i.rules.is_empty() => Op::SetRuleEnabled {
                    rule: RuleId(rng.next_u32() % i.rules.len() as u32).into(),
                    enabled: rng.next_u32() % 2 == 0,
                },
                86..=91 if !i.rules.is_empty() && i.rules.len() < 10 => {
                    let id = RuleId(rng.next_u32() % i.rules.len() as u32);
                    // A reference-only rule has no premise: Rebind its first
                    // reference (rejected as NotPremise).
                    let rule = &i.rules[id.index()];
                    let from = rule
                        .premises
                        .first()
                        .map_or_else(|| rule.references[0], |p| p.premise);
                    let to = i
                        .revisions
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| r.logical == i.revisions[from.index()].logical)
                        .map(|(n, _)| RevisionId(n as u32))
                        .next_back()
                        .unwrap_or(from);
                    Op::Rebind {
                        rule: id.into(),
                        from: from.into(),
                        to: to.into(),
                    }
                }
                92..=97 if !i.rules.is_empty() => {
                    let id = RuleId(rng.next_u32() % i.rules.len() as u32);
                    let mut seen = current_vector(i, &i.rules[id.index()]);
                    if rng.next_u32() % 4 == 0 {
                        seen.clear();
                    }
                    Op::RecordValidation {
                        rule: id.into(),
                        curvec_seen: seen,
                        verdict: match rng.next_u32() % 3 {
                            0 => Verdict::Verified,
                            1 => Verdict::Rejected,
                            _ => Verdict::Pending,
                        },
                        validator: "generator".into(),
                    }
                }
                _ => Op::RetractBase {
                    rev: RevRef::New(u16::MAX),
                },
            };
            batch.ops.push(op);
            // Preview the complete prefix, preserving this batch's local bindings and epoch.
            let mut candidate = model.clone();
            if candidate.apply(&batch).is_ok() {
                preview = candidate;
            }
        }
        let _ = model.apply(&batch);
        batches.push(batch);
    }
    EventLog { batches }
}

pub mod shapes;
pub use shapes::{Shape, ShapeStats, Shaped, shape};
