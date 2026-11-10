# Reference

[README](../README.md) · [Getting started](GETTING_STARTED.md) · **Reference** · [Development](DEVELOPMENT.md)

What each phase added, how to use it, and its configuration. The normative semantics are in [SPEC.md](../SPEC.md); the evidence per phase is in [gates/](gates/).

## Crates

| crate | Role |
|---|---|
| `contextdb-model` | Dependency-free transaction vocabulary: IDs, the 14 Ops, wire encoding |
| `contextdb-core` | Incremental counting kernel: support / witness / three-layer certification, CURRENT, WHY, IMPACT, DIFF, certificates |
| `contextdb-oracle` | Independent reference implementation, for verification only |
| `contextdb-store` | op_log, blobs, snapshots, recovery, SQLite projections, session log |
| `contextdb-daemon` | `ctxd`: single-writer daemon, snapshot publication, local IPC, sessions and gating |
| `contextdb-proto` | IPC protocol, lease discovery, blocking client, gate predicate, hook outbox |
| `contextdb-mcp` | `ctx-mcp`: MCP stdio client |
| `contextdb-hooks` | `ctx-hook`: Claude Code / Codex / OpenCode hook adapters |
| `contextdb-anchors` | File-anchor observer: Git OID capture, classification, commit |
| `contextdb-symbols` | Symbol anchors: tree-sitter extraction and the normaliser (observer side only; `ctxd` never links it) |
| `contextdb-cli` | `ctx`: operations, checks, anchors, gating, setup and hook installation |
| `contextdb-testkit` / `-harness` / `-workload` / `-bench` | Goldens, property tests, replay, workload generation and benchmarks |

Separate workspaces: `crates/contextdb-dd` (research baseline, outside the product dependency graph) and `fuzz/`.

## Kernel (P0–P1)

The Rust API provides `Db<Counting, NoStats>` / `Db<FullRecompute, CommitStats>`, atomic transactions, CURRENT, WHY / WHY_NOT, Reachable / Exact IMPACT, cross-epoch DIFF and deterministic digests. Counting maintains L0 incrementally and L1/L2 as counters (ADR 0008 §7); `paranoid` builds cross-check every commit against the independent oracle.

- Immutable revisions and logical heads, with mandatory CAS
- Forward-only, acyclic AND/OR justifications; exact incremental set support / unsat / witness; Pearce–Kelly dynamic topological order with a NaiveTopo reference
- Structural support and semantic PENDING are independent
- **Pin-strict**: a `Pin` to a replaced revision (except Evidence) is STALE. Use Evidence or `Ref` for historical references, and `Rebind` or `Sem` to follow new revisions
- Transactions publish atomically; intermediate states are invisible to readers
- Certificate issue and validation: L2 witness closure, the `since_epoch` window in strict mode, op_log link checks and re-certification (`ValidRecertified`)

```sh
cargo run --locked -p contextdb-core --example basic
```

The example builds a source → derived rule, retracts the source, and asserts that both nodes flip and that derived becomes STALE.

## Durability (P2)

- **Source of truth**: an own append-only segmented log in `<dir>/log/`; one frame per committed batch, linked by a blake3 hash chain. SQLite only holds rebuildable projections (`diff_log`, `head_history`, `base_history`).
- **When a commit is acknowledged**: only after its frame is written and synced. macOS uses F_FULLFSYNC by default (`StdDisk { full_sync: true }`); the fullfsync-off mode exists only for measurement. A failed sync or write is fail-stop: the store is poisoned and can only be recovered by reopening in a new process.
- **After a crash**: recovery to the acknowledged prefix (I5). An incomplete last frame is truncated and reported in the open report (`--strict-tail` refuses to open instead); damage to an earlier frame refuses to open and can only be repaired explicitly with `ctx doctor --truncate-before`, which first lists the epochs and client_txn_ids that would be lost.
- **Idempotency**: a retry with the same `client_txn_id` returns the original result; `txn_status` returns `Committed{epoch}` or `NotCommitted`. A poisoned store refuses all reads and `txn_status`; the outcome of an in-flight transaction is known only after restart.
- **Operator tools**: `ctx` never creates a store; a mistyped path gives `NotAStore`. `doctor --truncate-before` and `migrate` hold the same directory lock. An interrupted migration makes open refuse (`MigrationIncomplete`); running `ctx migrate` again completes it.
- **Not supported**: erasure before P9 (blobs are never deleted); no signed heads, so the hash chain detects damage but not a wholesale rewrite.

