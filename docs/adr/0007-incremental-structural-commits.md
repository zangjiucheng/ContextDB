# ADR 0007 — Incremental structural commits

Status: **implemented**, 2026-10-20 (results below). No SPEC semantics change. Every P0
and P1 suite is the regression gate.

## Problem

Structural commits (any batch with an Op other than AssertBase, RetractBase,
SetHead or SetRuleEnabled) are O(N) in total database size. Each one:
- exports and clones `Inputs` twice;
- rebuilds the per-transaction key, revision and rule maps;
- clones Pearce–Kelly;
- rebuilds every index;
- recomputes every layer-memo entry and compares every node;
- scans every revision for A5.

The P1 report recorded this ("structural transactions still stage/rebuild input
indexes and query caches"); P1 G5 measured only mutable commits.

Measured in P2:
- 20,000 single-create commits take 44 s to replay (2.2 ms per commit at an average of 10K revisions).
- At this cost, P2 G5 (cold-start replay of 100K transactions in ≤ 5 s) cannot be met.
- P3 G5 (MCP write p95 ≤ 15 ms) is at risk at 100K revisions.

## Decision

Structural commits stage against an **overlay** and publish incrementally. The
counting maintainer is already incremental given consistent indexes and a net delta.

1. **Overlay staging.**
   - `txn` validation runs against a `Stage`: the published database plus appended logicals, revisions, rules and artifacts, and override maps for heads, base, enabled, validations, metadata, artifacts, policy and watermarks.
   - Rejection discards the overlay. Published state is never touched before every Op and A5 have passed.
   - Error order and messages are unchanged (P0 error-equivalence suite).
2. **Persistent lookup indexes**, replacing the per-transaction `Context` rebuild:
   - logical key → id;
   - (logical, content) → revision;
   - canonical rule key → rule.

   These are open-addressing tables of u32 ids keyed by hash. Probes compare against the arenas, so no keys are duplicated. The running incidence count is kept as well.
3. **Appendable adjacency.**
   - Each reverse index is a CSR base plus per-row overflow vectors for rows touched since the last compaction.
   - Rows are read as base followed by overflow, which preserves ascending rule/incidence order and therefore canonical witnesses.
   - When the overflow exceeds a quarter of the base, a compaction rebuilds the CSR, so the cost is amortized O(1) per entry.
   - A `revisions_by_logical` index replaces the O(R) scan when a logical first gains a Sem incidence.
4. **Pearce–Kelly with an undo journal.** Edge and node insertions and rank reassignments are journalled during staging and reverted on rejection, instead of cloning PK per commit. `sync` reads the staged overlay through a view.
5. **Incremental A5.**
   - BaseNotAllowed is checked for revisions whose base became true in the batch.
   - NoEnabledRule is checked for new revisions.
   - A `SetPolicy` in the batch triggers the full scan.
   - Candidates are scanned in ascending RevisionId order, so the first error is identical.
6. **Incremental status.**
   - Publication appends to the arenas, updates the indexes and runs the maintainer.
   - The status walker then runs, as in mutable commits, seeded by: new revisions, conclusions of new rules, base flips, head moves (with exact/pin/Sem dependants), enable flips and rules with new validation records.
   - New revisions have no pre-state.
   - A materialized `DerivedSnapshot` is extended and patched, not rebuilt.

## Verification

- Unchanged and required:
  - P0 goldens and error equivalence;
  - P1 goldens in both modes;
  - 10,000 paranoid seeds (oracle Jacobi after every commit);
  - exhaustive search at depth 6 and 8;
  - PROP7;
  - P2 PROP10;
  - the mutation audit, with `txn.rs` and the new staging module in scope.
- New:
  - structural-commit latency probes in the bench (p99 at 100K revisions);
  - cold-start replay of 100K structural transactions.
- Memory: the new indexes are counted in the ADR 0005 hot scope. If a scope then exceeds its budget, that is reported, not silently recalibrated.

## Results (P2 gate, 2026-10-20)

**Implementation differences from the decision text.**
- **Lookups.** Only logical key → id is an open-addressing table. Its probes are bounded by `len + 1`, because the load factor is at most 3/4. The other two lookups scan one adjacency row instead of using a table:
  - (logical, content) → revision scans the logical's row of `revisions_by_logical`;
  - rule key → rule scans the conclusion's row of `rules_by_concl`.

  Both rows are short in every measured shape, and no extra table is kept.
- **Growth.** Arena columns grow by one rule, `memory::growth`: exact from empty and for bulk appends, at least 1.25× for small appends. This keeps the memory scopes within ADR 0005 under incremental publication (hub hot was 73.9 B/inc with per-commit exact reserve, and is 67.06 now).
- **A5 candidates.** A5 checks every revision the batch made base, including overrides that were already base. These passed A5 at the previous commit and pass it again, so the first error is unchanged.
- **NoEnabledRule.** For new revisions only staged rules are consulted (`Stage::staged_rule_enabled`), because no published rule can conclude a new revision.

**Verification.** Every check listed above passed on the final code:
- P0 52 and P1 24 tests in both modes;
- 10,000 paranoid seeds;
- exhaustive depth 6 (770 states, 11,275 transitions) and depth 8 (1,711 / 30,350), with 0 violations;
- PROP7;
- PROP10, both the store variant at 10,000 seeds and core `from_inputs` == incremental at 10,000 seeds;
- the mutation audit over `counting.rs`, `txn.rs`, `stage.rs`, `arena.rs` and `memory.rs` ends with 0 surviving mutants (see `docs/gates/p2-mutation-summary.json`).

New core tests:
- rebuilt indexes equal incremental ones;
- staging-overlay reads (published, then staged);
- A5 candidates;
- key-table wrap-around and growth;
- adjacency overflow, compaction and strict equality;
- exact growth capacities.

**Measurements** (Apple M1 Pro, release, details in `docs/gates/p2-env.json`):

| Probe | Before | After |
|---|---:|---:|
| Replay 20,000 single-create commits | 44 s | — |
| Cold open, full replay of 100,000 structural transactions | O(N²), not completed | 1,245 ms |
| Cold open with a snapshot at the tail | — | 158 ms |
| Structural commit p99 at 100K revisions (create logical + rule), multi_support / hub | — | 39.0 µs / 5.0 µs |

The P1 memory scopes still pass on both shapes. Live heap is 184.89 (multi_support) and 194.63 (hub) B/incidence, against 181.15 / 188.87 at the P1 gate; the new `revisions_by_logical` index and overflow rows are counted in the hot scope.
