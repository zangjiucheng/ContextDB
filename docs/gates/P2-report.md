# P2 implementation report — 2026-10-20

**Gate passed** on `21da1f7`, after the independent review and two closure checks. Remote Linux/macOS CI (run 39497969575) and the nightly exhaustive, four-shard mutation and durability workflow (run 39498455621) both passed. See [P2.md](P2.md). Related documents:
- [gate matrix](P2.md)
- [durable-log ADR 0006](../adr/0006-p2-durable-log.md), whose §12 records where the implementation differs from the text
- [incremental structural commits ADR 0007](../adr/0007-incremental-structural-commits.md)

## Implemented behavior

**op_log.** The op_log is an own append-only segment log and the single source of truth.
- Each frame holds one complete committed batch: length, length check, body, and a blake3 checksum chained to the previous frame.
- Segment headers carry the format version and the previous checksum.
- Durable decoding has explicit limits (`DecodeLimits::DURABLE`).

**Commit order** (ADR 0006 §3):
1. idempotency lookup (TxnIdReuse);
2. per-Op validation, with MissingBlob at the per-Op slot;
3. append and sync (F_FULLFSYNC on macOS);
4. publish, then projections.

Any persistence I/O failure poisons the store (fail-stop). A poisoned store refuses `db()`, every read mode, DIFF, `read_row`, `get_blob` and `txn_status`. `epoch()` and `projection_watermark()` still answer, and report only durable state. Reopening the same directory in the same process returns `RestartRequired`. A directory lock admits one writer, and the offline tools (`doctor --truncate-before`, `migrate`) take the same lock.

**Open:**
1. Take the lock and remove temp blobs.
2. Refuse an interrupted migration (`MigrationIncomplete`); operator opens never create a store (`NotAStore`).
3. Verify the chain.
   - A final frame with a crash-type fault (incomplete, length check, checksum) is truncated and reported (T34), or fails closed with `--strict-tail`.
   - A frame whose checksum verifies but which is otherwise wrong fails closed even at the tail.
   - An earlier broken frame fails closed (T33).
   - A final segment with an invalid header is removed only when it contains no valid frame.
4. Load the newest snapshot that verifies and that the core can rebuild.
5. Replay with a CommitResult comparison (`ReplayDivergence`), rolling projections forward in one SQLite transaction.
6. Rebuild the id index.
7. Run the 1,000-revision sampled self-check against the per-node oracle definition.

**Projections.** `diff_log`, `head_history` and `base_history` live in SQLite, with a watermark that names the log checksum. They accept contiguous epochs only. Reads use, in order:
1. in-memory history;
2. projections, when the watermark covers the epoch;
3. log replay.

All three go through one shared state function.

**Operator tools.** `ctx`: `replay`, `doctor` (full oracle, blob check, `--rebuild`, `--truncate-before E [--yes]`), `snapshot`, `migrate` and `export`.

**Incremental structural commits** (ADR 0007). Without them, cold replay was quadratic: 20,000 single-create commits took 44 s.

## G2 / G3 / G4

| Check | Result |
|---|---|
| T07b, T13, T18, T18b, T33, T34, T35, T68 | pass (tests listed in [P2.md](P2.md)) |
| PROP10, store: reopen (half the seeds with a snapshot), digest and DIFF for all epoch pairs | 10,000 seeds pass |
| PROP10, core: `from_inputs` == incremental, identical later results/errors, DIFF, oracle | 10,000 seeds pass |
| Read modes reproduce past states | 1,000 seeds pass |
| G4 pre-WAL / post-WAL-pre-sync / post-sync-pre-publish | 1,000 effective injections each. Post-sync-pre-publish uses a hook: the frame is synced, then the commit fails unpublished. Four tail fates cycle through every point, but they only change bytes where unsynced data exists: post-WAL-pre-sync, and segment creation. Half the seeds use 512-byte segments (rollover) |
| G4 mid-snapshot, blob put (4 steps each) | 1,004 runs each |
| G4 projection write (SQLite transaction fails before commit) | 1,000 runs |
| Harness self-test (no-op sync) | detected |

