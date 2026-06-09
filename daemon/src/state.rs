//! Shared daemon state. The polling tasks update it; WS handlers read it.

use std::collections::HashMap;
use std::sync::Arc;

use cc_console_proto::{ClaudeMeta, ServerMessage, Window};
use tokio::sync::{Mutex, RwLock, broadcast};

use crate::session::SessionBackend;
use crate::usage_store::UsageStore;

/// Cache of usage per claude window, keyed by JSONL file path (one path = one Claude Code session).
/// Bound to the window through the window's cwd in the poller.
#[derive(Default, Debug, Clone)]
pub struct JsonlSession {
    /// Where we are in the file (bytes read), so we only parse new lines next tick.
    pub offset: u64,
    /// Per-requestId dedup so streamed assistant turns don't double-count.
    pub seen_request_ids: std::collections::HashSet<String>,
    pub meta: ClaudeMeta,
}

/// Lookup key for the JSONL cache: the absolute jsonl file path (string).
pub type JsonlKey = String;

/// One-click managed-link (device authorization) progress, surfaced to the UI via
/// `/api/link/connect/status`. Once `Approved`, the tunnel state machine
/// (`/api/tunnel/status`) reports the live hostname.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum ConnectState {
    /// No link in progress.
    Idle,
    /// Waiting for the user to approve in the browser.
    Pending {
        user_code: String,
        verification_uri: String,
        expires_at: u64,
    },
    /// Approved + device token received; the tunnel is coming up.
    Approved,
    /// The link attempt failed; `message` explains why.
    Error { message: String },
}

pub struct AppState {
    /// Terminal-session backend (tmux on unix, pty cross-platform).
    pub session: Arc<dyn SessionBackend>,
    /// Latest windows snapshot, keyed by window id (`@N`).
    pub windows: RwLock<HashMap<String, Window>>,
    /// JSONL parse state, keyed by jsonl file path.
    pub jsonl: Mutex<HashMap<JsonlKey, JsonlSession>>,
    /// Map cwd → jsonl file path currently associated with it (latest mtime), so the poller can
    /// quickly find which session belongs to a claude window.
    pub cwd_to_jsonl: RwLock<HashMap<String, JsonlKey>>,
    /// Raw byte stream of the active window (the backend's `output()`). Drives the
    /// terminal view; WS clients subscribe to it.
    pub output_tx: broadcast::Sender<Vec<u8>>,
    /// Structured events (windows changes, usage deltas).
    pub event_tx: broadcast::Sender<ServerMessage>,
    /// Persistent token-usage store (full history across all projects), backing `/api/stats`.
    pub usage_store: Arc<UsageStore>,
    /// Shared secret required to access the daemon. `None` = auth disabled (loopback dev).
    /// Interior-mutable so the tunnel control path can enable auth on demand: the moment
    /// a tunnel exposes the daemon publicly, a token must be required.
    pub auth_token: RwLock<Option<String>>,
    /// Autopilot: when true, the window poller auto-confirms Claude `❯ 1. Yes`
    /// prompts (sends Enter to that window) on the awaiting rising edge.
    pub autopilot: std::sync::atomic::AtomicBool,
    /// On-demand tunnel supervisor (cloudflared), driven by the `/api/tunnel/*` routes
    /// and the settings "Remote access" button.
    pub tunnel: Arc<crate::tunnel::TunnelManager>,
    /// Tunnel settings from config.toml (hostname/name for named mode; mode default).
    pub tunnel_cfg: crate::config::TunnelConfig,
    /// The loopback port cloudflared should point at (= the daemon's own port).
    pub local_port: u16,
    /// In-flight one-click device-authorization link (managed mode).
    pub connect: Mutex<ConnectState>,
    /// The control-plane session cookie (`cc_session=…`) held server-side after an
    /// in-app email/password login, so account API calls don't need a browser.
    pub account_cookie: Mutex<Option<String>>,
}

impl AppState {
    pub async fn snapshot_windows(&self) -> Vec<Window> {
        let map = self.windows.read().await;
        let mut v: Vec<Window> = map.values().cloned().collect();
        v.sort_by_key(|w| w.index);
        v
    }

    pub async fn active_window_id(&self) -> Option<String> {
        self.windows
            .read()
            .await
            .values()
            .find(|w| w.active)
            .map(|w| w.id.clone())
    }

    /// Return the current access token, generating+persisting+enabling one if auth
    /// was previously off. Called before a tunnel goes up so the public entry is
    /// never unauthenticated.
    pub async fn ensure_token(&self) -> String {
        {
            let g = self.auth_token.read().await;
            if let Some(t) = g.as_ref() {
                return t.clone();
            }
        }
        let mut g = self.auth_token.write().await;
        if let Some(t) = g.as_ref() {
            return t.clone();
        }
        let t = crate::auth::load_or_generate();
        tracing::info!("auth enabled on demand (tunnel) — token now required");
        *g = Some(t.clone());
        t
    }
}
