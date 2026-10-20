# ADR 0006 — P2 durable op_log, ordering and recovery

Status: **Accepted (revision 3)**, 2026-10-20, after three rounds of independent review.
Remaining minors R1–R4 from the final pass are folded in.
- Revision 1: 2 blockers, 7 majors, 8 minors.
- Revision 2: accepted subject to 4 majors (N1–N4) and 10 minors (n1–n10).

§11 maps every finding to its fix. The storage decision in §1 belongs to the
project owner, who chose an own append-only log on 2026-10-20. SPEC v1.4.3 records it.

## 1. Where the truth lives

- **op_log is our own append-only segment log** under `<dir>/log/`. SQLite is not used for it.
  - All log I/O goes through a `Durability` trait: create, append, sync, rename, sync_dir, read, truncate, lock.
  - Production uses std files.
  - Tests use a simulated disk that tracks *synced* versus *unsynced* bytes per file and per directory entry (§9). This is the only way crash tests can tell a missing fsync from a durable write (review M1).
- **Projections live in SQLite (WAL) and can always be rebuilt (I8):** `logicals`, `revisions`, `heads`, `head_history`, `base`, `base_history`, `rules`, `rule_enabled`, `incidences`, `validation_records`, `policies`, `watermarks`, `artifacts` and `diff_log`.
  - Projections are written *after* the log frame is durable, together with a single-row `projection_watermark(epoch)` in the same SQLite transaction.
  - At open, a projection whose watermark is below the log tail is rolled forward from the log. A projection with a mismatched or missing watermark is rebuilt from scratch.
  - Projections are never consulted for truth or for replay.
- **Payload blobs** live at `<dir>/blobs/<2 hex>/<62 hex>`, content-addressed by blake3.
- **Dependencies** (store crate only; core stays restricted): `rusqlite` with bundled SQLite (public domain / MIT) for projections, and `nix` for the "fullfsync off" measurement mode. No unsafe code in our crates. `config/dependency-policy.toml` gains a `[store]` note recording the review.

## 2. Durable format

**Segment files.** Files are named `seg-<first_epoch:020>.log`. Each starts with a header:
- magic `CDBLOG\0\x01`
- `format_version` u32 (starts at 1)
- `first_epoch` u64
- `prev_checksum` [32]

A new segment starts after a frame once the segment exceeds 64 MiB. A single frame may be larger than that.

**Frames.** A frame is `len: u64 LE` ‖ `len_check: [8]` ‖ `body` (len bytes) ‖ `checksum: [32]`.
- `len_check` = blake3("contextdb.frame.len" ‖ len)[..8]. A corrupted length can never be mistaken for a long frame running past EOF.
- `body` is the durable encoding of `LogRow{ epoch, branch, principal, channel, client_txn_id: Option<[u8;16]>, batch_hash, batch_blob: Vec<u8>, result_blob: Vec<u8>, prev_checksum }`. Wire Option = presence byte, then 16 raw bytes. The `batch_blob` and `result_blob` byte fields carry **u64** lengths. Every other field uses the ordinary Wire encoding.
- `batch_blob` = Wire(Batch), i.e. the EventLog v0 layout. Revision payloads appear only as hashes; PolicyDoc is inline (SPEC §7).
- `result_blob` = Wire(CommitResult). This requires new Wire impls for `NetDelta` and `CommitResult`, an append-only model change.
- `batch_hash` = blake3(batch_blob).
- `checksum` = blake3("contextdb.op_log.v1" ‖ body). The genesis `prev_checksum` is 32 zero bytes.

**Durable decoding (review B2, N4).**
- **Limits as a parameter.** `Wire::decode` gains a `DecodeLimits` context threaded through every nested decode, replacing the hard-coded 16 MiB / 1,000,000-element caps in wire.rs.
  - `DecodeLimits::EVENTLOG` keeps today's import limits unchanged.
  - `DecodeLimits::durable(frame_len)` bounds each element count by the bytes remaining, with no fixed cap.
