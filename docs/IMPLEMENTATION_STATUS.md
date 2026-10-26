# Implementation status

**P6 branches, transactions and WHAT_IF are implemented and under gate verification** ([P6 gate](gates/P6.md), [ADR 0013](adr/0013-p6-branches-transactions.md) r0.6). G0–G5, G7 and G8 pass and G6 is none for P6. G3: PROP13 against an independent branch oracle (10,000 seeds), PROP7/8/10/11 per branch, and [mutation](gates/p6-g3-mutation.md) with every survivor closed by a test or argued equivalent. G4's branch stress found two merge defects (a cubic merge-base search and sync points retaining whole databases), fixed in ADR r0.6 without semantic change. [G5](gates/p6-g5.md): fork O(1), WHAT_IF ≤ 1.6× a commit, branch commit at parity with `main`; the first write after a fork or merge copies the branch (≈ 0.05–0.25 s at 10⁶ revisions). The independent implementation review found 8 majors (among them a `require_certified` merge passing with Conflicts off the head, an idempotency hash ignoring the branch, and group-commit projections differing from a rebuild), all fixed with regression tests (ADR r0.7, SPEC v1.6.9); G1, G3, G4 and G7 were rerun on the fixed tree. All P6 layers pass. The phase gate also waits on P5b, P5a and P4 (SPEC A11); the live dogfood stores stay on protocol 3 / op_log format 2 until after the P4 report.

**P5b symbol anchors are implemented and under gate verification** ([P5b gate](gates/P5b.md), [ADR 0012](adr/0012-p5b-symbol-anchors.md) r0.5). G0, G2, G3, G4 and G5 passed; [G2](gates/p5b-g2.md) closes the formatter evidence (6,820 symbols across six corpora) and the legacy `#` key-collision regression. [G3](gates/p5b-g3.md) passed full-count Linux properties (two separately written 10,000-seed symbol suites plus the P5a carry-forward) and normaliser/extraction mutation: 230 caught, 13 compile-invalid, no survivors. The phase gate remains open and cannot pass before P5a/P4. [G4](gates/p5b-g4.md) fixed three robustness defects (deep nesting, deadline enforcement, `MISSING` names) and passed with fuzz and mutation; [G5](gates/p5b-g5.md) keeps the P4 hook budgets at 1,000 symbol anchors. G6 failed with walk 2 (symbol FI 34.2 %, missed +13.1 pp), then passed with walk 3 (same-file context) on a fresh sample under the owner's move convention: FI 18.7 % ≤ ½ × 79.7 %, missed +0.0 pp ([retest](gates/p5b-g6-r2-report.md); 70.4 % under the pre-registered reading). Remaining: G1, G7, G8 and the independent review.

**P5a is implemented; its gate is open** ([P5a gate](gates/P5a.md), [ADR 0011](adr/0011-p5a-file-anchors.md) r0.6). G0–G5, G7, G8 and prior independent reviews pass. G6 uses 210 agent-labelled pairs with explicit provenance and hashed evidence; the 84 historical human reviews remain optional diagnostics and no manual top-ups are required (SPEC v1.6.4 §4.4). G6 passed under this agent-evidence criterion (`p5a_g6_evidence_complete=true`; [report](gates/p5a-g6-agent-report.md)). P5a cannot pass before P4.

**P4 is implemented and under gate verification** ([P4 gate](gates/P4.md), [ADR 0010](adr/0010-p4-hooks-action-gating.md) rev 3.3). Maintenance agents run and review the dogfood and generate its report; no owner labelling or sign-off is required. The ≥2 weeks of real use with ≥2 parallel agents and evidence requirements remain; P4 passes only with that report. Earliest planned report remains 2026-11-04.

**Gate P3 passed** (2026-10-21) after independent review, a closure check and a closure confirmation ([P3 gate](gates/P3.md),
[ADR 0008](adr/0008-p3-daemon-sessions-certificates.md) rev 3.3,
[ADR 0009](adr/0009-p3-memory-scopes-after-maintained-layers.md)).

**Gate P2 passed** (2026-10-20, commit `21da1f7`) after independent review and two closure
checks, with CI run 39497969575 and nightly run 39498455621.
See [P2 gate](gates/P2.md), [P2 report](gates/P2-report.md),
[durable-log ADR 0006](adr/0006-p2-durable-log.md) and
[incremental structural commits ADR 0007](adr/0007-incremental-structural-commits.md).

P1 functionality is implemented, and every local P1 gate layer passes. After
storage compaction, memory is 181.15 / 188.87 B/incidence of live heap. It is within
the per-scope budgets of ADR 0005, which the owner chose to calibrate per scope.
**Gate P1 passed** (CI run 39436790664, nightly run 39436822063).
See [P1 report](gates/P1-report.md), [gate matrix](gates/P1.md) and
[memory ADR](adr/0005-p1-memory-budget.md).

