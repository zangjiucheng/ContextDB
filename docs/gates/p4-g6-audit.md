# P4 G6 dogfood: audit, labelling and closure

Date: 2026-11-02. Reviewer: Claude Code (Opus 5.5, `claude-opus-5-5`) as the maintenance agent, with two independent blind reviewers (Claude subagents) for the labelling check. Report: [`p4-g6-dogfood-report.md`](p4-g6-dogfood-report.md) (`ctx gate-report P4`, ctx 0.1.3, the binary that served the dogfood). Labels: [`p4-g6/review.jsonl`](p4-g6/review.jsonl). Rules: [`p4-g6/label.py`](p4-g6/label.py).

**Outcome.** The owner closed the dogfood on 2026-11-02 at 11.8 days instead of the planned ≥ 14 (SPEC v1.6.12). The audit below shows why the extra days would have added no information: real use stopped on 10-24. The measured results are:
- **Coverage passes:** 69.6%.
- **Enforce everywhere is not recommended:** false-deny is 30.6%.
- **Enforce for file edits is recommendable:** with `mode = "enforce"` and `shell_mode = "warn"`, false-deny is 0.11% (5/4,475).
- **Warn stays the default.**

## 1. Data

Telemetry was read from the running dogfood daemon (protocol 3) by a v0.1.3 `ctx` with one added read-only subcommand that prints every `TelemetryEntry` with its id. It was built from `git archive v0.1.3` outside the repository. Parsing is in [`p4-g6/parse.py`](p4-g6/parse.py). Neither `.contextdb/bin` nor the store was modified; labels went through `ctx dogfood review`, the intended channel.

| session | harness | first | last | gated | Valid | Empty | Invalid | unavailable |
|---|---|---|---|---|---|---|---|---|
| 07ef1c45 | Claude Code | 10-21 20:05 | 10-21 20:05 | 1 | 0 | 1 | 0 | 0 |
| 1f0ab464 | Claude Code | 10-21 20:08 | 10-21 21:20 | 546 | 521 | 25 | 0 | 0 |
| 2535fe88 | Claude Code | 10-21 20:11 | 10-22 15:11 | 376 | 374 | 1 | 0 | 1 |
| 6bf86cd2 | Claude Code | 10-22 13:15 | 10-22 14:57 | 168 | 0 | 168 | 0 | 0 |
| bf8e2546 | Claude Code | 10-22 15:14 | 10-22 22:56 | 248 | 238 | 10 | 0 | 0 |
| 47d3814a | Claude Code | 10-22 23:01 | 10-24 11:24 | 1401 | 1328 | 64 | 3 | 6 |
| 1a6aac24 | Codex | 10-22 23:32 | 10-23 00:03 | 165 | 159 | 6 | 0 | 0 |
| a9fe2545 | Codex | 10-23 13:22 | 10-24 11:44 | 114 | 52 | 0 | 61 | 1 |
| 6127a581 | Claude Code | 10-24 12:09 | 10-26 22:59 | 1083 | 0 | 1073 | 0 | 10 |
| 7b041010 | Claude Code | 10-27 20:23 | 10-28 10:55 | 182 | 0 | 180 | 0 | 2 |
| d3b3d9e5 | Claude Code | 10-31 11:28 | 10-31 12:29 | 138 | 0 | 138 | 0 | 0 |
| 609e5afb | Claude Code | 11-02 12:50 | 11-02 13:13 | 53 | 0 | 53 | 0 | 0 |

(Valid includes 2 ValidRecertified.)

## 2. Findings

1. **Use stopped after 10-24.** Every session from 10-24 on has zero certified reads: about 1,450 actions, all Empty. The last write is epoch 23 on 10-23; no later decision was recorded in this store, though there were many (P6, the v1.6.11 fixes, ADR 0014). The two harnesses ran in parallel only between 10-22 and 10-24. Running to 11-04 would have added Empty warnings only.
2. **Coverage, 69.6% (16/23), passes on a small sample.**
   - 15 of the 23 writes are the 10-21 seed; 6 of those are root decisions with nothing to depend on.
   - The 8 organic writes all carry deps.
3. **The cross-agent intercept worked, and its warnings were ignored.**
   - **What happened:** Codex session a9fe2545 read the P5b decisions at 10-23 13:22. At 15:08 Claude session 47d3814a superseded `p5b/symbol-contract`. On 10-24 Codex resumed P5b G2 work against the old contract: it rewrote `P5b.md` and `p5b-g2.md`, then added, committed and pushed.
   - **What ContextDB did:** it warned on all 61 actions, and Codex never re-read. Only 5 of the 61 change the repository. The other 56 (`git status`, `cat`, tests) repeat the same warning to no effect.
   - **The 3 Claude Invalid warnings are false alarms:**
     - two came right after the session's own supersede (writing its private notes about it);
     - one is a read, recertified 7 s later.
