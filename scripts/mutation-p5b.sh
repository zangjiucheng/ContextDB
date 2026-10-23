#!/usr/bin/env bash
# ADR 0012 §9 G3: normaliser and extraction mutation on the Linux host.
# A deterministic 60-seed slice keeps each mutant bounded; the independent
# complete 10,000-seed run is separate. Both oracle and G2 goldens run here.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p results
# Every parallel mutant needs its own target directory, otherwise it may run
# another mutant's executable. At most 24 × 2 compilation jobs = 48 cores.
unset CARGO_TARGET_DIR
export CONTEXTDB_SYMBOL_SEEDS=60
jobs=${MUTATION_JOBS:-24}
if (( jobs > 24 )); then jobs=24; fi
CARGO_BUILD_JOBS=2 cargo mutants --package contextdb-symbols \
  --file crates/contextdb-symbols/src/walk.rs \
  --file crates/contextdb-symbols/src/extract.rs \
  --jobs "$jobs" --timeout 120 \
  --output "${MUTATION_OUTPUT:-results/p5b-mutation}" "$@" -- \
  --package contextdb-symbols --test symbols --test prop12_symbols --locked
