// Random multi-branch histories for the P6 G3 properties (ADR 0013 §11),
// shared with the store's PROP10 test through `#[path]`. Batches are in
// global ids, built from the branch's view. A self-contained SplitMix64
// keeps it free of RNG dependencies.
#![allow(dead_code)]
use contextdb_core::Kernel;
use contextdb_model::*;

pub const SCOPE: &str = "repo";
pub const ANCHOR: &str = "code:repo/a.rs";

pub struct Gen {
    state: u64,
    keys: u32,
    branches: u32,
}
impl Gen {
    pub fn new(seed: u64) -> Self {
        Gen {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
            keys: 0,
            branches: 0,
        }
    }
    pub fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn pick(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    pub fn coin(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
    pub fn branch(&mut self, k: &Kernel) -> BranchId {
        BranchId(self.pick(k.branches().count()) as u16)
    }
    /// A fresh fact; keys are unique across the whole history.
    pub fn fact(&mut self) -> Op {
        self.keys += 1;
        Op::CreateLogical {
            key: format!("k{}", self.keys),
            kind: RecordKind::Fact,
            scope_lane: SCOPE.into(),
            content: [self.pick(6) as u8; 32],
            adopt_as_base: self.coin(60),
        }
    }
    pub fn create_branch(&mut self, on: BranchId) -> Batch {
        self.branches += 1;
        on_branch(
            on,
            vec![Op::CreateBranch {
                name: format!("b{}", self.branches),
            }],
        )
    }
    pub fn merge(&mut self, into: BranchId, source: BranchId) -> Batch {
        on_branch(
            into,
            vec![Op::Merge {
                source,
                require_certified: self.coin(30),
                certify: Vec::new(),
            }],
        )
    }
    /// A random agent batch on `branch`.
    pub fn batch(&mut self, k: &Kernel, branch: BranchId) -> Batch {
        let view = k.branch(branch).unwrap();
        let inputs = view.db().inputs();
        let rev = |g: &mut Self| {
            let n = inputs.revisions.len();
            view.global_revision(RevisionId(g.pick(n) as u32))
        };
        let mut ops = Vec::new();
        for _ in 0..1 + self.pick(3) {
            let code = self.pick(100);
            let op = match code {
                _ if inputs.revisions.is_empty() || code < 15 => self.fact(),
                15..=34 => {
                    let l = self.pick(inputs.logicals.len());
                    if inputs.logicals[l].kind == RecordKind::CodeAnchor {
                        self.fact()
                    } else {
                        Op::AddRevision {
                            logical: view.global_logical(LogicalId(l as u32)).into(),
                            content: [self.pick(6) as u8 + 10; 32],
                            adopt_as_base: self.coin(50),
                        }
                    }
                }
                35..=49 => {
                    let l = self.pick(inputs.logicals.len());
                    let of_l: Vec<_> = (inputs.revisions.iter().enumerate())
                        .filter(|(_, r)| r.logical.index() == l)
                        .map(|(n, _)| n as u32)
                        .collect();
                    let to = of_l[self.pick(of_l.len())];
                    Op::SetHead {
                        logical: view.global_logical(LogicalId(l as u32)).into(),
                        expected: view.global_revision(inputs.heads[l]).into(),
                        new: view.global_revision(RevisionId(to)).into(),
                    }
                }
                50..=57 => Op::AssertBase {
                    rev: rev(self).into(),
                },
                58..=64 => Op::RetractBase {
                    rev: rev(self).into(),
                },
                65..=82 => {
                    let kind = [EdgeKind::Pin, EdgeKind::Cur, EdgeKind::Sem][self.pick(3)];
                    Op::AddRule {
                        conclusion: rev(self).into(),
                        incidences: vec![(rev(self).into(), kind)],
                        enabled: self.coin(80),
                    }
                }
                83..=89 if !inputs.rules.is_empty() => Op::SetRuleEnabled {
                    rule: view
                        .global_rule(RuleId(self.pick(inputs.rules.len()) as u32))
                        .into(),
                    enabled: self.coin(50),
                },
                90..=94 => Op::OpenArtifact {
                    ty: ArtifactType::Conflict,
                    target: rev(self).into(),
                    payload_hash: [7; 32],
                },
                _ if !inputs.artifacts.is_empty() => Op::ResolveArtifact {
                    id: view
                        .global_artifact(ArtifactId(self.pick(inputs.artifacts.len()) as u32))
                        .into(),
                },
                _ => Op::AssertBase {
                    rev: rev(self).into(),
                },
            };
            ops.push(op);
        }
        on_branch(branch, ops)
    }
    /// Observer batches on `branch`: the CodeAnchor (create, new content and
    /// head moves) and watermark advances, so lag and strict changes occur.
    pub fn observer(&mut self, k: &Kernel, branch: BranchId) -> Batch {
        let view = k.branch(branch).unwrap();
        let ops = match (view.logical_by_key(ANCHOR), self.pick(3)) {
            (None, _) => vec![Op::CreateLogical {
                key: ANCHOR.into(),
                kind: RecordKind::CodeAnchor,
                scope_lane: SCOPE.into(),
                content: [200; 32],
                adopt_as_base: true,
            }],
            (Some(l), 0) => vec![
                Op::AddRevision {
                    logical: l.into(),
                    content: [200 + self.pick(3) as u8; 32],
                    adopt_as_base: true,
                },
                Op::SetHead {
                    logical: l.into(),
                    expected: view.head(l).unwrap().into(),
                    new: RevRef::New(0),
                },
            ],
            (Some(_), _) => vec![Op::ObserverWatermark {
                scope: SCOPE.into(),
                expected: view.watermark(SCOPE).map(<[u8]>::to_vec),
                position: vec![self.pick(4) as u8],
            }],
        };
        let mut b = on_branch(branch, ops);
        b.channel = Channel::Observer;
        b
    }
    /// One random step of a history: fork, merge, observer or agent batch.
    pub fn step(&mut self, k: &Kernel) -> Batch {
        let n = k.branches().count();
        let branch = self.branch(k);
        match self.pick(100) {
            0..=7 if n < 5 => self.create_branch(branch),
            8..=22 if n > 1 => {
                let source = self.branch(k);
                self.merge(branch, source)
            }
            23..=32 => self.observer(k, branch),
            _ => self.batch(k, branch),
        }
    }
}
pub fn on_branch(branch: BranchId, ops: Vec<Op>) -> Batch {
    let mut b = Batch::new(ops);
    b.branch = branch;
    b
}