- **Writer side.**
  - Before applying a batch, the store checks that every count in Wire(Batch) fits its u32 length field. Otherwise it returns `CapacityExceeded{kind: Encoding}`, a new `CapacityKind` variant (catalogued in SPEC §6.2 as a batch-level check after idempotency and before per-Op checks).
  - CommitResult counts are bounded by the u32 ID spaces, so they always fit.
  - The blob fields use u64 lengths.
- Therefore every frame the writer produces decodes, and an acked batch can never make the store unopenable.
- Tests encode and decode frames above both EventLog limits, plus one at the encoding boundary.
- Snapshots (§6) use the durable decoder.

**Verification at open (review M4).** Open checks every frame:
1. Recompute the checksum.
2. `prev_checksum` equals the previous frame's checksum (or the segment header's / genesis).
3. Epochs are contiguous from 1.
4. `batch_hash` == blake3(batch_blob).
5. The redundant branch, principal, channel and client_txn_id fields equal the values decoded from batch_blob.
6. Decoded `result.epoch` == row epoch.
7. Segment headers chain correctly.

**Threat model.** The chain is unkeyed. It detects accidental corruption and inconsistent edits. It cannot detect deletion of trailing frames together with their checksums, or a full rewrite that recomputes every checksum. Detecting those needs an external anchor (signed head), which is deferred to P9 (principals/ACL). The P2 claim is therefore "integrity against corruption and partial tampering", not "tamper-evident".

## 3. Commit order and publication

The Store owns the Db and runs commits under a single writer mutex. Reads take the same lock or read the last published snapshot, so they never observe steps 3–5 in progress (review M2).

0. **Exclusive directory lock** (`File::lock` on `<dir>/LOCK`) is taken at open. A second process (the daemon plus `ctx doctor`, for example) fails to open.
1. **Idempotency first (review B1).** If `client_txn_id` is in the id index:
   - equal batch_hash → return the stored CommitResult, read from the log frame (its `acked_result` if present, see §7);
   - different hash → `TxnIdReuse{id}`.

   No other validation runs. A blob shredded after the original commit therefore does not change the retry answer. Ids of rejected batches are never recorded (§6.1 contract 1).
2. **Apply in core** with a `BlobPresence` input. Core `commit` gains a presence parameter; P0/P1 callers and replay pass `AllPresent`.
   - **Placement.** `MissingBlob{content}` is checked inside `txn::apply` at its SPEC §6.2 per-Op slot (after InvalidPolicy, before InvalidMetadata). Error equivalence and "first failing Op wins" are preserved.
   - **Fields checked.** `CreateLogical.content` and `AddRevision.content`, at every occurrence, dedup hits included, so the check never depends on state.
   - **Not checked.** `OpenArtifact.payload_hash` is not a revision payload; it is out of scope until P7 defines artifact payload storage.
   - **Oracle.** The oracle's InputModel takes the same presence set for P2 error-equivalence tests.
   - **When a blob counts as present.** Only after its `put` has returned, which happens after its directory fsync (§5).
3. **Encode and append** one frame, then `sync` (F_FULLFSYNC on macOS through std `sync_all`; fdatasync elsewhere). A new segment also syncs its directory.
4. **Publish.** Update the id index, make the epoch readable, return the ack, then write projections (§1).
5. **Fail-stop (review M2).** Any I/O error in step 3, or a panic in step 2 or 3, poisons the Store.
   - Every later call returns `Poisoned`, and the daemon process exits.
   - Reopening in the same process is refused (`RestartRequired`). Recovery happens only in a fresh process, so page-cache frames that never reached disk cannot be resurrected from memory.
   - A durability error is never retried in place.
   - A projection write failure after step 3 does not poison. The commit is durable; the projection is marked stale and rolled forward at the next open.

## 4. Recovery and T33/T34

