//! axum WebSocket server, PTY broker, and window/JSONL poller orchestrator.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::RustEmbed;
use base64::Engine;
use cc_console_proto::{ClientMessage, ServerMessage, Window};
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio::time::sleep;
use tower_http::services::ServeDir;

use crate::auth;
use crate::jsonl;
use crate::session::{self, BackendKind};
use crate::state::AppState;
use crate::tunnel;
use crate::usage_store;

const EVENT_BUFFER: usize = 256;
const WINDOW_POLL_MS: u64 = 500;

pub async fn run(
    host: String,
    port: u16,
    socket: String,
    session_name: String,
    initial_cmd: String,
    backend: BackendKind,
    web_dir: String,
    token_arg: Option<String>,
    tunnel: crate::config::TunnelConfig,
    session_env: std::collections::BTreeMap<String, String>,
) -> Result<()> {
    // Inject configured session env (e.g. a proxy) into the daemon process *before*
    // anything is spawned: the tmux server (started via `Command::new("tmux")`), the
    // pty backend, and cloudflared all inherit the daemon's environment. The desktop
    // app launches from the GUI with no proxy vars, so this is where the user's
    // `[session.env]` gets them to the AI CLIs. (Edition 2021: set_var is safe.)
    for (k, v) in &session_env {
        std::env::set_var(k, v);
    }

    // Build the session backend. It owns the PTY layer and the output broadcast;
    // tmux on unix (detach/reattach), pty cross-platform (incl. Windows).
    let session: Arc<dyn session::SessionBackend> = match backend {
        BackendKind::Tmux => Arc::new(session::tmux::TmuxBackend::ensure(
            &socket,
            &session_name,
            &initial_cmd,
            &session_env,
        )?),
        BackendKind::Pty => Arc::new(session::pty::PtyBackend::new(&initial_cmd)?),
    };
    tracing::info!(label = %session.label(), ?backend, "session backend ready");
    let output_tx = session.output();

    // Persistent usage store (full history across all projects).
    let db_path = usage_store::default_db_path()
        .context("$HOME not set — cannot locate usage db")?;
    let usage_store = Arc::new(usage_store::UsageStore::open(&db_path)?);
    tracing::info!(db = %db_path.display(), "usage store ready");

    let (event_tx, _) = broadcast::channel::<ServerMessage>(EVENT_BUFFER);

    // Resolve the access token: explicit flag/env wins; otherwise auto-generate+persist when
    // the daemon is reachable from outside this machine. "Outside" means either a non-loopback
    // bind (public/LAN) OR an active tunnel — a tunnel reaches 127.0.0.1 from the public
    // internet, so loopback-only must NOT mean "no auth" once a tunnel is up.
    let loopback_bind = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1");
    let tunnel_enabled = tunnel.enabled();
    let local_only = loopback_bind && !tunnel_enabled;
    let auth_token = auth::resolve_token(token_arg, local_only);

    let tunnel_mgr = tunnel::TunnelManager::new();
    let state = Arc::new(AppState {
        session,
        windows: RwLock::new(HashMap::new()),
        jsonl: Mutex::new(HashMap::new()),
        cwd_to_jsonl: RwLock::new(HashMap::new()),
        output_tx,
        event_tx,
        usage_store: usage_store.clone(),
        auth_token: RwLock::new(auth_token.clone()),
        autopilot: std::sync::atomic::AtomicBool::new(false),
        tunnel: tunnel_mgr.clone(),
        tunnel_cfg: tunnel.clone(),
        local_port: port,
        connect: Mutex::new(crate::state::ConnectState::Idle),
        account_cookie: Mutex::new(None),
    });

    // Usage scanner: blocking OS thread, sweeps every project's jsonl into SQLite.
    {
        let store = usage_store.clone();
        std::thread::Builder::new()
            .name("usage-scanner".into())
            .spawn(move || usage_store::run_scanner(store))
            .context("failed to spawn usage-scanner thread")?;
    }

    // Window poller: diffs tmux state every 500ms, broadcasts structural events.
    {
        let s = state.clone();
        tokio::spawn(window_poller(s));
    }
    // JSONL watcher: 1Hz, drives UsageDelta events for claude windows.
    {
        let s = state.clone();
        tokio::spawn(jsonl::run(s));
    }
    // Codex live watcher: same, for codex windows (reads ~/.codex/sessions).
    {
        let s = state.clone();
        tokio::spawn(crate::codex_live::run(s));
    }
    // Gemini live watcher: same, for gemini windows (reads ~/.gemini/tmp/<slug>/chats).
    {
        let s = state.clone();
        tokio::spawn(crate::gemini_live::run(s));
    }

    // Static web assets: prefer an on-disk `--web-dir` if it exists (dev hot-edit),
    // otherwise fall back to the copy embedded into the binary at build time
    // (so a distributed single-file binary needs no separate web/ directory).
    let serve_from_disk = std::path::Path::new(&web_dir).join("index.html").exists();
    let mut app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/stats", get(stats_handler))
        .route("/api/conversations", get(conversations_handler))
        .route("/api/sessions", get(sessions_handler))
        .route("/api/sessions/messages", get(session_messages_handler))
        .route("/api/sessions/delete", axum::routing::post(session_delete_handler))
        .route("/api/sessions/resume", axum::routing::post(session_resume_handler))
        .route("/api/dirs", get(dirs_handler))
        .route("/api/tunnel/status", get(tunnel_status_handler))
        .route("/api/tunnel/start", axum::routing::post(tunnel_start_handler))
        .route("/api/tunnel/stop", axum::routing::post(tunnel_stop_handler))
        .route("/api/link/connect", axum::routing::post(link_connect_handler))
        .route("/api/link/connect/status", get(link_connect_status_handler))
        .route("/api/account/register", axum::routing::post(account_register_handler))
        .route("/api/account/login", axum::routing::post(account_login_handler))
        .route("/api/account/me", get(account_me_handler))
        .route("/api/account/logout", axum::routing::post(account_logout_handler))
        .route("/api/account/resend-verify", axum::routing::post(account_resend_handler))
        .route("/api/account/connect-remote", axum::routing::post(account_connect_remote_handler));
    app = if serve_from_disk {
        tracing::info!(web_dir, "serving web assets from disk");
        app.fallback_service(ServeDir::new(&web_dir))
    } else {
        tracing::info!("serving embedded web assets");
        app.fallback(static_handler)
    };
    let app = app
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth::guard))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| format!("failed to bind {host}:{port}"))?;
    match &auth_token {
        Some(tok) => tracing::info!(
            "auth ENABLED — first visit must include ?token=… ; e.g. http://{host}:{port}/?token={tok}"
        ),
        None if !local_only => tracing::warn!(
            %host,
            "listening on a non-loopback address with NO authentication — anyone who can reach \
             this port has full control of your terminal and claude."
        ),
        None => {}
    }
    tracing::info!(
        url = format!("http://{host}:{port}"),
        web_dir,
        "cc-console daemon listening"
    );

    // Public entry (docs/05 §L2): if config.toml turns the tunnel on, auto-start it
    // through the manager so boot and the settings button share one state machine.
    // The tunnel reaches 127.0.0.1, so the daemon stays loopback and auth is forced on.
    if tunnel_enabled {
        let mgr = tunnel_mgr.clone();
        let cfg = tunnel.clone();
        let tok = auth_token.clone();
        tokio::spawn(async move {
            mgr.start(cfg.mode, cfg, port, tok).await;
        });
    }

    axum::serve(listener, app).await.context("axum serve failed")?;
    Ok(())
}

