# ADR 0019 — P11 cascade-bounded lazy maintenance, payload GC and scale

Status: **Proposed (r0.4), G0 pending owner review.**

The independent final check of r0.4 found no blocker or major in either
part. Its two implementation notes are in §3.7 and §5.1 K5. The owner
confirmed O1–O5 as recommended on 2026-11-20 (§0.1). The owner's review of
the ADR remains.

r0.4 answers the second closure review of r0.3 (§12.5).

- Lazy: 1 blocker, 1 major and 6 minors.
- GC: 2 majors and 6 minors.

That review found every earlier lazy finding closed except a format-read
detail (N9). Every earlier GC finding is closed or depends on the new
fixes.

Review history:

- **r0.1.** Two independent reviews found 7 blockers, 14 majors and
  27 minors (lazy and format: 3/9/13; GC and layout: 4/5/14).
- **r0.2** answered them.
- **r0.2's closure review.** Lazy: 7 of the 12 r0.1 findings closed and
  5 partial, with 1 new blocker and 3 new majors. GC: 4 closed, 3 closed
  in substance and 2 partial, with 1 new blocker and 5 new majors.
- **r0.3** answers those. §12 maps every finding to its fix.

What changed from r0.2:

- **Certificates during a frontier.** The ChangeLog is recorded at ack.
  The epoch's history transition opens at ack and grows with every settled
  change, so a certificate over settled nodes is never judged better than
  eager would judge it.
- **Settling.** Only the store drains, and every drain that settles an
  epoch captures its projections and is published. `Kernel::run` asserts
  instead of draining.
- **Mirror export** needs full settlement.
- **Fan-in is chunked too.**
- **GC keep set.** It is computed from the op_log window `(cutoff, now]`
  instead of `head_history`. Certificates are pinned exactly. The cutoff is
  also bounded by the oldest snapshot any reader still holds.
- **GC ledger.** Entries name key-log positions, which exist.
- **Mirror check** of a collected body compares the body hash and the
  re-rendered front matter.

The owner's scope decisions of 2026-11-20 are in §0. The open points in
§0.1 are not yet confirmed. Nothing here changes the SPEC until G0 passes;
§11 lists the SPEC changes.

## 0. Scope and decisions taken

P11 lowers maintenance cost and supports larger stores while STRICT reads
still fail closed (SPEC §P11, SD14a). The SPEC scope has four parts:

- eager / lazy selection;
- layout ablation;
- an optional Postgres backend;
- payload GC.

The owner decided four things on 2026-11-20, after the baseline in §1.3.