Open performs these steps:
1. Take the lock (§3.0) and read the segments in order.
2. **Tail classification (review N1).** Only one frame can be unsynced at a time: commits run under the single writer mutex, and each frame is synced before the ack. So only the **final frame of the final segment** may fail verification as a result of a crash.
   - **Final frame or trailing bytes fail verification.** This covers: incomplete header, `len_check` mismatch, declared end past EOF, checksum mismatch, zero-filled, garbage or out-of-order unsynced pages. Open truncates to the end of the last verified frame, syncs, and reports `TruncatedTail{after_epoch, bytes, reason}` in the open report and in `ctx doctor` (SPEC T34).
   - **Residual risk.** Media corruption of an *acked* final frame after its sync looks the same as a torn write. In that case exactly one acked frame is lost, and the report says so. `open --strict-tail` fails closed instead, for deployments that prefer refusing to open.
   - **Final segment with an invalid or partial header and no verified frames.** It is removed and reported. A segment's header is written, synced and directory-synced before its first frame, so no acked frame can be lost this way.
3. **Earlier frame fails verification (review M3).** Any frame *before* the final one that fails a §2 check fails closed with `ChainBroken{epoch}` (T33). The store does not open.
   - `ctx doctor --truncate-before <epoch>` is the only remedy. It is explicit and requires confirmation. It drops frames whose epoch is ≥ `<epoch>`, so `<epoch> − 1` becomes the tail (review n5).
   - Before acting, it lists the acked epochs and client_txn_ids that will be lost. This is acked-data loss, called out as a violation of I5(a) that the operator accepts.
   - It truncates the log, removes projection and diff_log rows above the new tail, deletes snapshots beyond it, and the dropped ids become NotCommitted.
4. **Load the newest valid snapshot** (§6) at or below the tail. Otherwise start empty.
5. **Replay the remaining frames** through `Db::commit` with `AllPresent`, without the idempotency step and without appending.
   - The replayed CommitResult must equal the decoded `result_blob` as a value (`ReplayDivergence{epoch}` otherwise, fail closed); see §7 for the upgrade policy.
   - Replay never reads blobs, so shredded or missing blobs never block opening (SPEC §7).
6. **Rebuild the id index** from the frames.
7. **Sampled self-check (review m6, n7).** Take a 1,000-revision sample, seeded by the head checksum (all revisions if fewer).
   - For each sampled revision, evaluate the oracle's *per-node* definition against the recovered Inputs and the stored premise bits: support count = enabled rules whose premise literals all hold, plus the sat bits and the L0 bit. Compare with the counting state; any mismatch fails closed.
   - The graph is acyclic and every sampled node is checked against its own premises, so a node that passes is locally consistent with the oracle. This is the SPEC's "oracle 抽样".
   - `ctx doctor` runs the full oracle (Jacobi) and digest comparison instead.

## 5. Blobs

**Write** (`put`): temp file in the same directory → write → `sync` → rename → `sync_dir`, after which the put returns. If the target hash already exists, the put is a no-op (T35).

**Existing but corrupt (review R4).** If the target hash already exists, `put` verifies the existing bytes. A corrupt file is replaced through the temp-then-rename path. T35 still holds: a valid identical file is not duplicated.

**Presence (review n1).** `BlobPresence` is an in-memory set of hashes. It is built at open, after syncing every blob directory and deleting leftover temp files, and a hash is added only when its `put` returns. A file renamed by an unfinished put is therefore never counted as present. The set costs 32 B per blob and is counted in the store memory report.

**Read** (`get`) verifies blake3. It returns one of:
- `Available(bytes)`
- `Unavailable(Missing)` — the file is absent
- `Unavailable(Corrupt)` — the bytes don't match the hash
- `Unavailable(Shredded | Collected)` — reserved for P9 and P10

`Missing` and `Corrupt` are added to SPEC §7 (review m2). `ctx doctor` lists them. Derived state is never affected.

## 6. Snapshots

**File.** `snap-<epoch:020>.bin` contains:
- header: magic, format_version, epoch, the log checksum at that epoch, a body length;
- body: `Wire(Inputs)`;
- trailer: blake3 of the **body only** (review m7).

**Write.** Temp → sync → rename → sync_dir.

