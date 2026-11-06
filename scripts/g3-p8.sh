#!/usr/bin/env bash
# ADR 0016 §10 G3: PROP14 (retrieval against the oracle) at 10,000 seeds,
# plus the injective-path property; run through remote.sh on the pinned
# Linux node.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p results
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-8}
{
  rustc --version
  CONTEXTDB_PROP14_SEEDS=${CONTEXTDB_PROP14_SEEDS:-10000} \
    cargo test --release --locked -p contextdb-daemon --test p8_prop14 -- --nocapture
  cargo test --release --locked -p contextdb-daemon --lib mirror -- --nocapture
  echo "G3 PASS"
} 2>&1 | tee results/p8-g3-properties.txt