4. **Enforce would deny ordinary work.** In enforce every warn is a deny. 1,370 of 4,475 gated actions (30.6%) would have been denied wrongly:
   - 1,307 Empty and 58 Invalid shell commands that change nothing in the repository;
   - 5 writes outside it.
   The Empty part alone is 29.3%. Invalid alone is 1.3%: Invalid only, without Empty, would still be under 2%.

   The existing `shell_mode` setting already separates these cases. With `mode = "enforce"` and `shell_mode = "warn"`, only the 60 flagged file edits (Edit, Write, Patch) would have been denied. 55 of those were right to flag; 5 were writes outside the repository (Claude's private memory notes). That is a false-deny rate of 0.11% (5/4,475).

   Shell commands stay warn-only, as SPEC §10 already expects: a shell side effect can only be detected after the fact.
5. **Drift counts are inflated.** 21,270 "undeclared" entries cover about 200 distinct paths. Each Stop re-reports the whole dirty tree instead of what changed in that turn.
6. **Claude Code's harness version is never recorded.** 4,177 entries say `(unknown)`; only Codex writes its version.
7. **The daemon was unavailable 20 times (0.45%),** at the 150 ms budget. v1.6.11 raised the default to 500 ms.
8. **MCP calls without a claim: 0.** Claude Code's `_meta` key (`claudecode/toolUseId`) is confirmed on live traffic, the check G2 left to the dogfood.
9. **The telemetry keeps full shell commands,** including remote host names and heredoc bodies (as README states). This audit quotes no commands; the labels file holds only entry ids.

## 3. Labelling

Every warn (1,783) and every sampled allow (126) is labelled by rule.

**The question per entry (`ctx dogfood review`):** for a warn, was ContextDB right to flag it? For an allow, was allowing it right?

**The rules ([`label.py`](p4-g6/label.py), r2):**
- **A sampled allow is right.** Its certificate was Valid.
- **A warn on a file write** is right inside the repository and a false alarm outside it.
- **A warn on a shell command** is right only if the command changes tracked repository work:
  - git add, commit, push, rm, mv, reset, checkout, apply, stash, merge, rebase or tag;
  - `sed -i`, or `cargo fmt`/`rustfmt` without `--check`;
  - a redirect, `tee`, copy or move, or a Python write whose target is a repository path, run in the repository.
  Anything else is a false alarm: reads, tests, builds, logs written to temporary directories, and work in other directories.

**Blind checks.** Neither reviewer saw the rule labels.

| Round | Sample | Rules | Agreement |
|---|---|---|---|
| 1 | 163, stratified by verdict and rule outcome, including all 64 Invalid | r1 | 153/163 (93.9%) |
| 1, rechecked | same 163 | r2 | 162/163 (in sample; the remaining entry was truncated for the reviewer, and the rule is right) |
| 2 | 83, fresh, no overlap with round 1 | r2, frozen before the round | **79/83 (95.2%)** |

- **Round 1 → r2.** All 10 r1 disagreements were the rule calling an action a repository change when it only wrote logs to temporary directories or worked in another repository. r2 tracks `cd` per segment and separates heredoc bodies from shell syntax.
- **Round 2.** All 4 disagreements are the rule saying "right to flag" where the reviewer said "false alarm". The rules therefore understate the false-deny rate, if anything; the enforce conclusion does not depend on them.

## 4. Gate result

| Gate | Threshold | Measured | Result |
|---|---|---|---|
| Period | ≥ 14 days | 11.8 days; parallel use 10-22 – 10-24 | **deviation, owner decision** (SPEC v1.6.12) |
| Parallel agents | ≥ 2 | 12 sessions; Claude Code and Codex in parallel | Pass |
| Dependency coverage | ≥ 60% | 69.6% (16/23; 8/8 organic) | Pass (small sample) |
| False-deny rate | ≤ 2% to recommend `enforce` | 30.6% (1,370/4,475) with shell enforced; 0.11% (5/4,475) with `shell_mode = "warn"` | enforce for shell **not recommended**; enforce for file edits (`shell_mode = "warn"`) meets the threshold; the default stays warn |
| Stale-action intercepts | report only | 64; 5 change the repository (7.8%) | reported |
| Legitimate allow rate | report only | 126/126 sampled | reported |

## 5. Follow-ups (not P4 blockers)

1. **Adoption.** Agents stop reading certified context once the warning becomes background noise. v1.6.11 reduces the Empty warning to once per session and agent. What is still missing is a reason to read: for example, a briefing that names the decisions relevant to the files being touched (P5a anchors make that possible).
2. **Repeated Invalid warnings.** Like Empty before v1.6.11, Invalid repeats on every action. It should be said once per stale read, and again when the agent changes the repository. **Done** (ADR 0010 r3.5): once per verdict, named reads and git HEAD; file edits and denies always.
3. **Gate file edits, warn on shell.** Document `mode = "enforce"`, `shell_mode = "warn"` as the recommended stricter setting. A further step would skip writes outside the repository: those are the 5 remaining false alarms.
4. **Drift should report per-turn deltas,** not the whole dirty tree each Stop. **Done** (r3.5): only new paths are shown, and the report counts paths once per session.
5. **Record Claude Code's harness version** in telemetry. **Done** (r3.5): read from the session transcript.
