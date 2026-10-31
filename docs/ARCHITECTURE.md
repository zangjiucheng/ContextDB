# Architecture

This page shows how ContextDB's processes, crates and files fit together.
The normative semantics are in [SPEC.md](../SPEC.md). Each design decision
has an ADR under [docs/adr/](adr/), cited below as "ADR nnnn".

## 1. System context

Agents work through their harness. A harness reaches ContextDB through two
paths:

- **MCP tools** (`ctx-mcp`), to read and write context;
- **hooks** (`ctx-hook`), so that edits are checked and recorded without the
  agent having to ask.

A trusted observer reports code changes. One `ctxd` per data directory is
the only writer.

```mermaid
flowchart LR
  subgraph Harnesses
    CC["Claude Code<br/>(+ subagents)"]
    CX["Codex CLI<br/>(+ subagent threads)"]
    OC["OpenCode 2.0<br/>(+ child sessions)"]
  end
  subgraph Clients["Per-call client processes"]
    MCP["ctx-mcp<br/>MCP stdio server"]
    HOOK["ctx-hook<br/>hook adapter"]
    PLUG["OpenCode plugin<br/>.opencode/plugins/contextdb"]
  end
  subgraph Observer["Trusted observer"]
    GH["git hooks<br/>post-commit / checkout / merge"]
    WATCH["ctx anchors watch / scan"]
  end
  CTX["ctx CLI<br/>admin, setup, ui"]
  CTXD[("ctxd<br/>one per data dir")]
  DATA[("data dir<br/>op_log · blobs · session log · projections")]

  CC -- "MCP tools" --> MCP
  CX -- "MCP tools" --> MCP
  OC -- "MCP tools (code mode)" --> MCP
  CC -- "command hooks" --> HOOK
  CX -- "command hooks" --> HOOK
  OC -- "plugin hooks" --> PLUG
  PLUG -- "Claude-shaped stdin" --> HOOK
  MCP -- agent.sock --> CTXD
  HOOK -- agent.sock --> CTXD
  HOOK -. "outbox when ctxd is down" .-> DATA
  GH --> WATCH
  WATCH -- observer.sock --> CTXD
  CTX -- admin.sock --> CTXD
  CTX -. "ctx ui: read-only op_log replay" .-> DATA
  CTXD --> DATA
```

## 2. Processes, sockets and channels

The socket a request arrives on decides its **channel**; the request never
claims an identity. Each channel may submit only its own kinds of writes
(SPEC §2.5 A7, ADR 0008 §2):

- only the observer channel writes code anchors and watermarks;
- only the admin channel records human validations and policy.

```mermaid
flowchart TB
  subgraph ctxd["ctxd process"]
    direction TB
    A["agent.sock<br/>channel Agent"]
    O["observer.sock<br/>channel Observer"]
    D["admin.sock<br/>channel Admin"]
    SVC["service<br/>sessions · certificates · gating · anchors routing"]
    ACT["commit actor<br/>single writer, group commit"]
    SNAP["published Snapshot<br/>epoch-pinned, arc-swap"]
    A --> SVC
    O --> SVC
    D --> SVC
    SVC -- "writes" --> ACT
    SVC -- "reads" --> SNAP
    ACT -- "publish per group" --> SNAP
  end
  LEASE["ctxd.lease<br/>pid · protocol · sockets"]
  LOCK["LOCK (flock)<br/>single instance"]
  ctxd --- LEASE
  ctxd --- LOCK
  SVC_MGR["launchd / systemd --user<br/>ctx service"] -- "starts, restarts on failure" --> ctxd
```

- **Discovery.** Clients find `ctxd` through `ctxd.lease` and refuse a
  protocol mismatch; all four binaries ship from one build.
- **Service.** `ctx setup` installs the per-repository login service. A
  `ctxd` started by hand is left alone.

## 3. Crates

Product crates, with arrows pointing at dependencies. `contextdb-model` is
the dependency-free vocabulary (ids, the Ops, wire encoding).
`contextdb-oracle` is an independent reference implementation that core
uses only for its paranoid self-checks.

```mermaid
flowchart BT
  model["contextdb-model<br/>ids · 14 Ops · wire"]
  oracle["contextdb-oracle<br/>InputModel · Jacobi · Kahn"]
  core["contextdb-core<br/>kernel · branches · certificates"]
  store["contextdb-store<br/>op_log · blobs · projections · session log"]
  proto["contextdb-proto<br/>IPC · lease · client · gate · outbox"]
  anchors["contextdb-anchors<br/>file observer: Git OID capture, classify"]
  symbols["contextdb-symbols<br/>tree-sitter extraction, normaliser"]
  daemon["contextdb-daemon<br/>ctxd"]
  mcp["contextdb-mcp<br/>ctx-mcp"]
  hooks["contextdb-hooks<br/>ctx-hook"]
  cli["contextdb-cli<br/>ctx"]

  oracle --> model
  core --> model
  core --> oracle
  store --> core
  proto --> core
  anchors --> proto
  symbols --> anchors
  daemon --> store
  daemon --> proto
  daemon --> anchors
  mcp --> daemon
  mcp --> proto
  hooks --> proto
  cli --> daemon
  cli --> store
  cli --> symbols
```