```sh
ctx replay <dir>        # open, replay and print the open report
ctx doctor <dir>        # hash chain, full oracle check, blob check; --rebuild rebuilds projections
ctx snapshot <dir>      # write an input-layer snapshot for faster cold starts
ctx migrate <dir>       # migrate an older store to format 5: log, encrypted packs,
                        # rebuilt projections, modes (keeps a log backup; rerunnable)
ctx export <dir>        # export as EventLog v0 JSONL
```

## Daemon and MCP (P3)

- **`ctxd <dir>`**: one daemon per data directory, the only writer of the op_log. A second instance is refused by the directory lock and lease (T37). Network and virtual file systems are refused by default; `--allow-fs <name>` allows one explicitly.
- **Commits**: a single commit actor batches write requests into a group commit with one op_log sync per group; every reply waits for that sync. Payloads are appended to self-verifying blob packs, with ordered syncs so they are durable before the frames that reference them (ADR 0008 §17).
- **Reads**: readers get an epoch-pinned snapshot and never see a half-written group (I4). Snapshots alternate between two replicas, and a reclaimed replica is reused only when no reader holds it (checked with loom).
- **Three sockets**: `<dir>/run/agent.sock`, `observer.sock`, `admin.sock` (in a temporary directory if the path would be too long), mode 0600 with a peer-uid check. The socket determines the channel; request parameters cannot forge an identity. Controlled semantic validation (`ctx validate`, which an authorised maintenance agent may run) and `shutdown` are admin-only; raw batches are not accepted from agents.
- **Trust boundary**: P3 promises only a cooperative boundary. A malicious agent that can run a shell as the same OS user can still connect to the observer/admin sockets; hard isolation comes with principals and ACLs in P9.
- **Sessions**: sessions, read ledgers and certificates are written to a separate session log (`<dir>/sessions/`), grouped and hash-chained like the op_log, so sessions survive a daemon restart. Read events are durable within 100 ms; `SessionStart`, `CertIssued` and `SessionEnd` sync immediately.
- **Timeouts and overload**: a full write queue returns `Busy`. A request over 30 s returns `Timeout`; a timed-out write may still commit, and `ctx_txn_status` with its `client_txn_id` tells you whether it did.

**MCP**: `ctx-mcp <dir>` serves JSON-RPC 2.0 over stdio (one message per line) and finds the agent socket through `<dir>/ctxd.lease`.

| Tool | Purpose |
|---|---|
| `ctx_recall` | Search by key / body (FTS5); certified mode returns only CERTIFIED items, with handles usable as deps (`<12 hex>.<idx>`) |
| `ctx_read` | Read one record; certified mode fails unless the current head is CERTIFIED |
| `ctx_record` | Create (`create_only` + `key`) or revise by CAS (`expected_head = key@rev`). Each dep is a handle or `key@rev` with a kind: `cur`, `pin`, `sem` (the only premise kind a Summary accepts) or `ref` (a reference that never changes status). Deps that are all `ref` support nothing, so such a record holds only with `adopt_as_base` |
| `ctx_supersede` | Replace a head; `carry_rules=copy` copies only the old head's own justifications, and downstream rules are never rebound automatically |
| `ctx_why` / `ctx_why_not` / `ctx_impact` | Explain why something holds or not, and list what depends on it |
| `ctx_check` | Check this session's certificate (this agent's view): verdict, diffs, and the reads that changed since the last acknowledgement (`context_update`, peeked not consumed); optionally with `action{tool, paths, command}` |
| `ctx_txn_status` | Look up `Committed{epoch}` or `NotCommitted` by `client_txn_id` |
| `ctx_commit` / `ctx_what_if` | Several ops atomically on the session's branch (optionally only while the session certificate is valid), or a dry run of them (P6) |
| `ctx_branch` / `ctx_merge` | Create, list or archive branches; merge one into another (P6) |
| `ctx_changes` | What changed status on the session's branch since an epoch, page by page, optionally waiting up to 30 s for the next epoch (P7) |
| `ctx_handoff_begin` / `ctx_handoff_accept` | Hand a certified session state (reads plus a summary) to another session; accepted once, and the acceptor's checks then cover the handed-over reads (P7) |
| `ctx_conflicts` / `ctx_resolve` | List contradictions, validator reviews and lexical candidate pairs on the session's branch; assert that two records cannot both hold (both read CONFLICTED until resolved or superseded); resolve with a note (P7) |

P3 does not offer `ctx_validate` to agents.

