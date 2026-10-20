# P2 implementation report — 2026-10-20

**Every local gate layer passes.** The gate is not yet passed: independent review, remote Linux/macOS CI and the nightly workflow are still needed. Related documents:
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

Any persistence I/O failure poisons the store (fail-stop). Reopening in the same process returns `RestartRequired`. A directory lock admits one writer.

**Open:**
1. Take the lock and remove temp blobs.
2. Verify the chain.
   - A broken final frame is truncated and reported (T34), or fails closed with `--strict-tail`.
   - A broken earlier frame fails closed (T33).
3. Load the newest valid snapshot.
4. Replay with a CommitResult comparison (`ReplayDivergence`).
5. Rebuild the id index.
6. Run the 1,000-revision sampled self-check against the per-node oracle definition.

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
| G4 pre-WAL / post-WAL-pre-sync / post-sync-pre-publish | 1,000 effective injections each; four tail fates (torn, zero-filled, garbage, out-of-order) |
| G4 mid-snapshot, blob put (4 steps each) | 1,004 runs each |
| G4 projection write (SQLite transaction fails before commit) | 1,000 runs |
| Harness self-test (no-op sync) | detected |

After every G4 crash the harness checks five things:
- I5(c), `last_acked ≤ recovered ≤ last_acked + 1`, with a synced frame never lost and an unwritten one never recovered;
- the recovered digest equals the committed-prefix reference;
- derived state equals Jacobi(inputs);
- every acked id is `Committed` with its original result;
- the in-flight id is `Committed` or `NotCommitted`, and its retry applies at most once.

## G5 (initial targets)

Apple M1 Pro, macOS 27.0.1, APFS internal SSD, Rust 1.95.0 release, one thread ([p2-env.json](p2-env.json)).

| Metric | Target | Measured | Verdict |
|---|---:|---:|---|
| Single-transaction commit p99, fullfsync on | ≤ 25 ms | 13.22 ms | PASS |
| Single-transaction commit p99, fullfsync off (fsync(2), measurement only) | ≤ 10 ms | 0.49 ms | PASS |
| Cold open, full replay of 100,000 structural transactions | ≤ 5 s | 1.245 s | PASS |
| Cold open with a snapshot at the tail | ≤ 1 s | 0.158 s | PASS |

Notes:
- The replay log is built without syncs (build 4.8 s). Open is measured with the production `StdDisk`, and includes the lock, blob scan, chain verification, replay and self-check.
- Every transaction is structural (each one creates a logical, `structural_every=1`). This is the costly case for replay; with mixed workloads, fast-path base toggles replay faster.

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
- **SimDisk renames.** `SimDisk::rename_dir` does not model a lost directory rename, so a crash between the two renames of `migrate` is not injected. Migration is an explicit offline operation that keeps a backup.
- **Projections after a snapshot.** Projections stay behind when open starts from a newer snapshot. Reads fall back to log replay until `doctor` catches up.
- **Not implemented** (no gate depends on these): an `inputs_at` cache, bounding the in-memory history, and a store memory report covering the id index and `BlobPresence`.
- **No signed head.** The hash chain detects corruption but not a full rewrite. Signing arrives with principals in P9.
- **No erasure** before P9.

## Independent review

Pending: a separate non-implementing reviewer per SPEC §4.2.

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
