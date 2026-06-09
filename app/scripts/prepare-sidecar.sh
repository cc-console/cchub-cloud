#!/bin/sh
# Build the cc-console daemon and drop it where Tauri expects a sidecar:
#   app/src-tauri/binaries/cc-console-<target-triple>[.exe]
# Tauri's `externalBin` resolves the triple suffix per platform, so both
# `tauri dev` and `tauri build` pick up the matching binary automatically.
set -eu

APP_DIR="$(cd "$(dirname "$0")/.." && pwd)"   # app/
ROOT_DIR="$(cd "$APP_DIR/.." && pwd)"          # repo root

# Allow building for a specific target in CI (e.g. cross/arm); default = host.
TARGET="${SIDECAR_TARGET:-$(rustc -vV | awk '/^host:/ {print $2}')}"

EXT=""
case "$TARGET" in
  *windows*) EXT=".exe" ;;
esac

echo "→ building cc-console daemon (release, target: $TARGET)…"
if [ "$TARGET" = "$(rustc -vV | awk '/^host:/ {print $2}')" ]; then
  ( cd "$ROOT_DIR" && cargo build --release --bin cc-console )
  SRC="$ROOT_DIR/target/release/cc-console$EXT"
else
  ( cd "$ROOT_DIR" && cargo build --release --bin cc-console --target "$TARGET" )
  SRC="$ROOT_DIR/target/$TARGET/release/cc-console$EXT"
fi

DEST_DIR="$APP_DIR/src-tauri/binaries"
DEST="$DEST_DIR/cc-console-$TARGET$EXT"
mkdir -p "$DEST_DIR"
cp "$SRC" "$DEST"
chmod +x "$DEST" 2>/dev/null || true
echo "✓ sidecar → $DEST"
