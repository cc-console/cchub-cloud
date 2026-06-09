//! PTY session backend (cross-platform, incl. Windows via ConPTY).
//!
//! Unlike [`super::tmux::TmuxBackend`], there is no external multiplexer: the
//! daemon owns one PTY per window directly (`portable-pty`) and multiplexes them
//! itself. A per-pane `vt100` parser tracks each window's screen so we can render
//! it for late-joiners and detect Claude prompts — the job tmux's `capture-pane`
//! used to do.
//!
//! MVP trade-offs (docs/07 §2, persistence option a): sessions live only as long
//! as the daemon process; a restart loses them, and no external client can attach.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;

use super::{RawWindow, SessionBackend, build_repaint, keyspec_to_bytes};

const BROADCAST_BUFFER: usize = 1024;
const PTY_READ_BUFFER: usize = 4096;
const SCROLLBACK: usize = 50000; // deep history; keep in sync with web/index.html xterm scrollback + tmux history-limit
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;

/// One window = one PTY + the child running in it + a vt100 view of its screen.
struct Pane {
    index: u32,
    name: String,
    cwd: String,
    /// The command line the pane was started with (drives agent classification).
    start_command: String,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    parser: Arc<Mutex<vt100::Parser>>,
}

struct Inner {
    panes: Mutex<HashMap<String, Pane>>,
    active: Mutex<Option<String>>,
    size: Mutex<(u16, u16)>, // (cols, rows)
    seq: AtomicU64,
    output_tx: broadcast::Sender<Vec<u8>>,
}

pub struct PtyBackend {
    inner: Arc<Inner>,
}

impl PtyBackend {
    /// Create the backend and its first window running `initial_cmd`.
    pub fn new(initial_cmd: &str) -> Result<Self> {
        let (output_tx, _) = broadcast::channel::<Vec<u8>>(BROADCAST_BUFFER);
        let inner = Arc::new(Inner {
            panes: Mutex::new(HashMap::new()),
            active: Mutex::new(None),
            size: Mutex::new((DEFAULT_COLS, DEFAULT_ROWS)),
            seq: AtomicU64::new(0),
            output_tx,
        });
        let backend = Self { inner };
        backend
            .spawn_window(None, None, Some(initial_cmd))
            .context("failed to start initial window")?;
        Ok(backend)
    }

    /// Spawn a new pane, register it, and make it active. Returns the new id.
    fn spawn_window(
        &self,
        name: Option<&str>,
        cwd: Option<&str>,
        cmd: Option<&str>,
    ) -> Result<String> {
        let (cols, rows) = *self.inner.size.lock().unwrap();
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("failed to open PTY")?;

        let cmdline = cmd.map(str::to_string).unwrap_or_else(default_shell);
        let cwd = cwd
            .map(str::to_string)
            .or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string()))
            .unwrap_or_default();

        let child = pair
            .slave
            .spawn_command(build_command(&cmdline, &cwd, cmd.is_some()))
            .context("failed to spawn pane command")?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().context("PTY reader clone failed")?;
        let writer = pair.master.take_writer().context("PTY writer take failed")?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK)));

        let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed);
        let id = format!("@{seq}");
        let index = seq as u32;
        let name = name
            .map(str::to_string)
            .unwrap_or_else(|| program_basename(&cmdline));

        // Reader thread: feed this pane's parser always; broadcast only while active.
        {
            let inner = self.inner.clone();
            let parser = parser.clone();
            let id = id.clone();
            std::thread::Builder::new()
                .name(format!("pty-reader-{id}"))
                .spawn(move || pane_reader_loop(inner, id, parser, reader))
                .context("failed to spawn pane reader thread")?;
        }

        self.inner.panes.lock().unwrap().insert(
            id.clone(),
            Pane {
                index,
                name,
                cwd,
                start_command: cmdline,
                master: pair.master,
                writer,
                child,
                parser,
            },
        );
        self.activate(&id);
        Ok(id)
    }

    /// Make `id` the active window: resize it to the current geometry and repaint
    /// every client with its current (coloured) screen.
    fn activate(&self, id: &str) {
        *self.inner.active.lock().unwrap() = Some(id.to_string());
        let (cols, rows) = *self.inner.size.lock().unwrap();
        let panes = self.inner.panes.lock().unwrap();
        let Some(pane) = panes.get(id) else { return };
        let _ = pane.master.resize(PtySize { cols, rows, pixel_width: 0, pixel_height: 0 });
        let mut redraw = b"\x1b[H\x1b[2J".to_vec();
        {
            let mut p = pane.parser.lock().unwrap();
            p.set_size(rows, cols);
            redraw.extend_from_slice(&p.screen().contents_formatted());
        }
        let _ = self.inner.output_tx.send(redraw);
    }

    /// Clone of the `Arc<Mutex<Parser>>` for `id`, so we can lock it without
    /// holding the panes map lock (avoids reader-thread contention/deadlock).
    fn parser_of(&self, id: &str) -> Option<Arc<Mutex<vt100::Parser>>> {
        self.inner.panes.lock().unwrap().get(id).map(|p| p.parser.clone())
    }
}