/// Web UI assets baked into the binary. In debug builds rust-embed reads these
/// from `daemon/../web` on disk (hot reload); release builds embed the bytes so
/// the shipped binary is self-contained.
#[derive(RustEmbed)]
#[folder = "../web"]
struct WebAssets;

/// Fallback handler that serves a file from the embedded `WebAssets`. `/` maps to
/// `index.html`; unknown paths 404.
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match WebAssets::get(path) {
        Some(content) => {
            let mime = content.metadata.mimetype().to_string();
            ([(header::CONTENT_TYPE, mime)], content.data).into_response()
        }
        None => (StatusCode::NOT_FOUND, "404 not found\n").into_response(),
    }
}

/// Heuristic: does this captured pane show an agent confirmation prompt waiting
/// for the user to choose an option? Returns the question line as a preview.
///
/// Each agent CLI renders its own prompt shape, so we dispatch on the classified
/// `agent` kind and anchor on agent-specific phrasing — that keeps ordinary output
/// mentioning "yes"/"no" from tripping the alarm.
fn detect_awaiting(pane: &str, agent: &str) -> Option<String> {
    match agent {
        "claude" => detect_claude_awaiting(pane),
        "codex" => detect_codex_awaiting(pane),
        "gemini" => detect_gemini_awaiting(pane),
        _ => None,
    }
}

/// Claude renders a bordered prompt with a `❯ 1. Yes` selector and a
/// `No, and tell Claude what to do differently` option.
fn detect_claude_awaiting(pane: &str) -> Option<String> {
    let has_selector = pane.contains('❯');
    let has_yes = pane.contains("1. Yes");
    let claude_ish = pane.contains("Do you want") || pane.contains("tell Claude");
    if !(has_selector && has_yes && claude_ish) {
        return None;
    }
    // Prefer the actual question ("Do you want to …") as the preview; fall back to a generic line.
    let preview = pane
        .lines()
        .rev()
        .find(|l| l.contains("Do you want"))
        .map(clean_box_line)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Claude is waiting for your confirmation".to_string());
    Some(preview)
}

/// Codex renders an approval prompt with a `›` selector and numbered `1. Yes …`
/// options; the decline option always reads "tell Codex what to do differently".
/// Verified against codex-cli 0.137.0 (command-approval form). The cursor sits on
/// option 1 and "Press enter to confirm", so autopilot's Enter selects Yes.
fn detect_codex_awaiting(pane: &str) -> Option<String> {
    let has_selector = pane.contains('›');
    let has_yes = pane.contains("1. Yes");
    let codex_ish = pane.contains("tell Codex")
        || pane.contains("Would you like to")
        || pane.contains("don't ask again");
    if !(has_selector && has_yes && codex_ish) {
        return None;
    }
    // Prefer the `$ <command>` line (most informative); fall back to the question, then generic.
    let preview = pane
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("$ ") || l.contains("Would you like to"))
        .map(clean_box_line)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Codex is waiting for your approval".to_string());
    Some(preview)
}

/// Gemini renders confirmations in a bordered box with a `●` selector and the
/// fixed options "Allow once / Allow for this session / No, suggest changes (esc)".
/// The question line varies by tool ("Allow execution of [Shell]?", "Apply this
/// change?", "Do you want to proceed?"). Verified against gemini-cli 0.45.2
/// (shell-exec and skill-activation forms). Cursor sits on option 1 → Enter = Yes.
fn detect_gemini_awaiting(pane: &str) -> Option<String> {
    let has_selector = pane.contains('●');
    let gemini_ish = pane.contains("Allow once")
        || pane.contains("Allow for this session")
        || pane.contains("No, suggest changes");
    if !(has_selector && gemini_ish) {
        return None;
    }
    // The question is the last box line ending in '?' (e.g. "Allow execution of [Shell]?").
    let preview = pane
        .lines()
        .rev()
        .map(clean_box_line)
        .find(|l| l.ends_with('?'))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Gemini is waiting for your confirmation".to_string());
    Some(preview)
}