Two splits are deliberate:

- **`ctxd` never links `contextdb-symbols`.** The daemon's live path never
  parses source (ADR 0012); symbol hashing happens in the observer.
- **`ctx-hook` links only `proto` and `model`.** It stays small and fast:
  it runs on every tool call.

Verification crates (`testkit`, `harness`, `workload`, `bench`) and the
separate `contextdb-dd` research workspace are left out.

## 4. Inside ctxd: one write

Writes go through a single actor. It commits queued writes as a group:

1. append every batch's frame to the op_log with **one** sync;
2. only then publish the new epoch's snapshot;
3. only then acknowledge.

Readers hold an epoch-pinned snapshot and never see half a group (ADR 0006,
ADR 0008 §3).

```mermaid
sequenceDiagram
  autonumber
  participant C as client (ctx-mcp / ctx-hook / observer)
  participant S as service
  participant A as commit actor
  participant St as store
  participant K as kernel (core)
  participant P as published snapshot
  C->>S: Request (Record, Commit, Merge, ...)
  S->>S: resolve session / binding, deps, channel rules
  S->>A: WriteRequest (batch + blobs + client_txn_id)
  A->>St: group of queued writes
  St->>St: idempotency (client_txn_id) and capacity checks
  St->>K: apply batch: staging, admission, cycle check, maintenance
  K-->>St: CommitResult (epoch, allocations, status_diff)
  St->>St: blobs to pack, frame to op_log (hash chain), one sync per group
  St-->>A: committed group
  A->>P: publish Snapshot(epoch) via arc-swap
  A-->>S: results
  S-->>C: Committed / error
```

A failed sync poisons the store until a fresh process opens it, so an
acknowledged write is always durable. SQLite projections and the FTS recall
index are updated after the log. They can be rebuilt from it and are never
read past their watermark.

## 5. The data directory

```mermaid
flowchart LR
  subgraph dir["&lt;data dir&gt; (.contextdb/db)"]
    LOG["log/seg-*.log<br/>op_log: checksummed, hash-chained frames<br/>THE source of truth"]
    BLOBS["blobs/pack-*<br/>self-verifying payload records"]
    SESS["sessions/seg-*.log<br/>session log: sessions, read ledgers,<br/>certificates, hook facts, telemetry"]
    PROJ["projections(.v3).sqlite<br/>diff_log · head/base history · branches"]
    REC["recall.sqlite<br/>FTS5 over key, kind, scope, payload"]
    OUT["outbox/*<br/>per hook stream, at-least-once facts"]
    RUN["run/*.sock · ctxd.lease · LOCK"]
    SNAPF["snapshots<br/>input-layer accelerators"]
  end
  LOG -- "replay rebuilds" --> PROJ
  LOG -- "replay rebuilds" --> REC
  LOG -- "replay checked against" --> SNAPF
  OUT -- "delivered idempotently per (stream, seq)" --> SESS
```

- **Source of truth.** Only `log/` defines kernel state.
- **Never feeds the kernel.** The session log is a second hash-chained log
  family for agent-side records.
- **Rebuildable.** Everything in SQLite can be dropped and rebuilt
  (`ctx doctor --rebuild`).
- **Format changes.** A new op_log format is explicit: an older store is
  refused until `ctx migrate` rewrites it and keeps a backup.

## 6. The kernel

`Kernel` holds the branch table, the global key namespace and the global id
counters. Every branch is an ordinary single-branch `Db` that holds only the
objects visible in it, under local ids. Ids are mapped at the boundary
(SD19, ADR 0013).

Inside a `Db`, a batch goes through these steps:

1. it is staged;
2. it is admitted: CAS, the kind admission matrix, the rule canonical form,
   policy;
3. it is checked for cycles on an incrementally maintained topological order;
4. the derived state is maintained incrementally:
   - support counts and witnesses (L0);
   - the L1/L2 certification columns;
   - semantic gates.

```mermaid
flowchart TB
  subgraph K["Kernel"]
    BT["branch table · sync points<br/>global keys and ids"]
    MAIN["main: Db"]
    BR["branch b: Db (copy-on-write)"]
    MERGE["kernel_merge<br/>merge base by containment, 3-way,<br/>Conflict artifacts, certify"]
    WI["whatif<br/>undo-journal dry run"]
  end
  subgraph DB["Db (one branch)"]
    STAGE["stage / txn<br/>CAS · admission (§2.5) · canonical rules"]
    PK["PearceKelly<br/>incremental topological order, cycle check"]
    CNT["Counting engine<br/>unsat · support · witness (L0)"]
    LAY["LayerColumns<br/>L1 / L2 · semantic gate · stale reason"]
    Q["queries<br/>why · why_not · impact · diff · certificates"]
  end
  BV["BranchView<br/>global-id reads"]
  ORA["contextdb-oracle<br/>InputModel + Jacobi / Kahn"]
  BT --- MAIN
  BT --- BR
  MERGE --> MAIN
  STAGE --> PK --> CNT --> LAY
  LAY --> Q
  MAIN -.-> DB
  BV --> Q
  ORA -. "exact comparison: property tests,<br/>paranoid builds, store open self-check" .-> CNT
```

