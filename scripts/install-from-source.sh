#!/usr/bin/env bash
# Build cc-console from source and install it onto your PATH.
# For developers / users who have the Rust toolchain. Needs: cargo, tmux (runtime).
set -euo pipefail

cd "$(dirname "$0")/.."
PREFIX="${CC_CONSOLE_PREFIX:-$HOME/.local/bin}"

if ! command -v cargo >/dev/null 2>&1; then
  echo "✗ cargo (Rust) not found. Install from https://rustup.rs" >&2
  exit 1
fi

echo "→ building release binary (web assets are embedded)…"
cargo build --release

mkdir -p "$PREFIX"
install -m 0755 target/release/cc-console "$PREFIX/cc-console"
echo "✓ installed ${PREFIX}/cc-console"

if ! command -v tmux >/dev/null 2>&1; then
  echo "⚠ tmux not found — cc-console needs it. Install: 'brew install tmux' or 'sudo apt install tmux'"
fi
case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *) echo "⚠ ${PREFIX} is not on your PATH. Add: export PATH=\"$PREFIX:\$PATH\"" ;;
esac
echo "→ run: cc-console --help"
