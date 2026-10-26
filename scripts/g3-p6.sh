#!/usr/bin/env bash
# ADR 0013 §11 G3: the complete P6 properties at gate counts; run through
# remote.sh on the pinned Linux node.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p results
unset CONTEXTDB_PROP13_SEEDS CONTEXTDB_P6_PROP7_SEEDS CONTEXTDB_P6_PROP11_SEEDS CONTEXTDB_P6_PROP10_SEEDS
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-8}
{
  rustc --version
  # PROP13 against the independent branch oracle (10,000 seeds).
  cargo test --release --locked -p contextdb-testkit --test p6_prop13 -- --nocapture
  # PROP7 for the branch Ops and PROP11 per branch (10,000 seeds each).
  cargo test --release --locked -p contextdb-testkit --test p6_g3_properties -- --nocapture
  # PROP8/I1 per branch and dry runs; the oracle Conflict gate.
  cargo test --release --locked -p contextdb-testkit --test p6_branch_fuzz --test p6_conflict_gate -- --nocapture
  # PROP10 per branch through the store.
  CONTEXTDB_P6_PROP10_SEEDS=2000 cargo test --release --locked -p contextdb-store --test p6_prop10 -- --nocapture
} 2>&1 | tee results/p6-g3-properties.txt
