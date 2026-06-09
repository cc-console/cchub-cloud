//! cloudflared supervisor — the "L2" public-entry layer from docs/05.
//!
//! `TunnelManager` owns at most one running `cloudflared` child and exposes
//! start/stop/status so both the boot path (config.toml `[tunnel]`) and the
//! on-demand path (settings "Remote access" button → `/api/tunnel/*`) drive the
//! same state machine.
//!
//! We never hardcode any hostname: `quick` mode uses an ephemeral
//! `*.trycloudflare.com` URL; `named` mode uses the user's OWN Cloudflare domain.
//! A tunnel makes the daemon publicly reachable, so callers enable token auth
//! (`AppState::ensure_token`) before starting one.

use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::config::{Mode, TunnelConfig};

/// Public status of the tunnel, serialised to the settings UI.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum TunnelStatus {
    /// No tunnel running.
    Off,
    /// Spawned cloudflared, waiting for it to register / print its URL.
    Starting { mode: String },
    /// Healthy; `url` is the public, reachable address (with `?token=` appended).
    Up { mode: String, url: String },
    /// Last attempt failed; `message` explains why.
    Error { message: String },
}

pub struct TunnelManager {
    inner: Mutex<Inner>,
}

struct Inner {
    status: TunnelStatus,
    /// Supervisor task; aborting it drops the child (kill_on_drop) → cloudflared dies.
    task: Option<JoinHandle<()>>,
}

