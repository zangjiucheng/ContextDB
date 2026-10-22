#!/usr/bin/env bash
# Install ctx, ctxd, ctx-mcp and ctx-hook from this checkout into one bin
# directory (cargo's default, ~/.cargo/bin, or --root DIR -> DIR/bin).
# `ctx setup` expects all four side by side, built from the same commit.
#
#   scripts/install.sh [--root DIR]
set -euo pipefail
cd "$(dirname "$0")/.."
root=()
if [[ ${1:-} == --root ]]; then
  root=(--root "${2:?usage: scripts/install.sh [--root DIR]}")
fi
for spec in contextdb-daemon:ctxd contextdb-mcp:ctx-mcp contextdb-hooks:ctx-hook contextdb-cli:ctx; do
  cargo install --locked --force --path "crates/${spec%%:*}" --bin "${spec##*:}" ${root[@]+"${root[@]}"}
done
bin=${2:-${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}}/bin
echo
echo "installed ctx, ctxd, ctx-mcp, ctx-hook into $bin ($(git rev-parse --short HEAD))"
echo "next, in a repository: ctx setup"