/// Strip tmux box-drawing borders/padding from a captured line.
fn clean_box_line(l: &str) -> String {
    l.trim()
        .trim_matches(|c| c == '│' || c == '|' || c == '╮' || c == '╭' || c == ' ')
        .trim()
        .chars()
        .take(120)
        .collect()
}

/// Polls tmux every `WINDOW_POLL_MS` and emits structural events on diffs.
async fn window_poller(state: Arc<AppState>) {
    let mut prev_active: Option<String> = None;
    let mut prev_ids: HashSet<String> = HashSet::new();
    let mut prev_names: HashMap<String, String> = HashMap::new();
    let mut prev_awaiting: HashMap<String, bool> = HashMap::new();

    loop {
        sleep(Duration::from_millis(WINDOW_POLL_MS)).await;

        let raw = match state.session.list_windows() {
            Ok(ws) => ws,
            Err(e) => {
                tracing::debug!(?e, "list_windows failed (tmux may be transitioning)");
                continue;
            }
        };

        // Build current snapshot, inheriting any cached claude meta from previous state.
        let mut current: Vec<Window> = Vec::with_capacity(raw.len());
        {
            let prev_snap = state.windows.read().await;
            for w in raw {
                let claude_meta = prev_snap.get(&w.id).and_then(|p| p.claude.clone());
                let agent = cc_console_proto::classify_agent(&w.start_command, &w.current_command);
                current.push(Window {
                    id: w.id,
                    index: w.index,
                    name: w.name,
                    active: w.active,
                    cwd: w.cwd,
                    current_command: w.current_command,
                    agent,
                    claude: claude_meta,
                    awaiting_input: false,
                    awaiting_prompt: None,
                });
            }
        }

        // Detect agent (claude/codex/gemini) confirmation prompts per window (works
        // for background windows too, since capture-pane reads each pane independently
        // of the PTY).
        for w in &mut current {
            if !w.is_agent() {
                continue;
            }
            let agent = w.agent.clone();
            match state.session.capture_pane(&w.id) {
                Ok(pane) => {
                    if let Some(prompt) = detect_awaiting(&pane, &agent) {
                        w.awaiting_input = true;
                        w.awaiting_prompt = Some(prompt);
                    }
                }
                Err(e) => tracing::debug!(?e, window = %w.id, "capture-pane failed"),
            }
        }

        let current_ids: HashSet<String> = current.iter().map(|w| w.id.clone()).collect();
        let current_active = current.iter().find(|w| w.active).map(|w| w.id.clone());
        let current_names: HashMap<String, String> =
            current.iter().map(|w| (w.id.clone(), w.name.clone())).collect();

        // Diff: created / removed.
        for w in &current {
            if !prev_ids.contains(&w.id) {
                tracing::debug!(window = %w.id, name = %w.name, "window created");
                let _ = state.event_tx.send(ServerMessage::WindowCreated { window: w.clone() });
            }
        }
        for id in prev_ids.difference(&current_ids) {
            tracing::debug!(window = %id, "window removed");
            let _ = state.event_tx.send(ServerMessage::WindowRemoved { window_id: id.clone() });
        }

        // Diff: renamed.
        for (id, name) in &current_names {
            if let Some(prev_name) = prev_names.get(id) {
                if prev_name != name {
                    tracing::debug!(window = %id, %name, "window renamed");
                    let _ = state.event_tx.send(ServerMessage::WindowRenamed {
                        window_id: id.clone(),
                        name: name.clone(),
                    });
                }
            }
        }

        // Diff: awaiting-input edges. Fire on every transition so the client can both
        // raise the alert (false→true) and clear it (true→false).
        for w in &current {
            let was = prev_awaiting.get(&w.id).copied().unwrap_or(false);
            if w.awaiting_input != was {
                tracing::debug!(window = %w.id, awaiting = w.awaiting_input, "awaiting-input changed");
                let _ = state.event_tx.send(ServerMessage::AwaitingInput {
                    window_id: w.id.clone(),
                    awaiting: w.awaiting_input,
                    prompt: w.awaiting_prompt.clone(),
                });

                // Autopilot: on the rising edge, confirm the prompt by selecting its
                // pre-highlighted default "Yes" (Enter) — claude/codex/gemini all land
                // the cursor on it. Only on the edge, so we never spam: if Enter doesn't
                // dismiss it, prev_awaiting is now true and we won't retry.
                if w.awaiting_input
                    && !was
                    && state.autopilot.load(Ordering::Relaxed)
                {
                    tracing::info!(window = %w.id, "autopilot: confirming prompt (Enter)");
                    if let Err(e) = state.session.send_keys(&w.id, "Enter") {
                        tracing::warn!(?e, window = %w.id, "autopilot send-keys failed");
                    }
                }
            }
        }

        // Diff: active changed.
        if current_active != prev_active {
            if let Some(active_id) = &current_active {
                tracing::debug!(?prev_active, %active_id, "active window changed");
                let _ = state.event_tx.send(ServerMessage::WindowActiveChanged {
                    window_id: active_id.clone(),
                    from: prev_active.clone(),
                });
            }
        }

        // Commit snapshot.
        {
            let mut snap = state.windows.write().await;
            snap.clear();
            for w in &current {
                snap.insert(w.id.clone(), w.clone());
            }
        }

        prev_ids = current_ids;
        prev_names = current_names;
        prev_active = current_active;
        prev_awaiting = current.iter().map(|w| (w.id.clone(), w.awaiting_input)).collect();
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

#[derive(serde::Deserialize)]
struct StatsParams {
    /// Range start, epoch millis. Defaults to 30 days before `to`.
    from: Option<i64>,
    /// Range end, epoch millis (exclusive). Defaults to now.
    to: Option<i64>,
}

/// GET /api/stats?from=<ms>&to=<ms> — aggregated token usage over a time range.
async fn stats_handler(
    State(state): State<Arc<AppState>>,
    Query(p): Query<StatsParams>,
) -> impl IntoResponse {
    let now = chrono::Utc::now().timestamp_millis();
    let to = p.to.unwrap_or(now);
    let from = p.from.unwrap_or(to - 30 * 24 * 60 * 60 * 1000);
    let store = state.usage_store.clone();
    match tokio::task::spawn_blocking(move || store.query(from, to)).await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(?e, "stats query failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "stats query failed").into_response()
        }
        Err(e) => {
            tracing::warn!(?e, "stats task join failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "stats task failed").into_response()
        }
    }
}

/// GET /api/conversations?from=<ms>&to=<ms> — per-agent conversation history
/// (distinct sessions, projects, first/last activity, tokens, cost). Defaults to
/// all-time so the History tab shows your whole record. Backs the settings History tab.
async fn conversations_handler(
    State(state): State<Arc<AppState>>,
    Query(p): Query<StatsParams>,
) -> impl IntoResponse {
    let now = chrono::Utc::now().timestamp_millis();
    let to = p.to.unwrap_or(now);
    let from = p.from.unwrap_or(0); // all-time by default
    let store = state.usage_store.clone();
    match tokio::task::spawn_blocking(move || store.conversations(from, to)).await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(?e, "conversations query failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "conversations query failed").into_response()
        }
        Err(e) => {
            tracing::warn!(?e, "conversations task join failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "conversations task failed").into_response()
        }
    }
}

/// GET /api/sessions — every local Claude/Codex/Gemini session, newest first.
async fn sessions_handler() -> impl IntoResponse {
    match tokio::task::spawn_blocking(crate::sessions::scan_sessions).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => {
            tracing::warn!(?e, "scan_sessions task failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "scan failed").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct SessionMsgParams {
    provider: String,
    path: String,
}

/// GET /api/sessions/messages?provider=&path= — one session's full transcript.
async fn session_messages_handler(Query(p): Query<SessionMsgParams>) -> impl IntoResponse {
    match tokio::task::spawn_blocking(move || crate::sessions::load_messages(&p.provider, &p.path)).await {
        Ok(Ok(msgs)) => Json(msgs).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e).into_response(),
        Err(e) => {
            tracing::warn!(?e, "load_messages task failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "load failed").into_response()
        }
    }
}

/// POST /api/sessions/delete — remove a session file (validated against provider roots).
async fn session_delete_handler(
    Json(req): Json<crate::sessions::DeleteSessionRequest>,
) -> impl IntoResponse {
    match tokio::task::spawn_blocking(move || {
        crate::sessions::delete_session(&req.provider_id, &req.session_id, &req.source_path)
    })
    .await
    {
        Ok(Ok(_)) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e).into_response(),
        Err(e) => {
            tracing::warn!(?e, "delete_session task failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "delete failed").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct SessionResumeReq {
    /// The provider's resume command, e.g. `claude --resume <id>`.
    cmd: String,
    /// The session's working directory (so the agent resumes in the right place).
    cwd: Option<String>,
    /// Optional window name (the session title).
    name: Option<String>,
}

/// POST /api/sessions/resume — open the resume command as a new live console window.
async fn session_resume_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SessionResumeReq>,
) -> impl IntoResponse {
    let cwd = req.cwd.as_deref().map(expand_tilde);
    match state
        .session
        .new_window(req.name.as_deref(), cwd.as_deref(), Some(req.cmd.as_str()))
    {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => {
            tracing::warn!(?e, "resume new_window failed");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("resume: {e}")).into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct DirsParams {
    /// Absolute path to list. `~` is expanded; defaults to the user's home directory.
    path: Option<String>,
}

#[derive(serde::Serialize)]
struct DirEntry {
    name: String,
    path: String,
}

#[derive(serde::Serialize)]
struct DirListing {
    /// The canonical path that was listed.
    path: String,
    /// Parent directory, or null at the filesystem root.
    parent: Option<String>,
    /// Immediate subdirectories (hidden ones omitted), sorted case-insensitively.
    entries: Vec<DirEntry>,
}

fn home_dir() -> String {
    crate::paths::home_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/".to_string())
}

/// Expand a leading `~` / `~/…` (and the empty string) to the user's home dir;
/// other paths pass through unchanged. tmux's `new-window -c` does NOT expand
/// `~` itself — it would try to `chdir` into a literal "~" and fail — so the
/// daemon must expand before handing a cwd to the session backend.
fn expand_tilde(base: &str) -> String {
    if base == "~" || base.is_empty() {
        home_dir()
    } else if let Some(rest) = base.strip_prefix("~/") {
        format!("{}/{}", home_dir(), rest)
    } else {
        base.to_string()
    }
}

/// Strip Windows' verbatim / extended-length prefix so canonicalized paths read
/// cleanly and stay usable as a normal cwd:
///   `\\?\C:\Users\…`        → `C:\Users\…`
///   `\\?\UNC\server\share`  → `\\server\share`
/// No-op on paths without the prefix (i.e. always, on unix).
fn strip_verbatim(p: String) -> String {
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = p.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        p
    }
}

fn list_dirs(base: &str) -> Result<DirListing> {
    // Windows: a virtual "This PC" that lists the available drive roots (C:\, D:\,
    // E:\ …). Reached by going Up from a drive root, so the working-dir picker can
    // hop between drives instead of being stuck on C:.
    #[cfg(windows)]
    if base == "::drives" {
        let mut entries = Vec::new();
        for c in b'A'..=b'Z' {
            let root = format!("{}:\\", c as char);
            if std::path::Path::new(&root).exists() {
                entries.push(DirEntry {
                    name: format!("{}:", c as char),
                    path: root,
                });
            }
        }
        return Ok(DirListing {
            path: "::drives".to_string(),
            parent: None,
            entries,
        });
    }

    let expanded = expand_tilde(base);
    let path = std::path::PathBuf::from(&expanded);
    let canon = std::fs::canonicalize(&path).unwrap_or(path);
    let mut entries = Vec::new();
    for e in std::fs::read_dir(&canon).with_context(|| format!("read_dir {}", canon.display()))? {
        let e = e?;
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue; // skip hidden dirs to keep the picker readable
            }
            entries.push(DirEntry {
                name,
                path: strip_verbatim(e.path().to_string_lossy().to_string()),
            });
        }
    }
    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    // `mut` is only used on Windows, where a drive root gets a virtual parent.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut parent = canon
        .parent()
        .map(|p| strip_verbatim(p.to_string_lossy().to_string()));
    // Windows: a drive root (C:\) has no filesystem parent — point Up at the
    // virtual drive list instead so the user can switch to D:\, E:\, …
    #[cfg(windows)]
    if parent.is_none() {
        parent = Some("::drives".to_string());
    }
    Ok(DirListing {
        path: strip_verbatim(canon.to_string_lossy().to_string()),
        parent,
        entries,
    })
}

/// GET /api/dirs?path=<abs> — list immediate subdirectories, for the new-window folder picker.
async fn dirs_handler(Query(p): Query<DirsParams>) -> impl IntoResponse {
    let base = p.path.unwrap_or_default();
    match tokio::task::spawn_blocking(move || list_dirs(&base)).await {
        Ok(Ok(listing)) => Json(listing).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, format!("dirs: {e}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("dirs task: {e}")).into_response(),
    }
}

/// GET /api/tunnel/status — current tunnel state for the settings UI to poll.
async fn tunnel_status_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.tunnel.status().await)
}

#[derive(serde::Deserialize, Default)]
struct TunnelStartReq {
    /// "quick" (default), "named", or "managed".
    #[serde(default)]
    mode: Option<String>,
    /// `managed` mode only: the device token (`ccd_…`) to provision with. Optional
    /// here if one is already saved on disk / in config.toml.
    #[serde(default)]
    device_token: Option<String>,
}

/// POST /api/tunnel/start — open a tunnel (Phase 0: defaults to a one-click quick
/// tunnel). Enables token auth first (the tunnel exposes us publicly) and sets the
/// cookie on this response so the local caller that clicked the button stays signed in.
async fn tunnel_start_handler(
    State(state): State<Arc<AppState>>,
    body: Option<Json<TunnelStartReq>>,
) -> Response {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let mode = match req.mode.as_deref() {
        Some("named") => crate::config::Mode::Named,
        Some("managed") => crate::config::Mode::Managed,
        _ => crate::config::Mode::Quick,
    };
    if mode == crate::config::Mode::Named
        && state
            .tunnel_cfg
            .hostname
            .as_deref()
            .filter(|h| !h.is_empty())
            .is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            "named mode needs [tunnel] hostname in config.toml\n",
        )
            .into_response();
    }

    let mut cfg = state.tunnel_cfg.clone();
    cfg.provider = crate::config::Provider::Cloudflared;
    cfg.mode = mode;

    if mode == crate::config::Mode::Managed {
        // A pasted token wins and is persisted (survives restart); otherwise reuse
        // whatever the daemon booted with (config.toml or the saved device_token file).
        if let Some(dt) = req.device_token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            cfg.device_token = Some(dt.to_string());
            crate::config::save_device_token(dt);
        }
        if cfg.device_token.as_deref().filter(|t| !t.is_empty()).is_none() {
            return (
                StatusCode::BAD_REQUEST,
                "managed mode needs a device token — paste one from app.cchub.cloud/account\n",
            )
                .into_response();
        }
    }

    // A tunnel makes us publicly reachable — require a token from now on.
    let token = state.ensure_token().await;

    state
        .tunnel
        .start(mode, cfg, state.local_port, Some(token.clone()))
        .await;

    // Keep this (local) client authenticated now that auth just turned on.
    let cookie = format!("{}={token}; Path=/; Max-Age=31536000; SameSite=Lax; HttpOnly", "cc_token");
    let mut resp = Json(state.tunnel.status().await).into_response();
    if let Ok(v) = header::HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

/// POST /api/tunnel/stop — tear the tunnel down (auth stays enabled).
async fn tunnel_stop_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.tunnel.stop().await;
    Json(state.tunnel.status().await)
}