After every G4 crash the harness checks five things:
- I5(c), `last_acked ≤ recovered ≤ last_acked + 1`, with a synced frame never lost and an unwritten one never recovered;
- the recovered digest equals the committed-prefix reference;
- derived state equals Jacobi(inputs);
- every acked id is `Committed` with its original result;
- the in-flight id is `Committed` or `NotCommitted`, and its retry applies at most once.

Before the crash, the poisoned store must report only the durable epoch, refuse `db()` and refuse `txn_status`.

## G5 (initial targets)

Apple M1 Pro, macOS 27.0.1, APFS internal SSD, Rust 1.95.0 release, one thread ([p2-env.json](p2-env.json)).

Ranges cover eight runs of `store_bench` on the final code (`21da1f7`): four of mine and four by the independent reviewer.

| Metric | Target | Measured (min–max) | Verdict |
|---|---:|---:|---|
| Single-transaction commit p99, fullfsync on | ≤ 25 ms | 8.66–16.97 ms | PASS |
| Single-transaction commit p99, fullfsync off (fsync(2), measurement only) | ≤ 10 ms | 0.16–0.41 ms | PASS |
| Commit p99, fullfsync on, production file projections (`ctx` configuration) | ≤ 25 ms | 8.01–14.76 ms | PASS |
| Cold open, full replay of 100,000 structural transactions | ≤ 5 s | 0.490–0.620 s (five runs; ≤ 1.33 s before the bulk projection fix) | PASS |
| Cold open, full replay, file projections written for all 100K epochs (first open / after `--rebuild`) | ≤ 5 s | 0.543–0.621 s | PASS |
| Cold open, full replay, file projections already current | ≤ 5 s | 0.257–0.333 s | PASS |
| Cold open with a snapshot at the tail, memory projections | ≤ 1 s | 0.157–0.174 s | PASS |
| Cold open with a snapshot at the tail, file projections | ≤ 1 s | 0.158–0.171 s | PASS |
| Historical read at epoch 50,000 below the snapshot: log replay / projections (informational) | — | 785–872 ms / < 1 ms | — |

