#!/usr/bin/env bash
# Local release, for when hosted GitHub Actions cannot run: builds the macOS
# archives here (arm64 and x86_64), the Linux x86_64 archive on the remote host
# (scripts/remote.sh), writes checksums.txt and publishes a GitHub release
# with gh (a full release: GitHub's "latest" skips pre-releases). Same archive layout as .github/workflows/release.yml.
#
#   scripts/release.sh <tag>            tag must point at HEAD; tree must be clean
#   scripts/release.sh <tag> --dry-run  build and checksum only, no upload
set -euo pipefail
cd "$(dirname "$0")/.."
tag=${1:?usage: scripts/release.sh <tag> [--dry-run]}
dry=${2:-}
[[ $(uname -s) == Darwin ]] || { echo "run on macOS (builds the darwin archives)" >&2; exit 2; }
[[ -z $(git status --porcelain) ]] || { echo "working tree is not clean" >&2; exit 2; }
[[ $(git rev-parse "$tag^{commit}" 2>/dev/null) == $(git rev-parse HEAD) ]] ||
  { echo "tag $tag must exist and point at HEAD" >&2; exit 2; }

dist=target/dist/$tag
rm -rf "$dist"
mkdir -p "$dist"
for t in aarch64-apple-darwin x86_64-apple-darwin; do
  rustup target add --toolchain 1.95.0 "$t" >/dev/null
  CONTEXTDB_VERSION=$tag bash scripts/package-release.sh "$t" "$dist"
done

# Linux x86_64 on the remote host; its archive comes back via results/.
job=release-$tag
bash scripts/remote.sh run "$job" env CONTEXTDB_VERSION="$tag" bash scripts/package-release.sh x86_64-unknown-linux-gnu results/release
bash scripts/remote.sh wait "$job"
bash scripts/remote.sh fetch
cp results/remote/release/contextdb-linux-amd64.tar.gz "$dist/"

(cd "$dist" && shasum -a 256 contextdb-*.tar.gz > checksums.txt && cat checksums.txt)
if [[ $dry == --dry-run ]]; then
  echo "dry run: archives in $dist"
  exit 0
fi
git push origin "$tag"
gh release create "$tag" "$dist"/* --title "ContextDB $tag" \
  --notes "Install: curl -fsSL https://raw.githubusercontent.com/zangjiucheng/ContextDB/main/install.sh | bash -s -- --version $tag (Local build: Linux arm64 is not included and the Linux x86_64 binary needs glibc >= 2.39.)"
