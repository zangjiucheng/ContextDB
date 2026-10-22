#!/usr/bin/env bash
# Build ctx, ctxd, ctx-mcp and ctx-hook for one target and package them as
# contextdb-<os>-<arch>.tar.gz (binaries at the archive root, plus install.sh,
# LICENSE and a VERSION file). Used by .github/workflows/release.yml and by
# scripts/release.sh.
#
#   scripts/package-release.sh <rust-target> <out-dir>
set -euo pipefail
cd "$(dirname "$0")/.."
target=${1:?usage: scripts/package-release.sh <rust-target> <out-dir>}
out=${2:?usage: scripts/package-release.sh <rust-target> <out-dir>}
case $target in
  aarch64-apple-darwin) name=darwin-arm64 ;;
  x86_64-apple-darwin) name=darwin-amd64 ;;
  x86_64-unknown-linux-gnu) name=linux-amd64 ;;
  aarch64-unknown-linux-gnu) name=linux-arm64 ;;
  *) echo "unsupported target $target" >&2; exit 2 ;;
esac
cargo build --release --locked --target "$target" \
  -p contextdb-cli -p contextdb-daemon -p contextdb-mcp -p contextdb-hooks \
  --bin ctx --bin ctxd --bin ctx-mcp --bin ctx-hook
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
for b in ctx ctxd ctx-mcp ctx-hook; do
  install -m 755 "${CARGO_TARGET_DIR:-target}/$target/release/$b" "$dir/$b"
done
cp install.sh LICENSE "$dir/"
# CONTEXTDB_VERSION names the build where Git metadata is absent (the remote
# host's synced tree has no .git).
version=${CONTEXTDB_VERSION:-$(git describe --tags --always --dirty 2>/dev/null || echo unknown)}
printf '%s %s\n' "$version" "$target" > "$dir/VERSION"
mkdir -p "$out"
tar -C "$dir" -czf "$out/contextdb-$name.tar.gz" ctx ctxd ctx-mcp ctx-hook install.sh LICENSE VERSION
echo "$out/contextdb-$name.tar.gz"
