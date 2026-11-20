# ADR 0019 — P11 cascade-bounded lazy maintenance, payload GC and scale

Status: **Proposed (r0.2), G0 pending.**

- r0.1 was reviewed independently in two parts. Lazy maintenance and the
  log format had 3 blockers, 9 majors and 13 minors. GC and layout had
  4 blockers, 5 majors and 14 minors.
- r0.2 answers all of them. §12 maps each finding to its fix.

The biggest changes from r0.1:

- **No log rewrite and no `Settled` frames.** Old segments stay format 6,
  and new frames hold exactly the acknowledged result.
- **Each deferred epoch's projections are applied when it settles.**
- **Exact settledness.** A bounded backward search decides it, instead of
  treating every rank at or above the cursor as unsettled.
- **Fan-out is resumable.**
- **GC is a store-level step, not a kernel op.** Every op that makes an
  existing revision live checks that its payload is still there.

The owner's scope decisions of 2026-11-20 are in §0. The open points in
§0.1 are not yet confirmed. Nothing here changes the SPEC until G0 passes;
§11 lists the SPEC changes.

## 0. Scope and decisions taken

P11 lowers maintenance cost and supports larger stores. STRICT reads must
still fail closed (SPEC §P11, SD14a). The SPEC scope has four parts:

- eager / lazy selection;
- layout ablation;
- an optional Postgres backend;
- payload GC.

The owner decided four things on 2026-11-20, after the baseline in §1.3.

**D1, cascade-bounded lazy.**

- A commit whose status propagation goes past a work budget `E` stops
  there. The rest stays as a *frontier* that is drained later.
- A read that needs a node the frontier can still change either waits a
  bounded time for it to be verified or reports
  `PENDING(StructuralUnknown)`.
- Settled results are bit-identical to eager maintenance; only *when* the
  work runs changes.
