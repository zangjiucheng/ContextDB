#!/usr/bin/env bash
# ADR 0013 §10 G3: mutation of visibility and dedup, merge, ExpectHeads, the
# WHAT_IF undo journal and the SD17 gate, on the Linux host. The P6 kernel
# files are mutated whole; in the shared core files only the P6 functions are
# (`--re`). PROP13, PROP7 and PROP11 run short slices per mutant; the
# complete 10,000-seed runs are separate (scripts/g3-p6.sh).
#
#   scripts/mutation-p6.sh kernel   kernel, merge, whatif, view files
#   scripts/mutation-p6.sh shared   P6 functions in arena, layers, txn, lib
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p results
# Every parallel mutant needs its own target directory, otherwise it may run
# another mutant's executable. At most 24 × 2 compilation jobs = 48 cores.
unset CARGO_TARGET_DIR
export CONTEXTDB_PROP13_SEEDS=300 CONTEXTDB_P6_PROP7_SEEDS=800 CONTEXTDB_P6_PROP11_SEEDS=300 \
  CONTEXTDB_PROP11_SEEDS=500
jobs=${MUTATION_JOBS:-24}
if (( jobs > 24 )); then jobs=24; fi
part=${1:?kernel or shared}; shift
core=crates/contextdb-core/src
case $part in
  kernel)
    files=(--file $core/kernel.rs --file $core/kernel_merge.rs --file $core/merge.rs
      --file $core/whatif.rs --file $core/kernel_view.rs)
    ;;
  shared)
    files=(--file $core/arena.rs --file $core/layers.rs --file $core/txn.rs --file $core/lib.rs
      --re 'KeyTable::remove|::truncate|truncate_rows|adjust_conflicts|Indexes::conflicted|LayerColumns::(footprint|restore|node)|final_read_set|ExpectHeads|anchor_unobserved|::publish')
    ;;
  *) echo "kernel or shared" >&2; exit 2 ;;
esac
CARGO_BUILD_JOBS=2 cargo mutants --package contextdb-core "${files[@]}" \
  --jobs "$jobs" --timeout 300 \
  --output "${MUTATION_OUTPUT:-results/p6-mutation-$part}" "$@" -- \
  --package contextdb-core --package contextdb-testkit --lib \
  --test p0 --test p1 --test p3 --test p3_certificates \
  --test p6_branches --test p6_visibility --test p6_what_if --test p6_merge \
  --test p6_conflict_gate --test p6_read_set --test p6_views --test p6_branch_fuzz \
  --test p6_prop13 --test p6_g3_properties --test p6_kernel_edges --locked
