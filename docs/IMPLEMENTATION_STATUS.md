# Implementation status

P0 implementation and local verification are complete. Independent adversarial
review passed with no blocking findings. Remote Linux/macOS CI has not run; the
repository has no configured remote. The formal CI requirement is therefore still
unverified, and P1 has not started. See [P0-report.md](gates/P0-report.md).

## Available now

- Dependency-free model: IDs, references, all 14 Ops, Batch, errors, input/derived
  snapshots, status dimensions, bounded wire decoding and EventLog v0.
- Oracle depends only on model: independent transactional InputModel, Jacobi least
  fixed point (also raw cyclic fixtures), Kahn DAG evaluator, summarize, WHY / WHY_NOT
  and the epoch-independent P0 required-entry certificate function.
- Core depends on model/oracle/BLAKE3: independent input validation, SoA append-only
  revision/rule arenas, CSR premises, support/provenance indexes, NaiveTopo,
  staged mutable tables, enabled-column undo and arena truncation on reject.
- `Db::commit`, input export, DerivedView, status/explanations, deterministic full
  and derived digests, allocations, NetDelta, l0_flips and status_diff.
- Testkit scenario macro, trace export, ChaCha8 bounded generation and coverage
  counters. 26 grouped golden tests, 3 wire tests, one policy hash test, 3 compile-fail
  checks and a 1,000-seed / 12,000-transaction differential property test pass.
- Rust 1.95.0, Apache-2.0, locked dependencies, common Linux/macOS CI checks and
  architecture/license gates. Seven Python architecture negative tests pass.

## Boundaries

- After each successful commit, the oracle fully rebuilds the cached DerivedSnapshot;
  queries read that snapshot. No incremental counting or PK (P1).
- MAIN only, single subject, positive DAG, transaction time only. Inputs reserve
  branch/principal fields; no branch isolation product API (P6), ACL (P9), negation,
  recursive production rules (P11), erasure or replication.
- No durable storage/acknowledgement/recovery (P2). client_txn_id is carried but not
  deduplicated until P2. Content hashes are accepted without a blob store until P2.
- Full certificate closure issuance, strict history, daemon/MCP/hooks and trusted
  transport binding are P3+. P0 cert_verdict checks explicit required entries only.
- CodeAnchor input/channel semantics exist; no filesystem observer before P5.
  Conflict is always None before P7; visibility always Authorized before P9;
  structural Unknown/freshness are synthetic summary-function fixtures only.
- `ctx` exposes help/version. Operational CLI commands still exit 2. Other product
  crates remain explicit placeholders; research DD/DBSP workspaces stay excluded.
- No performance, mutation, bounded-exhaustive, crash or concurrency gate is claimed
  for P0. P1's coverage thresholds/10,000 seeds remain future work.

## Dependency policy

`scripts/check_dependencies.py` checks the full all-features graph, including
normal/build/dev/optional/target edges. Model has no dependencies; oracle reaches
only model. Core's explicit transitive allowlist admits BLAKE3 and its platform/build
helpers, not database, network or LLM crates. ChaCha8 stays outside core.

`cargo deny` checks permissive licenses, advisories, bans and sources, including
private/build/dev dependencies. No GPL/AGPL-only dependency is admitted.
