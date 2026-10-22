# Gate P5a — File-level code anchors

Status: **Implemented; gate open.** G0 passed 2026-10-21 ([ADR 0011](../adr/0011-p5a-file-anchors.md) r0.3, independent G0 review). The implementation passed an independent counterexample review; its findings are fixed or listed below. Open: G5 (quiet-machine rerun and an owner decision on the 1,000-anchor rule), G6 (human labels), M2 (outbox growth, needs an ADR amendment), and P4 itself: this gate cannot pass before P4 (SPEC A11). Live `.contextdb/bin` and `.contextdb/db` are untouched.

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
| G4 | interruption, CAS concurrency, missed wakeups, restart/failure | Pass | Observer batch interrupted at every store step (append, write, sync, sync_weak, rename, sync_dir, sync_dir_weak) then crashed with torn/zero/garbage tails: state is wholly before or after, rescan converges; corrupt descriptor → scope Unknown; racing wakeups commit once; racing restores of a broken anchor lose on the watermark CAS; watch/restart/missed wakeup/oversized file in the CLI test. Timed goldens use the real 40 ms budget and are load-sensitive (fail closed to Unknown under heavy load) |
| G5 | P4 hook budgets; capture/reconcile costs at 10/100/1,000 anchors | **Open** | [`p5a-g5-anchors.txt`](p5a-g5-anchors.txt). Capture is now parallel, reuses gitlink decoding while the index bytes are identical and reuses the opened repository while its identity holds; descriptor checks run beside capture. 10/100 anchors: every certified read, 0 capture failures. 1,000 anchors: capture p95 28–30 ms, p99 at the 40 ms deadline; 908–934 of 1,000 certified reads (the rest Unknown, fail closed). Measured under unrelated desktop load (load 32–55); needs a quiet rerun. `anchor_bench` demands zero failures at 1,000, stricter than ADR §7 (budgets + reporting): owner to decide |
| G6 | ≥ 3 real repos × ≥ 1,000 commits, ≥ 200 human labels | **Open (owner labels)** | `anchor_replay` replayed ripgrep, click and fd (1,000 first-parent commits each, 20 anchors by a fixed rule) through the real daemon/observer: 1,352 unlabelled pairs in [`p5a-g6/`](p5a-g6/), 0 disagreements with `git diff` in 3,000 commits. Labels: fill `invalidates` (y/n) and `labeler`, then `anchor_replay score docs/gates/p5a-g6/*.tsv`. Path-domain note: only lowercase paths can be anchors (ripgrep 129/184, click 95/115, fd 32/38 files at the start commit) |
| G7 | P0–P4 regressions; P4 must pass first | Pass (P4 pending) | `scripts/gate-regression.sh` (now incl. P5a suites) on the remote host for the final tree: G7 PASS |
| G8 | README, semantics, CHANGELOG, status | Pass | README §文件级代码锚点（P5a）, CHANGELOG, IMPLEMENTATION_STATUS |
| Independent review | non-implementing counterexample review | Pass with open items | See below |

## Independent implementation review (2026-10-21)

No blockers. Fixed with regression tests unless noted:

- **M1** path filter ignored nested repositories/linked worktrees inside the bound repo (wrong Allow): any intermediate `.git` now forces a full check.
- **M2** outbox streams are never deleted; the per-sample hint cutoff opens every stream and errors above 1,000, so live evidence eventually fails closed for every anchor. **Open**: needs outbox GC or a global append counter, an ADR 0010/0011 amendment (deleting a stream risks seq reuse after `--resume`). Dogfood has 3 streams after the first day.
- m1 Unix socket at an anchor path (EOPNOTSUPP) made the scope Unavailable: now Broken.
- m2 cached repo identity now covers the `.git` entry (gitfile) and the canonical root.
- m3 per-root vector merge was order-dependent: earliest read wins (fixed; no dedicated test).
- m4 case-alias registration: exact on-disk spelling required.
- m5 **Open (documented limit)**: the gitlink decode reads the whole index (≤ 8 MiB, about 80k files); index rewrites during capture give Unknown.
- m6 `.git` probe errors other than ENOENT are Unavailable; a `.git` symlink is Broken.
- m7 the watermark CAS is first in the batch, so a racing loser retries.
- Git hooks: a failed scan no longer fails `git checkout`/`commit`/`merge`.