**Operator commands** (through the admin socket while `ctxd` runs; offline commands are refused with `Locked` while it runs):

```sh
ctxd <dir> [--allow-fs <name>]   # start the daemon
ctx status <dir>                 # revision / logical / session counts
ctx why|why-not|impact <dir> <key[@rev]>
ctx diff <dir> <from> <to>
ctx validate <dir> <key@rev> verified|rejected [--yes]
                                 # controlled validation: shows the current semantic vectors first;
                                 # automation may pass --yes, still admin-only with a curvec CAS
ctx conflicts list|open|resolve <dir> ...
                                 # contradictions, validator reviews and lexical candidate pairs
                                 # (SD20), as admin; open --from-candidate ID opens a listed pair
ctx changes <dir> [--since E] [--branch B] [--scope S] [--follow]
                                 # status changes per epoch; --follow keeps waiting for new ones
ctx validators status|reload|check <dir>
ctx validators retry <dir> <job-key>|--failed
                                 # P7 validator jobs; none run until <dir>/validators.toml lists a
                                 # validator. LLM validators only open PendingReviews; one with
                                 # `pairs = true` also judges lexical candidate pairs.
ctx shutdown <dir>
```

## Hooks and action gating (P4)

`ctx-hook <claude|codex|opencode> <event>` is the hook command for Claude Code (2.1.306), Codex CLI (0.163.0) and OpenCode (2.0.32, through its plugin): it reads one hook event from stdin, prints at most one JSON object, and always exits 0 (ADR 0010). OpenCode 2.0 has plugins instead of hook commands: `ctx hooks install --harness opencode` writes a plugin that calls `ctx-hook opencode <event>` with the same stdin and maps its answer back. A deny fails the tool call, a warning follows the tool's output, and the briefing and per-turn context go into the system prompt. OpenCode's MCP calls carry their session id, so no claim is needed, and a subagent's child session is its own ContextDB session (ADR 0014).