// ---- in-app account (email/password, proxied to the control plane) ----

#[derive(serde::Deserialize)]
struct AccountCreds {
    email: String,
    password: String,
}

/// Resolve the account state from the stored control-plane session cookie.
/// `signedIn` is true ONLY for a verified email — an unverified account is reported
/// as `pendingVerify` so the client treats "must verify before you're really in".
async fn account_me_value(state: &Arc<AppState>) -> serde_json::Value {
    let cp = state.tunnel_cfg.control_plane.clone();
    let Some(cookie) = state.account_cookie.lock().await.clone() else {
        return serde_json::json!({ "signedIn": false });
    };
    match crate::account::me(&cp, &cookie).await {
        Ok(Some(u)) => {
            let verified = u.get("email_verified").and_then(|v| v.as_bool()).unwrap_or(false);
            serde_json::json!({
                "signedIn": verified,
                "pendingVerify": !verified,
                "email": u.get("email").and_then(|v| v.as_str()),
                "plan": u.get("plan").and_then(|v| v.as_str()),
                "emailVerified": verified,
            })
        }
        _ => serde_json::json!({ "signedIn": false }),
    }
}

/// Mint + persist a device token and bring the managed tunnel up. The control plane
/// only issues a token to a verified account, so this is the enforcement point for
/// "remote requires a verified login". Returns Ok only when the tunnel is starting.
async fn enable_remote(state: &Arc<AppState>) -> Result<(), String> {
    let cp = state.tunnel_cfg.control_plane.clone();
    let cookie = state
        .account_cookie
        .lock()
        .await
        .clone()
        .ok_or_else(|| "not signed in".to_string())?;
    let token = crate::account::mint_device_token(&cp, &cookie)
        .await
        .map_err(|e| e.to_string())?;
    crate::config::save_device_token(&token);
    let auth = state.ensure_token().await;
    let mut cfg = state.tunnel_cfg.clone();
    cfg.provider = crate::config::Provider::Cloudflared;
    cfg.mode = crate::config::Mode::Managed;
    cfg.device_token = Some(token);
    state
        .tunnel
        .start(crate::config::Mode::Managed, cfg, state.local_port, Some(auth))
        .await;
    Ok(())
}

