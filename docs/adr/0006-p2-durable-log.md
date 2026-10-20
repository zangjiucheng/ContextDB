# ADR 0006 — P2 durable op_log, ordering and recovery

Status: proposed (P2 G0 prerequisite per SPEC §5 P2 / §7 / review R09), 2026-10-20.
Becomes accepted after independent review; no P2 gate claim depends on it before then.

## 1. Storage engine

SQLite through `rusqlite` with the bundled SQLite (store crate only; core stays
dependency-restricted). `journal_mode=WAL`, `synchronous=FULL`. On macOS also
`fullfsync=ON` and `checkpoint_fullfsync=ON`. The durability claim is made only for
F_FULLFSYNC; reports give both on and off latencies (SPEC P2 G5).

A `meta` table records `format_version` (initially 1), `created_by` and the genesis
checksum domain. Opening a store whose format is newer than the binary fails closed.
Opening an older format runs a registered, tested migration (T68).

## 2. Canonical encodings

- `batch_blob` = `Wire` encoding of `Batch` (EventLog v0 layout, docs/EVENTLOG.md).
  Revision payloads appear only as ContentHash; PolicyDoc is inline (SPEC §7).
- `result_blob` = `Wire` encoding of `CommitResult` (epoch, allocations, net_delta,
  l0_flips, status_diff). This adds `Wire` impls for `NetDelta` and `CommitResult` (append-only model change).
- `batch_hash` = blake3(batch_blob).
- `checksum` = blake3(`"contextdb.op_log.v1"` ‖ prev_checksum ‖ epoch u64 LE ‖
  branch u32 LE ‖ principal u32 LE ‖ channel u8 ‖ Option(client_txn_id) ‖
  batch_hash ‖ blake3(result_blob)). Genesis prev_checksum = 32 zero bytes.
  Every row field participates, so editing any column breaks the chain (T33).

## 3. Commit order and publication

The Store owns the Db. Readers see Db state only through the Store, and the
Store publishes a commit only after it is durable.

1. Every revision payload referenced by the batch must already be in `blobs/`;
   otherwise `MissingBlob{content}` is returned at the SPEC §6.2 check-order position.
   Blob write: temp file → fsync → rename to `blobs/<2 hex>/<62 hex>` → fsync directory.
   An identical hash is a no-op (T35).
2. Idempotency check (§6.1 contract 1): a `client_txn_id` already present with an
   equal batch_hash returns the stored CommitResult without applying anything.
   A different hash returns `TxnIdReuse`. Lookups use an in-memory map rebuilt at open.
3. Apply to the in-memory Db (staged validation, rollback on error). Rejected
   batches are never logged.
4. Append exactly one op_log row in one SQLite transaction and COMMIT, which syncs per §1.
5. Only then return the result (the ack) and make the new epoch readable.

If step 4 fails after step 3 has mutated memory, the Store is **poisoned**. Every later
read and write returns an error until the store is reopened from op_log. A non-durable
epoch is therefore never observable (I5(b)). Projection tables (§7: heads,
head_history, base_history, diff_log, …) are written in the same SQLite transaction
as the op_log row. They are rebuildable caches (I8) and never a source of truth.

## 4. Recovery

Open = SQLite WAL recovery (incomplete WAL frames are discarded, which yields a
durable prefix), then:
1. Verify `meta`, then the hash chain from genesis or from the last verified snapshot.
2. Load the newest snapshot whose hash and epoch match op_log. Otherwise start empty.
3. Replay later rows through the ordinary commit path. The replayed CommitResult
   must equal `result_blob` byte for byte; any mismatch fails closed (`ReplayDivergence{epoch}`).
4. Rebuild the in-memory client_txn_id map; sample nodes against the oracle
   (`ctx doctor` does a full check). Any mismatch fails closed.

Chain or checksum failure on any row fails closed with `ChainBroken{epoch}` (T33).
SQLite rows are atomic, so a torn final row cannot exist. A final row that fails its
checksum is reported as `TailCorrupt{epoch}` and the store does not open.
`ctx doctor --truncate-tail` is the explicit operator action: it removes the
trailing invalid rows, reports the epochs it dropped, and then opens normally (T34).
The store never truncates automatically: a bad tail can be either tampering or
corruption, and the store cannot tell which.

## 5. Missing blobs

Replay never reads payloads (op_log carries only hashes). A missing blob at read time
reports payload state `Unavailable(Missing)`. `Shredded` and `Collected` are reserved
for P9 and P10. `ctx doctor` lists revisions whose blob is missing. Derived state is
unaffected (SPEC §7).

## 6. Snapshots

A snapshot is `Wire(Inputs)` plus epoch plus the op_log checksum at that epoch plus a
blake3 of the file. Writes go to a temp file, fsync, rename, fsync dir (crash point
"mid-snapshot"). A snapshot is only an accelerator: a corrupt or mismatched snapshot
is ignored and full replay is used. This requires a core constructor that rebuilds
indexes, counters and topology from Inputs at a given epoch, verified against the
oracle (PROP10).

## 7. History and txn_status

- `diff_log(epoch, revision, post_state_blob)` persists the post-states that the
  in-memory history already keeps (ADR 0005), so HISTORICAL / SNAPSHOT / DIFF work
  across restarts. After open, the in-memory history may be empty; DIFF reads diff_log.
- `txn_status(id)` returns `Committed{epoch, result}` or `NotCommitted`
  (SPEC §6.1 contract 1). The word `Unknown` in the P2 G4 text is replaced by
  `NotCommitted` (SPEC v1.4 review note). An id that was in flight at crash time and
  is not durable is NotCommitted, so retrying it is safe.

## 8. Crash injection

Store operations go through a `Durability` trait with injectable failure points:
pre-WAL, post-WAL-pre-sync, post-sync-pre-publish, mid-snapshot. Tests simulate a
crash by dropping the process-level state at the point and reopening the directory.
Each point runs ≥ 1,000 seeded runs (G4), checking I5(c), no partial transaction,
replay == oracle, and idempotent retry.
