//! Performance measurements and benchmark reporting.
//!
//! The binaries live in `src/bin`; this library holds what the P11
//! harness binaries (`p11_ablation`, `p11_g5`) share.

pub mod p11 {
    use contextdb_model::*;
    use contextdb_testkit::{Shape, Shaped};
    use std::time::Instant;

    /// `multi_support`, `hub` or `chain`, as `contextdb-bench` names them.
    pub fn parse_shape(name: &str) -> Option<Shape> {
        match name {
            "multi_support" => Some(Shape::MultiSupport { p_alt: 0.3 }),
            "hub" => Some(Shape::Hub),
            "chain" => Some(Shape::Chain),
            _ => None,
        }
    }
    /// Asserts (`true`) or retracts revision 0's base: the cascade the
    /// harness measures (as `p11_cascade`).
    pub fn toggle(on: bool) -> Batch {
        let rev = RevisionId(0).into();
        Batch::new(vec![if on {
            Op::AssertBase { rev }
        } else {
            Op::RetractBase { rev }
        }])
    }
    /// The L0 flips retracting revision 0's base causes, computed from the
    /// generated batches alone (not by the code under test): every rule's
    /// premises are lower-numbered than its conclusion, so one pass in
    /// conclusion order is the fixpoint.
    pub fn expected_retract_flips(shaped: &Shaped) -> usize {
        let support = |retracted: bool| {
            let mut l0 = Vec::new();
            for op in &shaped.batches[0].ops {
                if let Op::CreateLogical { adopt_as_base, .. } = op {
                    l0.push(*adopt_as_base);
                }
            }
            if retracted {
                l0[0] = false;
            }
            for op in shaped.batches[1..].iter().flat_map(|b| &b.ops) {
                if let Op::AddRule {
                    conclusion: RevRef::Existing(c),
                    incidences,
                    enabled: true,
                } = op
                {
                    let held = incidences.iter().all(|(p, _)| match p {
                        RevRef::Existing(p) => l0[p.index()],
                        RevRef::New(_) => false,
                    });
                    l0[c.index()] |= held;
                }
            }
            l0
        };
        let (on, off) = (support(false), support(true));
        on.iter().zip(&off).filter(|(a, b)| a != b).count()
    }
    pub fn ms(t: Instant) -> f64 {
        t.elapsed().as_secs_f64() * 1e3
    }
    pub fn median(values: &mut [f64]) -> f64 {
        values.sort_by(f64::total_cmp);
        values.get(values.len() / 2).copied().unwrap_or(f64::NAN)
    }
    /// The nearest-rank p99, as `contextdb-bench`'s P1 numbers.
    pub fn p99(values: &mut [u128]) -> u128 {
        values.sort_unstable();
        values[(values.len() * 99).div_ceil(100).saturating_sub(1)]
    }
    /// Counts every failed operation and failed work check of a run; each
    /// is described on stderr, and the run exits non-zero if any occurred.
    #[derive(Default)]
    pub struct Errors(pub u64);
    impl Errors {
        pub fn ok<T, E: std::fmt::Debug>(&mut self, what: &str, r: Result<T, E>) -> Option<T> {
            r.map_err(|e| self.fail(what, format_args!("{e:?}"))).ok()
        }
        /// Counts an error unless `held`.
        pub fn check(&mut self, what: &str, held: bool, detail: std::fmt::Arguments<'_>) {
            if !held {
                self.fail(what, detail);
            }
        }
        pub fn fail(&mut self, what: &str, detail: std::fmt::Arguments<'_>) {
            self.0 += 1;
            eprintln!("error: {what}: {detail}");
        }
        /// The value, or, for a step the run cannot go on without, the
        /// error counted and `line errors=N` printed before exiting 1.
        pub fn need<T, E: std::fmt::Debug>(
            &mut self,
            line: &str,
            what: &str,
            r: Result<T, E>,
        ) -> T {
            r.unwrap_or_else(|e| {
                self.fail(what, format_args!("{e:?}"));
                println!("{line} errors={}", self.0);
                std::process::exit(1)
            })
        }
        /// Prints `line errors=N`, then exits 1 if any error occurred.
        pub fn finish(&self, line: &str) {
            println!("{line} errors={}", self.0);
            if self.0 > 0 {
                std::process::exit(1);
            }
        }
    }
}