impl TunnelManager {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            inner: Mutex::new(Inner {
                status: TunnelStatus::Off,
                task: None,
            }),
        })
    }

    pub async fn status(&self) -> TunnelStatus {
        self.inner.lock().await.status.clone()
    }

    async fn set_status(&self, s: TunnelStatus) {
        self.inner.lock().await.status = s;
    }

    /// Stop any running tunnel and reset to Off.
    pub async fn stop(&self) {
        let mut inner = self.inner.lock().await;
        if let Some(task) = inner.task.take() {
            task.abort();
        }
        inner.status = TunnelStatus::Off;
        tracing::info!("tunnel stopped");
    }

    /// Start (or restart) a tunnel in the given mode. Returns immediately after the
    /// supervisor task is spawned; callers poll `status()` for the URL. `token` is
    /// appended to the announced URL so the first visit authenticates.
    pub async fn start(
        self: &std::sync::Arc<Self>,
        mode: Mode,
        cfg: TunnelConfig,
        local_port: u16,
        token: Option<String>,
    ) {
        // Replace any existing tunnel.
        {
            let mut inner = self.inner.lock().await;
            if let Some(task) = inner.task.take() {
                task.abort();
            }
            inner.status = TunnelStatus::Starting {
                mode: mode_str(mode).to_string(),
            };
        }
        let me = self.clone();
        let handle = tokio::spawn(async move {
            me.supervise(mode, cfg, local_port, token).await;
        });
        self.inner.lock().await.task = Some(handle);
    }

    /// Run one cloudflared lifecycle, updating status as it goes. On unexpected exit
    /// the status becomes Error/Off (the on-demand model lets the user click again);
    /// we deliberately don't crash-loop here.
    async fn supervise(
        &self,
        mode: Mode,
        mut cfg: TunnelConfig,
        local_port: u16,
        token: Option<String>,
    ) {
        let bin = match resolve_bin(&cfg).await {
            Some(b) => b,
            None => {
                let hint = if cfg!(windows) {
                    "winget install --id Cloudflare.cloudflared, or scoop install cloudflared"
                } else if cfg!(target_os = "macos") {
                    "brew install cloudflared"
                } else {
                    "see https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/"
                };
                self.set_status(TunnelStatus::Error {
                    message: format!(
                        "cloudflared not found — install it ({hint}) or set [tunnel] cloudflared_path in config.toml"
                    ),
                })
                .await;
                tracing::error!("tunnel: cloudflared not found");
                return;
            }
        };

        if mode == Mode::Named {
            if let Err(e) = ensure_named_setup(&bin, &cfg).await {
                self.set_status(TunnelStatus::Error {
                    message: e.to_string(),
                })
                .await;
                tracing::error!(error = %e, "named-tunnel setup failed");
                return;
            }
        }

        // Managed: the control plane owns the tunnel. Trade the device token for a
        // connector token + the public hostname, then run cloudflared with that token.
        let mut managed_token: Option<String> = None;
        if mode == Mode::Managed {
            let Some(device_token) = cfg.device_token.clone().filter(|t| !t.is_empty()) else {
                self.set_status(TunnelStatus::Error {
                    message: "no device token — paste one from app.cchub.cloud/account".to_string(),
                })
                .await;
                tracing::error!("managed tunnel: missing device token");
                return;
            };
            match crate::provision::provision(&cfg.control_plane, &device_token, local_port).await {
                Ok(p) => {
                    tracing::info!(hostname = %p.hostname, "managed tunnel provisioned");
                    cfg.hostname = Some(p.hostname);
                    managed_token = Some(p.tunnel_token);
                }
                Err(e) => {
                    self.set_status(TunnelStatus::Error {
                        message: e.to_string(),
                    })
                    .await;
                    tracing::error!(error = %e, "managed-tunnel provision failed");
                    return;
                }
            }
        }

        let local_url = format!("http://127.0.0.1:{local_port}");
        if let Err(e) = self
            .run_child(
                &bin,
                mode,
                &cfg,
                &local_url,
                token.as_deref(),
                managed_token.as_deref(),
            )
            .await
        {
            self.set_status(TunnelStatus::Error {
                message: e.to_string(),
            })
            .await;
            tracing::error!(error = %e, "cloudflared run error");
        }
    }

    async fn run_child(
        &self,
        bin: &str,
        mode: Mode,
        cfg: &TunnelConfig,
        local_url: &str,
        token: Option<&str>,
        managed_token: Option<&str>,
    ) -> Result<()> {
        let mut cmd = Command::new(bin);
        // Isolate from the user's own ~/.cloudflared/config.yml (named/quick only).
        // cloudflared loads it by default; if it defines a named tunnel with a
        // catch-all ingress (e.g. `service: http_status:404`), that ingress hijacks
        // OUR tunnel and 404s every request whose hostname doesn't match. An empty
        // config = no ingress binding. Managed mode runs with `--token`, which pulls
        // its ingress from the remote (control-plane) config, so we skip this.
        if mode != Mode::Managed {
            if let Some(empty) = empty_config_path() {
                cmd.arg("--config").arg(empty);
            }
        }
        cmd.arg("tunnel").arg("--no-autoupdate");
        match mode {
            Mode::Named => {
                cmd.args(["run", "--url", local_url, &cfg.name]);
            }
            Mode::Quick => {
                cmd.args(["--url", local_url]);
            }
            Mode::Managed => {
                // Ingress (→ our local port) and DNS were configured server-side; the
                // connector token is all cloudflared needs to dial out and serve.
                let tok = managed_token.context("managed mode requires a connector token")?;
                cmd.args(["run", "--token", tok]);
            }
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        tracing::info!(?mode, %local_url, "starting cloudflared");
        let mut child = cmd.spawn().context("failed to spawn cloudflared")?;
        let stderr = child.stderr.take().context("no stderr pipe")?;
        let stdout = child.stdout.take().context("no stdout pipe")?;

        // Watch logs: cloudflared (and the quick-tunnel URL) go to stderr.
        let token_owned = token.map(|s| s.to_string());
        let hostname = cfg.hostname.clone();
        let mode_s = mode_str(mode).to_string();
        let url_seen = std::sync::Arc::new(tokio::sync::Notify::new());
        let url_slot: std::sync::Arc<Mutex<Option<String>>> =
            std::sync::Arc::new(Mutex::new(None));
        {
            let url_slot = url_slot.clone();
            let url_seen = url_seen.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let found = extract_trycloudflare(&line).or_else(|| {
                        if line.contains("Registered tunnel connection") {
                            hostname.as_deref().map(|h| format!("https://{h}"))
                        } else {
                            None
                        }
                    });
                    if let Some(base) = found {
                        let mut slot = url_slot.lock().await;
                        if slot.is_none() {
                            let full = match token_owned.as_deref() {
                                Some(t) => format!("{base}/?token={t}"),
                                None => base.clone(),
                            };
                            tracing::info!("tunnel is up — open: {full}");
                            *slot = Some(full);
                            url_seen.notify_one();
                        }
                    }
                    tracing::debug!(target: "cloudflared", "{}", line.trim_end());
                }
            });
        }
        // Drain stdout so the pipe never blocks cloudflared.
        let drain = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "cloudflared", "{}", line.trim_end());
            }
        });

        // Promote to Up as soon as we learn the URL, while the child keeps running.
        let promote = {
            let url_slot = url_slot.clone();
            async move {
                url_seen.notified().await;
                if let Some(url) = url_slot.lock().await.clone() {
                    self.set_status(TunnelStatus::Up { mode: mode_s, url }).await;
                }
            }
        };

        // Race: child exit vs. URL discovery; keep waiting on the child either way.
        let status = tokio::select! {
            s = child.wait() => s,
            _ = promote => child.wait().await,
        };
        drain.abort();
        let status = status.context("waiting on cloudflared")?;
        // Child ended — reflect that (Error if it never came up cleanly).
        self.set_status(TunnelStatus::Error {
            message: format!("cloudflared exited ({status})"),
        })
        .await;
        tracing::warn!(?status, "cloudflared process ended");
        Ok(())
    }
}

