#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Extra arguments go to cargo-mutants, e.g. `--shard 0/4` (nightly matrix).
mkdir -p results
# Each cargo-mutants build directory needs its own target directory: a
# shared CARGO_TARGET_DIR (as scripts/remote.sh exports) lets parallel
# mutants overwrite each other's binaries, so tests run the wrong mutant.
unset CARGO_TARGET_DIR
# The baseline must select testkit too; --test-package alone does not apply
# to cargo-mutants 27.1.0's initial baseline build/test selection.
CONTEXTDB_PROP11_SEEDS=${CONTEXTDB_PROP11_SEEDS:-2000} cargo mutants --workspace --test-workspace true \
  --file crates/contextdb-core/src/engine/counting.rs \
  --file crates/contextdb-core/src/txn.rs \
  --file crates/contextdb-core/src/arena.rs \
  --file crates/contextdb-core/src/stage.rs \
  --file crates/contextdb-core/src/memory.rs \
  --file crates/contextdb-core/src/layers.rs \
  --file crates/contextdb-core/src/cert.rs \
  --jobs "${MUTATION_JOBS:-4}" --timeout "${MUTATION_TIMEOUT:-60}" \
  --output results/p1-mutation "$@" -- \
  --package contextdb-core --package contextdb-testkit --lib --test p0 --test p1 --test memory_accounting --test p3 --test p3_certificates --locked
