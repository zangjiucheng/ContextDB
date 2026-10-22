# Gate P5a — File-level code anchors

Status: **Implemented; gate open.** G0 passed 2026-10-21 ([ADR 0011](../adr/0011-p5a-file-anchors.md) r0.3, independent G0 review; r0.4 §9 and r0.5 §10 amendments). The implementation passed an independent counterexample review; its findings are fixed or listed below. Open: 9 G6 audit top-ups and P4 itself: this gate cannot pass before P4 (SPEC A11). Live `.contextdb/bin` and `.contextdb/db` are untouched.

## Scope

SPEC SD18, SD14b, §5 P5a, §6.5 path filtering and pre-push range checks.
No symbol parsing, branch product API, validators or ACL.

## Gate matrix

| Layer | Requirement | Result | Evidence |
|---|---|---|---|
| G0 | ADR 0011, SPEC ambiguity closure, independent review | Pass | ADR 0011 r0.3; SPEC v1.6.1 |
| G1 | fmt, Clippy, workspace tests, deny, dependency boundaries | Pass | `bash scripts/check.sh` on macOS and on Linux (remote Ubuntu 24.04) for the final tree; GitHub Actions still starts no job (account billing) |
| G2 | T42a, T42c, T43, T43b, T44, T46b, T46c, S13-rows-e2e | Pass | `anchors/tests/anchors.rs` (T42/T43/T46b with exact oracle counters, T44 incl. Sem asserted-p closure, descriptor/OID bounds, raw SHA-1/SHA-256 OIDs vs Git, symlink/FIFO/socket/nested-`.git`, gitlinks); `daemon/tests/anchors.rs` (T44 for read/recall/check, T46c incl. case alias, hard link, symlink, nested repository and gitfile fallbacks, strict root proofs across restart, legacy MissingBaseline, range binding); `cli/tests/anchors_cli.rs`; `proto/tests/observation_cutoff.rs` |
| G3 | PROP12, 10,000 fixed seeds, coverage; classifier mutation | Pass | PROP12 rewritten to the ADR: generated multi-file histories, Git fast-import/cat-file as the independent record, oracle re-encodes descriptor/position bytes and never calls the classifier. 100,000 transactions: delete 26,802, identical restore 7,153, old-head ABA 22,673, CAS retry 5,701, lag 6,850, unavailable 7,090, unsupported paths 5,971, SHA-256 2,000 (all above the floors). Injected classifier bug (no AssertBase on restore) is caught. Mutation of `classify.rs` on the final tree: 19 mutants, 19 caught |
| G4 | interruption, CAS concurrency, missed wakeups, restart/failure | Pass | Observer batch interrupted at every store step (append, write, sync, sync_weak, rename, sync_dir, sync_dir_weak) then crashed with torn/zero/garbage tails: state is wholly before or after, rescan converges; corrupt descriptor → scope Unknown; racing wakeups commit once; racing restores of a broken anchor lose on the watermark CAS; watch/restart/missed wakeup/oversized file in the CLI test. Semantic goldens use a 2 s sample deadline (`set_anchor_live_timeout`) so a cold sample on a loaded machine is not Unknown; the production 40 ms budget is measured in G5 |
| G5 | P4 hook budgets; capture/reconcile costs at 10/100/1,000 anchors | Pass (rule: owner decision 2026-10-22) | [`p5a-g5-anchors.txt`](p5a-g5-anchors.txt), confirmation run at load ≈ 8–10 on tree `1ce43dc`: `p5a_g5_pass=true`. 10/100 anchors: every certified read, 0 capture failures, hook p95 ≤ 7.1 ms, p99 ≤ 13.6 ms. 1,000 anchors (the supported bound): capture p95 18.5–19.2 ms, 0 capture failures, 972–982 of 1,000 certified reads (the rest Unknown, fail closed), hook p95 36.2–37.3 ms, p99 50.4–50.5 ms (budgets 50 / 100 ms). Capture is parallel, reuses gitlink decoding while the index bytes are identical and the opened repository while its identity holds; descriptor checks run beside capture; the hint cutoff is O(1) (r0.4). Earlier runs on a loaded desktop (load 12–55) are in the file's history |
| G6 | ≥ 3 real repos × ≥ 1,000 commits; assisted ground truth (ADR 0011 r0.5 §10, SPEC v1.6.3, owner decision) | **Open: 9 per-stratum audit top-ups** | `anchor_replay` replayed ripgrep, click and fd (1,000 first-parent commits each, 20 anchors by a fixed rule) through the real daemon/observer: 1,352 pairs in [`p5a-g6/`](p5a-g6/), 0 disagreements with `git diff` in 3,000 commits. [210 AI-proposed labels](p5a-g6/ai-review/README.md) on a pre-registered stratified sample (35 per repository × fired/not-fired). Owner audit so far G6-001–084: 80/84 agree = 95.2 % (Wilson 95 % lower bound 88.4 %; not blind, so an upper bound); 4 contested (fd not-fired 3, click not-fired 1). The independent review of r0.5 found the global prefix covered only 8 fired pairs (click 1, fd 7, ripgrep 0), so the rule now needs ≥ 5 audited per stratum: click fired needs 4 more, ripgrep fired 5 (`scripts/p5a_review.py --ids …`, see the README). Current per-stratum report: `anchor_replay score --assisted docs/gates/p5a-g6/ai-review/proposals.tsv docs/gates/p5a-g6/*.tsv` (false invalidation click 28.6 %, fd 34.3 %, ripgrep 34.3 %; invalidating-but-not-fired click 8.8 %, fd 9.4 % [contested range 11.4–14.3 %], ripgrep 42.9 % on 7 files). Not population rates. Path-domain note: only lowercase paths can be anchors (ripgrep 129/184, click 95/115, fd 32/38 files at the start commit) |
| G7 | P0–P4 regressions; P4 must pass first | Pass (P4 pending) | `scripts/gate-regression.sh` (now incl. P5a suites) on the remote host for the final tree: G7 PASS |
| G8 | README, semantics, CHANGELOG, status | Pass | README §文件级代码锚点（P5a）, CHANGELOG, IMPLEMENTATION_STATUS |
| Independent review | non-implementing counterexample review | Pass with open items | See below |

