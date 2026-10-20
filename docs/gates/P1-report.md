# P1 implementation report — 2026-10-20

**Gate remains open.** Functional implementation and local semantic checks pass.
The unchanged G5 whole-database memory budget fails; P1 remote Linux/macOS CI is
not yet available. P2 has not started. [Gate matrix](P1.md),
[maintenance ADR](../adr/0004-p1-maintenance.md),
[memory ADR](../adr/0005-p1-memory-budget.md).

## Implemented behavior

Counting executes final-net-delta seeding, stored-bit incidence updates and ordered
propagation. Disabled rules retain counters; Ref is non-supporting. Pearce–Kelly
includes Sem virtual nodes and all rules, with independent NaiveTopo diagnostics.
FullRecompute is a second exact mode; L1/L2 are independently memoized.

Mutable transactions stage undo without cloning the database. A before-row journal
and PK-ordered status walker preserve exact support/witness/status changes while
stopping propagation when only local witnesses/counts change. Existing complete
snapshots are patched, avoiding whole-snapshot destruction on a local commit.
Structural transactions still stage/rebuild input indexes and query caches.

CURRENT L0, WHY/WHY_NOT, depth-limited canonical proof, Reachable IMPACT, Exact
IMPACT with full undo, and endpoint-cancelling/reversible epoch DIFF are available.
Stats are deterministic; NoStats is zero-sized. Timing is outside digests/stats.
DA assertions run in debug and in release with paranoid.

## Local verification

`bash scripts/check.sh` passed: fmt, all-target/all-feature Clippy with warnings
rejected, workspace all-feature tests, three compile-fail checks, seven dependency
policy negative tests, dependency graph, cargo-deny and separate DD build checks.
Core journal boundary tests additionally check pre-existing versus newly allocated
rows. Both exact modes run all 52 P0 tests and 24 P1 tests/regressions.

The paranoid testkit run also passed, including 10,000 fixed ChaCha8 seeds / 120,000
transactions. It compares independent InputModel errors, inputs, Jacobi/Kahn,
Counting/FullRecompute, counters, exact flips/status_diff, rollback, PK validity,
locality and deterministic counters. Panic/error mismatches save EventLog traces.

| Event | Count / 120000 | Fraction | Required |
|---|---:|---:|---:|
| Same revision head/base mixed in batch | 32275 | 26.90% | >=5% |
| Alternative support saves | 20187 | 16.82% | >=5% |
| Cycle rejection | 11471 | 9.56% | >=3% |
| CAS conflict | 10788 | 8.99% | >=3% |
| Rule plus new premise in batch | 10272 | 8.56% | >=3% |
| PK slow path | 14258 | 11.88% | >=2% |

PROP7 adds 1,000 seeds for allowed disjoint writes, split/merge, canonical renaming
and creation-order permutation. Allocator-conflicting operations are not claimed
to satisfy commutation premises. G6 MongoDB→PostgreSQL and T31's grounded DAG
fixture both pass. T08b wording now distinguishes unchanged L0 from changed witness
in status_diff, as SD8.4 already required.

Release + paranoid finite-state exploration uses 3 logicals, 4 revisions, 3 possible
rules, 25 operations (base/head/rule/enable/validation), and canonical input-state
merging. This is bounded exploration of that declared universe, not all histories.

| Depth | States | Transitions | Accepted | Rejected | Pending-sem transitions | Rejected-sem transitions | Violations |
|---|---:|---:|---:|---:|---:|---:|---:|
| 6 (gate) | 770 | 11275 | 6155 | 5120 | 2562 | 284 | 0 |
| 8 (report) | 1711 | 30350 | 18104 | 12246 | 9174 | 2818 | 0 |

Mutation history and precise waivers are recorded in [P1-mutations.md](P1-mutations.md).
The final semantic audit tested 150 mutants: 142 caught, 8 unviable, zero survivors.
Eight non-semantic retained-memory diagnostic mutants from the unfiltered audit
are explicitly waived and excluded, not labelled caught. A journal boundary
survivor was closed by a public Maintainer result-contract test.

