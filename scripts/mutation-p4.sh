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
  --jobs "${MUTATION_JOBS:-4}" --timeout "${MUTATION_TIMEOUT:-120}" \
  --output results/p4-mutation "$@" -- \
  --package contextdb-proto --package contextdb-hooks --package contextdb-daemon --locked
