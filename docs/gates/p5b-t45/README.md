# P5b T45 — formatter corpus

ADR 0012 §4 (r0.4): every symbol of a pinned commit is reformatted by the real
formatter, with default settings and with a stress style, and has its comments
and doc comments rewritten; its normalised hash must not change, apart from
listed, explained exceptions. Normaliser walk revision 2.

| Corpus | Commit | Language | Symbols | Default | Stress | Comments | Doc comments |
|---|---|---|---:|---:|---:|---:|---:|
| ripgrep (G6) | `3fce3b5` | Rust, edition 2024 | 3,613 | 3 | 2 | 0 | 0 |
| fd (G6) | `cfed17a` | Rust, edition 2024 | 459 | 0 | 0 | 0 | 0 |
| click (G6) | `2247b35` | Python | 1,379 | 0 | 5 | 0 | n/a |
| cobra v1.8.1 | `e94f6d0` | Go | 646 | 0 | n/a | 0 | n/a |
| ky v1.7.2 | `3ba40cc` | TypeScript | 116 | 0 | 0 | 0 | 0 |
| express 4.21.1 | `8e229f9` | JavaScript | 607 | 0 | 0 | 0 | 0 |

Changed symbols are listed with their first differing normalised tokens in
`<corpus>.tsv`; per-variant counts (files, rejected inputs) are in
`<corpus>.summary.txt`. The G6 commits are each replay's last commit. The G6
repositories have no TypeScript, JavaScript or Go, hence the pinned
supplements. Go has no stress style (gofmt has no options) and its doc
comments are ordinary `//` comments, covered by the comment edit; Python has
no doc comments (docstrings count, §4).

**Listed exceptions (10, all explained):**

- ripgrep, 5 (`assert_eq_printed`, `eqnice`, `eqnice_repr`; `tests`,
  `tests::name` in `globset/src/pathutil.rs`): rustfmt rewrites code inside
  `macro_rules!` bodies and macro arguments, which tree-sitter keeps as token
  trees, so the walk cannot apply its block/semicolon rules there.
- click, 5 (stress only): at width 30 black moves a docstring's closing quotes
  onto their own line, which changes the docstring's content and so
  `__doc__`; docstrings count by design.

**Tools:** rustfmt 1.9.0-stable (Rust 1.95.0), black 24.10.0, prettier 3.3.3,
gofmt (Go 1.25.0). Stress styles: rustfmt `max_width=40, hard_tabs, tab_spaces=2,
use_small_heuristics=Max, fn_params_layout=Vertical`; black `-l 30
--skip-magic-trailing-comma`; prettier `--print-width 30 --no-semi
--single-quote --trailing-comma none --arrow-parens avoid --tab-width 8`.

```sh
cargo build --release -p contextdb-bench --bin symbol_corpus
symbol_corpus <clone> <commit> <out.tsv> [--edition 2024] [--prettier <bin>]
```

**Walk check (normaliser version change 1 → 2):** `symbol_hashes` built at
both revisions over the same six commits: 2,226 of 6,820 symbols re-hash
(ripgrep 681, fd 117, click 855, cobra 0, ky 63, express 510). Existing symbol
anchors need `ctx anchors migrate`.
