//! PROP13 (ADR 0013 §11 G3): random multi-branch histories (create, commit,
//! merge both ways, archive, WHAT_IF, dedup) against the independent branch
//! oracle. After every step, every branch's canonical state (visible
//! objects, records, input values, `anchor_unobserved`, every node and rule
//! state, all in global ids) equals the oracle's; results agree, dry runs
//! promise the real results and change nothing.
use contextdb_core::Kernel;
use contextdb_model::*;
use contextdb_oracle::{BranchOracle, Canonical, MergeOutcome, canonical};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::BTreeSet;

fn kernel_canonical(k: &Kernel, b: BranchId) -> Canonical {
    let v = k.branch(b).unwrap();
    let db = v.db();
    let (gl, gr) = (
        |x: u32| v.global_logical(LogicalId(x)).0,
        |x: u32| v.global_revision(RevisionId(x)).0,
    );
    let (gj, ga) = (
        |x: u32| v.global_rule(RuleId(x)).0,
        |x: u32| v.global_artifact(ArtifactId(x)).0,
    );
    let unobserved: BTreeSet<u32> = (0..db.logical_count() as u32)
        .filter(|l| db.anchor_unobserved(LogicalId(*l)))
        .map(gl)
        .collect();
    canonical(
        &db.inputs(),
        DerivedView::derived(db),
        [&gl, &gr, &gj, &ga],
        unobserved,
    )
}
fn report(r: &MergeReport) -> MergeOutcome {
    MergeOutcome {
        moved_heads: (r.moved_heads.iter())
            .map(|(l, o, n)| (l.0, o.0, n.0))
            .collect(),
        conflicts: (r.conflicts.iter())
            .map(|c| contextdb_oracle::Conflict {
                kind: c.kind as u8,
                logical: c.logical.map(|l| l.0),
                rule: c.rule.map(|j| j.0),
                target: c.target.0,
                artifact: c.artifact.0,
            })
            .collect(),
        metadata_conflicts: (r.metadata_conflicts.iter())
            .map(|(l, f)| (l.0, *f as u8))
            .collect(),
        newly_visible: r.newly_visible,
        convergent: r.convergent,
    }
}
/// Errors compare exactly unless they may name staged objects, whose ids
/// the oracle does not reconstruct; those compare by kind.
fn same_error(a: &TxError, b: &TxError) -> bool {
    use TxError::*;
    match (a, b) {
        (
            HeadConflict { .. }
            | CycleDetected { .. }
            | PolicyViolation { .. }
            | DuplicateRule { .. }
            | NoSemIncidence { .. }
            | CurvecMoved { .. }
            | ArtifactNotOpen { .. }
            | InvalidRule(_),
            _,
        ) => std::mem::discriminant(a) == std::mem::discriminant(b),
        _ => a == b,
    }
}

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
    fn key(&mut self) -> String {
        // A small shared pool collides across branches (T75).
        if self.coin(15) {
            format!("shared{}", self.pick(4))
        } else {
            self.keys += 1;
            format!("k{}", self.keys)
        }
    }
    /// A random batch on `b`, ids picked from the oracle's view.
    fn batch(&mut self, o: &BranchOracle, b: BranchId) -> Batch {
        let c = o.canonical(b);
        let logicals: Vec<u32> = c.logicals.keys().copied().collect();
        let revisions: Vec<u32> = c.revisions.keys().copied().collect();
        let rules: Vec<u32> = c.rules.keys().copied().collect();
        let artifacts: Vec<u32> = c.artifacts.keys().copied().collect();
        let fact = |c: &Canonical, l: u32| c.logicals[&l].0.kind != RecordKind::CodeAnchor;
        let facts: Vec<u32> = logicals.iter().copied().filter(|l| fact(&c, *l)).collect();
        let fact_revs: Vec<u32> = (revisions.iter().copied())
            .filter(|r| fact(&c, c.revisions[r].0.logical.0))
            .collect();
        let mut channel = Channel::Agent;
        let mut ops = Vec::new();
        if (revisions.is_empty() && self.coin(50)) || self.coin(6) {
            // Code anchors and their watermark, on the observer channel.
            channel = Channel::Observer;
            self.keys += 1;
            ops.push(Op::CreateLogical {
                key: format!("code:f{}.rs#g", self.keys),
                kind: RecordKind::CodeAnchor,
                scope_lane: "fs".into(),
                content: [self.pick(3) as u8 + 40; 32],
                adopt_as_base: true,
            });
            let anchors: Vec<u32> = (logicals.iter().copied())
                .filter(|l| !fact(&c, *l))
                .collect();
            if !anchors.is_empty() && self.coin(50) {
                let l = anchors[self.pick(anchors.len())];
                ops.push(Op::AddRevision {
                    logical: LogicalRef::Existing(LogicalId(l)),
                    content: [self.pick(4) as u8 + 50; 32],
                    adopt_as_base: true,
                });
                ops.push(Op::SetHead {
                    logical: LogicalRef::Existing(LogicalId(l)),
                    expected: RevisionId(c.logicals[&l].1).into(),
                    new: RevRef::New(1),
                });
            }
            if !anchors.is_empty() && self.coin(40) {
                // A broken anchor: retract its base, open AnchorBroken.
                let l = anchors[self.pick(anchors.len())];
                let head = RevisionId(c.logicals[&l].1);
                ops.push(Op::RetractBase { rev: head.into() });
                ops.push(Op::OpenArtifact {
                    ty: ArtifactType::AnchorBroken,
                    target: head.into(),
                    payload_hash: [9; 32],
                });
            }
            ops.push(Op::ObserverWatermark {
                scope: "fs".into(),
                expected: c.watermarks.get("fs").cloned(),
                position: vec![self.pick(250) as u8],
            });
            let mut batch = Batch::new(ops);
            batch.branch = b;
            batch.channel = channel;
            return batch;
        }
        if self.coin(4) {
            channel = Channel::Admin;
            ops.push(Op::SetPolicy {
                policy: PolicyDoc {
                    version: 1,
                    require_deps: if self.coin(50) {
                        [RecordKind::Plan].into_iter().collect()
                    } else {
                        Default::default()
                    },
                },
            });
        }
        let rev = |g: &mut Self| revisions[g.pick(revisions.len())];
        let fact_rev = |g: &mut Self| {
            if fact_revs.is_empty() {
                revisions[0]
            } else {
                fact_revs[g.pick(fact_revs.len())]
            }
        };
        let mut creates = false;
        if fact_revs.is_empty() {
            // Agents need facts to work on.
            creates = true;
            ops.push(Op::CreateLogical {
                key: self.key(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: [self.pick(5) as u8; 32],
                adopt_as_base: true,
            });
        }
        if fact_revs.is_empty() {
            let mut batch = Batch::new(ops);
            batch.branch = b;
            batch.channel = channel;
            return batch;
        }
        for _ in 0..1 + self.pick(3) {
            let op = match self.pick(100) {
                0..=13 => {
                    creates = true;
                    Op::CreateLogical {
                        key: self.key(),
                        kind: if self.coin(10) {
                            RecordKind::Plan
                        } else {
                            RecordKind::Fact
                        },
                        scope_lane: "repo".into(),
                        content: [self.pick(5) as u8; 32],
                        adopt_as_base: self.coin(60),
                    }
                }
                14..=29 if !facts.is_empty() => {
                    creates = true;
                    let l = facts[self.pick(facts.len())];
                    ops.push(Op::AddRevision {
                        logical: LogicalRef::Existing(LogicalId(l)),
                        content: [self.pick(5) as u8 + 10; 32],
                        adopt_as_base: self.coin(50),
                    });
                    if self.coin(50) {
                        let k = (ops.iter())
                            .filter(|op| {
                                matches!(op, Op::AddRevision { .. } | Op::CreateLogical { .. })
                            })
                            .count()
                            - 1;
                        Op::SetHead {
                            logical: LogicalRef::Existing(LogicalId(l)),
                            expected: RevisionId(c.logicals[&l].1).into(),
                            new: RevRef::New(k as u16),
                        }
                    } else {
                        continue;
                    }
                }
                30..=45 if !facts.is_empty() => {
                    let l = facts[self.pick(facts.len())];
                    let of_l: Vec<u32> = (revisions.iter().copied())
                        .filter(|r| c.revisions[r].0.logical.0 == l)
                        .collect();
                    Op::SetHead {
                        logical: LogicalRef::Existing(LogicalId(l)),
                        expected: RevisionId(c.logicals[&l].1).into(),
                        new: RevisionId(of_l[self.pick(of_l.len())]).into(),
                    }
                }
                46..=51 => Op::AssertBase {
                    rev: RevisionId(fact_rev(self)).into(),
                },
                52..=55 => Op::RetractBase {
                    rev: RevisionId(fact_rev(self)).into(),
                },
                56..=71 => {
                    creates = true;
                    let kind = [EdgeKind::Pin, EdgeKind::Cur, EdgeKind::Sem][self.pick(3)];
                    let conclusion = fact_rev(self);
                    let of = |r: u32| c.revisions[&r].0.logical;
                    // Mostly a premise on another logical (else InvalidRule).
                    let others: Vec<u32> = (revisions.iter().copied())
                        .filter(|r| of(*r) != of(conclusion))
                        .collect();
                    let premise = if others.is_empty() || self.coin(5) {
                        rev(self)
                    } else {
                        others[self.pick(others.len())]
                    };
                    Op::AddRule {
                        conclusion: RevisionId(conclusion).into(),
                        incidences: vec![(RevisionId(premise).into(), kind)],
                        enabled: self.coin(80),
                    }
                }
                72..=77 if !rules.is_empty() => Op::SetRuleEnabled {
                    rule: RuleId(rules[self.pick(rules.len())]).into(),
                    enabled: self.coin(50),
                },
                78..=81 if !rules.is_empty() => {
                    // Validate a Sem rule against its current vector.
                    let j = rules[self.pick(rules.len())];
                    let mut curvec: Vec<_> = (c.rules[&j].premises.iter())
                        .filter(|p| p.kind == EdgeKind::Sem)
                        .map(|p| {
                            let l = c.revisions[&p.premise.0].0.logical.0;
                            (LogicalId(l), RevisionId(c.logicals[&l].1))
                        })
                        .collect();
                    curvec.sort_unstable();
                    Op::RecordValidation {
                        rule: RuleId(j).into(),
                        curvec_seen: curvec,
                        verdict: [Verdict::Verified, Verdict::Rejected, Verdict::Pending]
                            [self.pick(3)],
                        validator: format!("v{}", self.pick(2)),
                    }
                }
                82..=86 => Op::OpenArtifact {
                    ty: ArtifactType::Conflict,
                    target: RevisionId(fact_rev(self)).into(),
                    payload_hash: [7; 32],
                },
                87..=90 if !artifacts.is_empty() => Op::ResolveArtifact {
                    id: ArtifactId(artifacts[self.pick(artifacts.len())]).into(),
                },
                91..=94 if !facts.is_empty() => {
                    let l = facts[self.pick(facts.len())];
                    let dirs = ["src", "src/a", "src/b", "docs", "docs/x"];
                    let current = &c.logicals[&l].2.dirs;
                    let pickable: Vec<&str> = (dirs.iter().copied())
                        .filter(|d| {
                            current.is_empty()
                                || current
                                    .iter()
                                    .any(|o| d == o || d.starts_with(&format!("{o}/")))
                        })
                        .collect();
                    let mut chosen = BTreeSet::new();
                    for _ in 0..1 + self.pick(2) {
                        if !pickable.is_empty() {
                            chosen.insert(pickable[self.pick(pickable.len())].to_owned());
                        }
                    }
                    Op::SetMetadata {
                        logical: LogicalRef::Existing(LogicalId(l)),
                        tags: [format!("t{}", self.pick(3))].into(),
                        dirs: if chosen.is_empty() {
                            current.clone()
                        } else {
                            chosen
                        },
                    }
                }
                95..=97 if !facts.is_empty() => {
                    let l = facts[self.pick(facts.len())];
                    let head = c.logicals[&l].1;
                    let of_l: Vec<u32> = (revisions.iter().copied())
                        .filter(|r| c.revisions[r].0.logical.0 == l)
                        .collect();
                    let read = if self.coin(70) {
                        head
                    } else {
                        of_l[self.pick(of_l.len())]
                    };
                    Op::ExpectHeads {
                        entries: vec![(LogicalRef::Existing(LogicalId(l)), RevisionId(read))],
                        source: ReadSource::Client,
                    }
                }
                _ => Op::AssertBase {
                    rev: RevisionId(fact_rev(self)).into(),
                },
            };
            ops.push(op);
        }
        // Names of objects invisible here, but never where a staged import
        // could carry that name (ADR §3.1).
        if !creates && self.coin(5) {
            let all = o.visible(MAIN)[1].len().max(revisions.len()) as u32 + 3;
            let r = self.rng.next_u32() % all;
            if !c.revisions.contains_key(&r) {
                ops.push(Op::AssertBase {
                    rev: RevisionId(r).into(),
                });
            }
        }
        let mut batch = Batch::new(ops);
        batch.branch = b;
        batch.channel = channel;
        batch
    }
}

