//! Live per-window token badge for Gemini CLI sessions — the Gemini counterpart
//! to `jsonl.rs` (Claude) and `codex_live.rs` (Codex).
//!
//! Gemini stores each chat as a single rewritten JSON file at
//! `~/.gemini/tmp/<slug>/chats/session-*.json`, where `<slug>` is the project
//! name mapped from the absolute cwd in `~/.gemini/projects.json`. Each assistant
//! (`type: "gemini"`) message carries `model` + a per-message `tokens`
//! `{input, output, cached, thoughts, tool, total}`. Unlike Codex's cumulative
//! totals, these are per-message, so we **sum** across the session.
//!
//! The file is rewritten wholesale (not appended), so we re-parse on mtime change
//! rather than reading byte offsets, and reuse the `ClaudeMeta` shape as a generic
//! per-window usage payload (same sidebar UI path as Claude/Codex).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use cc_console_proto::{ClaudeMeta, ServerMessage, Usage};
use serde_json::Value;
use tokio::time::sleep;

use crate::pricing;
use crate::state::AppState;

const POLL: Duration = Duration::from_millis(1000);

#[derive(Default)]
struct GeminiWatcher {
    /// Parsed result cache: session file → (mtime_ns, meta). Avoids re-parsing an
    /// unchanged file.
    cache: HashMap<PathBuf, (u128, ClaudeMeta)>,
    /// Per-window last emit (file, mtime_ns), to skip redundant UsageDelta sends.
    emitted: HashMap<String, (PathBuf, u128)>,
}

pub async fn run(state: Arc<AppState>) {
    let Some(home) = crate::paths::home_dir() else {
        tracing::error!("home dir not found — gemini live watcher disabled");
        return;
    };
    let gemini_root = home.join(".gemini");
    tracing::info!(dir = %gemini_root.display(), "gemini live watcher started");
    let mut w = GeminiWatcher::default();
    loop {
        sleep(POLL).await;
        if let Err(e) = tick(&state, &gemini_root, &mut w).await {
            tracing::warn!(?e, "gemini live tick failed");
        }
    }
}

async fn tick(state: &Arc<AppState>, gemini_root: &Path, w: &mut GeminiWatcher) -> Result<()> {
    let gemini_cwds: Vec<(String, String)> = {
        let snap = state.windows.read().await;
        snap.values()
            .filter(|win| win.agent == "gemini")
            .map(|win| (win.cwd.clone(), win.id.clone()))
            .collect()
    };
    if gemini_cwds.is_empty() {
        return Ok(());
    }

    let slugs = load_projects(gemini_root); // cwd → slug
    let mut emit: Vec<(String, ClaudeMeta)> = Vec::new();

    for (cwd, window_id) in gemini_cwds {
        let Some(slug) = slugs.get(&cwd) else { continue };
        let chats = gemini_root.join("tmp").join(slug).join("chats");
        let Some((path, mtime)) = newest_session(&chats) else {
            continue;
        };

        // Skip if we already emitted this exact (file, mtime) for this window.
        if w.emitted.get(&window_id) == Some(&(path.clone(), mtime)) {
            continue;
        }

        // Reuse cached parse if the file hasn't changed; else parse and cache.
        let meta = match w.cache.get(&path) {
            Some((m, meta)) if *m == mtime => meta.clone(),
            _ => {
                let Some(meta) = parse_session(&path) else { continue };
                w.cache.insert(path.clone(), (mtime, meta.clone()));
                meta
            }
        };

        w.emitted.insert(window_id.clone(), (path, mtime));
        emit.push((window_id, meta));
    }

    if !emit.is_empty() {
        let mut snap = state.windows.write().await;
        for (window_id, meta) in &emit {
            if let Some(win) = snap.get_mut(window_id) {
                win.claude = Some(meta.clone());
            }
        }
        drop(snap);
        for (window_id, claude) in emit {
            let _ = state
                .event_tx
                .send(ServerMessage::UsageDelta { window_id, claude });
        }
    }
    Ok(())
}

/// Read `~/.gemini/projects.json` → map of absolute cwd → project slug.
fn load_projects(gemini_root: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(s) = fs::read_to_string(gemini_root.join("projects.json")) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<Value>(&s) else {
        return out;
    };
    if let Some(obj) = v.get("projects").and_then(Value::as_object) {
        for (cwd, slug) in obj {
            if let Some(slug) = slug.as_str() {
                out.insert(cwd.clone(), slug.to_string());
            }
        }
    }
    out
}

/// Newest `session-*.json` in `chats/` with its mtime in nanoseconds.
fn newest_session(chats: &Path) -> Option<(PathBuf, u128)> {
    let entries = fs::read_dir(chats).ok()?;
    entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?;
            if !name.starts_with("session-") || p.extension().and_then(|x| x.to_str()) != Some("json")
            {
                return None;
            }
            let mtime = fs::metadata(&p)
                .ok()?
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_nanos();
            Some((p, mtime))
        })
        .max_by_key(|(_, m)| *m)
}

/// Parse a Gemini session file: sum per-message token usage across the session and
/// take the latest model. Returns None if the file can't be read/parsed.
fn parse_session(path: &Path) -> Option<ClaudeMeta> {
    let s = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&s).ok()?;
    let msgs = v.get("messages").and_then(Value::as_array)?;

    let mut usage = Usage::default();
    let mut model: Option<String> = None;
    for m in msgs {
        let Some(t) = m.get("tokens") else { continue };
        let g = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
        let input = g("input");
        let cached = g("cached").min(input);
        usage.input += input - cached; // uncached prompt tokens
        usage.cache_read += cached;
        usage.output += g("output") + g("thoughts") + g("tool"); // fold reasoning/tool in
        if let Some(mo) = m.get("model").and_then(Value::as_str) {
            model = Some(mo.to_string());
        }
    }
    if usage.input == 0 && usage.output == 0 && usage.cache_read == 0 {
        return None;
    }
    let estimated_cost = model
        .as_deref()
        .map(|mo| pricing::cost(mo, &usage))
        .unwrap_or(0.0);
    let session_id = v
        .get("sessionId")
        .and_then(Value::as_str)
        .map(String::from);

    Some(ClaudeMeta {
        session_id,
        model,
        total_usage: usage,
        estimated_cost,
        last_message_preview: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn sums_per_message_tokens_and_takes_latest_model() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("session-x.json");
        let mut f = fs::File::create(&p).unwrap();
        let body = r#"{
          "sessionId":"s1",
          "messages":[
            {"type":"user","content":"hi"},
            {"type":"gemini","model":"gemini-3-flash-preview","tokens":{"input":100,"output":10,"cached":20,"thoughts":5,"tool":0,"total":115}},
            {"type":"gemini","model":"gemini-3-pro","tokens":{"input":200,"output":30,"cached":0,"thoughts":0,"tool":0,"total":230}}
          ]
        }"#;
        f.write_all(body.as_bytes()).unwrap();

        let meta = parse_session(&p).unwrap();
        assert_eq!(meta.model.as_deref(), Some("gemini-3-pro"));
        // input: (100-20) + (200-0) = 280 ; cache_read: 20 ; output: (10+5+0)+(30+0+0)=45
        assert_eq!(meta.total_usage.input, 280);
        assert_eq!(meta.total_usage.cache_read, 20);
        assert_eq!(meta.total_usage.output, 45);
        assert_eq!(meta.session_id.as_deref(), Some("s1"));
    }
}
