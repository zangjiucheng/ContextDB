# ADR 0016 — P8 retrieval, packing and the git mirror

Status: **accepted r0.3**, 2026-11-04. **P8 G0 passed** after two review
rounds (§13). r0.1: no blocker, 7 majors, 12 minors. r0.2's closure check
left 2 majors (N1, N3) and some minor items. All were text fixes, applied in r0.3.
Nothing here is implemented until G0 passes and the SPEC amendments (§11)
land.

## 0. Scope and decisions taken

P8 implements SPEC §5 P8. Its goal is retrieval that agents actually use,
without weakening a single certified guarantee:

- the recall projection as a health-reporting index (`IndexState`), rebuilt
  by `ctx reindex` (§2);
- hybrid ranking: key match, bm25 and, optionally, vector similarity,
  fused with reciprocal rank fusion (RRF, k = 60). Status is a hard filter
  applied before any cut (§3);
- read levels `abstract → outline → full`, with every expansion recorded in
  the read ledger (§4);
- token-budget packing that never drops or changes a status label (§5);
- derived Summaries: the pending count visible on read, and the digest
  early stop the kernel already gives (§6);
- embedding contradiction candidates that feed P7's Conflict flow (§7);
- a one-way git mirror under `refs/contextdb/…` with hand-edit detection
  (§8).

Gate items: T54–T57 and the new T85–T97 (§10).

**No op_log change.** Every new index is a projection rebuilt from the
op_log (SPEC §3.5 #7), so P8 needs no `ctx migrate`. Two other formats
change:

- The session log goes to format 4 for read levels (§4.2). As in P7 it rolls
  over to a new segment, and old segments stay readable. A format-4 session
  log cannot be read by older binaries, so a downgrade needs the session log
  removed (sessions restart).
- `mirror.sqlite` is a cache that the git objects can rebuild (§8.4).

**Decisions on the four open points (2026-11-04).** The owner said
"continue" without choosing among the options below, so this draft takes
the recommended option for each. The G0 review found all four acceptable,
with the conditions noted; the owner can override any of them before G0
passes.

- **E1, embedding source.** An external-command adapter, like P7's
  validators, with no model or runtime inside ContextDB. It is **off by
  default**, so P8 works with FTS only. Bodies go to the configured command,
  so configuring one is the owner's privacy decision (§7, §10 threat
  model).
- **E2, vector storage.** `f32` BLOBs in a SQLite projection, loaded into an
  in-memory cache per model and searched brute-force by cosine. There is no
  `sqlite-vec` dependency.
  - Memory is `4 × dims` bytes per live head: 40 MB at 10⁴ × 1,024 dims and
    400 MB at 10⁵.
  - Above `vectors.max_cache_mb` (default 256), the cache holds only the most
    recently touched heads, and the rest are scanned from SQLite. G5 reports
    both paths.
  - `sqlite-vec` stays an option for P10 (scale).
- **E3, G6 labels.** Two independent agents label the retrieval query set,
  and they never see any system's output. They find the relevant records by
  reading the whole store export: every live head, grouped by scope and
  kind. Disagreements are reported as disputed. Human labels are optional
  comparisons (SPEC §4.4).
- **E4, hand edits to the mirror.** Export refuses to move a ref whose tip
  is not a commit ContextDB would have written (`MirrorModified`, §8.4).
  `ctx mirror proposals` lists each edited file as a supersede proposal.
  Nothing is written back.

**P7 follow-ups taken here** (P7 gate §3):

- m4: a pair review commits only while both sides are the heads, and a
  skipped pair is released without counting an attempt (§7.3).
- m6: a validator or embedder child is kept in its process group, its pipes
  are closed on kill, and over-long output fails as `Malformed`. Embedder
  children get a minimal environment and an empty working directory, as
  `claude` does (§7.1).
- m7: there is one open two-sided Conflict per unordered pair on a branch,
  checked inside the writer (§7.3).

Out of scope:

- permissions and redaction beyond today's `Authorized ≡ true` (P9). The
  filter in §3.2 is written so that P9 adds a visibility check in the same
  place;