P0 Gate passed, including independent review and remote Linux/macOS
[CI run 39364257458](https://github.com/zangjiucheng/ContextDB/actions/runs/39364257458)
for implementation commit `73789ef96efc3d385cefe78ae7c3b6aa1ec4980d`.
That run does not validate the current P1 changes.

## Available now

- Dependency-free model: IDs, all 14 Ops, bounded wire decoding, EventLog v0,
  input/derived snapshots and status dimensions.
- Independent oracle: transactional InputModel, Jacobi least fixed point, Kahn DAG
  evaluator, WHY / WHY_NOT, summarize and P0 required-entry certificate predicate.
- `Db<M, S>` with Counting / FullRecompute and CommitStats / zero-sized NoStats.
  Counting maintains L0 support, sat/unsat, enabled rules and canonical witnesses;
  Pearce–Kelly maintains the revision/Sem-virtual DAG with NaiveTopo reference.
- Atomic mutable commits with undo, final-net-delta seeding, local status propagation,
  retained snapshot patching, allocations, exact flips/diffs and deterministic digests.
  Structural changes validate against a staging overlay and publish incrementally
  (appendable CSR adjacency, key table, PK undo journal; ADR 0007).
- O(1) CURRENT L0; counter-maintained L1/L2 columns (ADR 0008 §7, rebuilt independently in FullRecompute); WHY with bounded canonical proof,
  WHY_NOT; Reachable and undo-restored Exact IMPACT; endpoint-folded in-memory DIFF.
- Debug/paranoid DA1–DA6 checks, release-paranoid oracle cross-checks, separate timing
  feature, conservative memory categories, deterministic shape generators and stats.
- Both exact modes: 52 P0 and 24 P1 golden/regression tests. 10,000 fixed ChaCha8
  seeds / 120,000 transactions, PROP7 metamorphic checks, depth-6/8 finite-state
  exploration and counting/transaction mutation audit. See report for exact results.
- Rust 1.95.0, Apache-2.0, architecture/license gates, standard Linux/macOS CI and
  a nightly workflow: exhaustive depth 6/8, four-shard mutation audit, P2 G4/PROP10 at gate counts.

- `contextdb-store`:
  - own segmented op_log behind `Durability` (StdDisk, and SimDisk for crash injection);
  - blake3 blobs;
  - idempotent `client_txn_id` with `txn_status`;
  - snapshots and replay with a sampled oracle self-check;
  - SQLite projections with a watermark;
  - Historical / Snapshot / DIFF over persisted data;
  - format-0 → 1 migration;
  - doctor truncation.
- `ctx replay | doctor | snapshot | migrate | export`.
- P3 store additions: op_log format 2 with group framing and one sync per group
  (`commit_group`), the session log, `LogReader`, `watermark_history`, FTS5 recall,
  and self-verifying blob packs with ordered syncs (ADR 0008 §17).
- Certificates: `issue_certificate` / `validate_certificate` with an independent
  oracle (PROP11).
- `ctxd` (`contextdb-daemon`): commit actor with group commit, two-replica
  snapshot publication (loom-modelled), three channel sockets with peer-uid checks,
  sessions, handles, `ctx_check`, `Busy` / 30 s `Timeout`.
- `ctx-mcp` (`contextdb-mcp`): nine agent tools over JSON-RPC 2.0 on stdio.
- `ctx status | why | why-not | impact | diff | validate | shutdown` through the
  admin socket.
- P4 hooks and action gating, outbox delivery and parallel-agent session views;
  P5a file observers with watermark CAS, code freshness and rebind suggestions.
- P5b symbol extraction/normalisation for Rust, TypeScript/TSX/JavaScript,
  Python and Go; `ctx anchors symbols | add | scan | watch | status | migrate`.
  Grammars run only in the observer; `ctxd` checks host-file positions without parsing.

## Boundaries

- No signed log head (P9).
- No erasure: blobs are never deleted before P9.
- No `inputs_at` cache.
- In-memory history is not bounded after open.
- One subject, positive DAG, transaction time only. Branches (P6) are explicit;
  no branch deletion or automatic git-merge detection. No ACL, negation,
  recursive production rules, erasure or replication.
- The P3 trust boundary is cooperative: a same-user agent with a shell can reach
  the observer and admin sockets (hard separation with principals and ACL in P9).
- Open Conflict artifacts gate their target (SD17, P6); merges open them, and
  semantic conflict producers arrive in P7. Visibility is Authorized before P9; structural
  Unknown/freshness are summary-function fixtures.
- Research DD/DBSP workspaces stay excluded. P4's real dogfood report and
  P5b's remaining gate evidence are still required.

## Dependency policy

`scripts/check_dependencies.py` checks all normal/build/dev/optional/target edges.
Model has no dependencies; oracle reaches only model. Core's explicit allowlist
admits BLAKE3 and build/platform helpers, not database/network/LLM crates. ChaCha8
and benchmark generation stay outside core. `cargo deny` checks licenses,
advisories, bans and sources; no GPL/AGPL-only dependency is admitted.
