# Development and verification

[README](../README.md) · [Getting started](GETTING_STARTED.md) · [Reference](REFERENCE.md) · **Development**

Requires Rust 1.95.0 and Python 3.11+. Install the check tools once:

```sh
cargo install cargo-deny --version 0.20.2 --locked
```

The same base checks as CI (fmt, Clippy, all tests, loom model, dependency boundaries, licenses):

```sh
bash scripts/check.sh
```

Heavier verification:

```sh
cargo test --release --locked -p contextdb-testkit --test exhaustive -- --ignored --nocapture   # finite-state exhaustive exploration
bash scripts/gate-regression.sh     # earlier phases' G2/G3/G4 at gate counts
bash scripts/mutation.sh            # cargo-mutants (also mutation-p4.sh, -p5a, -p5b, -p6)
bash scripts/fuzz.sh                # cargo-fuzz targets
bash scripts/remote.sh run <name> <cmd...>   # long jobs on the remote Linux host
```

**Process**: every phase starts with an ADR and an independent review (G0); after implementation, evidence is collected layer by layer, a reviewer who did not implement the phase looks for counterexamples, and the results go into `docs/gates/<phase>.md`. The project runs an agent-autonomous process (SPEC §4.4): labelling, independent reviews, dogfood upkeep and phase reports are done by agents without requiring human participation; labels keep their provenance, rationale and evidence hashes, and historical human judgments are optional comparisons only. Real-use duration, deterministic oracles, performance budgets and failure states are still verified item by item.
