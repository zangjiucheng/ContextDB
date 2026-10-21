#!/usr/bin/env bash
# SPEC Gate P3 G4: ctxd-stress at 1/8/32/128 clients (default 600 s each).
set -euo pipefail
cd "$(dirname "$0")/.."
seconds=${STRESS_SECONDS:-600}
cargo build --release --locked -p contextdb-daemon --bin ctxd-stress
mkdir -p results
status=0
for clients in ${STRESS_CLIENTS:-1 8 32 128}; do
  ./target/release/ctxd-stress "$clients" "$seconds" | tee -a results/stress.txt || status=1
done
exit $status
