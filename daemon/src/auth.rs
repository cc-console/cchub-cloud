//! Token gate for the daemon.
//!
//! A single shared secret protects every route (static UI, `/ws`, `/api/*`). A client proves
//! it knows the token by either `?token=…` in the URL or a `cc_token` cookie. On a valid
//! `?token=` the guard sets the cookie, so the phone visits `https://host/?token=…` once and
//! every later request (page refresh, WebSocket handshake, API fetch — all same-origin, cookies
//! sent automatically) is authorised without the token in the URL.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::state::AppState;

const COOKIE_NAME: &str = "cc_token";

/// Decide the effective token. Precedence:
/// 1. explicit `--token`/env (empty or "none"/"off" disables auth),
/// 2. else if bound to a non-loopback address: load-or-generate a persisted token,
/// 3. else (loopback dev): no auth.
pub fn resolve_token(arg: Option<String>, local_only: bool) -> Option<String> {
    if let Some(raw) = arg {
        let t = raw.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("off") {
            return None;
        }
        return Some(t.to_string());
    }
    if local_only {
        return None;
    }
    Some(load_or_generate())
}

fn token_path() -> Option<PathBuf> {
    crate::paths::home_dir().map(|h| h.join(".claude/cc-console/token"))
}

/// Load the persisted token or generate+persist a fresh one. Public within the
/// crate so the tunnel control path can enable auth on demand (when a tunnel goes
/// up, the daemon becomes publicly reachable and must require a token).
pub(crate) fn load_or_generate() -> String {
    if let Some(path) = token_path() {
        if let Ok(s) = fs::read_to_string(&path) {
            let t = s.trim().to_string();
            if !t.is_empty() {
                return t;
            }
        }
        let tok = random_hex();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if fs::write(&path, &tok).is_ok() {
            // best-effort 0600
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            }
            tracing::info!(path = %path.display(), "generated access token");
        }
        return tok;
    }
    random_hex()
}

/// 16 random bytes as hex, read from the OS CSPRNG (`/dev/urandom`). Falls back to a
/// time-seeded value only if that read fails (shouldn't on macOS/Linux).
fn random_hex() -> String {
    let mut buf = [0u8; 16];
    if fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .is_ok()
    {
        return buf.iter().map(|b| format!("{b:02x}")).collect();
    }
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:032x}")
}

/// Axum middleware: allow the request through iff it presents the token (query or cookie),
/// otherwise 401. Sets the cookie when the token arrives via `?token=`.
pub async fn guard(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    // Read the current token (interior-mutable: a tunnel can enable auth at runtime).
    let Some(expected) = state.auth_token.read().await.clone() else {
        return next.run(req).await;
    };

    let from_query = query_token(req.uri().query()).is_some_and(|t| t == expected);
    let from_cookie = cookie_token(&req).is_some_and(|t| t == expected);

    if !from_query && !from_cookie {
        return (
            StatusCode::UNAUTHORIZED,
            "401 unauthorized — open this site with ?token=YOUR_TOKEN once.\n",
        )
            .into_response();
    }

    let mut resp = next.run(req).await;
    if from_query {
        // Persist the token so future requests (and the WS handshake) carry it automatically.
        let cookie = format!("{COOKIE_NAME}={expected}; Path=/; Max-Age=31536000; SameSite=Lax; HttpOnly");
        if let Ok(v) = header::HeaderValue::from_str(&cookie) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    resp
}

fn query_token(query: Option<&str>) -> Option<String> {
    let q = query?;
    for pair in q.split('&') {
        if let Some(v) = pair.strip_prefix("token=") {
            return Some(urldecode(v));
        }
    }
    None
}

fn cookie_token(req: &Request) -> Option<String> {
    let raw = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for c in raw.split(';') {
        let c = c.trim();
        if let Some(v) = c.strip_prefix(&format!("{COOKIE_NAME}=")) {
            return Some(v.to_string());
        }
    }
    None
}

/// Minimal percent-decoding (enough for tokens, which are hex but may be pasted with %XX).
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
