//! Session backends: the terminal-multiplexing layer behind the WS server.
//!
//! The daemon used to be hard-wired to tmux. This module abstracts the session
//! layer behind [`SessionBackend`] so the same server logic can run on:
//!   - [`tmux::TmuxBackend`] (unix) — attach to a real tmux server, unchanged.
//!   - [`pty::PtyBackend`]  (cross-platform incl. Windows) — the daemon owns one
//!     PTY per window directly and multiplexes them itself.
//!
//! See docs/07-cross-platform-distribution.md §2.

use std::str::FromStr;

use anyhow::Result;
use tokio::sync::broadcast;

pub mod pty;
pub mod tmux;

/// Raw window record produced by a backend. The state layer upgrades this into a
/// `proto::Window` (attaching agent classification + claude usage meta).
#[derive(Debug, Clone)]
pub struct RawWindow {
    pub id: String,
    pub index: u32,
    pub name: String,
    pub active: bool,
    pub cwd: String,
    pub current_command: String,
    /// The command the window was originally started with. For app-created windows
    /// this is the agent CLI (`claude`/`codex`/`gemini`); empty for plain shells.
    /// Used to classify the agent type for UI coloring.
    pub start_command: String,
}

/// The terminal-session capabilities the WS server depends on. Methods are sync:
/// tmux ops are a quick CLI spawn, pty ops are in-memory behind a `std::sync::Mutex`,
/// so they're safe to call briefly from async handlers without `async-trait`.
pub trait SessionBackend: Send + Sync {
    /// Human-readable session label (shown in the WS `Hello`).
    fn label(&self) -> String;

    fn list_windows(&self) -> Result<Vec<RawWindow>>;
    fn new_window(&self, name: Option<&str>, cwd: Option<&str>, cmd: Option<&str>) -> Result<()>;
    fn select_window(&self, id: &str) -> Result<()>;
    fn rename_window(&self, id: &str, name: &str) -> Result<()>;
    fn kill_window(&self, id: &str) -> Result<()>;

    /// Change a window's working directory by sending a `cd` to its shell. Only
    /// takes effect when that window sits at a shell prompt — not while an agent
    /// (claude/codex/gemini) is in the foreground.
    fn change_dir(&self, id: &str, path: &str) -> Result<()>;

    /// Send a tmux-style key spec (e.g. `"Enter"`, `"C-c"`) to a specific window's
    /// active pane, regardless of which window is currently selected (autopilot).
    fn send_keys(&self, id: &str, keys: &str) -> Result<()>;

    /// Rendered (ANSI-stripped) screen text of a window's active pane. Used to
    /// detect Claude confirmation prompts in any window, foreground or not.
    fn capture_pane(&self, id: &str) -> Result<String>;

    /// Full repaint payload for a freshly-connected (or window-switched) client:
    /// clear screen + scrollback + the visible screen padded to the pane height +
    /// a final cursor-position (CUP) escape so the caret lands on the *real* cell.
    /// Without the explicit CUP, `capture-pane` trims trailing blank lines, so for
    /// TUIs whose cursor sits mid-screen (Claude Code's input box, vim, …) the
    /// caret would otherwise rest at the end of the captured text and the user's
    /// first keystrokes would echo in the wrong place until the next redraw.
    fn capture_repaint(&self) -> Result<String>;

    /// Write raw bytes to the active pane (client keystrokes).
    fn write_input(&self, data: &[u8]) -> Result<()>;

    /// Resize the active pane.
    fn resize(&self, cols: u16, rows: u16) -> Result<()>;

    /// Live raw-byte stream of the active pane. Clients `.subscribe()` to it.
    fn output(&self) -> broadcast::Sender<Vec<u8>>;
}

/// Assemble a repaint payload from captured `history` (scrollback above the
/// screen) and `screen` text. The visible screen is padded/truncated to exactly
/// `height` rows so the trailing CUP escape addresses the right cell: after the
/// client clears and writes `history` + `height` screen rows, the bottom `height`
/// rows of its viewport are the visible screen, so a 1-based `\e[row;colH` lands
/// on tmux's real cursor. Both backends share this so the math stays in one place.
pub(crate) fn build_repaint(
    history: &str,
    screen: &str,
    height: usize,
    cursor_y: usize,
    cursor_x: usize,
) -> String {
    let mut payload = String::from("\x1b[H\x1b[2J");

    let history = history.trim_end_matches('\n');
    if !history.is_empty() {
        payload.push_str(&history.replace('\n', "\r\n"));
        payload.push_str("\r\n");
    }

    let screen = screen.trim_end_matches('\n');
    let mut lines: Vec<&str> = if screen.is_empty() { Vec::new() } else { screen.split('\n').collect() };
    lines.truncate(height);
    while lines.len() < height {
        lines.push("");
    }
    payload.push_str(&lines.join("\r\n"));

    // 1-based cursor position, relative to the visible screen (now the bottom rows).
    payload.push_str(&format!("\x1b[{};{}H", cursor_y + 1, cursor_x + 1));
    payload
}