impl SessionBackend for PtyBackend {
    fn label(&self) -> String {
        "pty:main".to_string()
    }

    fn list_windows(&self) -> Result<Vec<RawWindow>> {
        let active = self.inner.active.lock().unwrap().clone();
        let panes = self.inner.panes.lock().unwrap();
        let mut v: Vec<RawWindow> = panes
            .iter()
            .map(|(id, p)| RawWindow {
                id: id.clone(),
                index: p.index,
                name: p.name.clone(),
                active: active.as_deref() == Some(id.as_str()),
                cwd: p.cwd.clone(),
                // No live foreground-process probe in MVP: approximate the current
                // command with the start command, which is enough for agent
                // classification (`claude`/`codex`/`gemini`) and `is_claude()`.
                current_command: program_basename(&p.start_command),
                start_command: program_basename(&p.start_command),
            })
            .collect();
        v.sort_by_key(|w| w.index);
        Ok(v)
    }

    fn new_window(&self, name: Option<&str>, cwd: Option<&str>, cmd: Option<&str>) -> Result<()> {
        self.spawn_window(name, cwd, cmd).map(|_| ())
    }

    fn select_window(&self, id: &str) -> Result<()> {
        if !self.inner.panes.lock().unwrap().contains_key(id) {
            anyhow::bail!("no such window {id}");
        }
        self.activate(id);
        Ok(())
    }

    fn rename_window(&self, id: &str, name: &str) -> Result<()> {
        let mut panes = self.inner.panes.lock().unwrap();
        let pane = panes.get_mut(id).context("no such window")?;
        pane.name = name.to_string();
        Ok(())
    }

    fn change_dir(&self, id: &str, path: &str) -> Result<()> {
        // Send `cd "<path>"` + Enter to this pane's shell. Unlike tmux there's no
        // live cwd probe here, so also update the stored cwd for the listing.
        let line = format!("cd \"{}\"\r", path.replace('"', ""));
        let mut panes = self.inner.panes.lock().unwrap();
        let pane = panes.get_mut(id).context("no such window")?;
        pane.writer.write_all(line.as_bytes()).and_then(|_| pane.writer.flush())?;
        pane.cwd = path.to_string();
        Ok(())
    }

    fn kill_window(&self, id: &str) -> Result<()> {
        let mut pane = self
            .inner
            .panes
            .lock()
            .unwrap()
            .remove(id)
            .context("no such window")?;
        let _ = pane.child.kill();
        // If we killed the active window, fall back to the lowest-index survivor.
        let mut active = self.inner.active.lock().unwrap();
        if active.as_deref() == Some(id) {
            let next = {
                let panes = self.inner.panes.lock().unwrap();
                panes
                    .iter()
                    .min_by_key(|(_, p)| p.index)
                    .map(|(k, _)| k.clone())
            };
            *active = next.clone();
            drop(active);
            if let Some(next) = next {
                self.activate(&next);
            }
        }
        Ok(())
    }

