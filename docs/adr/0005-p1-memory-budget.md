# ADR 0005 — P1 memory budget scope

Status: **Accepted**, 2026-10-20. The project owner chose "compact first, then
calibrate a budget per scope". This replaces the earlier proposed version of the ADR.

## Problem

SPEC P1 G5 (initial) required the whole in-memory database to use ≤ 64 B/incidence,
excluding payload, at 100K revisions and about 300K incidences on `multi_support(0.3)`
and `hub`. The first P1 build measured **lower bounds** of 292.53 and 314.32 B/inc.
More than half of that was storage overhead, not data. SPEC §4 lets an initial budget
be calibrated by ADR, but never loosened just to make a gate pass.

## Step 1: compaction (no semantic change)

| Change | Effect (multi_support, lower bound) |
|---|---|
| Pearce–Kelly: dense u32 handles and direct revision/logical columns replace `BTreeMap<CycleNode, usize>`, `usize` ranks and the `sem_logicals` set. The retained duplicate `NaiveTopo` order is dropped; `Db::topo()` derives it on demand. | topology 16.70 → 10.35 MB |
| Indexes: `rule_offset`/`rule_len` (two usize columns) become one u32 `rule_start`. The duplicate `inc_premise`/`inc_kind` columns are removed and resolved through `InputView::premise_at`. | indexes 12.37 → 7.40 MB |
| RuleArena: u32 CSR for premises (bounded by the u32 incidence capacity) and u64 offsets for references, replacing usize pairs plus `Vec<Vec>`. | inputs ↓ |
| LogicalArena: keys in one buffer (u64 offsets), scope lanes interned through a map, metadata stored sparsely. Previously each logical held two `String`s and two `BTreeSet`s. | inputs ↓ |
| Structural publication reserves exact capacity. That path already copies its inputs, so its asymptotic cost is unchanged. | inputs/indexes ↓ |
| History keeps only post-states. A change's pre-state is the previous post-state, and an invariant assertion checks this on every record. A fresh buffer is allocated so an in-place collect cannot keep the old allocation alive. | history 20.79 → 10.57 MB |

## Step 2: a measurement that can prove a pass

- **Live heap.** The bench counts live heap through an allocation-counting global allocator (`stats_alloc`, used only in the bench and in the testkit). It sees every container, B-tree node and cache, but not allocator-internal metadata.
- **Accounting test.** `crates/contextdb-testkit/tests/memory_accounting.rs` does three things:
  - Calibrates the allocator: a grow, in-place collect and drop sequence must leave a residual of 0.
  - Asserts that the category accounting never exceeds the bytes the allocator actually frees when the Db is dropped. This covers both shapes, plus a scenario with sparse metadata, interned lanes and B-trees.
  - Asserts that the untracked remainder is ≤ 1 % (shapes) and ≤ 2 % (metadata).
- **Mutation coverage.** This test is in the mutation audit, so the `retained_bytes` functions are no longer waived.
- **Per-scope bounds.**
  - Each scope's upper bound is its lower bound plus all untracked bytes (live heap − Σ lower bounds). A scope passes only if that upper bound fits its budget.
  - If Σ lower bounds > live heap, the bench reports INVALID.
  - The bench exits non-zero on any FAIL, UNVERIFIED or INVALID.
- **Untracked bytes in the bench.** 65,788 B, which is the probe sample arrays.
- **Mutation audit.** `counting.rs`, `txn.rs`, `arena.rs` and `memory.rs`: 394 mutants, 361 caught, 33 unviable, 0 survivors. The fixed category values in the accounting test pin the accounting arithmetic and the exact-reserve publication path.

## Decision: per-scope budgets

Each budget is ⌈1.10 × the larger measured upper bound of the two shapes⌉.
Baseline: M1 Pro, Rust 1.95.0, release build, seed 42; values marked initial in the SPEC.

| Scope | Contents | Budget | multi_support (upper) | hub (upper) |
|---|---|---:|---:|---:|
| hot | support indexes + counting state + PK topology | ≤ 72 B/inc | 62.61 | 64.80 |
| inputs | revision/rule/logical columns, heads, base, artifacts, validations. No payload bodies; includes a 32 B content hash per revision. | ≤ 50 B/inc | 43.99 | 45.18 |
| query_cache | layer memo + materialized full `DerivedSnapshot` | ≤ 49 B/inc | 42.91 | 44.09 |
| history | in-memory epoch post-states | ≤ 58 B per recorded status change | 52.65 | 52.65 |
| total live heap | all of the above | ≤ Σ scope budgets | 181.15 B/inc | 188.87 B/inc |

Before and after on comparable metrics:
- Before: 292.53 / 314.32 B/inc. This is a **lower bound**, so the true value was higher.
- After: 181.15 / 188.87 B/inc of **live heap**, which includes everything. The direction of the comparison is therefore conservative.

Process RSS deltas are 620.47 and 630.64 B/inc. RSS also includes allocator retention and the pages freed after bulk staging (structural commits copy their inputs). RSS is reported for information but is not gated.

**Owner acknowledgement required by this ADR.** Together the budgets allow about 207–211 B/inc (measured: 181–189). That is roughly 2.8–3× the original 64 B whole-database figure, which is no longer a budget. Its closest descendant is the hot scope, 72 B/inc. The rest is explained by:
- the irreducible 32 B content hash per revision in inputs;
- `DerivedSnapshot`, a frozen P0 model type with a per-rule `Vec<bool>`;
- history, which grows with epochs rather than incidences.

Exceeding any of these numbers later is a gate failure. A budget is never recalibrated silently.

## Consequences

- P1 G5 is judged by `contextdb-bench` per scope and on total live heap. Both shapes must exit 0 with `memory_scopes_pass=true` and `live_heap_within_total_budget=true`.
- In P2, history retention moves into the persistent `diff_log`. ADR 0006 decides whether the in-memory history is bounded.
- Candidates for further compaction, not required now: a compact pooled PK adjacency, a packed `NodeState` in history, and a bitset for `DerivedSnapshot.sat` (which needs a model change).
