//! cc-console WebSocket protocol — M1 subset.
//!
//! Adds windows / claude metadata / usage to the M0 single-stream baseline.
//! Out of scope for M1 (placeholders only): detach, rename ack, kill ack,
//! structured `message` events (only UsageDelta).

use serde::{Deserialize, Serialize};

/// Token accounting, normalized across all assistant messages of a JSONL session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

/// Claude-Code-specific metadata attached to a window when its current command is `claude`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ClaudeMeta {
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub total_usage: Usage,
    pub estimated_cost: f64,
    pub last_message_preview: Option<String>,
}

/// One tmux window snapshot for UI rendering.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Window {
    pub id: String,
    pub index: u32,
    pub name: String,
    pub active: bool,
    pub cwd: String,
    pub current_command: String,
    /// Agent kind for UI coloring: "claude" | "codex" | "gemini" | "other".
    #[serde(default)]
    pub agent: String,
    pub claude: Option<ClaudeMeta>,
    /// True when this window is showing an agent (claude/codex/gemini) confirmation
    /// prompt waiting on the user.
    #[serde(default)]
    pub awaiting_input: bool,
    /// The question line of that prompt, for the notification body.
    #[serde(default)]
    pub awaiting_prompt: Option<String>,
}

impl Window {
    /// True when this window is running Claude Code. The CLI reports its process
    /// name as `claude` on some setups and `claude.exe` on others, so match the prefix.
    pub fn is_claude(&self) -> bool {
        self.current_command.starts_with("claude")
    }

    /// True when this window runs one of the agent CLIs we can detect confirmation
    /// prompts for. codex/gemini report `node` as their current command, so we key
    /// off the classified `agent` field rather than the process name.
    pub fn is_agent(&self) -> bool {
        matches!(self.agent.as_str(), "claude" | "codex" | "gemini")
    }
}

/// Classify the agent kind from a window's start/current command, for UI coloring.
/// `pane_start_command` is the agent CLI for app-created windows (`claude`/`codex`/`gemini`);
/// `pane_current_command` is `claude.exe` for Claude and `node` for codex/gemini.
pub fn classify_agent(start_command: &str, current_command: &str) -> String {
    let hay = format!("{} {}", start_command, current_command).to_ascii_lowercase();
    if hay.contains("claude") {
        "claude"
    } else if hay.contains("codex") {
        "codex"
    } else if hay.contains("gemini") {
        "gemini"
    } else {
        "other"
    }
    .to_string()
}

/// Client → Server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Input { text: String },
    Resize { cols: u16, rows: u16 },
    /// Toggle "autopilot": when on, the daemon auto-confirms Claude `❯ 1. Yes`
    /// prompts (sends Enter) the moment a window starts waiting on one.
    SetAutopilot { enabled: bool },
    WindowList,
    WindowSelect { window_id: String },
    WindowCreate {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        cmd: Option<String>,
    },
    WindowRename { window_id: String, name: String },
    /// Change a window's working directory. Sent to the window's shell as a `cd`,
    /// so it only takes effect when that window is at a shell prompt.
    WindowChdir { window_id: String, path: String },
    WindowKill { window_id: String },
}

/// Server → Client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        daemon_version: String,
        tmux_session: String,
        windows: Vec<Window>,
        active_window_id: Option<String>,
        /// Current autopilot state, so a fresh client renders the toggle correctly.
        #[serde(default)]
        autopilot: bool,
    },
    Output {
        bytes: String,
    },
    WindowList {
        windows: Vec<Window>,
    },
    WindowCreated {
        window: Window,
    },
    WindowRemoved {
        window_id: String,
    },
    WindowRenamed {
        window_id: String,
        name: String,
    },
    WindowActiveChanged {
        window_id: String,
        #[serde(default)]
        from: Option<String>,
    },
    UsageDelta {
        window_id: String,
        claude: ClaudeMeta,
    },
    /// A window started (or stopped) waiting on a Claude confirmation prompt.
    /// On the `awaiting: true` edge the client raises an obvious alert.
    AwaitingInput {
        window_id: String,
        awaiting: bool,
        #[serde(default)]
        prompt: Option<String>,
    },
    /// Autopilot was toggled (by any client); all clients sync their toggle UI.
    AutopilotChanged {
        enabled: bool,
    },
    Error {
        message: String,
    },
}

impl ServerMessage {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("ServerMessage is always serializable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_claude_matches_plain_and_exe() {
        let mut w = Window::default();
        w.current_command = "claude".into();
        assert!(w.is_claude());
        w.current_command = "claude.exe".into();
        assert!(w.is_claude());
        w.current_command = "node".into();
        assert!(!w.is_claude());
        w.current_command = "zsh".into();
        assert!(!w.is_claude());
    }
}
