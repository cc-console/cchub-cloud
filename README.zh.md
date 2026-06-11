<div align="center">

<img src="assets/logo.png" width="76" alt="cc-console"/>

# cc-console

### 把你终端里的 AI 编码 agent，实时装进手机。

cc-console 把你电脑终端里正在跑的 **Claude Code / Codex / Gemini** 会话，实时镜像到手机、平板或任意浏览器。随时查看、一键接管、在任何地方回它的 Yes/No —— 会话永不掉线。

[**🌐 官网**](https://cc.cchub.cloud) · [**⬇️ 下载**](https://cc.cchub.cloud/#download) · [**🔑 登录**](https://app.cchub.cloud) · [**🇬🇧 English**](README.md)

![platforms](https://img.shields.io/badge/macOS%20·%20Windows%20·%20Linux-supported-7c8cff)
[![downloads](https://img.shields.io/github/downloads/cc-console/releases/total?color=5ad1ff&label=downloads)](https://github.com/cc-console/releases/releases)
[![stars](https://img.shields.io/github/stars/cc-console/cchub-cloud?style=social)](https://github.com/cc-console/cchub-cloud)

<br/>

<img src="assets/demo.gif" width="640" alt="cc-console —— 随时随地接管你的终端 AI agent"/>

</div>

---

## 为什么用 cc-console

你的 agent 跑在桌面电脑上，但你不可能一整天守在屏幕前。cc-console 把它们正在进行的会话带到你身边 —— **而且代码和密钥不经过任何第三方服务器。**

- ⚡ **实时镜像** —— 和桌面完全同步的同一个终端。
- 📱 **手机随时接管** —— 通勤路上、沙发上,用手机就能回它的确认。
- 🧠 **多 agent 并行** —— Claude、Codex、Gemini 并排显示,各有配色。
- 🔔 **主动提醒** —— agent 停下来等你拍板 Yes/No 时主动通知你(声音 + 推送 + 标签角标)。一键 **Autopilot** 可自动确认,让长任务自己跑完。
- 📊 **用量与花费** —— 每个会话的实时 token 计量,以及聚合统计。
- 🔒 **本地优先 · 私密** —— 默认只在本机回环;远程走你自己的端到端隧道 + token 鉴权。无需开放端口,也不需要公网 IP。

## 安装

> macOS/Linux 需要 `tmux`,以及你要用的 agent CLI(`claude` / `codex` / `gemini`)在 `PATH` 里。远程访问还需要 `cloudflared`。

### macOS
1. 下载 **[.dmg](https://cc.cchub.cloud/#download)**,把 **cc-console** 拖进「应用程序」。
2. 安装包暂未签名,首次打开会被系统拦住。**新建一个终端窗口**(聚焦搜索 Spotlight → 输入「终端 / Terminal」),运行下面两条命令:
   ```bash
   xattr -dr com.apple.quarantine /Applications/cc-console.app   # 解除未签名应用的拦截
   brew install tmux cloudflared                                 # tmux 必装;cloudflared 仅远程访问需要
   ```
3. 现在从「应用程序」打开 **cc-console**(或右键点应用 →「打开」)即可。

### Windows
下载 **[.exe 安装程序](https://cc.cchub.cloud/#download)** 并运行(SmartScreen 弹窗点「更多信息 → 仍要运行」)。Windows 自带会话引擎 —— 无需 tmux。

### Ubuntu —— 桌面系统
1. 下载 **[.deb](https://cc.cchub.cloud/#download)**(或 AppImage)并安装:
   ```bash
   sudo dpkg -i cc-console-linux-x64.deb
   ```
2. 装依赖 —— **tmux**(必装)和 **cloudflared**(仅远程访问需要),一条一条来:
   ```bash
   sudo apt install -y tmux
   sudo curl -fsSL https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64 -o /usr/local/bin/cloudflared
   sudo chmod +x /usr/local/bin/cloudflared
   ```
   并确保你要用的 CLI(`claude` / `codex` / `gemini`)在 `PATH` 里。
3. 从应用菜单打开 **cc-console**。

### Ubuntu —— 服务器终端(无界面)
用你专属的 `你的名字.cchub.cloud` 地址,从手机直接访问一台远程服务器。先到 <https://cc.cchub.cloud> 注册 → 设一个名字 → 点 **「生成设备码」**(`ccd_…`)。然后 SSH 进服务器,逐块执行:

**1) 依赖 —— tmux + cloudflared(一条一条来):**
```bash
sudo apt install -y tmux
sudo curl -fsSL https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64 -o /usr/local/bin/cloudflared
sudo chmod +x /usr/local/bin/cloudflared
```

**2) cc-console(单独装):**
```bash
curl -fsSL https://github.com/cc-console/releases/releases/latest/download/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"     # 若提示不在 PATH
```

**3) 粘设备码绑定 + 启动:**
```bash
cc-console link        # 粘入 ccd_… 设备码
cc-console             # 启动后用 https://你的名字.cchub.cloud 访问
```

> 前置:服务器要能连上 AI 接口(必要时配代理),且你应先能自己跑通 `claude`。完整服务器指南(代理、systemd 常驻):**[cc.cchub.cloud/#server](https://cc.cchub.cloud/#server)**。

## 工作原理

```
  你的电脑                            Cloudflare 边缘             你,在任何地方
┌─────────────────────┐                                       ┌───────────────┐
│ claude / codex /     │   cc-console 守护进程                  │  手机 /       │
│ gemini  (tmux/pty)   │──►  (Rust,本地)   ──► 加密     ───► │  浏览器       │
│                      │     WebSocket 桥接     隧道           │  同一个会话   │
└─────────────────────┘                                       └───────────────┘
```
守护进程完全跑在你自己的机器上。远程访问是**可选**的 Cloudflare 隧道,且强制 token 鉴权 —— 你的代码和 API 密钥永远不碰我们的服务器。托管选项还能给你一个零配置、稳定的 `你的名字.cchub.cloud` 地址。

## 链接

- **官网 / 下载:** https://cc.cchub.cloud
- **账号 / 登录:** https://app.cchub.cloud
- **所有版本:** https://github.com/cc-console/releases/releases

---

<div align="center">

🇬🇧 English: **[README.md](README.md)**.

觉得有用的话,点个 ⭐ Star 支持一下。

**[⬇️ 下载](https://cc.cchub.cloud/#download)** · **[🌐 cc.cchub.cloud](https://cc.cchub.cloud)**

</div>