#[test]
fn prop13_branches_equal_the_oracle() {
    let seeds: u64 = std::env::var("CONTEXTDB_PROP13_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let (mut ok, mut merges, mut conflicts, mut imports, mut errors) = (0, 0, 0, 0, 0);
    let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
    let first: u64 = std::env::var("CONTEXTDB_PROP13_FIRST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    for seed in first..seeds {
        let mut g = Gen {
            rng: ChaCha8Rng::seed_from_u64(seed ^ 0x13),
            keys: 0,
        };
        let (mut k, mut o) = (Kernel::default(), BranchOracle::default());
        for step in 0..24 {
            let n = o.branch_count();
            let branch = BranchId(g.pick(n) as u16);
            let mut batch = match g.pick(100) {
                0..=9 if n < 4 => Batch::new(vec![Op::CreateBranch {
                    name: format!("b{step}"),
                }]),
                10..=11 => Batch::new(vec![Op::ArchiveBranch {
                    name: match (branch, g.coin(70)) {
                        (MAIN, _) => "main".into(),
                        (_, true) => o.branch_name(branch),
                        _ => format!("b{}", g.pick(24)),
                    },
                }]),
                12..=34 if n > 1 => {
                    // Mostly another branch (else InvalidMerge).
                    let mut source = BranchId(g.pick(n) as u16);
                    if source == branch && g.coin(90) {
                        source = BranchId(((branch.0 as usize + 1 + g.pick(n - 1)) % n) as u16);
                    }
                    let mut ops = vec![Op::Merge {
                        source,
                        require_certified: g.coin(40),
                        certify: Vec::new(),
                    }];
                    if g.coin(20) {
                        let c = o.canonical(branch);
                        if let Some((l, (_, head, _))) = c.logicals.iter().next() {
                            ops.insert(
                                0,
                                Op::ExpectHeads {
                                    entries: vec![(
                                        LogicalRef::Existing(LogicalId(*l)),
                                        RevisionId(*head),
                                    )],
                                    source: ReadSource::Client,
                                },
                            );
                        }
                    }
                    Batch::new(ops)
                }
                _ => g.batch(&o, branch),
            };
            batch.branch = branch;
            let at = format!("seed {seed} step {step}: {batch:?}");
            // Dry runs first: equal promises, nothing changes.
            let before: Vec<_> = (0..n)
                .map(|b| kernel_canonical(&k, BranchId(b as u16)))
                .collect();
            let kd = k.what_if_detailed(&batch);
            let od = o.apply(&batch, false);
            let after: Vec<_> = (0..n)
                .map(|b| kernel_canonical(&k, BranchId(b as u16)))
                .collect();
            assert!(before == after, "{at}: kernel dry run changed state");
            let kr = k.commit_detailed(&batch);
            let or = o.apply(&batch, true);
            for (kx, ox, what) in [(&kd, &od, "dry"), (&kr, &or, "real")] {
                match (kx, ox) {
                    (Ok((r, m)), Ok((a, om))) => {
                        assert_eq!(r.epoch, a.epoch, "{at} {what}");
                        assert_eq!(r.allocations, a.allocations, "{at} {what}");
                        assert_eq!(m.as_ref().map(report), *om, "{at} {what}");
                    }
                    (Err(e), Err(f)) => assert!(same_error(e, f), "{at} {what}: {e:?} vs {f:?}"),
                    _ => panic!("{at} {what}: kernel {kx:?} vs oracle {ox:?}"),
                }
            }
            match &kr {
                Ok((_, Some(m))) => {
                    merges += 1;
                    conflicts += m.conflicts.len();
                    ok += 1;
                }
                Ok(_) => ok += 1,
                Err(e) => {
                    errors += 1;
                    let name = format!("{e:?}");
                    let name = if name.starts_with("InvalidRule") {
                        name.split(['(']).take(2).collect::<Vec<_>>().join("(")
                    } else {
                        name.split([' ', '(', '{']).next().unwrap_or("").to_owned()
                    };
                    *kinds.entry(name).or_default() += 1;
                }
            }
            assert_eq!(k.epoch(), o.epoch(), "{at}");
            for b in 0..o.branch_count() {
                let b = BranchId(b as u16);
                let (kc, oc) = (kernel_canonical(&k, b), o.canonical(b));
                // Branch-aware reads (protocol 4) answer in global ids; only
                // `main`'s read-time BRANCH_ONLY annotation may differ.
                let view = k.branch(b).unwrap();
                for (r, node) in &oc.nodes {
                    let mut read = view.node(RevisionId(*r)).unwrap();
                    if read.dimensions.currency == Currency::BranchOnly {
                        assert_eq!(b, MAIN, "{at}");
                        read.dimensions.currency = Currency::Superseded;
                    }
                    assert_eq!(&read, node, "{at}: view of r{r} on {b:?}");
                }
                if kc != oc {
                    let diff = [
                        ("logicals", kc.logicals == oc.logicals),
                        ("revisions", kc.revisions == oc.revisions),
                        ("rules", kc.rules == oc.rules),
                        ("artifacts", kc.artifacts == oc.artifacts),
                        ("validations", kc.validations == oc.validations),
                        ("policy", kc.policy == oc.policy),
                        ("watermarks", kc.watermarks == oc.watermarks),
                        ("unobserved", kc.unobserved == oc.unobserved),
                        ("nodes", kc.nodes == oc.nodes),
                        ("rule_states", kc.rule_states == oc.rule_states),
                    ];
                    for (r, node) in &kc.nodes {
                        if oc.nodes.get(r) != Some(node) {
                            eprintln!("r{r}: kernel {node:?}\n   oracle {:?}", oc.nodes.get(r));
                        }
                    }
                    panic!("{at}: branch {b:?} differs in {diff:?}");
                }
            }
        }
        imports += (0..k.branches().count())
            .map(|b| k.branch(BranchId(b as u16)).unwrap().imports().len())
            .sum::<usize>();
    }
    eprintln!(
        "PROP13: {seeds} seeds, {ok} commits, {errors} rejections, {merges} merges, \
         {conflicts} conflicts, {imports} visibility rows; rejections {kinds:?}"
    );
    assert!(merges > seeds as usize / 2 && conflicts > 0 && imports > 0);
}