**Use.**
- A snapshot whose epoch or checksum does not match the log, or whose body hash fails, is ignored and reported. Full replay is used instead, so a snapshot is purely an accelerator.
- Loading requires a core constructor, `Db::from_inputs(inputs, epoch)`. It rebuilds arenas, indexes, counting state and PK. It must give the same derived state as a full replay (PROP10).
- PK ranks may differ from those of a replayed Db; see §7.

## 7. Determinism and upgrade policy (review M5)

- **Acked results survive upgrades (review n4).** For idempotent retries the store keeps the originally acked `result_blob`. A result-changing migration writes re-derived results into the new log's frames. It records the original acked result for each client_txn_id as an `acked_result: Option<Vec<u8>>` field of that frame's `LogRow`, so the record is chained, durable and part of op_log, never a projection (review R1, I8). The idempotency lookup (§3 step 1) returns `acked_result` when it is present, and the frame's `result_blob` otherwise. The §6.1 contract 1 promise ("return the original CommitResult") still holds.
- **Pure results.** CommitResult (and every TxError) must be a pure function of (Inputs, Batch, BlobPresence). It must not depend on PK ranks, memo/cache state, history or the order in which earlier batches were applied.
- **Today's code already satisfies this:**
  - flips are sorted;
  - fast-path deltas use ordered maps;
  - fast-path eligibility depends only on the batch;
  - results carry no stats or timing.
- **P2 property test.** For random histories, a Db restored from a snapshot and a Db produced by full replay return identical CommitResults and errors on the same subsequent random batches.
- **Upgrade policy.** A kernel change that alters any CommitResult for an existing log is a format change:
  - bump `format_version`;
  - ship a migration that re-derives `result_blob` and re-chains checksums into a new log;
  - keep the old log as a backup.

  Without that, the change is a ReplayDivergence bug.

## 8. History, HISTORICAL, SNAPSHOT and DIFF (review M6)

**`diff_log(epoch, revision, post_state_blob)`.** A projection holding exactly the post-states that the in-memory history records (ADR 0005).

**Read modes (review N3; SPEC v1.4.3 §6.1).**
- **`ReadMode::Snapshot(e)` on a logical target** → `(RevisionId, NodeStatus)`. It returns the logical's head *as of epoch e* and that revision's status as of e (the state a reader at e saw). The logical must exist at e; otherwise `UnknownRef`.
- **`ReadMode::Historical(e)` on a revision target** → `NodeStatus` as of e, plus payload state, whether or not the revision was the head at e. T07b (a superseded revision is still readable) is this mode. The revision must exist at e; otherwise `UnknownRef`.
- **Payload.** Payloads are immutable and not versioned. Their state is reported as of read time (`Available` or `Unavailable(..)`).
- **Byte stability (T13).** NodeStatus results are compared through their Wire encodings and must be byte-identical across repeated reads and restarts.
- **Input-layer snapshot.** The whole `Inputs` at e (`inputs_at(e)`) is an internal doctor and research export, not a read mode. It is computed from the nearest snapshot ≤ e plus replay. Any cache of it is bounded LRU, 64 MiB by default, and counted in memory (review n8).

**Sources and freshness (review N2).** A Snapshot or Historical read at epoch e is answered from the first source that is guaranteed complete for e:
1. in-memory history, when e ≥ the Db's initial epoch;
2. projections, only when `projection_watermark ≥ e`;
3. otherwise, replay of the log prefix.

All three use one shared state function, and a property test requires identical bytes from every source. Reads never fall back to a projection behind the watermark, so a stale projection can never surface.

**Writing projections.** Projections are written under the writer mutex, after the frame sync and before the lock is released (and therefore before the next commit).

**Roll-forward at open.**
- If the watermark is below the log tail, projections are rolled forward by replaying from the newest snapshot at or below the watermark (or from genesis), and post-states are written for epochs above the watermark.
- A snapshot newer than the watermark is not a valid roll-forward base for projections.

