# Implementation status

P1 functionality is implemented; **Gate P1 is not complete**. Local semantic and
latency checks pass, but the whole-database memory lower bound is 292.53–314.32
B/incidence against the unchanged 64-byte target. P1 Linux/macOS CI remains pending.
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
  Structural changes use staged input validation and rebuilt indexes.
- O(1) CURRENT L0; independent memoized L1/L2; WHY with bounded canonical proof,
  WHY_NOT; Reachable and undo-restored Exact IMPACT; endpoint-folded in-memory DIFF.
- Debug/paranoid DA1–DA6 checks, release-paranoid oracle cross-checks, separate timing
  feature, conservative memory categories, deterministic shape generators and stats.
- Both exact modes: 52 P0 and 24 P1 golden/regression tests. 10,000 fixed ChaCha8
  seeds / 120,000 transactions, PROP7 metamorphic checks, depth-6/8 finite-state
  exploration and counting/transaction mutation audit. See report for exact results.
- Rust 1.95.0, Apache-2.0, architecture/license gates, standard Linux/macOS CI and
  scheduled/manual P1 exhaustive/mutation workflow (new workflow not yet run remotely).

## Boundaries

- P1 Gate remains open. No P2 persistence/acknowledgement/recovery work has begun.
  client_txn_id is carried but not deduplicated. Content hashes have no blob store.
- L1/L2 use memoized evaluation; full incremental layer maintenance is P3.
  Structural insertion is not covered by mutable-commit latency measurements.
- MAIN only, one subject, positive DAG, transaction time only. No branch-isolation
  product API, ACL, negation, recursive production rules, erasure or replication.
- Certificate closure issuance, daemon/MCP/hooks and transport binding are P3+.
  CodeAnchor input/channel rules exist, but no filesystem observer before P5.
- Conflict remains None before P7, visibility Authorized before P9; structural
  Unknown/freshness are summary-function fixtures. No durable history retention.
- `ctx` exposes help/version; operational commands and other product crates remain
  placeholders. Research DD/DBSP workspaces stay excluded.

## Dependency policy

`scripts/check_dependencies.py` checks all normal/build/dev/optional/target edges.
Model has no dependencies; oracle reaches only model. Core's explicit allowlist
admits BLAKE3 and build/platform helpers, not database/network/LLM crates. ChaCha8
and benchmark generation stay outside core. `cargo deny` checks licenses,
advisories, bans and sources; no GPL/AGPL-only dependency is admitted.
