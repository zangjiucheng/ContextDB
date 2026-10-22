# P4 G6 dogfood kit

SPEC §5 P4 G6 requires real use of ContextDB on this repository: **≥ 2 parallel agents for ≥ 2 weeks**. The report then gives the false-deny rate (threshold ≤ 2% to recommend `enforce`) and the dependency coverage (threshold ≥ 60%). Maintenance agents run, label and report this (SPEC v1.6.4 §4.4 supersedes the owner-run workflow in ADR 0010 §0.1); P4 is marked Passed only with the resulting report.

## 1. Build and start

```sh
cargo build --release -p contextdb-daemon -p contextdb-mcp -p contextdb-hooks -p contextdb-cli
export PATH="$PWD/target/release:$PATH"
DATA="$PWD/.contextdb/db"            # inside the repo; add .contextdb/db/ to .git/info/exclude
mkdir -p "$DATA" && ctxd "$DATA" &   # keep it running for the whole period
```

Start from an empty data directory. Outbox records and session logs written by pre-release P4 builds are not compatible, so remove a data directory created by earlier development builds.

## 2. Configure gating and policy (committed to the repository)

`.contextdb/gating.toml`: start in `warn`. The false-deny rate is measured on the actions that would have been flagged.

```toml
mode = "warn"
on_unavailable = "open"
timeout_ms = 150
```

`.contextdb/policy.toml` (optional; enforced in the kernel):

```toml
version = 1
require_deps = ["Decision", "Plan"]
```

Apply them:

```sh
ctx gating apply "$DATA"
ctx policy apply "$DATA"           # only if you use a policy
```

## 3. Register the MCP server and install the hooks

The MCP server must be named `contextdb`, because the Claude claim hook matches `mcp__contextdb__…`. Using another name needs `--mcp-name`.

Claude Code:

```sh
claude mcp add contextdb -- ctx-mcp "$DATA"
ctx hooks install --harness claude --project "$PWD" --dir "$DATA"
```

Codex (`~/.codex/config.toml` or the project's `.codex/config.toml`):

```toml
[mcp_servers.contextdb]
command = "ctx-mcp"
args = ["/absolute/path/to/repo/.contextdb/db"]
```

```sh
ctx hooks install --harness codex --project "$PWD" --dir "$DATA"
```

Then open `/hooks` in Codex (or accept the startup review) to trust the new hooks. Codex skips hooks it has not trusted.

## 4. Work

- Use at least two agents in parallel on this repository, for at least two weeks. Claude Code and Codex both count.
- Have the agents read the decisions they rely on with `ctx_recall` / `ctx_read` (certified), and record decisions and plans with dependencies (`ctx_record` with `deps`). Coverage counts writes that carry dependencies.
- If the daemon is restarted, nothing is lost. Hooks keep facts in the outbox, and `ctxd` delivers them at start; `ctx hooks flush "$DATA"` delivers them at once.

## 5. Agent review (weekly, and at the end)

```sh
ctx dogfood review "$DATA"
```

A reviewing agent examines each action against its recorded dependencies, certificate and relevant implementation evidence, then supplies the per-entry answers. No human review is required. Keep a companion provenance record with reviewer/model/version when available, rationale and evidence hashes; do not infer correctness just from the gate verdict. If evidence is missing, skip and report it as unreviewed. The CLI shows every warn or deny, and 1 in 20 allowed actions:
- For a warn or deny, answer `y` if ContextDB was right to flag it and `n` if it was a false alarm.
- For an allow, answer `y` if allowing it was right.
- `s` skips an entry and `q` stops.

The false-deny rate needs every warn and deny labelled; unanswered entries remain incomplete, not correct by default. The report identifies these as agent judgments, not human ground truth.

## 6. Report

```sh
ctx gate-report P4 "$DATA" --out docs/gates/p4-dogfood.md
```

This writes the Markdown report and `p4-dogfood.json`:
- period, parallel sessions, false-deny rate and coverage, each with PASS/FAIL;
- intercepts, drift, unclaimed MCP calls and outbox skips;
- verdict and harness-version breakdowns.

If coverage is below 60%, SPEC says to improve automatic dependency inference (O8) before adding features. If the false-deny rate is above 2%, `enforce` is not recommended.

## What to watch for

- `MCP calls without a claim` is also the live check of Claude Code's `_meta` key (T41b pins the keys from source, not from captured traffic). Codex calls bind by thread id and are never counted as unclaimed; for Codex, check that its MCP reads appear in the Codex sessions' checks. It must stay near zero; a high count means MCP reads are not reaching the hooks' session. Check the server name in the claim matcher (step 3), and whether a harness upgrade changed its `_meta` keys.
- Many `Empty` warnings mean the agents act without certified reads.
- Drift `undeclared` lists files changed with no recorded edit. These are mostly Bash side effects, and also edits by people. The count is an upper bound.