**`DIFF(a, b)`.**
- One shared endpoint-comparison function runs over `(epoch, revision, post)` rows from whichever source is complete for both endpoints, chosen in the same order as above.
- A property test requires the in-memory and diff_log answers to be identical.

**Memory.** The in-memory history may be bounded or dropped after open, because diff_log is durable. The id index is counted in the store memory report (review m3).

## 9. Crash injection (SPEC P2 G4)

**Simulated disk.** `SimDisk` implements `Durability`.
- Every write is unsynced until its file is synced.
- A rename or create is unsynced until its directory is synced.
- A crash keeps only synced state. The unsynced region of the final frame becomes one of these, chosen by a seeded random draw: a torn prefix, zero-filled pages, garbage pages, or pages persisted out of order. Each case must be classified as in §4 step 2.

**Injection points**, matching SPEC:
- **pre-WAL** — before the frame is written;
- **post-WAL-pre-sync** — written, not synced;
- **post-sync-pre-publish** — synced, before the ack and projections;
- **mid-snapshot** — at each step of the snapshot write.

**Additional points:** each blob-put step and each projection write.

**Checks.** Each point runs ≥ 1,000 seeded runs. After every crash, a fresh open must satisfy:
- I5(c), a durable prefix with `last_acked ≤ recovered ≤ last_durable`;
- no partial transaction;
- replayed derived state == oracle;
- every in-flight client_txn_id gives `Committed{epoch}` or `NotCommitted`;
- a retry never applies twice.

**Self-test of the harness.** A test variant whose `sync` is a no-op must make G4 fail. This proves the harness can detect a missing fsync.

**Real filesystem.** A separate smoke test exercises std files on a real filesystem (no fault injection) to check the production paths.

## 10. Formats, migrations and settings

- **T68 migration fixtures.** Format 1 is the first durable format. Two synthetic format-0 fixtures are checked in:
  1. a header-only change with a no-op migration, to test the framework;
  2. a different CommitResult encoding, whose migration re-derives results and re-chains checksums. After it, replay state must equal the original and acked results must be preserved (review n6).
- **Running migrations.** Migrations are explicit (`ctx migrate`); the old segments are kept as a backup, and opening an older format without migrating fails closed with `FormatMigrationRequired`. Opening a newer format fails closed.
- **Directories at init (review n5, n9).** `init` creates `<dir>`, `LOCK`, `log/` and `blobs/`, and syncs each parent directory. A smoke test confirms that directory `sync_all` works on macOS/APFS (F_FULLFSYNC on a directory fd) and on Linux.
- **Sync modes (review n10).**
  - macOS: "fullfsync on" is F_FULLFSYNC through std `sync_all`; "fullfsync off" is plain `fsync(2)` through `nix`, for measurement only.
  - Linux: fdatasync, with no on/off distinction.
- **Store errors (review n3).** A versioned `StoreError` covers every failure outside the §6.2 catalog. The MCP mapping is defined in P3.
  - `Tx(TxError)`
  - `Poisoned`
  - `RestartRequired`
  - `Locked`
  - `FormatMigrationRequired`
  - `FormatTooNew`
  - `ChainBroken{epoch}`
  - `ReplayDivergence{epoch}`
  - `Io(kind)`
  `TruncatedTail` is an open-report entry, not an error.
- **Recorded durability settings.** Open writes the effective settings into the open report and the bench `env.json`: sync mode (full / fdatasync / off-for-measurement-only), and SQLite projection pragmas read back after being set (review m5).

## 11. Risks and deferred items

- **G5 cold-start replay (review M7).**
  - Structural commits currently copy their inputs, which is O(N) per commit. Replaying 100K structural transactions without a snapshot may therefore be O(N²), above the 5 s target.
  - The P2 plan benchmarks this first. Remedies, in order:
    1. a bulk replay path producing identical CommitResults (checked by the §7 property);
    2. tuning snapshot frequency;
    3. calibrating the target by ADR, never by silent loosening.
