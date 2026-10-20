# P1 mutation audit

Tool: cargo-mutants 27.1.0, Rust 1.95.0. Scope (current): `engine/counting.rs`, `txn.rs`,
`arena.rs`, `memory.rs`. The first three rounds below covered counting/txn only.
Tests: both exact modes of P0/P1 golden plus core unit boundary tests.

The first audit tested 146 mutants: 122 caught, 8 unviable, 16 survivors. Survivors
revealed missing positive inclusive bounds (1024 premises / 256-byte key / 64-byte
watermark), oversized keys/scopes, metadata invalid path variants, nonzero artifact
IDs, Observer anchor updates/resolution, and incidence capacity boundaries. These
now have explicit positive/negative contract tests; no production rule was weakened.

The second unfiltered audit tested 156 mutants: 140 caught, 8 unviable, 8 survivors.
All remaining survivors are in `Counting::retained_bytes`: return 0 / 1 and six
arithmetic changes. This diagnostic is a conservative allocation estimate, not a
truth-maintenance or transaction path. Its result is never used to publish state,
accept input, choose witnesses, or certify a memory pass. A below-budget lower
bound is explicitly UNVERIFIED. These eight diagnostic mutants are waived, not
counted as caught; the G5 memory failure remains real.

`scripts/mutation.sh` excludes only that diagnostic method for recurring semantic
mutation checks. All counting and transaction behavior remains in scope, including
the counter-before journal added by the locality repair. The first journal-aware semantic rerun had 150 mutants: 141 caught, 8 unviable,
1 survivor (`r.index() < old_len` changed to `<=`). Published transaction behavior
was unchanged because structural commits do not consume the journal. However,
the Maintainer API promises pre-existing rows only. A focused contract test now
checks old-row contents and absence of invented prestate for newly allocated rows.

Final semantic audit: **150 mutants, 142 caught, 8 unviable, 0 survivors**; exit 0.
This includes the journal boundary mutant. The eight unfiltered diagnostic waivers
remain explicit, excluded rather than misreported as caught. Machine-readable
counts, source hashes and unviable mutations: [p1-mutation-summary.json](p1-mutation-summary.json).
Raw final output was `/tmp/contextdb-p1-mutation-closed/mutants.out` for this run;
future runs write ignored `results/p1-mutation`.

## Compaction round (ADR 0005)

ADR 0005 uses the category accounting to certify memory scopes. The diagnostic
waiver above therefore no longer applies and has been removed.
`tests/memory_accounting.rs` now does three things:
- checks the accounting against a counting allocator (no over-count, ≤ 1–2 %
  untracked);
- pins exact category values for two deterministic scenarios;
- exercises references, validations, artifacts, metadata and interned lanes.

The scope grew to include `arena.rs` and `memory.rs`.

The first extended run had 52 survivors. They were accounting terms that were
zero or below the tolerance, plus three real test gaps:
- the fast path's key lookup;
- structural `impact_exact` rollback of arenas;
- CSR equality only ever checked positively.

All three now have tests. A dead `RuleArena.flags` column was removed. An
intermediate run left 11 survivors, all on empty artifact/validation/reference
paths; the scenario was extended to cover them.

Final: **394 mutants, 361 caught, 33 unviable, 0 survivors, no exclusions**.
Source hashes and the unviable list: [p1-mutation-summary.json](p1-mutation-summary.json).

## P2 round (ADR 0007)

The scope now includes `stage.rs`. The first full run on the incremental-commit code tested 617 mutants: 42 survived and 8 timed out, all in the new key table, adjacency, indexes and staging overlay. Fixes:
- **Removed code.** Four overlay guards were dead or had no observable effect, and were removed (empty adjacency rows already cover them). The NoEnabledRule lookup became `staged_rule_enabled`.
- **Bounded probes.** Key-table probes are bounded, so a broken probe panics instead of looping forever.
- **Shared lookup.** The rehash closure reuses the key lookup used by `find`.
- **New unit tests:** overlay reads, A5 candidates, rebuilt indexes == incremental, key-table wrap-around and growth, and adjacency overflow, compaction and equality. The accounting scenario gained Cur incidences, with updated goldens.

The second full run tested 601 mutants: 535 caught, 43 unviable, 0 timeouts and 23 survivors. Every survivor was in capacity growth, which is invisible to accounting that measures a clone. The growth arithmetic became one function, `memory::growth`, tested together with exact arena reserve capacities.

The rerun of `arena.rs` and `memory.rs` tested 350 mutants: 321 caught, 29 unviable, 0 survivors.

Final: **0 surviving mutants, 0 timeouts, no exclusions.** See [p2-mutation-summary.json](p2-mutation-summary.json). The nightly job runs the full scope in four shards.