- lazy maintenance and index sharding (P10);
- LLM-generated summaries. An agent writes a Summary, and P8 only reports
  how fresh it is (§6; SPEC §3.5 #14);
- any new writer of ValidationRecords (§6);
- pushing the mirror anywhere (§8.5).

## 1. Today

Everything listed is at `882108e` plus the P7 review fixes.

- **`ctx_recall`** (`service.rs` `fn recall`, MCP `ctx_recall`).
  - It is reachable only on the agent channel, bound to a session. There is
    no `ctx recall` CLI.
  - It takes `query`, `scope` (an exact lane), `mode` and `limit` (clamped
    to 1..=1,000).
  - It ranks exact, prefix and substring key matches over every logical on
    the branch (ranks 0–2), then FTS hits in bm25 order (rank 3+). An FTS hit
    counts only while it is still the head in the snapshot.
  - It then truncates to `limit`, and **only after that** sorts uncertified
    items into `withheld` in certified mode. Withheld items therefore use up
    `limit` slots, and a page can be all withheld (§3 fixes this).
  - Items carry `handle, key, logical, rev, kind, summary, status`, with no
    body text: the shape is header only. There is no `abstract`, no anchors
    and no `budget_tokens`.
  - Certified recall with items issues one recall certificate (SPEC §6.4)
    and records each item as a `required = true` read.
- **`ctx_read`.** It returns the full payload only, with no `level`. Its
  ledger rules are SPEC §6.3. `AgentState::record` replaces a logical's
  ledger entry on a re-read.
- **Recall index.** `contextdb-store/src/recall.rs` holds one
  `recall.sqlite` with FTS5 (unicode61) and `recall_map`.
  - Each logical has rows for main's head and for each live branch head that
    differs from main. Bodies are lossy UTF-8, cut at 64 KiB.
  - The P7 indexer thread (`candidates.rs`) updates it from
    `net_delta.head_moves` behind an epoch watermark.
  - It rebuilds fully on a branch op, an unreadable row, a watermark ahead
    of the log, or an old schema.
  - Recall ignores FTS errors and never reports lag. There is no
    `IndexState` and no `ctx reindex`.
- **Revisions are deduplicated by `(logical, content)`** (SPEC §7
  `UNIQUE(logical_id, content_hash)`, `stage.rs` `find_revision`). Recording
  identical text again, or reverting to it, reuses the existing RevisionId.
- **Summaries.** `RecordKind::Summary` exists, and its Cur/Pin premises are
  refused (`ForbiddenPremise`). There is no pending count; only the
  SessionStart briefing counts pending heads.
- **Token counting.** None exists. The briefing uses `budget_chars` with a
  `key.len() + 48` estimate.
- **Git.** Anchors, symbols, hooks and the CLI shell out to `git` through
  capped, deadline-bounded helpers (`contextdb-anchors/src/capture.rs`
  `git`). There is no export, mirror or `refs/contextdb`.

## 2. The index as a projection with health

### 2.1 Projections

| File | Contents | Updated by | If deleted |
|---|---|---|---|
| `recall.sqlite` | FTS5 rows (as today) plus `abstract` and `outline` per row (§4) | indexer thread | rebuilt at start or by `ctx reindex` |
| `vectors.sqlite` | `(content_hash, model) → f32[dims]`, keyed by content, not by logical | embedding thread (§7) | rebuilt; costs one embedding per live head |
| `mirror.sqlite` | per branch: the last commit export wrote, with its epoch | `ctx mirror export` | a cache only: the tip is verified by re-rendering (§8.4) |

None of these files is a source of truth (SPEC §3.5 #7).

### 2.2 `IndexState`

Each index projection reports one of:

```
IndexState = Ready { watermark }
           | Lagging { watermark, head_epoch }
           | Rebuilding { done, total }
           | Failed { error }          // the last update failed; retried
           | Disabled                  // vectors with no embedder configured
```

- `watermark` is the last epoch fully applied. The index is `Lagging` while
  `watermark < snapshot.epoch`.
- `ctx_recall` returns `index: {fts: IndexState, vectors: IndexState}`, and
  `ctx status` shows the same (T94).
- **Lag never bypasses certification** (SPEC P8). Status, branch visibility
  and head checks always come from the snapshot, never from the index
  (§3.2).
  - Lag can only **miss** a record: one newer than the watermark is not
    found by text.
  - It can never return a stale record as current.
  - Key matches never lag, because they scan the snapshot.
- **Bounded wait.** Before searching, a recall waits up to `index_wait_ms`
  (default 200 ms, at most 2 s) for the FTS watermark to reach the
  snapshot's epoch.
  - It wakes on the indexer's progress signal, the same mechanism as
    `ctx_changes`.
  - If it times out, it searches anyway and reports `Lagging`, so the caller
    knows the results may be incomplete.
  - G5 measures recall latency with and without a pending index update
    (§10).

### 2.3 `ctx reindex`

`ctx reindex <dir> [--fts] [--vectors] [--all]` (admin socket) marks the
chosen projection `Rebuilding`, clears it, and rebuilds it from the current
snapshot on the indexer thread. Recall keeps answering throughout:

- key matches are unaffected;
- text hits come from rows already rebuilt;
- the reported state is `Rebuilding{done, total}`.

The vector rebuild re-embeds only content hashes missing for the configured
model.

**T56 (restated).** For the same snapshot and query, recall before and
after `ctx reindex`, and after deleting `recall.sqlite`, returns the same
items with the same statuses. Ranks may differ only where bm25 ties break
differently, so the test compares sets and status per item.

## 3. Hybrid ranking with status first

### 3.1 Request

```
ctx_recall {
  query, mode: certified | advisory,
  scope?: string | { lane?, kinds?: [Kind], dir?: path prefix },
  limit? (default 10; values above 100 are clamped, with limit_clamped: true),
  budget_tokens? (§5),
  level?: header | abstract | outline | full,
  index_wait_ms?
}
```

- **Compatibility** (T95).
  - A plain `scope` string is `{lane}`, as today.
  - `limit` above 100 is clamped and reported, never refused. Today's clamp
    was 1,000, and certified recall issues a certificate for every item, so
    100 bounds that cost.
  - **Without `budget_tokens` and `level`, a response has today's shape:
    headers only, with no body text.** Existing callers therefore see no new
    bodies, and their ledger entries stay as today.
- **`dir`** matches if the path prefix covers either of:
  - any anchor of the revision (P5a/P5b CodeAnchor premises);
  - any entry of the logical's `dirs` metadata (`SetMetadata{dirs}`, SPEC
    §7 `dirs_json`, set by agents).

  The two sources are different, and `dir` is defined as their union.

### 3.2 Pipeline

1. **Eligibility before ranking.** A `(logical, rev)` on the caller's
   branch is eligible when all of these hold:
   - `view.head(logical) == rev` in the snapshot;
   - it passes the scope filter;
   - in P9, the caller is authorised;
   - in certified mode, its `summary_at` is CERTIFIED.

   An item that fails only the last condition becomes a `withheld` entry.
   Withheld entries are capped at `limit` and never compete for item slots.
2. **Rankers produce eligible candidates only.** No ranker cuts before the
   filter.
   - **Key ranker.** It scans every logical, as today, applies eligibility
     inside the scan, and orders exact, then prefix, then substring.
   - **bm25.** It runs today's AND query and, if that is short, P7's OR query
     (`any_of`). It pages through hits (pages of `limit × 4 + 16`), dropping
     ineligible ones, until it holds `limit` eligible candidates or the
     source is exhausted.
     *M7 amendment (G5, implementation review):* bm25 scores every row a
     query matches, so a term most records hold cost time linear in the
     store. Matches are ranked in windows of 2,048 rows, in bm25 order
     within a window. Windows follow the row id `(logical << 16) | branch`,
     highest first (latest created logical first), which depends only on
     the rows, so a rebuild gives the same windows and the same results. A
     query with at most 2,048 matches has one window: the plain bm25
     order. Paging walks every window, so `exhausted` and the short-page
     rule are unchanged; one recall reads every page in one read snapshot
     of the index, so a concurrent index write cannot shift rows past a
     page boundary. P7's `any_of` for lexical candidates uses the same
     windows.
   - **Vectors** (when enabled, §7) page through cosine order the same way.

   Each ranker reports `exhausted: bool`. For bm25, `exhausted` means
   both the AND and the OR query have run out. A page whose top hits are
   mostly ineligible can scan many hits; G5's lagging-load run reports
   that cost (hits scanned per item returned).
3. **Fusion.** Reciprocal rank fusion with k = 60 over the rankers that
   produced candidates: `score = Σ 1/(60 + rank_r)`.
   - Ties break by key rank, then logical id, so the order is deterministic.
   - An exact key match is ranked first overall.
4. **Cut** to `limit`, then pack (§5).

**T54 (restated).**
- In certified mode, every returned item is CERTIFIED at the response's
  epoch. A STALE, PENDING, CONFLICTED or Unknown head never appears in
  `items`.
- In advisory mode, every item carries exactly its `summary_at` label (the
  read-time label, including the observation dimension, SD14b).
- A page holds fewer than `limit` items only if every ranker is exhausted.
  This is the defect of filtering after the cut, checked by the r0.1
  review's counterexample: 60 PENDING `auth/*` keys and one CERTIFIED key.

### 3.3 Certificates and ledger

The recall certificate is unchanged: one certificate covers the returned
items (SPEC §6.4).

- Each returned item is recorded in the ledger at its packed level (§4.2).
- A certificate attests that a revision was current and certified when it
  was returned. It does **not** attest that the agent read all of it; a
  header-only item is a `required = true` read, as today.
- Withheld entries and omitted items (§5.2) were never shown with text. They
  get no handle, no certificate entry and no ledger entry.

## 4. Read levels

### 4.1 Levels as pure functions

`abstract(body)` and `outline(body)` are pure, deterministic functions of
the payload bytes, in a new module `contextdb-model::levels` (*implementation note, milestone 1: the store, which builds the projection, does not depend on `contextdb-proto`, and the model crate is dependency-free*). They have no
dependencies and no model, and are versioned (`LEVELS_VERSION = 1`).

- **header.** No body text, which is today's recall shape.
- **abstract.** The first paragraph (up to a blank line), after skipping a
  leading front-matter block and a leading heading line.
  - It is cut at 320 bytes on a UTF-8 boundary, with `…` appended if cut.
  - An empty result falls back to the first 320 bytes.
- **outline.** Every Markdown heading line, plus each section's first
  non-empty line (cut at 160 bytes), in order, up to 2 KiB.
  - Lines are marked `…` where something was cut.
  - A body with no headings gives the abstract followed by the line count.
- **full.** The body as stored.

Every rendered item carries its `level` and `cut: bool` in its header, even
when nothing was cut. A reader can then always tell that an abstract may
omit a qualifier further down the body.

The projection stores `abstract` and `outline` per row, so recall does not
recompute them. `ctx_read` recomputes them from the payload, so a stale
projection can never serve a wrong abstract for a revision. T87 checks that
the stored copy equals the recomputed one.

### 4.2 Ledger

`ctx_read` gains `level` (default `full`, today's behaviour).

- **Every expansion is recorded** (SPEC P8). Each read appends a read event
  with its level, so the session log's append-only events hold the full
  history of expansions.
- **The ledger keeps the highest level.** `LedgerEntry.level: Header |
  Abstract | Outline | Full` is the highest level read of that revision in
  the session; a re-read merges by max and never lowers it. This is session
  log format 4; format-3 segments read as `Full` for reads and `Header` for
  recall entries. *(Implementation note, milestone 3: a format-3 read event
  does not say whether a recall or a read produced it, so every format-3
  read event decodes as `Full`. A `Full` read keeps format 3's encoding,
  tag 15; only levels below `full` use the new tag 22, which a format-3
  segment refuses. The level is informational: no check depends on it.)*
- **Certification is per revision, not per level.** A certified abstract
  read of a CERTIFIED head is a certified read of that revision (SPEC §6.3).
  A level is a rendering of an immutable revision: if the revision is current
  and certified, so is any rendering of it. PreToolUse checks are unchanged.

## 5. Packing

### 5.1 Token estimate

There is no tokenizer dependency. `tokens(s) = ceil(bytes(s) / 4)` plus a
fixed per-item header cost.

- This is within about ±30 % for English prose and code under common BPE
  tokenizers. G6 reports the ratio against the token counts `claude -p`
  reports for the same packs.
- The estimate is named in the response (`estimate: "bytes/4 v1"`), so a
  caller can apply its own margin.

### 5.2 Algorithm

With `budget_tokens`, the response is packed greedily in fused order. The
`level` is the highest wanted, and defaults to `outline` when a budget is
given.

1. **Header first.** Every included item has a header: `handle, key, kind,
   rev, status label, level, cut` and, for a Summary, its pending count
   (§6). The header is never cut, and its cost is charged before any body
   text.
2. **Level fitting.** The item gets the highest level, up to the requested
   one, that fits the remaining budget: `full`, then `outline`, then
   `abstract`, then header only.
3. **Stop.** When a header no longer fits, packing stops. The remaining
   eligible items are counted in `omitted: n`, with no handles or text
   (§3.3). An item is never shown without its label.
4. **Withheld entries** (certified mode) are listed as headers outside the
   budget, with their label and no text, so the caller always sees what it
   cannot rely on.

**T55 (restated).**
- For every budget, every included item's label equals its label without a
  budget, and so does every withheld entry's.
- The included items are a prefix of the unbudgeted order.
- No PENDING, STALE or Unknown item is ever rendered with a CERTIFIED label.
- An unvalidated Summary is never labelled verified (§6).

## 6. Derived Summaries

A Summary is an agent-written record whose premises are Sem or Ref
(Cur/Pin premises are refused today). Its semantic state is the kernel's
Sem state (SD10): CERTIFIED only after a validation for its current vector,
and PENDING after any Sem premise moves. **P8 adds no kernel state and no
writer of ValidationRecords.**

- **Pending count.** A Summary item carries `premises: {moved, total}`.
  - It comes from the Summary's SD13 witness rule: the rule whose state
    gives the summary label. With several active rules, it is the first in
    the witness order.
  - `total` is that rule's number of Sem premises.
  - `moved` is the number whose current head differs from the vector of the
    rule's latest **Verified** ValidationRecord. With no Verified record, it
    is the vector at the rule's creation.
  - Packing shows `PENDING (summary; 2 of 7 premises changed)`, and the
    briefing lists Summaries with `moved > 0`.
  - *Implementation note, milestone 4 (SPEC v1.8.2).*
    - The kernel stores ValidationRecords by vector, not by time, so
      "latest Verified" is implemented as the Verified vector with the
      fewest differences from the current one. The two agree in the usual
      case, and the count is 0 exactly when the current vector is verified.
    - The witness rule is the first enabled rule concluding the revision
      with a Sem premise, in rule-id order (the canonical witness order).
    - A Summary must be recorded with `adopt_as_base = false`. A base
      Summary is supported by its base, and its rule does not gate its
      label.
- **Digest and early stop (SPEC P8 "digest 早停").**
  - The digest of a rule's premises is its semantic vector. Revisions are
    deduplicated by `(logical, content)` (§1), so identical content means
    the identical RevisionId, and the digest equals the vector.
  - The early stop is therefore what the kernel and P7 already do:
    - re-recording identical text, or reverting a premise to the validated
      content, returns the head to the same revision;
    - the vector equals the validated one, and SD10 gives the stored
      verdict;
    - P7 discovery enqueues no job for an unchanged `(branch, rule, curvec)`;
    - downstream status does not change.
  - P8 makes this observable and tested (T90) instead of adding a validator.
    r0.1's `digest` validator is withdrawn: it could only re-record a vector
    that already has a verdict, and recording `Verified` over a stored
    `Rejected` would have been wrong.
- **Visibility on read** ("PENDING 在读取时可见").
  - In advisory mode, a PENDING Summary is an item with its label and count.
  - In certified mode, it is a withheld header with its label and count, and
    no body.
  - `ctx_read` in either mode returns the label and count. Certified mode
    returns no payload, as today.

## 7. Embeddings

### 7.1 Adapter

`<data>/embeddings.toml`:

```toml
[embedder]
command = ["/path/to/embed", "--model", "x"]   # reads JSON lines, writes JSON lines
model = "x"                                     # the cache key
dims = 1024
max_batch = 32
timeout_secs = 30
```

- **Protocol.** Each input line is `{"id": n, "text": "..."}`, where `text`
  is the abstract plus the outline plus the body, cut at 8 KiB. Each output
  line is `{"id": n, "vector": [f32; dims]}`. A wrong dimension, NaN, a
  missing id or output over 4 MiB fails the batch (`Malformed`), and the
  batch is retried with back-off, as P7 pairs are.
- **Process** (P7 m6 fixed for both validators and the embedder).
  - The child runs in its own process group with a deadline.
  - On kill, the parent closes its pipe ends and does not wait for reader
    threads that a stray grandchild keeps open.
  - The embedder gets a minimal environment (`HOME`, `PATH`, `LANG`) and an
    empty 0700 working directory, because it receives record bodies.
- **Off by default.** Without the file, the vector projection is `Disabled`
  and recall uses FTS and keys only.
- **Tests** use a deterministic mock embedder: a hashed bag of words,
  normalised.

### 7.2 Vector search

- **Filling.** The embedding thread fills `vectors.sqlite` for every live
  head whose content hash is missing for the model. It reads the snapshot
  and never blocks the writer.
- **Query embedding.** The vector ranker embeds the query with one call,
  cached by query text for 10 minutes.
- **Search.** It pages through the cosine order of the vectors of the heads
  visible on the branch, from the in-memory cache (E2). Like every ranker,
  it yields only eligible candidates (§3.2).
- **Branches.** Vectors are keyed by content, so branches share them, but
  the scan covers only heads visible on the caller's branch (SD19).
- *Implementation note, milestone 5b:* the embedding thread is separate from
  the recall indexer, so a hung embedder delays only vectors. Each call runs
  on a helper thread, so shutdown never waits for it. A query embedding
  waits at most 2 s, and a failed one is not retried for 30 s. `ctx reindex
  --vectors` prunes other models' vectors and fills in what is missing. A
  mock embedder can be configured with `type = "mock"` for trials; it never
  counts for G6.

### 7.3 Embedding contradiction candidates, and the P7 fixes

- **Candidates.** When a new head of kind Decision, Plan or Fact arrives on
  branch β (the same trigger as P7 §5), its nearest neighbours with cosine
  ≥ `min_cosine` go to the existing `candidates.sqlite`.
  - `min_cosine` defaults to 0.82 and is calibrated in G6.
  - They get `source = embedding`, and the same per-head cap, states,
    judging and `ctx_conflicts open{from: candidate}` flow as P7's lexical
    candidates. A pair found by both sources is one row with `source = both`.
  - Candidates are per branch and never pair a revision invisible on the
    branch (T89).
- **m7.** `ctx_conflicts open` for an unordered pair `{a, b}` that already
  has an open two-sided Conflict on the branch returns that Conflict with
  `already_open: true`, whatever the reason or evidence.
  - The check is a daemon check, not a kernel rule (SD20 and the oracle
    are unchanged). It runs on the single writer (actor) thread, in the
    same turn that stages the open batch, against that turn's state, so no
    other commit interleaves. Two concurrent opens therefore give one
    Conflict.
  - A one-sided merge Conflict on `a` does not count as the pair's Conflict.
  - *Implementation note, milestone 5a:* the check is a daemon lock held across `ctx_conflicts open`, re-checked on a fresh snapshot taken under the lock, not a check on the actor thread. Every open the daemon accepts is serialised, so concurrent opens still give one Conflict (T93). A raw admin batch with its own `OpenArtifact` ops is not checked.
- **m4.**
  - A pair review's batch carries `ExpectHeads` for both sides, so a side
    that moved during the call gives a superseded pair, never a review on an
    old revision.
  - A skipped pair is released inside its claim transaction, so
    `judge_expire` never counts an attempt for it.

## 8. Git mirror

### 8.1 What is exported

`ctx mirror export <dir> [--branch B] [--project <repo>]` (admin socket).
SPEC §6.6's name `export-mirror` becomes this subcommand (§11). `ctxd` can
also export after `mirror.every_epochs` epochs that move heads on the
branch: the SPEC's "configurable interval", off by default.

**Ref name.** The ref is `refs/contextdb/b<branch-id>`, with the branch's
name in the README and in a trailer.
- Branch ids are single path segments, so `feat` and `feat/x` can never
  collide as git refs.
- Backups are `refs/contextdb-backup/b<id>-<unix-time>`.

**Tree.** Export renders the branch's live heads as
`<lane>/<kind>/<key-path>.md`. One encoder applies to every segment: the
lane, the kind and each `/`-separated segment of the key.

- Every byte outside `[A-Za-z0-9_-]` is percent-encoded with uppercase hex.
  That includes every `.` and `%`, which become `%2E` and `%25`.
- An empty segment is encoded as a bare `%`. A bare `%` cannot otherwise
  occur, because `%` itself is always `%25`.
- The leaf alone gets `.md`. No encoded segment contains a `.`, so a
  directory can never be named like a file:
  - key `x` is the file `x.md`;
  - key `x.md/y` is the directory `x%2Emd` holding the file `y.md`.
- So the encoding is injective, and no segment is ever `.`, `..` or `.git`
  in any case. `a//b` gives `a/%/b.md`, while `a/./b` gives `a/%2E/b.md`.
- Paths that are equal after case-folding are renamed to `<leaf>~<logical
  id>.md`, every one of them (case-insensitive file systems).
- Encoding can triple a key's length, so a segment may exceed a file
  system's 255-byte name limit on checkout. The git tree stays valid, and
  only checking it out fails; `ctx mirror export` warns about such paths.

**Files.** Each file is a front-matter block followed by the full body.

- The front matter holds `logical, rev, kind, label, head_epoch, premises:
  [key@rev (edge kind)]`.
- `head_epoch` is the epoch that made this revision the head, which is
  stable.
- `label` is the snapshot summary **without** the read-time observation
  dimension (SD14b), so it is a function of the snapshot.
- An unavailable payload renders as `(payload unavailable)` (T97).

**`README.md`** at the root holds the branch name and id, the tool version,
and the rule that **this ref is generated and edits are not read back**. It
holds no epoch.

**Determinism.** The tree is a function of the branch's committed state
only. A commit on another branch changes no byte of it.

*Implementation notes, milestone 6:*
- The front matter carries `rev_epoch`, the epoch that created the revision,
  instead of `head_epoch`. The kernel keeps no per-branch head history, and a
  scan that tracked forks and merges for every export would be both more
  costly and more fragile. Like `head_epoch`, `rev_epoch` is stable and a
  function of the log. It differs only for a revision that became the head
  later than it was created (a merge, or a revert to an older revision). The
  front matter also carries `key`, so that `proposals` can name the record.
- The README names the mirror format (`mirror format 1`), not the tool
  version. A tool upgrade would otherwise change every tree and turn every
  older tip into a false `MirrorModified` on a cache miss.
- With the kernel's key rule (lower-case, unique per branch), two paths can
  differ only in case if their lanes do, and then their keys differ too. So
  case-folded collisions cannot occur, and the renaming rule is kept only as
  a safeguard.

### 8.2 Commits

The tree is written in the project repository with git plumbing:
- the blobs go through one `git fast-import` stream, from memory. It uses
  `blob` commands with marks, `--export-marks` to read their ids, and no
  `commit` command, so it touches no ref, and no body is written to a
  temporary file;
- `mktree`;
- `commit-tree -p <previous mirror commit>`.

Every git process runs through the existing capped, deadline-bounded helper,
with `-c core.hooksPath=/dev/null`. `update-ref` would otherwise run the
`reference-transaction` hook.

The commit message:

```
contextdb mirror: <branch name> epochs <a>..<b>

Code-Commit: <observed git HEAD, or "none">
Ctx-Branch: <name> (b<id>)
Ctx-Epoch-Range: <a>..<b>
Ctx-Log-Checksum: <op_log frame checksum at epoch b>
Ctx-Mirror-Version: 1
```

- `a` is the previous mirror commit's `b` plus one, and `b` is the snapshot
  epoch. One mirror commit therefore covers an epoch range, as the SPEC
  requires. Ranges are contiguous (T91).
- `Code-Commit` names the code state the branch's anchors were last
  observed at, not the work tree's HEAD at export time.
  - It is decoded from the position (`CTXPOS01/02`) of the branch's latest
    `ObserverWatermark` for the scope of the project repository being
    exported into: the repository whose `refs/contextdb/*` the mirror
    writes.
  - With no watermark for that scope, it is `none`.
  - *Implementation note, milestone 6:* a position is a BLAKE3 digest of
    the observation (`capture::position`), not an encoding that holds the
    commit, so the commit cannot be decoded from it. `Code-Commit` is
    therefore always `none` for now. Recording the observed HEAD would need a
    new field in the observation, which is left to a later milestone.
- If the tree equals the previous commit's tree, no commit is made, and only
  `mirror.sqlite`'s epoch advances. Export is idempotent.

### 8.3 Moving the ref

`git update-ref refs/contextdb/b<id> <new> <old>` takes the expected old
value, and creation uses the zero OID, so every move, including creation, is
a compare-and-swap (SPEC §3.5 #4). If another process moved the ref in
between, the update fails with `MirrorRaced`, and nothing is retried
silently (T92).

### 8.4 Hand-edit detection (T57)

The tip is verified from git itself, so `mirror.sqlite` is only a cache.
Export reads the ref's tip `T`:

- **Fast path.** If `T`'s commit id equals the id cached in `mirror.sqlite`,
  the tip is accepted. A commit id binds the tree and the parents, and only
  the data directory's owner writes the cache. This is the normal case, and
  it costs nothing.
- **Cache miss: validation by re-rendering.** This happens after a crash
  between `update-ref` and the cache write (T96), a deleted `mirror.sqlite`,
  or a tip that is not the cached one. Export validates `T`:
  - `T` has the mirror trailers and range `a..b`;
  - `Ctx-Log-Checksum` equals the op_log frame checksum at `b`;
  - `T`'s tree equals the render of the branch at epoch `b`.

  The store keeps no snapshots of past epochs, so rendering at `b` replays
  the kernel to `b` (`replay_kernel_to`). This costs O(epochs), which is why
  it runs only on a cache miss. G5 gives the cache-miss path its own budget,
  measured at a stated epoch count.
- **A valid tip is accepted** and cached, whether or not the cache had it.
- **Anything else gives `MirrorModified{ref, found}`, and export changes
  nothing.** That includes an edited, amended or rebased tip, a hand-made
  commit, and a tree that differs from the render.
- **A different log gives `MirrorStale{LogRewritten}`, not `MirrorModified`.**
  This is the case where `Ctx-Log-Checksum` does not match, or `b` is beyond
  the current log.
  - `doctor --truncate-before e` drops the log from `e` on, and the log then
    grows with different frames at the same epoch numbers. A tip with `b ≥ e`
    therefore names a history that no longer exists. That is not a hand edit.
  - `--force` then moves the ref after a backup.
  - **Constraint for P9 and P10:** a shredded (P9) or garbage-collected (P10)
    payload re-renders as `(payload unavailable)`. Re-validating a tip that
    exported it must be reported as `MirrorStale{PayloadGone}`, not as a hand
    edit, and those phases add that case.
- **A deleted ref whose tip is cached** is recreated with the cached commit
  as parent, if that commit still exists in the object store. Otherwise the
  ref is created fresh. Both are reported.
- **`--adopt`** accepts an unknown tip as the parent only if it validates
  (trailers, checksum, re-rendered tree), exactly as on a cache miss. It
  exists to say that an unknown tip is expected. It never skips detection.

**`ctx mirror proposals <dir> [--branch B]`** diffs the found tree against
the render of the last valid tip.

- For each changed file it emits a proposal `{key, base_rev, new_body,
  kind}`, plus `added` and `deleted` lists.
- `base_rev` comes from the valid render, never from the edited front
  matter.
- Nothing is applied. An agent or the owner may turn a proposal into
  `ctx_supersede` with `expected_head = base_rev`, and the CAS refuses it if
  the record moved since.
- **Proposal bodies are untrusted text** (§10 threat model): they are data
  for an agent to read, not instructions.
- Edits made in a remote copy of the ref are invisible until fetched back
  into this ref.

`--force` moves the ref anyway, after saving the found tip as a backup ref.
Backups are listed by `ctx mirror status` and kept until `ctx mirror prune
--older-than`.

**No write-back** (SPEC P8). No code path reads mirror content into the
op_log. Proposals go through the agent's own supersede call, under its own
session, checks and certificates.

*Implementation notes, milestone 6:*
- Trees are written with one `git mktree --batch` call per directory depth,
  after the single `fast-import` blob stream. Commits use `commit-tree
  --no-gpg-sign` with a fixed `ContextDB <contextdb@localhost>` identity, so
  a user's signing configuration never prompts.
- The parent of a forced export is the last commit, at most 64 first-parent
  steps below the found tip, that validates (none after `MirrorStale`).
  `proposals` diffs against that same commit.
- `mirror.every_epochs`, the automatic export, is not implemented. Export
  runs only on `ctx mirror export`.

### 8.5 Privacy

Mirror content is every live head's full body.

- ContextDB never pushes. A plain `git push` does not send
  `refs/contextdb/*`, but `git push --mirror`, `--all` with custom refspecs,
  and repository backup or mirroring tools do. The README, `ctx mirror
  export`'s output and the SPEC threat model say so.
- A shredded payload (P9) must not be exported. Until P9, an unavailable
  payload is rendered as `(payload unavailable)`.
- Export writes no body to a temporary file (§8.2). Bodies leave the data
  directory only as git objects in the project repository.

## 9. Protocol and compatibility

- **IPC protocol 5 → 6.** A protocol-5 client is refused with the usual
  version error; binaries upgrade together.
  - **`Recall`** gains:
    - `scope` as a string or a struct;
    - `budget_tokens`, `level` and `index_wait_ms`.

    Its response gains:
    - `index`, `omitted` and `limit_clamped`;
    - per item: `level`, `cut`, the rendered text, `premises` for
      Summaries, and `anchors`.
  - **`Read`** gains `level`.
  - **New admin requests:** `Reindex` and `Mirror{Export, Proposals, Status,
    Prune}`.
- **MCP.** The `ctx_recall` and `ctx_read` schemas gain the new fields, and
  all of them are optional.
  - With none of them, calls behave as today. The only differences:
    - `limit` above 100 is clamped and reported;
    - withheld entries no longer use up `limit`, so a certified page can
      hold more certified items than before, never fewer.
  - No new MCP tool is added, so hook claims are unchanged.
- **Session log format 4** (`LedgerEntry.level`, read-event levels). It
  rolls over to a new segment, and format-3 segments still read. A downgrade
  needs the session log removed (§0).
- **op_log** is unchanged (format 4).

## 10. Verification plan

| Gate | Items |
|---|---|
| G1 | fmt, clippy `-D warnings`, tests, `cargo deny` (no new native dependency; E2) |
| G2 | See the T-items listed below. |
| G3 | **PROP14 (retrieval)**, 10,000 seeds. Random stores (`generate_p7` histories, several branches, Summaries), random queries, modes, budgets and levels. **Oracle:** the eligible set computed from the oracle's summaries over every head. **Checks:** every certified item is CERTIFIED at the epoch; every advisory label equals the oracle's summary; a page is short only when the eligible set is exhausted; packing keeps labels and order; the result is equal, as a set with statuses, after a rebuild. **Metamorphic:** adding unrelated records never removes a certified exact-key match; budgets never break the prefix property; switching `scope` between its string and struct forms changes nothing. |
| G4 | Kill points: during `ctx reindex`, during an embedding batch, between `commit-tree` and `update-ref`, and **between `update-ref` and the cache write** (T96). Each ends with a consistent projection or ref: never a half-moved ref, and a leftover commit object is harmless. Disk full: in `recall.sqlite` and `vectors.sqlite`, recall keeps working with the index state `Failed` and retried. A hung or crashing embedder, a grandchild holding pipes, and git lock contention (`index.lock`, `packed-refs.lock`, a concurrent export). |
| G5 | Each measured idle and with a commit stream that keeps the index lagging. Recall p99 (key + FTS): < 50 ms at 10⁴ records and < 150 ms at 10⁵. Hybrid with 1,024-dim vectors: < 100 ms at 10⁴ from the cache; 10⁵ and the uncached path reported. Packing overhead < 5 ms. Indexer lag p99 after a commit < 500 ms, against the 200 ms bounded wait (the wait's hit rate is reported). Mirror export of 10⁴ records < 10 s; an unchanged re-export (cache hit) < 1 s; a cache-miss validation at 10⁴ records and 10⁵ epochs < 60 s (replay included), reported with the epoch count. Hits scanned per returned item under the lagging load, reported. All budgets are initial; ADR calibration applies. |
| G6 | **Report-only with a floor.** At least 100 queries over the three dogfood stores, written and labelled by two independent agents (E3). The report covers recall@5, recall@10 and MRR for: key only (the P3 baseline), key + FTS, and hybrid (when a real embedder is configured; the mock never counts). **Pass condition:** key + FTS is not worse than key only on recall@10. **Token cost:** per query, the tokens of a packed recall at budgets 1k, 2k and 4k, against the full bodies of the same relevant records and against the P4 dogfood's recorded reads where they exist. The `bytes/4` estimate is compared with the counts `claude -p` reports. **`min_cosine` calibration** when a real embedder is configured. Raw queries and labels contain project text and stay out of the repository, as in P7. |
| G7 | Earlier phases' G2/G3/G4 (`gate-regression.sh`, `g3-p6.sh`); the P7 suites; P7 stress. |
| G8 | README (retrieval, packing, mirror), CHANGELOG, SPEC amendments, an independent implementation review. |

G2 items:

- **T54.** Certified recall returns only CERTIFIED heads; advisory labels
  equal `summary_at`; a page is short only when the rankers are exhausted
  (the r0.1 counterexample is a fixture).
- **T55.** Packing never changes or drops a label; included items are a
  prefix; withheld entries are listed; omitted items have no handle.
- **T56.** Results and statuses are equal before and after `ctx reindex`
  and after deleting `recall.sqlite`.
- **T57.** An edited, amended, rebased or hand-made tip, or a rewritten
  tree, gives `MirrorModified`. `proposals` lists exactly the edited files
  with `base_rev` from the valid render. `--force` keeps a backup ref.
- **T85.** RRF is deterministic; an exact key is first; scope filters
  (`kinds`, `dir`).
- **T86.** Index lag never returns a non-current revision; a lagging recall
  reports `Lagging`; the bounded wait wakes on progress.
- **T87.** `abstract`/`outline` are pure (property over random UTF-8 and
  Markdown); the stored copy equals recomputation; `level` and `cut` are in
  every header.
- **T88.** Each expansion appends a read event with its level. The ledger
  keeps the highest level, and a re-read never lowers it. Format-3 segments
  read as `Full`/`Header`. Certificates are per revision.
- **T89.** Embedding candidates are per branch and never pair an invisible
  revision. The mock embedder's failures (wrong dims, NaN, hang, oversize
  output, a grandchild holding pipes) never block the indexer or the writer.
  The embedder gets the minimal environment.
- **T90.** Summary pending count against the last Verified vector (a
  Rejected record does not reset it). Re-recording identical text or
  reverting a premise returns to the same revision: no job is enqueued, the
  label returns to the stored verdict, and no ValidationRecord is written.
  - **The main early-cutoff case:** after its premises change, a Summary is
    re-recorded with byte-identical text. Its head stays the same revision,
    so its downstream Sem dependents' vectors and labels do not change, and
    the Summary stays PENDING until it is validated.
  - The test also pins `ctx_supersede`'s reply when the content equals the
    current head (the existing revision is returned and no head moves). The
    exact reply, including whether an epoch is spent, is recorded in SPEC
    §6.3.
- **T91.** Mirror export is deterministic and idempotent: the same branch
  state gives the same tree, and a commit on another branch gives no new
  commit. Trailers are present; ranges are contiguous.
- **T92.** The mirror ref moves only by CAS, including creation from the
  zero OID: of two concurrent exports, one wins and the other gets
  `MirrorRaced`.
- **T93.** `ctx_conflicts open` on an already-open pair returns it, even
  under two concurrent opens (m7). A one-sided Conflict does not block a
  pair. A pair review on a moved side is superseded, and a skipped pair never
  loses an attempt (m4).
- **T94.** `IndexState` in `ctx_recall` and `ctx status`; `ctx reindex`
  reports `Rebuilding`.
- **T95.** Compatibility: a string `scope` works; the limit clamp is
  reported; with no `budget_tokens` or `level`, the response is header only
  and the ledger levels are `Header`.
- **T96.** A crash after `update-ref` and before the cache write gives no
  false `MirrorModified`. Deleting `mirror.sqlite` gives none either. A
  deleted ref is recreated and reported. `--adopt` still rejects a tip
  without valid trailers.
- **T97.** Path and ref encoding:
  - `feat` and `feat/x` both export;
  - keys `x` and `x.md/y`, `a//b` and `a/./b`, and keys with `.`, `..`,
    `.git` and case-only differences give valid, distinct paths (`git fsck`
    clean);
  - an unavailable payload renders as a placeholder;
  - a property over random keys checks that the encoding is injective;
  - a truncated log gives `MirrorStale{LogRewritten}`, not `MirrorModified`.

**Owner check before G6.** An embedder is needed to measure the hybrid path
on real data, and E1 lets the owner configure one. If none is configured at
G6, the hybrid path is reported as "not evaluated on real data" and gated by
T85, T89 and PROP14 with the mock only. The owner decides before G6 whether
that is acceptable.

## 11. SPEC amendments (before implementation)

- **§5 P8:**
  - status filtered before any ranking cut; withheld entries never use
    `limit`; `exhausted`;
  - `IndexState` and the bounded wait;
  - levels as pure functions, `level`/`cut` in headers, certification per
    revision, every expansion recorded and the ledger at the highest level;
  - the `bytes/4` estimate;
  - the Summary pending count, and the digest early stop as the dedup of
    revisions (no new validator);
  - E1–E4;
  - the mirror: ref names by branch id, CAS including creation, tip
    validation by re-rendering, proposals, backup refs and their pruning,
    `Code-Commit` from the anchor observation;
  - session log format 4 and its downgrade note.
- **§6.3:** the `ctx_recall` and `ctx_read` rows (new fields, the limit
  clamp, header-only default), and the `ctx_supersede` row's reply when the
  content equals the current head (T90).
- **§6.6:** `export-mirror` becomes `ctx mirror export|proposals|status|prune`;
  add `ctx reindex`.
- **§7:** the new projections (`vectors.sqlite`, `mirror.sqlite`) and the
  abstract/outline columns.
- **§8.1:** T54–T57 restated; T85–T97.
- **§8.2:** PROP14.
- **§10 threat model:**
  - an embedder receives record bodies (owner-configured; minimal
    environment);
  - the mirror ref contains full bodies and is pushed by `--mirror` or
    backup tools;
  - mirror proposals are untrusted text.
- **§3.5:** no change. The design adds no forbidden mechanism:
  - the ref moves by CAS (#4);
  - projections are rebuildable (#7);
  - nothing is written back;
  - no new writer of verdicts (#14).

## 12. Milestones

1. **Projection health.** `IndexState`, the bounded wait, `ctx reindex`, and
   abstract/outline columns (T56, T86, T87, T94).
2. **Ranking.** Eligibility first, paging rankers, structured scope, RRF,
   protocol 6 and compatibility (T54, T85, T95).
3. **Levels and packing.** Read levels with session log format 4, and
   packing (T55, T88).
4. **Summaries.** The pending count and the early-stop tests (T90).
5. **Embeddings.** The adapter and the m6 process fixes, vector search with
   the cache, embedding candidates, and the m4/m7 fixes (T89, T93).
6. **Git mirror.** Export, encoding, CAS, tip validation, proposals and
   backups (T57, T91, T92, T96, T97).
7. **Verification.** G3–G8, the G6 evaluation and the independent review.

## 13. Review history

| Round | Findings | Result |
|---|---|---|
| r0.1 review | 0 blockers, 7 majors (M1–M7), 12 minors | not acceptable as r0.1; acceptable after text fixes |
| r0.2 (author) | M1 eligibility inside rankers, paging, `exhausted`; M2 `digest` validator withdrawn, early stop = revision dedup (T90); M3 render without global epochs or the read-time dimension; M4 tip validated by re-rendering, cache-only `mirror.sqlite`, crash after `update-ref` (T96), deleted ref, creation CAS; M5 refs by branch id, reserved and case-folded paths (T97); M6 clamp not refuse, header-only default (T95); M7 every expansion as an event, max-merge, omitted and withheld items unrecorded, the certificate's meaning. Minors m1–m12 | |
| r0.2 closure check | M1, M2, M3, M6, M7 closed; M4, M5 partly closed; new N1 (major: replay on every export), N2, N3 (major: path encoding not injective), N4, N5; m2/m4/m7 partly; T90 gap | acceptable after text fixes |
| r0.3 (author) | N1 fast path on the cached commit id, replay only on a miss, its own G5 budget; N2 `Ctx-Log-Checksum`, `MirrorStale{LogRewritten}`, P9/P10 constraint; N3 one injective encoder with tests; N4 `fast-import` from memory; N5 `exhausted` covers AND and OR; m2 daemon check on the actor thread; m4 scope of the target repository; m7 `dir` as a union; T90 Summary re-record and supersede reply | |
| r0.3 check | every item closed; the encoder is injective and gives valid tree entries; 3 minor text points (the case-fold suffix and long names, the `ctx_supersede` row, this table), fixed in place | **G0 acceptable** |
| M7 implementation review (2026-11-06) | 0 blockers; M1 bm25 pages on separate snapshots, M2 windows depended on indexing history (both fixed: one read snapshot, row ids by logical and branch); m3–m5 fixed; m6 G6 caveats disclosed; m7 window bounds by `OFFSET` open (gate P8 §3) | **Gate P8 passed** |
