# P5b G3 — two-oracle PROP12 and mutation

Status: **passed**, 2026-10-23. This report covers G3 only;
the P5b phase gate still depends on its remaining layers and P5a/P4.

## Property design

`contextdb-symbols/tests/prop12_symbols.rs` uses 10,000 fixed seeds, each
with 20 transactions and six host files (Rust, TypeScript, TSX, JavaScript,
Python, Go). Three hosts also have file anchors; the other three are host-only.
Each starts with three symbols including a pair differing only by case.
The seeded choices vary names, values, language targets and overlapping edits.

- The renderer's **meaning model** stores ordered same-name occurrences,
  body, signature, attributes and directives. It knows no syntax-tree hashes.
  Meaning equality is checked in both directions against observed hash
  equality, including formatting/comments, outside-span changes and restores.
- The **SPEC-form oracle** shares pinned tree-sitter grammars but independently
  extracts top-level generated declarations, recursively encodes the normalised
  stream, and encodes keys, descriptors and `CTXPOS02` positions. It calls no
  production walk, extractor, trace or classifier. Its domain is the renderer's
  declaration subset; G2 goldens cover the broader language forms.
- Git `fast-import` records every manifest; `cat-file --batch-all-objects --batch`
  reads raw commits, trees and blobs back. Blob bytes and SHA-1/SHA-256 IDs are
  checked against the renderer/capture before observations are made.
- The SUT is the production observer's `targets`/`prepare_scan`, classifier
  and kernel commit. The oracle reconstructs all revision contents, deduped
  heads, bases, open/resolved breaks and the watermark independently. Kernel
  derived state and lag certificate validation are also checked with the
  existing independent kernel oracle.

Edits include insertion, body/signature edits, formatting, ordinary comments,
directive/attribute edits, rename, grouping, deletion, identical restore,
old-head ABA, outside-span edits and fatal syntax errors. Every fatal injection
must actually produce an `ERROR`/`MISSING` in its declaration span before it
counts. The same bytes first fail as an edited worktree capture (no publish),
then observe Broken as committed bytes, then recover. CAS conflicts must fail
atomically; retries rebuild and parse against reloaded state.

Floors count actual transactions, with at most one count per category per
transaction. The language count is the target of the generated transaction,
not merely the presence of a language in the repository. SHA-256 uses 1 in 50
seeds. The original P5a PROP12 remains a separate full-count check.

## Reproduction

`CTXDB_REMOTE` (the remote Linux host) is set in the environment.

```sh
scripts/remote.sh run p5b-g3-properties bash scripts/g3-p5b.sh
CTXDB_REMOTE_JOBS=20 \
  scripts/remote.sh run p5b-g3-mutation bash scripts/mutation-p5b.sh
```

Properties use release mode and the full 10,000 seeds. Mutation tests run the
G2 language goldens plus a fixed 60-seed property slice; this keeps each mutant
bounded and does not replace the full-count run. `CARGO_TARGET_DIR` is unset
for mutation so parallel mutants cannot execute one another's binaries.
The script caps mutation at 24 workers × 2 build jobs; this run uses 20.

The two closure runs used the same target sources and additional assertions:

```sh
CTXDB_REMOTE_JOBS=4 \
  scripts/remote.sh run p5b-g3-mutation-closure env \
  MUTATION_OUTPUT=results/p5b-mutation-closure bash scripts/mutation-p5b.sh \
  --re 'walk.rs:(108|109|136):'
CTXDB_REMOTE_JOBS=2 \
  scripts/remote.sh run p5b-g3-mutation-termination env \
  MUTATION_OUTPUT=results/p5b-mutation-termination bash scripts/mutation-p5b.sh \
  --re 'walk.rs:327:'
```

## Results

Remote Linux host (Ubuntu 24.04), Rust 1.95.0, release properties:

| Suite | Fixed seeds | Transactions | Result |
|---|---:|---:|---|
| Independent normalisation + SPEC reconstruction (`prop12_symbols`) | 10,000 | 200,000 | Pass; 287.56 s |
| Separate random model/renderer (`prop12_symbols_model`) | 10,000 | 100,000 | Pass; all four tests, including premise/error/locality checks and smoke, 81.54 s |
| Original file-level PROP12 (`contextdb-anchors`) | 10,000 | 100,000 | Pass; 5.20 s |

