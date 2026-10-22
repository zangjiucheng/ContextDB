# ADR 0008 — P3 daemon, sessions, certificates and MCP

Status: **Accepted (revision 3.2)**, 2026-10-20, for P3 G0; **revision 3.3** adds the §17 G5 amendment (payload packs, ordered syncs, G5 calibration) and the P3 review fixes (§18). A non-implementing reviewer found 5 blockers, 14 majors and 12 minors in revision 1 (§14 maps them). In revision 2 it found 1 blocker, 7 majors and 9 minors, N1–N17 (§15 maps them). Revision 3 drew no blockers and four text-level majors, R1–R4, which revision 3.1 resolves (§16). The reviewer's confirmation closed R1–R4 and raised one more major, N-R3a, now amended. The reviewer stated that G0 is acceptable with that amendment and that no blockers remain.

## 0. Owner decisions (2026-10-20)

1. **Session log.** Sessions, read ledgers and certificates live in a session log of their own: a second append-only, hash-chained log in the store. op_log stays the only truth for kernel state. SPEC §7 and §3.5 #7 are amended (SPEC v1.5.0) so the session log is a separate truth for a disjoint domain that never feeds kernel state.
2. **MCP.** `ctx-mcp` is hand-rolled JSON-RPC over stdio, built on `serde_json`, without the `rmcp` SDK.
3. **`carry_rules`.** `ctx_supersede(carry_rules=copy)` copies the old revision's *own* justification rules, those concluding the old head, onto the new revision. Rules that depend on the old revision are never rebound or edited (SPEC §3.5 #2), and the owners of those rules must rebind them explicitly.
4. **Controlled validation (execution by authorized agent allowed; SPEC v1.6.4 §4.4).** In P3 only `ctx validate` over `admin.sock` may submit `RecordValidation` (SPEC §3.5 #14). The daemon enforces this: agent connections have no request that can produce one. `Record` and `Supersede` never emit it, `Validate` is admin-only and `Raw(Batch)` is unavailable on `agent.sock`. The kernel input contract (SPEC §6.1 contract 7) is unchanged, so P0 goldens and the oracle are unaffected. The MCP server offers no `ctx_validate` tool to agents in P3. P7 adds validator workers.
5. **Trust boundary.** P3 claims a cooperative boundary. Channel separation stops identity spoofing through MCP or request parameters. It does not stop a malicious agent with shell access as the same OS user from connecting to `observer.sock` or `admin.sock`. SPEC §10 and the docs say so. Hard separation arrives with principals and ACL in P9.

Scope: SPEC §5 P3, §6.1 read modes, §6.3, §6.4; SD13, SD14(b), SD15, SD16, SD18.3; I3–I6, I8.

## 1. Processes, crates and dependencies

- **`contextdb-daemon`** builds `ctxd`, one per data directory. It runs the commit actor (§3), the session-log writer (§4), the reader pool and the IPC server (§5).
- **`contextdb-mcp`** builds `ctx-mcp`, a thin stdio server holding one connection to `agent.sock` plus its session id.
- **`contextdb-core`** gains:
  - `cert.rs`, with closure, issuance and validation as pure functions;
  - counter-based L1/L2 (§7);
  - a per-epoch change record (§6.3).
- **`contextdb-store`** gains:
  - op_log format 2 with group framing (§3.2);
  - the session log (§4);
  - a shareable read-only `LogReader`.
- **`contextdb-cli`** gains `ctx init | status | check | why | why-not | impact | log | diff | validate`.
- **Dependencies:**
  - daemon and mcp: `serde`, `serde_json` with their proc-macro and runtime closure (`serde_derive`, `syn`, `quote`, `proc-macro2`, `unicode-ident`, `itoa`, `ryu`, `memchr`), and `arc-swap` for lock-free publication;
  - `nix` with the features `socket`, `user`, `process` and `fs`;
  - `loom` as a dev-dependency behind `cfg(loom)`.
  - Core stays restricted (no serde).
  - `config/dependency-policy.toml` gains `[daemon]` and `[mcp]` allowlists, and the dependency check enforces them.
  - Fuzz targets live in a separate `fuzz/` workspace with a pinned dated nightly in its own `rust-toolchain.toml`. The main workspace stays on 1.95.0.
- **Threads.** No async runtime. One accept thread per socket, one thread per connection, the commit actor, the session-log writer and a projection-maintenance thread. At 128 clients this is about 135 threads.

## 2. Single-instance lease (T37)

1. **Filesystem check.** The daemon refuses network or virtual filesystems by checking `statfs` on the data directory.
   - It rejects NFS, SMB/CIFS/smb2, AFP, WebDAV, 9p/v9fs (WSL2), virtiofs, FUSE variants (`fuse`, `fuseblk`, macFUSE) and WSL drvfs.
   - Linux matches `f_type` magic numbers; macOS matches `f_fstypename`.
   - The check is a pure function over the statfs result, so tests inject values.
   - Rejection gives `UnsupportedFilesystem{fs}`. `--allow-fs <name>` overrides it, and the override is recorded in the open report.
2. **Exclusion.** The store flock on `<dir>/LOCK` (ADR 0006 §3.0) is the only exclusion and fencing mechanism. A second daemon gets `Locked` (T37). Every write goes through the one actor of the process holding the flock.
3. **Capability probe.** A second file descriptor in the same process tries a non-blocking flock on `LOCK` and must see `WouldBlock`. flock is per open file description, so this detects a no-op flock.
4. **Lease record.** `<dir>/ctxd.lease` holds `{pid, process start time, boot id, socket paths, protocol version}`, written by temp file, sync and rename after the flock is taken. It is only a discovery record and is never used for exclusion (§3.5 #5). Clients treat a lease whose pid is dead, or whose start time differs, as "daemon not running".
5. **Stale sockets.** Socket files are unlinked and re-created only after the flock is held.
6. **Short socket paths.** If `<dir>/run/*.sock` would exceed the platform `sun_path` limit (104 bytes on macOS, 108 on Linux), the sockets go in a per-uid short directory: `$TMPDIR/ctxd-<uid>/<16 hex of blake3(dir)>/`, mode 0700. The lease records the actual paths.

## 3. Commit actor, group commit and publication

### 3.1 Commit actor

- One thread owns `Store` and both kernel replicas (§3.3).
- Write requests arrive on a bounded channel (capacity 1,024). A full channel returns `Busy`, and the saturation point is reported in G5.
- **Natural batching.** The actor takes everything already queued, up to 64 requests, without waiting. A lone request forms a group of one, so there is no fixed collection delay.

### 3.2 Group commit (op_log format 2)

- **Frame format.** Each frame carries `group: {seq: u64, idx: u16, count: u16}`. This is format version 2. The migration framework rewrites format-1 logs (each frame becomes a group of one) and format-0 logs, with T68 extended to cover 1 → 2.
- **Group processing.** For a group of n requests, in order:
  1. **Idempotency.** Each batch is checked against the durable id index and against the ids staged earlier in the same group. A duplicate of a staged id with the same hash shares the staged result. A duplicate with a different hash gets `TxnIdReuse`. In both cases the reply waits for the group sync.
  2. **Apply.** Each batch is applied to the active replica. A rejected batch consumes no epoch (I3) and is not written.
  3. **Append.** The accepted batches' frames are appended with the group header. Rollover happens only at group boundaries: a new segment is created, its header synced and its directory synced *before* the group's first frame.
  4. **Sync** the segment once.
  5. **Publish and reply** (§3.3).
- **Every reply in the group is held until step 4 succeeds, rejections included.** A rejection can depend on an earlier batch of the same group (for example `HeadConflict` caused by batch k−1), and it must not reveal state that never became durable (I5(b)).
- **Failure.** If any step after the first apply fails, or a panic occurs, the store is poisoned. Every request in the group then gets `OutcomeUnknown`: the client asks `txn_status` after the daemon restarts. No `TxError` is reported for a poisoned group.
- **Tail classification** (amends ADR 0006 §4 step 2 and SPEC T34). This algorithm is shared by op_log and the session log (§4). Open scans the frames in order:
  1. Verify each frame structurally and semantically (chain, epoch or sequence, group header continuity) until the first fault F. Let G be the group containing F. When F is a structural fault, G is the group of the last verified frame if that group is incomplete, or else the next group.
  2. **Semantic faults** (review R1). If F is semantic (checksum valid but chain, epoch, sequence, group header, hash, redundancy or encoding wrong), the log fails closed (`ChainBroken`), including when F is G's first frame. A semantic fault is tolerated only after a crash-type fault in the same group, and step 3 handles that case. A crash cannot produce a checksum-valid but wrong frame, so this costs no availability.
  3. **Crash-type faults** (incomplete, length check, checksum) and a short G (fewer than `count` frames):
     - Resynchronize past F: scan for any structurally valid frame further on.
     - If one exists whose group `seq` is greater than G's, acked data follows the damage, and the log fails closed (`ChainBroken`).
     - Otherwise, truncate from G's first frame. Frames of G after F are not chain-checked, since they were never acked.
  4. A **crash-type** fault at a group boundary (G's previous group complete, F at G's first frame) truncates G only. A complete final group is kept even if junk follows it, and the junk is truncated.
  - The resynchronization in step 3 scans the rest of the current segment and every later segment.
  - `seq` increases by one for each group actually written. A group whose batches were all rejected writes nothing and consumes no `seq`. A gap or repeat in `seq` is a semantic fault.
  - Each case has a test, including a semantic fault at a group boundary followed by valid groups.
  5. `--strict-tail` refuses any truncation.

  Every case gets its own test: a torn frame inside G followed by a checksum-valid but chain-broken frame; a later valid group after damage; a boundary fault; a short group. The residual risk is unchanged: media corruption of the final, acked group looks like a crash and loses that group, and the loss is reported.
- **Crash injection** gains:
  - multi-frame unsynced regions with all four tail fates applied across frames;
  - rollover in the middle of a run of groups;
  - a crash after k of n frames are appended.

### 3.3 Publication and readers (I4): two replicas ("left-right")

The actor keeps two kernel replicas, `active` and `standby`, both `Db`. Readers hold an `Arc<Db>` published through `arc-swap`, which is lock-free for readers.

**`Db: Send + Sync`** (review N4). The P1 `LayerMemo` (`RefCell`) moves into the FullRecompute maintainer behind a `Mutex`. Counting-mode queries read the counter-based L1/L2 columns (§7) and need no memo. The lazily materialized `DerivedSnapshot` uses `OnceLock`. A compile-time assertion `fn _assert<T: Send + Sync>()` covers `Db<Counting, NoStats>` and `Db<FullRecompute, NoStats>`.
1. The group is applied to `active` (§3.2) and becomes durable.
2. `active` is published: readers loading the pointer from now on see the new epoch.
3. **Reclaim the old replica** (review N5). `ArcSwap::swap` returns the previously published `Arc`. arc-swap settles every outstanding `Guard` on swap, either converting it into a real strong reference or completing it, so `Arc::get_mut` afterwards is exact.
   - Immediately after publishing, the actor waits up to 5 ms for `Arc::get_mut` to succeed. Requests arriving meanwhile queue behind this wait, so it does lie on the next group's latency path. G5 measures it separately (wait time distribution) and counts it in the write cells (review m2).
   - It then replays the accepted batches into that replica with `commit_with(AllPresent)`, which makes it `standby`. Each replayed `CommitResult` must equal the original; a mismatch is a bug and poisons the store.
4. **Clone fallback.** If the replica is still referenced after 5 ms, the actor clones `active` into a fresh standby. This is O(N), counted, and reported per G5 cell.
   - Long requests drop their `Arc<Db>` as early as possible. Log-replay reads take only the `LogReader` and tail epoch they need.
   - A 30 s request timeout bounds the rest.
   - **Fallback trigger:** if the clone rate exceeds 1% of groups in any G5 cell, or clones push write p95 over budget, the chunked persistent `Db` storage (§13) becomes required work before the gate closes.

**Why two replicas** (review M6): the snapshot is a complete `Db`, built by the same deterministic code, so no second representation can disagree with the kernel. Every query, including history, WHY, IMPACT (Reachable) and validation, runs unchanged on the published `Db`.

**Costs:**
- Memory doubles the ADR 0005 scopes. Both replicas are counted and reported, never hidden.
- Publication costs one extra replay per group.
- In debug and paranoid builds the two replicas' digests are compared after every group.

**Exact IMPACT** (hypothetical batch, review N9) runs on the actor between groups, on the caught-up `standby` replica, which the actor owns uniquely. It uses the existing commit-and-restore path (`impact_exact`), then the standby is restored. The restore covers every field, including the new ones: `EpochChanges`, the change indexes, the L1/L2 columns and the `OnceLock` snapshot. A test asserts that the digest and history are equal after an impact (review m1). Impacts are rate-limited to one per group interval and counted in the write cells. That path clones the inputs, which is O(N). In P3 its latency is reported separately from the read and write budgets. The O(affected) undo-log implementation is shared with WHAT_IF and is P6 work (SD19). If the standby is unavailable (clone fallback in progress), the request waits for it.

**Reads below the in-memory history** (ADR 0006 §8 source 3) run on reader threads:
- Log replay goes through a shareable `LogReader`, which holds the frame index up to the published tail and uses read-only `read_range`.
- Projection reads use a reader-side read-only SQLite connection. The watermark is checked inside the same SQLite read transaction.

## 4. Session log

**Files.** `<dir>/sessions/seg-<first_seq:020>.log` uses the ADR 0006 framing with its own domains (`contextdb.session_log.v1`) and its own chain, sharing `log.rs`.

**Events** (Wire-encoded, append-only):
- `SessionStart{session_id: [u8;16] (random), parent, harness, repo, branch, head_sha, principal, channel, started_epoch, log_checksum_at_epoch}`
- `Read{session_id, seq, logical, rev, layer, epoch, required}`
- `CertIssued{cert}`, where `cert` includes `log_checksum_at_issue`
- `Write{session_id, client_txn_id, epoch, reason}`: audit only, e.g. a supersede reason
- `SessionEnd{session_id, epoch}`

**Sync groups** (review N1).
- The session-log writer thread drains its FIFO queue (N17) and writes everything queued as one group. Every frame carries `{seq, idx, count}`, exactly as in op_log §3.2, and the group is synced once.
- **At most one unsynced group** (review R2). A new group is appended only after the previous group's sync has returned. Events arriving in between stay in memory and are queued. A debug assertion checks this, and session-log crash injection covers it.
- The writer syncs immediately when a durable event is pending (`SessionStart`, `CertIssued`, `SessionEnd`), and otherwise at most every 100 ms.
- Recovery uses the op_log tail algorithm (§3.2) unchanged. A crash during a sync can only damage the last group, which was never acknowledged, so the last group is truncated. Earlier damage fails closed.
- There is no separate mark frame.

**Ordering.**
- Every session has an in-memory ledger behind a mutex.
- All session events go through one FIFO queue to the writer thread. A `CertIssued` is enqueued after every `Read` it includes, so its sync covers them (N17).
- A `Read` is appended to the in-memory ledger *before* the read's reply is sent, then queued to the writer thread without waiting.
- Issuance takes the session's ledger lock, so it sees every read that has been answered. The issuance reply waits for the `CertIssued` sync.
- Session syncs never run on the commit actor.

**Lifetime.**
- Sessions survive daemon restarts. Open replays the session log and rebuilds the in-memory ledgers from every group the tail algorithm keeps.
- Reads lost in a crash simply never reached a certificate.
- Handles (§6.1) are bound to durable certificates, so they never change meaning after a restart.

**op_log linkage** (review M3):
- `SessionStart` and `CertIssued` record the op_log checksum at their epoch.
- At validation, a certificate whose `issued_epoch` exceeds the op_log tail, or whose recorded checksum no longer matches the frame at `issued_epoch`, is `Invalid{LogMismatch}`.
- This catches the two ways op_log can lose acked frames: the residual tail risk, and `ctx doctor --truncate-before`. Reallocated dense ids can therefore never validate an unrelated revision.
- Doctor truncation does not rewrite the session log; `Read` epochs are not monotone in log order, because readers can hold older snapshots. Certificates above the new tail are caught by `LogMismatch` at validation. The doctor plan lists how many session events reference epochs above `keep_through` (N13).

**Projections.** `sessions`, `read_ledger` and `certificates` (SPEC §7) are SQLite projections of the session log with their own watermark.

## 5. IPC, channels and identity

**Sockets.** `agent.sock`, `observer.sock` and `admin.sock` are mode 0600 inside a 0700 `run/` directory owned by the daemon's uid.
- The channel is the socket the client connected to.
- The principal comes from `[principals]` in `config.toml`, applied after the peer-credential check (`getpeereid` / `SO_PEERCRED`). A peer uid other than the daemon's uid is rejected.
- No request carries a channel or principal. The daemon builds every Batch from the connection's identity.

The trust boundary is cooperative (§0.5).

**Framing.**
- Each message is `len: u32 LE` ‖ `version: u16` ‖ `kind: u16` ‖ body.
- The body is the Wire encoding of `proto::Request` or `proto::Response`, decoded with `DecodeLimits::IPC`.
- `len` is at most 16 MiB.
- An unknown version gets `ProtocolVersion{supported}` and the connection closes.
- Every response carries the epoch of the snapshot it was computed on.
- A request runs for at most 30 s; past that it gets `Timeout`.

**Requests and channels:**

| Request | Channels |
|---|---|
| `SessionStart`, `SessionEnd` | all |
| `Recall`, `Read`, `Why`, `WhyNot`, `Impact`, `Diff`, `Check`, `Status`, `TxnStatus` | all |
| `Record`, `Supersede` | agent, admin |
| `Validate` | admin only (§0.4) |
| `Raw(Batch)`: kernel channel rules still apply (CodeAnchor needs observer, `SetPolicy` needs admin) | observer, admin |
| `Shutdown` | admin |

## 6. Certificates (SPEC §6.4)

### 6.1 Handles (review B5)

- A handle names one entry of a certificate issued to this session: `<first 12 hex of cert_id>.<entry index>`.
- `ctx_recall` in certified mode and `ctx_check` return certificates, so they hand out handles.
- A dep given as a handle resolves only if three conditions hold:
  - the certificate was issued in the caller's session, as recorded in the session log;
  - the index exists;
  - the entry is `required=true`.

  Otherwise the result is `InvalidDep`. An unknown prefix, a certificate from another session and an advisory entry are all rejected (T36).
- Handles are content-bound, because `cert_id` hashes the certificate. They are durable before they can be used: `ctx_check` syncs `CertIssued` before replying, and recall replies before its sync (§8.5, N8), so a write that uses a not-yet-durable handle forces the sync first.
- This follows SPEC §5 P3 literally: deps reference "handles in this session's certificate" or an explicit `logical@rev`.

### 6.2 Issuance: `cert::issue(db, entries)`

There are two kinds of certificate (review N6). Both use the issuance rules below and differ only in their entries:
- A **recall certificate** is issued by `ctx_recall` in certified mode. Its entries are exactly the items that call returned, all required. Handles come from it. Advisory recall issues no certificate and returns neither `cert_id` nor handles (review N7). An advisory read can still be used as a dep through an explicit `logical@rev`.
- The **session certificate** is issued by `ctx_check` when the session has required reads that no session certificate covers yet. Its entries are the session's *live* required entries:
  - every successful certified read (`ctx_read` in certified mode, and the items of certified recalls) records a required ledger entry;
  - a later required read of a different revision of the same logical **supersedes** the earlier entry, and the ledger records the supersession. A re-read after a conflict (the T32 flow) therefore never produces `Ambiguous`;
  - failed certified reads (`NotCertified`) and advisory reads record `required=false` entries, which never enter a certificate's required set;
  - `required` is OR-merged per `(logical, rev)`, so an advisory read never downgrades a required one.
- **Liveness.** If a live required entry is no longer CERTIFIED, `ctx_check` reports that entry's verdict and diffs (this is the "stale plan → Invalid" signal) until the agent re-reads that logical. A logical that cannot be certified again keeps the check at `warn` (P3 never denies). A new session starts from an empty ledger.

1. **Entries.** At least one required entry must exist. Otherwise issuance fails with `Empty`, and `ctx_check` returns verdict `Empty` and decision `warn`, never `allow` (I6, review N7).
2. **Status.** Every required entry must be CERTIFIED (SD13, observation from `live_pos`) at the issuing snapshot. Otherwise issuance fails and returns that entry's summary. No certificate is written.
3. **Vector.** The vector is built from required entries only. Starting from each entry, it takes the closure along *L2 witnesses*:
   - For a revision r whose L2 witness is a rule j, add every Cur and Pin premise p of j as `(l(p), p)`, and every Sem premise p as `(l(p), head(l(p)))`, its effective revision.
   - Recurse into each added revision's L2 witness.
   - A base witness stops the recursion. Ref is never followed.
   - An Evidence Pin is included like any other Pin.
   - Two distinct revisions of one logical in the vector make issuance fail with `Ambiguous` (T21).
4. **Fields.**
   - `session_id`, `principal`, `branch`, `issued_epoch`;
   - `log_checksum_at_issue`;
   - `code_commit` = the session's `head_sha`;
   - `watermarks` = `(scope_lane, committed position)` for every CodeAnchor logical in the vector;
   - `entries` = `{logical, rev, required, via}`, where `via = Head` at issuance and `Alternative` marks entries of a recertified certificate whose L2 witness changed;
   - `strict_no_aba` as requested.

   `cert_id = blake3(Wire(cert without id))`.

### 6.3 History for strict mode (review M1)

- Core retains a per-epoch `EpochChanges{epoch, head_moves, base_flips, watermark_moves}` in memory, in each replica's `Db`, appended by that replica's commit (review N12).
- Each replica also keeps indexes from logical, revision and scope to the epochs where they changed, so the strict check is O(vector · log(changes)), not O(epochs).
- Durable projections cover the same: the existing `head_history` and `base_history`, plus a new `watermark_history(scope, position, from_epoch)`.
- A **watermark advance** means a committed `ObserverWatermark` whose position differs from the previous one. Changes are net per batch (SD8.3), so a move away and back *within one batch* is not a change, while across batches it is.
- Source selection follows ADR 0006 §8: memory, then projections when the watermark covers the range, then log replay.

### 6.4 Validation: `cert::validate(cert, db, live_pos)`

0. **Log linkage.** If `issued_epoch` > tail, or the checksum differs from the one recorded, the result is `Invalid{LogMismatch}`.
1. **Strict check.** With `strict_no_aba`, any of the following in `(issued_epoch, now]` gives `Invalid{StrictChange}`, and recertification is not attempted:
   - a head move of a vector logical;
   - a base flip of a vector revision;
   - a watermark advance of a vector scope.
2. **Per-entry mapping.** Each required entry maps, using the *current head*'s SD13 summary with observation from `live_pos`, exactly per SPEC §6.4's table. An entry whose recorded revision is no longer head is `Invalid`.
3. **Aggregation.** `Denied > Invalid{diffs} > Conflicted > Ambiguous > Unknown{lag} > Pending{rules} > Ok`. Ambiguous means the vector recomputed on the current state has two effective revisions of one logical.
4. **All Ok.** The vector is recomputed (§6.2 step 3, from the same required entries at their current heads).
   - An identical vector gives `Valid`.
   - A different one gives `ValidRecertified{new_cert, diffs}`. The new certificate is issued to the same session and logged.
5. **`diffs`.** Each diff is `{logical, cert_rev, current_head, summary}` for every vector logical whose effective revision or status changed.

Validation never compares epochs except as the lower bound of the strict range (SD16).

**`live_pos`** maps `scope_lane` to a position.
- A scope missing from `live_pos` has no live information and counts as fresh. This is the frozen P0 oracle semantics; P4 and P5 hooks supply real positions.
- In P3, `ctx-mcp` passes the committed watermarks. `Unknown{lag}` is therefore reachable only through engine tests and PROP11, never in G6. The docs say so.

### 6.5 Oracle (review M12)

`contextdb-oracle` gains independent implementations:
- `cert_issue_oracle(ledger, inputs, live_pos)` → `Result<Cert, IssueFailure>`;
- `cert_validate_oracle(cert, inputs_history, live_pos)`.

Both are full recomputation with Jacobi plus explicit history replay. PROP11 runs 10,000 seeds with random certificates issued at random epochs, random later batches and random `live_pos`. It compares the full issuance output (entries, vector, watermarks, failure reason) and the full validation output (verdict, `diffs`, `new_cert.vector`).

## 7. Counter-based L1/L2 (SPEC §3.4 Phase 3b; review M5)

- **Columns.** `layers.rs` keeps per-incidence `lit1`/`lit2` bits, per-rule `unsat1`/`unsat2` and `counted1`/`counted2`, and per-rule gates. Per node it keeps `support1`/`support2` and canonical witnesses.
- **Propagation.** It uses the same rank order as L0, including `V_l` nodes, with the extra seeds of SPEC §3.4 Phase 3b.
- **When a node propagates.** Every popped node's full `NodeState` is compared with its pre-state: the bits, `stale_reason`, the three witnesses, the semantic dimension, currency and support.
  - Downstream is pushed whenever the bits or the `stale_reason` change. `stale_reason` is inherited along L0 witnesses, so a reason-only change must propagate. Pushes go to `rules_by_premise[r]`, and, when r is the head of its logical, also to `sem_by_logical[l(r)]` through `V_l` (review N15).
  - `status_diff` is every node whose full state changed.
  - This keeps the P1 walker's "full value comparison" and replaces only *how* L1/L2 are computed.
- **Generators.** PROP2b and incremental L1/L2 == oracle (10,000 seeds) gain generators that target reason-only and witness-only changes.
- **FullRecompute mode** keeps the P1 memo, so the two exact modes stay independent.
- **Assertions.** DA1–DA6 extend to L1/L2.
- **Memory.** The columns are counted in the ADR 0005 hot scope. If it exceeds its budget, that is reported and recalibrated by ADR.

## 8. Agent writes and read modes

### 8.1 `ctx_record`

Parameters: `(kind, body, deps, expected_head | create_only, adopt_as_base, key?, client_txn_id?)`. `anchors` must be empty in P3; anchors arrive in P5.

**Preconditions.** Exactly one of `expected_head` and `create_only` must be given, otherwise `InvalidArgument`. `adopt_as_base` is required (SD1).

**Batch:**
1. The body is stored first with `put_blob`.
2. `create_only` gives `CreateLogical`. Otherwise the batch is `AddRevision` followed by `SetHead{expected_head, New(0)}`.
3. If there are deps, one `AddRule{conclusion: New(0), incidences}` follows.
4. Every dep is either a handle (§6.1) or an explicit `logical@rev` (SPEC §6.1 key syntax), with an edge kind.

**Errors.** `InvalidDep` is a daemon-level check made before the batch is built. Its position in the catalog is "before `UnknownRef`, for deps only".

**Transaction ids.** Every write batch carries a `client_txn_id`. `ctx-mcp` and the CLI generate one when the caller gives none, so `OutcomeUnknown` can always be resolved with `txn_status` (review N14).

### 8.2 `ctx_supersede`

Parameters: `(logical, body, expected_head, adopt_as_base, carry_rules: copy | none, reason, client_txn_id?)`.
- The batch is `AddRevision`, then `SetHead{expected_head → New(0)}`.
- With `carry_rules=copy`, one `AddRule` follows for every *enabled* rule concluding `expected_head`, with the same incidences and conclusion `New(0)` (§0.3). Cycle and duplicate checks apply as usual.
- `reason` goes to the session log as a `Write` event.

### 8.3 `ctx validate` (admin CLI)

Parameters: `(rule, verdict, curvec_seen)`, or `(rev, verdict)`.
- The `rev` form first *shows* every active rule concluding `rev` that has a Sem incidence (SD10), with its current vector.
- After the authorized validator confirms (automation may use `--yes`), it submits one `RecordValidation` per rule with exactly the vector that was shown as `curvec_seen` (review N11). A vector that moved in between gives `CurvecMoved`, so the compare-and-set covers the time spent reviewing.

### 8.4 T32: retry escape

T32 is a documented limitation, tested as one. After a `HeadConflict{actual}`, a retry carrying `expected_head = actual` succeeds. The test asserts that the new revision's rule references exactly the handles or revisions the agent supplied, and that the system did not add or rebind anything implicitly.

### 8.5 CertifiedCurrent and T07e (review M7)

`ReadMode::CertifiedCurrent` targets a logical. It returns `(head, NodeState, payload)` only if the head's SD13 summary is CERTIFIED. Otherwise it fails with `NotCertified{logical, summary}`, a new read error in SPEC §6.2.

**`ctx_read`:**
- `mode=certified` uses CertifiedCurrent.
- A handle or `logical@rev` naming a revision that is no longer head fails with `NotCertified{logical, Stale(HeadSuperseded)}` and returns no payload (T07e).
- `mode=advisory` returns any revision with its full status and records `required=false`.

**`ctx_recall`:**
- `mode=certified` lists CERTIFIED items. Others go to `withheld` with their summary (I6).
- It records `Read{required: true}` for the returned items and issues a recall certificate. It returns its `cert_id` and handles **without waiting for the certificate's sync** (review N8), so recall stays on the read budget. A handle becomes usable as a dep once its `CertIssued` is durable: `Record` and `Supersede` with a not-yet-durable handle force a session-log sync before building the batch, which puts that cost on the write path.
- `mode=advisory` returns all items with status, records `required=false`, and issues no certificate.

**`ctx_check`** in P3 takes `(cert_id?, action?)`. It validates the given certificate, or else the session certificate. A recall certificate is validated only when it is given explicitly. Without an explicit `cert_id`:
1. **Validate first** (review R3). If a session certificate exists, it is validated per §6.4. A result other than `Valid` or `ValidRecertified` is returned as-is, and nothing is re-issued, so a strict change, a base flip or a watermark advance in the window can never be hidden by re-issuance.
   - **Superseded entries are skipped** (review N-R3a). Validation skips entries that the ledger has superseded since the certificate was issued, because the agent explicitly read a newer revision. Each skipped entry is reported once as a supersession diff, and the remaining entries are validated from their own `since_epoch`.
   - This lets the documented recovery work: re-reading after a conflict (the T32 flow), then `ctx_check`, gives `Valid`, or `ValidRecertified` after re-issue. A test covers it.
   - Strict mode is unaffected: every non-superseded entry keeps its full window.
2. **Re-issue only when needed.** If the result is `Valid` or `ValidRecertified` (adopting the recertified certificate) and the session has required reads the certificate does not cover, a new session certificate is issued.
   - It inherits `strict_no_aba`.
   - Each entry keeps its *own* strict-window start: `entries[].since_epoch`, the `issued_epoch` of the first certificate that covered it. Step 1 of §6.4 checks every entry's logicals, revisions and scopes from that entry's `since_epoch`.
   - A vector item in the closure of several entries is checked from the earliest `since_epoch` among them.
3. **Failed issuance maps to a verdict** (review R4). If issuance fails, the failing required entries go through the §6.4 per-entry table and aggregation:
   - STALE or superseded → `Invalid{diffs}`;
   - otherwise `Conflicted`, `Ambiguous`, `Unknown{lag}` or `Pending{rules}`, by the usual priority;
   - no required entries → `Empty`.

   `ctx_check` therefore always returns a §6.4 verdict and diffs. PROP11 checks this mapping against the oracle. `Empty` is added to `CertVerdict` and to the `ctx_check` verdict set (SPEC v1.5.2).
4. **Supersessions are reported** (review m5). When entry supersession dropped an earlier dependency since the last check, `ctx_check` lists it once in `diffs` as `{logical, superseded: old_rev → new_rev}`.
- `decision` is `allow` for `Valid` and `ValidRecertified`, and `warn` for everything else. `deny` is never returned in P3; P4 configures it.
- `action` is recorded but not used until P4 and P5a.

## 9. Retrieval (basic)

- An FTS5 projection indexes `(key, kind, scope_lane, tags, payload text)`. It is rebuildable, keeps its own watermark, and is maintained by the projection thread off the commit path, using the reader-side connection rules (§3.3).
- Matching tries an exact id or key, then a key prefix, then FTS5, filtered by scope.
- Packing and hybrid ranking arrive in P8.

## 10. MCP layer

- JSON-RPC 2.0 over stdio, one message per line.
- Methods: `initialize` (protocol version negotiation), `notifications/initialized`, `tools/list`, `tools/call`, `ping`. Anything else gets `-32601`.
- **Agent tools in P3:** `ctx_recall`, `ctx_read`, `ctx_record`, `ctx_supersede`, `ctx_why`, `ctx_why_not`, `ctx_impact`, `ctx_check`, `ctx_txn_status`. There is no `ctx_validate` (§0.4).
- `TxError` maps per SPEC §6.2.
- Daemon and store errors map to stable codes, all listed in SPEC §6.2 (v1.5.1) (review N10):
  - `OutcomeUnknown`, `Busy`, `Timeout`;
  - `NotCertified`, `InvalidDep`, `InvalidArgument`;
  - `RestartRequired`, `Poisoned`, `Locked`;
  - `UnsupportedFilesystem`, `ProtocolVersion`, `Unavailable`.
- **Fuzzing** targets the JSON→Request mapping and the IPC decoder.

## 11. Verification plan

| Layer | Plan |
|---|---|
| G2 | Engine-level: T06b, T06d-sem, T15/b/c/d (incremental mode), T16, T16b-cert (head ABA and anchor base ABA), T21, T62-P3, T65. End-to-end against `ctxd`: T07e (handle, `logical@rev`, recall paths), T08 concurrent, T12 concurrent (barrier so both writes land in one group), T32, T36 (another session's handle, an advisory entry, an unknown prefix), T37, T46a (engine: all five CodeAnchor write ops on Agent; e2e: each agent tool), plus `ctx validate` refused on the agent channel. |
| G3 | Incremental L1/L2 == oracle (10,000 seeds). PROP2b with reason/witness-only generators. PROP11 for full issuance and validation output, 10,000 seeds. Mutation over `layers.rs` and `cert.rs`. |
| G4 | **Stress** with 1/8/32/128 clients, mixed reads and writes for 10 minutes (nightly). Its invariants are explained below the table. **Snapshot immutability:** a retained `Arc<Db>`'s digest is unchanged after N later publications. **loom** only on our own publication and standby-reclaim handshake. loom cannot instrument arc-swap's internals, so the `cfg(loom)` shim replaces arc-swap with a loom-visible pointer, and arc-swap's own guarantees are taken from its documentation and test suite (review m6). **cargo-fuzz** for at least 1 h each on the IPC decoder and JSON→Request. **Crash injection** at group level and on the session log. **Injected statfs** tests for the filesystem refusal list. |
| G5 | Bench matrix: clients {1, 8, 32, 128} × fullfsync {on, off}. It reports read p95 and write p95 per cell, throughput, queue depth and the saturation point. The commit path's projection, FTS and session-log costs are included or separately reported. Read p95 ≤ 5 ms applies to every cell. Write p95 ≤ 15 ms is the one-client latency target in both fullfsync modes. A cell with N clients is bounded by write p95 ≤ 2 × N / measured throughput: one in-flight group plus its own. The peak throughput of each fullfsync mode must reach ≥ 2,000 tx/s. Every cell's p95 is reported. `mcp_bench` exits non-zero on any miss. This calibration was decided by the owner on 2026-10-20 after measurement (§17). It replaces the revision 3.2 wording that applied 15 ms to every cell. Exact IMPACT latency is reported separately (§3.3). |
| G6 | Three simulated agents through `ctx-mcp` follow §1.2: a stale plan's `ctx_check` returns `Invalid` with diffs; with alternative support it returns `ValidRecertified`. |
| G7 | P0–P2 suites at gate counts, including the format-2 migration (T68). |
| G8 | README; the trust boundary (§0.5); MCP tool docs; `ctxd` operation; SPEC v1.5.x appendix C. |

The G4 stress workload maintains invariants that a mixed-epoch read or a lost update would break, so the checks cannot pass vacuously:
- **Coupled heads.** T08-style coupled logicals are always moved together in one batch. A reader that sees them disagree has seen a mixed epoch.
- **Replay check.** Every k-th read response is recomputed by replaying the log to its reported epoch and must match.
- **CAS chain.** For each logical, the successful `SetHead`s must form one chain: each success's `expected` equals the previous success's `new`. The chain must equal `head_history`.

## 12. Milestones

Each milestone keeps G1 green and lands its tests.
1. Core: counter-based L1/L2, PROP2b, per-epoch change records, `cert.rs` with the oracle issuance and validation, PROP11.
2. Store: op_log format 2 with group framing, migration and crash injection; the session log; `LogReader`; the `watermark_history` projection.
3. Daemon: lease, sockets, identity, protocol, commit actor with group commit and two replicas, readers, sessions, certificates, writes, the CLI `validate`.
4. MCP server, FTS5 recall, `ctx` client commands, overlay-based Exact IMPACT.
5. G4 stress, loom and fuzz; G5 bench; G6 scenario; gate report; independent review.

## 13. Risks

- **Write latency with fullfsync on.** The latency floor is one fullfsync (P2 measured a single-commit p99 of 8.7–17.6 ms). Under concurrency a request can wait for an in-flight sync plus its own, so write p95 ≤ 15 ms is at risk for clients > 1 with fullfsync on. It is reported per cell and calibrated by ADR if missed.
- **Two replicas** double the kernel memory. The cost is reported against ADR 0005. If it is unacceptable, the fallback is chunked persistent storage in `Db` (a later ADR).
- **Standby clone fallback** is O(N). It is only triggered by long-held snapshots and is counted and reported.

## 14. Review mapping (revision 1)

**Blockers**
- B1 → §3.2 group framing, format 2 and tail rule; §4 `SyncMark`.
- B2 → §3.2: idempotency across the group, replies held until sync, `OutcomeUnknown`.
- B3 → §0.3, §8.2.
- B4 → §0.4, §5, §8.3, §10.
- B5 → §6.1, §4 lifetime.

**Majors**
- M1 → §6.3.
- M2 → §6.2.
- M3 → §4 op_log linkage, §6.4 step 0.
- M4 → §4 ordering.
- M5 → §7.
- M6 → §3.3.
- M7 → §8.5.
- M8 → §0.5.
- M9 → §0.1 and SPEC v1.5.0.
- M10 → §3.3: overlay impact, `LogReader`, reader-side SQLite.
- M11 → §11 G4.
- M12 → §6.5.
- M13 → §3.1, §11 G5, §13.
- M14 → §11 G2 (T62-P3) and SPEC v1.5.0. T62-P3 means unrelated same-branch commits leave a certificate Valid. A type-level test checks that `validate` takes no epoch parameter, and the cross-branch part moves to P6.

**Minors**
1. Lock-free readers → `arc-swap` (§3.3).
2. Panics → `OutcomeUnknown` (§3.2).
3. Error catalog → §10, SPEC §6.2.
4. Filesystem list and injectable statfs → §2.
5. Socket path length → §2.6.
6. Stale sockets → §2.5.
7. `live_pos` → §6.4.
8. Tool details → §8.
9. Test details → §11.
10. Toolchain and dependencies → §1.
11. Reader-side projections → §3.3.
12. Certificate fields and diffs → §6.2, §6.4.

## 15. Review mapping (revision 2)

**Blocker**
- N1 → §4 sync groups with `{seq, idx, count}` and the shared tail algorithm of §3.2.

**Majors**
- N2 → §3.2 tail classification algorithm, with tests per case.
- N3 → SPEC v1.5.1:
  - T34 covers the last group;
  - §6.4 gains step 0 (`Invalid{LogMismatch}`, the same priority as `Invalid`) and the `log_checksum_at_issue` field;
  - `ctx_read` gains `mode`;
  - certificate kinds and `Empty`.

  PROP11 and the oracle include `LogMismatch`.
- N4 → §3.3: `Db: Send + Sync`, memo moved into FullRecompute, `OnceLock`, compile-time assertion.
- N5 → §3.3:
  - `get_mut` after `swap`, relying on arc-swap's settling of outstanding guards;
  - catch-up right after publishing, with a 5 ms bounded wait;
  - early `Arc` drop;
  - clone rate per G5 cell, with the > 1% fallback trigger.
- N6 → §6.2: recall versus session certificates, supersession of entries, failed reads advisory, liveness defined.
- N7 → §6.2 and §8.5: advisory recall issues no certificate; zero required entries gives `Empty`/`warn`; `ctx_check` picks the session certificate.
- N8 → §8.5: recall replies before its certificate sync, and handles become usable once durable (writes force the sync). §11 G5 keeps the SPEC targets for every cell.

**Minors**
- N9 → §3.3: Exact IMPACT on the actor-owned standby; O(affected) deferred to P6 with WHAT_IF.
- N10 → §10 and SPEC §6.2.
- N11 → §8.3: shown vector, confirmed, active rules.
- N12 → §6.3: per-replica records with change indexes.
- N13 → §4: no quarantine rewrite; `LogMismatch` plus doctor plan counts.
- N14 → §8.1: `client_txn_id` always set.
- N15 → §7: `sem_by_logical` push on reason change.
- N16 → SPEC §10 row qualified.
- N17 → §4: a single FIFO queue.

## 16. Review mapping (revision 3)

**Majors**
- R1 → §3.2 step 2: semantic faults fail closed at a boundary too. Step 4 is limited to crash-type faults; resync scans later segments; `seq` is defined.
- R2 → §4: at most one unsynced session-log group.
- R3 → §8.5 `ctx_check`: validate before re-issuing, inherit strict, per-entry `since_epoch`.
- R4 → §8.5: failed issuance maps through the §6.4 table; `Empty` is in `CertVerdict`.

**Minors**
- m1 → §3.3: Exact IMPACT restores every field, with a test, and is rate-limited.
- m2 → §3.3: the reclaim wait is measured and counted in G5.
- m3 → a write that uses a not-yet-durable handle overlaps the session-log sync with its op_log sync where possible, and is reported in the write cells.
- m4 → SPEC v1.5.2:
  - §6.4 wording now includes log linkage and history;
  - the SessionStart hook's certificate is reconciled in P4 G0.
- m5 → §8.5 supersession diffs. The "uncertifiable logical stays at warn" case is flagged for P4, where `deny` exists.
- m6 → §11 G4 loom scope.
- N-R3a (confirmation round) → §8.5 step 1: superseded entries are skipped and reported. The strict window of a shared vector item starts at the earliest `since_epoch` (§8.5 step 2).

## 17. Amendment: payload packs and ordered syncs (revision 3.3, G5)

The first G5 measurements (fullfsync on) showed that the write path paid far more than the one op_log sync per group that §3.2 intends. With 8 clients, write p50 was 98 ms and only 81 tx/s went through. Three causes were measured and fixed; none changes a semantic or durability promise.

1. **Writers pinned replicas.** A write request held its snapshot while waiting for its group, so the actor could never reclaim the standby replica and cloned the Db on every group. Write paths now resolve deps and then drop the snapshot before waiting (§3.3). The indexer likewise drops its snapshot before it touches SQLite. Clone fallbacks fell from one per group to 1 per run (the `clone fallbacks` column of `docs/gates/p3-g5-mcp.txt`).
2. **The session-log writer synced every batch.** It now follows §4 as written: it syncs at once when a flush is requested or a durable event (`SessionStart`, `CertIssued`, `SessionEnd`) is pending, and otherwise within 100 ms. Read events had been forcing an F_FULLFSYNC that stalled the op_log.
3. **Payloads cost two full syncs each, plus file creation.** On APFS, creating and renaming one file per payload costs about 2–3 ms while another sync is in flight (scratch microbenchmark: 7 payload files ≈ 67 ms under contention versus ≈ 7 ms appended to one file).

**Payload packs (supersedes the file-per-blob layout of ADR 0006 §5 for new writes).**
- New payloads are appended to `blobs/pack-NNNNNNNN` as records `"CXBP" | len u32 LE | blake3(bytes) | bytes`.
- A record is valid exactly when its hash matches its bytes. The open-time scan therefore trusts no length: after an invalid header or a mismatching hash it resyncs at the next magic. Torn tails, garbage and media damage cost only the damaged records, which read as `Missing` or `Corrupt`. Nothing is ever truncated.
- A process appends only to segments it created, starting a new one after `segment_target` bytes.
- **Presence means durable** (ADR 0006 §5, review n1). A hash counts as present only after the put that wrote it returned, or, at open, when a legacy file or a verifying record on a *synced* segment exists.
  - Open fully syncs every pack segment before trusting it, and fails if a sync fails. A killed process can leave verifying records only in the page cache; without this, a retried write could dedup against one and lose it at the next power loss (P3 review finding 1, `g4_kill_restart_retry_never_loses_a_deduplicated_payload`).
  - For the same reason, open syncs the op_log tail segment (`g4_kill_restart_makes_page_cache_frames_durable`).
  - The open-time verification and syncs add about 120 ms for 100K payloads of 200 B: a snapshot open takes 289 ms with them and 172 ms without (`docs/gates/p3-g5-store.txt`).
- Legacy per-file blobs remain readable and present. A corrupt payload is replaced by the next put of the same bytes, and the newest verifying record wins at open.
- Readers locate payloads through a shared index, the counterpart of `LogIndex`, so they never touch the actor.
- The index costs about 48 B per payload (hash plus location) plus HashMap overhead. Like the presence set, it sits in the store, outside the Db scopes of ADR 0009.

**Ordered syncs.** `Durability` gains `sync_weak` and `sync_dir_weak`.
- On macOS they are `F_BARRIERFSYNC`, which costs about 0.4 ms against about 5 ms for `F_FULLFSYNC`. The bytes reach media no later than anything written after the call. Elsewhere they are fsync(2), which is at least as strong.
- `Store::stage_blobs` appends a group's new payloads to the active pack, issues one ordered sync, and issues an ordered `sync_dir` when the segment is new. The actor then runs `commit_group`, whose op_log `F_FULLFSYNC` flushes the whole drive cache.
- So a frame can never reach media ahead of its payloads, and the payloads are durable before any reply is sent (I5).
- `put_blob`/`put_blobs` (non-daemon callers) add a full sync and stay durable on return.
- A group whose batches are all rejected leaves staged payloads that no frame references, which is harmless.

**Model and tests.**
- SimDisk tracks ordered state separately. A full sync anywhere promotes it.
- On a crash, either the ordered state reached media, or it did not, and then neither did any unsynced byte written after it.
  - This under-approximates real outcomes. It never produces partially persisted ordered state, an earlier ordered sync persisted with a later one lost, or another file's pre-barrier tail surviving alone.
  - What it does produce is the hazard that matters: an unsynced frame persisting without its payload when the barrier is missing. The self-test below checks that the harness sees it.
  - `SimDisk::restart` models a killed process: same page cache, no power loss.
- `g4_staged_blobs_never_trail_their_frame` reopens first, so every step fires: ordered sync, ordered dir sync of the new segment, append and op_log sync. It crashes under all four tail fates. A surviving frame always finds its payloads.
- `g4_dedup_on_a_payload_of_a_rejected_group` covers a payload staged by an all-rejected group and reused by a later one.
- **fullfsync off** (measurement only) makes `sync_weak` a plain fsync, which orders nothing on macOS. That mode is not crash-safe, like the rest of the fullfsync-off configuration (ADR 0006 §10).
- `g4_staged_harness_detects_a_missing_barrier` shows the harness fails when payload bytes are left unsynced.
- `g4_blob_write` covers each pack step. `parse_skips_damage_and_resyncs` covers damage, garbage, torn tails and a payload containing the magic.

**Result (local M-series Mac, `mcp_bench`, 8 s per cell; the final gate run is `docs/gates/p3-g5-mcp.txt`; microbenchmark `crates/contextdb-bench/c/apfs_sync.c`, output `docs/gates/p3-apfs-sync.txt`).**

| clients | fullfsync | read p95 ms | write p50 ms | write p95 ms | write tx/s |
|---:|---|---:|---:|---:|---:|
| 1 | on | 0.59 | 5.62 | 10.93 | 153 |
| 8 | on | 2.47 | 11.99 | 19.06 | 597 |
| 32 | on | 0.67 | 13.69 | 18.96 | 2,193 |
| 128 | on | 1.69 | 26.52 | 40.17 | 4,521 |
| 1 | off | 0.09 | 0.56 | 0.91 | 1,539 |
| 8 | off | 0.19 | 2.83 | 4.55 | 2,767 |
| 32 | off | 0.61 | 6.74 | 10.88 | 4,466 |
| 128 | off | 0.99 | 18.68 | 27.35 | 6,533 |

Before the amendment the 8-client fullfsync-on cell was 98 / 121 ms at 81 tx/s, and the 128-client cell was 1,343 / 1,438 ms at 95 tx/s.

**Calibration (owner decision, 2026-10-20).** Under the revision 3.2 wording, write p95 missed 15 ms with fullfsync on at 8, 32 and 128 clients (19 / 19 / 40 ms), and with it off at 128 clients (27 ms).
- In this closed-loop benchmark, latency at N clients is N divided by throughput (Little's law). A 15 ms p95 at 128 clients would need over 8,500 tx/s, four times the throughput target.
- With one ~5 ms drive flush per group, a write waits for the in-flight group and then its own.
- The owner therefore chose the per-client calibration in §11.

Under it, every cell above passes. Budgets for N > 1 depend on each run's measured throughput. For the gate run they are in the `write budget ms` column of `docs/gates/p3-g5-mcp.txt`. The independent P3 review checks this calibration.

## 18. P3 review mapping (revision 3.3)

The independent P3 review of `5b49800` found 1 blocker, 2 majors and 7 minors. All are fixed with regression tests unless stated otherwise.

**Blocker**
- **1** → §17 "presence means durable". Open syncs every pack segment and the op_log tail. `SimDisk::restart` models a killed process; two kill-restart G4 tests cover it.

**Majors**
- **2** → `SessionCert{session_id, cert_id}` in the session log restores the session certificate on replay, so `ctx_check` validates before it re-issues (§8.5 step 1).
  - A session whose durable events name an epoch beyond the tail, or whose start checksum no longer matches, is marked log-broken. Its `ctx_check` returns `Invalid{LogMismatch}`, and its handles are refused.
  - Test: `restart_restores_the_session_certificate_and_a_cut_log_breaks_the_session`.
- **3** → a drain over u16::MAX events is split into several synced groups, so at most one group is unsynced. A failed append poisons the log, so later flushes fail instead of reporting Ok. Recall is capped at 1,000 items.
  - Tests: `oversized_drain_is_split_into_synced_groups`, `recall_is_capped`.

**Minors**
- **4** → session events are queued under the session lock (N17 order).
- **5** → `Service::stop_writer` writes the lazy events and joins the writer before the actor drops the store and its lock. Test: `clean_shutdown_writes_lazy_session_events`.
- **6** → the writer syncs no later than 100 ms after the oldest unsynced event, however steady the stream (bounded drain). Test: `steady_reads_are_synced_within_the_lazy_bound`.
- **7** → `g4_staged` covers every step; a rejected-group dedup test is added; the crash-model wording is fixed (§17).
- **8** → a replica replay mismatch poisons the store (`fail_stop`) and is counted (`replay_mismatches`). Exact IMPACT runs at most once per actor loop iteration; the rest queue. Test: `queued_exact_impacts_are_paced_and_all_answered`.
- **9** → session-log flushes wait at most the request timeout and return `Timeout`.
- **10** → evidence and text:
  - Committed artifacts: `docs/gates/p3-g5-mcp.txt`, `p3-g5-store.txt`, `p3-apfs-sync.txt` and the microbenchmark source.
  - `mcp_bench` reports groups and clone fallbacks per cell, plus handle-dep cells (N8 forced sync).
  - `store_bench` measures the open-time pack scan.
  - The loom model asserts the two-groups-behind replay.
  - §6.1 now agrees with §8.5.
  - The G5 write-p95 misses were calibrated by the owner (§11).
  - Busy cannot occur in a closed loop with at most 128 clients against a queue of 1,024, so the saturation curve is the throughput plateau across 1/8/32/128 clients.

**Notes**
- Socket directories, including the temp fallback's `ctxd-<uid>`, must be owned by the daemon's uid and are set to 0700.
- `ctx-mcp` fails instead of using an all-zero `client_txn_id`.
- `SessionEnd` checks the principal.
- A failed `SessionStart` flush removes the session.
- **Envelope epoch on writes:** it stays the pre-write snapshot's epoch. The commit's own epoch is in `CommitResult`.
- **Accepted deviation (m3):** a write with a not-yet-durable handle runs the session-log sync and the op_log sync one after the other, not overlapped. Its cost is reported in the handle-dep cells (write p95 ≈ 36 ms with fullfsync on).

**Closure check (of the fixes above).** Findings 1 and 3–10 and the notes were verified closed. Three more items were fixed:
- **N1 (major)** → a detected log cut is persisted as `SessionEvent::LogBroken` before the daemon serves. Replay also marks a session broken when a `CertIssued` no longer matches the log checksum at its issue epoch. A cut session therefore stays broken across any number of restarts. Test: `a_cut_session_without_a_certificate_stays_broken_across_restarts`, which fails without the marker.
- **N2** → `SessionLog::open` syncs the last segment, so groups a killed process left in the page cache cannot vanish after they are replayed. Test: `open_syncs_groups_a_killed_process_left_unsynced`.
- **N3** → `stop_writer` holds its lock until the writer has exited, so concurrent shutdowns cannot release the store early.
- **Residual notes, accepted:**
  - A `SessionStart` that timed out may become durable later.
  - Reads served after a poisoned session log, or after `Stop`, lose their lazy events.
  - A retried `SessionEnd` gets `UnknownSession`; clients treat that as done.

**Closure confirmation (of `4ba1291`).** A non-implementing reviewer found N1–N3 closed and no new blocker or major. It raised two minors, both now fixed:
- **C1** → if the startup `LogBroken` sync fails or times out, `Service::with_recall` stops the writer and the actor and returns an error, so `ctxd` refuses to serve. Otherwise plain writes could reuse the cut epochs and hide the break from the next restart. Test: `a_cut_that_cannot_be_marked_durably_refuses_to_serve`.
- **C2** → `SessionLog::open` also syncs `sessions/`. A rollover killed between the segment's file sync and its directory sync leaves a valid header-only segment that later groups append to, so its name must be durable. Test: `open_syncs_the_name_of_a_segment_whose_rollover_was_killed`, which fails without the sync.
- **Accepted (low):** a transient op_log read error during replay is treated as a cut and marks the session broken. This fails safe.
