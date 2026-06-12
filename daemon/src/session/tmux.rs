//! tmux session backend (unix). Drives a real tmux server: window-level commands
//! via the `tmux` CLI, and a single shared PTY running `tmux attach` whose bytes
//! are broadcast to every WS client. This is cc-console's original behavior,
//! unchanged — just moved behind [`SessionBackend`].

use std::io::{Read, Write};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;

use super::{RawWindow, SessionBackend, build_repaint};

const BROADCAST_BUFFER: usize = 1024;
const PTY_READ_BUFFER: usize = 4096;
const PTY_COLS: u16 = 120;
const PTY_ROWS: u16 = 40;

/// tmux session lifecycle + window-level commands (polling-based, not control mode).
#[derive(Debug, Clone)]
struct TmuxSession {
    socket: String,
    name: String,
}

impl TmuxSession {
    /// Idempotent: create the session running `initial_cmd` if it doesn't already exist.
    fn ensure(
        socket: &str,
        name: &str,
        initial_cmd: &str,
        env: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self> {
        // Advertise 24-bit color to programs in the session. claude renders its logo
        // in the brand orange only when COLORTERM signals truecolor; without it the
        // logo falls back to gray. xterm.js displays truecolor, and we enable tmux's
        // RGB passthrough below (`terminal-overrides …:Tc`) so the 24-bit escapes
        // aren't downsampled on the way out. User-set `[session.env]` still wins.
        let env = {
            let mut e = env.clone();
            e.entry("COLORTERM".to_string())
                .or_insert_with(|| "truecolor".to_string());
            e
        };
        let already = Command::new("tmux")
            .args(["-L", socket, "has-session", "-t", name])
            .status()
            .context("failed to spawn tmux (is it installed?)")?
            .success();

        if already {
            tracing::info!(socket, name, "tmux session already exists");
        } else {
            tracing::info!(socket, name, cmd = initial_cmd, "creating tmux session");
            // A pane's history-limit is locked at creation and the first window is
            // created by `new-session` itself, so pass the deep-scrollback setting
            // via a config file (`-f`) that takes effect at server start. Falls back
            // gracefully (no `-f`) if the file can't be written.
            let conf = write_tmux_conf();
            let mut args: Vec<&str> = vec!["-L", socket];
            if let Some(ref c) = conf {
                args.push("-f");
                args.push(c);
            }
            args.extend(["new-session", "-d", "-s", name, initial_cmd]);
            // Seed the session env via the tmux SERVER's process environment rather
            // than `new-session -e KEY=VAL`: `-e` was only added in tmux 3.0, and
            // older tmux (2.x — still shipped by some distros / conda) aborts with
            // `unknown option -- e`. The server this command starts inherits these,
            // so the first window's command (claude) sees them; apply_env()
            // (set-environment) covers later windows and an already-running server.
            let status = Command::new("tmux")
                .args(&args)
                .envs(env.iter())
                .status()
                .context("failed to spawn tmux new-session")?;
            if !status.success() {
                anyhow::bail!("tmux new-session exited with {status}");
            }
        }

        let session = Self {
            socket: socket.to_string(),
            name: name.to_string(),
        };
        // Always (re)apply: an already-running tmux server — persisted from an earlier
        // launch before the user added a proxy — won't have these. `set-environment`
        // updates the session env so newly opened windows pick them up.
        session.apply_env(&env);
        session.apply_theme();
        Ok(session)
    }

    /// Push `[session.env]` into the session environment so every window the user
    /// opens (where `claude`/`codex`/`gemini` actually run) inherits it. Best-effort.
    fn apply_env(&self, env: &std::collections::BTreeMap<String, String>) {
        for (k, v) in env {
            if let Err(e) = self
                .tmux()
                .args(["set-environment", "-t", &self.name, k, v])
                .status()
            {
                tracing::debug!(?e, key = %k, "tmux set-environment failed");
            }
        }
    }

    /// Dark status-bar / pane-border theme matching the web console. Best-effort:
    /// applied with `-g` on this isolated tmux server (`-L`), so it never touches the
    /// user's other tmux sessions, and any failure is non-fatal.
    fn apply_theme(&self) {
        const OPTS: &[(&str, &str)] = &[
            // Deep scrollback so the web client can scroll far back through a
            // window's history. `set-option -g` only reaches windows opened *after*
            // this runs; the first window gets it from the `-f` config at server
            // start (see `write_tmux_conf`). Keep this in sync with the xterm
            // client's `scrollback` (web/index.html) — sending more on a
            // window-switch repaint would just be dropped by xterm.
            ("history-limit", "50000"),
            // Strip alt-screen caps so `tmux attach` stays in the main screen buffer
            // (the alt buffer has no scrollback). Applied here too — not just in the
            // `-f` config — so it's in effect before the attach even on a reused
            // server. See `write_tmux_conf` for the full rationale.
            // `…:Tc` advertises 24-bit (truecolor) support so tmux passes RGB escapes
            // through to the xterm.js client instead of quantizing them to 256 colors
            // (keeps claude's logo in its true brand orange).
            ("terminal-overrides", ",*:smcup@:rmcup@,*:Tc"),
            ("status", "on"),
            ("status-style", "bg=#0d0e13,fg=#9aa0ad"),
            ("status-justify", "left"),
            ("status-left-style", "fg=#7c8cff,bold"),
            ("status-left", " #S "),
            ("status-left-length", "40"),
            ("status-right-style", "fg=#646b78"),
            ("status-right", " #H "),
            ("status-right-length", "60"),
            ("window-status-style", "fg=#646b78"),
            ("window-status-format", " #I #W "),
            ("window-status-current-style", "bg=#6d83ff,fg=#ffffff,bold"),
            ("window-status-current-format", " #I #W "),
            ("window-status-separator", ""),
            ("pane-border-style", "fg=#262833"),
            ("pane-active-border-style", "fg=#7c8cff"),
            ("message-style", "bg=#161620,fg=#e7e9ee"),
            ("mode-style", "bg=#6d83ff,fg=#ffffff"),
        ];
        for &(opt, val) in OPTS {
            if let Err(e) = self.tmux().args(["set-option", "-g", opt, val]).status() {
                tracing::debug!(?e, opt, "tmux set-option failed");
            }
        }
    }

    fn label(&self) -> String {
        format!("{}:{}", self.socket, self.name)
    }

    fn tmux(&self) -> Command {
        let mut c = Command::new("tmux");
        c.args(["-L", &self.socket]);
        c
    }

    /// `list-windows -F` with a `|`-delimited format. Returns one RawWindow per line.
    fn list_windows(&self) -> Result<Vec<RawWindow>> {
        let out = self
            .tmux()
            .args([
                "list-windows",
                "-t",
                &self.name,
                "-F",
                "#{window_id}|#{window_index}|#{window_name}|#{window_active}|#{pane_current_path}|#{pane_current_command}|#{pane_start_command}",
            ])
            .output()
            .context("tmux list-windows failed to spawn")?;
        if !out.status.success() {
            anyhow::bail!(
                "tmux list-windows: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut windows = Vec::new();
        for line in text.lines() {
            let parts: Vec<&str> = line.splitn(7, '|').collect();
            if parts.len() != 7 {
                tracing::warn!(line = %line, "skipping malformed list-windows row");
                continue;
            }
            let Ok(index) = parts[1].parse() else { continue };
            windows.push(RawWindow {
                id: parts[0].to_string(),
                index,
                name: parts[2].to_string(),
                active: parts[3] == "1",
                cwd: parts[4].to_string(),
                current_command: parts[5].to_string(),
                start_command: parts[6].to_string(),
            });
        }
        Ok(windows)
    }

    /// Visible text of a window's active pane (ANSI stripped). Used to detect
    /// Claude confirmation prompts in windows that aren't the attached/active one.
    fn capture_pane(&self, window_id: &str) -> Result<String> {
        let out = self
            .tmux()
            .args(["capture-pane", "-p", "-t", window_id])
            .output()
            .context("tmux capture-pane failed to spawn")?;
        if !out.status.success() {
            anyhow::bail!(
                "tmux capture-pane: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// `capture-pane -p -e` over an explicit line range (`-S start -E end`), with
    /// colours. Line numbers: `-` = start of history, `0` = top of the visible
    /// screen, positives go down, negatives are scrollback.
    fn capture_range(&self, start: &str, end: &str) -> Result<String> {
        let out = self
            .tmux()
            .args(["capture-pane", "-p", "-e", "-S", start, "-E", end, "-t", &self.name])
            .output()
            .context("tmux capture-pane failed to spawn")?;
        if !out.status.success() {
            anyhow::bail!(
                "tmux capture-pane: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Full repaint payload (see the trait doc): scrollback + screen padded to the
    /// pane height + a trailing CUP so the caret matches tmux's real cursor cell.
    fn capture_repaint(&self) -> Result<String> {
        // Pane geometry + real cursor (0-based, relative to the visible pane top-left).
        let meta = self
            .tmux()
            .args([
                "display-message", "-p", "-t", &self.name, "-F",
                "#{pane_height}\t#{cursor_y}\t#{cursor_x}",
            ])
            .output()
            .context("tmux display-message failed to spawn")?;
        if !meta.status.success() {
            anyhow::bail!(
                "tmux display-message: {}",
                String::from_utf8_lossy(&meta.stderr).trim()
            );
        }
        let meta = String::from_utf8_lossy(&meta.stdout);
        let mut it = meta.trim().split('\t');
        let height: usize = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let cursor_y: usize = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let cursor_x: usize = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);

        // If geometry is unreadable, fall back to a plain full-buffer repaint (no
        // cursor fix) rather than risk a malformed range.
        if height == 0 {
            let all = self.capture_range("-", "-")?;
            return Ok(format!("\x1b[H\x1b[2J{}", all.replace('\n', "\r\n")));
        }

        // Scrollback above the visible screen, then the visible screen itself.
        let history = self.capture_range("-", "-1")?;
        let screen = self.capture_range("0", &(height - 1).to_string())?;
        Ok(build_repaint(&history, &screen, height, cursor_y, cursor_x))
    }

    fn select_window(&self, window_id: &str) -> Result<()> {
        let status = self.tmux().args(["select-window", "-t", window_id]).status()?;
        if !status.success() {
            anyhow::bail!("tmux select-window failed");
        }
        Ok(())
    }

    fn send_keys(&self, window_id: &str, keys: &str) -> Result<()> {
        let status = self
            .tmux()
            .args(["send-keys", "-t", window_id, keys])
            .status()?;
        if !status.success() {
            anyhow::bail!("tmux send-keys failed");
        }
        Ok(())
    }

    fn new_window(&self, name: Option<&str>, cwd: Option<&str>, cmd: Option<&str>) -> Result<()> {
        let mut args: Vec<String> = vec!["new-window".into(), "-t".into(), self.name.clone()];
        if let Some(n) = name {
            args.push("-n".into());
            args.push(n.into());
        }
        if let Some(c) = cwd {
            args.push("-c".into());
            args.push(c.into());
        }
        if let Some(c) = cmd {
            args.push(c.into());
        }
        let status = self.tmux().args(&args).status()?;
        if !status.success() {
            anyhow::bail!("tmux new-window failed");
        }
        Ok(())
    }

    /// Send `cd "<path>"` + Enter to the window's pane. `-l` sends the command
    /// literally (so tmux doesn't treat it as key names). Only meaningful when the
    /// pane is at a shell prompt.
    fn change_dir(&self, window_id: &str, path: &str) -> Result<()> {
        let cmd = format!("cd \"{}\"", path.replace('"', ""));
        let s1 = self
            .tmux()
            .args(["send-keys", "-t", window_id, "-l", &cmd])
            .status()?;
        if !s1.success() {
            anyhow::bail!("tmux send-keys (cd) failed");
        }
        let s2 = self
            .tmux()
            .args(["send-keys", "-t", window_id, "Enter"])
            .status()?;
        if !s2.success() {
            anyhow::bail!("tmux send-keys (Enter) failed");
        }
        Ok(())
    }

    fn rename_window(&self, window_id: &str, name: &str) -> Result<()> {
        // automatic-rename / allow-rename would revert a manual name; turn both off.
        let _ = self
            .tmux()
            .args(["set-window-option", "-t", window_id, "automatic-rename", "off"])
            .status();
        let _ = self
            .tmux()
            .args(["set-window-option", "-t", window_id, "allow-rename", "off"])
            .status();
        let status = self
            .tmux()
            .args(["rename-window", "-t", window_id, name])
            .status()?;
        if !status.success() {
            anyhow::bail!("tmux rename-window failed");
        }
        Ok(())
    }

    fn kill_window(&self, window_id: &str) -> Result<()> {
        let status = self.tmux().args(["kill-window", "-t", window_id]).status()?;
        if !status.success() {
            anyhow::bail!("tmux kill-window failed");
        }
        Ok(())
    }
}

/// Write a minimal tmux config that raises the scrollback (`history-limit`) and
/// return its path, for passing to `new-session` via `-f`. Unlike `set-option -g`,
/// a `-f` config is read at server start, so even the first window — created by
/// `new-session` before any later option-setting runs — gets the deep history.
/// Returns `None` (and the caller skips `-f`) if the file can't be written.
fn write_tmux_conf() -> Option<String> {
    let path = crate::paths::home_dir()?.join(".claude/cc-console/cc-tmux.conf");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // `terminal-overrides …:smcup@:rmcup@` strips the alternate-screen enter/leave
    // caps, so `tmux attach` does NOT switch the client into the alt-screen buffer
    // (which has no scrollback). Without this the web terminal only ever shows the
    // last page and can't scroll up — and our injected history lands in the
    // unscrollable alt buffer. With it, output stays in the main screen and
    // scrollback (live + the repaint's injected history) works.
    std::fs::write(
        &path,
        "set -g history-limit 50000\nset -g terminal-overrides \",*:smcup@:rmcup@,*:Tc\"\n",
    )
    .ok()?;
    Some(path.display().to_string())
}

/// tmux-backed [`SessionBackend`]: a `TmuxSession` for window ops plus one shared
/// PTY running `tmux attach`, whose bytes are broadcast to all clients.
pub struct TmuxBackend {
    session: TmuxSession,
    /// The attach PTY's master, kept so we can resize.
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// The attach PTY's writer (client input → tmux).
    writer: Mutex<Box<dyn Write + Send>>,
    /// Last geometry the shared PTY was set to.
    size: Mutex<(u16, u16)>,
    output_tx: broadcast::Sender<Vec<u8>>,
}

impl TmuxBackend {
    /// Ensure the tmux session exists, attach a PTY to it, and start broadcasting
    /// its output. Mirrors the old inline setup in `server::run`.
    pub fn ensure(
        socket: &str,
        name: &str,
        initial_cmd: &str,
        env: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self> {
        let session = TmuxSession::ensure(socket, name, initial_cmd, env)?;
        tracing::info!(label = %session.label(), "tmux session ready");

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: PTY_ROWS,
                cols: PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open PTY")?;

        let mut cmd = CommandBuilder::new("tmux");
        cmd.args(["-L", &session.socket, "attach", "-t", &session.name]);
        // Under launchd the daemon inherits a minimal env with no TERM/LANG;
        // `tmux attach` needs a usable TERM or it exits instantly. Set them so the
        // bridge survives regardless of how the daemon was launched.
        if std::env::var_os("TERM").is_none() {
            cmd.env("TERM", "xterm-256color");
        }
        if std::env::var_os("LANG").is_none() {
            cmd.env("LANG", "en_US.UTF-8");
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn tmux client in PTY")?;
        drop(pair.slave);
        tracing::info!(pid = ?child.process_id(), "spawned tmux client in PTY");

        let pty_reader = pair.master.try_clone_reader().context("PTY reader clone failed")?;
        let pty_writer = pair.master.take_writer().context("PTY writer take failed")?;

        let (output_tx, _) = broadcast::channel::<Vec<u8>>(BROADCAST_BUFFER);
        {
            let tx = output_tx.clone();
            std::thread::Builder::new()
                .name("pty-reader".into())
                .spawn(move || {
                    let mut reader = pty_reader;
                    let mut buf = [0u8; PTY_READ_BUFFER];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) => {
                                tracing::warn!("PTY EOF; reader exiting");
                                break;
                            }
                            Ok(n) => {
                                let _ = tx.send(buf[..n].to_vec());
                            }
                            Err(e) => {
                                tracing::error!(?e, "PTY read error");
                                break;
                            }
                        }
                    }
                })
                .context("failed to spawn pty-reader thread")?;
        }

        Ok(Self {
            session,
            master: Mutex::new(pair.master),
            writer: Mutex::new(pty_writer),
            size: Mutex::new((PTY_COLS, PTY_ROWS)),
            output_tx,
        })
    }
}

impl SessionBackend for TmuxBackend {
    fn label(&self) -> String {
        self.session.label()
    }
    fn list_windows(&self) -> Result<Vec<RawWindow>> {
        self.session.list_windows()
    }
    fn new_window(&self, name: Option<&str>, cwd: Option<&str>, cmd: Option<&str>) -> Result<()> {
        self.session.new_window(name, cwd, cmd)
    }
    fn select_window(&self, id: &str) -> Result<()> {
        self.session.select_window(id)
    }
    fn rename_window(&self, id: &str, name: &str) -> Result<()> {
        self.session.rename_window(id, name)
    }
    fn change_dir(&self, id: &str, path: &str) -> Result<()> {
        self.session.change_dir(id, path)
    }
    fn kill_window(&self, id: &str) -> Result<()> {
        self.session.kill_window(id)
    }
    fn send_keys(&self, id: &str, keys: &str) -> Result<()> {
        self.session.send_keys(id, keys)
    }
    fn capture_pane(&self, id: &str) -> Result<String> {
        self.session.capture_pane(id)
    }
    fn capture_repaint(&self) -> Result<String> {
        self.session.capture_repaint()
    }

    fn write_input(&self, data: &[u8]) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(data).and_then(|_| w.flush()).context("PTY write failed")?;
        Ok(())
    }
    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .lock()
            .unwrap()
            .resize(PtySize { cols, rows, pixel_width: 0, pixel_height: 0 })
            .context("PTY resize failed")?;
        *self.size.lock().unwrap() = (cols, rows);
        Ok(())
    }
    fn output(&self) -> broadcast::Sender<Vec<u8>> {
        self.output_tx.clone()
    }
}
