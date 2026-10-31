# ADR 0014 — OpenCode 2.0 as a third harness

Status: **accepted r0.1**, 2026-10-31. The owner asked for full OpenCode
support. Everything below about OpenCode was checked against OpenCode 2.0.32
(the installed build): its plugin documentation, the strings of its binary,
and a logging probe plugin run under `opencode serve` and `opencode run`.
None of it relies on memory of OpenCode 1.x.

This is outside the P4 gate. P4's dogfood pre-registered Claude Code and
Codex (owner decision, 2026-10-21), and its report reads this repository's
store, which no OpenCode session writes to.

## 1. What OpenCode 2.0 offers

- **Plugins, not hook commands.** A plugin default-exports
  `{ id, setup(ctx) }`; a directory under `.opencode/plugins/` loads by
  itself. The 1.x `(input, output)` hooks (`tool.execute.before`, …) are
  gone: the 2.0 binary has no such strings, and `setup` receives
  `ctx.tool.hook`, `ctx.session.hook`, `ctx.permission.hook` and
  `ctx.event.subscribe` instead.
- **`ctx.tool.hook("execute.before")`** gets
  `{tool, sessionID, agent, messageID, id, input}`. Throwing fails the tool
  call with the error's message. **`"execute.after"`** adds `status` and,
  when completed, a mutable `result{output, content[], metadata}`.
- **`ctx.session.hook("context")`** runs before every model request, with
  `sessionID`, `agent` and a mutable `system` array of `{type: "text", text}`.
  `"prompt"` runs once per user message. `"compaction"` runs before a
  compaction.
- **Events** arrive as `{id, created, type, location, data}`.
  `data.sessionID` names the session. The ones used here are
  `session.execution.succeeded`/`failed` (end of a turn) and
  `session.deleted`. `session.created` carries `parentID` for a subagent's
  child session.
- **Built-in tools:**
  - `edit {path, oldString, newString, replaceAll}`
  - `write {path, content}`
  - `patch {patchText}` (the `*** Begin Patch` format Codex uses)
  - `shell {command, workdir, timeout}`
- **Code mode.** These four tools are top-level (`codemode: false`). MCP
  tools and OpenCode's own namespace are reached only through the `execute`
  tool's code mode, so a hook sees `execute`, not the MCP call.
- **MCP calls carry the session.** `tools/call` carries
  `_meta: {"ai.opencode/sessionID": <session id>}`.

## 2. Session binding

- **Hook events** bind by `session_id` = the OpenCode session id, as for
  Claude Code and Codex.
- **MCP calls** bind by `_meta["ai.opencode/sessionID"]` to
  `Binding::Key{harness: "opencode", harness_session, agent: None}`, the same
  session. No claim is needed: unlike Claude Code, OpenCode names the session
  itself.
- **A subagent runs in a child session** with its own id. Its tool calls and
  MCP calls carry that id, so it is its own ContextDB session. Its reads
  therefore do not join its parent's certificate. That is more conservative
  than Claude Code's agent views (more warnings, never a false allow).
  Joining child sessions to their parent would need a daemon-side alias; it
  is left until it is needed.

## 3. The plugin

- **Installation.** `ctx hooks install --harness opencode` writes
  `.opencode/plugins/contextdb/index.js`, or the same path under
  `~/.config/opencode` without `--project`. The file is generated from
  `crates/contextdb-cli/assets/opencode/plugin.js` with the `ctx-hook` path
  filled in. Its first line marks it as ContextDB's, so `install` never
  overwrites, and `uninstall` never removes, someone else's plugin of that
  name. `opencode --version` is saved as `opencode.version` in the data
  directory: the hook reports it as the harness version, as with Codex.
- **How it talks to `ctx-hook`.** The plugin turns each event into the
  Claude-shaped stdin `ctx-hook` already parses: `session_id`, `cwd` (the
  plugin's location directory), `hook_event_name`, `tool_name`, `tool_input`,
  `tool_use_id`. It runs `ctx-hook opencode <event>` and reads the
  Claude-shaped answer.

| OpenCode | `ctx-hook` event | the answer goes to |
|---|---|---|
| first `prompt`/`context` of a session in this OpenCode process | `session-start` | the briefing, kept for that session |
| `session.hook("prompt")` | `user-prompt-submit` | this turn's context |
| every `session.hook("context")` | — | `system` gets the briefing and this turn's context |
| `tool.hook("execute.before")` | `pre-tool-use` | deny → throw with the reason; warn → appended to that tool's result |
| `tool.hook("execute.after")` | `post-tool-use`, not awaited | — |
| `session.hook("compaction")` | `pre-compact` | the next turn's context |
| `session.execution.succeeded`/`failed` | `stop` | the next turn's context (drift) |
| `session.deleted` | `session-end` | — |

- **Classifying tool calls.** `ctx-hook` (adapter `Harness::OpenCode`)
  classifies the four built-in tools as Edit, Write, Patch and Shell. It also
  accepts 1.x `filePath`. Everything else is not gated and not recorded,
  including `execute`, `read` and `subagent`.
- **Gating.** It is the same certificate check, warn and enforce, as for
  the other harnesses (T38 now runs for all three). Paths are normalised
  against the repository as before.
- **Where notices go.** OpenCode has no channel for a user-only notice.
  `systemMessage` text (unavailable daemon, drift) goes to the model with the
  rest of the context.

## 4. Setup

`ctx setup --harness opencode` (or `all`; `both` still means Claude Code and
Codex):

- adds `mcp.contextdb = {type: "local", command: [ctx-mcp, dir], enabled}` to
  the project's `opencode.json`;
- installs the plugin;
- adds both files to `.git/info/exclude`, since they hold machine paths.

An `opencode.jsonc` is never rewritten, because comments would be lost; the
entry to add is printed instead.

## 5. Limits

- OpenCode 2.0 only. 1.x plugins used the hooks 2.0 removed.
- The briefing is held per OpenCode process. After a restart, the first
  request runs `session-start` again; `ctx-hook` keeps the baseline taken the
  first time.
- An edit made from code mode cannot happen: `edit`, `write`, `patch` and
  `shell` are not code-mode tools.
- Subagent sessions are separate ContextDB sessions (§2).

## 6. Verification

- **Adapter.** Unit tests cover the four tools, `filePath`, and that
  `execute`/`read`/`subagent` and Claude names stay ungated.
- **MCP binding.** The `ai.opencode/sessionID` binding has a unit test.
- **End to end.** T38 runs the real `ctx-hook opencode` against a real
  `ctxd`: a fresh read prints nothing, a stale read warns, enforce denies
  with a reason of at most 480 bytes, and a re-read clears it.
- **Plugin under node** (`tests/opencode_plugin.rs`), with a fake OpenCode
  context and a fake `ctx-hook`:
  - deny throws;
  - a warning follows the tool's output;
  - the briefing and the turn's update reach `system`;
  - a drift report at the end of a turn reaches the next one;
  - `session.deleted` ends the session;
  - stdin is Claude-shaped.
- **Install.** Install, reinstall, uninstall, and refusing a foreign file
  are tested. The `opencode.json` merge and the JSONC refusal have unit
  tests.
- **Real OpenCode 2.0.32 in a scratch repository.** After
  `ctx setup --harness opencode`, the model quoted the ContextDB briefing
  from its system prompt, and `ctxd` holds the `opencode` stream with its
  baseline.
- **Not yet run on real OpenCode:**
  - a gated edit, because it needs OpenCode's permission prompt answered;
  - an MCP call through code mode, for the same reason.

  The plugin test and T38 cover their mapping.