- **Install**: `ctx hooks install --harness claude|codex|opencode --project <repo> --dir <data dir>` merges the hooks into `.claude/settings.local.json` (personal and untracked) or `.codex/hooks.json`, keeping any other hooks, or writes the OpenCode plugin to `.opencode/plugins/contextdb/`. Reinstalling changes nothing, and `uninstall` removes only ContextDB's entries. **Codex runs new hooks only after you trust them** (open `/hooks` in Codex, or accept the startup review), and again after every change. `ctx setup` does this for you.
- **Session binding**: each hook event binds to a ctxd session by the harness's `session_id`. MCP calls bind by the tool-call id the harness puts in `_meta`: Codex sends `sessionId` / `threadId` directly, OpenCode sends `ai.opencode/sessionID`; for Claude Code, the PreToolUse hook claims the `tool_use_id` first. A call without an id uses `ctx-mcp`'s own session and is marked `unclaimed`; nothing is guessed from content.
- **Subagents**: a Claude Code or Codex subagent shares its root's session with a per-agent view: it inherits the root's reads, its reads enter the root's view, and the root's own reads of a logical take precedence. An OpenCode subagent runs in its own child session, which is a separate ContextDB session (more warnings, never a false allow).
- **PreToolUse**: Edit / Write / MultiEdit / NotebookEdit / Bash (Claude), `apply_patch` / Bash (Codex) and `edit` / `write` / `patch` / `shell` (OpenCode) check the session certificate before running. `warn` mode adds context explaining which reads changed; `enforce` mode denies on Invalid / Conflicted / Denied / Ambiguous with a reason of at most 480 bytes; an allow prints nothing (it never bypasses the harness's own permission prompts). A session with no certified reads (`Empty`) never counts as passing; it warns once per session and agent, and every occurrence is recorded. Other warnings are shown once per set of stale reads: a shell command does not repeat an identical warning, while file edits, denies and a new commit show it again. A check that does not get an answer in time says `ctxd did not answer within N ms`.
- **Gating configuration**: `.contextdb/gating.toml` (`mode = off|warn|enforce`, `shell_mode`, `on_unavailable = open|closed`, `timeout_ms`, `[[rule]]` per tool / path glob) takes effect after `ctx gating apply <dir>`. Defaults: `warn`, fail-open, 500 ms. The P4 dogfood supports `mode = "enforce"` with `shell_mode = "warn"` as the stricter setting (0.11 % false denies); enforcing shell commands too would have wrongly denied 30.6 % of actions, mostly read-only commands. If the daemon is unreachable, times out or the input cannot be parsed, `open` only notes it and `closed` denies; outside a ContextDB repository nothing happens.
- **context_update**: UserPromptSubmit appends the reads that changed since the last acknowledgement to the new prompt as `<context_update epoch=…>`, without rewriting the existing prefix (keeps the prompt cache); they are acknowledged with the next prompt, and lost output is sent again.
- **Outbox and drift**: PostToolUse writes facts to a local outbox first (`<data dir>/outbox/`, self-verifying records, a monotonic seq per stream), then delivers them; the daemon applies them idempotently by `(stream, seq)`, and `ctxd` delivers leftover streams at start and every 30 s. Stop / SubagentStop / PreCompact compare Git's actual changes with the recorded edits and report "files changed without a record" (such as Bash side effects) as a `systemMessage`, naming only paths new since that agent's last report. Drift is reported, never gated, and is an upper bound: changes by people or by agents without hooks also appear.
- **Policy**: `.contextdb/policy.toml` (e.g. `version = 1`, `require_deps = ["Decision", "Plan"]`) is committed with `ctx policy apply <dir>` through the admin socket; it is enforced inside kernel transactions, so no channel can bypass it, regardless of hooks or `git --no-verify`.
- **What is recorded**: action facts (including full Bash commands) go to the session log for drift and dogfood reports; a secret in a command is recorded too.
- **Remaining window (TOCTOU)**: there is still a window between the PreToolUse check and the actual write; it narrows the risk but does not remove it. Bash side effects cannot be resolved to paths before execution and are only found afterwards by drift. The trust boundary is cooperative: an agent can uninstall hooks or connect to the admin socket directly (hard isolation in P9).

```sh
ctx check <dir> --harness claude --key <session_id> [--strict] [--dry-run --action-tool edit --action-path src/a.rs]
                                 # exit codes: 0 allow (warn is 0 too unless --fail-on-warn), 1 deny, 2 error, 3 daemon unreachable
ctx check <dir> --all            # exit 1 if any head is STALE / CONFLICTED (optional, not installed by default)
ctx hooks flush <dir>            # deliver all pending outbox facts now
ctx gate-report P4 <dir> [--out docs/gates/p4-g6-dogfood-report.md]
ctx dogfood review <dir>         # agents review warn/deny decisions and sampled allows for the false-deny rate
```

**Upgrade note**: P4 raised the IPC protocol to version 2; a P3 `ctx-mcp` gets a `ProtocolVersion` error and recovers when the harness restarts it. P4 adds session-log events that P3 binaries cannot open (no downgrade); P3 session logs replay unchanged under P4.

## File-level code anchors (P5a)

Register repository files as `CodeAnchor` records, and decisions and plans can depend on them like on any other record; when a file changes, dependents become STALE or PENDING according to their edge kind (ADR 0011).

- **Identity**: an anchor revision is the Git blob OID of the **raw worktree bytes** (as `git hash-object --no-filters`, SHA-1 and SHA-256 repositories), without clean / EOL filters. Identical content reuses the same RevisionId, so restoring a deleted file restores its dependents. The key is `code:<repo-id>/<relative path>`, and only lowercase ASCII paths are accepted; `..`, `.git`, symlinks (including parent directories), submodules and the runtime data directory are refused or classified Broken.
- **Observer**: `ctx anchors scan|watch` takes two complete samples of HEAD and every registered file and, only if they agree, commits the content changes and an `ObserverWatermark` in one batch (CAS; resampled on conflict). Deletion → `RetractBase` + `AnchorBroken`; restoration → the base is asserted again.
- **Freshness**: for every certified read, recall and check, the daemon samples live through trusted routes (`<data dir>/anchors.routes`, written by `ctx anchors add`). A live position that differs from the watermark, a failed sample, exceeded bounds (1,000 anchors per scope, 16 MiB per file, a 200 ms sample), missing routes or a corrupt descriptor, an edit announced during the sample (hooks update `outbox/announce` before appending an edit; the daemon reads it before and after sampling), a missing or corrupt announce record, a failed announce (repair with `ctx anchors repair-announce <dir>`), or a data directory without the `announce-v1` marker (written by `ctx hooks install --dir`; every harness using the directory must be reinstalled with P5a hooks) all give `PENDING(ObservationLag)` / certificate `Unknown`, never fresh.
- **Path filtering**: PreToolUse for Edit / Write / apply_patch checks only the required reads whose support closure reaches the target paths (Sem looks at both the asserted premise and the current head; Ref is not followed). Unanchored reads, Bash, and unknown or alias-ambiguous paths (case aliases, hard links, symlinks, nested repositories) fall back to a full check. Each root's proof is persisted when it is first certified, and filtered checks keep its original strict window.
- **Git hooks and pre-push**: `hooks install` adds a scan to post-commit / post-checkout / post-merge, keeping and running any existing hook first (backed up as `<hook>.contextdb-original`); only `--pre-push` installs the hard gate. Pre-push handles every ref pair: the pushed commit must be the current HEAD and every registered file's raw bytes must match that commit, otherwise the result is Unknown and the push is refused.

```sh
ctx anchors add <dir> <repo> <path>...   # register (and write the trusted route), then scan once
ctx anchors scan|watch|status <dir> <repo>
ctx anchors hooks <dir> <repo> install [--pre-push] | uninstall
ctx check <dir> --strict --range <a>..<b> [--repo <repo>]
                                 # point-in-time check: from the registered files changed in a..b, over every
                                 # reachable current head (not limited to a session's reads); issues a new certificate
                                 # whose strict window starts now; does not claim the absence of historical ABA
                                 # across the Git range. Non-Valid exits 1
```

**Upgrade note**: P5a raises the IPC protocol to 3 and writes format-2 session-log segments (forcing a rollover from a format-1 segment before the first new event). P4 session logs read unchanged under P5a; P4 binaries refuse format-2 segments before any truncation, and downgrade is not supported.

## Symbol-level code anchors (P5b)

A decision can depend on one function, type or method instead of a whole file, so unrelated edits to the same file leave it CERTIFIED (SD18, ADR 0012).

- **Languages**: by extension only. Rust (`.rs`), TypeScript / TSX / JavaScript (`.ts .mts .cts .tsx .js .mjs .cjs .jsx`), Python (`.py`) and Go (`.go`); other files keep file-level anchors. Symbols are item-level declarations (functions, types, traits, impl and class members, module-level constants and variables); nothing inside a function body is a symbol. Declarations sharing a path (overloads, getter + setter, `cfg` duplicates, interface merging) form one symbol.
- **Keys**: `code:<repo-id>/<path>#<encoded symbol>`. Rust impl items are `Type::item` or `Type#Trait::item`, with an 8-hex key hash for generic trait arguments or a non-path self type (`src/x.rs#T#From#3fa1c2d0::from`). Keys never change with the normaliser. File paths may no longer contain `#`; a legacy file anchor with `#` is `Broken("reserved-character")`.
- **Identity**: a symbol's revision is a hash of its normalised syntax (tree-sitter, pinned grammars): formatting, comments and formatter-equivalent forms do not change it, while a change to the same-file helpers, constants or imports it names does (walk 3), and so does any source text, such as string contents between escapes (walk 4). A symbol that disappears is `Broken`, and its `AnchorBroken` lists rebind candidates with the same body under another name or in a sibling file (suggestions only). A pure move or rename therefore invalidates, because the anchor has to be rebound.
- **Freshness**: the observer parses; the daemon and hooks never do (`ctxd` and `ctx-hook` link no grammar). Watermarks record each host file's blob (`CTXPOS02`), so any byte change in a host file makes the scope `PENDING(ObservationLag)` until the next scan; unchanged symbols then come back fresh without a new revision. Parse failures stay local: a broken region does not break symbols elsewhere in the file, except those that use a same-file helper, constant or import that does not parse, and a scan that runs out of budget is unavailable as a whole (fail closed).
- **Cost**: at 1,000 symbol anchors the P4 hook stays at p95 ≤ 29 ms; a full scan costs ≈ 0.38 s per MiB of host files, so the 5 s deadline binds at ≈ 13 MiB.

```sh
ctx anchors symbols <repo> <path>                    # list a file's symbols and their keys
ctx anchors add <dir> <repo> <path>#<symbol>...      # e.g. src/store.rs#Store::open; must exist
ctx anchors migrate <dir> <repo>                     # after a normaliser upgrade; reports changed revisions
```

**Upgrade note**: the protocol stays at 3 and file anchors are unchanged. Symbol anchors made with an earlier normaliser walk (the current one is walk 4) need `ctx anchors migrate` once; dependents of re-hashed symbols go STALE or PENDING once. An anchor that is Missing or Broken during the migrate keeps its old descriptor and no longer blocks scans.

## Branches, transactions and WHAT_IF (P6)

Context can be forked, changed in isolation and merged back, like the code it describes (SPEC SD19, ADR 0013).

- **Branches**: `main` always exists; other branches are created explicitly (`ctx branch create`, `ctx_branch`). A session binds to the ContextDB branch named like its worktree's git branch, otherwise to `main`, and the observer commits a repository's anchors on that branch the same way. Reads on a branch see only what is visible there; ids are global, and `main`'s reads mark objects that exist only on other branches `BRANCH_ONLY`. A fork shares its parent's state (constant time and memory); the first write after a fork or merge copies that branch's state once.
- **Merge**: three-way, against the last common sync point (Dolt-style classification of heads, base bits, rule enablement, ValidationRecords, artifacts, metadata and policy). Values changed on both sides keep the target's value and open a `Conflict` artifact; an open Conflict gates its target revision (`CONFLICTED`, SD17) until it is resolved. `require_certified` (the default) refuses a merge unless the moved, new and conflicted records are CERTIFIED afterwards. Agents may merge, including into `main`.
- **Transactions**: `ctx_commit` commits several ops atomically on the session's branch. With `expect_cert_valid`, the session's certificate is validated inside the writer and its read set checked in the same step (`ExpectHeads`), so of two plans that read the same records and write conflicting changes, the first commits and the second is refused (write skew, T19).
- **WHAT_IF**: `ctx_what_if` returns what a batch (including a merge) would do, status changes and allocations, without committing; the database is restored from an undo journal.

```sh
ctx branch create <dir> <name> [--from B] | list <dir> | archive <dir> <name>
ctx merge <dir> <source> [--into B] [--no-require-certified] [--certify K ...]
ctx status|why|why-not|impact <dir> ... --branch B       # read another branch
```

**Upgrade note**: P6 raises the IPC protocol to 4 and the op_log to format 3 (each frame records its branch and merge report). Stop `ctxd`, run `ctx migrate <dir>` once (it keeps the old log as `log.v2.bak`, can be rerun after an interruption, and lists any epoch whose recorded result changes under the Conflict gate), then upgrade every binary together. Older binaries refuse a format-3 store. Format 3 keeps its projections in `projections.v3.sqlite`; the old `projections.sqlite` stays as it was, so rolling back (restore `log.v2.bak` as `log`, before new commits) needs nothing else. A store that crashed rather than shut down must be opened once by the old binary before migrating.

## Semantic validation, conflicts and handoff (P7)

A rule with a semantic (`Sem`) premise stays Pending until someone checks it against the premises' current revisions. P7 adds validators that do this, contradiction tracking, and session handoff (SPEC SD20, ADR 0015).

- **Validators**: `<data dir>/validators.toml` lists them; **none is configured by default**, so nothing is called until you add one. When a Pending rule's semantic vector changes, the daemon queues one job per `(branch, rule, vector)` in `jobs.sqlite`, and workers run them with leases, retries and recovery after a crash. A `command` validator with `trusted = true` is deterministic and commits a ValidationRecord. A `claude` validator, or a command without `trusted`, counts as an LLM: it can **only** open a non-gating PendingReview that an admin accepts or rejects. An LLM verdict never writes a record (T50). `claude -p` runs sandboxed, with no tools, MCP servers, settings or hooks, in an empty working directory with a minimal environment. `token_file` is read only by path.
- **Manual validation**: `ctx validate` (admin socket) shows the current semantic vector and commits with a compare-and-set on it, so a premise that moved in between is refused (`CurvecMoved`). Agents cannot commit `RecordValidation` (`ChannelRestricted`).
- **Conflicts (SD20)**: `ctx_conflicts open` records that two revisions contradict. Both sides read `CONFLICTED` on that branch until `ctx_resolve` closes it or one side is superseded. Agents and admin may open them.
- **Candidates**: when a Decision, Plan or Fact gets a new head, a per-branch lexical index (SQLite FTS5, bm25) proposes up to `per_head` likely contradictions above `min_score`. They are listed, never gating, and can be opened as a Conflict. A validator with `pairs = true` also judges them, and a `conflict` answer opens a PendingReview.
- **Handoff**: `ctx_handoff_begin` packs the session's certified state; `ctx_handoff_accept` (exactly once, even under concurrent accepts or a crash) copies its read set into the acceptor's session. The acceptor's certificate turns Invalid as soon as a handed-over read changes.
- **Changes**: `ctx_changes` pages through status changes per epoch from the durable diff log and can long-poll for new ones (`ctx changes --follow`).

```toml
# <data dir>/validators.toml
[limits]                      # defaults
workers = 2
lease_secs = 120
attempts = 3
llm_calls_per_hour = 200

[candidates]
min_score = 1.0               # bm25 floor (kept at 1.0 after the G6 run)
per_head = 5

[[validator]]
name = "claude"
type = "claude"               # claude | command
model = "claude-haiku-5-5"
token_file = "~/.config/contextdb/claude-token"   # optional, passed by path
branches = ["main"]           # optional filters: branches, kinds, scopes
pairs = true                  # also judge candidate pairs

[[validator]]
name = "lint"
type = "command"
command = ["/usr/local/bin/ctx-lint", "--json"]
trusted = true                # deterministic: may commit a ValidationRecord
local = true                  # keeps data on this machine (P9 egress)
```

A job goes to the first validator whose filters all match. With an access document (P9), work that reads a lane whose `egress` is off (personal lanes by default) goes only to a validator marked `local = true`, your statement that the command keeps data on the machine (a `claude` validator cannot be marked); until one is configured, or the lane's egress turns on, it waits with no attempt counted. `ctx validators reload <dir>` applies an edited file; `check` runs every loaded validator once on a probe task (one LLM call each, within the hourly cap); `status` lists jobs, counters and Failed jobs.

**Upgrade note**: P7 raises the IPC protocol to 5 and the op_log to format 4 (a ValidationRecord can carry a metadata hash). Stop `ctxd`, run `ctx migrate <dir>` once (it keeps `log.v3.bak`; batches, hashes and frame checksums are unchanged), then upgrade every binary together.

## Embeddings (P8, optional)

Recall can add a third ranker that matches by meaning, next to keys and bm25 (ADR 0016 §7). It is **off by default**: without `<data dir>/embeddings.toml`, `ctx status` shows `vectors=disabled`, and recall uses keys and text only.

```toml
# <data dir>/embeddings.toml
[embedder]
command = ["/path/to/embed", "--model", "x"]  # JSON lines: {"id","text"} in, {"id","vector"} out
model = "x"                                    # the cache key
dims = 1024
max_batch = 32                                 # defaults
timeout_secs = 30

[vectors]                                      # optional; defaults
max_cache_mb = 256                             # in-memory vector cache
min_cosine = 0.82                              # embedding contradiction candidates
per_head = 5
```

- **Privacy.** The command receives record bodies, so configuring one is your decision. It runs in its own process group with a deadline, a minimal environment (`HOME`, `PATH`, `LANG`) and an empty private working directory.
- **Vectors.** They live in `vectors.sqlite`, keyed by content and model, so branches and reverted revisions share them. A background thread fills them and never blocks the writer or the text index. A failing embedder shows `vectors=failed` and is retried with back-off. `ctx reindex --vectors` drops other models' vectors and fills in what is missing.
- **Certification is unchanged.** The vector ranker, like the others, yields only eligible heads, so certified recall still returns only CERTIFIED records.
- **Candidates.** New Decision, Plan and Fact heads are also paired with their nearest neighbours on the branch (cosine ≥ `min_cosine`). A pair found by both sources is listed once, as "lexical and embedding similarity".
- `type = "mock"` (with `dims`, no `command`) is a hashed bag of words for trials. It is not a real embedder.
- **A local model.** `scripts/ollama-embed.py` (Python standard library only) adapts a local [Ollama](https://ollama.com) so bodies never leave the machine:
  ```toml
  [embedder]
  command = ["/usr/bin/env", "python3", "-I", "/path/to/ContextDB/scripts/ollama-embed.py", "--model", "nomic-embed-text"]
  model = "nomic-embed-text"
  dims = 768
  ```
  On this project's three dogfood stores (P8 G6, 120 queries labelled by two independent agents) hybrid ranking found slightly more of the relevant records than keys and bm25 alone (recall@10 0.82 vs 0.81). Cosine similarity is a weak contradiction signal: of 130 labelled neighbour pairs only 6 were contradictions, and no `min_cosine` separated them well, so the default stays 0.82.

## Git mirror (P8)

`ctx mirror export <dir>` writes the branch's live records as Markdown files, one commit per export, to `refs/contextdb/b<branch id>` in the project repository, so `git log`, `git diff` and `git grep` work on them. Some properties:

- **Read-only.** It is never read back. If someone edits the ref, the next export refuses with `MirrorModified`, `ctx mirror proposals` lists the edits as supersede proposals for an agent to apply through `ctx_supersede`, and `--force` moves the ref after keeping a backup.
- **Never pushed by ContextDB.** It contains every live record's full body. A plain `git push` leaves `refs/contextdb/*` alone, but `git push --mirror` and repository backup tools send it.
- **Concurrency-safe.** The ref moves only by compare-and-swap, so a concurrent export fails with `MirrorRaced` instead of overwriting.
- **After a shred** the next export refuses with `MirrorStale` (`PayloadGone`) while the ref's history holds the payload. `--rebuild` replaces the history with one fresh commit and deletes every backup ref. Git keeps the old objects until you run `git reflog expire --expire=now --all && git gc --prune=now`.

## Multi-user mode, egress and shred (P9)

Several OS users on one machine can share a store. Each user is a **principal** that sees only the lanes it may read, and payloads can be erased cryptographically. Multi-user mode is **off unless you turn it on**. Without an access document, a store works exactly as in P8.

**Setup** (the store owner, i.e. the user `ctxd` runs as):

1. Write `.contextdb/access.toml`. A minimal one:

   ```toml
   version = 1
   [principals.alice]          # the store owner: the uid ctxd runs as
   uid = 501
   [principals.bob]
   uid = 502

   [lanes.repo]
   members = ["*members"]      # every principal in the document
   writers = ["*members"]
   curators = ["alice"]
   egress = true               # bodies may leave the machine (validators, embedder, mirror)

   [default_lane]              # every lane not listed: owner only (fail closed)
   members = ["*owner"]
   ```

   Groups (`@billing`), dir grants, key-prefix denies, `code_lanes` and `personal:<name>` lanes are described in ADR 0017 §4.
2. Run `ctx access apply <data dir>` (it reads `.contextdb/access.toml`; `--file` names another), then check with `ctx access show <data dir>` and `ctx access check <data dir> <principal> <key>`.
3. Start the daemon with `ctxd <data dir> --multi-user`, or install it with `ctx service install <data dir> --multi-user`. Without the switch, a document that names other principals maps nobody.
4. Multi-user mode checks the file system before it maps anyone:
   - `<data>`, `<data>/run` and `<data>/run/shared` are 0711, and everything else is private;
   - there are no ACLs, and every entry is owned by the store owner;
   - each mapped uid can reach the shared socket.

   `ctx doctor <data dir> --fix-modes` repairs modes and ACLs.

**What other principals get.** Other users connect over the agent socket with MCP only: no hooks, outbox or observer (F8).
- A record they may not read is `Unavailable`, and lists drop it and say how many were dropped.
- Writes need `Write` on the lane, and asserting a base needs `Curate`.
- Sending a content hash needs its bytes (proof of possession).
- Branches belong to their creator. A merge into `main` applies only what the merger could do directly.
- `ctx ui` needs its launch token while others are mapped. Restart `ctx ui` after a `SetAccess` that maps someone new.

**Egress.** Once an access document exists, a lane with `egress = false` (personal lanes by default) keeps its bodies on the machine:
- only validators marked `local = true` receive them;
- the embedder skips them;
- the git mirror exports only lanes that are egress and readable by every principal.

**Shred.** `ctx shred <data dir> <key>[@rev] [--with-reviews] --yes` erases a payload. Payloads are encrypted, one key per content hash, so a shred:
- deletes the key from the key log;
- purges every projection (`recall`, `vectors`, `jobs`, `candidates`, with their `-wal` files);
- reports `purged` only when all of that is durable.

The git mirror then refuses with `PayloadGone` until `ctx mirror export --rebuild`. A shred does **not** reach the session log or outbox, text agents copied elsewhere, earlier backups or exports, swap, file-system snapshots, freed blocks or SSD remanence. `ctx shred` prints this list.

**Upgrading.** A store from P8 or earlier must be migrated once, with `ctxd` stopped: `ctx migrate <data dir>`.
- It re-encrypts the packs, rebuilds the projections without old free pages, and fixes modes.
- It can be rerun after an interruption.
- Until it completes, `ctxd`, `ctx` and `doctor` refuse the store (`MigrationIncomplete`).

**Backups.** Back up `<data>/keys/` together with `blobs/` and `log/`: without the key log every payload is unreadable. A shredded payload stays readable in any backup taken before the shred.

**Threat model, in short.**
- Other OS users cannot read the store's files or impersonate the daemon.
- Mapped principals see and change only what the access document allows.
- Agents running as the **store owner's** uid are still trusted cooperatively, as before.
- ADR 0017 §2 lists the guarantees and their limits: count side channels, key existence, and confirming a known text by its hash.