/// Which backend to run. Defaults to tmux on unix, pty on Windows (tmux absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Tmux,
    Pty,
}

impl BackendKind {
    /// Platform default: tmux where it exists, pty otherwise.
    pub fn platform_default() -> Self {
        if cfg!(windows) {
            BackendKind::Pty
        } else {
            BackendKind::Tmux
        }
    }
}

impl FromStr for BackendKind {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "tmux" => Ok(BackendKind::Tmux),
            "pty" => Ok(BackendKind::Pty),
            other => Err(format!("unknown backend '{other}' (expected 'tmux' or 'pty')")),
        }
    }
}

/// Translate a tmux key spec into the bytes a PTY expects. Covers the specs the
/// app actually emits (autopilot sends `"Enter"`) plus the common control set;
/// anything unrecognised is sent through as its literal UTF-8 bytes.
pub fn keyspec_to_bytes(spec: &str) -> Vec<u8> {
    match spec {
        "Enter" => vec![b'\r'],
        "Tab" => vec![b'\t'],
        "Escape" | "Esc" => vec![0x1b],
        "Space" => vec![b' '],
        "BSpace" | "Backspace" => vec![0x7f],
        "Up" => b"\x1b[A".to_vec(),
        "Down" => b"\x1b[B".to_vec(),
        "Right" => b"\x1b[C".to_vec(),
        "Left" => b"\x1b[D".to_vec(),
        // Ctrl-<letter>: map to the corresponding control byte (C-a = 0x01 …).
        s if s.len() == 3 && (s.starts_with("C-") || s.starts_with("c-")) => {
            let c = s.as_bytes()[2].to_ascii_lowercase();
            if c.is_ascii_lowercase() {
                vec![c - b'a' + 1]
            } else {
                s.as_bytes().to_vec()
            }
        }
        other => other.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyspecs() {
        assert_eq!(keyspec_to_bytes("Enter"), b"\r");
        assert_eq!(keyspec_to_bytes("C-c"), vec![0x03]);
        assert_eq!(keyspec_to_bytes("C-a"), vec![0x01]);
        assert_eq!(keyspec_to_bytes("y"), b"y");
        assert_eq!(keyspec_to_bytes("Up"), b"\x1b[A");
    }

    #[test]
    fn backend_kind_parse() {
        assert_eq!("tmux".parse::<BackendKind>().unwrap(), BackendKind::Tmux);
        assert_eq!("PTY".parse::<BackendKind>().unwrap(), BackendKind::Pty);
        assert!("zellij".parse::<BackendKind>().is_err());
    }

    #[test]
    fn repaint_pads_screen_and_appends_cursor() {
        // 2 content lines, a 4-row pane, cursor mid-screen at row 1 / col 3 (0-based).
        let out = build_repaint("", "ab\ncd", 4, 1, 3);
        // Clear, the two lines, then two pad lines (CRLF-joined, no trailing), then CUP.
        assert_eq!(out, "\x1b[H\x1b[2Jab\r\ncd\r\n\r\n\x1b[2;4H");
    }

    #[test]
    fn repaint_prepends_history_then_screen() {
        let out = build_repaint("old1\nold2", "now", 2, 0, 0);
        // History (CRLF) + separator, then the screen padded to 2 rows, then CUP home.
        assert_eq!(out, "\x1b[H\x1b[2Jold1\r\nold2\r\nnow\r\n\x1b[1;1H");
    }

    #[test]
    fn repaint_truncates_overlong_screen_to_height() {
        // More captured lines than the pane height → keep only the first `height`.
        let out = build_repaint("", "l0\nl1\nl2", 2, 0, 0);
        assert_eq!(out, "\x1b[H\x1b[2Jl0\r\nl1\x1b[1;1H");
    }
}
