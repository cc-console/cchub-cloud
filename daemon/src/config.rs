//! Optional on-disk config at `~/.claude/cc-console/config.toml`.
//!
//! Everything is optional; a missing file means "no tunnel, loopback only" —
//! exactly the prior behaviour. The tunnel is **off by default**; the user opts
//! in by setting `[tunnel] provider = "cloudflared"` and choosing a mode.
//!
//! ```toml
//! [tunnel]
//! provider = "cloudflared"   # "none" (default) | "cloudflared"
//! mode     = "named"         # "named" (stable, needs your own domain) | "quick" (ephemeral trycloudflare URL)
//! hostname = "cc.example.com"  # required for `named`; this is YOUR domain on Cloudflare
//! name     = "cc-console"      # tunnel name (named mode)
//! # cloudflared_path = "/opt/homebrew/bin/cloudflared"  # optional; else found on PATH
//! ```

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub tunnel: TunnelConfig,
    pub session: SessionConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Extra environment variables for the AI CLIs (claude/codex/gemini) running in
    /// the session. The desktop app launches from the GUI, which — unlike your login
    /// shell — carries no proxy vars, so `claude` can't reach Anthropic through your
    /// local proxy and OAuth fails. Set them here, e.g.:
    ///
    /// ```toml
    /// [session.env]
    /// HTTPS_PROXY = "http://127.0.0.1:7897"
    /// HTTP_PROXY  = "http://127.0.0.1:7897"
    /// ALL_PROXY   = "http://127.0.0.1:7897"
    /// ```
    pub env: BTreeMap<String, String>,

    /// When `env` defines no proxy, probe localhost for a running proxy (Clash,
    /// v2rayN, Surge, …) at startup and inject HTTP(S)_PROXY/ALL_PROXY for it, so
    /// the GUI app works out of the box on any machine without hand-editing this
    /// file. Anything you put in `env` always wins; set this to `false` to opt out:
    ///
    /// ```toml
    /// [session]
    /// auto_proxy = false
    /// ```
    pub auto_proxy: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            env: BTreeMap::new(),
            auto_proxy: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// No tunnel — daemon stays on its bind address only (default).
    #[default]
    None,
    /// Supervise a `cloudflared` child process.
    Cloudflared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Stable hostname under the user's own Cloudflare domain. Requires a prior
    /// `cloudflared tunnel login`.
    #[default]
    Named,
    /// Zero-config ephemeral `*.trycloudflare.com` URL (changes every run).
    Quick,
    /// Hosted: a stable `<handle>.cchub.cloud` address provisioned by the
    /// cc-console control plane (Phase 2). No `cloudflared login`, no own domain —
    /// the daemon trades a device token for a connector token and just runs it.
    Managed,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TunnelConfig {
    pub provider: Provider,
    pub mode: Mode,
    /// Required for `named` mode — the user's own hostname (e.g. `cc.alice.dev`).
    pub hostname: Option<String>,
    /// cloudflared tunnel name for `named` mode.
    pub name: String,
    /// Explicit path to the `cloudflared` binary; otherwise resolved from PATH.
    pub cloudflared_path: Option<String>,
    /// `managed` mode: base URL of the cc-console control plane that issues the
    /// connector token (`POST /api/link/provision`).
    pub control_plane: String,
    /// `managed` mode: device token (`ccd_…`) tying this daemon to your account.
    /// Set it from the settings UI or paste it here; it is also persisted to
    /// `~/.claude/cc-console/device_token` so it survives restarts.
    pub device_token: Option<String>,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            provider: Provider::None,
            mode: Mode::Named,
            hostname: None,
            name: "cc-console".to_string(),
            cloudflared_path: None,
            control_plane: default_control_plane(),
            device_token: None,
        }
    }
}

fn default_control_plane() -> String {
    "https://app.cchub.cloud".to_string()
}

/// Path to the persisted device token (written by the settings "Connect" flow).
pub fn device_token_path() -> Option<PathBuf> {
    crate::paths::home_dir().map(|h| h.join(".claude/cc-console/device_token"))
}

