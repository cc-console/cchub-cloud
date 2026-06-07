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

> Need `tmux` on macOS/Linux, plus the agent CLI you use (`claude` / `codex` /
> `gemini`) on your `PATH`. Remote access additionally needs `cloudflared`.

### macOS
1. Download the **[.dmg](https://cc.cchub.cloud/#download)** and drag **cc-console** into Applications.
2. The build isn't code-signed yet, so the first launch is blocked. **Open a new Terminal window** (Spotlight → type "Terminal") and run the two commands below:
   ```bash
   xattr -dr com.apple.quarantine /Applications/cc-console.app   # clear the unsigned-app block
   brew install tmux cloudflared                                 # tmux is required; cloudflared only for remote access
   ```
3. Now open **cc-console** from Applications (or right-click the app → **Open**).

### Windows
Download the **[.exe installer](https://cc.cchub.cloud/#download)** and run it
(SmartScreen → More info → Run anyway). A built-in session engine — no tmux needed.

### Ubuntu server (headless)
Reach a remote box from your phone via your own `yourname.cchub.cloud` address:
```bash
# 1) get a device code: sign up at https://cc.cchub.cloud → set a name → Generate device token
# 2) on the server:
sudo apt install -y tmux
curl -fsSL https://github.com/cc-console/releases/releases/latest/download/install.sh | sh
cc-console link        # paste the ccd_… device code
cc-console             # opens at https://yourname.cchub.cloud
```
Full server guide (proxy, cloudflared, systemd persistence): **[cc.cchub.cloud/#server](https://cc.cchub.cloud/#server)**.

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
