# Implementation status

## Current phase

P0 is in progress. Only the workspace and development checks exist. **No P0 gate
is complete**, and there is no functioning database, oracle, daemon, or MCP server.
The `ctx` binary exposes help/version metadata; operational commands exit with code 2.

## Established

- Rust 1.95.0, edition 2024, Apache-2.0, committed lockfiles.
- 13 workspace members; default members are model, core, oracle, and testkit.
- `contextdb-core` currently depends on model and oracle, consistent with the P0
  oracle-backed design. Oracle depends only on model; model has no dependencies.
- Later product crates contain documentation only. They have no placeholder
  success-returning database API. Executables `ctxd`, `ctx-mcp`, `ctx-hook`, and
  `cdb-bench` will be added with their actual phase implementations.
- `crates/contextdb-dd` is an independent workspace without baseline dependencies.
  The archived DD/DBSP experiments are excluded from product Cargo commands.
- `scripts/check.sh` runs the common local/CI checks. The Python negative tests
  verify the architecture gate; they are not T-matrix or PROP tests.

## Remaining P0 work

1. Freeze the executable model contract: IDs, references, complete Ops/errors,
   input state, snapshots, status dimensions, and versioned EventLog.
2. Implement independent InputModel staging/rollback and Jacobi/Kahn evaluators.
3. Implement the core front-end with its own validation/staging and oracle-backed queries.
4. Add golden scenarios, generators, coverage counters, and the specified 1,000-seed suites.
5. Finish semantics documentation, align remaining error-catalog details, and complete
   the independent review and report required by `docs/gates/P0.md`.

Before step 1, reconcile the small remaining catalog inconsistencies noted in
`SPEC_REVIEW_v1.4.md`; no missing behavior has been silently implemented.

## Dependency policy

`scripts/check_dependencies.py` uses full `cargo metadata --all-features` and checks
resolved normal/build/dev/target dependency edges, plus declared optional dependencies
for model and oracle. It enforces SPEC §3.2 across the transitive graph.

Core uses an explicit allowlist in `config/dependency-policy.toml`; no external
dependency is approved yet. Add reviewed low-level crates and their transitive
dependencies there when needed. Database, network, and LLM dependencies belong in
outer crates and must not be admitted to the core allowlist.

`deny.toml` explicitly allows permissive licenses and checks private, build, and dev
dependencies as well. GPL/AGPL-only dependencies are not admitted. Configuration
follows the [cargo-deny license documentation](https://embarkstudios.github.io/cargo-deny/checks/licenses/cfg.html).
