# P1 mutation audit

Tool: cargo-mutants 27.1.0, Rust 1.95.0. Scope: `engine/counting.rs`, `txn.rs`.
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