- **D1, cascade-bounded lazy.**
  - A commit whose status propagation goes past a work budget `E` stops
    there. The rest stays as a *frontier* that is drained later.
  - A read that needs a node the frontier can still change either waits a
    bounded time for it or reports `PENDING(StructuralUnknown)`.
  - Settled results are bit-identical to eager maintenance; only *when*
    the work runs changes.
  - This narrows SPEC §P11's per-node selection by `read_heat, impact_size,
    risk, estimated_verification_cost`:
    - `impact_size`, the commit's own work, decides whether anything is
      deferred;
    - `read_heat` decides what a drain does first (§3.7);
    - `risk` is covered only by the existing rules: conflicts stay eager
      (R3) and STRICT consumers fail closed (R2);
    - `estimated_verification_cost` is not used;
    - there are no per-node eager/lazy labels and no persistent heat
      statistics.
- **D2, no Postgres backend in P11.** P10 chose the git remote as the team
  transport, so there is no team server. Postgres becomes its own phase when
  a server deployment is needed.
- **D3, payload GC is an explicit `ctx gc`.** It is a dry run by default;
  `--apply` collects.
- **D4, layout: measure, then adopt.** A candidate is merged only if it
  beats the threshold in §6.

**The design in one paragraph.**

- The eager maintainer already processes a commit as rank-ordered passes:
  every node is popped at most once, in increasing topological rank, and
  reads only nodes of lower rank (P-1, §3.1).
- Cut the interleaved pass after `E` work units and keep its whole state.
  Resuming later, with no input change in between, pops the same nodes in
  the same order with the same inputs. So the final derived state,
  `status_diff`, history and digest are those of eager maintenance.
- No input change happens in between, because the store drains the
  frontier before every kernel entry (I-L1) and journalled dry runs are
  always eager.
- While a frontier exists:
  - a node is *settled* if no frontier entry can reach it; a bounded
    backward search over PK edges decides this (§3.4);
  - unsettled nodes read as `PENDING(StructuralUnknown)` unless a
    higher-priority SD13 state applies;
  - conflicts are read from inputs.
- Input-level effects are recorded at ack: the ChangeLog and the frame. The
  per-epoch history transition fills in as nodes settle.
- A deferred epoch's diff reaches `diff_log` and `ctx_changes` when it
  settles.
- An op_log frame holds exactly what the client was acknowledged. A 1M
  cascade's frame is a few hundred bytes instead of 8 MB.
- GC never changes kernel state or the op_log:
  - ctxd computes a keep set from the current state, the op_log window
    `(cutoff, now]`, the certificates that sessions and handoffs hold, and
    the snapshots that readers hold;
  - one exclusive actor step then drops the collected keys and records
    them in a durable ledger.
- Later ops that make an old revision live again must find its payload, or
  fail with `MissingBlob`.

### 0.1 Owner decisions on the open points (confirmed 2026-11-20, all as recommended)

| # | Question | Recommendation |
|---|---|---|
| O1 | Is lazy on by default, and what is `E`? | On. `E` is fixed in milestone 1 from a measured unit rate on the local baseline machine; the target is a cascade-commit ack of about 50 ms. `lazy = false` turns it off |
| O2 | How long does a Certified/STRICT read wait for a pull before `PENDING(StructuralUnknown)`? | 50 ms (`read_settle_ms`). Advisory reads never wait |
| O3 | GC grace period | `keep_epochs = 10,000`, with a floor of 100 |
| O4 | How is disk space reclaimed? | `ctx gc --compact`, offline: rewrite packs to drop shredded and collected records (§5.5) |
| O5 | Git mirrors and GC | Amend SPEC E4: GC does not make a mirror `PayloadGone`. A collected body is verified by hash (§5.6) |

## 1. Today

### 1.1 The commit path

`Db::commit_in` (crates/contextdb-core/src/lib.rs:608) stages a batch on an
overlay and publishes inputs (`publish`, :729). The shared tail is
`finish` (:1091):

1. status seeds (`fast::status_seeds`, fast.rs:199);
2. `Maintainer::update` (:1111);
   - `Counting::update` seeds everything first (DA1, engine/counting.rs:174–201).
   - It then pops a rank-ordered heap in strictly increasing rank (DA2,
     :205–208), and drains each node's pending incidences and rules
     (:220–260).
   - On a flip it marks every downstream incidence (:271–281).
   - The heap runs to empty (:283–296).
3. the L1/L2 walk (`walk`, lib.rs:950–1037), a second rank-ordered heap
   over the same PK ranks;
4. the `derived` patch (:1135–1173);
5. `ChangeLog::record` (cert.rs:29), which records input-level moves;
6. `status_diff` (:1184);
7. a history `Transition` (:1185);
8. the `CommitResult`.

History lookups use the newest transition at or below `e` (history.rs:96–102).

Kernel-level ops return from `Kernel::run` (kernel.rs:297) without reaching
`Db::commit_in`: CreateBranch, ArchiveBranch, SetAccess and ShredContent
(kernel.rs:375–389).

Admission reads inputs only: `Stage::new`, A5, ExpectHeads, and the SD17
gate through the input index `open_conflicts`. Three things read derived
state around admission:

- `require_certified` merges, after a journalled dry merge
  (kernel_merge.rs:1073–1101);
- `expect_certificate`, in the store's `prepare` callback
  (store.rs:871–880; actor.rs:762–785);
- WHAT_IF and IMPACT{Exact}, which are journalled commits (whatif.rs:55–94).
  The actor serves them through `Store::what_if_detailed` and
  `impact_exact` (store.rs:1538–1550).

### 1.2 Where the derived diff goes

**The log.** An op_log frame stores the full `CommitResult`, and its
checksum covers it (log.rs:122–145). There is one frame per epoch, numbered
by epoch (log.rs:234–237, 523, 564; `Store::epoch() = frames.len()`,
store.rs:1336).

**Open and retries.**

- Open requires the exact current format (store.rs:492–497) and replays
  each frame, requiring `result == row.result()` (store.rs:601–614).
- Idempotent retries and `TxnStatus` return `row.acked()` (store.rs:758–770,
  858–862).

**Projections.**

- `capture` takes `diff_log` rows from `status_diff` and the batch
  (`created`, `archived`, `watermark_moves`, `visible`) right after each
  batch (store.rs:133–169, 927).
- They are applied after the group's sync (store.rs:984–995), and only at
  watermark+1 (projection.rs:290–293).
- Replay re-captures frames above the watermark (store.rs:615).
- `Store::changes` answers up to the log tail (store.rs:1481–1493).
- Long polls wait on the epoch condvar (actor.rs:203–215).

**The actor.**

- It publishes only when the store's epoch advanced (actor.rs:672).
- The spare kernel replays each published group and must agree, or ctxd
  fail-stops (actor.rs:680–745).
- Readers run on connection threads against the published `Snapshot`
  (actor.rs:177). Long polls must not hold one (service.rs:1472–1474).

**Mirrors.**

- `render_now` renders the published snapshot (mirror.rs:957–964).
- `validate` re-renders a tip by eager replay and compares trees
  (`render_at`, mirror.rs:967–1006).

**Other consumers of the frame result:**

- `ctx ui` (graph.rs:54–98);
- validators.rs:1187;
- candidates.rs:671, 691;
- service.rs:4619;
- mirror.rs:546.

All but `ctx ui` use only `net_delta` and `allocations`.

**StructuralUnknown.** `Structural::Unknown` and
`PendingReason::StructuralUnknown` exist (contextdb-model/src/status.rs:31,
49). `summarize` orders them after ObservationLag (oracle eval.rs:336).
Certificates map them to `Pending` (cert.rs:124; SPEC §6.4; T65). The engine
never produces them today (layers.rs:417–421).

**STRICT consumers.** "STRICT" means, in this ADR:

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

**Run `p11cascade`** (`p11_cascade`). One commit toggles base 0, and the
cascade reaches every revision. Ranges cover three samples per direction.

| shape (1M) | core commit | store, memory projections | store, file projections | log frame |
|---|---|---|---|---|
| hub | 1,731–1,984 ms | 3,329–3,680 ms | 3,741–4,522 ms | 8,000,216 B |
| chain | 932–966 ms | 2,284–2,669 ms | 2,780–3,489 ms | 8,000,232 B |

- Reopening a store whose log held 8 such frames took 13.5–35.1 s.
- ctxd pays the core cost again on the spare.
- No other writer is served meanwhile.

These numbers are not gate evidence.

## 2. Requirements

- **R1, exactness.** Once settled, everything equals eager maintenance:
  derived state, `status_diff`, `l0_flips`, history, `ChangeLog`, digest,
  certificates and `diff_log` (PROP1-lazy, §9).
- **R2, fail closed.** A STRICT consumer never gets a verdict better than
  the eager verdict at the same epoch. In particular it never sees
  CERTIFIED, `Ok`, `Valid`, `ValidRecertified` or `Ambiguous` computed
  from unsettled state (T61).
- **R3, conflicts stay exact.** For an unsettled node, `conflicted` is read
  from `indexes.conflicted(r)`. `publish` updates it before maintenance
  (lib.rs:842–859).
- **R4, rebuild is eager.** These run with the default `lazy: None` and
  never leave a frontier: replay, `from_inputs`, `ctx migrate`, `ctx ui`,
  the team replica, FullRecompute, and every journalled dry run. Derived
  state is never persisted.
- **R5, bounded ack.** A commit's own maintenance, before its frame is
  written and acknowledged, is at most `E` units, plus one chunk, plus one
  node's witness scan (`rules_by_concl.row(r)`), plus seeding. Seeding is
  not cut (§3.2, §10), and G5 measures it separately, including a
  Sem-cited head move.
- **R6, determinism.** Slice boundaries depend only on inputs, `E` and `C`.

## 3. Cascade-bounded lazy maintenance

### 3.1 One interleaved pass and P-1

Lazy mode interleaves the two passes by rank:

- take the smaller of the two heap tops;
- on equal rank, take the counting pop first.

**P-1.** Processing node r in either pass reads only state of nodes of rank
≤ rank(r), and of rank(r) only after r's counting pop.

- **The counting pop of r** reads its pending incidences. Their `sat` comes
  from premise `s` and input heads. Premises rank below r through PK's
  premise→conclusion edges (pk.rs:275–300).
- **The walk's `refresh(r)`** reads:
  - r's own counting state (layers.rs:337–340);
  - the layer columns `truth` and `reason` of r's premises, and of the
    current head h for a `Sem` incidence (layers.rs:193–203, 365–373).
    `pk.sync` adds `h→Logical(l)→conclusion` for every Sem incidence, so
    rank(h) < rank(r);
  - inputs and validations (layers.rs:22–35);
  - `indexes.conflicted`.
- **Ref incidences** add no edge and no derived dependency.

Debug assertions check P-1 on every layer-column and counting read.

**DA2, restated.** Each heap pops in strictly increasing rank. The walk may
hold r at the rank of the counting pop that pushed it.

**Moved assertions.** DA5, `assert_matches_rebuild` and counting's
unvisited-row check move to *full drain*.

`lazy: None` runs the same code to empty. The P1 properties and per-epoch
statistics hold on it.

### 3.2 Units, chunks and slices

**Units.** A unit is any one of:

- a heap pop or push (either pass);
- a pending mark (`dirty_inc` or `dirty_rule` set);
- a drained pending incidence or rule.

They are counted as `CommitStats.units`.

**Chunks.** Both directions of a node's work run in chunks of at most
`C = 4,096` units:

- fan-out: marking downstream incidences, the walk's `propagate`, and its
  second `rules_by_premise` loop (lib.rs:1007–1023);
- fan-in: draining `pending_inc` and `pending_rule` for r.

**Paused nodes.**

- If a slice ends inside a node, that node is *paused*. Its position
  (which loop, row and offset) and its partial pre-state are part of the
  frontier, and its rank is part of the cursor.
- A paused node in fan-in is unsettled itself. A paused node in fan-out has
  settled values, but its unpushed conclusions rank above it, so
  everything they reach is still covered by the cursor and the search
  (§3.4).
- The node's witness scan over `rules_by_concl.row(r)` is not chunked
  (R5).

**Slices.** A slice stops at the first chunk or node boundary after
`units ≥ E`.

**Seeding** (DA1) runs whole, because all pending marks for a rank must
exist before that rank pops. It is bounded by:

- the incidences in `exact_by_rev` and `pin_by_rev` of moved revisions;
- plus `sem_by_logical` of moved logicals;
- plus new and toggled rules.

### 3.3 The frontier

The frontier lives in the branch's `Db`. It holds:

- **Heaps and the paused node:** both heaps, and the paused node with its
  loop, row, offset and partial pre-state.
- **Counting state:** `pending_*`, `dirty_*`, `queued`, `popped`, `flips`
  and `previous`.
- **Layer state:** `pending_*` and `dirty_*`.
- **Walk state:** `seeds`, `visited` and `changes`, plus the mid-propagate
  `before`/`after` of a node paused in fan-out.
- **Commit context:**
  - `Maintenance.before`;
  - `undo`, `delta`, `revalidated` and `old_revisions` (needed by
    `before_view`, lib.rs:964–972);
  - the accumulated `stats`;
  - the epoch.

**At ack.**

- `ChangeLog::record` runs at ack. It is input-level (cert.rs:29–56).
- The epoch's history `Transition` is opened at ack.
- Every `NodeChange` the walk produces is appended to it, in pop order, as
  it is produced.

`state_at(r, e)` takes the newest transition at or below `e` that mentions
r (history.rs:96–102). So:

- for a settled node, history is exact at every epoch while a frontier
  exists;
- unsettled nodes are never read through history: their reads are gated
  (§3.6).

**At full drain.**

- The `derived` patch runs once. Per-slice node pushes would trip its
  `n == snapshot.nodes.len()` check (lib.rs:1143–1145).
- The open transition's changes are sorted by revision, as eager history
  is (lib.rs:1035). `state_diff` (whatif.rs:172) and lockstep comparisons
  then see identical history.

While the transition is open its order is pop order. `state_at` does not
depend on that order.

**Copies.** The frontier is never persisted (R4). `Arc::make_mut` clones it
with the `Db`. Nothing makes a second copy observable, because every
kernel entry first settles (§3.5).

**Cursor.** `cursor = min(rank of both heap tops, rank of the paused
node)`, or `None` when the frontier is empty.

### 3.4 Exact settledness

`settled(r)` holds when any of these holds:

- `cursor = None`;
- `rank(r) < cursor`;
- a backward search from r over PK in-edges, restricted to nodes of rank ≥
  cursor, finds no frontier member.

**Search details.**

- The search walks PK's virtual `Logical(l)` nodes as well (pk.rs:285–300).
  A walk Sem push goes `r → Logical(l) → c`, so c's in-edge is from
  `Logical(l)`.
- A node is a frontier member if it is queued in the counting heap
  (`queued[r]`), a walk seed or queued, or the paused node. These tests
  are O(1): every popped node has rank < cursor, except the paused node.
- The search is bounded by `S = 4,096` visited nodes. If the bound is hit,
  r is unsettled.

**Soundness.**

- Every push path is a subset of PK edges. This was checked in review:
  - counting `rules_by_premise`;
  - DA1 seeds;
  - walk `propagate` over `rules_by_premise` and `sem_by_logical`;
  - the second `rules_by_premise` loop;
  - `layers.seed`, `status_seeds` and `extra`.
- So anything the frontier can still change is reachable from a member.
- A node of rank < cursor cannot be reached.
- Settledness is closed under ancestors, so a settled head's closure and
  observation support read only final nodes.

**Memo.** The memo lives in the published `Snapshot`, not in `Db`, so
copying a `Db` never copies a stale memo. When a search succeeds, every
node it visited is recorded as settled.

**Chain stores.** In a chain-shaped store, an unrelated node can have more
than S ancestors at rank ≥ cursor. It then reads unsettled. G5 and the S13
trial report this on chain.

### 3.5 I-L1: only the store settles

1. **`Store::settle()`** is the only drain entry point for the live
   kernel. It drains the frontier to empty, captures and applies the
   settled epoch's projections (§3.8), and records `settled_since_publish`.
   The store calls it before every kernel entry:
   - each write in `commit_group_with`, before the idempotency check and
     `prepare`;
   - `what_if_detailed` and `impact_exact`;
   - `snapshot`;
   - `Message::Gc` (§5.2).

   `replace_kernel` is **not** on the list. The actor acks only after it
   publishes (actor.rs:672–678), so settling there would drain every
   cascade before its ack.

   **The spare kernel is the second drainer.** It drains through
   `Kernel::drain(Through)`, which needs no projections. It drains only
   while replaying a recorded `Drain` step (§3.7).
2. **`Kernel::run` and `Db::dry`** check that no frontier exists. In debug
   builds a violation panics; in release builds it returns
   `TxError::Unsettled`, a new variant (§8, SPEC §6.2, and the
   error-equivalence suite). This covers every op kind, including
   kernel-level ops, merges and branch creation. So a branch never shares a
   frontier, and PK ranks never change while one exists.
3. **Group commit.**
   - A write that leaves a frontier ends its group.
     `commit_group_with` returns the writes it did not process. The actor
     puts them back at the head of its queue, ahead of newer messages, so
     FIFO order holds.
   - The next group settles first, so its ack includes the remainder. The
     cascading commit's own ack is never delayed by a later write.
4. **Dry runs** (`Db::dry`: dry merges, WHAT_IF, IMPACT{Exact}) run with
   `lazy: None`. A `require_certified` merge therefore reads settled
   statuses.
5. **Whole-state `&self` reads** (`derived()`, `digest()`,
   `derived_digest()`) assert that no frontier exists.
   - `replace_kernel`'s debug check compares digests only when both kernels
     are settled. Otherwise it compares inputs plus frontier equality:
     cursor, both heaps' contents and the paused node.
   - `export` and kernel snapshots serialize inputs only (kernel.rs:862–897).
6. **Publication.** After any message, the actor publishes if the store's
   epoch advanced or if `settled_since_publish` is set. So a drain caused
   by an idempotent retry, a refused write or a dry run is always published
   (§3.7).
   - The store appends `Step::Drain(...)` to the actor's step list at the
     moment it drains. A settle at the top of a write therefore lands
     *before* that write's `Commit` step.
   - The spare replays the steps in exactly that order.

### 3.6 Reads of unsettled nodes

**The override point.** There is one: `Db::dimensions(r)`. Every derived
read goes through it, as SPEC.md:404 requires of `DerivedView`. The reads
that bypass `Db::node` today move behind it:

- `closure` (cert.rs:290);
- `query::why_not` (query.rs:55, 75);
- `pending_rules` (cert.rs:629);
- mirror `status`;
- `Store::read_historical` and `diff` (store.rs:1355–1389);
- `BranchView::at` for epochs ≥ the frontier's (kernel_view.rs:447).

`current_l0` (lib.rs:559) becomes `Option<bool>`, with `None` when r is
unsettled. Its callers treat `None` as not CURRENT.

**What an unsettled r reads as.**

- `structural = Unknown`. L0/L1/L2, `sem`, the witness and the stale reason
  are withheld.
- Currency, `conflicted` (R3), visibility and replication lag are exact.
- Observation lag is not evaluated for r, because its closure needs
  witnesses.
- `summarize` gives UNAVAILABLE, CONFLICTED or STALE(HeadSuperseded) where
  they apply, and `PENDING(StructuralUnknown)` otherwise.

**Read shapes that need settled state for every node they touch.**

| Read | When a node is unsettled |
|---|---|
| WHY, WHY_NOT | Pull (§3.7). On timeout, `Settling{epoch}` (retryable) |
| Certificate issue | Pull. On timeout, unsettled required entries are `PENDING(StructuralUnknown)`, so `NotCertified` |
| Certificate validate | See below |
| Mirror export (`render_now`) | Pull to full settlement (`through = ∞`). On timeout, export answers `Settling{epoch}`. A tree is never rendered from a snapshot with a frontier, so `validate`'s eager re-render always agrees |
| IMPACT{Reachable} | Index only; allowed |

**Certificate validate.** It runs steps 0–2 as today: the log and branch
checks, step 1's strict check against the ChangeLog (recorded at ack) and
history (exact for settled nodes), and the per-entry mapping.

If a required head is still unsettled after the pull:

1. Compute observation lag over the observation support of the *settled*
   required heads, as step 3 would (cert.rs:551–565).
2. Return the highest of:
   - any higher-priority result steps 0–2 already produced;
   - `Unknown{lag}` if any scope lags;
   - otherwise `Pending{rules: []}`.

This is the SPEC §6.4 priority. It skips step 3's Ambiguous recompute and
vector', which need every head settled. A strict ABA over settled nodes is
still caught by step 1, because the ChangeLog and history cover them.

**History.** `Historical(e)` and `Snapshot(e)` are exact for e below the
frontier's epoch. For e equal to it:

- they are exact for settled nodes, and unsettled nodes read as above;
- `Snapshot(e)` picks the logical's head from the *input* head, not from
  `state_at`'s Current scan (history.rs:117–126).

The second rule matters because a SetHead seeds both the old and the new
head. If the new one pops first and the slice stops, the old head's older
transition would still read Current.

**Verdict order.** For R2 and the PROP11 companion property, verdicts are
ranked by the SPEC §6.4 priority:

`Denied > Invalid > Conflicted > Ambiguous > Unknown > Pending > Ok`

with `Valid` and `ValidRecertified` as `Ok`. "Never better than eager"
means three things:

- the lazy verdict is never `Ok`/`Valid`/`ValidRecertified` when eager's is
  not;
- when eager's is `Denied`, `Invalid` or `Conflicted`, lazy's is the same;
- among `Ambiguous`, `Unknown` and `Pending`, the early return may answer
  `Unknown` or `Pending` where eager would answer `Ambiguous`, because
  `Ambiguous` needs every head settled.

Its `lag` list covers settled heads only, so it may be a subset of
eager's.

### 3.7 Pull, idle drain and publication

**The pull.**

1. The reader computes its target set N.
2. If any r ∈ N is unsettled, it records `need = max rank(N)`, or ∞ for
   mirror export, and drops its snapshot (service.rs:1472–1474).
3. It sends `Message::Settle{epoch, through: need}` with `try_send`. A
   `Busy` reply counts as an immediate timeout.
4. It waits on the publication condvar for a *publication generation*
   newer than the one it saw. The generation is bumped only when the
   snapshot is swapped.
5. On wake it restarts the whole request on the new snapshot (I4).

Wait bounds:

- Certified/STRICT reads: up to `read_settle_ms` (O2).
- Advisory reads send the message and do not wait.
- Gates and mirror export use their own deadlines, on connection threads.

**The actor.**

- It serves `Settle` as `Store::settle_through(k)`: slices that stop at the
  first rank > k, then publish.
- **Idle drain:** while the store's kernel has a frontier, the loop uses
  `try_recv`. When no message is waiting, it runs one slice of `E` units
  and publishes.
- A slice that empties the frontier settles the epoch (§3.8).

**The spare.** `previous` becomes a list of
`Step::{Commit(Batch, Result), Drain(Units(E) | Rank(k) | Full)}`.

- Steps are recorded in execution order. That includes a `Drain(Full)` the
  store ran at the top of a write: it precedes that write's `Commit`.
- `publish` takes the spare's steps from the store's ordered step log.
  - Today it builds them from `fresh` results, those with an epoch above
    `before` (actor.rs:686–698).
  - Built that way, a `Drain(Full)` with no `Commit` after it would be
    lost: an idempotent hit, a refused write or a WHAT_IF settle.
- The spare replays `Commit` steps with `commit_with` and `Drain` steps with
  `Kernel::drain`, and reaches the same frontier (R6).
- The debug frontier comparison compares heaps as multisets sorted by
  `(rank, r)`. A `BinaryHeap`'s internal layout depends on its push
  history.
- Any mismatch fail-stops, as today.

Example:

1. Group 1 is [W1], a deferred cascade. Its steps are [Commit(W1)].
2. Group 2 is [W2]. Its steps are [Drain(Full), Commit(W2)].
3. The spare drains W1's frontier before it replays W2.

**Clone cost.** `reclaim` falls back to a kernel clone when a reader still
holds the spare (actor.rs:722–757). Readers drop snapshots before waiting.
G5 reports `clone_fallbacks` during a 1M cascade.

### 3.8 Projections and `ctx_changes`

For a deferred epoch e:

- **At commit.** The store keeps e's batch and frame result in its frontier
  context. `capture` needs them for `created`, `archived`,
  `watermark_moves` and `visible` (store.rs:133–169). The projection
  watermark stays at e−1.
- **At settlement,** in `Store::settle` or the slice that empties the
  frontier:
  1. capture e's rows from the batch, the completed `status_diff` and the
     post-states;
  2. apply them in one SQLite transaction, which moves the watermark to e;
  3. set `settled_since_publish`.
- **`Store::changes`** answers up to `min(tail, last settled epoch)`, and
  `reached` never passes an unsettled epoch. This is not `Truncated`.
- **Long polls** wait on the publication generation.
- **Crash.** If ctxd crashes before settlement, reopen replays e eagerly and
  captures it, since e > watermark (store.rs:615). T76 holds.

### 3.9 Commit responses and the frame

**Responses.**

- With a frontier at ack, the response carries `settling: true`, the
  input-level fields (`epoch`, `allocations`, `net_delta`), and empty
  `l0_flips` and `status_diff`. MCP and hooks show "status changes
  settling (epoch e); see ctx_changes".
- Otherwise the response is today's.

**The frame** stores exactly the acknowledged result.

- Idempotent retries and `TxnStatus` return `row.acked()` unchanged.
- Replay compares the full result for settled frames, and the input-level
  fields for `settling` frames.
- The spare runs with the same `E`, so its results compare equal.
- `ctx ui` replays and takes the diff from its replay for `settling` frames.

## 4. Log format 7 and protocol 9

**What format 7 changes.** Only the encoding of a frame's result: a
`settling` flag, and empty diff vectors when it is set.

**No rewrite, mixed formats.** Old segments keep format 6. A log may hold
both formats.

- `LogRow::result()` decodes according to its segment's format.
  - `FrameLoc` records the segment format.
  - `read_frame` (store.rs:1654) decodes with it. Every log consumer goes
    through `read_frame`: `LogReader::read_row`, idempotency, `TxnStatus`
    and `checksum_at`.
- `scan_with_formats` enforces a non-decreasing `header.format` across
  segments. Today it only checks each segment against `min..=max`
  (log.rs:510–517).
- These places stop requiring a single format:
  - `Store::open`, which today returns `FormatMigrationRequired` for older
    segments (store.rs:492–497);
  - the `ctx ui` scan (graph.rs:54–58);
  - `migrate::log_format`, which reads the first segment (migrate.rs:96–158).
- They accept format-6 segments followed by format-7 segments. They refuse
  any other order, or a format above 7.

**Upgrade.**

- The first write after upgrade rolls over to a new format-7 segment.
- Every existing checksum is kept, so certificates (`LogMismatch`), mirror
  tips (`LogRewritten`), kernel snapshots and projections stay valid. This
  is the T77 precedent.

**Downgrade.** It is not supported. An older ctxd refuses the format-7
segment header before truncating anything.

**Protocol 9** adds:

- `settling` on commit responses;
- the `Settling` read error;
- the GC requests (§5.7).

## 5. Payload GC

### 5.1 The keep set

A content hash `c` is kept if it is in any of the sets K1–K5. They are
evaluated on any branch, archived ones included.

**K1, live now.** `c` is the content of a revision that is any of:

- a head;
- a base revision;
- a conclusion, premise (any kind) or reference of any rule, enabled or not;
- a head at a retained sync point.

**K2, non-revision contents.** `c` is:

- an artifact payload;
- a validation `meta`, current or replaced;
- a resolution `note`;
- a hash referenced from inside a K1/K2 document payload: `ReviewDoc.meta`
  (contextdb-proto/src/docs.rs:150–157).
  - `ConflictDoc` and `ResolutionDoc` hold no hash fields (docs.rs:205–226).
  - A test decodes every doc kind and fails if a hash field is not covered.

Replaced metas and notes exist only in the op_log, so they come from a log
scan (as `team::notes_in_log`, team.rs:1077–1104).

**K3, recent window.** `c` is the content of a revision named by any frame
in `(cutoff, now]`:

- named by any op: content fields, `SetHead{new}`, `AssertBase`,
  `RetractBase`, every `AddRule` member, `Rebind{from, to}`,
  `MirrorAttach`, and merge results;
- or named in its result's `net_delta` head and base moves, both from and
  to.

A revision that was a head or base at some epoch ≥ cutoff is either one
now (K1), or it was moved away in the window and its move is in a frame.
This holds on branches too, because a fork's inherited heads that later
move appear in the child's frames. No projection is used.

**K4, held certificates.** `c` is the content of a revision named by a
certificate's `vector` or `entries`, wherever ctxd keeps it:

- `Session.certs` (service.rs:245);
- `proof_history` and `AgentState.proofs`, the RootProof certificates
  (service.rs:3410–3418);
- `AgentState.session_cert` (service.rs:218, 4137);
- the `cert` of a pending handoff (handoff.rs:37).

These are exact pins. An abandoned session pins only its own certificates'
revisions; it does not hold back the cutoff.

Anchor range certificates (anchors.rs:459) go only to the client and are
not pinned. The threat model says so.

**Where K4 is computed.** Service computes K4 after phase 1 has taken its
snapshot, and passes it in `Message::Gc`. A certificate issued after that
point came from one of two places:

- a snapshot that K5 bounded in phase 1;
- a snapshot at an epoch ≥ E0, which the `(E0, now]` window (K3) covers.

The actor takes no session lock.

**K5, held snapshots.** `cutoff ≤ the minimum epoch of any published
snapshot still alive`.

- Each `Snapshot` carries a drop guard that registers its epoch in a shared
  `HeldEpochs` multiset at publication and removes it on drop. The actor's
  own published and spare snapshots count too. That bounds the cutoff to at
  most one group behind, which is harmless.
- No `Weak` reference is kept. `publish::reclaim`'s `Arc::get_mut`
  (publish.rs:46–58) fails whenever a `Weak` exists, which would force a
  kernel clone per group.
- Guards are dropped on reader threads, so the `HeldEpochs` lock is a leaf
  lock: nothing is acquired while holding it.

With K3, this covers:

- a reader rendering a mirror, which holds its snapshot for up to 120 s per
  git call;
- the indexer;
- a certificate being issued. Issuance stores the certificate while still
  holding its snapshot (service.rs:3410–3430, 3613–3617, 4135–4138).

**The cutoff.** `cutoff = min(now − keep_epochs, K5 bound)`.
`keep_epochs ≥ 100` is enforced (O3).

**Collectable** contents are those in the pack index that are not kept, not
shredded, and not collected under their current live key.

**Shared hashes.** Contents are shared by hash across logicals. A hash is
kept if any of its revisions is kept.

### 5.2 GC is a store step, not a kernel op

Collection never changes kernel inputs, the op_log or the digest. PROP10 is
unaffected, and nothing in replay depends on GC.

`ctx gc --apply` sends `Request::Gc{keep_epochs, confirm}`. The work is
done in two phases.

**Phase 1, outside the actor**, on a held snapshot at epoch E0. It
computes:

- the candidate set: collectable at E0 under K1 and K2, with K2's log scan
  run through E0;
- K3 for `(cutoff, E0]`.

This is the expensive part (a full keep set and a log scan). It does not
block writers.

**Phase 2, one exclusive actor step, `Message::Gc{candidates, E0}`**, never
part of a commit group:

1. `Store::settle()` (I-L1).
2. Re-check only what can have changed since E0:
   - K3 for `(E0, now]`, which also covers K1 changes made since E0, since
     every way a revision becomes live is an op in a frame;
   - every K2 kind in `(E0, now]`: artifact payloads (`OpenArtifact` does
     no presence check, txn.rs:999–1011), notes, metas, and nested
     `ReviewDoc.meta`;
   - K4 now;
   - the K5 bound now;
   - contents staged again since E0.

   Remove all of these from the candidates.
3. In the store:
   - (a) Append the collected entries to the **GC ledger**:
     - the ledger lives at `gc/ledger`, a framed append-only file with a
       blake3 per record;
     - each entry is `(content, key_pos, gc_generation, cutoff, now)`;
     - `key_pos` is the key-log position (segment number, byte offset) of
       the live key being collected. It is matched together with the hash.
     - A byte offset survives media damage to earlier records. A record
       ordinal does not: `keys::parse` resyncs past a bad record
       (keys.rs:94–111), which would shift every later ordinal.
     - after appending, sync the file and `sync_dir`.
   - (b) Update the shared `PackIndex` under its lock: insert the entries
     into `PackIndex.collected` and remove the hashes from `Store.blobs`.
     Reader threads use the same `PackIndex` (store.rs:1658–1669), so from
     here on they read `Collected`.
   - (c) Rewrite the key log once (`rewrite_keys`). Drop each key whose
     position equals a ledger entry's `key_pos`.
4. Purge projections:
   - `vectors.sqlite` rows keyed by collected contents;
   - jobs and candidates.

   Each projection gets its own `collected_through` mark, beside shred's
   `purged_through`.

**Failure and crash states.**

| Failure point | Result |
|---|---|
| (a) fails | Nothing in memory has changed; the step answers an error |
| (a) succeeds, (b)/(c) fail | The store is poisoned, as `put_blobs` failure is (store.rs:1161–1185). Reopen redoes (b) and (c) from the ledger |
| Crash after (a) | Open reads the ledger. For each entry whose `key_pos` is the hash's *current* loaded key position, it drops the hash from `blobs` (before the key rewrite at store.rs:676–681, as shred's purge) and redoes (c). An entry whose hash has a newer key (staged again) is ignored |
| During (c) | The key-log generation protocol (tmp, sync, rename, `sync_dir`, unlink) leaves the old or the new generation |

**Key-log generation header.** It gains `collect_generation` beside shred's
`purged_through` (keys.rs:5–8; store.rs:1059–1101).

`keys_need_purge` is true when either holds:

- a loaded key's position matches a ledger entry;
- the header's generation is older than the ledger's.

**Key positions.** Positions are monotone. Generation segments take numbers
from `key_next`, above every earlier segment (keys.rs:128–155;
store.rs:1124–1131).

- The rewrite sorts survivors by hash, but each survivor *gets a new
  position* in the new generation.
- The ledger stores the position collected at that time.
- So a survivor with a new position never matches an old ledger entry.

### 5.3 Staging again

A collected content can come back, for example:

- an agent supersedes back to old text;
- team catch-up copies replica bytes again (actor.rs:281–290).

`stage_blobs` treats a collected hash as absent, because it is not in
`blobs`. It writes:

- a fresh key at a new position;
- a new pack record.

The pack index points the hash at that record. A still-loaded key (left
because the rewrite failed) is never reused for a hash whose loaded
position matches a ledger entry.

**Live key.** A *live key* is a loaded key whose `(hash, position)`
matches no ledger entry. So a key that step (b) recorded but step (c) has
not yet dropped is already not live. Readers see `Collected` from (b) on.

**Reads.** `read_payload` checks in this order:

1. Shredded → `Shredded`.
2. The pack location's record authenticates under the hash's live key →
   read it.
3. The hash has a ledger entry → `Collected`.
4. Otherwise → `Missing`.

A later rewrite keeps the newer key, because positions differ. The test
"stage again → collect or shred another hash → reopen → still readable"
covers this.

**Re-collection.** A ledger entry does not block collecting the hash again.
The new entry names the new key position.

### 5.4 Ops that make an old revision live again

These ops fail with `MissingBlob{content}` when the named revision's
content is collected and not staged again:

- `SetHead{new}`;
- `AssertBase`;
- `AddRule` (conclusion and every premise and reference);
- `Rebind{to}`;
- `MirrorAttach` of an existing revision;
- merges that make such a revision a head or a rule member on the target.

How the check is wired:

- `BlobPresence` gains `collected(c)`, defaulting to false.
- Replay uses `AllPresent`.
- `Kernel::merge` receives the store's presence.
- Dry runs (`what_if_detailed`, `impact_exact`) receive a presence whose
  `contains` is always true, as `AllPresent` is today (store.rs:1538–1549),
  and whose `collected(c)` comes from the store.
  - A WHAT_IF carries no blobs (service.rs:2101–2110), so a full presence
    would fail every new content.
  - The §5.4 check is the only difference from today, so a WHAT_IF and its
    commit agree on it.
- Shredded contents keep P9 semantics: none of these ops check them.

**Team catch-up** must never be refused for local content (SPEC.md:295).

- `team::contents` (team.rs:281–287) resolves the content of
  `SetHead.new`, `AssertBase` and the other revision-naming ops through the
  kernel, and stages it from the replica.
- If the replica no longer has those bytes (after team compaction), the
  `Channel::Replica` exemption applies, as it does for shredded content
  (txn.rs:1088–1092). The op is admitted, and the mirror's head reads
  `Collected`.
- `ctx doctor` and `ctx team status` report a `Collected` **head** as a
  problem. Only non-head collected contents are harmless.

Rolling a head back past GC otherwise needs the bytes sent again, as
`ctx_supersede` does. SPEC §6.2 and T107 gain these rows.

### 5.5 Disk reclaim (`ctx gc --compact`)

`ctx gc --compact` is an offline command that takes the store lock.

**Which records go.** It rewrites each pack segment that holds a record
which both:

- does not authenticate under its hash's current live key;
- and whose hash is shredded (from the replayed kernel) or has a ledger
  entry.

It never drops a record only because its key is missing. A damaged key
log is reported, not compacted.

**Crash protocol.**

1. Write `compact/pack-N.new`, outside `blobs/`. Open's sweep of non-pack
   names in `blobs/` (store.rs:475–486) would otherwise remove it.
2. Read it back and verify every remaining record, then sync.
3. Rename it over `blobs/pack-N`, then `sync_dir` both directories.

The rename is atomic, and both states are valid segments. Open removes
leftover `compact/*.new` files *before* the `blobs/` sweep. No intent file
is needed.

**Ledger.** `--compact` also rewrites `gc/ledger` as a new generation. It
keeps one entry per hash that still has no live record, and drops entries
whose hash was staged again.

**Snapshots.** It removes kernel snapshots that no longer load (wrong
format or no checksum match), keeping the newest two valid ones. Snapshots
come only from `ctx snapshot`.

T67 checks PROP10 after compaction.

**Truncation.** `ctx doctor` truncation to an epoch t below
`max(ledger cutoff)` is refused without `--force`. At t a collected
content may have been live. The data source is the ledger only, so this
works on a damaged log.

### 5.6 Git mirrors (O5)

**The problem with SPEC E4 (SPEC.md:739) as written.** It requires GC to
report `MirrorStale{PayloadGone}`. That would make every mirror permanently
unexportable after the first GC:

- `payload_gone` scans the whole history (mirror.rs:1061–1080);
- export refuses even with `--force` (mirror.rs:1180–1188);
- shred's reason for refusing, that the backup keeps the payload
  (mirror.rs:1181), does not apply to GC.

**The change.**

- `payload_gone` checks shredded contents only.
- **Re-rendering a tip** (`render_at`). For a body whose content is
  collected:
  1. Read the git blob at the path the re-render computes for that
     logical.
  2. Accept it iff `blake3(body_bytes(blob)) == content`. This matches
     `payload_gone`'s own check (mirror.rs:1072–1075; `body_bytes`,
     :292–296).
  3. Also require that the blob's front matter equals the re-rendered front
     matter. Key, logical, revision, kind, status label and premises are
     all derivable without the payload (mirror.rs:322–388).
  4. Anything else is `MirrorModified`, as today.
- **The clean-cache mark** (mirror.rs:1056–1058) includes the GC
  generation.

### 5.7 CLI and reporting

| Command | Effect |
|---|---|
| `ctx gc` | Dry run (phase 1 only). Prints collectable count and bytes, the cutoff and its cause (keep_epochs or a held snapshot), and kept counts by K1–K5 |
| `ctx gc --apply [--keep-epochs N]` | Admin socket (§5.2) |
| `ctx gc --compact` | Offline (§5.5) |

Reporting:

- `ctx doctor` reports unavailable contents as counts by reason. `Collected`
  is not a problem.
- `ctx_read` adds `unavailable: "collected"` (service.rs:3640–3644). Pinned
  and Historical reads return the same.

**Daemon projections after GC.**

- Recall indexes heads only, and heads are K1.
- Vectors and candidates rebuild from kept contents only. So
  `ctx reindex` after GC gives the same rows as the purged incremental
  state, and T67 checks it.

## 6. Layout ablation (D4)

**Already shipped, so not candidates:**

- CSR with a delta map (`Adjacency`, arena.rs:509–521);
- re-sealing (`maybe_compact`, arena.rs:633–652).

**Dropped.** StampedVec: bits are cleared one at a time (counting.rs:220,
238, 283–285; layers.rs:262, 289), so stamps would add bytes and save
nothing.

**Candidates.**

- **A1, radix heap.** A monotone radix heap replaces the `BinaryHeap` in
  both passes. Pushes go to strictly higher ranks.
- **A2, packed rows.** Per revision: `s`, `support` and `witness`. Per
  rule: `unsat` and `counted`. `sat` stays a bit vector.
- **A3, pending vectors.** The `pending_*` BTreeMaps become a rank-bucketed
  vector arena.

**Method.**

- Each candidate is measured alone against the post-milestone-1 code.
- Runs are on the local baseline machine, at 1M revisions, on
  multi_support, hub and chain. Chain is added to `contextdb-bench`.
- Metrics:
  - commit p99;
  - cascade commit, eager and lazy;
  - total settle time;
  - WHY depth3 p99;
  - live heap B/incidence.
- 5 runs each. The noise band is the baseline's max/min spread.

**Adoption rule.**

- At least 15 % better on live heap or on cascade/settle time, outside the
  band.
- No metric more than 5 % worse outside the band.
- P1 and PROP1-lazy stay green.
- Rejected candidates keep their numbers in docs/gates/P11.md.

## 7. Postgres (D2)

Out of P11. SPEC §P11 gains an owner-decision note.

## 8. Errors, model and config

**New behaviour.**

- `Structural::Unknown` and `PendingReason::StructuralUnknown` are produced
  for the first time.
- New errors:
  - `Settling{epoch}`;
  - `ProjectionsBehind`, kept only for `--compact`'s replay check;
  - `Unavailable::Collected`;
  - `TxError::Unsettled`.
- `MissingBlob` is raised on the §5.4 ops.

**New fields and types.**

- `settling` in the frame result and the commit response;
- `CommitStats.units`;
- `current_l0 → Option<bool>`.

**Unchanged.** There is no new `Op` and no new kernel snapshot field.

**New files.**

- `gc/ledger`;
- `compact/`;
- the key-log header field `collect_generation`.

**Config.**

- `[maintenance] lazy`, `lazy_budget` (O1), `read_settle_ms` (O2);
- `[gc] keep_epochs` (O3).

**Internal API.** `Commit` gains `lazy: Option<Budget>`. ctxd's live commits
are the only lazy caller.

## 9. Verification (Gate P11)

### G2

- **T61.** A STRICT consumer of an unsettled node never gets a verdict
  better than eager's. Cover:
  - issue and validate across a frontier;
  - the N1 counterexample: a strict certificate whose base revision was
    retracted and settled in the first slice, which must be
    `Invalid{StrictChange}`;
  - `expect_certificate` on the next write;
  - a `require_certified` merge;
  - a gate.
- **S13-rows-e2e (StructuralUnknown).** Through ctxd, with a hub cascade
  and a small `E`:
  - a Certified read with `read_settle_ms = 0` gives
    `PENDING(StructuralUnknown)`;
  - an unrelated record stays CERTIFIED;
  - after the drain, it is CERTIFIED again.
- **T67**, after `--apply` and `--compact`:
  - PROP10 holds, from a snapshot and fully;
  - every K1–K5 content is readable, including a fork case (a branch's
    inherited head superseded on the branch inside the window, plus a
    session certificate) and a mirror export holding an old snapshot;
  - a collected content reads `Collected`;
  - each §5.4 op gives `MissingBlob`, then succeeds after the bytes are
    staged again;
  - stage again → collect or shred another hash → reopen → still readable;
  - crash points of §5.2 and §5.5;
  - mirrors over collected history still validate and export, and a
    hand-edited front matter on a collected body is `MirrorModified`;
  - team catch-up of a reverted head whose content was collected;
  - `ctx reindex` equals the purged state.
- **New goldens:**
  - P-1 with a Sem head ranked right below the conclusion;
  - a slice ending at a rank boundary, inside fan-out, and inside fan-in;
  - `Kernel::run` and `Db::dry` refusing a frontier;
  - a settle by an idempotent retry, a refused write and a WHAT_IF is
    published and captured;
  - group ending and requeue order;
  - settledness of an unrelated component, and the chain S-bound case;
  - a conflict on an unsettled target reads CONFLICTED;
  - `diff_log` across a crash before settlement;
  - a `ctx_changes` long poll woken by settlement;
  - an idempotent retry of a `settling` commit;
  - spare replay of `Drain` steps;
  - a mixed format-6/7 log, with a certificate issued before the upgrade
    still `Valid`;
  - validate's early return with a lagging settled scope gives
    `Unknown{lag}`.

### G3

- **PROP1-lazy.** The P1 suite runs on 10,000 seeds, in lockstep with eager
  `Counting`, `FullRecompute` and the oracle.
  - Per step, `E` is drawn from {1, 2, 3, 8, 64, ∞} and `C` from
    {1, 2, 4096}, with random `Settle{through}` and idle slices.
  - At every point, for every node:
    - settled ⇒ `dimensions == jacobi`, and its history at every epoch
      equals eager history;
    - unsettled ⇒ the summary is `PENDING(StructuralUnknown)`, or a
      higher-priority state the oracle also gives.
  - An exhaustive check confirms that `settled(r)` is sound: the final
    value equals the current one.
  - After full drain: `derived`, digest, history, stats and the epoch's
    `status_diff` and `l0_flips` equal eager.
- **SPEC Gate P11 G3** ("STRICT 读 == oracle") is read as equal on settled
  state, never better while unsettled. §11 amends the wording.
- **PROP2 and PROP2b** run on the settled diff.
- **PROP11** runs on settled states. A companion property checks that with
  frontiers present, issue and validate never return a verdict better than
  the oracle's at the same epoch.
- **PROP10** runs on mixed format-6/7 logs.
- **GC property.** Random histories with GC at random points, re-staging,
  §5.4 ops, forks, sessions, held snapshots and reopen. Every content in
  K1–K5 at GC time is readable at that time, and every content later made
  live through a §5.4 op is readable.
- **Mutation coverage** extends to slicing, settledness, settle and
  publish, the `settling` frame path, the keep set, the ledger and §5.4.

### G4

- Fuzz the format-7 frame result decoder and the GC ledger.
- Kill points through:
  - a deferred commit;
  - an idle drain;
  - a settle-time projection apply;
  - `--apply` steps (a) to (c);
  - `--compact`.

### G5

Measured on the local baseline machine at 1M revisions. Initial budgets,
calibrated at the gate:

- P1 budgets on multi_support and hub.
- Cascade commit ack (store, file projections) ≤ 150 ms on hub and chain
  with the default `E`.
- Seeding time reported for a base toggle and for a SetHead on a record
  that 10⁵ rules cite through Sem.
- The next commit's ack after a cascade, with and without an idle gap:
  reported.
- Total settle time ≤ 1.25 × eager.
- A deferred cascade's frame ≤ 1 KiB.
- Reported:
  - `clone_fallbacks`;
  - the fraction of Certified reads that answer StructuralUnknown, on hub
    and chain;
  - `ctx gc` phase 1 and phase 2 times;
  - reopen time.

### G7 and G8

**G7.** Regressions of earlier gates, P9 and P10 included. Team `accept`
stays eager.

**G8.**

- Docs: README, setup, CHANGELOG and threat model.
- The threat model notes:
  - collected payloads read `Collected` for old certificates no session
    holds;
  - rolling back past GC needs the bytes again;
  - a mirror over collected history is verified by hash.
- An independent implementation review.

### Milestones

1. **Core:** interleaved pass, units, chunks, frontier, ChangeLog at ack,
   incremental history, settledness, `dimensions`, `Kernel::run` and `dry`
   assertions, eager dry runs. PROP1-lazy. Measure the unit rate and fix
   `E` (O1).
2. **Store:** format 7 and mixed-format scanning, `Store::settle`,
   settle-time projections, the `changes` cap, group ending.
3. **ctxd:** idle drain, pull, publication generation, `Drain` steps,
   mirror export settlement, responses, protocol 9, T61, S13-rows-e2e.
4. **GC:** keep set K1–K5, two-phase step, ledger and key positions,
   re-staging, §5.4 and team catch-up, compact, mirrors, CLI. T67 and the
   GC property.
5. **Layout ablation and G5.**
6. **Gate evidence and docs.**

## 10. Risks

- **Seeding is not cut (R5).** A SetHead on a record that 10⁶ rules cite
  through Sem seeds 10⁶ incidences before the first slice. G5 measures it.
  Slicing seeding needs a rank-indexed seed queue; that is deferred unless
  G5 shows it matters.
- **The next writer pays** for a remainder that idle drain has not yet
  done.
- **The settledness bound S** makes chain-shaped stores read
  conservatively. Reported.
- **Determinism of the spare.** Units come only from heap and mark
  operations. The G3 run replays the steps on a second `Db`.
- **GC phase 2 cost** is proportional to the window `(E0, now]` and to K4.
  Phase 1 runs outside the actor.

## 11. SPEC changes (after G0)

- **§P11:**
  - D1–D4 note; selection narrowed as in D1;
  - Postgres moves out;
  - G3 wording and G5 budgets per §9.
- **SD14a:** frontier, settled (§3.4), the pull and its budget, I-L1. Lag is
  not evaluated on unsettled nodes.
- **§3.4:**
  - DA2 restated;
  - DA5 and the rebuild assertion at full drain;
  - Phase 4 (SPEC.md:440): a deferred commit publishes its snapshot,
    input-level result, ChangeLog and partial history at ack, and its
    `status_diff` reaches `diff_log` at settlement.
- **SD8.4 / API:**
  - `ctx_commit` gains `settling`;
  - `ctx_changes` covers settled epochs only;
  - `Truncated` keeps its meaning.
- **§6.2 and T107:** `MissingBlob` on the §5.4 ops; the team catch-up
  exemption.
- **§6.4:** validate's early return (§3.6); the table is unchanged.
- **§7:**
  - log format 7 and mixed-format logs;
  - `Unavailable::Collected`;
  - the GC ledger;
  - the keep set K1–K5;
  - `ctx gc`;
  - compaction.
- **E4 (SPEC.md:739):** GC does not raise `PayloadGone`; collected bodies
  are verified by hash and front matter.
- **SPEC.md:313:** team payloads stay readable "unless collected".
- **Appendix C:** T61, T67 and the S13 row per §9; new goldens; the GC
  property.
- **Version:** 1.11.0.

## 12. Reviews

### 12.1 r0.1, lazy maintenance and log format

| Finding | Fix | r0.2 closure |
|---|---|---|
| B1, migration changed checksums | No rewrite; rollover (§4) | Closed. Mixed-format scanning added in r0.3 (§4) |
| B2, `Settled` frames | Removed; the frame holds the acked result | Closed |
| B3, `diff_log` of deferred epochs | Settle-time capture, held watermark, `reached` cap | Partial. r0.3: only the store settles, every settle is captured and published, and the generation is bumped at the swap (§3.5, §3.7–§3.8) |
| M1, actor-thread consumers | Settle before `prepare`; dry runs eager | Closed |
| M2, I-L1 incomplete | Kernel entry; `&self` asserts | Partial (mirrors). r0.3: export needs full settlement (§3.6) |
| M3, stale conflict column | R3 | Closed |
| M4, bounded ack | Units, resumable fan-out, group ending | Partial. r0.3: fan-in chunked, R5 wording corrected, Sem SetHead in G5 |
| M5, cursor too conservative | Backward search | Closed. r0.3 adds `Logical(l)` nodes, O(1) membership and the memo in `Snapshot` |
| M6, pull | Snapshot dropped, restart, `Busy` | Partial. r0.3: drains without a new epoch are published (§3.5.6) |
| M7, idempotency | Frame = acked result | Closed |
| M8, certificate semantics | Early `Pending` | Partial. r0.3: lag before the early return; ChangeLog at ack (§3.6, §3.3) |
| M9, gate text | Restated | Closed |

### 12.2 r0.2 closure, lazy maintenance (new findings)

| Finding | Fix |
|---|---|
| N1, deferred ChangeLog and history give better-than-eager validate verdicts | ChangeLog at ack; history grows per settled change; T61 case (§3.3) |
| N2, a drain with no new epoch is never published | `settled_since_publish`; publish after any message (§3.5.6) |
| N3, WHAT_IF/IMPACT drain bypasses the store | Only `Store::settle` drains; `Kernel::run` asserts (§3.5) |
| N4, mirror export during a frontier | Full-settlement pull or `Settling` (§3.6) |
| N5–N11 (minors) | `Logical(l)` in the search and O(1) membership; memo in `Snapshot`; S bound reported on chain; group-ending requeue; mixed-format scan list; batch kept for settle-time capture; R5 wording |

### 12.3 r0.1, GC and layout

| Finding | Fix | r0.2 closure |
|---|---|---|
| B1, ops revive collected revisions | §5.4 checks | Closed. r0.3 adds team catch-up staging plus the replica exemption, and presence in dry runs |
| B2, staging again | Ledger | Partial. r0.3: key positions, conditional open-time drop, ledger in `PackIndex`, poison on failure (§5.2–§5.3) |
| B3, race in a group | Exclusive step | Closed |
| B4, replay | No kernel op | Closed |
| M1, notes in the log | Log scan | Closed. r0.3 moves the scan out of the actor (phase 1) |
| M2, nested hashes | §5.1 | Closed |
| M3, mirrors | E4 amended | Partial. r0.3: `body_bytes` hash plus front-matter equality (§5.6) |
| M4, session rule | Cutoff | Partial. r0.3: exact certificate pins (K4) and held-snapshot bound (K5) |
| M5, layout | §6 | Closed |

### 12.4 r0.2 closure, GC (new findings)

| Finding | Fix |
|---|---|
| N1, `head_history` misses fork-inherited heads | K3 from the op_log window; fork case in T67 (§5.1) |
| N2, catch-up stalls | Stage from the replica; replica exemption (§5.4) |
| N3, no key sequence; unconditional open drop; ledger not shared with readers | Key positions; conditional drop; `PackIndex.collected`; poison (§5.2–§5.3) |
| N4, mirror hashes the wrong bytes | `body_bytes` plus front-matter equality (§5.6) |
| N5, exclusive step scans everything | Two phases; the actor re-checks `(E0, now]` only (§5.2) |
| N6, reader snapshots older than the cutoff | K5 held-snapshot bound; `keep_epochs ≥ 100` |
| N7, abandoned sessions pin the cutoff | Exact certificate pins (K4) |
| n1–n5 and N8 (minors) | Presence in dry runs and merges; window scan covers `Rebind{from}`; truncation guard from the ledger; `ProjectionsBehind` no longer needed for apply; compact files outside `blobs/`, no intent file, record criterion stated; ledger generations at compact; daemon projections after GC stated and tested |

### 12.5 r0.3 second closure review

#### Lazy

| Finding | Fix |
|---|---|
| B1, settling in `replace_kernel` drains before every ack | Removed from the settle list. The debug check compares inputs plus frontier equality while a frontier exists (§3.5 items 1 and 5) |
| M1, spare step order and its drain entry | Steps recorded in execution order, so a settle precedes the write's `Commit`. `Kernel::drain` for the spare, which is the second drainer (§3.5, §3.7) |
| N9 (partial), per-frame format | `FrameLoc` records the segment format, and `read_frame` decodes with it (§4) |
| m1, history order; `Snapshot(e)` at the frontier epoch | Sorted at full drain; input head for `Snapshot(e)` (§3.3, §3.6) |
| m4, validate order | Verdict order and "never better" defined; lag subset stated (§3.6) |
| m5, missing `TxError::Internal` | New `TxError::Unsettled` (§3.5, §8) |
| m6, scan order | Non-decreasing segment format enforced in `scan_with_formats` (§4) |
| m2, m3 | Confirmed sound; no change |

#### GC

| Finding | Fix |
|---|---|
| G1, `Weak<Snapshot>` defeats reclaim | Drop-guard `HeldEpochs` multiset; no `Weak` (§5.1 K5) |
| G2, store presence breaks dry runs | Dry runs: `contains` true, `collected` from the store (§5.4) |
| G3, where certificates are held | K4 lists every holder; anchor range certificates are unpinned and noted in the threat model |
| G4, phase-2 re-check of all K2 kinds | Added (§5.2) |
| G5, robust `key_pos` | Byte offset, matched with the hash (§5.2) |
| G6, live key | Defined; readers see `Collected` from step (b) (§5.3) |
| G7, `Collected` mirror heads | Reported as problems (§5.4) |
| G8, where K4 is computed | In Service, passed in `Message::Gc`; the actor takes no session lock (§5.1) |