The separate random suite shares no generator or oracle code with the first
suite. It adds late registration, mixed supported/unsupported paths, committed
versus uncommitted worlds, and random overlapping edits. Its SPEC reconstruction
uses production `parse` for syntax hashes, so the first suite's independent
normalisation oracle remains necessary. Every suite meets its floors separately.

Coverage in the independent-normalisation suite (denominator 200,000):

| Category | Transactions | Rate | Floor |
|---|---:|---:|---:|
| Body / formatting / ordinary comments / outside-span (each) | 20,000 | 10% | 5% |
| Delete | 10,000 | 5% | 5% |
| Identical restore | 20,000 | 10% | 5% |
| Old-head ABA | 15,002 | 7.501% | 5% |
| CAS retry | 10,000 | 5% | 3% |
| Lag | 190,000 | 95% | 3% |
| Unavailable / unsupported / rename / grouping / `#` refusal / case alias / fatal syntax (each) | 10,000 | 5% | 2% |
| Restore | 20,000 | 10% | 3% |
| SHA-256 | 4,000 | 2% | 1% |

Target-language transaction counts: Rust 35,006; TypeScript 33,330; TSX
31,672; JavaScript 31,666; Python 36,659; Go 31,667. Each exceeds 10% even
when TS, TSX and JS are counted separately.

Mutation of unchanged `walk.rs` and `extract.rs` generated **243 distinct
mutants: 230 caught, 13 compile-invalid, 0 surviving, 0 unresolved timeouts**.
[Machine-readable audit](p5b-g3-mutation.json) records source hashes, raw outcome
hashes, each run's counts and all compile-invalid mutant names. Final outcomes
are combined by stable mutant name, replacing the earlier result with the
closure result; counts from overlapping runs are not summed.

Preserved progression:

- Initial full run: 199 caught, 31 missed, 13 compile-invalid. Those 31 led to
  complete-name-table assertions, Unicode/`$` cases, foreign/ambient declarations,
  multi-declarator isolation, aliases, comment ranges, and required stream-shape
  markers (important where a wrapper deletion changes pinned bytes without
  collapsing an existing same/differ pair).
- Full follow-up: 226 caught, 3 missed, 13 compile-invalid, 1 timeout.
  The misses were typed TS arrow parameter wrappers and a parenthesised Python
  singleton loop target. Their closure run tested 8 variants, all caught.
- The timeout was `next += 1` → `next *= 1` while skipping a comment after a
  trailing comma: the index never advances. A test-level five-second bound now
  makes this an assertion failure rather than relying on the mutation runner's
  timeout. Its two-variant closure run caught both variants, with no timeout.
- The 13 compile-invalid variants are 8 attempts to use nonexistent `Default`
  implementations (`Node`, `Unwrap`, `Rules`) and 5 illegal `||` let chains.
  They are reported separately, never counted as caught.

The independent suite also confirmed the `CTXPOS02` header must sort version
strings rather than iterate language enum order. That production fix and the
`targets`/`prepare_scan` refactor are covered here. No normalised syntax hashes
change; the walk stays at revision 2. Existing multi-language watermarks may
lag once until the next scan publishes the corrected position.

An initial run of the separate model suite failed because its smoke and full
tests both used the same PID-based temporary Git directory and deleted one
another's files. A per-history atomic run ID fixes that test harness race;
the full four-test suite then passed with default parallel test execution.

Local Mac checks: fmt, symbol-crate Clippy with warnings denied, 15 language /
boundary goldens, and both CLI symbol integration tests passed. The CLI tests
need local Unix-socket access; the sandboxed attempt failed to connect to its
temporary daemon, and the same tests passed outside that restriction. These
are targeted checks; G1 and G7 remain separate pending gate layers.

Raw logs and all mutation outcome directories (including the failed initial
run) are under ignored `results/remote/`. The working tree is based on
`ff78187`; source hashes identify the tested G3 inputs without claiming the
uncommitted work is already a release or the whole P5b gate has passed.

## Later changes

G4 changed `walk.rs`, `extract.rs` and `lib.rs`: a nesting bound, deadline
checks, linear ancestor facts, a single walk pass for both hashes, and
`MISSING` names excluded. Hashes did not change on the T45 corpora. Both
PROP12 suites pass again at full count on that code. The normaliser
mutation was rerun on the changed files, with this suite's 60-seed slice:
250 caught, 28 compile-invalid, 0 missed ([G4 report](p5b-g4.md)). The
source hashes in the [mutation audit](p5b-g3-mutation.json) identify the
code G3 tested, before those changes.

