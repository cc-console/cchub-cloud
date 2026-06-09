#!/usr/bin/env bash
# Build a distributable, self-contained tarball for one target triple.
# Usage: scripts/build-release.sh [TARGET]
#   TARGET defaults to the host triple. Output goes to ./dist/.
# Examples:
#   scripts/build-release.sh                          # host platform
#   scripts/build-release.sh aarch64-apple-darwin
#   scripts/build-release.sh x86_64-unknown-linux-gnu
set -euo pipefail

cd "$(dirname "$0")/.."
TARGET="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
OUT="${OUT:-dist}"

echo "→ target: ${TARGET}"
rustup target add "$TARGET" >/dev/null 2>&1 || true
cargo build --release --target "$TARGET"

mkdir -p "$OUT"
tarball="$OUT/cc-console-${TARGET}.tar.gz"
tar -C "target/${TARGET}/release" -czf "$tarball" cc-console
echo "✓ ${tarball}"