- **Rules.** A rule's conclusion holds when the rule is enabled, has at
  least one premise, and every premise literal holds (SD3).
- **Reference-only rules.** A rule with only `Ref` incidences records
  references and never supports its conclusion (SPEC v1.6.11).
- **Pure state.** Derived state is a pure function of committed inputs.
- **Two independent checks.** Two oracles, plus a full-recompute engine,
  check the incremental engine exactly.

## 7. Hooks and gating

Every harness turns a tool call into the same check. The hook asks `ctxd`
to validate the session's certificate (the reads it relied on) for this
action:

- `warn` explains which reads changed;
- `enforce` denies.

Facts that must not be lost go through the outbox when `ctxd` is
unreachable (ADR 0010, ADR 0014).

```mermaid
sequenceDiagram
  autonumber
  participant H as harness
  participant P as OpenCode plugin (OpenCode only)
  participant K as ctx-hook (harness)
  participant D as ctxd (agent.sock)
  participant OB as outbox
  H->>P: tool.hook("execute.before")
  P->>K: pre-tool-use, Claude-shaped stdin
  Note over H,K: Claude Code and Codex run ctx-hook directly
  K->>D: SessionAttach + Check(action, mode)
  alt ctxd answers within timeout_ms (500 ms default)
    D-->>K: Validation + Decision (allow / warn / deny)
  else unavailable
    K->>OB: append Gate fact
    K-->>H: notice, or deny when fail-closed
  end
  K-->>P: hookSpecificOutput (deny reason / additionalContext)
  P-->>H: throw (deny) or append the warning to the tool result
  H->>K: post-tool-use (after the edit)
  K->>OB: append Action fact
  OB->>D: Facts(stream, seq...) delivered at least once
```

- **Binding a session:**
  - hook events bind by the harness `session_id`;
  - Codex MCP calls bind by the `sessionId`/`threadId` in `_meta`;
  - OpenCode MCP calls bind by `_meta["ai.opencode/sessionID"]`;
  - Claude Code MCP calls are claimed through the tool-use id the
    PreToolUse hook saw.
- **Getting context to the agent.** The briefing (SessionStart) and the
  changed reads (UserPromptSubmit) are injected as context. OpenCode has no
  context hook, so its plugin adds them to the system prompt of every model
  request instead.

## 8. Code anchors

A code anchor makes a file, or a symbol inside a file, a record. Only the
trusted observer channel writes anchors. An observer submits its captures
and classifications for a scope as one batch, together with a CAS on the
scope's `ObserverWatermark`.

Freshness is decided at read time: the committed watermark is compared with
the live position. A lagging observer makes dependents
`PENDING(ObservationLag)`, never silently certified (SD14b, SD18,
ADR 0011/0012).

```mermaid
flowchart LR
  subgraph repo["Git working tree"]
    F["files"]
    G["HEAD + index"]
  end
  subgraph obs["observer (ctx anchors scan / watch, git hooks)"]
    CAP["capture<br/>Git blob OID of raw bytes"]
    SYM["contextdb-symbols<br/>tree-sitter spans, normaliser walk 4,<br/>same-file context hash"]
    CLS["classify<br/>Present · Missing · Broken · moved"]
  end
  subgraph ctxd2["ctxd"]
    REG["anchor registry<br/>bounded, trusted routing"]
    WM["ObserverWatermark CAS"]
    LIVE["live position<br/>sampled on read"]
  end
  F --> CAP
  G --> CAP
  CAP --> SYM --> CLS
  CAP --> CLS
  CLS -- "observer.sock: one batch per scope" --> REG
  REG --> WM
  LIVE -- "read-time comparison" --> WM
  WM -- "Fresh → CERTIFIED / Lagging → PENDING" --> DEP["dependent decisions and plans"]
```

## 9. Where to read more

| Topic | Document |
|---|---|
| Semantics, gates, the phase plan | [SPEC.md](../SPEC.md), [SEMANTICS.md](SEMANTICS.md) |
| Durable log, recovery, snapshots | ADR 0006, ADR 0007 |
| Daemon, sessions, certificates | ADR 0008, ADR 0009 |
| Hooks and gating | ADR 0010; OpenCode: ADR 0014 |
| File and symbol anchors | ADR 0011, ADR 0012 |
| Branches, transactions, WHAT_IF | ADR 0013 |
| Phase status | [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md), [gates/](gates/) |
