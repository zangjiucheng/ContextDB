# ADR 0013 — P6 versioned transactions, branches and WHAT_IF

Status: **accepted r0.4**, 2026-10-23, with the r0.5 implementation note
(§3.1), the r0.6 merge-base implementation (§6.1) and the r0.7 fixes from
the independent implementation review (§13). **P6 G0 passed.** SPEC v1.6.8
mirrors this ADR, and the owner confirmed the SD17 gate on 2026-10-23. r0.1 was
reviewed independently: 4 blockers, 12 majors, 6 minors. The closure check
of r0.2 found 6 findings partial and 4 new majors plus 5 minors. The
re-check of r0.3 found R1–R4. r0.4 answers all of them (§13). It finalises SD19 and lists the SPEC amendments that must
land before implementation (§12). Until G0 passes, nothing here is
implemented.

## 0. Scope and decisions taken

P6 implements SPEC §5 P6:

- multi-head atomic commits with read-set certification (first committer
  wins);
- isolated branches: per-branch state, cycle graph and derived state;
- `WHAT_IF` as an undo-journal dry run;
- explicit merge with three-way classification;
- `BRANCH_ONLY` currency.

Gate items: T14, T19 (variants in §11), T20a, T20b, T47, T48, T72, T73,
T75, the S13-rows-e2e `BRANCH_ONLY` row, PROP13 and PROP7 extended to
branch ops. Per §6.4, T51 and the S13 `CONFLICTED` row move here from P7.

**Owner decisions (2026-10-23):**

- **D1, git following.** Branches are created explicitly. A session starts
  on the branch whose name equals the repository's current git branch if it
  exists and is open, otherwise on `main`. A git merge is ratified by an
  explicit `ctx merge` / `ctx_merge`.
- **D2, merge permission.** Agents may create branches, commit and merge,
  including into `main`. `require_certified` defaults to **true**.

**D3, SD17 (confirmed 2026-10-23).** A minimal Conflict gate moves into P6
(§6.4).

Out of scope:

- branch deletion (a branch can only be archived);
- automatic git-merge detection;
- merging observer-domain state (§6.3);
- P7 conflict producers (contradiction candidates);
- ACLs.

## 1. Today

Code facts, all confirmed by the reviewer:

- `BranchId` (u16, `MAIN = 0`) exists, as do `Batch.branch` and the
  `origin_branch`/`origin_epoch` fields on logicals, revisions, rules and
  artifacts.
- The op_log frame's `branch` is checked against the batch.
- `Certificate.branch` is part of `cert_id`, and `SessionEvent::Start` has
  a `branch` field.
- All mutable and derived state is single and global.
- The kernel (`core/src/lib.rs:439`) and the oracle (`oracle/src/input.rs:32`)
  reject non-main batches with `UnknownRef`.
- `impact_exact` clones the state (`history.rs:187`).
- `ReadSetConflict` and `MergeNotCertified` are never produced.
- Hard-coded `MAIN`: `service.rs:642,1054,1354,1871`,
  `anchor_checks.rs:13,129`, `anchors.rs:402,413`,
  `store/src/projection.rs:58,65`.

## 2. Branches

**Branch table** (input layer, global, in snapshots). Each entry has:

- `id: BranchId`. The u16 range includes `main`, so at most 65,535 more
  branches can be created; beyond that the batch fails with
  `CapacityExceeded{Branch}`.
- `name`, unique. It uses the git ref character set (`[A-Za-z0-9._/-]`,
  1–128 bytes, no leading `/` or `-`, no `..`), so every ordinary git
  branch name maps one to one (D1). `main` is reserved.
- `parent: BranchId` and `fork_epoch: Epoch`.
- `state: Open | Archived`.
- `syncs`: the sync points of §6.1.

New Ops:

- **`CreateBranch{name}`** forks the batch's branch at this commit's epoch
  `E` (`fork_epoch = E`). It must be the only Op in its batch, otherwise
  `SoleOpRequired`. A taken name gives `BranchExists{name}` and an invalid
  one `InvalidBranchName`.
- **`ArchiveBranch{name}`** runs on the branch being archived. Later batches
  on that branch fail with `BranchArchived` (batch level, §9). `main`
  cannot be archived (`InvalidBranchName`). Objects stay where they are
  visible. An archived branch's derived state may be evicted and rebuilt
  on demand.

Branch names are not secret: `ctx branch list` shows them all.

**Visibility (SD19, normative, recursive; review B1).** `vis(b, o, E)` means
object `o` is visible in branch `b` as of epoch `E`. It holds when any of
these is true:

1. `origin_branch(o) = b` and `origin_epoch(o) ≤ E`;
2. there is a `branch_visibility` row `(b, o, m)` with `m ≤ E`, written by
   a merge into `b` or a cross-branch dedup on `b`;
3. `b ≠ main` and `vis(parent(b), o, min(E, fork_epoch(b)))`.

Visible now means `vis(b, o, ∞)`. Rule 3 inherits everything the parent
could see at the fork: its own objects, its merges and dedups, and its own
ancestry. So "merge `b` into `main`, then fork `c` from `main`" lets `c`
see `b`'s merged revisions.

**Implementation.** Each branch holds visibility bitsets over the global
arena ids (logicals, revisions, rules, artifacts). At fork, the child's
bitsets share the parent's (COW, §3). Each later visible object sets a bit
in that branch only. A check is O(1).

**Bitsets answer "visible now" only (review N11).** A historical
`vis(b, o, E)` also filters by `origin_epoch` and by each row's `m`.

A reference to an object that is not visible gives `UnknownRef`, exactly as
for a nonexistent one.

Logical keys stay globally unique, and `LogicalExists{key}` reveals nothing
else (T75).

**Accepted information flow (review m1).** Global dense ids and capacity
counters can reveal that other branches allocate objects (id gaps, or a
`CapacityExceeded` caused elsewhere). The isolation guarantee (§3) covers
visible inputs, cycle graphs, derived state and digests, not id values.
PROP13 compares under canonical renaming.

## 3. Per-branch state (O5 resolved: shared-spine COW)

Each branch owns a `BranchState`:

- **Inputs:** `heads`, `base`, `enabled` (moved out of `RuleArena`),
  `artifact_open`, `anchor_unobserved` (§6.2), `validations`, `metadata`
  (moved out of `LogicalArena`), `watermarks`, `policy`, the visibility
  bitsets and its `syncs`. All of these are in the branch's canonical inputs
  projection, and so in its digest.
- **Derived:** `CountingState` and `LayerColumns` columns, the
  Pearce–Kelly graph over **visible** rules (SD5), topo rank, `ChangeLog`,
  `history` and its `DerivedSnapshot`.

**COW shape (review M7).**

- A column is `Arc<Spine>`, where `Spine = Vec<Arc<[T; 256]>>`.
- A map is a chunked map: 64 hash buckets, each an `Arc<BTreeMap>`.
- The Pearce–Kelly adjacency and rank arrays are COW columns too.

**Fork** clones one `Arc` per component: O(1) in the store size, with no
history replay and no element copy. Writes copy lazily:

- the first write to a column after the fork copies its spine (O(N/256)
  pointers) once, then copies each written chunk;
- the first write to a map bucket copies that bucket.

Arenas and indexes stay global and append-only. Every index walk tests the
branch's visibility bit, which is O(1). Indexes such as `rules_by_premise`
also list rules invisible on the branch, so propagation pays one bit test
per invisible neighbour. G5 measures that overhead with many private rules
(§11).

**Digest per branch.** It is `blake3` of the branch's canonical inputs
projection (visible objects in canonical order, plus overlay) and its
derived snapshot. Head currency is excluded (§7).