/// After a successful register/login: stash the cookie. If the email is already
/// verified, also enable remote (mint token + start tunnel) so it's ready at once.
/// Unverified accounts are reported as `pendingVerify` and get NO remote.
async fn after_account_auth(state: &Arc<AppState>, cookie: String) -> serde_json::Value {
    *state.account_cookie.lock().await = Some(cookie);
    let me = account_me_value(state).await;
    if me.get("signedIn").and_then(|v| v.as_bool()) == Some(true) {
        if let Err(e) = enable_remote(state).await {
            tracing::warn!(error = %e, "enable remote after auth failed");
        }
    }
    me
}

async fn account_register_handler(
    State(state): State<Arc<AppState>>,
    Json(c): Json<AccountCreds>,
) -> Response {
    let cp = state.tunnel_cfg.control_plane.clone();
    match crate::account::register(&cp, c.email.trim(), &c.password).await {
        Ok(cookie) => Json(after_account_auth(&state, cookie).await).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    }
}

async fn account_login_handler(
    State(state): State<Arc<AppState>>,
    Json(c): Json<AccountCreds>,
) -> Response {
    let cp = state.tunnel_cfg.control_plane.clone();
    match crate::account::login(&cp, c.email.trim(), &c.password).await {
        Ok(cookie) => Json(after_account_auth(&state, cookie).await).into_response(),
        Err(e) => (StatusCode::UNAUTHORIZED, format!("{e}")).into_response(),
    }
}

