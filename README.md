<div align="center">

<img src="assets/logo.png" width="76" alt="cc-console"/>

# cc-console

### Your terminal AI coding agents — on your phone, in real time.

cc-console mirrors the **Claude Code / Codex / Gemini** sessions running in your
terminal to your phone, tablet, or any browser. Watch them live, take over with a
tap, answer their Yes/No prompts from anywhere — the session never drops.

[**🌐 Website**](https://cc.cchub.cloud) · [**⬇️ Download**](https://cc.cchub.cloud/#download) · [**🔑 Sign in**](https://app.cchub.cloud) · [**🇨🇳 中文 README**](README.zh.md)

![platforms](https://img.shields.io/badge/macOS%20·%20Windows%20·%20Linux-supported-7c8cff)
[![downloads](https://img.shields.io/github/downloads/cc-console/releases/total?color=5ad1ff&label=downloads)](https://github.com/cc-console/releases/releases)
[![stars](https://img.shields.io/github/stars/cc-console/cchub-cloud?style=social)](https://github.com/cc-console/cchub-cloud)

<br/>

<img src="assets/demo.gif" width="640" alt="cc-console — take over your terminal AI agents from anywhere"/>

</div>

---

## Why cc-console

Your agents run on your desktop, but you can't sit there all day. cc-console brings
their live sessions to wherever you are — **without sending your code or keys
through anyone else's servers.**

- ⚡ **Real-time mirror** — the same terminal as your desktop, perfectly in sync.
- 📱 **Take over on mobile** — approve prompts from the couch or the train.
- 🧠 **Multiple agents at once** — Claude, Codex & Gemini side by side, colour-coded.
- 🔔 **Proactive alerts** — when an agent stops for a Yes/No, it pings you (sound +
  push + tab badge). One-tap **Autopilot** can auto-confirm so long tasks finish.
- 📊 **Usage & cost** — live token accounting per session and in aggregate.
- 🔒 **Local-first & private** — loopback-only by default; remote goes through your
  own end-to-end tunnel with token auth. No open ports, no public IP required.

## Install

> 📖 **Full step-by-step guide (with screenshots):** **[Operation Manual](https://my.feishu.cn/docx/ZFoKdHWU1o1mcQxzqNWciXu0nGe)**

### 1. macOS (Apple Silicon, M1–M5)
**Step 1 — Download** the installer from **[cc.cchub.cloud/#download](https://cc.cchub.cloud/#download)** and drag **cc-console** into Applications.

**Step 2 — First launch.** To keep development costs down the app isn't Apple-signed yet, so macOS blocks it as "damaged". Run this once in Terminal, then double-click as normal:
```bash
xattr -dr com.apple.quarantine /Applications/cc-console.app
```

**Step 3 — Dependencies.** `tmux` is required to run sessions; `cloudflared` is only needed for phone/remote access:
```bash
brew install tmux cloudflared
```
Also keep the agent CLI you use (`claude` / `codex` / `gemini`) on your `PATH`.

### 2. Windows
**Step 1 — Download** the **[.exe installer](https://cc.cchub.cloud/#download)**.

**Step 2 — Install.** Double-click it; if SmartScreen warns it's unsafe, click **More info → Run anyway**. A built-in session engine is included — no tmux needed.

**Step 3 — Launch** cc-console from the Start menu.

### 3. Ubuntu — desktop
**Step 1 — Download** the **[.deb](https://cc.cchub.cloud/#download)** and double-click to install (or `sudo dpkg -i cc-console-linux-x64.deb`).

**Step 2 — Install tmux** (required to run sessions):
```bash
sudo apt install -y tmux
```

**Step 3 — Install cloudflared** (the tunnel tool; only needed for remote access). You can also download it from GitHub locally and upload it to the box:
```bash
sudo curl -fL --progress-bar \
  https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64 \
  -o /usr/local/bin/cloudflared
sudo chmod +x /usr/local/bin/cloudflared
cloudflared --version
```

**Step 4 — Launch** cc-console from your applications menu. Keep your agent CLI (`claude` / `codex` / `gemini`) on `PATH`.

### 4. Server (Ubuntu, headless)
> Prerequisite: the server can already run `claude` / `codex` / `gemini` (set up a proxy first if the AI APIs aren't reachable).

**Step 1 — Install tmux:**
```bash
sudo apt install -y tmux
```

**Step 2 — Install cloudflared** (or download from GitHub locally and upload it):
```bash
sudo curl -fL --progress-bar \
  https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64 \
  -o /usr/local/bin/cloudflared
sudo chmod +x /usr/local/bin/cloudflared
cloudflared --version
```

**Step 3 — Install cc-console and add it to PATH:**
```bash
curl -fsSL https://github.com/cc-console/releases/releases/latest/download/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
```

**Step 4 — Bind your device code.** Sign up at **[app.cchub.cloud/account](https://app.cchub.cloud/account)**, pick a username (it becomes your `yourname.cchub.cloud` address), click **Generate device token** to get a `ccd_…` code, then on the server paste it in:
```bash
cc-console link        # paste the ccd_… device code
```

**Step 5 — Run.** Start the tool; it prints a `https://yourname.cchub.cloud/?token=…` address:
```bash
cc-console
```

**Step 6 — Open from any device.** Visit that `https://yourname.cchub.cloud/?token=…` URL on your phone or browser to drive the server's claude / codex / gemini live.

**Keep it running after you log out** (nohup — works even in containers with no systemd):
```bash
export PATH="$HOME/.local/bin:$PATH"
nohup cc-console > ~/cc-console.log 2>&1 &
disown
```
- Logs / address: `tail -f ~/cc-console.log`  ·  Running? `pgrep -af cc-console`
- Stop: `pkill -f cc-console`  ·  Restart: `pkill -f cc-console; sleep 1; nohup cc-console > ~/cc-console.log 2>&1 & disown`

## How it works

```
  Your machine                        Cloudflare edge            You, anywhere
┌─────────────────────┐                                       ┌───────────────┐
│ claude / codex /     │   cc-console daemon                   │  phone /      │
│ gemini  (tmux/pty)   │──►  (Rust, local)  ──► encrypted ───► │  browser      │
│                      │     WebSocket bridge   tunnel         │  same session │
└─────────────────────┘                                       └───────────────┘
```
The daemon runs entirely on your machine. Remote access is an **opt-in** Cloudflare
tunnel with forced token auth — your code and API keys never touch our servers. A
hosted option gives you a stable `yourname.cchub.cloud` address with zero setup.

## Links

- **Website / download:** https://cc.cchub.cloud
- **Account / sign in:** https://app.cchub.cloud
- **All releases:** https://github.com/cc-console/releases/releases

---

<div align="center">

🇨🇳 中文说明见 **[README.zh.md](README.zh.md)**.

If this is useful, a ⭐ helps a lot.

**[⬇️ Download](https://cc.cchub.cloud/#download)** · **[🌐 cc.cchub.cloud](https://cc.cchub.cloud)**

</div>
