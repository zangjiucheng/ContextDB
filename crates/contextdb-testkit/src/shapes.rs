//! Reproducible graph shapes. Every generated operation uses the public contract.
use contextdb_model::*;
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
#[derive(Clone, Copy, Debug)]
pub enum Shape {
    Chain,
    Tree { fanout: u32 },
    Hub,
    MultiSupport { p_alt: f64 },
}
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeStats {
    pub revisions: usize,
    pub rules: usize,
    pub incidences: usize,
    pub bases: usize,
    pub alternatives: usize,
    pub max_fan_out: usize,
    pub depth: usize,
}
#[derive(Clone, Debug)]
pub struct Shaped {
    pub batches: Vec<Batch>,
    pub stats: ShapeStats,
}
pub fn shape(kind: Shape, n: usize, seed: u64) -> Shaped {
    assert!(n >= 4 && n <= u32::MAX as usize);
    if let Shape::Tree { fanout } = kind {
        assert!(fanout > 0);
    }
    if let Shape::MultiSupport { p_alt } = kind {
        assert!((0.0..=1.0).contains(&p_alt));
    }
    let bases = match kind {
        Shape::Chain | Shape::Tree { .. } => 1,
        _ => 3,
    };
    let nodes = Batch::new(
        (0..n)
            .map(|k| {
                crate::create(
                    &format!("shape/{k}"),
                    RecordKind::Fact,
                    k < bases,
                    (k % 256) as u8,
                )
            })
            .collect(),
    );
    let mut ops = Vec::new();
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut fanout = vec![0; n];
    let mut depths = vec![0; n];
    let mut incidences = 0;
    let mut alternatives = 0;
    for c in bases..n {
        let ps: Vec<usize> = match kind {
            Shape::Chain => vec![c - 1],
            Shape::Tree { fanout } => vec![(c - 1) / fanout as usize],
            Shape::Hub => vec![0, 1, 2],
            Shape::MultiSupport { .. } => {
                let start = c.saturating_sub(32);
                let mut p = std::collections::BTreeSet::new();
                while p.len() < 3 {
                    p.insert(start + rng.next_u32() as usize % (c - start));
                }
                p.into_iter().collect()
            }
        };
        depths[c] = ps.iter().map(|p| depths[*p] + 1).max().unwrap();
        for p in &ps {
            fanout[*p] += 1;
        }
        incidences += ps.len();
        ops.push(Op::AddRule {
            conclusion: RevisionId(c as u32).into(),
            incidences: ps
                .iter()
                .map(|p| (RevisionId(*p as u32).into(), EdgeKind::Pin))
                .collect(),
            enabled: true,
        });
        if let Shape::MultiSupport { p_alt } = kind
            && (rng.next_u32() as f64) / (u32::MAX as f64) < p_alt
        {
            let p = if ps.contains(&0) { 1 } else { 0 };
            if ps.len() != 1 || ps[0] != p {
                ops.push(Op::AddRule {
                    conclusion: RevisionId(c as u32).into(),
                    incidences: vec![(RevisionId(p as u32).into(), EdgeKind::Pin)],
                    enabled: true,
                });
                fanout[p] += 1;
                incidences += 1;
                alternatives += 1;
            }
        }
    }
    let stats = ShapeStats {
        revisions: n,
        rules: ops.len(),
        incidences,
        bases,
        alternatives,
        max_fan_out: *fanout.iter().max().unwrap(),
        depth: *depths.iter().max().unwrap(),
    };
    Shaped {
        batches: vec![nodes, Batch::new(ops)],
        stats,
    }
}