## Local G5 measurements

Apple M1 Pro / 16 GiB / macOS 27.0.1 arm64, Rust 1.95.0, release, no features,
one thread, seed 42, CommitStats on, 1,000 samples per probe.
Machine-readable environment: [p1-env.json](p1-env.json).

The early probe alternates Assert/RetractBase on revision 3 with existing support;
its complete derived snapshot is populated outside timing before every commit.
It changes the canonical witness, visits zero incidences and only one status node.
The tail probe toggles real supporting rules among eight near-tail conclusions.
Query probes call CURRENT L0 and WHY depth 3. Timing includes the full commit API,
validation, propagation, status publication and history, without paranoid rebuilds.
These probes cover the reported updates; they are not an exhaustive latency bound
for every possible E_visited<=100 transaction, high-arity rule or structural insert.

| Shape | Revisions | Rules | Incidences | Early commit p99 | Tail commit p99 | Tail max E_visited | CURRENT p99 | WHY depth3 p99 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| multi_support(0.3) | 100000 | 129894 | 329888 | 3.917 us | 3.083 us | 2 | 0.042 us | 1.875 us |
| hub | 100000 | 99997 | 299991 | 4.083 us | 2.875 us | 0 | 0.042 us | 0.209 us |

Both measured latency probes meet initial budgets (commit 1 ms, CURRENT 50 us,
WHY 200 us). Load times were 290 ms / 254 ms respectively; these are informational.
Multi_support has 3 bases / 29897 alternatives / max fanout 29901 / depth 16522;
hub has 3 bases / 0 alternatives / max fanout 99997 / depth 1.

**Memory FAIL:** retained allocation lower bounds are 292.53 B/incidence for
multi_support and 314.32 for hub, including inputs, indexes, maintenance, topology,
query caches and history, excluding payload bodies (none stored in core). The
64-byte target is unchanged. Allocator/B-tree overhead is omitted, so this is a
conclusive failure, not a pass estimate. Vec<bool> is correctly counted as byte
storage in Rust. RSS deltas were 793.85 / 806.28 B/incidence and include freed
staging pages; they are not used as live-heap accounting. Detailed categories and
scope options are in ADR 0005; no budget adjustment has been approved.

## Independent review and remaining work

The non-implementing reviewer identified and drove fixes for release-paranoid DA
activation, metamorphic revision dedup identity, early-node over-invalidation and
whole-cache destruction during local commits. Main propagation was independently
retested at 0–10 us. Final cache-patch review independently found no missing derived columns, reran all
24 P1 paranoid tests and 10,000 seeds / 120,000 transactions successfully, and
recompiled the 100K complete-cache probe (0–22 us across 10 samples). No gate pass is claimed while memory/CI remains open.

Required to close P1: meet the existing memory target through a storage redesign,
or explicitly approve a revised budget scope/calibration; obtain P1 Linux/macOS CI;
preserve review closure through any memory redesign. The implementation does not start P2 while these remain open.

## Reproduce

```sh
bash scripts/check.sh
cargo test --locked -p contextdb-testkit --features contextdb-core/paranoid
cargo test --release --locked -p contextdb-testkit --features contextdb-core/paranoid --test exhaustive -- --ignored --nocapture
CONTEXTDB_EXHAUSTIVE_DEPTH=8 cargo test --release --locked -p contextdb-testkit --features contextdb-core/paranoid --test exhaustive -- --ignored --nocapture
cargo install cargo-mutants --version 27.1.0 --locked
bash scripts/mutation.sh
cargo run --release --locked -p contextdb-bench -- multi_support 100000 1000
cargo run --release --locked -p contextdb-bench -- hub 100000 1000
```

P1 nightly/manual exhaustive and mutation workflow is checked in but has not run
remotely. The old P0 CI result is not evidence for this P1 revision.