- **Review mapping (revision 1):**
  - B1 → §3 steps 1–2 and §4 step 5
  - B2 → §2 "Durable decoding"
  - M1 → §1, §9
  - M2 → §3 steps 0, 5
  - M3 → §4 steps 2–3
  - M4 → §2 "Verification at open" and "Threat model"
  - M5 → §7
  - M6 → §8
  - M7 → this section
  - m1 → SPEC G4 already says NotCommitted
  - m2 → §5
  - m3 → §8 "Memory"
  - m4 → §10
  - m5 → §10
  - m6 → §4 step 7
  - m7 → §6
  - m8 → §1 "Dependencies"
- **Scope of I5 (review R2).** I5 covers crashes and power loss. Media corruption after a sync is handled by T33/T34 (§4 steps 2–3). SPEC §4 says so, and `--strict-tail` gives an unconditional fail-closed alternative.
- **Snapshot(e) head-at-e (review R3).** The in-memory source also keeps `net_delta.head_moves` per epoch as `(epoch, logical, rev)`, so it can answer head-at-e directly. The projection source uses `head_history`. Both go through the shared state function and are covered by the same property test.
- **EventLog export (review R4).** A durable batch larger than the EventLog v0 limits cannot be exported to EventLog v0. The exporter reports this as `ExportLimit{epoch}`; EventLog v1 will lift the limit.
- **Cold-start cost (review R4).** Building `BlobPresence` at open (O(blobs) plus blob directory syncs) is included in the G5 cold-start measurement.
- **Review mapping (revision 2):**
  - N1 → §2 frame `len_check`, §4 steps 2–3, §9 (SimDisk zero, garbage and out-of-order pages)
  - N2 → §8 sources, freshness and projection timing
  - N3 → §8 read modes, plus SPEC v1.4.3 §6.1
  - N4 → §2 durable decoding
  - n1 → §5 presence
  - n2 → SPEC Appendix B16 annotated
  - n3 → §10 store errors
  - n4 → §7 acked results
  - n5 → §4 step 3 and §10
  - n6 → §10 fixtures
  - n7 → §4 step 7
  - n8 → §8
  - n9, n10 → §10

## 12. Implementation notes (P2 gate, 2026-10-20)

These record where the implementation differs from or makes concrete the text above. None changes a SPEC guarantee.

- **One format-0 fixture (§10).** A single frozen format 0 is checked in (`migrate::legacy`): identical framing, and a `result_blob` without `status_diff`. Its migration re-derives every result, checks the legacy fields against the re-derived ones, re-chains checksums, and keeps the originally acked bytes as `acked_result`. T68 checks the replayed state, the preserved acked results for retries, and the `log.v0.bak` backup. A corrupt legacy log aborts the migration and leaves the old log untouched. The no-op path of the framework (migrating a log that is already current) returns `from == to` without rewriting anything. A second, header-only fixture would test nothing that these two paths do not, so it was not added.
- **Projection roll-forward (§8).** `Projections::apply` accepts only the next contiguous epoch. Each projection row set and its watermark are written in one SQLite savepoint.
  - At open, frames replayed above the watermark are applied, so a log replayed from genesis brings projections current. The whole roll-forward is one SQLite transaction; a crash before it commits leaves the old watermark.
  - When open starts from a snapshot newer than the watermark, projections stay behind. Reads at epochs above the watermark then use source 3 (log replay), which keeps open proportional to the tail after the snapshot.
  - `Store::catch_up_projections` (`ctx doctor`) fills the gap.
  - A failed projection write after a durable commit marks projections stale for the rest of the process; the commit is still acked.
- **Doctor truncation (§4 step 3).**
  - Projection files are not touched. A watermark above the new tail no longer names a frame of the verified log, so the next open clears and rebuilds the tables. A watermark at or below the tail stays valid.
  - The plan lists every readable frame that would be dropped: frames after an unreadable region (by resynchronizing on the next valid frame) and frames in later segments included. `unreadable_tail` says that unreadable bytes are dropped too, which may hide more acked epochs.
  - `truncate_before` takes the store lock and re-plans. A store that is open gives `Locked`, and a log that changed since the reviewed plan gives `PlanOutdated`.
