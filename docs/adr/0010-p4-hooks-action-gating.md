# ADR 0010 — P4 hooks, outbox and action gating

Status: **Accepted (revision 3.2)**, 2026-10-21, for P4 G0; **revision 3.3** records implementation notes from milestone 1 (§4: requests changed in place under protocol 2, final tag tables; §2.3: a claim serves every request of its tool call). The reviewer's closure check of revision 3.1 found C1 and M1–M3 closed, no new blocker or major, and G0 acceptable; revision 3.2 applies its three text fixes (§17).

Review history (non-implementing reviewer):

| Revision | Findings | Mapping |
|---|---|---|
| 1 | 3 blockers, 14 majors, 18 minors | §14 |
| 2 (closure) | 2 blockers (N1, N2), 6 majors (N3–N8), 7 minors | §15 |
| 3 (closure) | N1–N8 closed; 1 blocker (C1), 3 majors (M1–M3), 7 minors | §17 |

Revision 3 changes:
- MCP sessions bind through **call claims** made by the PreToolUse hook, instead of by process ancestry (§2);
- each subagent gets its own ledger view inside the shared session (§7);
- outbox sequence recovery and gap handling are made explicit (§5).

Scope:
- SPEC: §5 P4; §6.3 `ctx_check` (action, `context_update`); §6.4 TOCTOU; §6.5 hook contract;
- §10 risks: missing dependencies, Bash side effects, external TOCTOU;
- tests T38–T41b and T66; invariant I6.

Builds on ADR 0008.

## 0. Owner decisions (2026-10-21)

1. **Dogfood gate.** G6 needs ≥ 2 weeks of real use with ≥ 2 parallel agents, which a session cannot produce.
   - P4 is implemented and G0–G5, G7 and G8 are closed.
   - Hooks, telemetry and `ctx gate-report P4` then go to the owner for the dogfood run.
   - `docs/gates/P4.md` keeps G6 at **Pending**.
   - P5a G0 and implementation may start meanwhile.
   - P4 is marked Passed only after the dogfood report.
2. **Harnesses.** Both are supported, with their hook protocols pinned in T41b:
   - Claude Code **2.1.306**;
   - Codex CLI **0.163.0** (tag `rust-v0.163.0`, commit `83086188`).

## 1. Processes, crates and dependencies

**`contextdb-proto` (new).**
- `proto`, `lease`, `Client`, the gate predicate (§6.1) and `GateConfig` move out of `contextdb-daemon` into this crate.
- It depends on `contextdb-model` and `contextdb-core`. Responses carry `EpochDiff`, `Impact` and `Proof`, whose `Wire` impls live in core, and core is the restricted crate with no store or rusqlite.
- It has no external dependencies: the client uses std `UnixStream`.
- The daemon re-exports it, so P3 paths and fuzz targets are unchanged.

**`contextdb-hooks`.**
- A library of pure parts: harness adapters (§3), renderers, the patch-path parser (§3.3), and outbox records and state (§5).
- The binary **`ctx-hook <claude|codex> <event>`**:
  - one short-lived process per hook event;
  - reads one JSON object from stdin and writes at most one JSON object to stdout;
  - leaves no child processes.
- Dependencies, all already reviewed:
  - `contextdb-model`, `contextdb-proto`;
  - `serde_json`, `blake3`;
  - `nix` with the `fs` feature, for `flock` and plain `fsync(2)`.
- No FFI is added, the workspace keeps `unsafe_code = "forbid"`, and no process inspection is done.

**Dependency policy.**
- `config/dependency-policy.toml`:
  - `[proto]` allows no external crates;
  - `[hooks]` gets `reviewed_external = serde, serde_json, blake3, nix`;
  - `[cli]` gets `reviewed_external = toml` plus toml's closure.
- `scripts/check_dependencies.py` checks `proto`, `hooks` and `cli`, with negative tests. One of them asserts that `hooks` does not reach `contextdb-store` or `rusqlite`.

**`contextdb-cli`.**
- New commands: `ctx check`, `ctx policy`, `ctx gating apply`, `ctx hooks install | uninstall | flush`, `ctx gate-report P4` and `ctx dogfood review`.
- `toml` is used only here. Hooks read compiled `Wire` files (§6.2).

**`contextdb-daemon`.** New requests (§4), call claims (§2), per-agent ledger views (§7), stream marks (§5.3), and the outbox sweeper (§5.2).

## 2. Session binding: hook sessions and MCP calls

**Problem.**
- The PreToolUse hook and `ctx-mcp` are different processes.
- PreToolUse must check the ledger that the MCP reads went into.
- MCP gives the server no harness session or agent id.
- Process topology is no help:
  - `/clear` replaces the harness session without restarting MCP;
  - Codex's default `daemon_auto_start` hosts many sessions and their MCP servers under one shared `codex app-server` process.

**Decision.** Bind each MCP call, not each process, using the **tool-call id** that both pinned harnesses send to the MCP server in the `tools/call` request's `_meta` (verified in source for Codex 0.163.0 and in the binary for Claude Code 2.1.306; pinned by T41b).

1. **Hook sessions.** Every hook event carries `session_id`, so the hook's ctxd session is keyed by `(harness, harness_session)`.
   - `SessionAttach{harness, harness_session, repo, head_sha}` returns the live session with that key, or starts one with a durable `SessionKey` event.
   - A key whose session has ended starts a **new** session with the same key, for example resume after SessionEnd.
