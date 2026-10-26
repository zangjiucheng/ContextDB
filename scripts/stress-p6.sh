#!/usr/bin/env bash
# ADR 0013 §11 G4: ctxd-branch-stress at 1/8/32 clients (default 600 s each),
# then crash injection around the branch Ops and the interrupted 2→3
# migration at gate counts.
set -euo pipefail
cd "$(dirname "$0")/.."
seconds=${STRESS_SECONDS:-600}
cargo build --release --locked -p contextdb-daemon --bin ctxd-branch-stress
mkdir -p results
status=0
for clients in ${STRESS_CLIENTS:-1 8 32}; do
  "${CARGO_TARGET_DIR:-target}/release/ctxd-branch-stress" "$clients" "$seconds" | tee -a results/p6-stress.txt || status=1
done
CONTEXTDB_G4_RUNS=${CONTEXTDB_G4_RUNS:-1000} CONTEXTDB_G4_MIGRATE_SEEDS=${CONTEXTDB_G4_MIGRATE_SEEDS:-64} \
  cargo test --release --locked -p contextdb-store --test p6_crash -- --nocapture 2>&1 | tee -a results/p6-stress.txt || status=1
exit $status
