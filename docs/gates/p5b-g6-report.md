# P5b G6 — symbol level against file level (agent-labelled)

> **Superseded for the gate by the [walk 3 retest](p5b-g6-r2-report.md)**,
> which passes under the owner's move convention (SPEC v1.6.7). This first
> run's sample became the training set for walk 3.

Status: **failed**, 2026-10-23. Neither threshold is met by the weighted
point estimates, under the fixed rules or under any of the sensitivity
analyses below. Under SPEC §5 P5b G6, symbol level therefore does **not**
enter the recommended configuration; file-level anchors remain the
recommendation. This report covers G6 only.

## Design (fixed before labelling)

[ADR 0012](../adr/0012-p5b-symbol-anchors.md) §9, r0.6. The tool is
`symbol_replay` (bench). The replay, the population and the 315-pair sample
were committed in `e66cf3b` before any label existed.

- **Replay.** The P5a G6 repositories and commits: ripgrep, click, fd, each
  1,000 first-parent commits. Each commit goes through the real daemon and
  symbol observer.
- **Symbols.** From each P5a anchor file of a supported language, take the
  symbols without a parse error at the selection commit and keep the first 5
  by `git hash-object("<path>#<symbol>")`. That gives 101 symbols in 32 host
  files: ripgrep 45, fd 36, click 20.
- **Pairs.** Every (symbol, commit) where the host file or a file in its
  directory changed: 9,983 pairs.
- **Automatic check.** Every symbol event was recomputed from the commit's
  blobs. It disagrees with the observer 0 times. No host file had a parse
  error in any replayed commit (0 / 32,000).
- **Sample.** Strata are repository × file fired × symbol fired, with 35 pairs
  per stratum. The (file not fired, symbol fired) stratum is empty by
  construction, as the ADR expects, so 9 strata × 35 = 315 pairs.
- **Rates.** False invalidation is FP / fired; missed invalidation is
  FN / positives. Each stratum is weighted by population over labelled count.
  Intervals are a stratified bootstrap with 10,000 resamples and a fixed
  seed.

## Labels

Nine independent agent labellers each took a batch of 35 pairs, mixed across
strata. The batches showed no predictor output or stratum. Labellers used
read-only Git on the clones and were told not to open the gate files. The
question was "does this commit invalidate a decision resting on this symbol
(as at the parent)?".

The labelling rule, written before labelling: a pure move or rename with
identical code is `n`, because finding the symbol again is a rebind, not a
re-validation.

Totals: 52 `y`, 263 `n`, 0 `unsure`. Confidence: 256 high, 56 medium, 3 low.

- [`p5b-g6/labels.tsv`](p5b-g6/labels.tsv) has the labeller, confidence and
  rationale for every label, plus the SHA-256 of its evidence item.
- [`p5b-g6/evidence.json.gz`](p5b-g6/evidence.json.gz) holds, for every pair,
  the commit message, the directory diff and the host file at the parent.

These are agent judgments, not independent human ground truth.

## Results

The full output is in [`p5b-g6-score.txt`](p5b-g6-score.txt).

| Stratum (file, symbol fired) | click | fd | ripgrep |
|---|---:|---:|---:|
| n, n: invalidating / labelled (population) | 0/35 (3,442) | 2/35 (4,663) | 0/35 (177) |
| y, n | 3/35 (257) | 2/35 (988) | 1/35 (55) |
| y, y | 17/35 (38) | 27/35 (318) | 0/35 (45) |

| Weighted, all repositories | Symbol level | File level | Threshold |
|---|---:|---:|---|
| False invalidation | **34.2 %** [23.8, 45.8] | 79.8 % [74.0, 84.6] | symbol ≤ 10 % |
| Missed invalidation | 56.8 % [14.2, 74.8] | 43.7 % [0.0, 67.4] | symbol ≤ file + 2 pp |
| Missed difference | **+13.1 pp** [2.2, 32.1] | | ≤ +2 pp |
| Missed given host changed | 23.3 % [5.0, 39.9] | 0 % by construction | reported |