2. **Codex: direct binding.** Codex sets `_meta.sessionId` (the hook's `session_id`), `_meta.threadId` (the child's `agent_id`, or the session id for the root) and `_meta.callId` (the hook's `tool_use_id`) on every MCP call (`core/src/mcp_tool_call.rs`). `ctx-mcp` sends `binding: Key{harness: codex, harness_session: sessionId, agent_id}`, where `agent_id = threadId` if `threadId ≠ sessionId` and `None` (root) otherwise; for a root thread Codex sets `session_id = thread_id`. The daemon treats an id as a child only if a hook has reported that `agent_id` for the session, and otherwise as the root, so an unusual resumed rollout whose stored session id differs from its thread id stays the root. No claim and no extra hook are needed.
3. **Claude Code: id-keyed claims.** Claude sets `_meta["claudecode/toolUseId"]` (the hook's `tool_use_id`) on MCP calls.
   - The installer adds a PreToolUse entry matching only the session-scoped ContextDB tools: `^mcp__contextdb__(ctx_recall|ctx_read|ctx_record|ctx_supersede|ctx_check)$`, using the registered server name or `--mcp-name`. The name is normalised as the harness does for tool names (characters outside `[A-Za-z0-9_-]` become `_`) and regex-escaped; a name that normalises to empty is rejected.
   - For such a call the hook sends `Claim{session, agent_id, call: tool_use_id}` and **waits for the reply** before exiting, so the claim exists before the tool runs (PreToolUse precedes the call in both harnesses). It prints nothing.
   - `ctx-mcp` sends `binding: Call(tool_use_id)`. The daemon resolves the claim for that exact id and runs the request in its session, as its `agent_id`. Every request of the same tool call (e.g. `ctx_record`'s advisory head lookup, then the record) resolves through the same claim; ids are unique per call, so reuse cannot misattribute.
   - Claims live until the session ends (at most 65,536 kept per daemon, oldest dropped first). There is no short TTL, so a long permission prompt cannot orphan a claim; a claim whose call never runs (denied or failed) is inert, because only its own id resolves it.
   - The claim's `agent_id` comes from the hook input and is authoritative. Claude's undocumented `_meta["claudecode/agentId"]`, when present, is only cross-checked: a mismatch is logged and counted, never used to override.
4. **No id.** If `_meta` carries neither form (a harness upgrade dropped it, or another client), `ctx-mcp` uses its own unkeyed session as in P3 and the response says `binding: Unclaimed`. There is **no** argument-digest fallback: matching by content could put one session's reads into another's ledger, and a check could then pass on reads that session never made (I6).
5. **Report.** `ctx gate-report` counts `Unclaimed` calls, so a broken binding shows up in the dogfood numbers.
6. **Trust.** Ids and claims are addresses, never trust claims. The boundary stays cooperative (ADR 0008 §0.5).

This works in every process topology, follows `/clear` immediately, attributes MCP reads to subagents (§7), and needs no `ps`, environment variable or FFI.

## 3. Harness adapters

### 3.1 Normalised event

```
HookEvent { harness, harness_version: Option<String>, kind, session: String,
            agent_id: Option<String>, turn_id: Option<String>, tool_use_id: Option<String>,
            cwd, transcript: Option<String> }
kind   = SessionStart{source} | UserPrompt | PreTool{action} | PostTool{action}
       | McpCall{tool, tool_use_id} | SubagentStart{agent_id} | SubagentStop{agent_id}
       | Stop | PreCompact | SessionEnd{reason}
Action { tool: ToolClass, paths: Vec<RepoPath>, command: Option<String> }
ToolClass = Edit | Write | Notebook | Patch | Shell | Other(String)
```

**Parsing is lenient.**
- Unknown fields are ignored.
- Only the fields an event needs are required.
- A missing required field gives `Unparsed`:
  - it follows `on_unavailable` (§6.3);
  - it always emits a `systemMessage` that names the event.

**Rendering is strict.** Output contains only the keys the pinned schema accepts.

**Harness version.**
- Claude: the basename of `CLAUDE_CODE_EXECPATH`.
- Codex: from `codex --version`, cached at install time.
- The version is recorded on `Gate` facts. On an unpinned version the hook behaves as on the pinned one, and the report flags it.

### 3.2 Field mapping (pinned versions)

| Item | Claude Code 2.1.306 | Codex 0.163.0 |
|---|---|---|
| session | `session_id` | `session_id` (root thread id) |
| Edit/Write | `tool_name ∈ {Edit, Write, MultiEdit}`, `tool_input.file_path` (absolute) | — |
| Notebook | `NotebookEdit`, `tool_input.notebook_path` | — |
| Patch | — | `apply_patch`, `tool_input.command` holds the patch text (§3.3) |
| Shell | `Bash`, `PowerShell`, `Monitor`: `tool_input.command` | `Bash`: `tool_input.command`; patch headers inside the command are also parsed (§3.3) |
| MCP | `mcp__<server>__<tool>`, `tool_input` = arguments | `mcp__<server>__<tool>`, `tool_input` = arguments |
| subagent | `agent_id`, `agent_type` | `agent_id` (child thread id), `agent_type` |

**Repository root.**
- The root is the **main worktree root**, `dirname(git rev-parse --path-format=absolute --git-common-dir)`. Worktrees of one repository therefore share one data directory and one path space.
- Inside a submodule, where `--show-superproject-working-tree` is non-empty, the root is the superproject's main worktree root instead.

**Paths.**
- Paths are made repository-relative.
- The root is cached in the stream state per `cwd` and resolved again when `cwd` changes.
- Codex patch paths are resolved against the event's `cwd`.
- Paths outside the repository become `External(path)`.

### 3.3 Patch paths

**Recognised headers.** Each of these lines contributes the path `p`:
- `*** Add File: p`
- `*** Update File: p`
- `*** Delete File: p`
- `*** Move to: p`

**Where headers are read.**
- the input of `apply_patch`;
- a Bash command that pipes a heredoc into `apply_patch`.

**No headers.** The action gets `paths = []`.

**Testing.** The parser is a pure function, property-tested and fuzzed.

### 3.4 Output rendering

| Event and result | Claude Code | Codex |
|---|---|---|
| PreToolUse allow | No output. Never `permissionDecision:"allow"`, which bypasses the permission prompt | No output; a bare `allow` is rejected |
| PreToolUse warn | `hookSpecificOutput{hookEventName:"PreToolUse", additionalContext}` + `systemMessage` | Same keys |
| PreToolUse deny | `hookSpecificOutput{hookEventName, permissionDecision:"deny", permissionDecisionReason}` | Same keys; the reason must be non-empty |
| PreToolUse on a ContextDB MCP call (claim, Claude only) | No output | — |
| SessionStart and UserPromptSubmit context | `hookSpecificOutput{hookEventName, additionalContext}` | Same |
| Drift at Stop, SubagentStop or PreCompact | `systemMessage` only | `systemMessage` only |
| SessionEnd, PostToolUse, SubagentStart | No output | No output |

**Limits.**
- A reason is at most **480 bytes** of UTF-8, cut at a char boundary with `…` (T38).
- Context is at most **2,000 chars**. Claude spills anything over 10,000 chars to a file with a 2,000-char preview, and Codex spills at about 2,500 tokens, so 2,000 chars is never spilled.

**Exit code 2 is never used.** Every rendered output is validated in T41b.

## 4. Daemon protocol (PROTOCOL_VERSION 2)

**Compatibility.**
- `PROTOCOL_VERSION` goes from 1 to 2, and the version check is strict equality. P3 clients cannot talk to a P4 daemon, so the session-scoped requests are changed in place (implementation note, revision 3.3), with no mirror tags. P3 `ctx-mcp` processes get `ProtocolVersion` errors until the harness restarts them.
- The session log gains tags 7–11, 14 and 15–18 (§4.3). A P3 binary cannot open a P4 session log, so downgrade is not supported; P3 logs replay under P4 unchanged.
- All of this is in the upgrade notes.

**Session binding field.** `Recall`, `Read`, `Record`, `Supersede`, `Check`, `Updates` and `Briefing` carry `binding: Session{session, agent} | Key{harness, harness_session, agent} | Call{id}` in place of `session`. `Key` and `Call` resolve to `(session, agent)` (§2).

### 4.1 Requests

| Tag | Request | Channel | Purpose |
|---|---|---|---|
| 10 | `Check{binding, cert_id?, live_pos, action?, mode?, harness_version}` (extended) | agent | `Check` for the bound agent's view (§7). With `mode` (resolved by the hook from the action and its configuration), the daemon applies the shared predicate, logs a `Gate` event and returns the gate decision. The action does not filter in P4 |
| 17 | `SessionAttach{harness, harness_session, repo, head_sha}` | agent | §2.1 |
| 18 | `Claim{session, agent, call}` | agent | §2.3 (Claude) |
| 19 | `Updates{binding, ack: Option<u64>, max_items}` | agent | peek/ack of flips in the agent's view (§4.4) |
| 20 | `Facts{harness, harness_session, stream, packets: Vec<Packet>, skip: Option<(u64, u64)>}` | agent | outbox delivery (§5); replies only once the facts are durable |
| 21 | `Briefing{binding, budget_chars}` | agent | advisory briefing (§4.5) |
| 22 | `Telemetry{since, limit}` | admin | input for the dogfood report (§10) |

Policy uses admin `Raw{[SetPolicy]}`.

### 4.2 Responses

- **`Check`** gains `gate: Option<Decision>`, present when `mode` was sent, and `updates_pending: u32`. It never consumes updates. `ctx-mcp` reports the binding (`harness`, `claimed`, `unclaimed` or `server`) in its own output.
- **`Updates`** returns `{token, items: Vec<UpdateItem>, more}`, where `UpdateItem = {logical, key, cert_rev, current: Option<RevisionId>, summary}`.
- **`Facts`** returns `{applied_through}`, or `Gap{expected}`.
- **`Briefing`** returns `{stale, pending, conflicted, items}`, all advisory and without handles.
- **`Attached`** returns `{session, created}`; **`Telemetry`** returns `{entries, next}`.

### 4.3 Session events

| Tag | Event | Durability |
|---|---|---|
| 7 | `SessionKey{session_id, harness, harness_session}` | durable |
| 8 | `Fact{session_id, stream, packet, epoch}`: an applied outbox packet (action, drift input, baseline, stream end, unavailable gate) | durable before the `Facts` reply |
| 9 | `Gate{session_id, agent, action, mode, verdict, decision, harness_version, epoch}` | lazy (telemetry) |
| 10 | `Drift{session_id, agent, report, epoch}`: the computed report | durable before the `Facts` reply |
| 11 | `Reported{session_id, agent, items}` | lazy (§4.4) |
| 14 | `StreamMark{stream, applied_through, skipped: Option<(u64, u64)>}` | durable; one per `Facts` batch |
| 15 | `Read{…, agent}` | as P3 `Read` |
| 16 | `Write{session_id, agent, client_txn_id, epoch, reason, with_deps}` | lazy |
| 17 | `SessionCert{session_id, agent, cert_id}` | durable |
| 18 | `Unclaimed{epoch}` | lazy (telemetry) |

- **Agent ids on existing events.** The in-memory `Read`, `Write` and `SessionCert` events carry `agent` and are written under tags 15–17. P3 tags 1, 3 and 5 still decode, as the root agent (`agent = None`, `with_deps = false`), so a P4 daemon replays a P3 session log unchanged (test `p3_events_decode_as_the_root_agent`).
- **`Record` now logs `Write`**, so every write is tied to its session and agent; `with_deps` says whether the batch added a rule with incidences (coverage, §10).
- **Replay** rebuilds keys, per-agent ledgers and session certificates, stream marks from `StreamMark`, reported state, write sets and telemetry.

### 4.4 `context_update` (UserPromptSubmit)

**Reported state.**
- The reported state is kept per `(session, agent_id)`.
- For each entry in that agent's view (§7), the session remembers the last reported `(current head, SD13 summary)`. It starts from the state at read time.

**`Updates` calls.**
- **`ack: None`** returns the entries whose head or summary changed since the last report, ordered by key, at most `max_items`, with a `token`. Nothing is consumed.
- **`ack: Some(t)`** marks token `t`'s items as reported (a `Reported` event), then computes the next batch.

**Hook behaviour.**
- The hook stores the token per `agent_id` in the stream state after printing.
- It acks that token on the agent's next UserPromptSubmit.
- A Codex child turn (with an `agent_id`) therefore consumes only its own updates. The parent sees the same flips through its own reported state.

**Delivery.**
- Delivery is at-least-once. A duplicate line repeats a fact and is harmless.
- The update is rendered as `<context_update epoch=N>`, one line per item, within 2,000 chars, ending `(+k more)` when cut.
- It is appended as `additionalContext` and never rewrites earlier context, so the prompt cache is kept.

### 4.5 SessionStart briefing

The `Briefing` is advisory. It adds no ledger entries, so I6 holds.

It contains:
- counts of STALE, PENDING and CONFLICTED heads in the repository's scopes;
- the most recently changed heads, within `budget_chars` (default 1,500);
- one line telling the agent to read with certified mode before acting;
- a note when `gating.toml` changed without `ctx gating apply`.

## 5. Outbox (at-least-once delivery, idempotent application)

### 5.1 Streams, files and sequence numbers

- **Stream.** A stream is one harness session:
  `stream = blake3::derive_key("contextdb outbox stream v1", harness ‖ harness_session)[..16]`.
- **Files.** `<data dir>/outbox/` holds per stream:
  - `<stream>.log`: the records;
  - `<stream>.seq`: `next_seq`, guarded by the **append lock** (`flock` on this file);
  - `<stream>.ack`: `acked_through`, guarded by the **delivery lock** (`flock` on this file);
  - `<stream>.state`: cached repository roots, per-agent update tokens, the last empty-warning time and the baseline (§5.4). It is written under the append lock.
- **Record format.** `"CXOB" | len u32 LE | blake3(body) | body`, where `body = Wire(OutboxRecord{seq, agent_id, event})`.
  - A record is valid iff its hash matches.
  - The scan resyncs at the next magic.
  - A torn tail loses only the record being written, whose hook did not finish.
- **Append protocol.** The append holds `LOCK_EX` on `.seq` for about 1 ms and does no IPC:
  1. Read `next_seq` from `.seq`, then set `next_seq = max(next_seq, last valid seq in .log + 1)`. A hook killed after appending but before updating `.seq` therefore never causes a reuse (N1).
  2. Append the record with one `write(2)`.
  3. Run `fsync(2)` on the log, then write `.seq` and run `fsync(2)` on it. Plain `fsync` is enough: a killed process does not lose the page cache, and power loss is covered by the daemon-side durability.
- **Outbox contents.**
  - `Action` from PostToolUse;
  - `Drift` from Stop, SubagentStop and PreCompact;
  - `Baseline` from SessionStart;
  - `StreamEnd` from SessionEnd;
  - `Gate` facts, only when the daemon was unavailable.
- **Not in the outbox.** PreToolUse with the daemon reachable and MCP claims never touch it.

### 5.2 Delivery

1. **Deliverers.**
   - PostToolUse delivers its own stream. It runs `async` in both harnesses, so it is off the agent's path.
   - Stop, SubagentStop, PreCompact, SessionEnd and `ctx hooks flush` deliver their own stream first, then **every other dirty stream**, oldest file first, within the remaining budget.
   - `ctxd` sweeps `outbox/` at startup and every 30 s, delivering in-process with the same protocol (N8). Leftover streams of ended sessions are always drained while a daemon runs, including facts from Codex `async` hooks whose process was dropped at shutdown.
2. **Delivery lock.** A deliverer takes `LOCK_EX|LOCK_NB` on `.ack`. If the lock is held, it skips that stream, since someone else is delivering it. The delivery lock is separate from the append lock, so appends never wait behind IPC.
3. **Order.** Records with `seq > acked_through` are sent in `seq` order, in batches of up to 256.
4. **Ack.**
   - The daemon applies the batch, appends one `StreamMark`, flushes the session log, and replies `applied_through`.
   - The deliverer then writes `.ack` and runs `fsync`.
   - Once `acked_through ≥` the last seq in the log, the deliverer releases the delivery lock, takes the append lock, retakes the delivery lock (lock order is always append → delivery), checks again, and truncates the log to zero.
5. **Budgets.** 50 ms in PostToolUse; 500 ms in Stop, SubagentStop and PreCompact; 1 s in SessionEnd; unbounded in `ctx hooks flush` and the sweeper.
6. **Cap.** A log is capped at 16 MiB. Beyond the cap, facts are dropped and counted in `.state`, and the count is reported at the next delivery.

### 5.3 Idempotent application and gaps

- **High-water mark.** For each stream the daemon keeps `applied_through`, persisted by the durable `StreamMark` of every batch whatever the event types (N2), and rebuilt on replay.
- **Duplicates.** A packet with `seq ≤ applied_through` is skipped.
- **Gaps.** A batch that does not start at `applied_through + 1` is answered with `Gap{expected}`, and the deliverer resolves it:
  - **Its log has `expected`:** it resends from there.
  - **`expected ≤ acked_through`:** the daemon lost marks it had acknowledged, for example after `ctx doctor --truncate-before`. Those records are already truncated, so the deliverer sends `skip = (expected, acked_through)`.
  - **The log lacks seqs from `expected` up to its first valid record** (a corrupt middle record): the deliverer sends `skip` for that range.
  - **Recording a skip.** The daemon records `StreamMark{skipped}`, counts the loss in the report, and advances. A gap can never stall a stream.
- **Ended sessions.** Packets for sessions that have ended are applied to the ended session's record. Stream marks are kept forever, at one `u64` per stream. Nothing is dropped by age.
- **No kernel writes (T40).** No packet creates a revision or touches the kernel. T40 tests two things:
  - redelivered packets give one event each and leave the kernel digest unchanged;
  - MCP `Record` retries with the same `client_txn_id` create no second revision.

### 5.4 Drift (`DriftReport`)

**Baseline.**
- At SessionStart the hook runs `git status --porcelain=v1 -z` and one `git hash-object --stdin-paths` over the dirty paths, capped at 2,000 paths and 300 ms.
- It stores the baseline in `.state` and sends it as a `Baseline` fact.

**Drift report.** At Stop, SubagentStop, PreCompact and SessionEnd the hook computes `done`:
- `git diff --name-only <head_sha>` plus untracked files;
- minus baseline paths whose hash is unchanged;
- within 300 ms, or else `Unknown`.

It sends `Drift{agent_id, done}`. The daemon computes:
- `undeclared = done \ (paths declared by any agent of this session) \ (paths declared by other sessions active at the same time in the same repository)`;
- `unrealised = (paths declared by this agent) \ done`;
- counts of shell actions, writes, and writes with dependencies.

**Limits and storage.**
- Edits made by people, or by agents without hooks, remain in `undeclared`. The report states that `undeclared` is an upper bound.
- Drift goes to the session log, and to the user as one `systemMessage`. It is never gated.
- It is not a kernel `Artifact` in P4, because `OpenArtifact` requires a target revision (amendment A3).

## 6. Gating

### 6.1 Predicate (`contextdb-proto::gate`)

```
resolve_mode(cfg: &GateConfig, action: &Action) -> Mode     // hook, CLI, MCP
gate(mode: GateMode, verdict: &Validation) -> Decision     // daemon (Check), CLI dry-run, report
```

**`resolve_mode`.**
- Rules are `[[rule]] tool, path glob, mode`; the last matching rule wins.
- With several paths, the most severe mode wins: `enforce > warn > off`.
- `Shell` defaults to `warn`, even when edits are `enforce`.

**Severity order.** Valid ≈ ValidRecertified < Empty < Pending < Unknown < Ambiguous < Conflicted < Invalid < Denied.

**Decision by mode.**

| Verdict | `warn` mode | `enforce` mode |
|---|---|---|
| Valid, ValidRecertified | allow | allow |
| Empty | warn | warn |
| Pending, Unknown | warn | warn |
| Ambiguous, Conflicted, Invalid (any), Denied | warn | deny |

- **`Off`:** allow, with no daemon call.
- **Empty never allows (I6).** The hook *renders* an Empty warning at most once every 10 minutes per stream and agent, but logs the decision every time.
- **Reasons** name the stale items, the agent whose read put each into the view (so reviewers can label child-read false denies), and how to clear the decision, in at most 480 bytes.

### 6.2 Configuration

- **Source.** `.contextdb/gating.toml`, committed to the repository.
- **Compiled form.** `ctx gating apply` validates the file and writes `<data dir>/gating.wire`: a `GateConfig` plus the source hash. The hook reads only this file.
- **Defaults** when no compiled file exists: `mode = "warn"`, `on_unavailable = "open"`, `timeout_ms = 150`.
- **Data directory lookup.** In order:
  1. `CTX_DIR`;
  2. `<root>/.contextdb/dir.wire`, written by `ctx hooks install` from `.contextdb/config.toml`;
  3. `<root>/.contextdb/db`.

### 6.3 Unavailable daemon (T39)

**Not a ContextDB repository.** If there is no data directory, the hook gives no output in every mode.

**Unavailable daemon.** The hook treats any of these as unavailable:
- a connect error;
- the `timeout_ms` deadline passing;
- a `ProtocolVersion` mismatch;
- `Unparsed` input.

**Response by mode.**
- **`open`:** one-line `systemMessage` ("ContextDB unavailable: action not checked") and no decision.
- **`closed`:** deny, with the same reason.
- **Crashed hook.**
  - Claude: with `closed`, the installer adds `onFailure:"block"`, so a crashed hook also blocks.
  - Codex: always fails open on a crashed hook. This is documented.

The unavailable outcome is recorded in the outbox as a `Gate` fact.

### 6.4 `ctx check`

**`ctx check [--session S] [--agent A]`**
- Prints the verdict and the gate decision.
- Exit codes: 0 allow, 1 deny, 2 error, 3 unavailable. A warn exits 0 unless `--fail-on-warn` is given.

**Options.**
- `--strict` validates with `strict_no_aba = true`.
- `--all` is opt-in: it exits 1 if any head in scope is STALE or CONFLICTED.
- `--dry-run --action-tool T --action-path P…` evaluates the predicate without logging.

**Pre-push.** `--range` and the pre-push hook move to P5a (amendment A4).

### 6.5 MCP `ctx_check` (SPEC §6.3)

**Call.** `ctx_check(cert_id?, action?{tool, paths, command})` sends `Check` with the binding from `_meta` (§2), the action, and the mode resolved from `gating.wire`.

**Returns** `{decision, verdict, diffs, superseded, cert_id, binding, context_update}`, where:
- `decision` is the gate decision;
- `context_update` is a peek (`Updates{ack: None}`) that MCP never acks.

### 6.6 Certificate at SessionStart (ADR 0008 §16 open item)

- SessionStart issues no certificate: with an empty ledger, issuance would give `Empty` (I6).
- The first `Check` of each agent issues it lazily, as in P3.
- Amendment A2.

## 7. Subagents (T66): one session, per-agent ledger views

**Session.** A subagent shares its parent's harness session, stream and ctxd session. Thanks to call binding (§2), every MCP read and every hook fact carries the `agent_id` of the agent that made it; the root agent is `None`. P3 `Check` (tag 10) and the other P3 session requests act as the root.

**Ledger.** The ledger is keyed by `(agent_id, logical)`. ADR 0008's supersession rule applies **within each agent's own entries**: a newer read by the same agent replaces its older one.

**View.** The **view** of agent `a`, used for its checks, certificate and `context_update`, is:
- for each logical where `a` has its own required entry: that entry;
- otherwise, the entry of the root agent if there is one;
- otherwise, the newest entry from any other agent, by read epoch.

**What the view gives.**
- **Root checks cover child reads (SPEC "回传父会话").** A child's read of a logical the root never read is in the root's view. If that read goes stale, the root's check becomes Invalid.
- **Children inherit the root's view (SPEC "继承父证书只读视图").** A child sees the root's entries for every logical it has not read itself.
- **Root entries win in the root's view.** Root reads X@r5, then a child reads X@r7: the root's view keeps X@r5 and its check is Invalid until the root re-reads. A child's newer read never makes the root's check weaker (N7).
- **Write sets.** Write facts carry `agent_id`. Drift subtracts every path the session declared (§5.4).

**Lifecycle.**
- SubagentStart records nothing.
- SubagentStop runs drift for its agent and ends nothing.
- Resumed agents, repeated stops (Codex fires one per child turn) and stops for unknown agents are harmless: agents are just keys.

**Nesting.** Codex `agent_max_depth > 1` gives grandchildren their own `agent_id`. Hook input carries no parent agent id, so every non-root agent is treated as a direct child of the root. That is conservative: it only adds root entries to its view.

**T66.**
- A child reads X; the root's check covers X.
- X is superseded elsewhere. The child's next PreToolUse is Invalid, and so is the root's.
- If the root had read X itself earlier, its own entry governs its check.
- Each fact carries the child's `agent_id`.

## 8. Policy (T41)

**Applying a policy.**
- `.contextdb/policy.toml` holds, for example, `version = 1` and `require_deps = ["Decision", "Plan"]`.
- `ctx policy apply` parses it into a `PolicyDoc` and commits it with admin `Raw{[SetPolicy]}`.
- The policy takes effect only when committed.

**Enforcement.** Enforcement stays in the kernel transaction, so neither `--no-verify` nor any hook setting can bypass it.

**T41 tests:**
1. A `Record` of a required kind without dependencies, sent through `ctx-mcp`, fails with `PolicyViolation{NoEnabledRule}`.
2. The same batch sent as `Raw` on **`admin.sock`** also fails with `PolicyViolation`.
3. With all hooks removed, tests 1 and 2 give the same results.
4. `ctx policy show` prints the committed policy hash.

## 9. Installation (`ctx hooks install`)

**Claude Code.** The installer merges entries into `.claude/settings.json` (with `--project`) or into `~/.claude/settings.json`. Commands use exec form: `command: "<abs>/ctx-hook"`, `args: ["claude", "<event>"]`.

| Event | Matcher | Timeout | Notes |
|---|---|---|---|
| SessionStart | all sources | 5 s | |
| UserPromptSubmit | none | 2 s | |
| PreToolUse | `Edit\|Write\|MultiEdit\|NotebookEdit\|Bash\|PowerShell\|Monitor` | 2 s | `onFailure:"block"` only if `on_unavailable = closed` |
| PreToolUse | `^mcp__contextdb__(ctx_recall\|ctx_read\|ctx_record\|ctx_supersede\|ctx_check)$` | 2 s | the claim entry |
| PostToolUse | the edit and shell matcher above | 5 s | `async: true` |
| SubagentStop | none | 5 s | |
| Stop | none | 5 s | |
| PreCompact | none | 5 s | |
| SessionEnd | none | 3 s | |

**Codex.** The installer writes `.codex/hooks.json` or `~/.codex/hooks.json`.
- Tool events use `apply_patch|Bash`. Codex needs no claim entry (§2.2).
- Every event gets an explicit timeout; SessionEnd gets 2 s.
- `async: true` is set on PostToolUse, Stop, SubagentStop and PreCompact. It is not set on SessionEnd, which Codex always runs synchronously, nor on PreToolUse or UserPromptSubmit, whose output matters.
- The command is `"<abs>/ctx-hook" codex <event>`, with the path double-quoted and `"`, `\`, `$` and `` ` `` escaped. An unrepresentable path is rejected.
- The installer prints that Codex requires trusting the hooks.

**Common.**
- Reinstalling is a no-op.
- `uninstall` removes only `ctx-hook` entries.
- `--mcp-name` sets the server name used in the claim matcher.

## 10. Telemetry and the dogfood report (G6)

`ctx gate-report P4 [--since]` reports the following.

**Gate counts.**
- **Gated actions:** PreToolUse calls with a mode other than `off`.
- **Warn and deny counts:** per verdict.
- **Unavailable checks:** counted separately.

**False-deny rate.**
- **Definition:** gated actions a human labelled "should not have warned or denied", divided by all gated actions.
- **Threshold:** ≤ 2% for recommending enforce.
- **Labelling:** `ctx dogfood review` walks every warn and deny, plus a uniform 5% sample of allows. Labels go to `<data dir>/reports/review.jsonl`, which is local and outside the kernel.

**Dependency coverage.**
- **Definition:** `Write` events whose batch has a rule with at least one incidence (or, from P5a, an anchor), divided by all `Write` events.
- **Threshold:** ≥ 60%.

**Reported, no threshold.**
- the true-positive share of stale-action intercepts, and the legitimate-action allow rate;
- drift totals;
- daemon-side check latency, per harness version;
- `Unparsed` events, `Unclaimed` MCP calls, and outbox drops and skips.

**Output.** `docs/gates/p4-dogfood.md`, plus a JSON file of raw counts.

## 11. Verification plan

**G1.** `check.sh` covers:
- the dependency policy for proto, hooks and cli, with negative tests including "hooks must not reach store";
- the Linux and macOS legs.

**G2 golden tests.**
- **T38.** One session reads `db` with certified mode, and another session supersedes it. Then `ctx-hook` PreToolUse(Edit):
  - warns in `warn` mode;
  - denies in `enforce` mode;
  - gives a reason of at most 480 B that names `db`.

  The test runs end to end against an in-process daemon, in both harness modes.
- **T39.** Each case is run with both `open` and `closed`, in both harness modes:
  - no data directory;
  - daemon down;
  - daemon stalled past `timeout_ms`;
  - protocol mismatch;
  - unparsed stdin.
- **T40.** §5.3.
- **T41.** §8.
- **T41b.**
  - Golden stdin fixtures for every event and harness.
  - The 23 vendored Codex 0.163.0 schemas validate every input fixture and every rendered output, including the negative cases: no `hookSpecificOutput` on Stop, and no bare allow.
  - Claude fixtures come from the public hooks reference plus stdin captured from 2.1.306 by a recording hook.
- **T66.** §7.
- **Other tests:**
  - binding: Codex `_meta` direct binding (root and child); Claude claim by `tool_use_id`, consumed exactly once; a denied call's claim removed by PostToolUse; `Unclaimed` without `_meta`; `/clear`; concurrent identical calls from two sessions land in their own sessions;
  - `_meta` contract: a recording MCP server captures hook stdin and the `tools/call` request side by side for each pinned harness, pinning `_meta.callId`/`sessionId`/`threadId` (Codex) and `claudecode/toolUseId` (Claude);
  - P3 session-log replay under P4 (old tags decode as root);
  - `context_update`: peek and ack per agent; a child does not consume the root's updates; an output discarded before the ack is re-sent;
  - worktree and submodule roots;
  - the patch parser, including Bash heredocs;
  - mode resolution and the predicate over every verdict and mode;
  - installer merge and uninstall, and Codex quoting.

**G3 properties (10,000 seeds each).**
- (a) An outbox scan after any truncation, garbage or duplication returns each whole record exactly once, and never a torn one.
- (b) Any interleaving of appends, kills between append and the `.seq` write, truncations, deliveries and gaps gives strictly increasing seqs that are never reused. Every record appended in full is applied, or reported as skipped.
- (c) `gate` is monotone in severity for every mode.
- (d) `Updates` peek and ack per agent, over random head moves and dropped outputs, reports every flip at least once, and never re-sends an acked flip.
- (e) The agent view equals the oracle view, and a child read never changes the root's verdict for a logical the root read itself.
- (f) Path normalisation is idempotent.

**Mutation testing** covers the hooks library, `contextdb-proto` and the new daemon session code. Surviving mutants need a waiver with a reason.

**G4 robustness.**
- **Hook kills** (SIGKILL subprocess tests): before the append; after the append but before `.seq`; after `.seq` but before delivery; after delivery but before `.ack`.
- **Daemon kills:** between applying a batch and the `StreamMark` sync; after the sync but before the reply.
- **Daemon restarts:** with only Gate facts delivered; mid-session.
- **Damage:** a corrupt middle record; a doctor-truncated session log.

Each case must end with every fully appended fact applied, or counted as skipped. Delivery is at-least-once and application is idempotent.

- **Stress:** 16 concurrent hook processes per stream × 8 streams × 10 min, with no lost or duplicated seq and no append waiting on delivery.
- **Fuzz** (≥ 1 h each): both stdin adapters; the patch parser; `_meta` binding extraction; `GateConfig` and `OutboxRecord` decoding.

**G5 performance.**
- **Budget (initial):** PreToolUse end to end p95 ≤ 50 ms and p99 ≤ 100 ms, from process spawn to exit.
- **Cells:**
  - fullfsync on and off;
  - 1 and 8 sessions;
  - ledgers of 10, 100 and 1,000 entries.
- **Also reported:**
  - the certificate re-issue rate;
  - the Claude claim hook latency, and MCP read latency including it as the agent sees it;
  - the Codex `$SHELL -lc` launch cost, as harness overhead;
  - PostToolUse and UserPromptSubmit latencies.

**G6 scenario.** Two scripted agents, one per harness mode, share a decision:
1. one supersedes it;
2. the other's next Edit is warned (warn mode) or denied (enforce mode);
3. a Bash write shows up as undeclared drift;
4. a child read is covered by the root's check;
5. MCP reads from two sessions under one process land in their own sessions (Codex `_meta`, Claude claims).

The dogfood row stays Pending (§0.1).

**G7 regression.** P0–P3 G2, G3 and G4 at gate counts, including P3 stress and fuzz on protocol v2.

**G8 docs.**
- a README hooks section: installation, modes, trust boundary, TOCTOU, Codex trust, `/clear` and resume, limitations;
- CHANGELOG;
- `docs/IMPLEMENTATION_STATUS.md`;
- upgrade and downgrade notes.

## 12. Milestones

1. **Protocol and claims.**
   - Split out `contextdb-proto`, then protocol v2.
   - `SessionAttach`, `_meta` binding, Claude claims.
   - Per-agent ledger views and the extended `Check`, `Updates`, `Facts`, `Briefing`, `Telemetry`.
   - `Record` logs `Write`; `Client` timeouts.
2. **`contextdb-hooks` library:** adapters, renderers, patch parser, outbox.
3. **`ctx-hook` binary:** every event; T38, T39 and T41b.
4. **Outbox delivery, sweeper and drift;** T40 and T66.
5. **CLI:** `ctx check`, `ctx policy`, `ctx gating apply`, `ctx hooks …`, `ctx gate-report`, `ctx dogfood review`; T41.
6. **Verification:** G3–G5 and the G6 scenario, then hand the dogfood kit to the owner.

## 13. SPEC amendments (SPEC v1.6.0)

| ID | Change |
|---|---|
| A1 | §5 P4, §6.5: subagents share the root's session through per-agent ledger views (§7). A child inherits the root's view; the root's checks cover the child's reads; the root's own reads govern its view |
| A2 | §6.5 SessionStart registers the session and injects an advisory briefing; the certificate is issued at the first check |
| A3 | §1, §5 P4, §6.5: in P4 a `DriftReport` is a session-log record, not a kernel `Artifact`, because `OpenArtifact` requires a target revision. P5a revisits this with anchor targets |
| A4 | §6.5 pre-push: `--range` and the pre-push hook move to P5a. P4 provides `ctx check --strict` per session and an opt-in `--all` |
| A5 | §5 P4 outbox: `packet_id = (stream, seq)`, with a recovered per-stream monotonic `seq` and `stream = blake3(harness, harness_session)`, replacing `sha256(repo, session, range)` |
| A6 | §6.3 `ctx_check`: `context_update` is a non-consuming peek in MCP; UserPromptSubmit acknowledges it |
| A7 | §10, §6.5: MCP calls bind to sessions by the harness tool-call id in `_meta` (Codex directly; Claude through a PreToolUse claim on ContextDB's MCP tools, a new hook role); unbound calls are reported |
| A9 | §7 and §5 P3: `read_ledger` gains `agent_id` (`read_ledger(session, agent, logical, rev, layer, epoch, required)`) |
| A10 | §6.4: session certificates are issued per agent view; the `Certificate` layout is unchanged and the session-log binding (`SessionCert2`) records the agent |
| A8 | §3.5 #7, §5 P3 and §7: the session log's domain widens from "sessions, read sets and certificates" to **session facts and telemetry** (actions, gate decisions, drift, baselines, update acknowledgements, stream marks, session keys). It still never feeds kernel state |
| A11 | Principle 2 and appendix C (owner decision §0.1): a dogfood layer that needs long real use may run in parallel with the next phase's G0 and implementation, but the next phase's gate cannot pass before this one |

## 14. Review mapping (revision 1)

| Finding | Resolution |
|---|---|
| B1 ack before durable | §5.2 and §5.3: the reply comes after `StreamMark` and the flush |
| B2 packet_id reuse | §5.1: a persistent per-stream seq, recovered from the log (see also N1) |
| B3 Empty allowed | §6.1: the option is removed |
| M1 Stop context | §3.4 |
| M2 binding | §2: replaced by call claims |
| M3 worktrees | §3.2 |
| M4 drift noise | §5.4 |
| M5 DriftReport | A3 |
| M6 hot path | §4.1 (Gate is logged by `Check2` itself) and §5.2 |
| M7 updates consumed | §4.4 |
| M8 subagents | §7 |
| M9 T41 | §8 |
| M10 dependencies | §1 |
| M11 fail-closed scope | §6.3 |
| M12 `--strict` | §6.4, A4 |
| M13 MCP and briefing | §6.5, §4.5 |
| M14 dedup | §5.3 |
| Minors | Closed in revision 2; the reviewer confirmed this |

## 15. Review mapping (revision 2 closure)

| Finding | Resolution |
|---|---|
| N1 seq reuse after a kill | §5.1 step 1: `next_seq = max(state, last valid seq in log + 1)` under the append lock; property (b); G4 kill points |
| N2 gap stall | §5.3: a durable `StreamMark` per batch, independent of event types; the `Gap` and `skip` resync with loss counted; G4 cases for Gate-only delivery, a corrupt middle record and a doctor truncation |
| N3 Codex binding | §2: call claims through the MCP PreToolUse hook, independent of process topology; process ancestry is removed |
| N4 `Check2` gate | §4.1 and §6.1: the hook sends only the mode; the daemon applies `gate` from `contextdb-proto` and logs it |
| N5 shared update token | §4.4: reported state and tokens per `(session, agent_id)` |
| N6 session-log domain | A8 |
| N7 alias weakens the root | §7: per-agent views where the root's own entries govern its view; property (e) |
| N8 leftover streams | §5.2: hooks drain all dirty streams within budget; the `ctxd` sweeper at start and every 30 s |
| m1 proto dependencies | §1: core instead of nix |
| m2, m3 `ps` and hints | Removed together with process binding |
| m4 lock contention | §5.1 and §5.2: separate append and delivery locks |
| m5 durability cost | §5.1: plain `fsync(2)`; Codex `async` (§9) |
| m6 drift details | §5.4: baseline in `.state`; `--stdin-paths`; "active at the same time"; whole-session declared set |
| m7 submodules | §3.2 |

## 17. Review mapping (revision 3 closure)

| Finding | Resolution |
|---|---|
| C1 digest claims misattribute | §2: bind by tool-call id from `_meta` (Codex `callId`/`sessionId`/`threadId` directly; Claude `claudecode/toolUseId` via id-keyed claims); digest fallback removed; T41b `_meta` contract test |
| M1 event layout change breaks upgrade | §4.3: new tags `Read2`, `Write2`, `SessionCert2`; old tags decode as root; P3-log replay test |
| M2 claim TTL vs permission prompts | §2.3: claims live until consumed, PostToolUse or session end; matcher limited to session-scoped tools |
| M3 missing amendments | A9 (`read_ledger.agent_id`), A10 (per-agent session certificates); A7 names the new hook role |
| Digest pitfalls | Moot: no digest |
| Claude argument parity | Moot for binding; the `_meta` capture test covers the call path |
| Truncation lock order | §5.2.4: always append → delivery |
| Codex `async` drift | §5.2.1: the sweeper delivers it |
| Child reads in the root view | §6.1 reasons name the agent; §16 |
| P3 `Check` semantics | §7: root view |
| Claim latency | §11 G5: reported with MCP latency as seen by the agent |
| Closure minor: PostToolUse claim removal | §2.3: claims live until consumed or session end; orphaned claims are inert |
| Closure minor: `--mcp-name` escaping | §2.3: normalised and regex-escaped |
| Closure minor: Codex root rule | §2.2: `threadId == sessionId` is the root; unknown ids treated as root |
| Closure answer: `claudecode/agentId` | §2.3: cross-check only |

## 16. Risks and limitations

- **TOCTOU.** PreToolUse narrows but does not close the window between check and write (SPEC §6.4).
- **Cooperative boundary.** An agent can disable hooks, call `admin.sock`, or forge claims (ADR 0008 §0.5). P9 adds hard separation.
- **Binding** depends on `_meta` keys that are pinned but, for Claude, undocumented, and on the installed claim matcher. A harness change gives `Unclaimed` calls, which are reported, never misattributed.
- **Child reads** stay in the root's view after the child finishes, so their later staleness can warn or deny the root's edits; reasons name the reading agent.
- **Codex.** Hooks run only after the user trusts them, and a crashed Codex hook fails open.
- **Bash** is gated without paths, and its effects show only in drift.
- **Drift.** `undeclared` is an upper bound when people or agents without hooks share the worktree.