/// Persist a managed-mode device token (0600 on unix) so the daemon reconnects on
/// the next boot. Shared by the GUI "Connect" flow, the in-app account login, and
/// the headless `cc-console link` command. Best-effort: a write failure only costs
/// the user a re-paste next time.
pub fn save_device_token(token: &str) {
    let Some(path) = device_token_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, token) {
        tracing::warn!(error = %e, "could not persist device token");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

impl TunnelConfig {
    pub fn enabled(&self) -> bool {
        self.provider == Provider::Cloudflared
    }
}

pub fn config_path() -> Option<PathBuf> {
    crate::paths::home_dir().map(|h| h.join(".claude/cc-console/config.toml"))
}

/// Common local proxy ports, in priority order. Mixed-mode ports (Clash/mihomo,
/// which serve HTTP and SOCKS on a single port) come first since that's the usual
/// setup; then v2rayN (10809 http / 10808 socks), Surge (6152/6153), and the bare
/// SOCKS/HTTP defaults.
const PROXY_PORTS: &[u16] = &[
    7897, 7890, 7891, 2080, 10809, 10808, 1087, 8889, 8888, 6152, 6153, 1080,
];

/// True if a proxy is already configured — either in `[session.env]` or already in
/// the daemon's own environment (e.g. launched from a terminal that exports it).
/// In both cases auto-detection should stand down and let the explicit value win.
fn proxy_already_set(env: &BTreeMap<String, String>) -> bool {
    let in_cfg = |k: &str| env.keys().any(|e| e.eq_ignore_ascii_case(k));
    let in_env = |k: &str| std::env::var_os(k).is_some();
    ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"]
        .iter()
        .any(|k| in_cfg(k) || in_env(k) || in_env(&k.to_lowercase()))
}

/// Probe v4 loopback for a listening proxy port, returning the first that accepts
/// a TCP connection within a short timeout. Loopback only — never touches the LAN.
fn detect_local_proxy() -> Option<u16> {
    PROXY_PORTS.iter().copied().find(|&port| {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        TcpStream::connect_timeout(&addr, Duration::from_millis(120)).is_ok()
    })
}

/// Load config, tolerating a missing file (returns defaults) but logging a parse
/// error loudly while still falling back to defaults so the daemon keeps running.
pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(s) => match toml::from_str::<Config>(&s) {
            Ok(c) => {
                tracing::info!(path = %path.display(), "loaded config");
                c
            }
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "config parse error — ignoring file, using defaults");
                Config::default()
            }
        },
        // No file is the normal case; don't warn.
        Err(_) => Config::default(),
    };
    // A device token saved by the settings "Connect" flow (or `cc-console link`)
    // lives outside config.toml (so we never rewrite the user's hand-edited file).
    // It only fills in when the config didn't already specify one.
    let mut token_from_file = false;
    if cfg.tunnel.device_token.is_none() {
        if let Some(tok) = device_token_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            cfg.tunnel.device_token = Some(tok);
            token_from_file = true;
        }
    }
    // A linked device (token persisted to its own file by `cc-console link` or the
    // GUI "Connect" flow) means the user wants the managed tunnel — so auto-enable
    // it on boot when config.toml didn't choose a provider. This makes a headless
    // `cc-console link` (and a GUI pairing) survive restarts with no hand-editing.
    // We gate on the *file* token, never a config.toml one: if you hand-wrote a
    // token into config.toml, your explicit `provider` (even "none") always wins.
    if token_from_file && cfg.tunnel.provider == Provider::None {
        tracing::info!(
            "linked device token found → starting the managed tunnel on boot \
             (set [tunnel] provider in config.toml to override)"
        );
        cfg.tunnel.provider = Provider::Cloudflared;
        cfg.tunnel.mode = Mode::Managed;
    }
    // Zero-config proxy: the GUI app inherits no shell proxy, so probe localhost for
    // a running proxy and inject it — unless the user set one or opted out. This is
    // a heuristic (assumes the port speaks HTTP for HTTP(S)_PROXY and SOCKS for
    // ALL_PROXY, which holds for Clash/mihomo mixed ports); explicit `env` wins.
    if cfg.session.auto_proxy && !proxy_already_set(&cfg.session.env) {
        if let Some(port) = detect_local_proxy() {
            let host = format!("127.0.0.1:{port}");
            tracing::info!(
                %host,
                "auto-detected local proxy → injecting HTTP(S)_PROXY/ALL_PROXY \
                 (disable with [session] auto_proxy = false)"
            );
            let env = &mut cfg.session.env;
            env.insert("HTTP_PROXY".into(), format!("http://{host}"));
            env.insert("HTTPS_PROXY".into(), format!("http://{host}"));
            env.insert("ALL_PROXY".into(), format!("socks5://{host}"));
            env.insert("NO_PROXY".into(), "localhost,127.0.0.1,::1".into());
        }
    }
    cfg
}
