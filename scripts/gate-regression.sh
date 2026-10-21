#!/usr/bin/env bash
# SPEC Gate G7 (P4: ADR 0010 §11): earlier gates' G2/G3/G4 at gate counts,
# plus the P4 suites in release. Performance baselines (P1 G5 memory, P2/P3
# G5 benches) stay on the reference Mac and are not run here.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p results
{
  echo "== paranoid testkit (P0 goldens, P1/P2/P3 properties at 10,000 seeds)"
  cargo test --release --locked -p contextdb-testkit --features contextdb-core/paranoid
  echo "== exhaustive depth 6 (gate)"
  cargo test --release --locked -p contextdb-testkit --features contextdb-core/paranoid --test exhaustive -- --ignored --nocapture
  echo "== exhaustive depth 8 (report)"
  CONTEXTDB_EXHAUSTIVE_DEPTH=8 cargo test --release --locked -p contextdb-testkit --features contextdb-core/paranoid --test exhaustive -- --ignored --nocapture
  echo "== store and daemon crash injection (1,000 per point), PROP10 (10,000 seeds)"
  CONTEXTDB_G4_RUNS=1000 CONTEXTDB_PROP10_SEEDS=10000 cargo test --release --locked -p contextdb-store -p contextdb-daemon
  echo "== P4 suites (release)"
  cargo test --release --locked -p contextdb-proto -p contextdb-hooks -p contextdb-cli -p contextdb-mcp
  echo "G7 PASS"
} 2>&1 | tee results/gate-regression.txt
