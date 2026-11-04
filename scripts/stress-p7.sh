#!/usr/bin/env bash
# ADR 0015 §11 G4: ctxd-validate-stress (4 writers on 2 branches, 4 mock
# workers; default 1,800 s), then process death at each job step
# (debug build: the crash points are compiled out of release) and the
# disk-full cases, at gate counts.
set -euo pipefail
cd "$(dirname "$0")/.."
seconds=${STRESS_SECONDS:-1800}
mkdir -p results
out=results/p7-g4-stress.txt
: > "$out"
status=0
cargo build --release --locked -p contextdb-daemon --bin ctxd-validate-stress
"${CARGO_TARGET_DIR:-target}/release/ctxd-validate-stress" "$seconds" 2>&1 | tee -a "$out" || status=1
CONTEXTDB_P7_G4_RUNS=${CONTEXTDB_P7_G4_RUNS:-25} \
  cargo test --locked -p contextdb-daemon --test p7_g4 -- --nocapture --test-threads 1 2>&1 | tee -a "$out" || status=1
exit $status
