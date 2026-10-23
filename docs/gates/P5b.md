# Gate P5b — Symbol-level code anchors

Status: **G0, G3, G4 and G5 passed; remaining verification in progress.** [ADR 0012](../adr/0012-p5b-symbol-anchors.md) r0.3 accepted 2026-10-22 (r0.4 adds walk revision 2 from the T45 corpus; r0.5 the G4/G5 resource bounds) after an independent review (1 blocker, 8 majors), a closure check (3 new majors) and a closure confirmation; SPEC v1.6.5 mirrors it. The gate cannot pass before P5a's, which waits on P4 (SPEC A11). Live `.contextdb/bin` and `.contextdb/db` are untouched.

## Scope

SPEC SD18 (symbol keys, descriptor v2, normaliser versions, grouping, local parse errors), SD14b (host-file positions, `CTXPOS02`), §5 P5b. Rust, TypeScript/TSX/JavaScript, Python and Go ship together; other files stay file-level.

| Layer | Requirement | Status |
|---|---|---|
| G0 | ADR 0012, SPEC v1.6.5, independent review | Passed (2026-10-22) |
| G1 | fmt, Clippy, tests, deny, dependency policy (`[symbols]`); `ctxd` links no grammar | Pending |
| G2 | T45 (real formatters), T45b, T42b, T42d, T42e, T46d, T46e; per-language goldens; must-differ pairs; key round trips | **Evidence complete, pending gate** — [T45 corpus](p5b-t45/README.md): 6 corpora, 6,820 symbols, formatter default/stress, comment and doc-comment edits; 10 listed exceptions. T45b, walk-2 equivalence/must-differ goldens, per-language goldens (`contextdb-symbols/tests/symbols.rs`); T42b, T42d, T45-min, T46d, T46e, T42e end to end (`contextdb-cli/tests/symbols_cli.rs`); walk check 1→2: 2,226/6,820 re-hash |
| G3 | PROP12 extended (generator model + SPEC-form oracle, all floors); normaliser/extraction mutation | **Pass (2026-10-23)** — [report](p5b-g3.md): 10,000 seeds / 200,000 transactions with independent normalisation; separate model-driven 10,000 seeds / 100,000 transactions; original P5a 10,000-seed carry-forward. All floors met. 243 mutants: 230 caught, 13 compile-invalid, no survivors or unresolved timeouts |
| G4 | P5a robustness plus broken host files, deep/large trees, parse cancellation, budgets, observer crash | **Pass (2026-10-23)**: [report](p5b-g4.md). Three defects found and fixed, with no hash change: a stack overflow on deep nesting, a deadline not enforced after the parse with a super-linear walk, and empty `MISSING` names. Tests: deep/large trees on a 2 MiB stack, walk cancellation, the parse budget end to end, observer SIGKILLed mid-scan (all or nothing). P5a carry-forward passes. `symbols_parse` fuzz 1 h, 0 crashes. Mutation of the changed files: 250 caught, 28 compile-invalid, 0 missed |
| G5 | P4 hook budgets (no parse on the live path); scan cost for 10/100/1,000 symbol anchors | **Pass (2026-10-23)**: [report](p5b-g5.md), [`p5b-g5-symbols.txt`](p5b-g5-symbols.txt), `p5b_g5_pass=true` in three runs on the M1 Pro. At 1,000 symbol anchors: hook p95 ≤ 29 ms, p99 ≤ 37 ms, 999/1,000 certified reads, steady scan p95 42 ms. 10/100 anchors: every read certified, no warnings. `ctxd`/`ctx-hook` link no grammar. A whole scan costs ≈ 0.38 s/MiB, so the deadline binds at ≈ 13 MiB of host bytes |
| G6 | Same three repositories × 1,000 commits; symbol FI ≤ ½ × file FI, missed ≤ file-level + 2 pp (weighted point estimates; owner rules v1.6.5–v1.6.7) | **Pass (2026-10-23, owner convention)**: [retest report](p5b-g6-r2-report.md), [`p5b-g6-final-score.txt`](p5b-g6-final-score.txt). First run (walk 2, 315 labels) failed: FI 34.2 %, missed +13.1 pp ([report](p5b-g6-report.md)). Walk 3 adds same-file context; the retest uses 287 fresh blind agent labels on disjoint symbols. Under the owner convention (pure moves invalidate; decided after the labels, disclosed): symbol FI 18.7 % ≤ ½ × 79.7 %, missed +0.0 pp. Under the pre-registered reading (moves do not invalidate): FI 70.4 % > ½ × 92.6 %, a fail driven by ripgrep's whole-tree move; both are reported |
| G7 | P0–P5a regressions | Pending |
| G8 | README, semantics, CHANGELOG, status | Pending |
| Independent review | non-implementing implementation review | Pending |
