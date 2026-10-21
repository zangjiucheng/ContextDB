#!/usr/bin/env bash
# SPEC Gate P3 G4: cargo-fuzz on the IPC decoder and the MCP JSON layer
# (default 3600 s each, in parallel; FUZZ_JOBS workers per target).
set -euo pipefail
cd "$(dirname "$0")/../fuzz"
seconds=${FUZZ_SECONDS:-3600}
jobs=${FUZZ_JOBS:-8}
mkdir -p ../results/fuzz
pids=()
for target in ipc_request mcp_json; do
  cargo fuzz run "$target" --release -- -max_total_time="$seconds" -fork="$jobs" -ignore_crashes=0 \
    > "../results/fuzz/$target.log" 2>&1 &
  pids+=($!)
done
status=0
for p in "${pids[@]}"; do wait "$p" || status=1; done
for target in ipc_request mcp_json; do tail -3 "../results/fuzz/$target.log"; done
exit $status
