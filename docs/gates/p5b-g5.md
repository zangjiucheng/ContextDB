# P5b G5 — budgets and scan cost

Status: **passed**, 2026-10-23. This report covers G5 only; the P5b phase
gate still depends on its remaining layers and on P5a and P4.

ADR 0012 §9 requires two things for G5. First, the P4 hook budgets must
still hold, with no parse on the live path. Second, the observer's scan cost
must be reported for 10/100/1,000 symbol anchors over host files up to the
size bound. The pass rule is P5a's (owner decision 2026-10-22):

- the P4 hook budgets (p95 ≤ 50 ms, p99 ≤ 100 ms) hold at every size;
- at 10 and 100 anchors, every read certifies and the hook gives no warning;
- at the 1,000 bound, misses are reported (they are Unknown, which fails
  closed).

Raw output: [`p5b-g5-symbols.txt`](p5b-g5-symbols.txt). The run is on the
local M1 Pro baseline (`symbol_bench`, release), never on the remote host.

## No parse on the live path

`ctxd` and `ctx-hook` link no grammar. `cargo tree` shows no `tree-sitter`
dependency for `contextdb-daemon`, `-hooks`, `-proto` or `-anchors`, and
neither release binary contains a `tree_sitter` symbol. The dependency
policy check (`scripts/check_dependencies.py`, the `[symbols]` boundary)
enforces this in CI. The daemon checks a symbol anchor's freshness from its
host files' positions.

## Hook budgets and scan cost

Each host is a 16 KiB Rust file with ten anchored functions. Every scan runs
the whole observer, start to commit:

- capture with two stable samples;
- parsing each host;
- normalisation;
- the watermark CAS commit.

"Edit" changes one anchored body and back, so it commits a new revision and
then restores the earlier one. Before editing, the session re-reads the
changed symbol, as an agent would. A stale read is rightly Invalid.

| Symbols (hosts) | Initial scan | Steady scan p95 | Edit scan p95 | Certified reads | Hook p95 / p99 | Warnings |
|---|---:|---:|---:|---:|---:|---:|
| 10 (1) | 18.5 ms | 10.2 ms | 9.6 ms | 10/10 | 6.2 / 9.9 ms | 0/100 |
| 100 (10) | 20.1 ms | 16.1 ms | 16.6 ms | 100/100 | 8.1 / 14.6 ms | 0/100 |
| 1,000 (100) | 38.5 ms | 41.8 ms | 42.5 ms | 999/1,000 | 28.7 / 36.7 ms | 0/100 |
| 10, full sync | 33.1 ms | 6.6 ms | 13.3 ms | 10/10 | 5.8 / 6.7 ms | 0/100 |
| 100, full sync | 25.7 ms | 16.2 ms | 20.7 ms | 100/100 | 8.0 / 10.7 ms | 0/100 |
| 1,000, full sync | 48.5 ms | 41.9 ms | 46.4 ms | 999/1,000 | 27.8 / 30.7 ms | 0/100 |

`p5b_g5_pass=true` in all three runs that day. The 1-minute load was 7–10 at
each start. With 1,000 symbol anchors, the hook's p95 (27–29 ms) is below
P5a's 1,000 file anchors (36–37 ms). That is expected: the daemon captures
100 host files rather than 1,000 paths.

**Host size** (one host, ten anchors, 5 s observer deadline):

| Host bytes | Initial scan | Steady scan p95 |
|---:|---:|---:|
| 64 KiB | 37 ms | 25 ms |
| 256 KiB | 114 ms | 95 ms |
| 1 MiB | 398 ms | 387 ms |
| 4 MiB | 1.55 s | 1.54 s |
| 16 MiB (capture bound) | unavailable (deadline) | — |

A whole scan costs about 0.38 s per MiB of host bytes. The 5 s deadline
therefore binds at about 13 MiB of distinct host bytes, before the 32 MiB
parse budget does. Beyond that the scan is unavailable: it fails closed, and
`ctx anchors scan` names the deadline. ADR 0012 §4's "≈ 3 s for 32 MiB" was
a parse-only estimate; r0.5 corrects it.

## Reproduction

```sh
cargo build --release -p contextdb-hooks -p contextdb-bench
target/release/symbol_bench "$PWD/target/release/ctx-hook"
```