    fn send_keys(&self, id: &str, keys: &str) -> Result<()> {
        let bytes = keyspec_to_bytes(keys);
        let mut panes = self.inner.panes.lock().unwrap();
        let pane = panes.get_mut(id).context("no such window")?;
        pane.writer.write_all(&bytes).and_then(|_| pane.writer.flush())?;
        Ok(())
    }

    fn capture_pane(&self, id: &str) -> Result<String> {
        let parser = self.parser_of(id).context("no such window")?;
        let text = parser.lock().unwrap().screen().contents();
        Ok(text)
    }

    fn capture_repaint(&self) -> Result<String> {
        let active = self
            .inner
            .active
            .lock()
            .unwrap()
            .clone()
            .context("no active window")?;
        let parser = self.parser_of(&active).context("no active window")?;
        let mut guard = parser.lock().unwrap();

        // Live screen + real cursor (scrollback offset 0 = the bottom/live view).
        guard.set_scrollback(0);
        let (rows, cols) = guard.screen().size();
        let (cursor_y, cursor_x) = guard.screen().cursor_position();
        let screen = guard.screen().contents();

        // Include the scrollback (lines that scrolled off the top) so the client can
        // scroll back through history — without this it only ever gets the visible
        // screen and can't scroll up, especially after a window switch clears its
        // buffer.
        let rows_u = (rows as usize).max(1);
        let history = collect_scrollback(&mut guard, cols);
        guard.set_scrollback(0); // restore the live view for subsequent renders
        drop(guard);

        Ok(build_repaint(
            &history.join("\n"),
            &screen,
            rows_u,
            cursor_y as usize,
            cursor_x as usize,
        ))
    }

    fn write_input(&self, data: &[u8]) -> Result<()> {
        let active = self
            .inner
            .active
            .lock()
            .unwrap()
            .clone()
            .context("no active window")?;
        let mut panes = self.inner.panes.lock().unwrap();
        let pane = panes.get_mut(&active).context("active window vanished")?;
        pane.writer.write_all(data).and_then(|_| pane.writer.flush())?;
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        *self.inner.size.lock().unwrap() = (cols, rows);
        let active = self.inner.active.lock().unwrap().clone();
        if let Some(active) = active {
            let panes = self.inner.panes.lock().unwrap();
            if let Some(pane) = panes.get(&active) {
                let _ = pane.master.resize(PtySize { cols, rows, pixel_width: 0, pixel_height: 0 });
                pane.parser.lock().unwrap().set_size(rows, cols);
            }
        }
        Ok(())
    }

    fn output(&self) -> broadcast::Sender<Vec<u8>> {
        self.inner.output_tx.clone()
    }
}

/// Reader loop for one pane. Feeds the vt100 parser unconditionally; broadcasts
/// bytes only while this pane is the active one. On EOF the child has exited:
/// drop the pane so the window poller emits `WindowRemoved`.
fn pane_reader_loop(
    inner: Arc<Inner>,
    id: String,
    parser: Arc<Mutex<vt100::Parser>>,
    mut reader: Box<dyn Read + Send>,
) {
    let mut buf = [0u8; PTY_READ_BUFFER];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                parser.lock().unwrap().process(&buf[..n]);
                let is_active = inner.active.lock().unwrap().as_deref() == Some(id.as_str());
                if is_active {
                    let _ = inner.output_tx.send(buf[..n].to_vec());
                }
            }
            Err(e) => {
                tracing::debug!(?e, window = %id, "pane PTY read error; reader exiting");
                break;
            }
        }
    }
    // Process exited: remove the pane.
    inner.panes.lock().unwrap().remove(&id);
    let mut active = inner.active.lock().unwrap();
    if active.as_deref() == Some(id.as_str()) {
        *active = None;
    }
    tracing::debug!(window = %id, "pane exited");
}

