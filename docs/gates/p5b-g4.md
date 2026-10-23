# P5b G4 — robustness

Status: **passed**, 2026-10-23. This report covers G4 only; the P5b phase
gate still depends on its remaining layers and on P5a and P4.

ADR 0012 §9 asks for G4 "as P5a, plus host files that stay broken while
edited (scope lags, nothing retracted) and when committed (Broken, scope
live), deep and large trees within bounds, parse cancellation at the
deadline, the parse-byte budget, and an observer crash mid-scan (nothing
published)".

## Findings and fixes

G4 found three defects, all fixed before the evidence below was produced
(commits `ab7091a`, `3a64dec`; ADR 0012 r0.5). Mutation then found four test
gaps, closed by new tests (below).

1. **Stack overflow on deep nesting.** Extraction recursed once per
   container. An 80 KB file of 10,000 nested `mod a {` aborted the observer
   process. **Fix:** containers nested deeper than 256 levels (the file
   counts as one) hold no symbols, so extraction never enters them.
2. **Deadline not enforced after the parse.** Only tree-sitter's parse
   checked the deadline. Walk cost grew faster than file size: tree-sitter's
   `parent()` descends from the root, `child(i)` is indexed from the first
   child, and the optional-chain check rescanned every subtree. As a result
   an 8 KB file of 1,000 nested modules took 29 s, and a 16 MiB flat file
   took 11 s, both returning `Ok` long after the 5 s deadline. **Fixes:**
   - extraction and walk check the deadline every 1,024 nodes;
   - ancestor facts (inside `ERROR`, optional chains) come from one pass;
   - each node's parent and grandparent are carried down the walk;
   - children are iterated, not indexed;
   - one pass feeds both the plain and the name-masked hash.
3. **Empty symbol names** (found by the new fuzz target). Error recovery's
   zero-width `MISSING` identifier produced the JavaScript symbol `""`. It
   has no key, so `ctx anchors symbols` failed on such a file. **Fix:**
   extraction admits exactly the paths `validate_symbol` accepts.

The scan also parses each distinct host content once and checks the 32 MiB
budget before any parse, as ADR 0012 §4 already said for blob OIDs.

**No hash changed.** Plain and name-masked hashes are byte-identical before
and after the fixes on the six T45 corpora (6,820 symbols, each binary's
`symbol_hashes` output diffed). Both PROP12 suites (10,000 seeds each) and
the 15 language goldens pass on the final code. Walk revision 2 was not yet
released, so the nesting bound is part of it and no normaliser version
changes.

| Input (release, 5 s deadline) | Before | After |
|---|---|---|
| Rust `mod a {` × 1,000 (8 KB) | `Ok` after 29 s | `Ok`, 256 symbols, 0.38 s |
| TypeScript `namespace a {` × 1,000 (14 KB) | `Ok` after 57 s | `Ok`, 256 symbols, 0.50 s |
| Rust `mod a {` × 10,000 (80 KB) | stack overflow, abort | `Ok`, 256 symbols, 4 s |
| TypeScript `namespace a {` × 10,000 and deeper | — | unavailable at the deadline |
| 16 MiB of `fn f() -> u8 { 1 }` | `Ok` after 11.0 s | unavailable at 5 s (7.1 s with no deadline) |

Each of the 256 nested spans covers the whole remaining nest, so very deep
nesting still costs up to 256 × the file size. Such inputs end at the
deadline as unavailable, the fail-closed outcome.

## Evidence