Branch-table entries and sync points are counted in the digest of the
branch that **receives** them, and only there (review N4):

- a fork counts in the child's digest;
- a merge's sync point counts in the target's digest.

So a fork never changes the parent's digest, and a merge never changes the
source's. The entry covers the name, parent, fork epoch and archived flag; a
sync point counts as `(source, epoch)`. `main` without merges hashes as its
database alone, so single-branch digests are unchanged (r0.7).

**Isolation (normative).** A commit on `b` changes only:

- `b`'s `BranchState`;
- `branch_visibility` rows for `b`;
- the global arenas and indexes, whose new objects are invisible elsewhere;
- the global key namespace;
- the branch table (`CreateBranch` adds an entry; `Merge` adds a sync
  point).

Other branches' visible inputs, cycle graphs, derived state and digests do
not change (T14, T47, PROP13). A fork's parent is not modified: the
parent's live state is untouched, and the child shares its chunks
read-only.

**Memory.** Each branch costs its touched chunks, plus its Pearce–Kelly
copy after its first structural commit. G5 reports the per-branch overhead.
There is no hard budget in P6 (initial). Archived branches' derived state
can be evicted.

### 3.1 Implementation note (r0.5, 2026-10-23)

The first kernel milestone realises §3 with coarser sharing. Semantics are
unchanged; only the representation differs.

- **One single-branch `Db` per branch.** It holds exactly the objects
  visible on that branch, under branch-local dense ids. Visibility is
  therefore membership; there are no visibility bitsets and no invisible
  neighbours to skip. Isolation holds by construction: a commit runs on one
  branch's database.
- **Global ids at the boundary.** The kernel keeps the global counters and,
  per branch, a local ⇄ global id map. Batches arrive with global ids and
  results and errors leave with them.
  - A global id with no local counterpart maps to a sentinel, so the branch
    resolver reports `UnknownRef` at the right Op (§6.2 order).
  - A batch may name an object it staged by the global id that object will
    receive (`next + i`), exactly as the single-branch engine allows with
    dense ids.
  - Errors that name staged objects report the global id they would have
    received.
- **Global key namespace.** The branch database consults a callback for
  keys taken on other branches, so `LogicalExists` (T75) is raised in its
  normal place.
- **Cross-branch dedup (contract 5, §11a).** The kernel keeps global
  indexes of revisions by `(logical, content)` and of rules by canonical
  form under global ids, plus each object's provenance. The branch database
  reports the revisions and rules a batch staged. A staged object identical
  to one created elsewhere receives that object's id: it is an import,
  recorded as a `branch_visibility` row `(b, o, epoch)`, and the branch's
  copy carries the original's author and origin. Base, rules and
  validations are not imported, since the copy is a new object of the
  branch database (T73). An imported rule enters the branch's cycle graph
  like any new rule.
- **Naming staged objects.** Which global id a staged object receives is
  known only after staging. When a batch names a revision or rule id that
  is invisible on the branch, and imports are possible, the kernel stages
  it again with the ids the previous attempt assigned until the two agree.
  The first attempt runs on a copy, so the retry costs one database copy,
  and only for such batches.
- **Fork** clones two `Arc`s: O(1) in the store size. The **first write**
  to either side copies that branch's whole database (`Arc::make_mut`),
  O(N), once; later writes are ordinary in-place commits. This replaces the
  per-chunk copy of §3. G5 measures fork time and first-write time. If the
  first write misses the branch-commit budget, the chunked columns of §3
  move inside `Db` behind the same kernel interface.
- **Digest per branch.** It is the branch database's digest under its local
  ids. Local ids are dense in creation order on that branch, so the
  projection is canonical. Branch-table entries and sync points join it
  with the merge milestone.
- **Equivalence.** With only `main`, global and local ids coincide and the
  kernel returns the single-branch results, errors, epochs and digests
  (tested on 1,000 generated traces).
- **Canonical order (decision).** SD15 picks the active rule with the
  smallest RuleId as the canonical witness. SD13 picks the failing
  incidence with the smallest RevisionId. Several sites order by id
  likewise. On a branch, "smallest" follows the branch's canonical order:
  the order in which objects became visible on that branch. A fork
  inherits its parent's order; a commit appends in staging order (fresh
  objects and imports alike); a merge appends newly visible objects in
  ascending global id, then its Conflicts. These are exactly the local ids.
  The order depends only on the branch's own history, never on when other
  branches allocated ids, so isolation holds under canonical renaming
  (review m1). On `main` with no imports it is the global order.

## 4. Transactions: multi-head atomicity and read-set certification

A batch is already atomic, so several ops in one batch commit together or
not at all (T48).

**`ExpectHeads{entries: Vec<(LogicalRef, RevisionId)>, source:
Client | Certificate(cert_id)}`** (review B3, M2):

- Checked at the **end** of the batch, after `final_policy`. Order: A5
  `PolicyViolation` first, then `ReadSetConflict`.
- Each entry compares against the logical's **head on the batch's branch
  before the batch**: the committed pre-batch state, not the staged one. A
  plan that reads A and B and then supersedes A in the same batch commits
  correctly.
- A mismatch fails with `ReadSetConflict{logical, read, now}`, where `now`
  is the pre-batch head.
- Two entries for one logical with different revisions fail with
  `InvalidReadSet{logical}`.
- `LogicalRef::New` is not allowed (`InvalidReadSet`).
- `ExpectHeads` may accompany `Merge` (review m5).

**`ctx_commit(ops, expect_cert_valid?, client_txn_id?)`** (daemon; review
M1, M2):

1. **Idempotency first.** A known `client_txn_id` returns its stored result
   when the stored *client hash* matches; a mismatch gives `TxnIdReuse`.
   The client hash covers the client's ops and `expect_cert_valid`, and
   the branch, channel and principal the batch commits as (the batch's own
   fields, contract 1). It does not cover the cert id: a `ctx_check` may
   re-issue the session certificate, and a retry of a durable commit must
   still find its result (r0.7). Daemon-appended
   `ExpectHeads{source: Certificate}` ops are left out of it. It is
   persisted as a format-3 `LogRow` field (§8, T68), so retries after a
   restart also match. A client-sent `ExpectHeads{source: Certificate}`
   fails with `ChannelRestricted`: only the actor may append one (review
   N6). The daemon refuses it before the kernel's checks, so this one
   error precedes the §9 order. With no session certificate,
   `expect_cert_valid` is refused with the `Empty` verdict.
2. **Validation in the actor.** With `expect_cert_valid=true`, the request
   carries the session's `cert_id`. The **actor** validates the
   certificate against its current state, after earlier batches in the same
   group and before staging this one. The whole certificate verdict counts
   (heads, base, enabled, validations, watermarks; §6.4), not heads alone.
   - Not Valid/ValidRecertified: refused with that verdict, nothing
     committed.
   - ValidRecertified: the **new** vector is used.
