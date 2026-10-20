#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p results
# The baseline must select testkit too; --test-package alone does not apply
# to cargo-mutants 27.1.0's initial baseline build/test selection.
cargo mutants --workspace --test-workspace true \
  --file crates/contextdb-core/src/engine/counting.rs \
  --file crates/contextdb-core/src/txn.rs \
  --file crates/contextdb-core/src/arena.rs \
  --file crates/contextdb-core/src/memory.rs \
  --jobs 4 --timeout 60 \
  --output results/p1-mutation -- \
  --package contextdb-core --package contextdb-testkit --lib --test p0 --test p1 --test memory_accounting --locked