- This narrows SPEC §P11's per-node selection by `read_heat, impact_size,
  risk, estimated_verification_cost`:
  - `impact_size`, the commit's own work, decides whether anything is
    deferred.
  - `read_heat` decides what a drain does first (§3.7).
  - `risk` is covered only by the existing rules: conflicts stay eager (R3)
    and STRICT consumers fail closed (R2).
  - `estimated_verification_cost` is not used as a separate signal.
  - There are no per-node eager/lazy labels and no persistent heat
    statistics.

**D2, no Postgres backend in P11.** P10 chose the git remote as the team
transport, so there is no team server. A Postgres backend becomes its own
phase when a server deployment is needed.

**D3, payload GC is an explicit `ctx gc`.** A dry run is the default;
`--apply` collects.

**D4, layout: measure, then adopt.** A candidate is merged only if it beats
the threshold in §6.

**The design in one paragraph.**

- The eager maintainer already processes a commit as rank-ordered passes.
  Every node is popped at most once, in increasing topological rank, and
  reads only nodes of lower rank (P-1, §3.1).
- Cut the interleaved pass after `E` work units and keep its whole state.
  Resuming later pops the same nodes in the same order with the same
  inputs, provided nothing changes the inputs in between. So the final
  state, `status_diff`, history and digest are those of eager maintenance.
- Two rules guarantee that nothing changes in between:
  - **I-L1.** Every `Kernel::run` and every store write drains the frontier
    first.
  - Journalled dry runs are always eager.
- While a frontier exists, a node is *settled* if no frontier entry can
  reach it. A bounded backward search over PK edges decides this (§3.4).
- Unsettled nodes read as `PENDING(StructuralUnknown)`, unless a
  higher-priority SD13 state applies (conflicts are read from inputs).
- The deferred epoch's diff reaches `diff_log` / `ctx_changes` only when it
  settles. The projection watermark stays below it until then.
- An op_log frame holds exactly what the client was acknowledged. For a
  deferred commit that is the input-level result plus `settling`, so a 1M
  cascade's frame is a few hundred bytes instead of 8 MB.
- GC never changes kernel state or the op_log. It is one exclusive actor
  step:
  1. compute the keep set (§5.1);
  2. drop the collected keys in one key-log rewrite;
  3. record them in a durable GC ledger.
- Later ops that make an old revision live again must find its payload, or
  fail with `MissingBlob`.

### 0.1 Open points for the owner

| # | Question | Recommendation |
|---|---|---|
| O1 | Is lazy on by default, and what is `E`? | On. `E` is fixed in milestone 1 from a measured unit rate on the local baseline machine; the target is a cascade-commit ack of about 50 ms. Lazy is off with `lazy = false`, not with a magic `E` |
| O2 | How long does a Certified/STRICT read wait for a pull before `PENDING(StructuralUnknown)`? | 50 ms (`read_settle_ms`). Advisory reads never wait |
| O3 | GC grace period | Keep contents that were live in the last `keep_epochs = 10,000` epochs (≥ 1 enforced) and since the oldest live session started (§5.1) |
| O4 | How is disk space reclaimed? | `ctx gc --compact`, offline: rewrite packs to drop shredded and collected records (§5.5) |
| O5 | Git mirrors and GC | Amend SPEC E4: GC does not make a mirror `PayloadGone`. A collected body is verified by its hash, not re-rendered (§5.6) |

## 1. Today

### 1.1 The commit path

`Db::commit_in` (crates/contextdb-core/src/lib.rs:608) stages a batch on an
overlay and publishes inputs (`publish`, :729). The shared tail is
`finish` (:1091), which runs in this order:

1. Status seeds are computed (`fast::status_seeds`, fast.rs:199).
2. `Maintainer::update` runs (:1111).
   - `Counting::update` (engine/counting.rs) seeds everything first (DA1,
     :174–201). It then pops a `BinaryHeap<Reverse<(rank, r)>>` in strictly
     increasing rank (DA2, :205–208) and drains the node's pending
     incidences and rules (:220–260).
   - On a flip it marks every downstream incidence and queues its
     conclusion (:271–281).
   - The heap always runs to empty (:283–296).
3. The L1/L2 walk runs (`walk`, lib.rs:950–1037). It is a second
   rank-ordered heap over the same PK ranks: `layers.refresh`, then
   `layers.propagate` and `rules_by_premise` pushes on change, then a
   `NodeChange`.
4. The lazily materialised `derived` snapshot is patched (:1135–1173).
5. `ChangeLog::record` (cert.rs:29) records input-level moves.
6. `status_diff` is assembled (:1184).
7. History records a `Transition` (:1185).
8. `CommitResult` is returned.

Kernel-level ops return from `Kernel::run` (kernel.rs:297) without
reaching `Db::commit_in`. These are CreateBranch, ArchiveBranch, SetAccess
and ShredContent (kernel.rs:375–389).

Admission reads inputs only (`Stage::new`, A5, ExpectHeads, and the SD17
gate through the input index `open_conflicts`). Derived state is read
around admission in three places:

- `require_certified` merges, after a journalled dry merge
  (kernel_merge.rs:1073–1101);
- the daemon's `expect_certificate`, in the store's `prepare` callback
  before the batch commits (store.rs:871–880; actor.rs:762–785);
- WHAT_IF and IMPACT{Exact}, which are journalled commits (whatif.rs:55–94).

### 1.2 Where the derived diff goes

**The log.** An op_log frame stores the full `CommitResult`. Its checksum
covers it (log.rs:122–145). There is one frame per epoch, numbered by
epoch (log.rs:234–237, 523, 564; `Store::epoch() = frames.len()`,
store.rs:1336).

**Replay.**

- Open replays each frame and requires `result == row.result()`
  (store.rs:601–614).
- Idempotent retries and `TxnStatus` return `row.acked()` (store.rs:758–770,
  858–862), the originally acknowledged result (log.rs:96–97).

**Projections.**

- `capture` takes `diff_log` rows from `status_diff` right after each batch
  (store.rs:133–169, 927).
- They are applied after the group's sync (store.rs:984–995).
- Replay re-captures frames above the projection watermark (store.rs:615).
- `Store::changes` answers `ctx_changes` up to the log tail
  (store.rs:1481–1493).
- Long polls wait on the epoch condvar (actor.rs:203–215).

**The spare kernel.** ctxd keeps one, replays each published group on it,
and requires equal `CommitResult`s, or it fail-stops (actor.rs:680–745).

**Readers** run on connection threads against the published `Snapshot`
(actor.rs:177). Long polls must not hold a snapshot (service.rs:1472–1474).

**Other consumers of the frame result:**

- `ctx ui` (graph.rs:66–98);
- validators.rs:1187;
- candidates.rs:671, 691;
- service.rs:4619;
- mirror.rs:546.

**StructuralUnknown.** `Structural::Unknown` and
`PendingReason::StructuralUnknown` exist (contextdb-model/src/status.rs:31,
49). `summarize` maps them after ObservationLag (oracle eval.rs:336).
Certificates map them to `Pending` (cert.rs:124; SPEC §6.4 table; T65).
The engine never produces them (layers.rs:417–421).

**STRICT consumers.** There is no "STRICT" read mode by that name. This
ADR uses the term for these consumers:

- `Mode::Certified` reads (proto.rs:129);
- certificate issue and validate;
- gates (pre-push anchors-gate, team gate);
- `require_certified` merges.

### 1.3 Baseline (remote Linux host, informational; G5 numbers come from the local baseline machine)

**Run `p11base2`** (P1 `contextdb-bench`, local-impact commits). Eager
`Counting` at 1M revisions meets every P1 budget:

| shape | commit p99 | CURRENT L0 p99 | WHY depth3 p99 | live heap | B/incidence |
|---|---|---|---|---|---|
| multi_support | 1,561 ns | 31 ns | 2,971 ns | 602 MB | 182.4 |
| hub | 1,350 ns | 30 ns | 250 ns | 567 MB | 189.1 |

**Run `p11cascade`** (`p11_cascade`). One commit toggles base 0 and the
cascade reaches every revision. Each range covers three samples per
direction.

| shape (1M) | core commit | store, memory projections | store, file projections | log frame |
|---|---|---|---|---|
| hub | 1,731–1,984 ms | 3,329–3,680 ms | 3,741–4,522 ms | 8,000,216 B |
| chain | 932–966 ms | 2,284–2,669 ms | 2,780–3,489 ms | 8,000,232 B |

- Reopening a store whose log held 8 such frames took 13.5–35.1 s.
- ctxd pays the core cost again on the spare kernel.
- The actor serves no other writer meanwhile.

These numbers motivate D1 and the frame change. They are not gate
evidence.

## 2. Requirements

- **R1, exactness.** Once settled, derived state, `status_diff`,
  `l0_flips`, history, `ChangeLog`, digest, certificates and `diff_log`
  equal eager maintenance (PROP1-lazy, §9).
- **R2, fail closed.** A STRICT consumer never sees CERTIFIED, `Ok`,
  `Valid` or `Ambiguous` computed from unsettled derived state (T61).
- **R3, conflicts stay exact.** For an unsettled node, `conflicted` is read
  from `indexes.conflicted(r)`, which `publish` updates before maintenance
  (lib.rs:842–859). The stale layer column is not used.
- **R4, rebuild is eager.** Replay, `from_inputs`, `ctx migrate`, `ctx ui`,
  the team replica and FullRecompute never leave a frontier. They run with
  the default `Commit` context (`lazy: None`). Derived state is never
  persisted.
- **R5, bounded ack.**
  - A commit's own maintenance stops after `E` units plus one chunk
    (§3.2). Its frame is written and acknowledged then.
  - Seeding (DA1) is not cut. It is bounded by the incidences on the
    batch's changed revisions and logicals (§3.2), and is reported
    separately in G5.
- **R6, determinism.** Slice boundaries depend only on inputs and `E`.

## 3. Cascade-bounded lazy maintenance

### 3.1 One interleaved pass and P-1

Today, `Counting::update` runs to empty and then `walk` runs to empty.
Lazy mode interleaves them by rank:

- take the smaller of the two heap tops;
- on equal rank, take the counting pop first.

**P-1.** Processing node r in either pass reads only state of nodes of rank
≤ rank(r), and of rank(r) only after r's counting pop.

- **The counting pop of r** reads r's pending incidences. Their `sat` is
  computed from premise `s` and input heads. Premises rank below r: PK
  holds premise→conclusion edges (pk.rs:275–300).
- **The walk's `refresh(r)`** reads:
  - the counting state of r itself (layers.rs:337–340);
  - the layer columns `truth`/`reason` of r's premises and, for a `Sem`
    incidence, of the current head h (layers.rs:193–203, 365–373).
    `pk.sync` adds `h→Logical(l)` and `Logical(l)→conclusion` for every Sem
    incidence, disabled rules included, so rank(h) < rank(r);
  - inputs and validations, through `semantic()` (layers.rs:22–35);
  - `indexes.conflicted`, an input index.
- **Ref incidences** add no PK edge and no derived dependency.

An implementation debug-asserts P-1 on every layer-column read in
`refresh`, and on counting reads.

**DA2 restated.** Each heap pops in strictly increasing rank. The walk may
hold node r at the rank of the counting pop that just pushed it.

**Moved assertions.** DA5, `assert_matches_rebuild` and counting's
unvisited-row check move to *full drain*.

`lazy: None` runs the same interleaved code to empty. The existing P1
properties and statistics must still hold on it. The PROP1 stats equality
is per epoch, accumulated over all slices.

### 3.2 Units, chunks and slices

**Units.** A unit is any one of:

- a heap pop (either pass);
- a heap push;
- a pending mark (`dirty_inc` / `dirty_rule` set, in either pass);
- a drained pending incidence or rule.

These are counted in `CommitStats` as `units`, in addition to today's
counters.

**Resumable fan-out.**

- A node whose processing marks downstream incidences does it in chunks of
  at most `C = 4,096` marks.
- If a slice ends inside a node, the node becomes the *paused node*. Its
  remaining iterator position (premise row and offset) is part of the
  frontier, and its rank is part of the cursor (§3.3).
- Conclusions not yet pushed have rank > rank(paused), so treating
  everything reachable from the paused node as unsettled is sound.

**Slices.**

- A slice stops at the first chunk or node boundary after `units ≥ E`.
- So it overruns `E` by at most one chunk or one node's fan-in drain.

**Seeding** (DA1) runs whole. All pending marks for a rank must exist before
that rank pops. Its size is the incidences in `exact_by_rev` / `pin_by_rev`
of moved revisions, plus `sem_by_logical` of moved logicals, plus new and
toggled rules. G5 reports it separately (R5).

### 3.3 The frontier

The frontier lives in the branch's `Db`. It holds all of the following:

- **Heaps and markers:** both heaps, and the paused node with its position.
- **Pending state:** `pending_*`, `dirty_*` and `queued` in `Counting` and
  `LayerColumns`.
- **Commit context:**
  - the walk's `seeds`, `visited` and `changes`;
  - `Maintenance.before`;
  - the commit's `undo`, `delta`, `revalidated` and `old_revisions`
    (needed by the walk's `before_view`, lib.rs:964–972);
  - the accumulated `stats`;
  - the epoch.

**Finishing.**

- The `derived` snapshot patch, `ChangeLog::record` and the history
  `Transition` run once, when the frontier empties.
- `ChangeLog::record` is input-level, but it runs at the end so that the
  commit's tail order stays as it is today.

**Persistence and copies.**

- The frontier is never persisted (R4).
- `Arc::make_mut` clones it with the `Db`. That is harmless because I-L1
  drains before any operation that could make a second copy observable.
  `create_branch` is a `Kernel::run` op (§3.5).

**Cursor.** `cursor = min(rank of both heap tops, rank of the paused
node)`, or `None` if the frontier is empty.

### 3.4 Exact settledness

`settled(r)` holds when either:

- `cursor = None`;
- `rank(r) < cursor`;
- a backward search from r over PK in-edges finds no frontier member. The
  search is restricted to nodes of rank ≥ cursor. Frontier members are the
  nodes queued in either heap and the paused node.

The search is bounded by `S = 4,096` visited nodes. If the bound is hit,
`settled(r) = false`.

**Soundness.**

- Every value the frontier can still change belongs to a node reachable by
  PK edges from a frontier member. Counting pushes follow
  `rules_by_premise`; walk pushes follow `rules_by_premise` and
  `sem_by_logical`. Both are subsets of PK edges (pk.rs:275–300).
- A node of rank < cursor cannot be reached from a frontier member, since
  PK edges increase rank.
- So "no frontier member among r's ancestors at rank ≥ cursor" implies that
  r is final.

Results are memoised per publication: one `BTreeMap<RevisionId, bool>` per
snapshot, filled lazily.

**Example.** A hub cascade makes every hub child unsettled until it is
drained. A record in an unrelated component stays settled and CERTIFIED,
whatever its rank.

### 3.5 I-L1 and eager dry runs

1. **Kernel.** `Kernel::run` drains the frontier at entry, before *any*
   op: commits on any branch, kernel-level ops (CreateBranch,
   ArchiveBranch, SetAccess, ShredContent), merges, WHAT_IF and
   IMPACT{Exact}. Consequences:
   - PK ranks never change while a frontier exists;
   - every frontier unit is attributed to the epoch that created it;
   - a branch never shares a frontier.
2. **Store.** `commit_group_with` drains at the top of *each* write, before
   the idempotency check and before `prepare` (store.rs:871–880).
   - `expect_certificate` therefore always validates settled state.
   - The drained epoch's projection rows are captured at that moment
     (§3.8).
3. **Group commit.** A write that leaves a frontier ends its group.
   - Writes still queued go to the next group, which drains first, so
     their ack includes the remainder.
   - The cascading commit's own ack is never delayed by a later write.
4. **Dry runs.** Journalled dry runs (`Db::dry`: dry merges, WHAT_IF,
   IMPACT{Exact}) run with `lazy: None`. Rule 1 has already drained, so
   they start and end settled, and a `require_certified` merge reads
   settled statuses.
5. **`&self` reads of the whole state.** `derived()`, `digest()` and
   `derived_digest()` assert that there is no frontier. They are not drain
   points:
   - `replace_kernel`'s debug digest comparison (store.rs:1525–1532) runs
     only when both kernels are settled;
   - `export` and kernel snapshots serialize inputs only (kernel.rs:862–897),
     so they need no drain.

### 3.6 Reads of unsettled nodes

There is one override point: `Db::dimensions(r)`, a new accessor. Every
derived read goes through it, as SPEC.md:404 already requires of
`DerivedView`. The reads that bypass `Db::node` today are moved behind it:

- `closure` (cert.rs:290);
- `query::why_not` (query.rs:55, 75);
- `current_l0` (lib.rs:559);
- `pending_rules` (cert.rs:629);
- mirror `status`;
- `Store::read_historical` and `diff` (store.rs:1355–1389);
- `BranchView::at` for epochs ≥ the frontier's (kernel_view.rs:447).

For an unsettled r:

- `structural = Unknown`, and L0/L1/L2, `sem`, witness and stale reason
  are withheld;
- currency, `conflicted` (R3), visibility and replication lag are exact;
- observation lag is **not** evaluated, because its closure needs
  witnesses. It cannot outrank StructuralUnknown spuriously.
- `summarize` then gives UNAVAILABLE, CONFLICTED or STALE(HeadSuperseded)
  where those apply, and `PENDING(StructuralUnknown)` otherwise.

**Read shapes that need settled state for every node they touch:**

| Read | When a node is unsettled |
|---|---|
| WHY and WHY_NOT | Pull (§3.7); on timeout, `Settling{epoch}` (retryable) |
| Certificate issue | Pull; on timeout, unsettled required entries are `PENDING(StructuralUnknown)`, so `NotCertified` |
| Certificate validate | Steps 0–2 run (log and branch checks, entries, HeadSuperseded and other input-level Invalid). If any required head is still unsettled after the pull, return before step 3 (the Ambiguous recompute and vector') with the §6.4 mapping: `Pending{rules: []}`, unless a higher-priority result already holds. Nothing new is added to the model |
| IMPACT{Reachable} | Index-only; allowed as is |

**History.** `Historical(e)` / `Snapshot(e)` for e below the frontier's
epoch are exact: history holds completed transitions only. A read at the
frontier's epoch is a current read.

### 3.7 Pull, idle drain and publication

**Pull.**

1. A reader computes its target set N.
2. If any r ∈ N is unsettled, it records `need = max rank(N)` and drops its
   snapshot (service.rs:1472–1474).
3. It sends `Message::Settle{epoch, through: need}` with `try_send`. A
   `Busy` result counts as an immediate timeout.
4. It waits on the publication condvar for a publication generation newer
   than the one it saw. The generation counter is new and is bumped by
   every publish, including drain-only ones.
5. On wake, it **restarts the whole request** on the new snapshot, as I4
   requires.

Waits are bounded:

- Certified/STRICT reads wait up to `read_settle_ms` (O2);
- Advisory reads send the message and do not wait;
- gates use their existing deadlines, and run on connection threads.

**Actor.**

- It serves `Settle` before idle drain, as slices that stop at the first
  rank > `through`, or at `E`, after which they continue.
- Idle drain: while a frontier exists, the actor loop uses `try_recv`. When
  no message is waiting it drains one slice and publishes.
- A drain that empties the frontier also applies that epoch's projections
  (§3.8).

**The spare kernel.** `previous` becomes a list of
`Step::{Commit(Batch, Result), Drain(Through)}`, with `Through` being
`Units(E)` or `Rank(k)`.

- The spare replays the steps in order and reaches the same frontier (R6).
- Any result mismatch fail-stops, as today.

**Clone cost.** `reclaim` falls back to a kernel clone when a reader still
holds the spare (actor.rs:722–757). Dropping the snapshot before waiting
(step 2 above) keeps that rare. G5 reports `clone_fallbacks` during a
1M cascade.

### 3.8 Projections and `ctx_changes`

For a deferred epoch e:

- **At the commit.** `capture` records the frame's input-level rows, but
  holds e's `diff_log` / head-history rows as *pending*. The projection
  watermark is held at e−1.
- **At settlement,** either at idle or at the top of the next write:
  1. the store captures e's rows from the completed `status_diff` and
     post-states;
  2. it applies them in one SQLite transaction, which advances the
     watermark to e;
  3. it then bumps the publication generation.
- **`Store::changes`** answers up to `min(log tail, last settled epoch)`.
  `reached` never passes an unsettled epoch. Not covering an unsettled
  epoch is *not* `Truncated`.
- **Long polls** wait on the publication generation, not only on the
  epoch.
- **Crash.** If ctxd crashes before settlement, reopen replays e eagerly.
  Since e > watermark (store.rs:615), its rows are captured then. Nothing
  is lost (T76 holds).

### 3.9 Commit responses and the frame

**Responses.**

- When the frontier is non-empty at ack time, the response carries
  `settling: true`, the input-level fields (`epoch`, `allocations`,
  `net_delta`), and empty `l0_flips` and `status_diff`.
- MCP and hooks show "status changes settling (epoch e); see ctx_changes".
- Otherwise the response is today's.

**The frame stores exactly the acknowledged result:**

- the full `CommitResult` when the commit settled within its slice;
- otherwise `{epoch, allocations, net_delta, settling: true}`.

Consequences:

- Idempotent retries and `TxnStatus` return `row.acked()` unchanged, so a
  retry gets what the first ack said.
- **Replay** compares the full result for settled frames and the
  input-level fields for `settling` frames. It then derives the diff
  itself.
- **Frame-result consumers** (§1.2: `ctx ui`, validators, candidates,
  service, mirror) treat `settling` frames as "diff in `diff_log`", or
  recompute the diff by replay where they already replay.

## 4. Log format 7 and protocol 9

- **The change.** Format 7 changes only how a frame's result is encoded:
  a `settling` flag, and empty diff vectors when it is set.
- **No migration rewrite.** Old segments keep format 6, and
  `scan_with_formats` decodes each segment by its own format
  (log.rs:481–520).
- **Upgrade.** At the first write after upgrade, the store rolls over to a
  new format-7 segment, as format-2 session logs did.
- **Checksums.** Every existing frame keeps its checksum, so certificates
  (`LogMismatch`), mirror tips (`LogRewritten`), kernel snapshots and
  projections stay valid. This is the T77 precedent.
- **Rollback.** An older ctxd refuses the format-7 segment header before
  truncating anything, as today. Downgrade is not supported.
- **Protocol 9.** It adds `settling` on commit responses, the `Settling`
  read error and the GC requests (§5.7).

## 5. Payload GC

### 5.1 Live contents and the keep set

A content hash `c` is **live** if any of the following holds, on any
branch, archived ones included:

1. `c` is the content of a revision that is:
   - a head;
   - a base revision;
   - a conclusion, premise (any kind) or reference of any rule, enabled or
     not;
   - a head at a retained sync point.
2. `c` is an artifact payload, a validation `meta` (current or replaced),
   or a resolution `note`.
   - Replaced metas and notes exist only in the op_log. They are found by
     scanning it, as `team::notes_in_log` and `shred_targets` already do
     (team.rs:1077–1086; service.rs:624–700).
3. `c` is referenced from inside a live document payload:
   - `ReviewDoc.meta` (contextdb-proto/src/docs.rs:150–157);
   - every hash field of `ConflictDoc` and `ResolutionDoc`.

   G0 review closes this list by grep. The rule from now on: a new doc kind
   that embeds a hash must add it to this list, enforced by a test that
   decodes every doc kind.

The **keep set** is the live contents now, plus every content that was
live at any epoch ≥ `cutoff`, where:

- `cutoff = min(now − keep_epochs, oldest open session's start epoch,
  oldest pending handoff's epoch)`;
- `keep_epochs ≥ 1` is enforced.

Certificate entries are heads when issued, so the session bound keeps
every payload a live certificate names.

**How "live at an epoch ≥ cutoff" is computed.** It is "head or base at
some epoch ≥ cutoff, or named by a rule that existed at an epoch ≥ cutoff".
It comes from `head_history` and `base_history` in `projections.sqlite`
(projection.rs:147–172), which are rebuildable. GC refuses with
`ProjectionsBehind` unless the projection watermark is at the tail.

**Collectable** contents are those in the pack index that are not in the
keep set, not shredded, and not already collected.

**Shared hashes.** Contents are shared by hash across logicals. A hash is
kept if any revision using it is kept.

### 5.2 GC is a store step, not a kernel op

Collection never changes kernel inputs, the op_log or the digest. PROP10 is
therefore unaffected, and nothing in replay depends on GC configuration.

`ctx gc --apply` sends `Request::Gc{keep_epochs, confirm}`. The handler
runs as one exclusive actor step, `Message::Gc`. It is never part of a
commit group.

1. Drain the frontier (I-L1).
2. Recompute the keep set on the actor's current kernel and log.
3. In the store, in this order:
   - (a) remove the collected hashes from `Store.blobs`;
   - (b) append them to the durable **GC ledger**
     `gc/ledger-<gen>`: the hash, the collect generation, and the
     `last_append_seq` of its key at collection; then sync and
     `sync_dir`;
   - (c) rewrite the key log once (`rewrite_keys`), dropping each
     collected hash's key *if its key's append sequence ≤ the ledger
     entry's*.
4. Purge projections: `vectors.sqlite` rows keyed by collected contents,
   jobs and candidates. Collect has its own `collected_through` mark per
   projection, separate from shred's `purged_through`.

Because the step runs between groups, no write can stage, check or name
`c` between the recomputation and step 3a.

**Crash states:**

- After (a): `blobs` is in memory only, so nothing persists.
- After (b), before (c): open reads the ledger. It drops the ledger's
  hashes from `blobs` (open builds `blobs` at store.rs:468–474, *before*
  the key rewrite at :676–681). It also redoes (c), as
  `retry_key_purge` does for shred.
- During (c): the existing key-log generation protocol (tmp, sync, rename,
  `sync_dir`, unlink) leaves the old or the new generation.

### 5.3 Staging again

A collected content may come back, for example:

- an agent supersedes back to old text;
- team catch-up copies replica bytes again (actor.rs:281–290).

`stage_blobs` treats a collected hash as absent: it is not in `blobs`.

- It writes a **fresh key** with a new append sequence, and a new record.
- A key still loaded because the rewrite failed is never reused for a
  collected hash. The ledger check compares sequences.
- The pack index points the hash at the new record.

**Reads.** `read_payload` checks in this order:

1. shredded → `Shredded`;
2. the pack location has a live key → read;
3. in the ledger → `Collected`;
4. otherwise → `Missing`.

A later rewrite (shred or collect) keeps a key whose append sequence is
newer than its ledger entry. A test covers stage again → collect or shred
another hash → reopen → still readable.

Being in the ledger does not block collection again. The ledger holds one
entry per collection.

### 5.4 Ops that make an old revision live again

Today only content-carrying ops check blob presence (txn.rs:705–708,
750–753, 882–887, 1014–1019, 1063).

From P11 on, the ops below fail with `MissingBlob{content}` when the
named revision's content is collected and not staged again. `BlobPresence`
gains `collected(c)`; it defaults to false, and replay uses `AllPresent`:

- `SetHead{new}`;
- `AssertBase`;
- `AddRule` (conclusion, every premise and reference);
- `Rebind{to}`;
- `MirrorAttach` of an existing revision;
- merges that make such a revision a head or a rule member on the target.

Shredded contents are unaffected. They keep P9 semantics.

Consequences:

- Rolling a head back past GC needs the bytes sent again, as
  `ctx_supersede` does.
- Team catch-up stages replica bytes before attaching (actor.rs:281–290),
  so it works.
- SPEC §6.2 and T107 gain these rows.

### 5.5 Disk reclaim (`ctx gc --compact`)

`--compact` runs offline, with the store lock held. It rewrites each pack
segment that holds a record whose hash is shredded (from the replayed
kernel) or in the ledger with an older-or-equal key sequence. It never
drops records just because their key is missing.

**Crash protocol.** It is new, with its own tests. migrate5 writes in place
and relies on open refusing plaintext, so it does not fit.

1. Write `pack-N.compact`.
2. Read it back and verify every remaining record.
3. Sync.
4. Write `compact.intent` naming N and the new length.
5. Sync the directory.
6. Rename it over `pack-N`.
7. Sync the directory.
8. Remove the intent.

On open, an intent file means either step 6 happened or it did not. Both
states are valid segments, so open finishes the remaining steps or deletes
the `.compact` file.

**Snapshots.** `--compact` also removes kernel snapshots that no longer
load: wrong format, or no checksum match. It keeps the newest two valid
ones. Snapshots are written only by `ctx snapshot`, so this matters only
there. PROP10 (open from snapshot + replay == full replay) is checked after
compaction in T67.

**Truncation.** `ctx doctor` truncation below an epoch at which a collected
content was live is refused without `--force`. It could resurrect a head
whose payload is gone, which would then read as `Collected`.

### 5.6 Git mirrors (O5)

SPEC E4 (SPEC.md:739) currently says P11 GC must report
`MirrorStale{PayloadGone}`. Applied literally, every mirror would become
permanently unexportable after the first GC, because:

- its history is mostly bodies of superseded heads;
- `payload_gone` scans the whole history (mirror.rs:1061–1080);
- export refuses even with `--force` (mirror.rs:1180–1188).

Shred's reason for that refusal is erasure ("its backup would keep the
payload too", mirror.rs:1181). That reason does not apply to GC. The
change:

- **`payload_gone`** keeps checking shredded contents only.
- **Re-rendering a tip** at its epoch (`render_at`, mirror.rs:967–978): for
  a collected body, the renderer reads the existing git blob and accepts it
  iff `blake3(blob bytes) == content hash`. Otherwise it is
  `MirrorModified`, as today.
- **The clean-cache mark** (mirror.rs:1056–1058) includes the GC
  generation.

SPEC E4 is amended (§11).

### 5.7 CLI and reporting

| Command | Effect |
|---|---|
| `ctx gc` | Dry run. Prints collectable count and bytes, the cutoff and its cause (keep_epochs, session or handoff), and kept counts by reason |
| `ctx gc --apply [--keep-epochs N]` | Admin socket, exclusive step (§5.2) |
| `ctx gc --compact` | Offline (§5.5) |

Reporting changes:

- `ctx doctor` reports unavailable contents as counts by reason. `Collected`
  is not a problem.
- `ctx_read` adds `unavailable: "collected"` (service.rs:3640–3644).
  Pinned and Historical reads of a collected payload return the same.

## 6. Layout ablation (D4)

These already ship and are not candidates:

- CSR with a delta map (`Adjacency`, arena.rs:509–521);
- re-sealing (`maybe_compact` folds `extra` when
  `extra_len > max(items/4, 1024)`, arena.rs:633–652).

Dirty and queued bits are cleared one at a time (counting.rs:220, 238,
283–285; layers.rs:262, 289), so epoch-stamped vectors would add bytes
without saving resets. StampedVec is dropped.

Candidates:

- **A1, radix heap.** A monotone radix heap replaces
  `BinaryHeap<Reverse<(usize, RevisionId)>>` in both passes. Pops are
  monotone (DA2).
- **A2, packed per-entity rows.**
  - Per revision: `s`, `support`, `witness` in one row.
  - Per rule: `unsat`, `counted`.
  - Per incidence: `sat` stays a bit vector.
- **A3, `pending_*` as vectors.** The `BTreeMap<RevisionId, Vec<_>>`
  pending maps become a rank-bucketed vector arena.

**Method.**

- Each candidate is measured alone against the post-milestone-1 code, on
  the local baseline machine, at 1M revisions.
- Shapes: multi_support, hub and chain. Chain is added to `contextdb-bench`.
- Metrics:
  - commit p99;
  - cascade commit, both eager and lazy;
  - total settle time;
  - WHY depth3 p99;
  - live heap B/incidence.
- Each configuration runs 5 times. The noise band is the max/min spread of
  the baseline's 5 runs.

**Adoption.**

- A candidate is adopted when it is ≥ 15 % better on live heap or on
  cascade/settle time, *outside* the noise band, and no metric is more than
  5 % worse outside the band.
- It must keep P1 and PROP1-lazy green.
- Rejected candidates keep their numbers in docs/gates/P11.md.

## 7. Postgres (D2)

Out of P11. SPEC §P11 gains an owner-decision note.

## 8. Errors, model and config

- **Produced for the first time:** `Structural::Unknown` and
  `PendingReason::StructuralUnknown`.
- **New errors:**
  - response error `Settling{epoch}`;
  - store error `ProjectionsBehind`;
  - `Unavailable::Collected`, a variant SPEC §7 already names;
  - `MissingBlob` on the ops of §5.4.
- **New fields:**
  - `settling` in the frame result and the commit response;
  - `units` in `CommitStats`.
- **Unchanged:** no new `Op`, no new kernel snapshot field.
- **Config:**
  - `[maintenance] lazy`, `lazy_budget` (O1), `read_settle_ms` (O2);
  - `[gc] keep_epochs` (O3).
- **Internal API:** `Commit` gains `lazy: Option<Budget>`; `None` means
  eager. ctxd's live commits are its only lazy caller.

## 9. Verification (Gate P11)

### G2

- **T61.** In a lazy configuration, a STRICT consumer of an unsettled node
  never gets cached CERTIFIED, `Ok`, `Valid` or `Ambiguous`. Cover:
  - certificate issue and validate across a frontier;
  - `expect_certificate` on a write after a cascade;
  - a `require_certified` merge after a cascade;
  - a gate.
- **S13-rows-e2e, StructuralUnknown row.** Through ctxd, with a hub
  cascade and a small `E`:
  - a Certified read with `read_settle_ms = 0` gives
    `PENDING(StructuralUnknown)`;
  - a record in an unrelated component stays CERTIFIED (§3.4);
  - after the drain, the first read is CERTIFIED again.
- **T67, after `--apply` and `--compact`:**
  - PROP10 holds, from a snapshot and fully;
  - every content in the keep set is readable, including §5.1(2)–(3);
  - a collected content reads `Collected`;
  - each §5.4 op on a collected revision gives `MissingBlob`, and succeeds
    after the bytes are staged again;
  - staging again → shred or collect another hash → reopen → still
    readable;
  - a commit group racing a GC cannot make a collected content live;
  - a git mirror over collected history still validates and exports;
  - crash points of §5.2 and §5.5.
- **New goldens:**
  - P-1, with a Sem head ranked right below the conclusion;
  - a slice ending exactly at a rank boundary, and one inside a paused
    fan-out;
  - I-L1 at every `Kernel::run` op kind and at each store write;
  - group ends after a deferred write;
  - settledness of an unrelated component;
  - a conflict opened on an unsettled target reads CONFLICTED (R3);
  - `diff_log` across a crash before settlement (§3.8);
  - `ctx_changes` long poll woken by settlement;
  - an idempotent retry of a `settling` commit;
  - spare replay of `Drain` steps;
  - format-6 + format-7 segments in one log, and a certificate issued
    before the upgrade still `Valid`.

### G3

- **PROP1-lazy.** The P1 property suite runs on 10,000 seeds, in lockstep
  with eager `Counting`, `FullRecompute` and the oracle.
  - Each step draws `E` from {1, 2, 3, 8, 64, ∞} and `C` from {1, 2, 4096},
    and inserts random `Settle{through}` and idle slices.
  - At each point, for every node: settled ⇒ `dimensions == jacobi`;
    unsettled ⇒ the summary is `PENDING(StructuralUnknown)` or a
    higher-priority state that the oracle also gives.
  - At each point, an exhaustive check over the whole graph confirms that
    `settled(r)` is sound: the node's final value equals its current one.
  - After full drain: `derived`, digest, history, stats and the epoch's
    `status_diff`/`l0_flips` equal eager.
- **SPEC Gate P11 G3** ("STRICT 读 == oracle") is read as: equal on settled
  state, and never wrong while unsettled. §11 amends the text.
- **PROP2/PROP2b** run on the settled diff.
- **PROP11** compares certificates with the oracle on settled states. A
  companion property checks that with frontiers present, issue and validate
  never return a verdict better than the oracle's.
- **PROP10** runs over mixed format-6/7 logs.
- **GC property.** Random histories with GC at random points, re-staging,
  §5.4 ops and reopen. Every content live at any later epoch is readable
  whenever it is live.
- **Mutation coverage** extends to slicing, settledness, I-L1, the frame
  `settling` path, the GC keep set and §5.4.

### G4

- Fuzz the format-7 frame result decoder and the GC ledger.
- Kill points through a deferred commit, an idle drain, a settle-time
  projection apply, `--apply` (§5.2) and `--compact` (§5.5).

### G5

Measured on the local baseline machine at 1M revisions. These are initial
budgets, calibrated at the gate.

- P1 budgets on multi_support and hub.
- Cascade commit ack (store, file projections) ≤ 150 ms on hub and chain,
  with default `E`. Seeding time is reported separately.
- The **next** commit's ack after a cascade, with and without an idle gap.
  Reported.
- Total settle time ≤ 1.25 × eager.
- Frame of a deferred cascade ≤ 1 KiB.
- `clone_fallbacks` during the cascade. Reported.
- Fraction of Certified reads that answer `PENDING(StructuralUnknown)` in
  the S13 trial. Reported.
- `ctx gc` dry run and `--apply` over 1M revisions; reopen at 1M. Reported.

### G7 and G8

- **G7.** Regressions of earlier gates, P9/P10 included. Team `accept`
  stays eager (R4).
- **G8.**
  - Docs: README, setup, CHANGELOG and threat model.
  - The threat model records two new effects: collected payloads read
    `Collected` for old certificates; rollback past GC needs bytes.
  - An independent implementation review.

### Milestones

1. **Core.** Interleaved pass, units, chunks, frontier, I-L1 in
   `Kernel::run`, settledness, `dimensions` override, eager dry runs.
   PROP1-lazy. Measure the unit rate and fix `E` (O1).
2. **Store.** Format 7 frames and rollover, drain at each write, settle-time
   projections, `changes` cap, mixed-format PROP10.
3. **ctxd.** Idle drain, pull and generation condvar, spare `Drain` steps,
   group ending, responses, protocol 9, T61, S13-rows-e2e.
4. **GC.** Keep set, ledger, exclusive step, re-staging, §5.4 checks,
   compact, mirrors, CLI; T67 and the GC property.
5. **Layout ablation and G5.**
6. **Gate evidence and docs.**

## 10. Risks

- **Seeding is not cut (R5).** A head move on a revision with 10⁶ direct
  dependents seeds 10⁶ incidences before the first slice. G5 reports it.
  Slicing seeding needs a rank-indexed seed queue; that is deferred unless
  G5 shows it matters.
- **The next writer pays.** Idle drain normally hides the remainder, but a
  burst right after a cascade does not get that.
- **Settledness search bound `S`.** Deep ancestors make reads conservative.
  The S13 trial reports how often.
- **Determinism of the spare.** Units are counted only from heap and mark
  operations. The G3 run replays the steps on a second `Db`.

## 11. SPEC changes (after G0)

- **§P11.**
  - The D1–D4 owner-decision note; selection narrowed as in D1.
  - Postgres moves out.
  - G3 wording per §9.
  - G5 budgets per §9.
- **SD14a.** Definitions of frontier, settled (§3.4), the read pull and its
  budget, and I-L1. Observation lag is not evaluated on unsettled nodes.
- **§3.4.**
  - DA2 restated (§3.1).
  - DA5 and the rebuild assertion apply at full drain.
  - Phase 4 (SPEC.md:440): a deferred commit publishes its snapshot and
    input-level result at ack. Its `status_diff` reaches `diff_log` at
    settlement.
- **SD8.4 / API.**
  - `ctx_commit` gains `settling`.
  - `ctx_changes` covers settled epochs only, and `Truncated` keeps its
    meaning.
- **§6.2 and T107.** `MissingBlob` on the §5.4 ops.
- **§6.4.** Validate's early `Pending` (§3.6). No table change.
- **§7.**
  - Log format 7 (the `settling` result) and rollover without rewrite.
  - `Unavailable::Collected`, the GC ledger, `ctx gc`, compaction.
  - Keep-set definition (§5.1).
- **E4 (SPEC.md:739).** GC does not raise `PayloadGone`. Collected bodies
  are verified by hash (§5.6).
- **SPEC.md:313.** The team-status statement that both sides' payloads
  stay readable gains "unless collected".
- **Appendix C.** T61, T67 and the S13 row as in §9. New goldens. The GC
  property.
- **Version 1.11.0.**

## 12. Review of r0.1

### Lazy maintenance and log format

| Finding | Fix |
|---|---|
| B1, the migration changes checksums | No rewrite; rollover to format 7 (§4) |
| B2, `Settled` frames break log invariants | Removed; frames hold the acked result (§3.9) |
| B3, `diff_log`/`ctx_changes` of deferred epochs | Settle-time capture, watermark held, `reached` capped, generation condvar (§3.8) |
| M1, actor-thread consumers | Drain at each write before `prepare`; dry runs eager; `require_certified` not a pull (§3.5) |
| M2, I-L1 incomplete | Drain at `Kernel::run` entry; `&self` reads assert; snapshot and export need no drain; mirror status through `dimensions` (§3.5–§3.6) |
| M3, conflicts read from a stale column | R3 override from the input index |
| M4, bounded ack does not hold | Pushes and marks count as units; resumable fan-out; the group ends after a deferred write; seeding bound stated; next-commit ack in G5 (§3.2, §3.5, §9) |
| M5, rank ≥ cursor is too conservative | Exact bounded backward search (§3.4) |
| M6, the pull | Snapshot dropped, generation condvar, restart, `Busy` = timeout (§3.7) |
| M7, idempotency and `TxnStatus` | The frame stores the acked result (§3.9) |
| M8, certificate semantics | The §6.4 mapping kept; early `Pending` before step 3 (§3.6) |
| M9, gate text | G3 and PROP11 restated; §11 amends the SPEC |
| Minors | P-1 restated over layer columns; DA2/DA5; full frontier list; one override point; observation skipped; `E` not stated without measurement; `lazy: None` default; frame consumers listed; drain points corrected; D1 states plainly which signals are dropped |

### GC and layout

| Finding | Fix |
|---|---|
| B1, ops revive collected revisions | §5.4 `MissingBlob` checks |
| B2, staging again vs the kernel set | Store ledger with key append sequences; read order defined (§5.2–§5.3) |
| B3, a race inside a group | GC is an exclusive actor step, never in a group (§5.2) |
| B4, admission not replayable | No kernel op; the keep set is computed by ctxd only (§5.2) |
| M1, notes and replaced metas only in the log | Log scan (§5.1(2)) |
| M2, hashes nested in docs | §5.1(3), plus a decode-every-doc test |
| M3, mirrors | O5: E4 amended, hash-verified bodies, clean mark (§5.6) |
| M4, the session rule | Cutoff from the oldest open session or handoff; `keep_epochs ≥ 1` (§5.1) |
| M5, layout candidates | A1 and StampedVec dropped (already shipped, or no gain); A2 made coherent; chain added; noise band; post-M1 baseline (§6) |
| Minors | Separate collect marks; open-time `blobs` pruning; one rewrite per GC; new compact protocol; snapshot cleanup scope; doctor counts; truncation guard; citations fixed (D3 no longer cites SD20); SPEC edits listed in §11; GC property in G3 |
