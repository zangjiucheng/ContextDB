#!/usr/bin/env bash
# ContextDB installer: downloads a release build of ctx, ctxd, ctx-mcp and
# ctx-hook, verifies its SHA-256 checksum and installs the binaries.
#
#   curl -fsSL https://raw.githubusercontent.com/zangjiucheng/ContextDB/main/install.sh | bash
#   bash install.sh [--version vX.Y.Z] [--dir DIR] [--setup] [--uninstall]
#
# --version   release tag (default: latest)
# --dir       install directory (default: ~/.local/bin, or $CONTEXTDB_INSTALL_DIR)
# --setup     run `ctx setup` in the current directory afterwards
# --uninstall remove the four binaries (run `ctx service uninstall <dir>` and
#             `ctx hooks uninstall ...` in your repositories first)
#
# Downloads use an authenticated `gh` when available (needed while the
# repository is private), otherwise public release URLs. CONTEXTDB_BASE_URL
# points at another location holding the archives and checksums.txt (a mirror,
# or file:// for testing).
set -euo pipefail

repo=${CONTEXTDB_REPO:-zangjiucheng/ContextDB}
version=latest
dir=${CONTEXTDB_INSTALL_DIR:-$HOME/.local/bin}
setup=0
uninstall=0
bins=(ctx ctxd ctx-mcp ctx-hook)

while [[ $# -gt 0 ]]; do
  case $1 in
    --version) version=${2:?--version needs a tag}; shift 2 ;;
    --version=*) version=${1#*=}; shift ;;
    --dir) dir=${2:?--dir needs a path}; shift 2 ;;
    --dir=*) dir=${1#*=}; shift ;;
    --setup) setup=1; shift ;;
    --uninstall) uninstall=1; shift ;;
    -h|--help) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "install.sh: unknown option $1" >&2; exit 2 ;;
  esac
done

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

if [[ $uninstall == 1 ]]; then
  for b in "${bins[@]}"; do rm -f "$dir/$b"; done
  say "removed ${bins[*]} from $dir"
  exit 0
fi

case $(uname -s) in
  Darwin) os=darwin ;;
  Linux) os=linux ;;
  *) die "unsupported OS $(uname -s) (macOS and Linux only)" ;;
esac
case $(uname -m) in
  arm64|aarch64) arch=arm64 ;;
  x86_64|amd64) arch=amd64 ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac
asset=contextdb-$os-$arch.tar.gz

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fetch() { # fetch <file>
  if [[ -n ${CONTEXTDB_BASE_URL:-} ]]; then
    curl -fsSL "$CONTEXTDB_BASE_URL/$1" -o "$tmp/$1"
  elif command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
    local tag=()
    [[ $version != latest ]] && tag=("$version")
    gh release download ${tag[@]+"${tag[@]}"} --repo "$repo" --pattern "$1" --dir "$tmp" --clobber
  else
    local base="https://github.com/$repo/releases"
    if [[ $version == latest ]]; then base+=/latest/download; else base+=/download/$version; fi
    curl -fsSL "$base/$1" -o "$tmp/$1" ||
      die "download failed: $base/$1 (private repository? log in with 'gh auth login' first)"
  fi
}
say "downloading $asset ($version) from $repo"
fetch checksums.txt
fetch "$asset"

# Verify before anything is extracted or installed.
expected=$(awk -v f="$asset" '$2 == f || $2 == "*"f {print $1}' "$tmp/checksums.txt")
[[ -n $expected ]] || die "$asset is not listed in checksums.txt"
if command -v sha256sum >/dev/null; then
  actual=$(sha256sum "$tmp/$asset" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')
fi
[[ $actual == "$expected" ]] || die "checksum mismatch for $asset (expected $expected, got $actual)"

tar -C "$tmp" -xzf "$tmp/$asset"
mkdir -p "$dir"
for b in "${bins[@]}"; do
  [[ -f $tmp/$b ]] || die "$asset lacks $b"
  install -m 755 "$tmp/$b" "$dir/$b"
  if [[ $os == darwin ]]; then
    xattr -d com.apple.quarantine "$dir/$b" 2>/dev/null || true
  fi
done
say "installed ${bins[*]} into $dir ($(cat "$tmp/VERSION" 2>/dev/null || echo "$version"))"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) say "note: $dir is not on PATH; add: export PATH=\"$dir:\$PATH\"" ;;
esac
if pgrep -x ctxd >/dev/null 2>&1; then
  say "note: a ctxd is running; restart it to use the new build (ctx service restart <data dir>, or ctx shutdown <data dir> then ctx setup)"
fi

if [[ $setup == 1 ]]; then
  "$dir/ctx" setup
else
  say "next, in a repository: ctx setup   (then optionally: ctx service install .contextdb/db)"
fi