## Independent implementation review (2026-10-21)

No blockers. Fixed with regression tests unless noted:

- **M1** path filter ignored nested repositories/linked worktrees inside the bound repo (wrong Allow): any intermediate `.git` now forces a full check.
- **M2** outbox streams are never deleted; the per-stream hint cutoff opened every stream twice per sample and errored above 1,000, so live evidence would eventually fail closed for every anchor. **Fixed by ADR 0011 r0.4 (§9)** after its own G0 review and closure check: hooks announce before appending an Action fact (random epoch + counter, replaced whole by rename); ctxd reads the record before the first and after the last capture; missing/corrupt/unequal records, a failed announce (record unlinked, `announce.failed` sentinel, `ctx anchors repair-announce`) or a missing `announce-v1` appender marker are Unknown; dropped facts no longer gate freshness. Tests: `proto/tests/announce.rs` (2,000 streams, corruption, delete/recreate, marker, sentinel, failed announce, dropped edit, kill points).
- m1 Unix socket at an anchor path (EOPNOTSUPP) made the scope Unavailable: now Broken.
- m2 cached repo identity now covers the `.git` entry (gitfile) and the canonical root.
- m3 per-root vector merge was order-dependent: earliest read wins (fixed; no dedicated test).
- m4 case-alias registration: exact on-disk spelling required.
- m5 **Open (documented limit)**: the gitlink decode reads the whole index (≤ 8 MiB, about 80k files); index rewrites during capture give Unknown.
- m6 `.git` probe errors other than ENOENT are Unavailable; a `.git` symlink is Broken.
- m7 the watermark CAS is first in the batch, so a racing loser retries.
- Git hooks: a failed scan no longer fails `git checkout`/`commit`/`merge`.
