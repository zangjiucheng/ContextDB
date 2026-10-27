# Gate P5b — Symbol-level code anchors

Status: **every layer passes and the independent implementation review is done (2026-10-26).** The review found 1 blocker and 6 majors; they are fixed as normaliser walk 4 and a live-only position header ([ADR 0012](../adr/0012-p5b-symbol-anchors.md) r0.9, SPEC v1.6.10), and G2, G3, G4 and G6 were rechecked on walk 4. The phase gate cannot pass before P5a's, which waits on P4 (SPEC A11). Live `.contextdb/bin` and `.contextdb/db` are untouched; symbol anchors made with walk 3 need `ctx anchors migrate`.

## Scope

SPEC SD18 (symbol keys, descriptor v2, normaliser versions, grouping, local parse errors), SD14b (host-file positions, `CTXPOS02`), §5 P5b. Rust, TypeScript/TSX/JavaScript, Python and Go ship together; other files stay file-level.

| Layer | Requirement | Status |
|---|---|---|
| G0 | ADR 0012, SPEC v1.6.5, independent review | Passed (2026-10-22) |
| G1 | fmt, Clippy, tests, deny, dependency policy (`[symbols]`); `ctxd` links no grammar | **Pass (2026-10-26)**: `bash scripts/check.sh` on macOS and on the remote Linux host (Ubuntu 24.04) for the walk-4 tree (loom models, Python script tests, dependency policy, `cargo deny`, the R2 workspace). `cargo tree` for `contextdb-daemon` and `contextdb-hooks` has no tree-sitter crate |
| G2 | T45 (real formatters), T45b, T42b, T42d, T42e, T46d, T46e; per-language goldens; must-differ pairs; key round trips | **Pass (2026-10-23)** — [closure](p5b-g2.md). [T45 corpus](p5b-t45/README.md): 6 corpora, 6,820 symbols; 10 explained exceptions; walk check 1→2: 2,226/6,820 re-hash; walk 4 (r0.9): T45 identical, walk check 3→4: 886/6,820, review counterexamples as goldens in `tests/walk4.rs`. Revalidated on 2026-10-24: 16 language/boundary goldens, 11 anchor/format tests, 3 CLI scenarios. Added the missing legacy `#` path/key-collision regression; registration now refuses the collision atomically |
| G3 | PROP12 extended (generator model + SPEC-form oracle, all floors); normaliser/extraction mutation | **Pass (2026-10-23)** — [report](p5b-g3.md): 10,000 seeds / 200,000 transactions with independent normalisation; separate model-driven 10,000 seeds / 100,000 transactions; original P5a 10,000-seed carry-forward. All floors met. 243 mutants: 230 caught, 13 compile-invalid, no survivors or unresolved timeouts. **Walk 4 (2026-10-26)**: the SPEC-form oracle restates the walk-4 rules and both suites model the live-only header; both pass at 10,000 seeds (macOS and Linux, G7). Mutation of `walk.rs`, `extract.rs`, `context.rs`, `lib.rs` with `robustness`: 399 mutants, every non-equivalent one caught (2 equivalent gap-guard survivors), [details](p5b-g3.md#walk-4-adr-0012-r09-2026-10-26) |
| G4 | P5a robustness plus broken host files, deep/large trees, parse cancellation, budgets, observer crash | **Pass (2026-10-23)**: [report](p5b-g4.md). Three defects found and fixed, with no hash change: a stack overflow on deep nesting, a deadline not enforced after the parse with a super-linear walk, and empty `MISSING` names. Tests: deep/large trees on a 2 MiB stack, walk cancellation, the parse budget end to end, observer SIGKILLed mid-scan (all or nothing). P5a carry-forward passes. `symbols_parse` fuzz 1 h, 0 crashes. Mutation of the changed files: 250 caught, 28 compile-invalid, 0 missed. **Walk 4 (2026-10-26)**: deep `use` trees and Python targets no longer recurse (5,000-deep test on a 2 MiB stack); fuzz rerun 1 h, 73.6 M executions, 0 crashes; robustness suite passes |
| G5 | P4 hook budgets (no parse on the live path); scan cost for 10/100/1,000 symbol anchors | **Pass (2026-10-23)**: [report](p5b-g5.md), [`p5b-g5-symbols.txt`](p5b-g5-symbols.txt), `p5b_g5_pass=true` in three runs on the M1 Pro. At 1,000 symbol anchors: hook p95 ≤ 29 ms, p99 ≤ 37 ms, 999/1,000 certified reads, steady scan p95 42 ms. 10/100 anchors: every read certified, no warnings. `ctxd`/`ctx-hook` link no grammar. A whole scan costs ≈ 0.38 s/MiB, so the deadline binds at ≈ 13 MiB of host bytes |
| G6 | Same three repositories × 1,000 commits; symbol FI ≤ ½ × file FI, missed ≤ file-level + 2 pp (weighted point estimates; owner rules v1.6.5–v1.6.7) | **Pass (2026-10-23, owner convention)**: [retest report](p5b-g6-r2-report.md), [`p5b-g6-final-score.txt`](p5b-g6-final-score.txt). First run (walk 2, 315 labels) failed: FI 34.2 %, missed +13.1 pp ([report](p5b-g6-report.md)). Walk 3 adds same-file context; the retest uses 287 fresh blind agent labels on disjoint symbols. Under the owner convention (pure moves invalidate; decided after the labels, disclosed): symbol FI 18.7 % ≤ ½ × 79.7 %, missed +0.0 pp. Under the pre-registered reading (moves do not invalidate): FI 70.4 % > ½ × 92.6 %, a fail driven by ripgrep's whole-tree move; both are reported. **Walk 4 rescore (2026-10-26, owner decision, same labels and strata)**: FI 17.5 % ≤ 39.8 %, missed +0.0 pp, pass; pre-registered 70.0 %, fail ([`p5b-g6-walk4-score.txt`](p5b-g6-walk4-score.txt)) |
| G7 | P0–P5a regressions | **Pass (2026-10-26)**: [`p5b-g7.txt`](p5b-g7.txt) (remote, `scripts/gate-regression.sh` on the walk-4 tree): paranoid testkit, exhaustive depth 6 (770 states) and 8 (1,711), store/daemon crash injection at 1,000 per point and PROP10 at 10,000 seeds, P4, P5a and P5b suites in release; 0 failures |
| G8 | README, semantics, CHANGELOG, status | **Pass**: README P5b section (languages, keys, identity, freshness, cost, commands, upgrade note), SPEC v1.6.10, CHANGELOG, `IMPLEMENTATION_STATUS.md` |
| Independent review | non-implementing implementation review | **Done (2026-10-26)**: 1 blocker, 6 majors fixed (walk 4), minors fixed or amended in the ADR (below) |

## Independent implementation review (2026-10-26)

A non-implementing reviewer read ADR 0012, SPEC SD18/SD14b and the code, and
confirmed each finding with a probe binary or a precise code path. Owner
decisions on 2026-10-26:
- fix everything as walk 4;
- keep the released bare type-parameter keys and amend the ADR;
- rescore G6 on the existing labels.

| # | Finding | Resolution |
|---|---|---|
| B1 | The walk emitted only kept children, so text no child covers was lost: Python string text between escapes, f-string text, format specifiers, bytes; Rust raw-string delimiters; `macro_rules!` separators | Gap leaves (verbatim inside strings), walk 4; goldens; the SPEC-form oracle restates the rule |
| M1 | A Missing/Broken anchor kept its pre-migrate descriptor: scans refused forever and the daemon's header disagreed with the observer's | Version check and `CTXPOS02` header over live anchors only (observer, daemon, range check); CLI test fails without either half |
| M2 | A context item's parse error did not mark the symbol (a helper mid-edit re-revisioned callers twice) | Counted items' errors and names in top-level `ERROR` regions mark the symbol; while closing it, broken module-level assignments and imports were found to have the same gap and are covered |
| M3 | Python statements in module-level `if`/`try` were items with the whole block's range and hash | One item per statement |
| M4 | Go `for` clause `;` dropped (`for g(); ; {}` ≡ `for ; ; g() {}`) | Kept in `for_clause` |
| M5 | Grouped Go `var ( … )` yielded no symbols | Extracted |
| M6 | Recursive Rust `use` and Python target walks overflowed on deep input | Iterative; 5,000-deep test on a 2 MiB stack |
| minor | JS array holes; multi-declarator keywords; `export declare`; dotted namespaces; `this.#p`/`r#` context names; Go `/vN` and `gopkg.in` names; `import a.b` source; normaliser version strings unchecked | Fixed in walk 4 with goldens; a test pins grammar versions to `Cargo.lock` |
| minor | Bare type-parameter self type keyed `T#Tr::m`, ADR said `#<hash>` | ADR amended (keys frozen; owner decision) |
| minor | Enclosing-chain errors: only `ERROR` ancestors checked | ADR clarified (header errors are outside the span like header changes) |
| minor | Same-directory rebind search promised by the ADR was never implemented | ADR amended: candidates come from host files only |
| minor | Python conditional blocks also descended at class level | ADR documents it |

Rechecks on walk 4:
- **T45**: identical on all six corpora.
- **Walk check 3 → 4**: 886 of 6,820 symbols re-hash.
- **PROP12**: both suites pass at 10,000 seeds.
- **Mutation**: see G3 above.
- **G6 rescore** ([report](p5b-g6-r2-report.md#walk-4-rescore-2026-10-26-adr-0012-r09)):
  under the owner convention FI 17.5 % ≤ 39.8 %, missed +0.0 pp, pass;
  under the pre-registered reading 70.0 %, fail.
- **G4**: `symbols_parse` fuzz rerun on walk 4, 1 h on the remote host (40 workers): 73.6 M executions, coverage 3,344, 0 crashes, timeouts or OOMs.
