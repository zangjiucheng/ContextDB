#!/usr/bin/env bash
# SPEC Gate P4 G3: mutation of the P4 code (ADR 0010 §11): the gate
# predicate and outbox (contextdb-proto), the hook library, and the daemon
# service (session binding, agent views, updates, facts, drift).
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p results
# Each cargo-mutants build directory needs its own target directory: a
# shared CARGO_TARGET_DIR (as scripts/remote.sh exports) lets parallel
# mutants overwrite each other's binaries, so tests run the wrong mutant.
unset CARGO_TARGET_DIR
# The test timeout scales with the unmutated baseline (at least 120 s): a
# fixed --timeout also caps the baseline, which outgrew 120 s on a 2-core
# runner once P9 added tests. MUTATION_TIMEOUT still forces one.
CONTEXTDB_P4_OUTBOX_SEEDS=${CONTEXTDB_P4_OUTBOX_SEEDS:-500} \
CONTEXTDB_P4_VIEW_SEEDS=${CONTEXTDB_P4_VIEW_SEEDS:-1000} \
cargo mutants --workspace --test-workspace true \
  --file crates/contextdb-proto/src/gate.rs \
  --file crates/contextdb-proto/src/outbox.rs \
  --file crates/contextdb-hooks/src/adapter.rs \
  --file crates/contextdb-hooks/src/patch.rs \
  --file crates/contextdb-hooks/src/paths.rs \
  --file crates/contextdb-hooks/src/render.rs \
  --file crates/contextdb-hooks/src/hook.rs \
  --file crates/contextdb-daemon/src/service.rs \
  --jobs "${MUTATION_JOBS:-4}" ${MUTATION_TIMEOUT:+--timeout "$MUTATION_TIMEOUT"} \
  --minimum-test-timeout 120 \
  --output results/p4-mutation "$@" -- \
  --package contextdb-proto --package contextdb-hooks --package contextdb-daemon --locked
