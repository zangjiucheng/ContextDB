# P6 G3 — mutation of the branch kernel

`scripts/mutation-p6.sh` on the remote Linux host (cargo-mutants 27.1.0,
one target directory per mutant, ≤ 48 cores). Each mutant runs the core
library tests and the testkit suites P0, P1, P3, P3 certificates and every
P6 suite (PROP13, PROP7 and PROP11 on short slices; their full 10,000-seed
runs are in [`p6-g3-properties.txt`](p6-g3-properties.txt)).

| Run | Tree | Caught | Missed | Timeout | Unviable |
|---|---|---|---|---|---|
| `kernel` 1: `kernel.rs`, `kernel_merge.rs`, `merge.rs`, `whatif.rs`, `kernel_view.rs` | f23f600 | 315 | 56 | 1 | 90 |
| `shared`: the P6 functions in `arena.rs`, `layers.rs`, `txn.rs`, `lib.rs` (`--re`) | 5968472 | 82 | 7 | 3 | 3 |
| `kernel` 2, after the new tests | 23ec037 | 339 | 24 | 1 | 90 |
| `kernel_merge.rs` again, after ADR r0.6 (merge bases, pruning) | e37e864 | 128 | 4 | 0 | 18 |
| `kernel` 3, after the independent review (ADR r0.7) | 6eab9bd | 352 | 27 | 1 | 90 |

The ADR's mutation targets (§11) are all covered: visibility and dedup
(`kernel.rs`), merge base and classification (`kernel_merge.rs`,
`merge.rs`), `ExpectHeads` (`txn.rs::final_read_set` and the merge read
set), the undo journal (`whatif.rs`, the arena/layer truncations) and the
conflict gate (`Indexes::conflicted`, `adjust_conflicts`, the layer node).
The file names differ from the ADR's sketch (`branch.rs`).

## Survivors that were real gaps (closed)

From run 1, `p6_kernel_edges.rs` adds:

- ValidationRecord conflicts in a merge (different verdicts conflict on the
  rule's conclusion with the canonical payload; the same verdict from
  another validator converges) — 10 mutants; no test merged
  ValidationRecords before;
- the merge report's `source`; duplicate entries in a merge's read set;
- `main_is_identity` (and `IdMap::push`'s renumbering flag);
- a dry run of a batch that needs the import retry (the restore of the
  backup must not happen for WHAT_IF);
- snapshot import refusing ids with a gap; `state_diff` detecting a
  difference;
- the view's accessors on a renumbered branch, historical reads below the
  global epoch, observation support and merged anchors' unobserved bit;
- a certificate entry the branch cannot see, required (Changed) or
  advisory (skipped).

From the shared run: WHAT_IF rollback of metadata on a new logical (a stale
entry would attach to the next logical with that id) and of many colliding
keys (the key table's probe chains). The merge-pruning code was killed in
full by `superseded_sync_points_are_pruned`. All were re-run locally
against the new tests before the full re-runs.

## Equivalent survivors (final runs)

`kernel.rs`:

- `run` 301/304 (`b.ops.len() == 1` guards → true): `SoleOpRequired` has
  already rejected a `CreateBranch`/`ArchiveBranch` that is not alone.
- `commit_ops` 418–420, 426, 439–442: the import-retry loop. `ambiguous`
  made true more often only adds attempts; the backup condition variants
  take an equal backup (the restored or untouched database); the attempt
  bound is defensive (every generated and constructed batch settles by
  the second attempt), so `attempts *= 1` still terminates there.
- `commit_ops` 480/481 (`logical` dropped from the kernel's provenance
  copy of a revision): only its origin fields are read
  (`set_revision_origin`).
- `resolve` 594/595: the fresh-id arithmetic for global ids beyond the
  previous attempt's plan. A wrong local id makes the attempt disagree
  with the assignment, and the loop retries with the corrected plan.
- `names_invisible` 602/605: made true, it only marks more batches
  ambiguous (more attempts, same result).
- `staged_global` 987/988: the branch for an id beyond everything staged.
  The resolver refuses forward references (`UnknownRef`) and every error
  that names a staged object names one the assignment covers, so no input
  reaches it (an attempted test got `UnknownRef`).

`kernel_merge.rs`:

- `merge_bases` 232 (`<` → `<=` choosing a branch's latest state): sync
  points are one per epoch, so two states of one branch never tie.
- `merge` 315 (`n < op_index` → `<=`): the Op at `op_index` is the merge,
  never an `ExpectHeads`.
- `merge` 449 (`||` → `&&` in the dirs conflict): an empty side cannot
  reach the conflict row (dirs only narrow, so an empty `S` or `T` equals
  an empty base), and an empty side makes the intersection empty anyway.

`whatif.rs` 183 (`state_diff`'s journal check `&&` → `||`): a journal
exists only inside a dry run, never between commits.

Shared files:

- `lib.rs` 701 (`&&` → `||` before clearing merged anchors on a watermark):
  with no unobserved anchors or no watermarks the `retain` removes nothing.
- `lib.rs` 688 (`<` → `<=` journalling metadata of logicals): the extra
  entry is for a logical the rollback removes; the new metadata test shows
  no trace.
- `KeyTable::remove` 61 (`len -= 1` → `+=`/`/=`): `len` only drives the
  resize threshold.
- `LogicalArena::truncate` 197 (`n < len` → `<=`): at `n == len` the block
  truncates nothing and drains no lane.
- `LayerColumns::footprint` 93 (`<` → `<=`): a saved row at the old length
  is dropped by the restore's truncation.

Timeouts (`IdMap::len → 0`, three `KeyTable::remove` probe mutants) loop
forever and count as detected.

## After the independent review (6eab9bd)

The kernel run on the reviewed tree left 27 survivors. 24 are the
equivalent survivors above, at shifted line numbers. The other 3 were in
code the review fixes added, and the tests now kill them (each mutant
applied by hand against `p6_kernel_edges`):

- `BranchView::digest` (`&&` → `||` in the `main`-without-merges shortcut):
  a merge into `main` now must change `main`'s digest
  (`branch_digests_cover_the_branch_table_and_sync_points`).
- `merge`'s `early` guard for `InvalidReadSet`, made `true` or `false`: an
  invalid read set before the Merge on a renumbered branch names the global
  logical, and one after the Merge comes after an unknown `certify` key
  (`late_invalid_read_set_in_a_merge_names_the_global_logical`).