- **Self-check (§4 step 7).** The sample is 1,000 revisions (all of them below that), drawn with xorshift seeded by the head checksum. `Db::verify_counting_sample` re-evaluates each sampled node's per-node definition: enabled rules with all premise literals true, the sat bits and the L0 bit. Any mismatch fails open with `SelfCheckFailed`. The open report counts the checked revisions.
- **Crash injection (§9).** Every listed point runs ≥ 1,000 effective injections in the gate configuration (`CONTEXTDB_G4_RUNS=1000`):
  - pre-WAL, post-WAL-pre-sync and post-sync-pre-publish, cycling the four tail fates;
  - mid-snapshot, at the write, sync, rename and sync_dir steps;
  - each blob-put step (write, sync, rename, sync_dir). A failed put fail-stops, the blob is afterwards absent or intact, and no temp file survives open;
  - the projection write. `Store::fail_next_projection_write`, a hidden test hook, fails the SQLite transaction before it commits. The test checks that the commit is acked, that reads are exact while stale, after reopening at the lagging watermark and after catching up.
- **Migration crashes.** `migrate` holds the store lock. `log.migrating` is complete and synced before the swap.
  - A crash before the first rename leaves the old log and a stale `log.migrating`, which the next run discards.
  - A crash between the two renames leaves no `log/` but leaves the backup and `log.migrating`. Open refuses this state with `MigrationIncomplete` instead of creating an empty log, and `ctx migrate` resumes by verifying `log.migrating` and finishing the second rename. A regression test reproduces this state.
  - `SimDisk::rename_dir` itself is atomic and does not model a rename lost for want of a parent sync.
- **Fixes from the independent P2 review** (each with a regression test in `tests/recovery.rs`, `review*`):
  1. A final segment whose header is invalid is removed only if it contains no structurally valid frame (an interrupted segment creation). Otherwise open fails closed with `ChainBroken{SegmentHeader}`, and `--strict-tail` refuses even the empty case.
  2. Final-frame faults are a torn tail only when a crash can cause them: incomplete, length check or checksum. A frame whose checksum verifies but whose chain, epoch, batch hash, redundant fields or encoding are wrong fails closed.
  3. A poisoned store serves nothing. `db()`, `txn_status`, `read_row`, the read modes and DIFF return `Poisoned`, and `epoch()` reports the last durable epoch. A client learns an in-flight id's fate only after restart.
  4. `read_row` reads one frame via `Durability::read_range`, not the whole segment. A Historical read 50,000 epochs below a snapshot takes 0.8 s, against an extrapolated 50 s before.
  5. A snapshot that verifies but cannot be rebuilt by the core is ignored like a corrupt one.
  6. The poison registry key is (disk instance, store directory).
  7. Operator opens (`ctx`) use `OpenOptions { create: false }`, so a mistyped path gives `NotAStore`. `doctor --rebuild` clears projections under the lock (`rebuild_projections`).
- **Crash-injection evidence (§9).**
  - Post-sync-pre-publish uses a real hook (`crash_after_next_sync`): the frame is synced, then the commit fails without publishing or acking.
  - After each injected failure, the harness checks that the poisoned store exposes neither the non-durable epoch nor a `txn_status`.
  - Odd seeds run with 512-byte segments, so crashes hit rollover, header-only segments and the cross-segment chain.
  - Tail fates only matter where unsynced bytes exist (post-WAL-pre-sync, and a crash during segment creation).
- **Blob layout.** Blobs are stored flat as `blobs/<64 hex>` rather than in the two-level `blobs/<2hex>/<62hex>` of §5. Fan-out directories are deferred to the blob GC work in P10.
- **Not implemented in P2 (no gate depends on them):**
  - a cache of `inputs_at(e)` (each use replays the prefix);
  - bounding the in-memory history after open;
  - a store memory report covering the id index and `BlobPresence`. P3's MCP memory budgets will include these.