| Requirement | Test | Result |
|---|---|---|
| P5a carry-forward | `contextdb-anchors` (incl. PROP12 10,000 seeds), `contextdb-daemon --test anchors` (store-step interruption, CAS races, missed wakeups), `contextdb-cli` (watch/restart, oversized file) | Pass |
| Broken host while edited: scope lags, nothing retracted; committed: Broken, scope live, rest unaffected | `symbols_cli::symbol_anchors_end_to_end` (T42d), `robustness::declarations_under_an_error_root_are_errors`, `missing_names_are_not_symbols`, both PROP12 suites (fatal syntax error ≥ 2 % of transactions, edited then committed then recovered) | Pass |
| Deep trees within bounds | `robustness::deep_containers_stop_at_the_nesting_bound`, `sibling_containers_are_not_nesting`: Rust, TS, TSX, Python at 255/256/257 levels (exact symbol counts) and 20,000; `deep_expressions_stay_linear`: 50,000-deep parentheses, blocks, lists and optional chains in all languages. All on a 2 MiB thread stack | Pass |
| Large trees within bounds | `robustness::large_files_within_the_deadline`: 16 MiB files, many declarations, and single declarations with millions of list elements, in every language family | Pass |
| Parse cancellation at the deadline | `robustness::walk_is_cancelled_at_the_deadline` (the parse ends early; extraction and walk over 255 nested copies of a large body are cancelled); `passed_deadline_is_unavailable_for_every_language` | Pass |
| Parse-byte budget | `symbols_cli::parse_byte_budget_end_to_end`: 3 × 11 MiB distinct hosts → unavailable before any parse, nothing committed; the same contents in all three count once | Pass |
| Observer crash mid-scan | `symbols_cli::observer_killed_mid_scan_publishes_all_or_nothing`: the real `ctx anchors scan` is SIGKILLed at 0–150 % of one observation time, until a run finishes. Every state seen is the state before or the complete scan; the next scan converges. 10–13 kills per run in debug and release, some after the commit | Pass |
| Arbitrary input | `fuzz/fuzz_targets/symbols_parse.rs`: any bytes in any language; no panic or stack overflow; deterministic; every extracted name round-trips through its key | Pass: 3,605 s on the remote Linux host, 24 workers, ≈ 46.7 million executions, 1,914-input corpus, 0 crashes, timeouts or OOMs (after fix 3, which an earlier run found) |

**Overshoot past the deadline.** Noticing the deadline and freeing the tree
and table of a 16 MiB file took 0.2–0.6 s in a solo run. Under parallel test
load it took up to about 1.6 s. The tests allow 2 s. A scan that misses its
deadline commits nothing, so the overshoot affects only latency.

## Mutation

G3's mutation ran on the walk and extraction before these fixes, so it was
repeated on the changed files (`walk.rs`, `extract.rs`, `lib.rs`) with the
G2 goldens, the 60-seed `prop12_symbols` slice and `robustness`.

- **First run.** 278 mutants: 242 caught, 28 compile-invalid, 7 missed and 1
  timeout. The misses showed four gaps:
  - the "inside `ERROR`" mark was never exercised (four mutants);
  - the nesting counter was never decremented across siblings (two);
  - `Unavailable`'s `Display` was untested (one);
  - a disabled deadline check only timed out instead of failing (one).
- **New tests.**
  - `declarations_under_an_error_root_are_errors`: a Go input from the fuzz
    corpus whose tree-sitter root is itself `ERROR`. This is the only way a
    declaration's ancestor can be `ERROR`, because extraction never enters an
    inner `ERROR` node.
  - `sibling_containers_are_not_nesting`: 300 sibling modules.
  - `unavailable_displays_its_reason`.
  - Every timed parse now fails as soon as its slack runs out. Timed parses
    run one at a time, so the slack is not spent waiting for cores.
- **Closure run.** The 13 mutants on those lines: 13 caught.
- **Final.** 278 mutants: **250 caught, 28 compile-invalid, 0 missed,
  0 timeouts**. The compile-invalid mutants are `Default` values for types
  without one (`Node`, `Unwrap`, `Rules`, `Tree`, `Range`, a tuple of them),
  and eight `&&` → `||` swaps inside `let` chains.
- **Integrity.** Outcome SHA-256: first run `4119490d…`, closure `5df4a4e2…`.
  Source SHA-256: `walk.rs` `9c03c109…`, `extract.rs` `0d4b9266…`, `lib.rs`
  `131826c8…`.

## Reproduction

```sh
cargo test --release -p contextdb-symbols --test robustness
cargo test -p contextdb-cli --test symbols_cli
cargo test -p contextdb-anchors && cargo test -p contextdb-daemon --test anchors
# remote Linux host (CTXDB_REMOTE in the environment):
scripts/remote.sh run p5b-g4-fuzz env FUZZ_TARGETS=symbols_parse FUZZ_SECONDS=3600 bash scripts/fuzz.sh
scripts/remote.sh run p5b-g4-mutation bash -c 'unset CARGO_TARGET_DIR; \
  CONTEXTDB_SYMBOL_SEEDS=60 CARGO_BUILD_JOBS=2 cargo mutants --package contextdb-symbols \
  --file crates/contextdb-symbols/src/walk.rs --file crates/contextdb-symbols/src/extract.rs \
  --file crates/contextdb-symbols/src/lib.rs --jobs 24 --timeout 300 \
  --output results/p5b-g4-mutation -- --package contextdb-symbols \
  --test symbols --test prop12_symbols --test robustness --locked'
```

`prop12_symbols` is the independent-normalisation suite from G3, so it must
be present in the tree.
