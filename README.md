<div align="center">

<img src="https://cc.cchub.cloud/favicon.svg" width="72" alt="cc-console"/>

# cc-console

### Your terminal AI coding agents — on your phone, in real time.

cc-console mirrors the **Claude Code / Codex / Gemini** sessions running in your
terminal to your phone, tablet, or any browser. Watch them live, take over with a
tap, answer their Yes/No prompts from anywhere — the session never drops.

[**🌐 Website**](https://cc.cchub.cloud) · [**⬇️ Download**](https://cc.cchub.cloud/#download) · [**🔑 Sign in**](https://app.cchub.cloud) · [**🇨🇳 中文**](#中文)

![platforms](https://img.shields.io/badge/macOS%20·%20Windows%20·%20Linux-supported-7c8cff)
[![downloads](https://img.shields.io/github/downloads/cc-console/releases/total?color=5ad1ff&label=downloads)](https://github.com/cc-console/releases/releases)
[![stars](https://img.shields.io/github/stars/cc-console/cc-hub-cloud?style=social)](https://github.com/cc-console/cc-hub-cloud)

<!-- TODO: drop a 10-second screen recording / GIF here — it's the single biggest
     conversion + star driver. e.g. ![demo](docs/demo.gif) -->

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
Download the **[.dmg](https://cc.cchub.cloud/#download)**, drag to Applications, open.
First launch is unsigned — right-click → **Open**, or:
```bash
xattr -dr com.apple.quarantine /Applications/cc-console.app
brew install tmux cloudflared        # tmux required; cloudflared for remote
```

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

<a name="中文"></a>

## 中文

**cc-console — 把你终端里的 AI 编码会话装进手机，实时接管。**

它把你电脑上正在跑的 **Claude Code / Codex / Gemini** 终端会话，实时镜像到手机和任意浏览器：随时查看、随手接管、在外面就能回它的 Yes/No，会话永不掉线。**代码和密钥不经过任何第三方服务器。**

- ⚡ 实时镜像桌面终端 · 📱 手机随时接管 · 🧠 多 agent 并行
- 🔔 后台会话需要你拍板时主动提醒；一键 **Autopilot** 自动确认长任务
- 📊 实时 token 与花费 · 🔒 默认只在本机，远程走你自己的隧道

**安装**：到 **[cc.cchub.cloud](https://cc.cchub.cloud)** 下载对应平台（macOS / Windows / Linux），双击即用。
**Ubuntu 服务器**：注册拿设备码 → 服务器 `install.sh` 安装 → `cc-console link` 粘设备码 → `cc-console` 启动，就能用 `你的名字.cchub.cloud` 随时访问。详见 **[cc.cchub.cloud/#server](https://cc.cchub.cloud/#server)**。

---

<div align="center">

如果觉得有用，点个 ⭐ Star 支持一下 · If this is useful, a ⭐ helps a lot.

**[⬇️ Download](https://cc.cchub.cloud/#download)** · **[🌐 cc.cchub.cloud](https://cc.cchub.cloud)**

</div>
