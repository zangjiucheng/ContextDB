# Gate P5b — Symbol-level code anchors

Status: **G0 passed; implementation in progress.** [ADR 0012](../adr/0012-p5b-symbol-anchors.md) r0.3 accepted 2026-10-22 after an independent review (1 blocker, 8 majors), a closure check (3 new majors) and a closure confirmation; SPEC v1.6.5 mirrors it. The gate cannot pass before P5a's, which waits on P4 (SPEC A11). Live `.contextdb/bin` and `.contextdb/db` are untouched.

## Scope

SPEC SD18 (symbol keys, descriptor v2, normaliser versions, grouping, local parse errors), SD14b (host-file positions, `CTXPOS02`), §5 P5b. Rust, TypeScript/TSX/JavaScript, Python and Go ship together; other files stay file-level.

| Layer | Requirement | Status |
|---|---|---|
| G0 | ADR 0012, SPEC v1.6.5, independent review | Passed (2026-10-22) |
| G1 | fmt, Clippy, tests, deny, dependency policy (`[symbols]`); `ctxd` links no grammar | Pending |
| G2 | T45 (real formatters), T45b, T42b, T42d, T42e, T46d, T46e; per-language goldens; must-differ pairs; key round trips | Pending |
| G3 | PROP12 extended (generator model + SPEC-form oracle, all floors); normaliser/extraction mutation | Pending |
| G4 | P5a robustness plus broken host files, deep/large trees, parse cancellation, budgets, observer crash | Pending |
| G5 | P4 hook budgets (no parse on the live path); scan cost for 10/100/1,000 symbol anchors | Pending |
| G6 | Same three repositories × 1,000 commits; symbol false invalidation ≤ 10 %, missed ≤ file-level + 2 pp (weighted point estimates; owner rule v1.6.5) | Pending |
| G7 | P0–P5a regressions | Pending |
| G8 | README, semantics, CHANGELOG, status | Pending |
| Independent review | non-implementing implementation review | Pending |