Walk 3 (ADR 0012 r0.7–r0.8) added `context.rs` and the trailing-comma rule.
Mutation was rerun on `walk.rs`, `context.rs` and `lib.rs`. The tests were
the goldens, the new `context` goldens, both PROP12 suites as 60-seed slices
(`CONTEXTDB_SYMBOL_SEEDS`; coverage floors apply only to full counts) and
`robustness`.

- **First run.** 37 missed, all untested context forms: import bindings in
  every language, overlap bounds and the mentions filter.
- **New tests.** `tests/context.rs` covers them.
- **Rerun.** 245 mutants: 218 caught, 26 compile-invalid, 1 missed (a Python
  `import x as y` arm). Its golden was added and kills it when the mutant is
  applied by hand.
- **Final.** 219 caught, 26 compile-invalid, 0 missed. Rerun outcome SHA-256:
  `f379423744e0d61d…`.

## Walk 4 (ADR 0012 r0.9, 2026-10-26)

The independent implementation review found walk-3 defects that G3 had not
caught. The worst was that the walk dropped text no child node covers, such
as Python string text between escapes. The SPEC-form oracle in
`tests/common/spec.rs` normalised the same way, so it agreed with the
production walk. Walk 4 fixes the defects (ADR 0012 §11 r0.9).

**Oracle changes.** The oracle restates every walk-4 rule independently, with
no production code:
- gap leaves, byte for byte inside strings;
- Go `for` clause semicolons;
- context-item errors;
- names in `ERROR` regions outside every span.

Both PROP12 suites now model the r0.9 position header, which lists live
symbol anchors only:
- in `prop12_symbols` the header lists only languages with a symbol observed
  Present;
- in `prop12_symbols_model`, `daemon_position` reads the post-commit live
  descriptors, as the daemon does.

**Full count.** Both suites pass at 10,000 seeds in release on macOS
(two-oracle 239 s, model 63 s). The Linux run is part of G7 (below).

**Mutation.** On the remote Linux host, over `walk.rs`, `extract.rs`,
`context.rs` and `lib.rs`. `scripts/mutation-p5b.sh` now runs the goldens,
`context`, `walk4`, 60-seed slices of both PROP12 suites and `robustness`,
with a 300 s timeout:

- **First run** (without `robustness`, 120 s timeout): 28 missed.
  - Most were new code with weak tests: the error-region names, Go's
    version-element rule, the multi-declarator wrapper keywords and
    `Unavailable`'s `Display`.
  - The rest were robustness-only code (deadline clock, nesting bound,
    `ERROR`-ancestor facts) that this run did not exercise.
- **New tests** in `tests/walk4.rs`:
  - helpers too broken to be extracted (Python, JS);
  - broken top-level assignments and imports (Python, Rust);
  - `export declare` with several declarators;
  - `a/b/vendor` imports.
  Writing them also found that a broken module-level binder was not marked;
  the rule became "names in `ERROR` regions outside every span" (ADR §4).
- **Full rerun** with `robustness`: 399 mutants, 356 caught, 36
  compile-invalid, 7 missed.
  - The `ERROR`-ancestor rule had become redundant on the old robustness
    input, because walk 4's error-region names also mark it.
  - The 2,015-input `symbols_parse` fuzz corpus and 24,000 generated
    Rust/Go inputs were run with the rule disabled. They found inputs decided
    by the ancestor alone, in JavaScript and Rust; these are now
    `robustness::declarations_under_an_error_ancestor_alone_are_errors`.
  - The multi-declarator wrapper search now uses `child_with_descendant`.
- **Iterated** (`--iterate`): 10 caught, 1 compile-invalid, 3 missed.
  - `Facts::of`'s early return for Rust/Go is killed by the Rust case of the
    new robustness test; the mutant was applied by hand.
  - The last two (`walk.rs` gap guards, `<` → `<=`) are equivalent: an empty
    gap emits nothing.
- **Final**: every non-equivalent mutant is caught; 2 equivalent survivors.

**Walk check and T45 on the final code.** 886 of 6,820 symbols re-hash 3 → 4,
and the T45 results are identical on all six corpora
([T45](p5b-t45/README.md)).