async fn account_me_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(account_me_value(&state).await)
}

async fn account_logout_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cookie = state.account_cookie.lock().await.take();
    if let Some(cookie) = cookie {
        let cp = state.tunnel_cfg.control_plane.clone();
        let _ = crate::account::logout(&cp, &cookie).await;
    }
    Json(serde_json::json!({ "ok": true }))
}

/// POST /api/account/resend-verify — re-send the verification email for the
/// currently signed-in (but unverified) account.
async fn account_resend_handler(State(state): State<Arc<AppState>>) -> Response {
    let cp = state.tunnel_cfg.control_plane.clone();
    let Some(cookie) = state.account_cookie.lock().await.clone() else {
        return (StatusCode::UNAUTHORIZED, "not signed in").into_response();
    };
    match crate::account::resend_verify(&cp, &cookie).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e}")).into_response(),
    }
}

/// POST /api/account/connect-remote — enable the managed private tunnel for the
/// signed-in account. Fails (403) if the email isn't verified (the control plane
/// won't issue a device token), which is exactly the gate we want.
async fn account_connect_remote_handler(State(state): State<Arc<AppState>>) -> Response {
    match enable_remote(&state).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => {
            let verified_issue = e.contains("email_not_verified");
            let code = if verified_issue { StatusCode::FORBIDDEN } else { StatusCode::BAD_GATEWAY };
            (code, e).into_response()
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// POST /api/link/connect — one-click managed link. Starts a device authorization
/// with the control plane; the user approves in their browser; a background task
/// polls for the issued device token, then persists it and brings the managed
/// tunnel up. The UI shows the user_code + opens `verification_uri`, then watches
/// `/api/link/connect/status` → `/api/tunnel/status`.
async fn link_connect_handler(State(state): State<Arc<AppState>>) -> Response {
    let cp = state.tunnel_cfg.control_plane.clone();
    let auth = match crate::provision::authorize(&cp).await {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "device authorize failed");
            return (StatusCode::BAD_GATEWAY, format!("{e}\n")).into_response();
        }
    };
    let expires_at = now_secs() + auth.expires_in;
    *state.connect.lock().await = crate::state::ConnectState::Pending {
        user_code: auth.user_code.clone(),
        verification_uri: auth.verification_uri_complete.clone(),
        expires_at,
    };
    let st = state.clone();
    let device_code = auth.device_code.clone();
    let interval = auth.interval.max(1);
    tokio::spawn(async move { poll_connect(st, cp, device_code, interval, expires_at).await });
    Json(state.connect.lock().await.clone()).into_response()
}

/// GET /api/link/connect/status — current device-authorization phase.
async fn link_connect_status_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.connect.lock().await.clone())
}

