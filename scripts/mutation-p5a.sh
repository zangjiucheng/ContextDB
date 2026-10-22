#!/usr/bin/env bash
# Pure observer classification; Git-history property is verified separately.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p results
unset CARGO_TARGET_DIR
CARGO_BUILD_JOBS=2 cargo mutants --package contextdb-anchors --file crates/contextdb-anchors/src/classify.rs \
  --jobs "${MUTATION_JOBS:-4}" --timeout 120 --output results/p5a-mutation "$@" -- \
  --package contextdb-anchors --test anchors --locked
