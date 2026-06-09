//! In-app account auth. The desktop client's email/password register / login /
//! logout are proxied **server-to-server** by the daemon to the cc-console control
//! plane (`app.cchub.cloud/api/auth/*`) — no browser round-trip, no CORS, and the
//! session cookie never touches the webview. On a successful login the daemon also
//! mints a device token (`/api/device/new`) so managed remote links work right away.
//!
//! OAuth (GitHub / Google) still needs a browser redirect, so those stay as
//! "continue in the browser" links in the UI; only email/password is in-app.

use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::header::SET_COOKIE;
use serde_json::Value;

const COOKIE_NAME: &str = "cc_session";

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| anyhow!("http client: {e}"))
}

/// Pull the `cc_session=…` pair out of a response's Set-Cookie headers.
fn capture_session_cookie(resp: &reqwest::Response) -> Option<String> {
    for hv in resp.headers().get_all(SET_COOKIE).iter() {
        if let Ok(s) = hv.to_str() {
            let pair = s.split(';').next().unwrap_or("").trim();
            if pair.starts_with(&format!("{COOKIE_NAME}=")) && pair.len() > COOKIE_NAME.len() + 1 {
                return Some(pair.to_string());
            }
        }
    }
    None
}

/// Read `{ "error": "..." }` from a non-OK JSON body, else a generic message.
async fn error_message(resp: reqwest::Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| format!("HTTP {status}"))
}

/// POST /api/auth/register. Returns the new session cookie on success.
pub async fn register(cp: &str, email: &str, password: &str) -> Result<String> {
    let url = format!("{}/api/auth/register", cp.trim_end_matches('/'));
    let resp = client()?
        .post(&url)
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await
        .map_err(|e| anyhow!("register request: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!(error_message(resp).await));
    }
    let cookie = capture_session_cookie(&resp).ok_or_else(|| anyhow!("no session cookie returned"))?;
    Ok(cookie)
}

/// POST /api/auth/login. Returns the session cookie on success.
pub async fn login(cp: &str, email: &str, password: &str) -> Result<String> {
    let url = format!("{}/api/auth/login", cp.trim_end_matches('/'));
    let resp = client()?
        .post(&url)
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await
        .map_err(|e| anyhow!("login request: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!(error_message(resp).await));
    }
    let cookie = capture_session_cookie(&resp).ok_or_else(|| anyhow!("no session cookie returned"))?;
    Ok(cookie)
}

/// GET /api/auth/me with the stored cookie → the signed-in user, or `None` if the
/// session is gone / invalid.
pub async fn me(cp: &str, cookie: &str) -> Result<Option<Value>> {
    let url = format!("{}/api/auth/me", cp.trim_end_matches('/'));
    let resp = client()?
        .get(&url)
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .map_err(|e| anyhow!("me request: {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(anyhow!(error_message(resp).await));
    }
    let user = resp.json::<Value>().await.map_err(|e| anyhow!("me body: {e}"))?;
    Ok(Some(user))
}

/// POST /api/auth/logout (best-effort).
pub async fn logout(cp: &str, cookie: &str) -> Result<()> {
    let url = format!("{}/api/auth/logout", cp.trim_end_matches('/'));
    let _ = client()?
        .post(&url)
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await;
    Ok(())
}

/// POST /api/auth/resend-verify with the cookie (best-effort).
pub async fn resend_verify(cp: &str, cookie: &str) -> Result<()> {
    let url = format!("{}/api/auth/resend-verify", cp.trim_end_matches('/'));
    let resp = client()?
        .post(&url)
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .map_err(|e| anyhow!("resend request: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!(error_message(resp).await));
    }
    Ok(())
}

/// POST /api/device/new with the cookie → a `ccd_…` device token (shown once).
pub async fn mint_device_token(cp: &str, cookie: &str) -> Result<String> {
    let url = format!("{}/api/device/new", cp.trim_end_matches('/'));
    let resp = client()?
        .post(&url)
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .map_err(|e| anyhow!("device/new request: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!(error_message(resp).await));
    }
    let v = resp.json::<Value>().await.map_err(|e| anyhow!("device/new body: {e}"))?;
    v.get("token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no device token returned"))
}
