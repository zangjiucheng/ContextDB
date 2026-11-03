# ADR 0015 — P7 semantic validation, conflicts, handoff and changes

Status: **accepted r0.3**, 2026-11-02. **P7 G0 passed** after the reviewer's
check of the N1–N7 edits (§13). Nothing here is implemented until the SPEC
amendments (§12) land. r0.1 was reviewed
independently: 4 blockers, 7 majors, 7 minors. The closure check of r0.2
found them closed or partly closed, plus 2 new majors and 5 minors (N1–N7),
all text fixes. r0.3 applies them.

## 0. Scope and decisions taken

P7 implements SPEC §5 P7. Its goal is that `PENDING` has a reliable way out
and that contradictions are found but not settled without authority:

- a validator worker pool outside the writer, deduplicated by
  `(branch, rule, curvec)` and fenced by the kernel's CAS;
- ValidationRecord metadata: validator kind and version, model,
  `prompt_sha256`, label and evidence;
- label mapping only for trusted validators. An LLM never writes a
  ValidationRecord. It can only suggest, through a non-gating
  `PendingReview` (SPEC P7, §4.4, §3.5 #14, §11);
- lexical contradiction candidates, `ctx_conflicts` and `ctx_resolve`;
- `ctx_handoff_begin` / `ctx_handoff_accept`;
- `ctx_changes(since_epoch)` with a long-poll stream.

Every item is per branch (SD19, §2–§8).

Gate items: T49, T50, T52, T53 and the new T76–T84 (§11). T51 and the S13
`CONFLICTED` row stay as P6 regressions. SPEC open question O4 (the formal
definition of Conflict) is answered by SD20 (§2) before G0, as SPEC §12.2
requires.

**Owner decisions (2026-11-02):**

- **D1, real LLM evaluation.** G6's real-validator run calls the local
  `claude` CLI headless (`claude -p`) on the owner's existing subscription,
  with no API key. The adapter depends only on an external command, and
  every test uses a mock.
- **D2, metadata storage.** op_log format 3 → 4. A ValidationRecord gains a
  32-byte metadata hash, and the metadata is a content-addressed blob in the
  store's blob store (§3). Upgrading needs `ctx migrate` once more.
- **D3, who may open a gating Conflict.** The agent and admin channels both
  may, as today. An LLM validator only writes `PendingReview`, which does
  not gate.

**How PENDING leaves (SPEC §4.4).** There are exactly three exits:

- a trusted validator (§4.2);
- an admin `ctx validate`;
- supersede.

An authorised maintenance agent may use the admin CLI with independent
evidence, for example after reading an LLM's suggestion. The suggestion
alone never approves anything. This is the SPEC's rule, so no owner
decision is needed.

Out of scope:

- embedding candidates (P8);
- validator ACLs and per-principal trust (P9);
- automatic resolution of any contradiction;
- per-revision union validation vectors (O2 stays open; §10 measures it).

## 1. Today

Code facts (paths under `crates/`; checked by the reviewer):

- **Kernel validation.** `Op::RecordValidation{rule, curvec_seen, verdict,
  validator}` (`contextdb-model/src/contract.rs:150`) applies with a CAS
  against the staged Sem current vector. An empty vector gives
  `NoSemIncidence`, a different one `CurvecMoved{rule, seen, current}`
  (`contextdb-core/src/txn.rs:527`; oracle twin
  `contextdb-oracle/src/input.rs:647`). A later record for the same key
  replaces the earlier one (`txn.rs:553`): latest wins, per SD10.
- **`ValidationRecord{verdict, validator: String}`** (`contract.rs:426`) is
  keyed by `(RuleId, HeadVector)`. It is a per-branch overlay (SD19).
  `sem(j)` is computed in `contextdb-core/src/layers.rs:22`. L1 is blocked
  by `Rejected`; L2 needs `Verified` or `NotRequired`.
- **Artifacts.** `OpenArtifact{ty, target, payload_hash}` and
  `ResolveArtifact{id}`. Merges open `Conflict`; the anchor observer opens
  `AnchorBroken` (observer only, `txn.rs:224`). No production code opens
  `PendingReview`. An open Conflict on `r` blocks `L2(r)` (SD17), in the
  engine, the oracle and the what-if undo.
- **Blobs.** The store owns `<data>/blobs` (loose hex files and packs,
  `contextdb-store/src/store.rs:361,405`). Commits carry blobs
  (`Request::Commit.blobs`), and the kernel refuses a revision whose content
  blob is not present (`MissingBlob`, `txn.rs:366`).
- **Change history.** The projection keeps a durable per-branch
  `diff_log(branch, epoch, revision, post)`
  (`contextdb-store/src/projection.rs:151`). Every `CommitResult` stores its
  `status_diff`.
- **Channels and principals.** `PrincipalId` is a `u32` (`contract.rs:11`);
  single-principal mode uses fixed ids per channel
  (`contextdb-daemon/src/server.rs:31-44`). The kernel accepts
  `RecordValidation` from Agent and Admin. The daemon's `Validate` is
  Admin-only (`service.rs:1113`, `ChannelRestricted`), but an agent's
  `Commit` or `WhatIf` (`service.rs:963-992`) can carry a `RecordValidation`
  op, including `Rejected`. That contradicts SPEC §6.3 (`ctx_validate`:
  admin CLI only, then the P7 worker); §4.3 closes it. No in-process
  identity commits today except the outbox sweeper (Agent,
  `server.rs:179`), so the worker identity is new.
- **Missing:**
  - validator metadata;
  - any worker pool or job table;
  - handoff;
  - a change feed request;
  - conflict candidates;
  - the MCP tools `ctx_conflicts`, `ctx_resolve`, `ctx_handoff_*` and
    `ctx_changes`.
- **Recall.** The FTS5 index (`contextdb-store/src/recall.rs`) runs AND
  queries ranked by bm25, with an epoch watermark. It holds each logical
  once, with the head of the first branch that sees it
  (`service.rs:3020-3029`).
- **Tests.** PROP1 (`contextdb-testkit/tests/p1_properties.rs`) uses
  `generate_p1`, which emits `RecordValidation` but no artifact ops. PROP13
  and the P6 conflict-gate test do include Conflict ops.

## 2. SD20: Conflict (answers O4)

**Definition.** A *contradiction* between revisions `a` and `b` on branch
`β` is an input-layer assertion (SD7) that `a` and `b` cannot both hold. It
is one open `Conflict` artifact per side, `target = a` and `target = b`,
opened on `β` in one batch with the same `payload_hash`. A one-sided
Conflict (a merge conflict, or a contradiction with something outside the
store) has one artifact. Artifacts are per branch (SD19); merge carries
them as P6 does.

**Gate.** As SD17: `L2(r)` additionally requires that no Conflict on `r` is
open on the branch. `open_conflict` is an input fact of a lower stratum than
L2, so the negation is stratified and the model stays unique. L0, L1, `sem`
and support are unaffected (de Kleer: contradiction detection is eager and
separate from support).

**Currency, not inheritance.**
- A Conflict targets a revision, not a logical. Superseding `a` with `a'`
  leaves the Conflict on `a`, and `a'` is not gated. Superseding is
  therefore one way out, and resolving is the other.
- The other side `b` stays CONFLICTED until it is resolved: `ctx_conflicts`
  marks such pairs `side superseded` so they are not forgotten.
- Nothing derives a Conflict. Candidates and LLM judgements never open one
  (D3, §5).

**Payload.** The payload blob is
`ConflictDoc{v: 1, sides: [RevisionId], reason, source: Agent | Admin |
Candidate{id} | Review{artifact}, evidence: [Handle]}`. It is shipped with
the batch (§3.2). The kernel stores only `payload_hash`; payloads never
enter derived state.

**A and not A.** O4 asks how `A` and `not A` connect. In this model `not A`
is never a record. A contradiction is a relation between two records,
asserted as input. A rule cannot conclude a Conflict (conclusions are
revisions, SD7), so no derivation introduces one.

Tests: T51 (P6) stays the gate regression. T78 adds the pair, the
one-side supersede and the per-branch cases.

## 3. Metadata and format 4 (D2)

### 3.1 Op and record changes

**`Op::RecordValidation`** gains two fields:

- `meta: Option<ContentHash>`;
- `if_absent: bool`. When true, the op fails with
  `ValidationExists{rule, curvec, verdict}` if a record for
  `(rule, curvec)` already exists on the branch. The check is in the
  kernel and the oracle, inside the transaction (§3.5 #5).
  - Workers always set it. A job claimed before an admin verdict can
    therefore never overwrite that verdict.
  - Admin `ctx validate` leaves it false, so latest wins (SD10).
  - In the §6.2 check order it is a CAS-class error, checked right after
    `CurvecMoved`.

**`ValidationRecord`** gains `meta: Option<ContentHash>`.

- `validator: String` keeps the short validator name, the display name in
  `ctx why`. `meta` holds everything else.
- The kernel and the oracle store and replay `meta`, but never read it, and
  it is not part of `sem`.
- **Merge** classifies validation records as today, except that `meta`
  is left out of the comparison:
  - only different verdicts conflict (`kernel_merge.rs:579`);
  - a source that changed only `meta` counts as unchanged, so the target's
    record is kept;
  - a record taken from the source keeps the source's `meta` (implementation
    note, milestone 1). `Take` carries the source's `meta`. Conflict payload hashes
  for validation entries exclude `meta`. With this rule, PROP7's "meta never
  changes derived state" holds (m6).

**`ResolveArtifact`** gains `note: Option<ContentHash>`.

**`MissingBlob`** extends to `RecordValidation.meta` and
`ResolveArtifact.note`. `OpenArtifact.payload_hash` stays unchecked, because
merge and anchor payloads are hashes of data the kernel already holds. P7
producers still ship their payload blob.

### 3.2 Blobs

- Metadata, notes, conflict and review documents are ordinary store blobs.
  They go with the batch through `Commit.blobs` (`service.commit(id, blobs,
  batch)`), so the single-owner blob store and its packing are reused, and
  nothing writes into `<data>/blobs` outside the store (M3).
- Each blob is a canonical wire encoding, capped at 64 KiB.
- `ValidationMeta{v: 1, kind: Deterministic | Admin | Llm, validator,
  version, model?, prompt_sha256?, label, rationale, evidence: [Handle],
  tokens_in?, tokens_out?, job?: [u8; 16], based_on?: ArtifactId, at}`.
  `label` is one of `Reinforcement | None | Conflict | BoundaryChange |
  NewRule`.
- `ReviewDoc{v: 1, suggestion: Verify | Reject | Investigate, rule,
  curvec, sides?, meta: ContentHash}` is the PendingReview payload.

## 4. Validators and the worker pool

### 4.1 Validator adapter (outside core)

A new crate `contextdb-validate` holds:

```rust
pub trait Validator: Send + Sync {
    fn kind(&self) -> ValidatorKind;          // Deterministic | Llm
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn validate(&self, task: &Task, deadline: Instant) -> Result<Outcome, ValidatorError>;
}
pub struct Task {
    pub job: [u8; 16],
    pub branch: BranchId,
    pub rule: RuleId,
    pub curvec: HeadVector,
    pub conclusion: Item,                     // handle, key, kind, body
    pub premises: Vec<PremiseChange>,         // handle, key, kind, asserted body, current body
}
pub struct Outcome {
    pub label: Label,
    pub rationale: String,                    // ≤ 2 KiB
    pub evidence: Vec<Handle>,                // must be handles from the task
    pub model: Option<String>,
    pub prompt_sha256: Option<ContentHash>,
    pub tokens: Option<(u32, u32)>,
}
```

- **Bodies are never truncated.** A task whose bodies exceed 256 KiB fails
  as `TooLarge`, and nothing is validated on partial content (m5).
- **Handle allowlist.** Evidence must name handles that appear in the task.
  An outcome that cites any other handle fails the job (SPEC §10, LLM
  threat row).

Implementations:

- **`Mock`** (tests): a scripted outcome per job, plus delays, failures,
  hangs and malformed replies on demand.
- **`Command`**: an external program. It gets the task as JSON on stdin and
  prints one JSON outcome on stdout, within the deadline.
  - It counts as `Deterministic` only if `validators.toml` says
    `trusted = true`; otherwise it is `Llm`.
  - That trust is self-declared by whoever writes the data directory. The
    threat row in SPEC §10 says so (m5).
- **`ClaudeCli`** (D1). It is always `Llm` and runs sandboxed (B3).
  - **Command:** `claude -p --model <m> --output-format json --tools ""
    --strict-mcp-config --mcp-config <empty file> --setting-sources ""
    --disable-slash-commands --no-session-persistence --system-prompt
    <fixed>`.
    - It loads no user, project or local settings, so their hooks do not
      fire (residuals below).
    - It runs no MCP server, so ContextDB is not reachable.
    - It has no tools.
    - It is not `--bare`, which would require an API key (D1).
  - **Environment:** an empty temporary working directory and a minimal
    environment (`HOME`, `PATH`, `LANG`). `CTX_*`, `CLAUDE_*` and the
    sockets' variables are removed.
  - **Process:** its own process group, killed at the deadline.
  - **Prompt:** the template is fixed, and its sha256 goes into the
    metadata. Untrusted bodies sit between per-job nonce fences, and the
    template says that text inside the fences is data.
  - **Reply:** the JSON envelope's result must be one JSON object
    `{label, rationale, evidence}`. Anything else is a failed job, never a
    label. That includes text outside the object and a reply that reports a
    tool call.
  - **Residuals.** Only `--bare` skips CLAUDE.md auto-discovery and
    auto-memory, and managed (enterprise) settings apply regardless of
    `--setting-sources`.
    - The worker therefore sets `CLAUDE_CONFIG_DIR` to an empty temporary
      directory with the subscription login copied in, when `ctx validators
      check` confirms that login still works there.
      - The copied login is a duplicated credential. The directory is
        created with mode 0700, outside `<data>`, and deleted after each run
        (and at daemon shutdown).
      - On macOS the login may live in the Keychain, so copying it may not
        be possible.
      - `ctx validators check` reports which mode is in use: isolated config
        or user config.
    - Otherwise the user's `~/.claude/CLAUDE.md`, memory and managed
      settings may still load. `ctx validators status` says so, and the
      threat row in SPEC §10 lists it.
    - Tools, MCP and project settings stay off either way.
  - **Login (implementation note, milestone 3).** On the owner's machine the
    standalone CLI has no login of its own; the interactive session's
    credentials do not reach child processes, and a nested `claude -p`
    cannot read the Keychain. The adapter therefore takes `token_file`: a
    long-lived subscription token from `claude setup-token` (still D1, not an
    API key), passed only as `CLAUDE_CODE_OAUTH_TOKEN`. A file readable by
    group or others is refused, and the token never appears in argv or
    errors (T82). This also works for `ctxd` under launchd. Smoke check: one
    real call returned `Conflict` with allowlisted evidence in 4 s.
  - **Tests (T82):** the exact argv and environment; that no hook ran and no
    memory file was read; an injection fixture in
    which a body tells the model to call a tool or open a Conflict; a fake
    `claude` that tries a tool call; a check that no ContextDB session is
    created during a run.

### 4.2 Label mapping

| Label | Trusted (Deterministic, Admin) | Llm |
|---|---|---|
| Reinforcement, None | `RecordValidation{Verified}` | PendingReview `suggestion: Verify` |
| Conflict | `RecordValidation{Rejected}` | PendingReview `suggestion: Reject` |
| BoundaryChange, NewRule | no record; outcome kept in the job row | PendingReview `suggestion: Investigate` |

- **An LLM never writes a ValidationRecord, whatever the policy.** Its
  PendingReview targets the rule's conclusion revision and does not gate.
  `sem` stays `Pending`, which every status already reports (I6).
- **Converting a suggestion.** `ctx validate --from-review <artifact>
  [--verdict]` is admin-only. In one batch it records the verdict, with
  `meta.kind = Admin` and `based_on` naming the review, and resolves the
  review. The SPEC §4.4 condition, independent evidence, is the caller's
  responsibility, and the metadata records who did it.
- **Dedup.** One validator review per job generation, enforced by the
  job's `client_txn_id` (§4.4). Payload hashes cannot dedup it, because the
  metadata includes the time and token counts. Before opening a review, the
  worker also skips the job if an open validator review for the same
  `(branch, rule, curvec)` exists. It reads this from the projection table
  `validator_reviews(branch, rule, curvec, artifact, open)` (§9). A manual
  retry is the exception: it opens a new review even if the old one is still
  open, since the retry was asked for. A new curvec gives a new review. The old
  review stays open until resolved, and `ctx_conflicts` marks it `stale
  curvec`.

### 4.3 Who may write a ValidationRecord

- **Daemon rule (protocol 5).** `RecordValidation` is accepted only from the
  Admin channel (`ctx validate`) and from the worker's in-process identity.
  An agent `Commit`, `WhatIf` or MCP `ctx_commit` that carries one is
  refused with `ChannelRestricted`. Agent `Raw` is already refused (T80).
- The kernel channel rule is unchanged, so old logs replay identically.
- **Worker principals.** The worker commits on the Admin channel. Trusted
  validators use `PrincipalId(0x5641_0001)`, LLM validators
  `PrincipalId(0x5641_0002)` ("VA"). Single-principal mode reserves both.
- The LLM path builds only `OpenArtifact{PendingReview}` batches. T50 checks
  that a committed batch from the LLM principal never contains a
  `RecordValidation` (property over 10,000 random outcomes), and the daemon
  also refuses such a batch before submitting it.

### 4.4 Jobs

**Discovery (M1).** Jobs come from the net delta, not from the per-revision
`status_diff`. For each published epoch and each branch it touched:

- every rule whose Sem premise logical had a head move (through the kernel's
  `sem_by_logical` index);
- every new or re-enabled rule with a Sem incidence;
- every rule that got a `RecordValidation{Pending}`, or a validation record
  brought in by a merge `Take`;
- on `CreateBranch` (empty net delta), every rule Pending on the new branch.
  The fork repeats the parent's pending jobs on the child, and the O2 report
  counts that duplicated cost;

where `sem` is now `Pending`. At start-up the scan covers all branches.

**Key.** `(branch, rule, curvec)`. The same rule pending on two branches is
two jobs.

**Durable job table.** `<data>/jobs.sqlite` (WAL), owned by the daemon:

```
jobs(claim_key PRIMARY KEY, branch, rule, curvec, generation, token, state,
     lease_until, attempts, last_error, outcome_meta)
```

- `state` is one of `Queued | Running | Done | Failed | Superseded`.
- `claim_key = H(branch, rule, curvec)`.
- This single-row-per-key table is SPEC §7's `jobs`: the "multi-slot" is
  the key space. Each key has one slot, and the token fences successive
  holders of that slot.

**Claim (T53).** Claiming is one SQLite transaction:

```
UPDATE jobs SET state = Running, token = token + 1, lease_until = now + deadline
WHERE claim_key = ? AND state = Queued
```

- It is marker-first: the row is durable before the validator is called.
- Of several concurrent claimers, exactly one update matches.

**Result.** The kernel CAS (`curvec_seen`, `if_absent`) and store
idempotency fence the commit itself. The token only fences the job row.

1. Build the batch: either `RecordValidation{rule, curvec_seen = curvec,
   verdict, meta, if_absent: true}`, or `OpenArtifact{PendingReview}`. Ship
   the blobs with it.
2. Commit with `client_txn_id = H("job", claim_key, generation)`, the same
   for every attempt of one generation. `generation` starts at 0, and only a
   manual retry increments it. Store idempotency (ADR 0008 §6.5) then allows
   at most one commit per job generation:
   - a retry with the same batch returns the first result;
   - a different outcome from a later attempt gets `TxnIdReuse` and is
     dropped.
3. If the commit fails with `CurvecMoved`, the job is `Superseded`; the new
   curvec is discovered as a new job (T49).
4. If it fails with `ValidationExists`, the job is `Done`, and the existing
   verdict stands.
5. Mark the job `Done` if the token still matches. Otherwise another worker
   holds the slot and has the same `client_txn_id`, so nothing is lost.

**Expiry.** A job whose lease has passed goes back to `Queued`, with its
token bumped and `attempts + 1`, and its worker's process group is killed.
A late result from it can only commit through the same `client_txn_id`.

**Recovery (M6).** The daemon holds the data directory's flock, so at
start-up every `Running` row belongs to a dead daemon. They all go back to
`Queued`. Before calling a validator, a worker asks `TxnStatus(client_txn_id)` for
the current generation. If that transaction already committed, the job is
marked `Done` without a call.

The start-up scan:
- creates jobs for keys with no row;
- requeues a `Superseded` row whose key is Pending again. After an ABA head
  move (v1 → v2 → v1), the v1 key runs again, which the kernel CAS allows,
  because the current vector is v1 once more;
- leaves `Done` and `Failed` rows alone, so a `BoundaryChange` outcome does
  not loop.

Runtime discovery applies the same rules.

`ctx validators retry <key | --failed>` requeues by hand and increments the
generation, so its commit is not answered by an earlier attempt's result.

**Implementation notes (milestone 4).**
- `jobs.sqlite` is opened by `ctxd` through `rusqlite` (bundled, as in
  `contextdb-store`); the daemon's dependency policy lists it. `validators.toml`
  is parsed in `contextdb-validate` (`toml`, parse only).
- Discovery reads each new op_log row's batch and result. It looks at the
  current snapshot, not the row's epoch: a rule that changed again later is
  also listed by the later row. A row it cannot read triggers a full rescan.
- Until the milestone 5 projection exists, the open-review check is a scan
  of the branch's open PendingReviews by the LLM principal. Within one job
  generation, `TxnStatus` answers first, so the scan only matters across
  generations.
- At shutdown, discovery is joined, but a worker inside a validator call is
  not waited for. Its row stays `Running` and is recovered at the next start.

**Limits** (`validators.toml`, defaults):
- 2 workers;
- a 120 s lease;
- 3 attempts, then `Failed`, listed by `ctx validators status`;
- an LLM call cap of 200 per hour per daemon. Over the cap, jobs wait.

**No validator is configured by default**, so P7 makes no LLM call unless
the owner configures one.

**Configuration.** `<data>/validators.toml` is read at start and on `ctx
validators reload`. It lists validators with their kind, command, model,
branch, scope or kind filters, and `trusted`. It is operational
configuration and never enters derived state (M2: r0.1's
`llm_may_verify` policy field is gone along with LLM verification).

## 5. Contradiction candidates (lexical)

- **When.** An epoch adds a head of kind Decision, Plan or Fact on branch
  `β`. The candidate thread then reads the new head's body and its
  highest-idf terms (at most 12). It queries a per-branch view of the
  recall index: logicals visible on `β`, with `β`'s heads, in the same
  scope. The query uses OR, and excludes the head's own logical, its direct
  premises and its direct dependents.
- **Index (B2).** The recall index gains a `branch` column and an OR query
  that returns bm25 scores. Rows are per `(branch, logical)`, and only
  branches with their own head differ from main.
  - A query for `β` filters by visibility on `β`: it drops main rows that
    `β` shadows or cannot see. It over-fetches before taking the top 5.
  - `ctx_recall` regression tests stay green with the new column.
- **Score.** Pairs with score ≥ `candidate_min_score` (calibrated in G6), at
  most 5 per new head.
- **Storage.** `<data>/candidates.sqlite`, with
  `candidates(id, branch, a_rev, b_rev, score, epoch, state)`, keyed by the
  branch and the unordered revision pair. A pair whose side is no longer
  visible on the branch is marked `gone`.
- **What happens next.**
  - If an LLM validator is configured for contradictions, it judges the
    pair under §4.1's sandbox. On `Conflict` it opens a PendingReview on the
    new head: `suggestion: Reject`, with both sides named. This does not
    gate.
  - In every case the pair is listed by `ctx_conflicts`. An agent or admin
    who confirms it opens the gating Conflict pair (D3).
- **Off the writer.** Candidate generation runs on its own thread and never
  delays a commit (G5).

**Implementation notes (milestone 6).**
- **One thread.** The recall indexer and the generator share a thread: each
  pass brings the index to the snapshot's epoch, then queries the heads
  moved since the candidate watermark. A `ctxd` stop joins it.
- **Rows carry their revision.** A reader on `β` keeps a hit only when the
  row's revision is `β`'s head. Main rows that `β` shadows or cannot see
  drop out without a per-branch visibility table.
- **Terms.** The query terms are the head's key and body terms that at
  least two rows hold, by idf over all rows (at most 12). A term only one
  row holds can pair with nothing.
- **Kinds.** Both sides are Decision, Plan or Fact.
- **Neighbours** are logical-level: premises of rules concluding any
  revision of the head's logical, and conclusions of rules citing one.
- **Backfill.** With no watermark (first start, or the log went backwards)
  the table is cleared and every live head is queried. A restart does not
  query again.
- **States.** `new`, `opened` (a Conflict was opened from it) and `gone` (a
  side is no longer the head, or the branch is archived). `list` re-checks
  both sides against the snapshot and fixes a stale state on the way.
- **Settings.** `validators.toml [candidates]`: `min_score` (1.0, initial)
  and `per_head` (5); reloaded with `ctx validators reload`.
- **Not yet.** LLM judging of candidate pairs is milestone 6b.

## 6. `ctx_conflicts` and `ctx_resolve`

All three operations act on the caller's branch.

- **`ctx_conflicts{list: {scope?, state?, limit}}`.** `state` is `open |
  review | candidates | all`. It returns:
  - open Conflicts with their pair and payload, marked `side superseded`
    where one side is no longer the head;
  - open PendingReviews, marked `stale curvec` where the curvec has since
    moved;
  - candidate pairs not yet acted on.
- **`ctx_conflicts{open: {a, b?, reason, from?: candidate | review}}`.**
  Opens the Conflict pair (or one side) in one batch, shipping its
  `ConflictDoc`. With `from = review`, the same batch resolves that review.
  It refuses:
  - `a == b`;
  - a side not visible on the branch;
  - a pair already open under the same `(ty, target, payload_hash)`.
- **`ctx_resolve{artifacts: [id], note}`.** Resolves the given artifacts in
  one batch, with a `ResolutionDoc` note. It accepts only Conflict and
  PendingReview; `AnchorBroken` stays observer-only (m4).
- **Who.** Agent and admin channels may do all three (D3). Each is an
  ordinary certified write, so it goes through `expect_cert_valid` when the
  caller asks for it.

**Implementation notes (milestone 5).**
- **Branch.** Agents must give a session binding, and act on its branch. An
  unbound admin call names a branch or uses `main`. A binding together with a
  different branch name is `BranchMismatch`.
- **Grouping.** `list` groups Conflicts by their shared `ConflictDoc`. A
  Conflict without one (a P6 merge conflict) is listed per artifact, with
  origin `merge`.
- **Candidates.** `from = candidate` was refused until milestone 6.
- **Projections.** §9's `validator_reviews` table is replaced by an
  in-memory cache of decoded review payloads in the worker. Payloads are
  immutable, so no blob is read twice. `list` reads review payloads directly;
  the open set is small.
  - `validation_meta` and `artifact_notes` are not built in P7, because no
    reader needs them yet. Metadata and notes are reachable from the record's
    and the op's hashes.

## 7. Handoff

**Storage.** Handoffs live in the session log (SPEC §3.5 #7's enumerated
session-log domain gains `HandoffBegin` and `HandoffAccept`). They replace
SPEC §7's `handoffs(id, owner, state, cert_id, summary)` table, which
becomes a projection of those events (§12).

**`ctx_handoff_begin{summary, to_agent?}`:**

- Issues the caller's session certificate, as `ctx_check` would. If the
  certificate is neither Valid nor ValidRecertified, it is refused with
  that verdict: a handoff passes on a certified state, not a stale one.
- Writes `HandoffBegin{id, owner: (session, agent), branch, cert_id,
  summary, to_agent, epoch}`. The summary is at most 8 KiB and lives in the
  event itself, since no commit carries it into the blob store.
- Flushes before replying.
- Is refused on an ended session.

**`ctx_handoff_accept{id}`:**

- **Claim once.** The CAS `Open → Accepted{by: (session, agent)}` happens
  under the session-log writer, and the reply is sent only after the
  `HandoffAccept` event is durable.
  - If the flush fails or times out, the daemon enters a daemon-level
    `session_log_failed` state. This is not the per-session `log_broken`
    flag, which means the op_log was cut. In that state the daemon refuses
    further handoff operations until restart.
  - On replay the durable event decides, so two accepts can never both
    succeed, even across a crash (T52, T83).
  - An acceptor who retries after a timeout and finds its own claim durable
    gets its own `Accepted` back, not `HandoffTaken`.
- **Checks.**
  - `to_agent`, when set, must equal the acceptor's agent id; otherwise the
    accept is refused with `HandoffNotAddressed`.
  - The acceptor's session must be on the handoff's branch; otherwise the
    accept is refused with `BranchMismatch` (SD19).
- **Reply.** It validates the handoff's certificate as `ctx_check{cert_id}`
  does, and returns the verdict, the named changed reads and the summary.
- **Read set.** It copies the certificate's read entries into the
  acceptor's ledger at their original revisions, as P4 does for a subagent
  (ADR 0010 §7). An entry for a logical the acceptor has already read is
  skipped: the acceptor's own read is the later one. The acceptor's next
  check is then Invalid exactly when a handed-over read changed (T79).

A handoff is not a lock. Its owner may keep working, and expiry is not
modelled.

## 8. `ctx_changes`

- **Source (M4).** It serves the projection's durable `diff_log(branch,
  epoch, revision, post)`, which persists across restarts. The projection is
  caught up to the store before serving.
- **`ctx_changes{since_epoch, scope?, branch?, max_items, wait_ms?}`.**
  `branch` defaults to the session's. It returns the items after
  `since_epoch` in epoch order, with the epoch reached and `more`.
  - With `wait_ms` (≤ 30 s), it blocks until the branch publishes a later
    epoch or the wait ends. `ctx changes --follow` loops on it.
  - It needs no in-memory ring. Waiting requests are bounded at 64 per
    daemon; more are refused with `Busy`.
- **Truncated.** The reply is `Truncated{earliest}` only when the projection
  was rebuilt from a later point, or is behind and cannot catch up within
  the request. The client then re-reads with `ctx_check`. Nothing is
  silently skipped.
- **T76:** the concatenated pages equal the replay's per-epoch
  `status_diff`s for the branch between the two epochs.

## 9. Protocol, formats and migration (M7)

**op_log format 4.**

- `RecordValidation` (tag 8) and `ResolveArtifact` (tag 13) gain fields
  under new tags, 18 `RecordValidation4` and 19 `ResolveArtifact4`.
- **Canonical encoding rule.** An op with `meta = None`, `if_absent = false`
  and `note = None` encodes under its old tag (8 or 13). Only an op that
  uses a new field encodes under 18 or 19. Every format-3 batch therefore
  has identical canonical bytes and an identical `batch_hash` under format 4
  (`migrate.rs:381`, `log.rs:232`). T77 asserts byte equality.
- New errors `ValidationExists` and the extended `MissingBlob` get new tags.
- `Inputs` encodes `ValidationRecord.meta`, with a version bump.
- Old binaries stop with `FormatTooNew`.

**`ctx migrate` 3 → 4.**

- It re-frames the log: old ops keep their old tags. That means re-writing
  frames with the new format number, not re-encoding any op.
- Every result is re-derived. Since no format-3 op changes meaning, any
  difference is a bug and is printed (T77).
- It writes `log.v3.bak`. An interrupted migration resumes. Rollback goes
  through the backup, and only before new commits land.

**Checksums and idempotency.** Re-framing changes the log checksum, so a
certificate issued before migration fails `log_checksum_at_issue` and reads
`Invalid(LogMismatch)`. Sessions are restarted after the upgrade, as for
0.2.0. Stored idempotency hashes cover the batch, not the frame, and stay
valid. T77 checks both.

**Other state.**

- **Snapshots:** they follow the log format; old snapshots are ignored and
  rebuilt.
- **Projections:** schema 3 adds the `branch` column to recall, plus
  `validation_meta(branch, rule, curvec, meta)`, `artifact_notes` and
  `validator_reviews(branch, rule, curvec, artifact, open)`. A
  version mismatch rebuilds them, into `projections.v4.sqlite` so a format-3
  binary keeps its own file.
- **PolicyDoc:** unchanged; r0.1's field is withdrawn.
- **Session log:** it gains the `HandoffBegin` and `HandoffAccept` tags. An
  old binary reading a log with them stops with `FormatTooNew` at replay;
  rollback restores the session log from `session.v3.bak`, which migrate
  writes.
- **EventLog:** v2 adds the new tags. Exporting to v1 refuses a log that uses
  them.

**Protocol 5**, in lockstep with ctxd, ctx, ctx-mcp and ctx-hook:

- `Validate` gains `label`, `evidence` and `from_review`, and ships a meta
  blob.
- New requests: `Conflicts`, `ConflictOpen`, `Resolve`, `HandoffBegin`,
  `HandoffAccept`, `Changes` and `Validators{status | reload | retry}`.
- `Commit` and `WhatIf` refuse `RecordValidation` (§4.3).

**Upgrade of live stores:** `ctx shutdown`, install, `ctx migrate`, `ctx
service restart`, then restart agent sessions.

## 10. O2 measurement

Validation stays per rule. G6 reports:

- how many jobs one head move creates, over the dogfood store and the
  generator;
- how many validator calls a per-revision union vector would have saved.

The decision is recorded after P7.

## 11. Verification plan

| Gate | Items |
|---|---|
| G1 | fmt, clippy, unit tests for every new module |
| G2 | **T49** a result whose curvec moved gets `CurvecMoved`, the job becomes Superseded, and the new curvec is validated once. **T50** an LLM `Conflict` gives a PendingReview and `sem` stays Pending; over 10,000 random LLM outcomes no `RecordValidation` is committed from the LLM principal, and the daemon refuses one if built. **T52** a handoff is accepted once, under 8 concurrent accepts and after a restart. **T53** concurrent identical jobs commit once; a crash at each step (before claim, after claim, after commit, after Done) and a lease expiry end with exactly one record or one PendingReview, and at most one repeated validator call. **T76** `ctx_changes` pages equal the replay's per-epoch diffs; `Truncated` only after a rebuild. **T77** 3 → 4 migration keeps digests and derived state; pre-upgrade certificates read `LogMismatch`; idempotency survives. **T78** SD20 pair, one-side supersede (`side superseded`), per branch; `ctx_conflicts open` refusals. **T79** the handoff copies the read set (skipping logicals already read), and the acceptor turns Invalid exactly when a handed-over read changed; `to_agent` and branch refusals. **T80** an agent `Commit`, `WhatIf`, MCP `ctx_commit` or `Raw` with `RecordValidation` is refused with `ChannelRestricted`. **T81** an admin `Rejected` committed after a worker's claim is never overwritten (`ValidationExists`). **T82** the `ClaudeCli` sandbox (§4.1). **T83** a crash between the handoff claim and its flush never lets two accepts succeed. **T84** the same rule pending on two branches gives two jobs and two records; candidates never pair a revision invisible on the branch; validator review dedup per curvec |
| G3 | PROP1's generator (`generate_p1`) adds Conflict and PendingReview open/resolve, and RecordValidation with `meta` and `if_absent`; exact comparison with both oracles. PROP7 metamorphic: `meta` and notes never change derived state, including through merge. A discovery property: after quiescence, every `(branch, rule)` with `sem = Pending` has a job row, checked against the oracle. P6 properties stay green |
| G4 | stress: 4 writers moving curvecs on 2 branches while 4 mock workers validate, 30 min, with no lost or duplicate record; kill -9 crash injection at each job step; the job table and blob store under disk-full |
| G5 | with mock validators: commit p99 latency with workers busy within 5% of idle; candidate generation p99 under 20 ms per new head, off the writer; a `ctx_changes` long-poll wakes within 10 ms (p99) of the epoch. Memory: the job table and candidate table bounded by keys, reported at 10⁵ pending rules |
| G6 | (a) deterministic mock scenarios; (b) one real LLM run (D1): about 100 jobs and 100 candidate pairs from the dogfood and generated stores, with independent reference labels (a deterministic oracle for generated pairs, independent agents for the rest), reporting calls, tokens, agreement and disputed items, separately from kernel correctness, with provenance per SPEC §4.4; (c) O2 numbers; (d) the `candidate_min_score` calibration |
| G7 | README (validators off by default, how to configure `claude -p`, how a suggestion is converted), CHANGELOG, SPEC amendments (§12) |
| G8 | independent implementation review |

## 12. SPEC amendments (before implementation)

- **§2:** SD20 Conflict (from §2), with O4 marked answered. SD10 gains
  `if_absent`.
- **§3.5 #7:** the session-log domain gains handoff events.
- **§6.1:**
  - `RecordValidation.meta` and `.if_absent`;
  - `ResolveArtifact.note`;
  - op_log format 4.
- **§6.2:** the new errors `ValidationExists`, `HandoffTaken`,
  `HandoffNotAddressed` and `BranchMismatch` (for handoff), and the extended
  `MissingBlob`.
- **§6.3:** the P7 tools. In the `ctx_validate` row: label, evidence,
  `from_review`, and the agent-channel refusal.
- **§7:** `jobs` as specified in §4.4. `handoffs` becomes a projection of
  session-log events. Add `candidates`.
- **§8.1:** T76–T84.
- **§10:** the LLM threat row points at §4.1–§4.3 (sandbox, no records,
  self-declared `trusted`).
- **Appendix C:** the version entry.

## 13. Review history

**r0.1 independent review (2026-11-02):** 4 blockers, 7 majors, 7 minors.
G0 was not acceptable.

| Finding | Resolution in r0.2 |
|---|---|
| B1 LLM `Verified` contradicts SPEC §4.4, §11, §3.5 #14 | An LLM never writes a ValidationRecord; it only suggests through PendingReview, which an admin converts with independent evidence (§0, §4.2) |
| B2 branches missing | per-branch job key, discovery, candidates and recall rows, handoff refusal, `ctx_changes` default (§2, §4.4, §5, §7, §8; T84) |
| B3 `claude -p` not sandboxed | no tools, MCP, settings or hooks; minimal environment and empty cwd; nonce fences; process group; T82 (§4.1) |
| B4 check-then-act job fence; LLM could overwrite a trusted verdict | `client_txn_id = H(job key)` (one commit per job, through store idempotency); in-kernel `if_absent` CAS (§3.1, §4.4; T81) |
| M1 discovery from `status_diff` misses rules | from the net delta through `sem_by_logical`; a quiescence property (§4.4, G3) |
| M2 `llm_may_verify` in PolicyDoc | withdrawn with B1 |
| M3 second blob store | the store's blobs, shipped with the batch; `MissingBlob` extended (§3) |
| M4 volatile ring | served from the durable `diff_log`; T76 restated (§8) |
| M5 handoff durability and gaps | flush before reply, a broken log on failure; precedence, `to_agent`, a non-Valid owner, the branch rule, the SPEC #7 amendment (§7; T83) |
| M6 recovery | every `Running` row at start is dead (flock); leases; `TxnStatus` before a call; retry; scan rules; review dedup (§4.2, §4.4) |
| M7 migration thin | §9 rewritten after ADR 0013 §8 |
| m1 facts | corrected in §1 (`PrincipalId` u32, no committing in-process identity, `generate_p1`, blob store, `diff_log`, `validator` vs `meta`) |
| m2 refusal code | `ChannelRestricted`; T80 covers Commit, WhatIf, MCP and Raw |
| m3 SD20 | `side superseded`; the duplicate check uses `(ty, target, payload_hash)` |
| m4 `ctx_resolve` scope | Conflict and PendingReview only |
| m5 truncation and self-declared trust | `TooLarge` instead of truncating; the threat row says `trusted` is self-declared |
| m6 merge and `meta` | merge leaves `meta` out of classification and out of conflict hashes |
| m7 gates | percentiles, mock validators, memory; T81–T84; "one record or one PendingReview"; multi-slot answered (§4.4) |

**r0.2 closure check (2026-11-02):** B1, B3, B4, M2–M4, M7, m1–m7 were
closed; B2, M1, M5 and M6 were partly closed. New findings: 2 majors, 5
minors. G0 was acceptable after text fixes.

| Finding | Resolution in r0.3 |
|---|---|
| N1 fixed `client_txn_id` with terminal rows (ABA stuck, retry cannot commit) | `generation` in the id, incremented by a manual retry; a `Superseded` row whose key is Pending again is requeued (§4.4) |
| N2 discovery misses forks and Pending verdicts | `CreateBranch` queues the child's Pending rules (cost reported in O2); `RecordValidation{Pending}` and merge-taken records are triggers (§4.4) |
| N3 handoff on log failure | a daemon-level `session_log_failed` state, not `log_broken`; an idempotent own accept; ValidRecertified accepted; the summary inline in the event (§7) |
| N4 canonical encoding across migration | old tags when the new fields are unset; byte-identical batches (§9; T77) |
| N5 sandbox residuals | `CLAUDE_CONFIG_DIR` when the login works there; otherwise residuals reported and listed in the threat row; T82 asserts no hook and no memory (§4.1) |
| N6 clarifications | dedup through `client_txn_id` plus the projection's `ReviewDoc` key; `ValidationExists` after `CurvecMoved`; merge leaves `meta` out of classification, and only verdicts conflict (§3.1, §4.2) |
| N7 recall visibility | filter by visibility on the branch, over-fetch; recall regressions (§5) |

**r0.3 check (2026-11-02):** N1–N7 were closed, and **G0 was acceptable**.
Three text gaps were fixed in place:

- `generation` added to the `jobs` schema;
- a `validator_reviews` projection, plus the manual-retry rule;
- the copied login's directory: mode 0700, outside `<data>`, deleted after
  each run; Keychain caveat; the mode reported by `ctx validators check`.
