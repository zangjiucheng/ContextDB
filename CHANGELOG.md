# Changelog

## Unreleased

- Adopt SPEC v1.4 as the implementation starting point; keep prior research and reviews.
- Establish the Rust 1.95.0 workspace with 13 product/verification crate boundaries and an isolated R2 baseline workspace.
- Add the bootstrap `ctx --help` / `--version` commands; database commands remain unimplemented.
- Add Linux/macOS CI, license checks, dependency policy checks, and negative tests for the architecture gate.
- Implement P0 model contracts, immutable inputs, complete Op/error types, canonical wire and versioned EventLog v0.
- Implement independent InputModel staging/validation and Jacobi/Kahn full recomputation, three-layer certification, summaries and explanations.
- Implement core SoA/CSR arenas, indexes, independent canonicalization/CAS/cycle validation, rollback, NetDelta and both diff sets, with oracle-backed queries.
- Add 26 golden groups, three wire tests, policy hash golden, compile-fail API checks and 1,000 ChaCha8 seeds / 12,000 transaction comparisons.
- Record SPEC v1.4.1 executable details in ADR 0003, semantics traceability and P0 verification/review evidence. Local checks and independent review passed; remote CI remains unexecuted.
