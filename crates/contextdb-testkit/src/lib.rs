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
    for tx in 1..12 {
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
                    let from = i.rules[id.index()].premises[0].premise;
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
