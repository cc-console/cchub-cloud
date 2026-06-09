//! Control-plane handshake for `managed` tunnels (Phase 2).
//!
//! A managed tunnel is owned by the cc-console control plane: it holds the
//! Cloudflare credentials and runs `cf.ts` to create the per-user named tunnel,
//! point its ingress at our local port, and add the `<handle>.cchub.cloud`
//! DNS record. The daemon's only job is to authenticate with its device token,
//! report the local port, and run the connector token it's handed back.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct ProvisionReq {
    port: u16,
}

#[derive(Deserialize)]
struct ProvisionResp {
    hostname: String,
    tunnel_token: String,
}

/// What `managed` mode needs to start cloudflared: the public hostname (for the
/// announced URL) and the connector token (`cloudflared tunnel run --token`).
pub struct Provisioned {
    pub hostname: String,
    pub tunnel_token: String,
}

/// Exchange the device token + local port for a connector token, by calling
/// `POST <control_plane>/api/link/provision`. The control plane creates/updates
/// the user's tunnel, ingress, and DNS as a side effect.
pub async fn provision(control_plane: &str, device_token: &str, port: u16) -> Result<Provisioned> {
    let base = control_plane.trim_end_matches('/');
    let url = format!("{base}/api/link/provision");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let res = client
        .post(&url)
        .bearer_auth(device_token)
        .json(&ProvisionReq { port })
        .send()
        .await
        .with_context(|| format!("could not reach control plane at {url}"))?;

    let status = res.status();
    if !status.is_success() {
        // The control plane returns {"error":"…"} for the cases the user can fix
        // (no handle yet, bad/expired device token, no subscription).
        let body = res.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| body.trim().to_string());
        bail!("provision failed ({status}): {msg}");
    }

    let resp: ProvisionResp = res.json().await.context("malformed provision response")?;
    if resp.hostname.is_empty() || resp.tunnel_token.is_empty() {
        bail!("provision response missing hostname or tunnel_token");
    }
    Ok(Provisioned {
        hostname: resp.hostname,
        tunnel_token: resp.tunnel_token,
    })
}

// ---- device authorization flow (one-click linking, RFC 8628) ----

#[derive(Deserialize)]
pub struct Authorization {
    pub device_code: String,
    pub user_code: String,
    /// Pre-filled approval URL the daemon opens in the user's browser.
    pub verification_uri_complete: String,
    pub interval: u64,
    pub expires_in: u64,
}

/// What a poll of `/api/device/token` yields.
pub enum PollResult {
    Pending,
    Token(String),
    Failed(String),
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")
}

/// Start a device authorization: returns the user code + the URL to open.
pub async fn authorize(control_plane: &str) -> Result<Authorization> {
    let base = control_plane.trim_end_matches('/');
    let url = format!("{base}/api/device/authorize");
    let res = client()?
        .post(&url)
        .send()
        .await
        .with_context(|| format!("could not reach control plane at {url}"))?;
    if !res.status().is_success() {
        bail!("authorize failed ({})", res.status());
    }
    res.json::<Authorization>().await.context("malformed authorize response")
}

#[derive(Deserialize)]
struct TokenResp {
    token: Option<String>,
    error: Option<String>,
}

/// Poll once for the issued device token. `authorization_pending` → keep waiting.
pub async fn poll_token(control_plane: &str, device_code: &str) -> Result<PollResult> {
    let base = control_plane.trim_end_matches('/');
    let url = format!("{base}/api/device/token");
    let res = client()?
        .post(&url)
        .json(&serde_json::json!({ "device_code": device_code }))
        .send()
        .await
        .with_context(|| format!("could not reach control plane at {url}"))?;
    let body: TokenResp = res.json().await.context("malformed token response")?;
    if let Some(t) = body.token.filter(|t| !t.is_empty()) {
        return Ok(PollResult::Token(t));
    }
    match body.error.as_deref() {
        Some("authorization_pending") => Ok(PollResult::Pending),
        Some("access_denied") => Ok(PollResult::Failed("authorization denied".into())),
        Some("expired_token") => Ok(PollResult::Failed("code expired — try again".into())),
        Some(other) => Ok(PollResult::Failed(other.to_string())),
        None => Ok(PollResult::Pending),
    }
}