Notes:
- The replay log is built without syncs (build 4.8 s). Open is measured with the production `StdDisk`, and includes the lock, blob scan, chain verification, replay and self-check.
- Every transaction is structural (each one creates a logical, `structural_every=1`). This is the costly case for replay; with mixed workloads, fast-path base toggles replay faster.
- Fullfsync-on p99 varies with APFS flush latency: 8.7–17.6 ms across thirteen runs on this machine (seven of mine, six of the reviewer's). Every run met the target.
- Before the review's fix (ranged frame reads), reads below a snapshot reread a whole segment per frame. The reviewer measured 469 ms at 10K epochs, quadratic growth, and about 50 s extrapolated at 100K.
- The first open with file projections took 6.3 s with one SQLite transaction and freshly prepared statements per epoch. A single transaction brought it to 4.4–6.1 s, which the closure check showed was at the limit (review N1). Cached statements and one watermark write per bulk transaction brought it to 0.55–0.61 s. The same change sped up plain replay, whose in-memory projection writes had used the per-epoch path too.

## G7 regression

| Suite | Result |
|---|---|
| `bash scripts/check.sh` (fmt, Clippy -D warnings, all tests, compile-fail, dependency policy negatives, cargo-deny) | pass |
| P0 52 + P1 24 tests, both exact modes; paranoid 10,000 seeds / 120,000 transactions | pass |
| Exhaustive depth 6 (gate): 770 states, 11,275 transitions, 0 violations | pass |
| Exhaustive depth 8 (report): 1,711 states, 30,350 transitions, 0 violations | pass |
| Mutation: counting, txn, stage, arena, memory | 0 survivors, 0 timeouts, no waivers ([history](P1-mutations.md), [summary](p2-mutation-summary.json)) |
| P1 G5 timing, multi_support / hub | commit p99 4.5 / 1.0 µs; CURRENT 0.042 / 0.042 µs; WHY depth 3 3.96 / 0.25 µs |
| P1 G5 memory (ADR 0005), multi_support / hub | hot 63.17 / 67.06; inputs 47.17 / 48.68; query cache 42.91 / 44.09; history 52.65 / 52.65; live heap 184.89 / 194.63 B/inc: all PASS |
| Structural commit p99 at 100K revisions (informational) | 39.0 / 5.0 µs |

The live heap rose from 181.15 / 188.87 B/inc at the P1 gate. The rise comes from the `revisions_by_logical` index and the adjacency overflow rows, both counted in the hot scope.

## Known limitations and deviations

Details are in ADR 0006 §12.
- **Migration fixtures.** There is one frozen format-0 fixture rather than the two ADR 0006 §10 named. The "no-op" framework path is the migration of a current log.
- **Migration crash.** The state left by a crash between the two renames of `migrate` is reproduced by a regression test; open refuses it, and `migrate` resumes. `SimDisk::rename_dir` itself is atomic, so a rename lost for want of a parent sync is not injected.
- **Projections after a snapshot.** Projections stay behind when open starts from a newer snapshot. Reads below the snapshot then fall back to log replay (0.8 s at 50K epochs) until `doctor` catches up. The P3 daemon will catch up in the background.
- **SQLite projections are not crash-injected.** They live on the real filesystem, and the G4 projection test fails the SQLite transaction instead of crashing it. SQLite's own atomicity plus the checksum-named watermark is relied on.
- **Blob layout** is flat (`blobs/<64 hex>`), not ADR 0006 §5's two-level fan-out.
- **Not implemented** (no gate depends on these): an `inputs_at` cache, bounding the in-memory history, and a store memory report covering the id index and `BlobPresence`.
- **No signed head.** The hash chain detects corruption but not a full rewrite. Signing arrives with principals in P9.
- **No erasure** before P9.

## Independent review

A separate non-implementing reviewer (SPEC §4.2) reviewed commit `341035e`. They reproduced findings 1–6 by executing probes, and verified the following as accurate:
- the G5 numbers;
- commit order;
- earlier-frame fail-closed, including across segments;
- T34 and `--strict-tail`;
- preservation of migration's `acked_result`;
- projection watermark validation;
- the ADR 0007 overlay, with no defect found.

Every finding was fixed and has a `review*` regression test in `crates/contextdb-store/tests/recovery.rs`.

| # | Finding | Severity | Fix |
|---|---|---|---|
| 1 | A corrupt header on the final segment silently deleted every acked frame in it, even with `--strict-tail` | blocker | Removed only if no valid frame exists anywhere in the segment; otherwise `ChainBroken{SegmentHeader}`; strict refuses even the empty case |
| 2 | The `doctor --truncate-before` plan under-reported loss (frames after corruption, later segments, boundary cuts) | major | Plan resynchronizes and lists every readable dropped frame across segments; `unreadable_tail` flags hidden loss; test checks exact lists for every cut over ≥ 3 segments |
| 3 | A crash between `migrate`'s renames produced a silently empty, writable store | major | Open refuses with `MigrationIncomplete`; `migrate` resumes after verifying `log.migrating`; SimDisk `list` now shows subdirectories, like `read_dir` |
| 4 | Offline tools bypassed the directory lock; `--rebuild` deleted files before locking | major | `truncate_before` and `migrate` take the lock; `truncate_before` re-plans (`PlanOutdated`); `--rebuild` clears projections under the lock; the CLI no longer deletes projection files |
| 5 | A poisoned store served the non-durable epoch and answered `txn_status` | major | Reads, `db()` and `txn_status` return `Poisoned`; `epoch()` is the durable epoch; G4 asserts it after each injection |
| 6 | `read_row` read the whole segment per frame (quadratic reads below a snapshot) | major | `Durability::read_range` reads one frame. The only guard is the informational bench line (788–817 ms at 50K); P3 sets read budgets |
| 7 | A checksum-valid final frame with a semantic fault was truncated as a torn tail | minor | Only incomplete, length-check and checksum faults count as torn |
| 8 | A verified but unbuildable snapshot made open fail | minor | Ignored and reported; replay starts earlier |
| 9 | Poison was per process, not per store | minor | Keyed by (disk instance, directory) |
| 10 | Post-sync-pre-publish had no hook; tail fates are mostly inert; rollover was never exercised | minor | Real `crash_after_next_sync` hook; odd G4 seeds use 512-byte segments; fate coverage documented (ADR 0006 §12) |
| 11 | G5 was not measured in the production configuration (file projections) | minor | Bench now gates commit p99 and cold opens with file projections; roll-forward is batched (6.3 s → 4.4 s) |
| 12 | A mistyped path created an empty store; the blob layout differed from the ADR | minor | `OpenOptions::create = false` for `ctx` (`NotAStore`); flat layout recorded in ADR 0006 §12 |

**Closure check** (same reviewer, commit `4659641`):
- All 12 findings are closed, with executed evidence.
- Reverting each fix in a copy made its `review*` test fail, so the tests are not vacuous.

New findings from the closure check:

| # | Finding | Severity | Fix |
|---|---|---|---|
| N1 | First open with file projections took 4.9–6.1 s; the 4.442 s PASS did not reproduce | major | Cached statements, no per-epoch savepoint in bulk mode, one watermark write per bulk: 0.55–0.61 s over four runs; the table reports ranges |
| N2 | The re-check for an interrupted migration under the lock could never fire (`log/` was created first) | minor | Open takes the lock before creating `log/`, then re-checks |
| N3 | `migrate` resume took the first backup in sort order; `log.migrating`'s entry was not parent-synced before the swap | minor | Picks the highest-version backup; syncs the parent directory before the first rename |
| N4 | `get_blob` was not gated on poison | minor | Returns `Result`; `Poisoned` after a failure (asserted in `review5`) |
| N5 | Rolled-forward projection rows were committed before the self-check | minor | `end_bulk` runs after the self-check |
| N6 | `catch_up_projections` was not bulked (≈ 9 s at 100K) | minor | One bulk transaction; on failure it rolls back and restores the watermark (`review_n6_*` checks the rollback, not the speedup). Catch-up at 50K epochs: 4.5 s → 1.05 s |

**Second closure check** (same reviewer, commit `21da1f7`): N1–N6 are all closed, and the projection rewrite introduced no regression.
- Gate-setting runs pass: 29 recovery, 2 migration, 1 CLI and 9 probe tests.
- All four `store_bench` runs give `g5_initial_pass=true`.
- One docs defect (a leftover table header in G5) has been fixed.
- N2, N3 and N5 are verified by code reading only: no test injects a migration that appears between the two checks, or a self-check failure.

## Reproduce

```sh
bash scripts/check.sh
cargo test --locked -p contextdb-testkit --features contextdb-core/paranoid
CONTEXTDB_G4_RUNS=1000 CONTEXTDB_PROP10_SEEDS=10000 cargo test --release --locked -p contextdb-store
cargo test --release --locked -p contextdb-testkit --test p2_rebuild
cargo test --release --locked -p contextdb-testkit --features contextdb-core/paranoid --test exhaustive -- --ignored --nocapture
CONTEXTDB_EXHAUSTIVE_DEPTH=8 cargo test --release --locked -p contextdb-testkit --features contextdb-core/paranoid --test exhaustive -- --ignored --nocapture
bash scripts/mutation.sh
cargo run --release --locked -p contextdb-bench -- multi_support 100000 1000
cargo run --release --locked -p contextdb-bench -- hub 100000 1000
cargo run --release --locked -p contextdb-bench --bin store_bench
```