3. **The kernel check.** The actor appends
   `ExpectHeads{source: Certificate(cert_id)}` with the vector's
   `(logical, head)` pairs, then stages. Validation and the kernel check run
   on the same state with no gap (§3.5 #13), so the first committer wins
   and write skew fails (T19).

On replay, `ExpectHeads` is re-checked from the logged batch, and the
result is deterministic.

## 5. WHAT_IF and IMPACT{Exact}: one undo journal (review M6)

`Db::what_if(branch, batch) -> Result<Diff, TxError>` runs on the actor's
**standby replica** (ADR 0008 §3.3). Readers never see the hypothetical
state (I4). It stages and applies the batch to the replica's branch state
under an undo journal, computes the diff, and rolls back. It writes no
op_log, allocates no persistent id and advances no epoch (T14).
`IMPACT{Exact}` is the same call, returning reachability.

The journal restores everything a commit touches:

| Part | Mechanism |
|---|---|
| Arenas: logicals, revisions, rules, artifacts, adjacency including per-premise overflow rows | Lengths and overflow-row log. |
| Logical key hash table | Inserted-key log. |
| `branch_visibility`, branch table, sync points | Truncate; a replaced sync point keeps its prior entry in the journal. |
| Overlay columns and maps (including `anchor_unobserved`), visibility bitsets | First-touch pre-values, including COW copies (the pre-copy `Arc` is restored). |
| Counting rows | `Maintenance.before`. |
| Layer columns | First-touch log. |
| Pearce–Kelly graph including `V_l` creation | Its journal. |
| `ChangeLog` including `rebindings` | Entry log. |
| `history` | Truncate. |
| `DerivedSnapshot` `OnceLock` and digest cache | Saved and restored. |
| `stats`, timing | Saved and restored (PROP5). |

**Test.** After `what_if`, the whole `Db` equals its pre-state: digest,
`history`, `ChangeLog`, `stats` and canonical bytes. The oracle's diff
equals that of a real commit of the same batch on a copy.

**Cost.** O(affected), and **G5 requires ≤ 2× an equivalent real commit**,
excluding durable sync. The clone-based `impact_exact` is removed.

**Implementation (r0.5).** `Db::what_if` (`core/src/whatif.rs`) opens a
journal, runs the ordinary commit path and rolls back:

- **Maintainers.** The counting maintainer and the layer walk write, in
  pre-existing rows, only the popped revision's row, the rules concluding
  it and their incidences. Each saves that footprint when it pops a
  revision, so the journal is O(popped). FullRecompute saves its whole
  state, since it rebuilds everything anyway.
- **Pearce–Kelly.** A structural commit leaves the staging journal open
  instead of committing it, and rollback undoes it.
- **Indexes.** Appends are not compacted while a journal is open, so
  rollback pops them from the overflow rows.
- **Inputs.** The commit's own undo (heads, base, enabled, validations) is
  kept; policy, watermarks, metadata and artifact states are saved on first
  touch; arenas truncate, and the logical key table removes keys by
  backward shift.
- **ChangeLog, history, epoch, stats.** The change-log record is undone;
  history, epoch, stats and timing are restored. A materialized derived
  snapshot is set aside, not patched.

`impact_exact` is now this call. `Kernel::what_if` adds global ids: the
result names the ids a commit would allocate, imports included, and
branch Ops are validated without being applied. A dry run on a branch that
still shares its database with a fork copies it first, like a first
write.

## 6. Merge

`Merge{source: BranchId, require_certified: bool, certify:
Vec<LogicalRef>}` runs on the target branch (the batch's branch). It may be
accompanied only by `ExpectHeads`, otherwise `SoleOpRequired`. The source
must exist and differ from the target, and may be archived; a violation
gives `InvalidMerge`.

### 6.1 Merge base (review B2)

A **sync point** is written when one branch's state is made available to
another:

- at `CreateBranch`, the pair (parent, child) gets base = the parent's
  inputs projection at `fork_epoch`;
- at `Merge{S}` into `T`, the pair `{S, T}` gets base = **S's** inputs
  projection at the merge epoch. `T` now contains it, and `S` still does.

The branch table stores each sync point as `syncs[{a, b}] = (epoch, base)`,
an `Arc`-shared COW inputs projection (heads, base, enabled,
`artifact_open`, validations, metadata, policy, and the visible-object
set). Sync points are inputs: they are in snapshots and the digest, they
are rebuilt by replay, and the merge result is a pure function of the
inputs and the batch (ADR 0006 §7). They cost only the chunks that later
diverge.

**Containment (review N1).** Read every sync point as an edge: the
state `src@e` is contained in `dst` from epoch `e` on. Branch history is
also an edge, since `b@e1` is contained in `b@e2` for `e1 ≤ e2`.

A stored state `X` is **contained in** branch `Y` now when either:

- it is `Y`'s own earlier state; or
- it is contained in some `Z@e'` whose sync into `Y` happened at or before
  now.

This relation is transitive, and finite over the stored sync points.

`B` for merging `S` into `T`:

1. **Candidates.** The stored sync-point states contained in both `S` and
   `T` now. Fork bases always qualify through the ancestry.
2. **Maxima.** Those candidates not contained in any other candidate.
3. **One maximum:** that is `B`.
4. **Several maxima (criss-cross; fail closed):** for each value, if
   `S = T` keep it; else if every maximal candidate gives the same base
   value, use it as `B`; otherwise open a **conflict** on that value.

**Example.** `S` moves `s0 → s1`; `s1` reaches `T` through `C`; then `S`
reverts to `s0` (SD12 allows ABA). The sync `S → C` stored `S`'s state
holding `s1`, and that state is contained in `T` through `C → T`. It
dominates the fork base, so `B` holds `s1`: `S` changed and `T` did not, so
`T` takes `S`'s revert `s0`. Nothing is lost silently.

**Retention (review N5, R1).** Every sync point is kept. A later point for
the same pair does not make an earlier one redundant. The earlier state may
be contained in a third branch (through `C`) that the later point never
reached, and then only the earlier point gives the right base:

1. `S → C` at `e1`.
2. `C → T` at `e2`.
3. `S` reverts, then `S → C` again at `e3`.
4. Merging `S` into `T` still needs `S@e1`.

Sync points are `Arc`-shared COW projections, so each costs only the chunks
that diverged after it. G5 reports the retained bytes. Pruning that is
safe under containment is deferred to P10 (GC).

**Implementation (r0.6, found by G4).** Two changes keep merges cheap; the
bases are unchanged (PROP13 against the oracle, which keeps every point and
searches naively).

- **Containment as reach.** Containment is closed under a branch's earlier
  epochs, so the states contained in `Y` are one latest epoch per branch.
  Sync points are created one per epoch, so a single pass in epoch order
  gives the reach of every stored state and of every branch now, and only
  each branch's latest common state can be a maximum. The recursive search
  was cubic in the number of sync points: after 2,000 merges a merge took
  60 ms instead of 0.2 ms, and the G4 stress timed out.
- **Dropping states that can never be a base.** Every sync point stays in
  the containment graph, but its *state* is dropped once no future merge
  can use it. Merges and forks read a branch's current state, so every
  future reach is a join of the branches' reaches now (plus newer states).
  If every branch that contains `(b, e)` now also contains a later synced
  state of `b`, `(b, e)` is never again the latest common state of `b`.
  In the example above `T` contains `S@e1` but not `S@e3`, so `S@e1` keeps
  its state. With the §3.1 representation each stored state is a whole
  database, so keeping them all grew memory quadratically (488 MB after
  1,500 merges of two branches; the G4 stress was killed). Now it is 2 MB.
  The rule is deterministic, so replay and snapshots agree; a snapshot
  marks a dropped state `PRUNED`. This is not the P10 GC: nothing a merge
  can read is removed.

### 6.2 Classification (Dolt DiffOp; review M4)

Each comparable value is classified on `(B, S, T)`. "Absent" is a value in
its own right: the object is not visible there, or it has no head or
entry. The comparable values are:

- per logical: its head and its base bit for each revision;
- per rule: `enabled`;
- per `(rule, curvec)`: its ValidationRecord;
- per artifact: open or resolved;
- per metadata key;
- policy.

| `S` vs `B` | `T` vs `B` | Result |
|---|---|---|
| same | any | keep `T` |
| changed | same | take `S` |
| changed | changed, equal to `S` | keep (convergent) |
| changed | changed, different from `S` | **conflict** |

Booleans (`base`, `enabled`, `artifact_open`) reach the conflict row with a
single base only when the object is absent in the base and became visible
on both sides independently, through cross-branch dedup (a revision adopted
as base on one side only; one canonical rule enabled on one side only).
Otherwise they conflict only through the criss-cross rule of §6.1 (review
R3; corrected in r0.7).

**Conflicts:**

- **Head conflict:** the target head stays, and the merge opens a
  `Conflict` artifact whose target is the **target's head revision**.
- **Conflicts on booleans** (review R3): the target value stays,
  and a `Conflict` targets:
  - for `base`: that revision;
  - for `enabled`: the rule's conclusion revision;
  - for `artifact_open`: the artifact's target revision.
  A criss-cross conflict on policy fails with `MergeConflict{Policy}`.
- **ValidationRecord conflict** (different verdicts for one
  `(rule, curvec)`): the target's record stays, and a `Conflict` artifact
  targets the rule's conclusion revision.
- **Metadata (review N3, R4).** Metadata takes part in no derived state
  (SD7), so a metadata conflict never opens a gating artifact. The target
  keeps its value, and the conflict is listed in the merge result.
  - `dirs` narrowed on both sides combine by **prefix-aware intersection**:
    for each pair `(a ∈ S, t ∈ T)`, keep the more specific path when one is
    a prefix of the other (`{a} ∩ {a/b} = {a/b}`); unrelated pairs add
    nothing.
  - An empty `dirs` result would mean "not narrowed", so it is never
    applied: it is a listed conflict, and the target keeps its value. The
    rule stays narrow-only (contract 10).
- **Policy conflict:** the merge is refused with
  `MergeConflict{kind: Policy}`.

Conflict artifacts are opened in canonical order: kind, then logical id,
then rule id. Each payload is the canonical encoding of `(merge epoch,
source, kind, B, S, T values)`; a ValidationRecord conflict's values start
with the record's curvec, so conflicts on one rule at two curvecs differ
(r0.7). The kernel derives that blob itself, and
the store persists it with the batch, so `MissingBlob` cannot arise.

A newly visible logical (absent in `T`) takes the source's head and base
bits. That includes CodeAnchor logicals (review N9, R2): their head and base
from `S` are taken as an **initial observation**, and the merge sets them
**unobserved**, a new input bit per branch (`anchor_unobserved`, part of the
overlay).

- On `T`, an unobserved anchor reads `PENDING(ObservationLag)` whatever the
  scope's watermark says (SD14b, extended), so it can never surface as
  CERTIFIED before `T`'s observer has seen it (I6).
- **Clearing.** A scan of unchanged content commits no anchor Op (SD18.5),
  so the bit is cleared by the watermark: committing
  `ObserverWatermark{scope, …}` on `T` clears `anchor_unobserved` for every
  anchor logical of that scope. Every scan advances the watermark, and the
  scan covers all of the scope's committed anchors, including the merged
  ones. A Broken observation also opens `T`'s own `AnchorBroken`.
- An anchor whose base `S` had retracted arrives with `base = false` and no
  `AnchorBroken`. `AnchorBroken` is observer state, so `T`'s scan decides
  it. A logical absent in `S` after being visible in `B` cannot
occur, since visibility only grows.

### 6.3 Steps and channel

1. Compute `B`, `S`, `T` and the classification.
2. Make every object visible in `S` and not in `T` visible in `T`
   (`branch_visibility`).
3. Recheck `T`'s cycle graph by inserting the newly visible rules in
   ascending `RuleId`. A cycle gives `CycleDetected` with a deterministic
   op index and path; the merge is refused and `T` is unchanged (T72).
4. Apply the classified changes. Head moves CAS against `T`'s pre-merge
   heads. The net delta treats newly visible rules and revisions as new
   rules and revisions, seeding their conclusions. A5 is vacuous, since a
   merge allocates no new revision. **Corrected in r0.5:** A5 is not
   vacuous. A merge can import base revisions of a required kind or take a
   stricter policy, so it checks A5 on the merged state, newly visible
   revisions counting as new, first among the end-of-batch checks (§9).
5. Recompute statuses on the merged `T`. The derivation itself rechecks Sem
   and Cur against the merged heads.
6. **Certification (review M5).** If `require_certified`, every logical in
   `certify` must be CERTIFIED. `certify` refs resolve in `T` after
   step 2, otherwise `UnknownRef`. When `certify` is empty, the required
   set is every logical whose head the merge moved, plus every logical on
   which the merge opened a `Conflict`. A logical on which the merge opened
   a `Conflict` counts as CONFLICTED, also when the Conflict targets a
   revision other than its head (a base, a rule's conclusion, an
   artifact's target; r0.7), so **a default merge with any conflict is
   refused**. A failure gives `MergeNotCertified{logical, status}` (T20b)
   for the first failing logical by id, with or without `certify`. With `false`, downstream goes STALE/PENDING by incidence
   kind (T20a), and conflicts stay open for resolution.
7. Set the `{S, T}` sync point (§6.1).
8. Return the typed result in `CommitResult`'s merge trailer (§8): moved
   heads, conflicts (each with its artifact id), newly visible object
   counts and convergent changes.

**Observer domain (review M4).** For logicals already visible in `T`, merge
never takes state that only the observer channel may write (SD18.3,
contract 7). That covers CodeAnchor heads and base bits, `AnchorBroken`
artifacts and `ObserverWatermark`s, which stay `T`'s. The one exception,
newly visible anchor logicals, is in §6.2. Merge may run on the agent
channel for this reason: it never moves an existing observed head. After a merge, the target's observer rescans its worktree
and reconciles code anchors through the normal observer path.

### 6.3a Implementation (r0.5)

The kernel classifies and plans in global ids (`core/src/kernel_merge.rs`);
the target's database publishes the plan like one structural commit
(`core/src/merge.rs`).

- **Sync points** hold the source state as the shared database and id map
  (`Arc`), so storing one is O(1). The source's next write then copies its
  database once, as after a fork. Every point is kept; G5 reports the
  memory this retains.
- **Merge base.** Containment is computed by memoized recursion over the
  sync points. Sync epochs strictly decrease along it (one commit per
  epoch), so it terminates. Candidates are the distinct stored states
  contained in both branches; the maxima are those contained in no other.
- **Classification** reads each value from the bases, the source and the
  target in global ids. "Absent" is a value. Several maxima merge a value
  that both sides agree on, use the bases' value when they all agree, and
  conflict otherwise. A validation record conflicts only when verdicts
  differ; equal verdicts with different validators keep the target's.
- **Newly visible objects** take the next local ids in ascending global
  order, so step 3 inserts the new rules in ascending `RuleId`. Their
  records keep their original provenance. `AnchorBroken` artifacts are
  never imported. A newly visible logical counts as one whose head the
  merge set, so `require_certified` checks it (T20b).
- **Order and atomicity.** `InvalidMerge`, then `ChannelRestricted` (the
  observer channel cannot merge), then the read set's Op errors when every
  `ExpectHeads` precedes the Merge, then `MergeConflict{Policy}`, then
  `CycleDetected` from publication, then `UnknownRef` for `certify`, then
  any later `ExpectHeads` errors, then `ReadSetConflict`, then
  `MergeNotCertified`. A merge that checks the merged state (read sets or
  certification) runs once as a dry run first, so a refusal changes
  nothing.
- **`anchor_unobserved`** is a per-branch set in the database, hashed into
  the digest only when non-empty, so single-branch digests are unchanged.
  A watermark commit clears the anchors of its scope. Persisting it is part
  of store format 3.
- **The report** (`MergeReport`) is returned by `Kernel::commit_detailed`;
  it becomes the `CommitResult` trailer with format 3.

### 6.4 Conflict gate (SD17 amended for P6; review M9)

SPEC §5 P6 requires merge conflicts to block CERTIFIED, and SD11 already
contains `¬Conflicted(r)`. So P6 brings the minimal L2 gate forward from
P7:

- A revision with an open `Conflict` artifact targeting it is not
  CERTIFIED. Its summary is `CONFLICTED` (SD13), and L0/L1 are unaffected
  (T51 moves to P6).
- Dependents whose premise fails L2 because of a conflict report
  `PENDING(Semantic)`. There is no new reason. WHY names the open
  conflict. **Decision (review M9a): `PENDING(Conflict)` is not introduced.**
- `ResolveArtifact` releases the gate. The open/resolve seeds (SPEC §3.4
  Phase 3b) and the oracle gate are implemented in P6 and covered by PROP1
  and PROP13. The S13 `CONFLICTED` row moves to P6.
- **Replay (review M9d).** Existing logs may already contain agent-written
  `OpenArtifact{Conflict}`, so their statuses change under the gate. The
  format-3 migration (§8) re-derives every result, keeps `acked_result`,
  and reports any status change. Before the migration ships, the dogfood
  stores are scanned offline (`ctx export`).

P7 adds the other Conflict producers.

**Implementation (r0.5).** The indexes count open Conflicts per target
revision. Publication updates the count when a Conflict opens or resolves
(journalled for `WHAT_IF`) and seeds the target into the status walk. The
layer columns keep `conflicted` per revision: L2 is false and has no
witness while it holds, and the summary reads it. The oracle (`jacobi`,
`kahn`, snapshot) applies the same rule.

## 7. Currency, statuses, certificates and sessions (review B4)

**Head currency is per branch and does not depend on other branches.** In
branch `b`, a revision is `Current` if it is `b`'s head and `Superseded`
otherwise. That is all the snapshot, `status_diff`, the digest and
certification ever see.

**`BRANCH_ONLY` is a read-time annotation on `main`** (like the observation
dimension, SD14b):

- When a reader on `main` asks, in a **current-mode read**, for a visible
  revision that is `Superseded` on `main` but is the head of some open
  branch, the reported currency is `BRANCH_ONLY`. Its summary is
  `STALE(HeadSuperseded)`, the same as `Superseded`.
- **Historical and snapshot reads** (`Historical(e)`, `Snapshot(e)`, `diff`)
  never report it; they report `Superseded`. Repeated historical reads stay
  byte-identical (T13; review N2).
- It never enters snapshots, `status_diff`, digests or certificates, so a
  commit on another branch changes no stored or derived state on `main`.
- Within any branch `b`, the branch's own heads are `Current`.
- A revision created on `b` and not yet merged is invisible on `main`, so
  it gives `UnknownRef`, not STALE.
- On a non-main branch, `BRANCH_ONLY` is never reported. That settles
  nested branches.

The S13 row: on `main`, revision `A@0` is superseded by `A@1`; branch `b`
sets its head back to `A@0`. `main` reads `A@0` as currency `BRANCH_ONLY`,
summary `STALE(HeadSuperseded)`, and `main`'s digest is unchanged. After
`b` is archived, `main` reads `A@0` as `Superseded`.

**Certificates.** `Certificate.branch` is the session's branch, and the
certificate is validated against that branch's state and `ChangeLog`. A
certificate presented on another branch gives `Invalid` with reason
`BranchMismatch`, which aggregates as `Invalid` (§6.4 priority
`Denied > Invalid > …`). T62 holds per branch, and also across branches:
commits on other branches never change a certificate's verdict.

**Sessions (D1).**

- `SessionStart` and `SessionAttach` take an optional `branch` name.
- Without one, the session uses the branch named like the repository's
  current git branch, if it exists and is open; otherwise `main`.
- The branch is fixed for the session's lifetime and recorded in
  `SessionEvent::Start`, a field that already exists. Old sessions replay
  as `main`.
- Reads, records, checks and hooks in the session use that branch.

**Observer (review m3).** The observer submits a scope's watermark and
anchors on the branch its worktree's checked-out git branch maps to under
D1; otherwise on `main`. Two consequences are documented and tested:

- Checking out a git branch that has no ContextDB branch writes its anchor
  observations to `main`.
- A session pinned to a branch other than the worktree's sees
  `ObservationLag` until that branch is observed.

**Implementation (r0.5): branch-aware reads.** `BranchView`
(`core/src/kernel_view.rs`) answers every read the daemon makes, with
global ids:

- node, status and `summary_at`;
- why, why-depth and why-not;
- rules concluding a revision, and reachable impact;
- historical reads, snapshot reads and diffs;
- certificates.

Each read maps its arguments to the branch database's local ids, runs the
single-branch query and maps every id back: witnesses, stale reasons, head
vectors and proof trees. Certificates are re-sorted under global ids and
`cert_id` hashes the global form. A branch database's epoch advances only
on its own commits, so certificates are issued and validated at the
kernel's global epoch (`issued_epoch` is a global epoch matching the log),
and historical reads at a later global epoch read the branch's latest
state. A certificate presented on another branch is `Invalid` with
`BranchMismatch`. On `main`, `node`, `status` and `summary_at` annotate
`BRANCH_ONLY`; historical reads never do. With only `main`, every view
read equals the single-branch database's (tested on generated traces).
PROP13 also checks every view node against the oracle.

**Implementation (r0.5): sessions and the observer.**

- **Protocol 4 fields.** `SessionStart` and `SessionAttach` carry `branch`
  (explicit) and `git_branch`. The client reads its checkout's `HEAD`
  (`contextdb_proto::git`), which handles linked worktrees, so the daemon
  never guesses from the main worktree path.
  - The hook reads its own checkout.
  - `ctx-mcp` reads the nearest checkout above its working directory.
  - Both take an explicit name from `CONTEXTDB_BRANCH`.
- **Resolution.** An explicit name must exist and be open, else
  `UnknownBranch` or `BranchArchived`. A git name binds when such an open
  branch exists, else the session uses `main`.
- **Daemon.** The branch is stored in the session and in
  `SessionEvent::Start`, and restored on replay; old sessions replay as
  `main`. Every bound read, record, supersede, check, certificate, update
  and briefing goes through `Snapshot::view(branch)` with global ids, and
  writes commit on that branch. The recall index holds every branch's
  logicals; candidates are re-read on the session's branch.
- **Observer.** `AnchorState` carries the worktree's git branch and returns
  the branch it read; the observer commits its batch there.
- **Still on `main` until 9c:** the unbound reads (`Why`, `WhyNot`,
  `Impact`, `ImpactBatch`, `Diff`, `Status`), admin `Validate` and
  `CheckAnchors`. 9c adds a branch parameter to them.

## 8. Persistence, wire and upgrade

**op_log format 3.**

- Op tags are appended: 14 `CreateBranch`, 15 `ArchiveBranch`,
  16 `ExpectHeads`, 17 `Merge`.
- `CommitResult` gains a versioned trailer (branch, merge result) and
  `Inputs` a v3 encoding (branch table, sync points, per-branch overlays,
  `branch_visibility`).
- Old binaries stop with `FormatTooNew`.
- `ctx migrate` re-frames format-2 logs: it re-derives every result under
  the P6 kernel (including the §6.4 gate), keeps `acked_result`, writes
  `log.v2.bak`, and prints any `CommitResult` that changed (T68 2→3).
- Snapshots follow the log format; old snapshots are ignored.

**Session log.** It needs no format change.

**EventLog.** v1 adds the new tags. Exporting to v0 refuses a log that uses
them.

**Projections (review M8).**

- Real branch values.
- A `branch` column in `diff_log` (T13 stability is per branch) and in
  `watermark_history`.
- New `branches`, `branch_visibility` and `rule_enabled(branch, …)`.
- `artifacts` and logical metadata projected per branch.
- A `projection_schema` version table; a mismatch clears and rebuilds them.

**Protocol 4**, lockstep with ctxd, ctx, ctx-mcp and ctx-hook.

- `SessionStart`/`SessionAttach` gain `branch`.
- New requests: `Commit`, `WhatIf`, `BranchCreate`, `BranchList`,
  `BranchArchive`, `Merge`.
- `Status`, `Why`, `WhyNot`, `Impact` and `Diff` take an optional branch,
  defaulting to the session's or `main`.
- All hard-coded `MAIN`s (§1) are removed.

**Upgrade of live stores** (protocol 3, format 2):

1. `ctx shutdown`;
2. install;
3. `ctx migrate`;
4. `ctx service restart`.

A format-2 store opened by a format-3 binary gives
`FormatMigrationRequired(2)`. Rollback goes through `log.v2.bak`, and only
before new commits land. Format 3 keeps its projections in
`projections.v3.sqlite`, so a format-2 binary reopens its own
`projections.sqlite` untouched (r0.7). An interrupted `ctx migrate` lists
the changed epochs when it resumes, and a rerun lists them from the backup.
Changed means the re-derived result differs from the result the old binary
logged, not from an earlier migration's `acked_result`. **Schedule (review m4):** the dogfood stores are
upgraded only after the P4 dogfood report (due ≥ 2026-11-04), unless the
owner decides otherwise.

**Implementation (r0.5).**

- **Store.** It holds the kernel. Replay recommits every frame and checks
  both its `CommitResult` and its trailer.
- **Result trailer.** A format-3 result is the `CommitResult` bytes followed
  by `ResultTrailer{branch, merge: Option<MergeReport>}`, version byte 1.
  Format-2 rows carry no trailer. The merge report types live in the model.
- **Snapshots.** A snapshot holds `Kernel::export`: distinct branch-database
  states (inputs, local → global ids, unobserved anchors), the branch
  table with each branch's `branch_visibility` rows, and the sync points.
  States shared in memory are written once. `Kernel::import` rebuilds each
  state with `Db::from_inputs` and derives the global tables from the
  branches, since every object stays visible on the branch that created
  it.
- **Migration.** `ctx migrate` takes formats 0–2 to 3. Every result is
  re-derived under the P6 kernel. The originally acked result is kept as
  `acked_result` wherever it differs, and those epochs are printed. Format
  2 keeps its groups.
- **Projections.** Schema 2 keys `diff_log`, `head_history`,
  `base_history` and `watermark_history` by branch and adds `branches`,
  `branch_visibility` and `rule_enabled`. A `projection_schema` version
  mismatch drops the tables for a rebuild. Artifact and metadata
  projections wait for their first protocol-4 reader.
- **EventLog.** Each line carries the lowest version that can represent
  its batch; a v0 line carrying P6 features is refused.
- **Protocol-3 readers** (`Store::db`, the daemon's snapshots) read `main`'s
  database. Its ids equal the global ids only until another branch
  allocates. After that, `Store::db` returns `BranchAwareReadRequired`.
  Protocol 4 makes every reader branch-aware.

**Historical reads on a branch (review M8).**

- `Historical(e)`, `Snapshot(e)` and `diff` for `e < fork_epoch(b)` resolve
  through the parent at `e`, recursively.
- A certificate's strict window never crosses its branch's fork, because a
  certificate is issued on one branch.

## 9. Errors and check order (review M10)

New `TxError` entries for SPEC §6.2. All are `TxError`s, and the MCP
mapping is by name.

| Error | Raised for | Level |
|---|---|---|
| `BranchExists{name}` | `CreateBranch` with a taken name | Op |
| `InvalidBranchName{name}` | Bad charset or length, `main` reserved, archiving `main`, or `ArchiveBranch` naming a branch other than the batch's | Op |
| `CapacityExceeded{Branch}` | Branch id space exhausted | Op |
| `BranchArchived{name}` | Any batch on an archived branch | Batch, before ops |
| `UnknownBranch` | Batch on a nonexistent branch | Batch, before ops |
| `SoleOpRequired{op}` | `CreateBranch` or `ArchiveBranch` not alone, or `Merge` with anything but `ExpectHeads` | Batch, before ops |
| `InvalidMerge` | Source unknown or equal to the target | Op |
| `MergeConflict{kind}` | Unmergeable policy | Op |
| `MergeNotCertified{logical, status}` | `require_certified` failure | End of batch |
| `InvalidReadSet{logical}` | Duplicate or `New` entries in `ExpectHeads` | Op |
| `ReadSetConflict{logical, read, now}` | `ExpectHeads` mismatch against the pre-batch head | End of batch, after `PolicyViolation` |

**Branches are not secret (review N7).** Every branch name and id is
visible from every branch. An unknown merge source is therefore
`InvalidMerge`, never `UnknownRef`. `UnknownRef` stays reserved for
logicals, revisions, rules and artifacts.

**Order.**

1. Idempotency: a known `client_txn_id` returns its stored result.
2. The store's encodability check (`CapacityExceeded{Encoding}`).
3. Batch level: `UnknownBranch`, `BranchArchived`, `SoleOpRequired`.
4. Per Op, in the §6.2 order, with the new checks placed as follows:
   - `ExpectHeads`: `UnknownRef` for its logicals, then `ChannelRestricted`
     (a client-sent certificate source), then `InvalidReadSet`;
   - `CreateBranch`: `InvalidBranchName`, then `BranchExists`, then
     `CapacityExceeded{Branch}`;
   - `ArchiveBranch`: `InvalidBranchName`;
   - `Merge`: `InvalidMerge`, `MergeConflict{Policy}` (classification,
     step 1), then `CycleDetected` (step 3), then `UnknownRef` for
     `certify` refs (after step 2).
5. End of batch:
   1. A5 `PolicyViolation`;
   2. `ReadSetConflict`;
   3. `MergeNotCertified`.

The certificate `BranchMismatch` reason belongs to §6.4 (it is not a
`TxError`).

## 10. Interfaces

- **MCP:**
  - `ctx_commit(ops, expect_cert_valid?, client_txn_id?)`;
  - `ctx_branch(create|list|archive, name?)`;
  - `ctx_what_if(ops)`, which returns the diff;
  - `ctx_merge(source, require_certified = true, certify?)`;
  - `ctx_impact` gains a hypothetical batch.
  - Write tools commit on the session's branch.
- **CLI:**
  - `ctx branch create|list|archive <dir> …`;
  - `ctx merge <dir> <source> [--into B] [--no-require-certified] [--certify K …]`;
  - `--branch` on `status`, `why`, `why-not`, `impact`, `diff`, `validate`
    and `policy apply`.
- **Hooks:** PreToolUse checks the session branch's certificate.

**Implementation (r0.5).**

- **Requests.** Protocol 4 adds `Commit`, `WhatIf`, `BranchCreate`,
  `BranchList`, `BranchArchive` and `Merge` (it replies `Merged` with the
  report from the result trailer).
  - Instead of a branch field on every unbound read, one wrapper:
    `OnBranch{branch: Named(name) | Session(binding), request}` carries
    `Why`, `WhyNot`, `Impact`, `ImpactBatch`, `Diff`, `Status` or
    `Validate`, one level deep.
  - The MCP read tools use `Session`, so they read the session's branch.
- **`ctx_commit`.**
  - Branch ops and client-sent certificate read sets are refused (the
    latter also in `Raw`).
  - The idempotency hash covers the client's ops, the flag, and the branch,
    channel and principal (r0.7; not the cert id). It is stored as the
    format-3 `client_hash` row field, so a retry after the state moved, or
    after the certificate was re-issued, returns the original result.
  - `ctx_branch` and `ctx_merge` take a `client_txn_id` like every write
    tool (SPEC §6.5; r0.7).
  - The store calls a `prepare` hook per write, after idempotency and
    before staging, with every earlier batch of the group applied. There
    the actor validates the session certificate and appends
    `ExpectHeads{source: Certificate}`.
  - A verdict other than Valid or ValidRecertified replies `Refused` and
    commits nothing.
  - The replica replays the extended batch.
- **MCP tools.** `ctx_commit` and `ctx_what_if` take JSON ops (logicals by
  key, revisions as `key@rev` or `new:N`, bodies as text). `ctx_branch`
  and `ctx_merge` complete the set, with `require_certified` defaulting to
  true (D2).
  - The hooks claim the session-branch tools: `ctx_why`, `ctx_why_not`,
    `ctx_impact`, `ctx_commit`, `ctx_what_if` and `ctx_merge`.
  - Hooks installed before protocol 4 need `ctx hooks install` again.
    Until then those calls run on the MCP server's own session, which is
    bound to the same git branch.
- **CLI.** `ctx branch create|list|archive`, `ctx merge`, and `--branch` on
  `status`, `why`, `why-not`, `impact`, `diff` and `validate`.
- **Deferred.** `policy apply --branch`, `CheckAnchors` on a branch, and a
  hypothetical batch on `ctx_impact`; `ctx_what_if` covers the last.

## 11. Verification (review M11)

- **G1.** fmt, Clippy, tests, deny, dependency policy, and loom on the
  publication and replica paths.
- **G2.**
  - T14, T20a, T20b, T47, T48, T72, T73, T75.
  - S13 `BRANCH_ONLY` and `CONFLICTED`, T51, and T62 across branches.
  - **T19 variants:**
    - raw `ExpectHeads`;
    - `expect_cert_valid` with a head move;
    - the same, with only a base retraction, a rule disable, a
      ValidationRecord or a watermark change in the window;
    - two plans in the same actor group;
    - read-then-supersede in one batch (must commit, B3);
    - cross-branch (no conflict);
    - idempotent retry of a durable but unacknowledged `ctx_commit` (M2).
  - **Goldens:**
    - visibility: the fork boundary, "merge then fork then reference" (B1),
      dedup imports and archived branches;
    - merge base: repeated merges both ways, criss-cross (fail-closed
      conflicts), siblings, a change through a third branch, and a revert
      after a third-branch flow (B2, N1);
    - `dirs` prefix intersection, including the empty-result conflict (N3);
    - newly visible anchor logicals reading `ObservationLag` until observed,
      even with a current watermark, and the next watermark commit clearing
      the bit, including for unchanged content (N9, R2);
    - the `e1/e2/e3` retention case (R1);
    - criss-cross conflicts on `base`, `enabled`, `artifact_open` and policy
      (R3);
    - metadata conflicts listed without an artifact (R4);
    - historical reads on `main` never reporting `BRANCH_ONLY` (N2);
    - every classification row for every value kind, including absent;
    - cross-branch rule dedup (M3, §11a);
    - the observer-domain exclusion.
- **G3.**
  - **PROP13**, against an **independent oracle branch model**: a
    full-copy `InputModel` per branch with naive visibility (rules 1–3
    literally), naive merge-base search, naive classification,
    `anchor_unobserved` and full recompute.
    - Run on 10,000 fixed seeds of random histories: create, commit, merge
      both ways, archive, `WHAT_IF`, dedup.
    - For each branch, derived state must equal the oracle; under
      canonical renaming, every other branch's digest must be unchanged by
      a commit.
  - **PROP7** read/write sets:
    - `CreateBranch` reads the parent's whole state and writes the branch
      table;
    - `Merge` reads both branches and their sync point, and writes the
      target and the sync point;
    - `ExpectHeads` reads the listed heads.
  - PROP8/I1, PROP10 and PROP11 per branch.
  - **Mutation targets:** `core` visibility (`branch.rs`), merge base and
    classification (`merge.rs`), `ExpectHeads` (`txn.rs`), the undo journal
    (`whatif.rs`), and the conflict gate (`layers.rs`).
- **G4.**
  - Concurrent create/commit/merge stress (1/8/32 clients, 10 min); I4
    holds across branches.
  - Crash injection around the new Ops: every point at least 1,000 times;
    replay equals the oracle.
  - `ctx migrate` 2→3, interrupted and resumed, plus the T68 report.
- **G5** (initial, on the M1 Pro).
  - Fork: constant time and memory at 10³–10⁶ objects.
  - `WHAT_IF` ≤ 2× an equivalent commit (p95, excluding sync).
  - A commit on a branch ≤ 1.2× the same commit on `main` (p95, including
    first-touch COW), reported with 0 and with 10⁴ private rules on shared
    premises.
  - Per-branch memory overhead and retained sync-point bytes.
  - Write latency with actor-side certificate validation, `expect_cert_valid`
    on versus off.
- **G6.** None.
- **G7.** P0–P5b regressions at gate counts.
- **G8.** README (branches, `ctx merge`, upgrade), CHANGELOG, status.

**PROP13 implementation (r0.5).** `contextdb-oracle::BranchOracle` keeps
global arenas, `branch_visibility` rows, a full overlay per branch and per
sync point, and each branch's canonical order. Each commit runs the
single-branch `InputModel` on the branch's projection in that order, with
the global key namespace. New objects identical to invisible ones become
imports by a linear scan. Merges use naive containment over all sync
points, naive classification, and recomputation through `jacobi`. Every
projection asserts that its order lists exactly the objects SD19 rules 1–3
make visible. The test (`p6_prop13.rs`) runs 10,000 seeds of up to four
branches with code anchors, watermarks, AnchorBroken, policy, metadata,
validations, read sets, dedup and merges both ways. Before every step it
compares the kernel's dry run with the oracle's; after every step it
compares every branch's canonical state (records, inputs, every node and
rule state, in global ids). Six planted kernel bugs are all caught. The
oracle compares errors that may name staged objects by kind, and cycle
paths not at all.

**§11a Cross-branch rule dedup (review M3).** `AddRule` canonical dedup
spans all branches, like revision dedup (contract 5). If an identical
canonical rule exists but is invisible, `AddRule` reuses its `RuleId`,
imports it into this branch's visibility, and sets its `enabled` there from
the Op. It reveals only what revision dedup already reveals. A later merge
then sees one rule id, so SD6 never sees duplicates. T73 gets a rule
variant.

As with `AddRevision` (review N8):

- a reused rule still uses up its `New(k)` slot and appears in `new_rules`;
- `Rebind` follows the same rule;
- the imported rule enters the branch's cycle graph (`topo.insert_edge`),
  so it can fail with `CycleDetected` (DA6).

## 12. SPEC amendments before implementation (review M12)

SPEC v1.6.8 must land with the G0 closure:

- **SD19:** recursive visibility, sync points and merge base, per-branch
  currency, `BRANCH_ONLY` as a read-time annotation on `main`, the
  observer domain excluded from merge, branch names.
- **SD13:** the `BRANCH_ONLY` read-time dimension; no `PENDING(Conflict)`.
- **SD17:** the P6 conflict gate (owner confirmation).
- **§6.1:**
  - the new Ops and their reference rules;
  - contract 1, the client hash for idempotency;
  - contract 2, the `New(k)` slot of a deduplicated rule;
  - contract 5 extended to rules (§11a);
  - contract 7, Merge's channel and the observer-domain rule;
  - contract 11, read sets.
- **SD6:** the cross-branch rule-reuse exception (§11a).
- **SD14b:** the per-branch `anchor_unobserved` input forcing
  `ObservationLag` (§6.2).
- **§3.3:** per-branch digests without currency; sync points and branch
  entries in the receiving branch's digest.
- **§3.4 Phase 3b:** Conflict open/resolve seeds from P6.
- **§6.2:** the errors and check order of §9.
- **§6.3:** the tool signatures of §10.
- **§6.4:** the `BranchMismatch` reason and `expect_cert_valid` semantics.
- **§6.6:** the WHAT_IF and replica rule.
- **§7:** the projection tables of §8; the client-hash field in the op_log
  frame.
- **§5 P6 and P7 gate lists:** T51 and S13 `CONFLICTED` move from P7 to P6.
- **T-matrix:** T19 variants; T51 and S13 `CONFLICTED` to P6; the new
  goldens.

## 13. Review log

- **r0.1** (2026-10-23): draft after a code map of the core, oracle, store,
  model, proto, daemon, MCP and CLI. Owner decisions D1 and D2.
- **Independent review of r0.1**: 4 blockers, 12 majors, 6 minors. The
  verdict was that G0 can pass after one revision. Resolution in r0.2:

  | Finding | Resolution |
  |---|---|
  | B1 | Recursive visibility, inherited through forks (§2) |
  | B2 | Sync points as inputs, base = source state at the sync, best common point otherwise (§6.1) |
  | B3 | `ExpectHeads` against the pre-batch head (§4) |
  | B4 | Per-branch currency; `BRANCH_ONLY` read-time only, on `main` (§7) |
  | M1 | Certificate validated in the actor (§4) |
  | M2 | Idempotency before validation; client hash excludes derived ops (§4) |
  | M3 | Rule dedup across branches (§11a) |
  | M4 | Complete classification, absent values, observer exclusion, determinism (§6.2–6.3) |
  | M5 | Required set includes conflicted logicals (§6.3) |
  | M6 | Complete journal, run on the replica, equality test (§5) |
  | M7 | `Arc` spine, chunked maps, visibility bitsets, G5 cases (§3, §11) |
  | M8 | Historical reads through the parent; projections per branch (§8) |
  | M9 | Gate conditions (a)–(e) (§6.4, §8, §12) |
  | M10 | Error table and order (§9) |
  | M11 | Independent oracle model, PROP7 sets, T19 variants, mutation targets (§11) |
  | M12 | Amendment list (§12) |
  | m1 | Accepted id leak documented (§2) |
  | m2 | `MAIN` list corrected (§1) |
  | m3 | Git-ref names; observer routing documented (§2, §7) |
  | m4 | Upgrade after the P4 report (§8) |
  | m5 | `ExpectHeads` alongside `Merge` (§4, §6) |
  | m6 | Branch limit and G5 percentiles stated (§2, §11) |

- **Closure check of r0.2**: 15 findings closed. B2, B4, M2, M4, M10 and
  M12 were partial, and the revision introduced N1–N3 and N9 (major) plus
  N4–N8 and N11 (minor). Resolution in r0.3:

  | Finding | Resolution |
  |---|---|
  | N1 (B2) | Merge base by transitive containment through sync points; criss-cross fails closed (§6.1) |
  | N2 (B4) | `BRANCH_ONLY` in current-mode reads only (§7) |
  | N3 (M4) | Prefix-aware `dirs` intersection; empty result is a conflict (§6.2) |
  | N9 (M4) | Newly visible anchor logicals are an initial observation, lagging until observed (§6.2, §6.3) |
  | N4 | Sync points and branch entries in the receiving branch's digest only (§3) |
  | N5 | Retention of fork points and the latest point per ordered pair (§6.1) |
  | N6 (M2) | Client hash as a `LogRow` field; forged certificate `ExpectHeads` rejected; G5 latency (§4, §11) |
  | N7 (M10) | Branches public; one total order of checks (§9) |
  | N8 | Rule dedup slot, `Rebind`, cycle check (§11a) |
  | N11 | Historical visibility filter (§2) |
  | M6 hole | Replaced sync point journalled (§5) |
  | M12 gaps | Amendment list completed (§12) |

- **Re-check of r0.3**: N2, N3, N4, N6, N8, N11, the M6 hole and the M12
  gaps were closed, and N7 was closed except two nits. R1–R4 were new.
  Resolution in r0.4:

  | Finding | Resolution |
  |---|---|
  | R1 (N5) | Every sync point kept; pruning deferred to P10 (§6.1) |
  | R2 (N9) | `anchor_unobserved` input forces lag until the observer covers the anchor; AnchorBroken handling stated (§6.2) |
  | R3 | Criss-cross conflict targets for booleans; policy refused (§6.2) |
  | R4 | Metadata conflicts listed, no gating artifact (§6.2) |
  | N7 nits | Source must exist; `ChannelRestricted` in the `ExpectHeads` order (§6, §9) |

- **Final check of r0.4**: R1–R4 and the N7 nits were closed. **Verdict: G0
  can pass** once the §12 SPEC amendments land and the owner confirms
  SD17. Two minor wording gaps were fixed in the r0.4 text before closing:
  - the `ObserverWatermark` commit clears `anchor_unobserved` (§6.2);
  - the bit is named in the digest projection (§3), the WHAT_IF journal
    (§5), the PROP13 oracle and the golden (§11).

- **G0 closed (2026-10-23):** the owner confirmed D3 (the SD17 gate). SPEC
  v1.6.8 carries the §12 amendments: the SD19 supplement, SD17, SD13,
  SD14b, SD6, §3.4, §6.1 contracts 12 and 13 (contract 11 was already
  taken by EventLog v0), §6.2–§6.4, §6.6, §7, the P6 and P7 gates, T51 and
  T19.
- **r0.5** (2026-10-24): canonical order per branch (§3.1), A5 on merges
  (§6.3 step 4), PROP13 oracle (§11), found by PROP13.
- **r0.6** (2026-10-26): merge bases by reach in one pass, and sync-point
  states that can never be a base are dropped (§6.1), found by the G4
  stress. Same bases; no semantic change.
- **r0.7** (2026-10-26): fixes from the independent implementation review
  ([P6 gate](../gates/P6.md)). Conflicts off the head refuse a default
  `require_certified` merge, and the first failing logical is by id with a
  `certify` list (§6.3); R3's claim corrected (§6.2); ValidationRecord
  payloads carry the curvec (§6.2); branch digests cover the branch entry
  and received sync points as §3 already required; the client hash covers
  branch, channel and principal and not the cert id (§4, §10); branch and
  merge requests take a `client_txn_id`; format-3 projections in their own
  file, and migration's changed-epoch listing survives interruption (§8).
- **r0.5** (2026-10-23): implementation note §3.1. Each branch is a
  single-branch database with local ids behind a global id map. Fork is
  O(1); the first write copies the branch, O(N), which G5 measures. No
  semantic change.