async fn poll_connect(state: Arc<AppState>, cp: String, device_code: String, interval: u64, deadline: u64) {
    use crate::state::ConnectState;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        if now_secs() >= deadline {
            *state.connect.lock().await = ConnectState::Error {
                message: "code expired — click Connect again".into(),
            };
            return;
        }
        match crate::provision::poll_token(&cp, &device_code).await {
            Ok(crate::provision::PollResult::Pending) => continue,
            Ok(crate::provision::PollResult::Failed(message)) => {
                *state.connect.lock().await = ConnectState::Error { message };
                return;
            }
            Ok(crate::provision::PollResult::Token(token)) => {
                tracing::info!("device linked — bringing managed tunnel up");
                crate::config::save_device_token(&token);
                let auth = state.ensure_token().await;
                let mut cfg = state.tunnel_cfg.clone();
                cfg.provider = crate::config::Provider::Cloudflared;
                cfg.mode = crate::config::Mode::Managed;
                cfg.device_token = Some(token);
                *state.connect.lock().await = ConnectState::Approved;
                state
                    .tunnel
                    .start(crate::config::Mode::Managed, cfg, state.local_port, Some(auth))
                    .await;
                return;
            }
            Err(e) => {
                // Transient network error — keep trying until the deadline.
                tracing::warn!(error = %e, "device token poll error");
                continue;
            }
        }
    }
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    tracing::info!("WS client connected");

    let hello = ServerMessage::Hello {
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        tmux_session: state.session.label(),
        windows: state.snapshot_windows().await,
        active_window_id: state.active_window_id().await,
        autopilot: state.autopilot.load(Ordering::Relaxed),
    };
    if socket.send(Message::Text(hello.to_json())).await.is_err() {
        return;
    }

    let mut out_rx = state.output_tx.subscribe();
    let mut evt_rx = state.event_tx.subscribe();

    // Paint the pane for this freshly-connected client. The single shared tmux PTY already
    // showed its content to its one client, so late-joining web clients (downstream of that
    // PTY) would otherwise see a blank terminal until the next output. We capture the pane
    // *with its full scrollback history* (with colours) and send it to *this* socket only —
    // deterministic, no reliance on a broadcast redraw or a SIGWINCH reaching us. Including the
    // history (vs. just the visible screen) lets the user scroll up to the very beginning. The
    // payload ends with an explicit cursor-position escape (see `capture_repaint`) so the caret
    // matches the real terminal — otherwise early keystrokes echo in the wrong place on TUIs.
    if let Ok(payload) = state.session.capture_repaint() {
        let msg = ServerMessage::Output {
            bytes: base64::engine::general_purpose::STANDARD.encode(payload.as_bytes()),
        };
        let _ = socket.send(Message::Text(msg.to_json())).await;
    }

    loop {
        tokio::select! {
            // PTY raw bytes (screen channel)
            recv = out_rx.recv() => {
                match recv {
                    Ok(bytes) => {
                        let msg = ServerMessage::Output {
                            bytes: base64::engine::general_purpose::STANDARD.encode(&bytes),
                        };
                        if socket.send(Message::Text(msg.to_json())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "WS client lagged behind output broadcast");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Structured events (windows / usage)
            recv = evt_rx.recv() => {
                match recv {
                    Ok(evt) => {
                        if socket.send(Message::Text(evt.to_json())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "WS client lagged behind event broadcast");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Client → server
            ws_msg = socket.recv() => {
                let Some(Ok(msg)) = ws_msg else { break };
                if !handle_client_message(msg, &state, &mut socket).await {
                    break;
                }
            }
        }
    }

    tracing::info!("WS client disconnected");
}

/// After the single shared tmux client re-points to a different window (a select
/// or a create — `new-window` auto-selects the new one), broadcast a deterministic
/// full repaint to every connected client. Window switching is global here (one
/// shared attach PTY), so all clients switch together and all need repainting.
///
/// Without this, clients rely on tmux's partial incremental redraw, which only
/// covers the new window's *visible* screen: the previous window's scrollback stays
/// in the xterm buffer and bleeds through when the user scrolls up. The leading
/// `\x1b[3J` wipes the client's scrollback; `capture_repaint` already begins with
/// clear-screen + home and replays the *new* window's own history, so the terminal
/// ends up showing exactly the selected window and nothing from the previous one.
fn repaint_after_switch(state: &Arc<AppState>) {
    match state.session.capture_repaint() {
        Ok(payload) => {
            let mut bytes = b"\x1b[3J".to_vec();
            bytes.extend_from_slice(payload.as_bytes());
            let _ = state.output_tx.send(bytes);
        }
        Err(e) => tracing::warn!(?e, "capture_repaint after window switch failed"),
    }
}

/// Returns false to break the WS loop.
async fn handle_client_message(
    msg: Message,
    state: &Arc<AppState>,
    socket: &mut WebSocket,
) -> bool {
    match msg {
        Message::Text(text) => {
            let parsed: serde_json::Result<ClientMessage> = serde_json::from_str(&text);
            match parsed {
                Ok(ClientMessage::Input { text }) => {
                    if let Err(e) = state.session.write_input(text.as_bytes()) {
                        tracing::error!(?e, "session write failed");
                        return false;
                    }
                }
                Ok(ClientMessage::Resize { cols, rows }) => {
                    if let Err(e) = state.session.resize(cols, rows) {
                        tracing::warn!(?e, "session resize failed");
                    }
                }
                Ok(ClientMessage::SetAutopilot { enabled }) => {
                    state.autopilot.store(enabled, Ordering::Relaxed);
                    tracing::info!(enabled, "autopilot toggled");
                    // Broadcast so every connected client (including this one) syncs its toggle.
                    let _ = state.event_tx.send(ServerMessage::AutopilotChanged { enabled });
                }
                Ok(ClientMessage::WindowList) => {
                    let resp = ServerMessage::WindowList { windows: state.snapshot_windows().await };
                    let _ = socket.send(Message::Text(resp.to_json())).await;
                }
                Ok(ClientMessage::WindowSelect { window_id }) => {
                    if let Err(e) = state.session.select_window(&window_id) {
                        tracing::warn!(?e, %window_id, "select_window failed");
                        let _ = socket
                            .send(Message::Text(
                                ServerMessage::Error { message: format!("select_window: {e}") }.to_json(),
                            ))
                            .await;
                    } else {
                        repaint_after_switch(state);
                    }
                }
                Ok(ClientMessage::WindowCreate { name, cwd, cmd }) => {
                    // Expand `~` here — tmux's `new-window -c ~` fails on a literal "~"
                    // (the new-window modal sends `~` as the default cwd on a fresh install).
                    let cwd = cwd.as_deref().map(expand_tilde);
                    if let Err(e) = state.session.new_window(name.as_deref(), cwd.as_deref(), cmd.as_deref()) {
                        tracing::warn!(?e, "new_window failed");
                        let _ = socket
                            .send(Message::Text(
                                ServerMessage::Error { message: format!("new_window: {e}") }.to_json(),
                            ))
                            .await;
                    } else {
                        repaint_after_switch(state);
                    }
                }
                Ok(ClientMessage::WindowRename { window_id, name }) => {
                    if let Err(e) = state.session.rename_window(&window_id, &name) {
                        tracing::warn!(?e, "rename_window failed");
                    }
                }
                Ok(ClientMessage::WindowChdir { window_id, path }) => {
                    if let Err(e) = state.session.change_dir(&window_id, &path) {
                        tracing::warn!(?e, %window_id, "change_dir failed");
                        let _ = socket
                            .send(Message::Text(
                                ServerMessage::Error { message: format!("change_dir: {e}") }.to_json(),
                            ))
                            .await;
                    }
                }
                Ok(ClientMessage::WindowKill { window_id }) => {
                    // Refuse to close the last window: killing it would tear down the whole
                    // tmux session/server, taking the daemon's PTY broker down with it.
                    let count = state.windows.read().await.len();
                    if count <= 1 {
                        let _ = socket
                            .send(Message::Text(
                                ServerMessage::Error {
                                    message: "cannot close the last window".into(),
                                }
                                .to_json(),
                            ))
                            .await;
                    } else if let Err(e) = state.session.kill_window(&window_id) {
                        tracing::warn!(?e, "kill_window failed");
                    }
                }
                Err(e) => {
                    tracing::warn!(?e, raw = %text, "malformed client message");
                }
            }
            true
        }
        Message::Close(_) => false,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT: &str = "\
⏺ I'll edit the file now.

╭──────────────────────────────────────────────────╮
│ Do you want to make this edit to server.rs?       │
│                                                    │
│ ❯ 1. Yes                                           │
│   2. Yes, allow all edits this session             │
│   3. No, and tell Claude what to do differently    │
╰──────────────────────────────────────────────────╯";

    // Captured verbatim from claude 2.1 (Write tool) during end-to-end testing —
    // note option 3 here is just "No", which an earlier draft of the heuristic missed.
    const REAL_WRITE_PROMPT: &str = "\
⏺ Write(/tmp/cc-test.txt)

 Create file
 ../../../../../../tmp/cc-test.txt
 Do you want to create cc-test.txt?
 ❯ 1. Yes
   2. Yes, allow all edits in tmp/ during this session (shift+tab)
   3. No

 Esc to cancel · Tab to amend";

    // Captured verbatim from codex-cli 0.137.0 (command approval, `-a untrusted`).
    const CODEX_PROMPT: &str = "\
• Running mkdir -p /tmp/ccverify_dir && touch /tmp/ccverify_dir/x

  Would you like to run the following command?

  $ mkdir -p /tmp/ccverify_dir && touch /tmp/ccverify_dir/x

› 1. Yes, proceed (y)
  2. Yes, and don't ask again for commands that start with `mkdir -p /tmp/ccverify_dir` (p)
  3. No, and tell Codex what to do differently (esc)

  Press enter to confirm or esc to cancel";

    // Captured verbatim from gemini-cli 0.45.2 (shell-exec confirmation).
    const GEMINI_PROMPT: &str = "\
╭────────────────────────────────────────────────────╮
│ ? Shell  touch /tmp/ccverify_gem.txt              │
│ ╭────────────────────────────────────────────────╮ │
│ │ touch /tmp/ccverify_gem.txt                    │ │
│ ╰────────────────────────────────────────────────╯ │
│ Allow execution of [Shell]?                       │
│                                                    │
│ ● 1. Allow once                                    │
│   2. Allow for this session                        │
│   3. No, suggest changes (esc)                     │
╰────────────────────────────────────────────────────╯";

    #[test]
    fn detects_confirmation_prompt() {
        let preview = detect_awaiting(PROMPT, "claude").expect("should detect");
        assert!(preview.contains("Do you want to make this edit"), "preview: {preview}");
    }

    #[test]
    fn detects_real_write_prompt() {
        let preview =
            detect_awaiting(REAL_WRITE_PROMPT, "claude").expect("should detect real prompt");
        assert!(preview.contains("Do you want to create"), "preview: {preview}");
    }

    #[test]
    fn detects_codex_prompt() {
        let preview = detect_awaiting(CODEX_PROMPT, "codex").expect("should detect codex prompt");
        // Preview is the `$ <command>` line, not the generic question.
        assert!(preview.contains("$ mkdir -p /tmp/ccverify_dir"), "preview: {preview}");
    }

    #[test]
    fn detects_gemini_prompt() {
        let preview =
            detect_awaiting(GEMINI_PROMPT, "gemini").expect("should detect gemini prompt");
        assert!(preview.contains("Allow execution of [Shell]?"), "preview: {preview}");
    }

    #[test]
    fn agent_detectors_dont_cross_match() {
        // A claude prompt fed to the codex/gemini detector must not trip, and vice versa.
        assert!(detect_awaiting(PROMPT, "codex").is_none());
        assert!(detect_awaiting(PROMPT, "gemini").is_none());
        assert!(detect_awaiting(CODEX_PROMPT, "gemini").is_none());
    }

    #[test]
    fn ignores_ordinary_output() {
        // Mentions yes/no but no selector + no agent phrasing → not a prompt, for any agent.
        let pane = "Tests passed. 1 yes, 0 no. All good, proceeding.";
        assert!(detect_awaiting(pane, "claude").is_none());
        assert!(detect_awaiting(pane, "codex").is_none());
        assert!(detect_awaiting(pane, "gemini").is_none());
        // Unknown agent kind never matches.
        assert!(detect_awaiting(PROMPT, "other").is_none());
    }

    #[test]
    fn clean_box_line_strips_borders() {
        assert_eq!(clean_box_line("│ Do you want to proceed?   │"), "Do you want to proceed?");
    }
}
