//! Random multi-branch histories (forks, commits, merges both ways, dry
//! runs). Invariants after every step: each branch's derived state equals
//! the oracle on its inputs; a commit or merge changes no other branch; a
//! merge makes the source's objects visible in the target; a dry run
//! promises the real result and changes nothing. (The independent oracle
//! branch model, PROP13, is a later milestone.)
use contextdb_core::{Import, Kernel};
use contextdb_model::*;
use contextdb_oracle::jacobi;
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};

struct Gen {
    rng: ChaCha8Rng,
    keys: u32,
}
impl Gen {
    fn pick(&mut self, n: usize) -> usize {
        self.rng.next_u32() as usize % n.max(1)
    }
    fn coin(&mut self, percent: u32) -> bool {
        self.rng.next_u32() % 100 < percent
    }
    /// A random batch for `branch`, in global ids from its view.
    fn batch(&mut self, k: &Kernel, branch: BranchId) -> Batch {
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
                _ if inputs.revisions.is_empty() || code < 15 => {
                    self.keys += 1;
                    Op::CreateLogical {
                        key: format!("k{}", self.keys),
                        kind: RecordKind::Fact,
                        scope_lane: "repo".into(),
                        content: [self.pick(6) as u8; 32],
                        adopt_as_base: self.coin(60),
                    }
                }
                15..=34 => {
                    let l = self.pick(inputs.logicals.len());
                    Op::AddRevision {
                        logical: LogicalRef::Existing(view.global_logical(LogicalId(l as u32))),
                        content: [self.pick(6) as u8 + 10; 32],
                        adopt_as_base: self.coin(50),
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
                        logical: LogicalRef::Existing(view.global_logical(LogicalId(l as u32))),
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
        let mut b = Batch::new(ops);
        b.branch = branch;
        b
    }
}

fn digests(k: &Kernel) -> Vec<ContentHash> {
    k.branches()
        .map(|b| k.branch(b.id).unwrap().digest())
        .collect()
}

#[test]
fn random_branch_histories() {
    let (mut merges, mut conflicts, mut imports) = (0, 0, 0);
    for seed in 0..300 {
        let mut g = Gen {
            rng: ChaCha8Rng::seed_from_u64(seed),
            keys: 0,
        };
        let mut k = Kernel::default();
        for step in 0..60 {
            let n = k.branches().count();
            let branch = BranchId(g.pick(n) as u16);
            let batch = match g.pick(100) {
                0..=7 if n < 5 => {
                    let mut b = Batch::new(vec![Op::CreateBranch {
                        name: format!("b{step}"),
                    }]);
                    b.branch = branch;
                    b
                }
                8..=27 if n > 1 => {
                    let source = BranchId(g.pick(n) as u16);
                    let mut b = Batch::new(vec![Op::Merge {
                        source,
                        require_certified: g.coin(30),
                        certify: Vec::new(),
                    }]);
                    b.branch = branch;
                    b
                }
                _ => g.batch(&k, branch),
            };
            let before = digests(&k);
            let epoch = k.epoch();
            let dry = k.what_if_detailed(&batch);
            assert_eq!(
                digests(&k),
                before,
                "seed {seed} step {step}: dry run changed state"
            );
            assert_eq!(k.epoch(), epoch);
            let source = match batch.ops[0] {
                Op::Merge { source, .. } => Some(source),
                _ => None,
            };
            let real = k.commit_detailed(&batch);
            assert_eq!(
                real.as_ref().map(|r| (&r.0.allocations, &r.1)),
                dry.as_ref().map(|r| (&r.0.allocations, &r.1)),
                "seed {seed} step {step}: {batch:?}"
            );
            let after = digests(&k);
            for (n, (old, new)) in before.iter().zip(&after).enumerate() {
                if n != branch.0 as usize {
                    assert_eq!(old, new, "seed {seed} step {step}: branch {n} changed");
                }
            }
            if let (Ok((_, Some(report))), Some(source)) = (&real, source) {
                merges += 1;
                conflicts += report.conflicts.len();
                let (s, t) = (k.branch(source).unwrap(), k.branch(branch).unwrap());
                let inputs = s.db().inputs();
                for n in 0..inputs.revisions.len() {
                    let g = s.global_revision(RevisionId(n as u32));
                    assert!(
                        t.is_visible_revision(g),
                        "seed {seed} step {step}: r{g:?} not merged"
                    );
                }
            }
            for info in k.branches() {
                let view = k.branch(info.id).unwrap();
                let inputs = view.db().inputs();
                assert_eq!(
                    contextdb_model::DerivedView::derived(view.db()),
                    &jacobi(&inputs),
                    "seed {seed} step {step}: branch {:?}",
                    info.id
                );
            }
        }
        // A snapshot round trip rebuilds every branch, and both copies
        // continue identically.
        let mut copy = Kernel::import(k.export()).unwrap();
        assert_eq!(digests(&copy), digests(&k), "seed {seed}: import");
        assert_eq!(copy.epoch(), k.epoch());
        for info in k.branches() {
            let (a, b) = (k.branch(info.id).unwrap(), copy.branch(info.id).unwrap());
            assert_eq!(a.info(), b.info());
            assert_eq!(a.imports(), b.imports());
        }
        for step in 0..5 {
            let n = k.branches().count();
            let branch = BranchId(g.pick(n) as u16);
            let batch = g.batch(&k, branch);
            assert_eq!(
                copy.commit_detailed(&batch),
                k.commit_detailed(&batch),
                "seed {seed} +{step}"
            );
        }
        assert_eq!(digests(&copy), digests(&k));
        imports += k
            .branches()
            .map(|b| {
                (k.branch(b.id).unwrap().imports().iter())
                    .filter(|(i, _)| matches!(i, Import::Revision(_)))
                    .count()
            })
            .sum::<usize>();
    }
    eprintln!("merges {merges}, conflicts {conflicts}, revision rows {imports}");
    assert!(
        merges > 500 && conflicts > 30,
        "merges {merges}, conflicts {conflicts}"
    );
}
