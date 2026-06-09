# cc-console desktop shell (Tauri)

Phase 1 of [docs/07-cross-platform-distribution.md](../docs/07-cross-platform-distribution.md):
a native window + installer around the existing `cc-console` daemon. No console
logic lives here — the shell spawns the daemon as a **sidecar** on a free
loopback port and points its webview at `http://127.0.0.1:<port>/`, the same UI
you get in a browser. The tunnel button, tmux bridge, usage tabs, etc. are all
the daemon's, unchanged.

```
app/
├─ ui/index.html            splash shown until the daemon is listening
├─ scripts/
│  ├─ prepare-sidecar.sh    builds the daemon → src-tauri/binaries/cc-console-<triple>
│  └─ gen-icon.cjs          regenerates icon-src.png (then: npx tauri icon icon-src.png)
└─ src-tauri/
   ├─ src/main.rs           spawns sidecar, waits for the port, navigates the window
   ├─ tauri.conf.json       bundle config (externalBin = the daemon)
   └─ binaries/             sidecar (gitignored; produced by prepare-sidecar.sh)
```

## Develop / build

```bash
cd app
npm install              # one-time: pulls @tauri-apps/cli
npm run dev              # builds the daemon sidecar, then `tauri dev`
npm run build            # release installers in src-tauri/target/release/bundle/
```

`npm run dev`/`build` run `prepare-sidecar.sh` first, so the sidecar always
matches your host triple.

## Install (local / small-scale)

`npm run build` produces, under `src-tauri/target/release/bundle/`:

- `dmg/cc-console_<version>_<arch>.dmg` — the installer
- `macos/cc-console.app` — the app itself

The build is **ad-hoc signed only** (no Apple Developer ID, not notarized) and is
built **for the host architecture** (e.g. `aarch64` on Apple Silicon; rebuild on
an Intel Mac for `x86_64`). That's fine for yourself or a few trusted people, but
Gatekeeper will warn on first open. To install:

1. Open the `.dmg` and drag **cc-console.app** into `/Applications`.
2. First launch is blocked by Gatekeeper ("cannot verify the developer"). Either
   **right-click → Open** once, or strip the quarantine flag:
   ```bash
   xattr -dr com.apple.quarantine /Applications/cc-console.app
   ```

No daemon to install separately — it's bundled as a sidecar and runs on a private
loopback port. First launch resolves your shell `PATH` (cached at
`~/.claude/cc-console/path-cache` for fast subsequent starts).

### Linux

Tauri can't be cross-built from macOS, so build the Linux artifacts **on a Linux
machine/VM** (the produced `.deb`/`.AppImage` match that machine's CPU arch).
Nothing in the repo is macOS-specific — `prepare-sidecar.sh` detects the host
triple and `npm run build` compiles the daemon sidecar for Linux automatically.

One-time prereqs (Ubuntu 22.04+ / Debian 12+; these versions ship WebKitGTK 4.1):

```bash
sudo apt-get update
sudo apt-get install -y build-essential curl nodejs npm \
  libwebkit2gtk-4.1-dev librsvg2-dev patchelf \
  libayatana-appindicator3-dev        # or libappindicator3-dev on older releases
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # if Rust absent
# Fedora: dnf install webkit2gtk4.1-devel librsvg2-devel patchelf rpm-build \
#   libappindicator-gtk3-devel + @development-tools
```

**Headless binary** (server + browser access — the same form as `install.sh`):

```bash
cargo build --release --bin cc-console
./target/release/cc-console            # serves http://127.0.0.1:7878
```

**Desktop GUI** (native window, `.deb` + `.AppImage`):

```bash
cd app
npm install
npm run build -- --bundles deb,appimage    # skip rpm unless rpmbuild is present
# → src-tauri/target/release/bundle/deb/cc-console_0.1.0_<arch>.deb
# → src-tauri/target/release/bundle/appimage/cc-console_0.1.0_<arch>.AppImage
```

Install the result:

```bash
sudo apt install ./cc-console_0.1.0_amd64.deb     # Debian/Ubuntu
# — or, portable, any distro:
chmod +x cc-console_0.1.0_amd64.AppImage && ./cc-console_0.1.0_amd64.AppImage
```

> For public, multi-platform, double-click-clean distribution (Developer ID
> signing + notarization, GitHub Releases via CI, auto-update), see
> [docs/07-cross-platform-distribution.md](../docs/07-cross-platform-distribution.md).

## Notes / not-yet

- **Windows**: the daemon no longer needs tmux — Phase 2's `SessionBackend`
  refactor landed, and on Windows the daemon defaults to the `PtyBackend` (ConPTY
  via `portable-pty`), so it self-multiplexes. Home-dir resolution
  (`%USERPROFILE%`) and the cloudflared tunnel are Windows-aware too. Still
  **unverified on real hardware**: ConPTY behavior must be smoke-tested from a
  Windows build (the `windows-msvc` CI job or a Windows box), and `claude` must be
  on `PATH`. Trade-off vs unix: sessions live only while the daemon runs (no
  detach/reattach). Treat the Windows installer as experimental until a Windows
  run confirms it.
- **Auto-updater**: off. Turning it on needs a minisign key (see
  `../docs/07-cross-platform-distribution.md` §5), then set
  `bundle.createUpdaterArtifacts: true` + a `plugins.updater` block.
- The single-binary `install.sh` distribution still exists for headless/server
  use; the desktop app is an additional option, not a replacement.