/// Build the pane command. `explicit` is true when the caller asked for a specific
/// command (vs. the default interactive shell).
///
/// On Windows the npm-installed agent CLIs (`claude`/`codex`/`gemini`) are
/// `.cmd`/`.ps1` shims; `CreateProcessW` can neither execute those nor apply
/// `PATHEXT`, so a bare `codex` resolves to the extensionless shell-script shim
/// and dies with "not a valid Win32 application" (os error 193). Route explicit
/// commands through `cmd.exe /C` so shim + PATH/PATHEXT resolution happen there.
/// Args are split out so cmd sees `cmd /C codex --flag` without quoting surprises.
fn build_command(cmdline: &str, cwd: &str, explicit: bool) -> CommandBuilder {
    let mut cb = if cfg!(windows) && explicit {
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        let mut c = CommandBuilder::new(comspec);
        c.arg("/C");
        for a in cmdline.split_whitespace() {
            c.arg(a);
        }
        c
    } else {
        let mut parts = cmdline.split_whitespace();
        let program = parts.next().unwrap_or("/bin/sh");
        let mut c = CommandBuilder::new(program);
        for a in parts {
            c.arg(a);
        }
        c
    };
    if !cwd.is_empty() {
        // Strip the Windows extended-length prefix (`\\?\C:\…`) that the directory
        // picker's `canonicalize` produces — CreateProcess rejects it as a cwd.
        let cwd = cwd.strip_prefix(r"\\?\").unwrap_or(cwd);
        cb.cwd(cwd);
    }
    if std::env::var_os("TERM").is_none() {
        cb.env("TERM", "xterm-256color");
    }
    if std::env::var_os("LANG").is_none() {
        cb.env("LANG", "en_US.UTF-8");
    }
    // Advertise 24-bit color so claude renders its logo in the brand orange (without
    // it the logo falls back to gray). No tmux in the pty backend, so the escapes go
    // straight to the truecolor-capable xterm.js client.
    if std::env::var_os("COLORTERM").is_none() {
        cb.env("COLORTERM", "truecolor");
    }
    cb
}

/// The interactive shell to spawn for a plain (commandless) window.
fn default_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

/// Basename of the program in a command line, for window naming / classification.
fn program_basename(cmdline: &str) -> String {
    cmdline
        .split_whitespace()
        .next()
        .map(|p| {
            p.rsplit(['/', '\\'])
                .next()
                .unwrap_or(p)
                .to_string()
        })
        .unwrap_or_default()
}

/// Collect the scrollback lines (oldest → newest, excluding the live screen) from a
/// vt100 parser. vt100 only exposes the buffer through the scrollback "view"
/// (`set_scrollback` shifts which rows are visible), so page through it from the top
/// a screenful at a time, appending each line once in order. Leaves the parser at
/// offset 0 (the live view) — callers that care should still reset it.
fn collect_scrollback(parser: &mut vt100::Parser, cols: u16) -> Vec<String> {
    let rows_u = (parser.screen().size().0 as usize).max(1);
    parser.set_scrollback(usize::MAX); // clamps to the real scrollback length
    let total = parser.screen().scrollback();
    let mut history: Vec<String> = Vec::with_capacity(total);
    let mut off = total;
    loop {
        parser.set_scrollback(off);
        let start = total - off; // buffer line index of the top visible row
        for (r, line) in parser.screen().rows(0, cols).enumerate() {
            let idx = start + r;
            if idx < total && idx == history.len() {
                history.push(line);
            }
        }
        if off == 0 {
            break;
        }
        off = off.saturating_sub(rows_u);
    }
    parser.set_scrollback(0);
    history
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_scrollback_recovers_full_history() {
        // 4-row screen, room for 100 scrollback lines.
        let mut p = vt100::Parser::new(4, 20, 100);
        for i in 1..=30 {
            p.process(format!("line{i}\r\n").as_bytes());
        }
        let hist = collect_scrollback(&mut p, 20);
        let joined = hist.join("\n");
        // The earliest lines scrolled off the visible 4-row screen but must be
        // recoverable from the scrollback (this is the bug the fix addresses).
        assert!(joined.contains("line1"), "missing earliest line; got:\n{joined}");
        assert!(joined.contains("line5"));
        assert!(joined.contains("line20"));
        // Lines are in order, no duplicates of the first line.
        assert_eq!(joined.matches("line1\n").count(), 1);
        // Parser is left at the live view.
        assert_eq!(p.screen().scrollback(), 0);
    }
}