fn mode_str(m: Mode) -> &'static str {
    match m {
        Mode::Named => "named",
        Mode::Quick => "quick",
        Mode::Managed => "managed",
    }
}

/// Resolve the cloudflared binary: explicit `cloudflared_path`, else `cloudflared`
/// on PATH. Verifies it actually runs (`--version`).
async fn resolve_bin(cfg: &TunnelConfig) -> Option<String> {
    let cand = cfg
        .cloudflared_path
        .clone()
        .unwrap_or_else(|| "cloudflared".to_string());
    match Command::new(&cand).arg("--version").output().await {
        Ok(o) if o.status.success() => Some(cand),
        _ => None,
    }
}

/// Ensure the named tunnel is ready: login cert present, tunnel created, DNS routed.
/// Steps are idempotent / tolerant of "already exists".
async fn ensure_named_setup(bin: &str, cfg: &TunnelConfig) -> Result<()> {
    let hostname = cfg.hostname.as_deref().filter(|h| !h.is_empty()).context(
        "[tunnel] mode = \"named\" requires `hostname = \"cc.yourdomain.com\"` in config.toml",
    )?;

    let cert = crate::paths::home_dir()
        .map(|h| h.join(".cloudflared/cert.pem"))
        .filter(|p| p.exists());
    if cert.is_none() {
        bail!(
            "no Cloudflare login found (~/.cloudflared/cert.pem). Run `cloudflared tunnel login` once \
             and pick the zone for {hostname}, then try again."
        );
    }

    let list = Command::new(bin)
        .args(["tunnel", "list"])
        .output()
        .await
        .context("`cloudflared tunnel list` failed")?;
    let listed = String::from_utf8_lossy(&list.stdout);
    let exists = listed
        .lines()
        .any(|l| l.split_whitespace().any(|tok| tok == cfg.name));
    if !exists {
        tracing::info!(name = %cfg.name, "creating cloudflared tunnel");
        let out = Command::new(bin)
            .args(["tunnel", "create", &cfg.name])
            .output()
            .await
            .context("`cloudflared tunnel create` failed")?;
        if !out.status.success() {
            bail!(
                "`cloudflared tunnel create {}` failed: {}",
                cfg.name,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }

    let out = Command::new(bin)
        .args(["tunnel", "route", "dns", &cfg.name, hostname])
        .output()
        .await
        .context("`cloudflared tunnel route dns` failed")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
        if err.contains("already") || err.contains("exists") {
            tracing::debug!(%hostname, "DNS route already present");
        } else {
            bail!(
                "`cloudflared tunnel route dns {} {hostname}` failed: {}",
                cfg.name,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    tracing::info!(%hostname, name = %cfg.name, "named tunnel ready");
    Ok(())
}

/// Path to an empty cloudflared config we pass via `--config`, so the user's own
/// `~/.cloudflared/config.yml` (named-tunnel ingress) never hijacks our tunnel.
/// Best-effort: returns None if `$HOME` is unset (then we just don't isolate).
fn empty_config_path() -> Option<std::path::PathBuf> {
    let dir = crate::paths::home_dir().map(|h| h.join(".claude/cc-console"))?;
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("cloudflared-empty.yml");
    if !path.exists() {
        let _ = std::fs::write(&path, b"# intentionally empty: isolates cc-console's tunnel from ~/.cloudflared/config.yml\n");
    }
    Some(path)
}

/// Pull a `https://<sub>.trycloudflare.com` URL out of a cloudflared log line.
fn extract_trycloudflare(line: &str) -> Option<String> {
    let i = line.find("https://")?;
    let rest = &line[i..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|')
        .unwrap_or(rest.len());
    let url = &rest[..end];
    if url.contains("trycloudflare.com") {
        Some(url.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_quick_url() {
        let line = "2026-06-04 INF |  https://brave-cats-run.trycloudflare.com  |";
        assert_eq!(
            extract_trycloudflare(line).as_deref(),
            Some("https://brave-cats-run.trycloudflare.com")
        );
    }

    #[test]
    fn ignores_non_quick_lines() {
        assert!(extract_trycloudflare("Registered tunnel connection conn=0").is_none());
        assert!(extract_trycloudflare("see https://developers.cloudflare.com/x").is_none());
    }
}
