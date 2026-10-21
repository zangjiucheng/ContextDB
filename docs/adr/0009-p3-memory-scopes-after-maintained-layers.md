# ADR 0009 — Memory scopes after maintained L1/L2 columns

Status: **Accepted**, 2026-10-20. The project owner chose "recalibrate scopes", applying ADR 0005's own rule to the moved state. This amends the ADR 0005 budget table; ADR 0005's method (live heap, category lower bounds, upper bound = lower + untracked) is unchanged.

## Change

P3 milestone 1 (ADR 0008 §7) replaced the P1 query-time L1/L2 memo with counter-based columns maintained on every commit:
- per incidence: literal bits;
- per rule: per-layer unsat counters, active flags and the semantic gate;
- per revision: per-layer truth, witnesses and the stale reason.

The memo was counted in the **query_cache** scope. The columns are maintenance state and are counted in the **hot** scope (support indexes + counting + topology + L1/L2 columns). No state was added overall: the memo's per-node entries are gone.

## Measurement

M1 Pro, Rust 1.95.0, release, seed 42, 100K revisions, `contextdb-bench`. All values are upper bounds in B/inc.

| Scope | ADR 0005 budget | multi_support (P1 → now) | hub (P1 → now) |
|---|---:|---:|---:|
| hot | 72 | 63.17 → 77.78 | 67.06 → 81.73 |
| inputs | 50 | 47.17 → 47.17 | 48.68 → 48.68 |
| query_cache | 49 | 42.91 → 28.35 | 44.09 → 27.89 |
| history (B per change) | 58 | 52.65 → 52.65 | 52.65 → 52.65 |
| **total live heap** | Σ | 184.89 → 184.94 | 194.63 → 193.09 |

The total live heap is unchanged. Only its split between hot and query_cache moved.

## Decision

Apply ADR 0005's rule, ⌈1.10 × the larger measured upper bound of the two shapes⌉, to the two scopes whose contents changed:

| Scope | Budget |
|---|---:|
| hot | ≤ 90 B/inc (⌈1.10 × 81.73⌉) |
| query_cache | ≤ 32 B/inc (⌈1.10 × 28.35⌉) |
| inputs | ≤ 50 B/inc (unchanged) |
| history | ≤ 58 B per change (unchanged) |

The scope budgets sum to 172 B/inc, against 171 before. Nothing is loosened overall: the hot budget rises by exactly the state that left the cache, and the cache budget falls accordingly.

The history scope now also holds the strict-mode change log (ADR 0008 §6.3). It is reported, and stays within its 58 B per change budget.

## Consequences

- `contextdb-bench` uses the new hot and query_cache budgets and still exits non-zero on any FAIL, UNVERIFIED or INVALID.
- SPEC v1.5.4 updates the P1 G5 text to cite this ADR.
- The P1 gate stays passed: it passed under ADR 0005 on its own code, and this ADR re-measures the current code.
- The second kernel replica of ADR 0008 §3.3 doubles the daemon's kernel memory. That cost is reported in P3 G5 per replica, not folded into these per-`Db` budgets.