**Sensitivity.** None of these is the gate; their inputs come from
`scripts/p5b_g6_sensitivity.py`.

| Variant | Symbol FI | Missed difference |
|---|---:|---:|
| Fixed rules (headline) | 34.2 % | +13.1 pp [2.2, 32.1] |
| Pairs whose symbol was already Broken at the parent removed (30 sampled; strata populations restricted alike) | 34.2 % | +9.2 pp [1.2, 26.1] |
| Pure moves and renames counted as invalidating (38 labels) | 20.2 % [10.3, 31.5] | +12.0 pp [2.1, 28.3] |
| Both | 20.2 % | +8.4 pp [1.1, 22.9] |

Symbol level fires far less wrongly than file level (34 % against 80 %). It
still misses both thresholds in every variant, and every missed-difference
interval excludes zero.

## Why

**False invalidations** are symbol-level firings labelled `n`; there are 61
in the sample.

- **Behaviour-preserving rewrites** make up most of them. Examples: `%` →
  `str.format` → f-strings, string concatenation → `.format`, `super()` for
  an explicit base call, a macro replaced by an equivalent function, a
  verbatim helper moved to another module and called there. The normaliser
  is syntactic. ADR 0012 puts semantic equivalence beyond syntax out of
  scope, so it cannot see these rewrites. Formatting-only changes (black,
  rustfmt) did not fire, as T45 predicts.
- **Pure moves and renames.** The 35 ripgrep pairs are all one commit
  (`fdd8510`, "move all source code in crates directory"). Elsewhere, click
  renamed `Context` → `Environment` and fd moved `fshelper/mod.rs` →
  `filesystem.rs`. Symbol level breaks on a move; the labelling rule calls
  that `n`.

**Missed invalidations** are symbol level not firing on a `y` pair; there are
8 in the sample.

- Most are real dependencies outside the span:
  - a module-level set the test checks (`ALLOWED_IMPORTS`, twice);
  - a fixed module import (`urllib.parse`);
  - a called function whose behaviour changed (`error_if_any_error`,
    `hyperlink::host`, `spawn_receiver` via `batch_size`);
  - a dependency swap behind a used type (`Utf8Sequences`).
- That is the inherent cost of the finer grain: file level sees the first
  three kinds whenever they are in the host file.
- P5B-G6-178 is an artefact. Its symbol (`spawn_senders`) had moved into
  `WorkerState` earlier (Broken since `8bbbd76`, 2023-10-27). The labeller
  judged the new method by name. The present-at-parent variant removes such
  pairs.

The missed difference is driven by the (y, n) strata, weighted by their
populations: click 3/35 of 257 and fd 2/35 of 988. The large (n, n)
populations, where both predictors miss alike, raise both missed rates
equally. This is the breadth dependence the ADR documents.

## Limitations

- These are agent labels, not human ground truth. The single-labeller
  batches were not cross-checked.
- The sample is concentrated: ripgrep's fired stratum is one commit, and
  click has 20 symbols, mostly tests and examples.
- **Selection gap.** The fixed rule kept same-directory pairs after a symbol
  was already gone; P5a excluded pairs without a parent anchor. The
  present-at-parent sensitivity covers this.
- Strata are small (2/35 positives drive the fd (n, n) weight of 4,663), so
  the missed-rate intervals are wide. The point estimates decide, as the
  rule says.

## Reproduction

```sh
cargo build --release -p contextdb-bench --bin symbol_replay
symbol_replay prepare <clone> docs/gates/p5a-g6/<repo>.tsv <repo>.tsv   # per repository
symbol_replay sample sample.tsv click.tsv fd.tsv ripgrep.tsv           # = p5b-g6/sample.tsv
symbol_replay score docs/gates/p5b-g6/sample.tsv docs/gates/p5b-g6/labels.tsv
python3 scripts/p5b_g6_sensitivity.py <dir>   # sensitivity inputs
```
