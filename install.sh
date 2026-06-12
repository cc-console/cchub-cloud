#!/bin/sh
# cc-console installer — downloads a prebuilt binary from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/<owner>/cc-console/main/install.sh | sh
#
# No Rust toolchain needed; only `tmux` is required at runtime (macOS/Linux).
# Override the install dir with CC_CONSOLE_PREFIX, or the repo with CC_CONSOLE_REPO.
set -eu

REPO="${CC_CONSOLE_REPO:-cc-console/releases}"
BIN="cc-console"
PREFIX="${CC_CONSOLE_PREFIX:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin) plat="apple-darwin" ;;
  Linux)  plat="unknown-linux-musl" ;;   # static — runs on any glibc (old servers included)
  *) echo "✗ unsupported OS: $os (only macOS and Linux are supported)" >&2; exit 1 ;;
esac
case "$arch" in
  arm64|aarch64) cpu="aarch64" ;;
  x86_64|amd64)  cpu="x86_64" ;;
  *) echo "✗ unsupported CPU: $arch" >&2; exit 1 ;;
esac
target="${cpu}-${plat}"
url="https://github.com/${REPO}/releases/latest/download/${BIN}-${target}.tar.gz"

echo "→ platform: ${target}"
echo "→ downloading: ${url}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
if ! curl -fSL "$url" -o "$tmp/pkg.tar.gz"; then
  echo "✗ download failed. Check that a release exists for ${target} at github.com/${REPO}/releases" >&2
  exit 1
fi
tar -xzf "$tmp/pkg.tar.gz" -C "$tmp"
mkdir -p "$PREFIX"
install -m 0755 "$tmp/$BIN" "$PREFIX/$BIN"
echo "✓ installed ${PREFIX}/${BIN}"

# Post-install guidance (non-fatal).
if ! command -v tmux >/dev/null 2>&1; then
  echo "⚠ tmux not found — cc-console needs it. Install: 'brew install tmux' or 'sudo apt install tmux'"
fi
case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *) echo "⚠ ${PREFIX} is not on your PATH. Add this to your shell rc:"
     echo "    export PATH=\"$PREFIX:\$PATH\"" ;;
esac
echo "→ run: ${BIN} --help"
