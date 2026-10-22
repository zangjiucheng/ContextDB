# Implementation status

**P5a is implemented; its gate is open** ([P5a gate](gates/P5a.md), [ADR 0011](adr/0011-p5a-file-anchors.md) r0.3). G0–G5, G7, G8 and the independent review pass (G5 by owner decision on the ADR rule); G6 needs ≥ 200 human labels on the prepared replay pairs, review finding M2 (outbox growth) needs an ADR amendment, and P5a cannot pass before P4.

**P4 is implemented and under gate verification** ([P4 gate](gates/P4.md), [ADR 0010](adr/0010-p4-hooks-action-gating.md) rev 3.3). G6 dogfood (≥ 2 weeks, ≥ 2 parallel agents) is run by the project owner; P4 passes only with that report.

**Gate P3 passed** (2026-10-21) after independent review, a closure check and a closure confirmation ([P3 gate](gates/P3.md),
[ADR 0008](adr/0008-p3-daemon-sessions-certificates.md) rev 3.3,
[ADR 0009](adr/0009-p3-memory-scopes-after-maintained-layers.md)).

**Gate P2 passed** (2026-10-20, commit `de80aa6`) after independent review and two closure
checks, with CI run 39497969575 and nightly run 39498455621.
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
- O(1) CURRENT L0; counter-maintained L1/L2 columns (ADR 0008 §7, rebuilt independently in FullRecompute); WHY with bounded canonical proof,
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
- P3 store additions: op_log format 2 with group framing and one sync per group
  (`commit_group`), the session log, `LogReader`, `watermark_history`, FTS5 recall,
  and self-verifying blob packs with ordered syncs (ADR 0008 §17).
- Certificates: `issue_certificate` / `validate_certificate` with an independent
  oracle (PROP11).
- `ctxd` (`contextdb-daemon`): commit actor with group commit, two-replica
  snapshot publication (loom-modelled), three channel sockets with peer-uid checks,
  sessions, handles, `ctx_check`, `Busy` / 30 s `Timeout`.
- `ctx-mcp` (`contextdb-mcp`): nine agent tools over JSON-RPC 2.0 on stdio.
- `ctx status | why | why-not | impact | diff | validate | shutdown` through the
  admin socket.

## Boundaries

- No signed log head (P9).
- No erasure: blobs are never deleted before P9.
- No `inputs_at` cache.
- In-memory history is not bounded after open.
- MAIN only, one subject, positive DAG, transaction time only. No branch-isolation
  product API, ACL, negation, recursive production rules, erasure or replication.
- Hooks and action gating (deny) are P4; until then `ctx_check` allows or warns.
  CodeAnchor input/channel rules exist, but no filesystem observer before P5.
- The P3 trust boundary is cooperative: a same-user agent with a shell can reach
  the observer and admin sockets (hard separation with principals and ACL in P9).
- Conflict remains None before P7, visibility Authorized before P9; structural
  Unknown/freshness are summary-function fixtures.
- Hooks and anchors crates remain placeholders (P4, P5). Research DD/DBSP workspaces stay excluded.

## Dependency policy

`scripts/check_dependencies.py` checks all normal/build/dev/optional/target edges.
Model has no dependencies; oracle reaches only model. Core's explicit allowlist
admits BLAKE3 and build/platform helpers, not database/network/LLM crates. ChaCha8
and benchmark generation stay outside core. `cargo deny` checks licenses,
advisories, bans and sources; no GPL/AGPL-only dependency is admitted.
