#!/usr/bin/env bash
# Complete G3 properties; run through remote.sh on the pinned Linux node.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p results
unset CONTEXTDB_SYMBOL_SEEDS
export CARGO_BUILD_JOBS=2
{
  rustc --version
  git --version
  cargo test --release --locked -p contextdb-symbols --test prop12_symbols -- --nocapture
  cargo test --release --locked -p contextdb-symbols --test prop12_symbols_model -- --nocapture
  cargo test --release --locked -p contextdb-anchors --test prop12 -- --nocapture
} 2>&1 | tee results/p5b-g3-properties.txt
