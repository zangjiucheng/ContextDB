# Implementation status

P2 durability and recovery are implemented, and every local P2 gate layer passes.
Independent review, remote CI and the nightly run are pending.
See [P2 gate](gates/P2.md), [P2 report](gates/P2-report.md),
[durable-log ADR 0006](adr/0006-p2-durable-log.md) and
[incremental structural commits ADR 0007](adr/0007-incremental-structural-commits.md).

P1 functionality is implemented, and every local P1 gate layer passes. After
storage compaction, memory is 181.15 / 188.87 B/incidence of live heap. It is within
the per-scope budgets of ADR 0005, which the owner chose to calibrate per scope.
**Gate P1 passed** (CI run 39436790664, nightly run 39436822063).
See [P1 report](gates/P1-report.md), [gate matrix](gates/P1.md) and
[memory ADR](adr/0005-p1-memory-budget.md).

P0 Gate passed, including independent review and remote Linux/macOS
[CI run 39364257458](https://github.com/zangjiucheng/ContextDB/actions/runs/39364257458)
for implementation commit `73789ef96efc3d385cefe78ae7c3b6aa1ec4980d`.
That run does not validate the current P1 changes.

## Available now

- Dependency-free model: IDs, all 14 Ops, bounded wire decoding, EventLog v0,
  input/derived snapshots and status dimensions.
- Independent oracle: transactional InputModel, Jacobi least fixed point, Kahn DAG
  evaluator, WHY / WHY_NOT, summarize and P0 required-entry certificate predicate.
- `Db<M, S>` with Counting / FullRecompute and CommitStats / zero-sized NoStats.
  Counting maintains L0 support, sat/unsat, enabled rules and canonical witnesses;
  Pearce–Kelly maintains the revision/Sem-virtual DAG with NaiveTopo reference.
- Atomic mutable commits with undo, final-net-delta seeding, local status propagation,
  retained snapshot patching, allocations, exact flips/diffs and deterministic digests.
  Structural changes validate against a staging overlay and publish incrementally
  (appendable CSR adjacency, key table, PK undo journal; ADR 0007).
- O(1) CURRENT L0; independent memoized L1/L2; WHY with bounded canonical proof,
  WHY_NOT; Reachable and undo-restored Exact IMPACT; endpoint-folded in-memory DIFF.
- Debug/paranoid DA1–DA6 checks, release-paranoid oracle cross-checks, separate timing
  feature, conservative memory categories, deterministic shape generators and stats.
- Both exact modes: 52 P0 and 24 P1 golden/regression tests. 10,000 fixed ChaCha8
  seeds / 120,000 transactions, PROP7 metamorphic checks, depth-6/8 finite-state
  exploration and counting/transaction mutation audit. See report for exact results.
- Rust 1.95.0, Apache-2.0, architecture/license gates, standard Linux/macOS CI and
  a nightly workflow: exhaustive depth 6/8, four-shard mutation audit, P2 G4/PROP10 at gate counts.

- `contextdb-store`:
  - own segmented op_log behind `Durability` (StdDisk, and SimDisk for crash injection);
  - blake3 blobs;
  - idempotent `client_txn_id` with `txn_status`;
  - snapshots and replay with a sampled oracle self-check;
  - SQLite projections with a watermark;
  - Historical / Snapshot / DIFF over persisted data;
  - format-0 → 1 migration;
  - doctor truncation.
- `ctx replay | doctor | snapshot | migrate | export`.

## Boundaries

- P2 closes on independent review plus remote CI/nightly.
- No signed log head (P9).
- No erasure: blobs are never deleted before P9.
- No `inputs_at` cache.
- In-memory history is not bounded after open.
- L1/L2 use memoized evaluation; full incremental layer maintenance is P3.
- MAIN only, one subject, positive DAG, transaction time only. No branch-isolation
  product API, ACL, negation, recursive production rules, erasure or replication.
- Certificate closure issuance, daemon/MCP/hooks and transport binding are P3+.
  CodeAnchor input/channel rules exist, but no filesystem observer before P5.
- Conflict remains None before P7, visibility Authorized before P9; structural
  Unknown/freshness are summary-function fixtures.
- `ctx` has operator commands only; agent-facing commands arrive with the daemon
  in P3. Other product crates remain placeholders. Research DD/DBSP workspaces stay excluded.

## Dependency policy

`scripts/check_dependencies.py` checks all normal/build/dev/optional/target edges.
Model has no dependencies; oracle reaches only model. Core's explicit allowlist
admits BLAKE3 and build/platform helpers, not database/network/LLM crates. ChaCha8
and benchmark generation stay outside core. `cargo deny` checks licenses,
advisories, bans and sources; no GPL/AGPL-only dependency is admitted.
