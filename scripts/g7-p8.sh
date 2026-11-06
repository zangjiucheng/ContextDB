#!/usr/bin/env bash
# ADR 0016 §10 G7 and G4: earlier phases' G2/G3/G4 (gate-regression.sh,
# g3-p6.sh), the P7 properties and suites, P7 stress, and the P8 robustness
# tests (debug build: the crash points are compiled out of release); run
# through remote.sh on the pinned Linux node.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p results
out=results/p8-g7-regression.txt
: > "$out"
{
  rustc --version
  echo "== gate-regression.sh (P0-P5b G2/G3/G4 at gate counts)"
  bash scripts/gate-regression.sh
  echo "== g3-p6.sh (P6 properties at gate counts)"
  bash scripts/g3-p6.sh
  echo "== P7 properties (10,000 seeds) and suites (release)"
  cargo test --release --locked -p contextdb-testkit --test p7_g3_properties -- --nocapture
  cargo test --release --locked -p contextdb-daemon -p contextdb-validate -p contextdb-mcp -p contextdb-cli
  echo "== P7 G4 (kill at each job step, 25 runs; debug)"
  CONTEXTDB_P7_G4_RUNS=25 cargo test --locked -p contextdb-daemon --test p7_g4 -- --test-threads 1
  echo "== P7 stress"
  STRESS_SECONDS=${STRESS_SECONDS:-1800} bash scripts/stress-p7.sh
  echo "== P8 G4 (kill points, disk full, git locks, truncation; debug)"
  cargo test --locked -p contextdb-daemon --test p8_g4 -- --test-threads 2
  echo "G7 PASS"
} 2>&1 | tee -a "$out"
