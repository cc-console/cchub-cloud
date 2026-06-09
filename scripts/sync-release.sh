#!/usr/bin/env bash
# Publish a release from the private build repo to the PUBLIC download repo.
#
# The release workflow (.github/workflows/release.yml) builds binaries + desktop
# installers and attaches them to a GitHub Release in THIS repo (the private
# `cc-console/cc.console`). End users, however, download from the public
# `cc-console/releases` repo (that's what install.sh and the landing page point
# at). This script copies the assets across, renaming the desktop bundles from
# Tauri's raw names to the friendly names the public release uses, and bundling
# install.sh.
#
# Usage:
#   scripts/sync-release.sh v0.2.0            # do it
#   scripts/sync-release.sh v0.2.0 --dry-run  # show the asset mapping, upload nothing
#
# Requires: gh (authenticated with access to both repos).
set -euo pipefail

TAG="${1:-}"
DRY=""
[ "${2:-}" = "--dry-run" ] && DRY=1
if [ -z "$TAG" ]; then echo "usage: $0 <tag> [--dry-run]" >&2; exit 2; fi

SRC="${SRC_REPO:-cc-console/cc.console}"
DST="${DST_REPO:-cc-console/releases}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

work="$(mktemp -d)"; raw="$work/raw"; out="$work/out"
mkdir -p "$raw" "$out"
trap 'rm -rf "$work"' EXIT

echo "→ source:  $SRC @ $TAG"
echo "→ target:  $DST @ $TAG"
echo "→ download assets…"
gh release download "$TAG" --repo "$SRC" --dir "$raw" --clobber

# Map each raw asset to its public name. Binaries (.tar.gz/.zip) already follow
# the cc-console-<target>.{tar.gz,zip} convention from release.yml, so they pass
# through unchanged. Desktop bundles get renamed by extension + arch keyword.
map_name() {
  local f="$1" base; base="$(basename "$f")"
  local lc; lc="$(printf '%s' "$base" | tr '[:upper:]' '[:lower:]')"
  case "$lc" in
    *.app.tar.gz|*.app.tar.gz.sig|*.sig) printf '' ;;                       # Tauri updater bundles: skip
    *.tar.gz|*.zip)            printf '%s' "$base" ;;                       # binaries: keep
    *aarch64*.dmg|*arm64*.dmg) printf 'cc-console-macos-arm64.dmg' ;;
    *x64*.dmg|*x86_64*.dmg)    printf 'cc-console-macos-x64.dmg' ;;
    *.dmg)                     printf 'cc-console-macos-arm64.dmg' ;;       # single dmg → arm64
    *.deb)                     printf 'cc-console-linux-x64.deb' ;;
    *.appimage)                printf 'cc-console-linux-x64.AppImage' ;;
    *.rpm)                     printf 'cc-console-linux-x64.rpm' ;;
    *setup.exe|*-setup.exe)    printf 'cc-console-windows-setup.exe' ;;
    *.exe)                     printf 'cc-console-windows-setup.exe' ;;     # NSIS installer
    *.msi)                     printf 'cc-console-windows.msi' ;;
    *)                         printf '%s' "$base" ;;
  esac
}

echo "→ asset mapping:"
shopt -s nullglob
for f in "$raw"/*; do
  dest="$(map_name "$f")"
  if [ -z "$dest" ]; then
    printf '   %-45s → (skip: updater artifact)\n' "$(basename "$f")"
    continue
  fi
  printf '   %-45s → %s\n' "$(basename "$f")" "$dest"
  cp "$f" "$out/$dest"
done
# Always ship the installer script so `curl .../latest/download/install.sh` works.
cp "$ROOT/install.sh" "$out/install.sh"
printf '   %-45s → %s\n' "install.sh (repo root)" "install.sh"

if [ -n "$DRY" ]; then
  echo "✓ dry-run: would publish $(ls -1 "$out" | wc -l | tr -d ' ') assets to $DST @ $TAG"
  exit 0
fi

notes="Automated sync of $TAG from $SRC. Install: \`curl -fsSL https://github.com/$DST/releases/latest/download/install.sh | sh\`"
if gh release view "$TAG" --repo "$DST" >/dev/null 2>&1; then
  echo "→ release $TAG exists on $DST — uploading/overwriting assets"
  gh release upload "$TAG" --repo "$DST" --clobber "$out"/*
else
  echo "→ creating release $TAG on $DST"
  gh release create "$TAG" --repo "$DST" --target main \
    --title "cc-console $TAG" --notes "$notes" "$out"/*
fi
echo "✓ published $DST @ $TAG"
