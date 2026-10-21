#!/usr/bin/env bash
# SPEC Gate P4 G4: outbox stress, 8 streams x 16 concurrent ctx-hook
# processes (default 600 s), then every stream's applied seq must equal its
# number of hook runs (ADR 0010 §11).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --locked -p contextdb-hooks --bin ctx-hook -p contextdb-bench --bin hook_stress
mkdir -p results
"${CARGO_TARGET_DIR:-target}/release/hook_stress" "${CARGO_TARGET_DIR:-target}/release/ctx-hook" \
  "${STRESS_SECONDS:-600}" "${STRESS_STREAMS:-8}" "${STRESS_PER_STREAM:-16}" | tee results/hook-stress.txt
