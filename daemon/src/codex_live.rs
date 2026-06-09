//! Live per-window token badge for Codex sessions — the Codex counterpart to
//! `jsonl.rs` (which does this for Claude).
//!
//! For each open Codex window we find the newest rollout log under
//! `~/.codex/sessions/**/rollout-*.jsonl` whose internal `cwd` matches the
//! window's cwd, read the latest cumulative `total_token_usage` from its
//! `token_count` events plus the current `model`, and emit a `UsageDelta`. We
//! reuse the `ClaudeMeta` shape as a generic per-window usage payload, so the
//! sidebar renders model / tokens / cost for Codex through the same UI path as
//! Claude (no separate frontend code).

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use cc_console_proto::{ClaudeMeta, ServerMessage, Usage};
use serde_json::Value;
use tokio::time::sleep;

use crate::pricing;
use crate::state::{AppState, JsonlSession};

const POLL: Duration = Duration::from_millis(1000);

/// Index of rollout files → their internal cwd (read once, cached). Codex doesn't
/// put cwd on every line — only on `session_meta` / `turn_context` headers — so we
/// scan the first lines of each file to find it.
#[derive(Default)]
struct RolloutIndex {
    file_cwd: HashMap<PathBuf, Option<String>>,
    file_size_at_read: HashMap<PathBuf, u64>,
}

impl RolloutIndex {
    fn refresh(&mut self, root: &Path) {
        let mut files = Vec::new();
        collect(root, &mut files, 0);
        for p in files {
            let size = fs::metadata(&p).ok().map(|m| m.len()).unwrap_or(0);
            let needs_read = match self.file_cwd.get(&p) {
                None => true,
                Some(Some(_)) => false, // got a cwd; never changes mid-file
                Some(None) => self
                    .file_size_at_read
                    .get(&p)
                    .map(|&prev| size > prev)
                    .unwrap_or(true),
            };
            if needs_read {
                self.file_cwd.insert(p.clone(), extract_cwd(&p));
                self.file_size_at_read.insert(p, size);
            }
        }
    }

    fn newest_for_cwd(&self, cwd: &str) -> Option<PathBuf> {
        self.file_cwd
            .iter()
            .filter_map(|(p, c)| {
                c.as_deref()
                    .filter(|x| *x == cwd)
                    .and_then(|_| fs::metadata(p).ok())
                    .and_then(|m| m.modified().ok())
                    .map(|t| (p.clone(), t))
            })
            .max_by_key(|(_, t)| *t)
            .map(|(p, _)| p)
    }
}

/// Recursively collect `rollout-*.jsonl` (the YYYY/MM/DD tree is shallow; cap depth).
fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 5 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out, depth + 1);
        } else if p.extension().is_some_and(|x| x == "jsonl")
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-"))
        {
            out.push(p);
        }
    }
}

/// First `cwd` found on a `session_meta` / `turn_context` line.
fn extract_cwd(path: &Path) -> Option<String> {
    let f = fs::File::open(path).ok()?;
    let rdr = BufReader::new(f);
    for line in rdr.lines().take(40).flatten() {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let t = v.get("type").and_then(Value::as_str).unwrap_or("");
        if t == "session_meta" || t == "turn_context" {
            if let Some(c) = v
                .get("payload")
                .and_then(|p| p.get("cwd"))
                .and_then(Value::as_str)
            {
                return Some(c.to_string());
            }
        }
    }
    None
}

fn codex_root() -> Option<PathBuf> {
    crate::paths::home_dir().map(|h| h.join(".codex/sessions"))
}

pub async fn run(state: Arc<AppState>) {
    let Some(root) = codex_root() else {
        tracing::error!("$HOME not set — codex live watcher disabled");
        return;
    };
    tracing::info!(dir = %root.display(), "codex live watcher started");
    let mut index = RolloutIndex::default();
    loop {
        sleep(POLL).await;
        if let Err(e) = tick(&state, &root, &mut index).await {
            tracing::warn!(?e, "codex live tick failed");
        }
    }
}

async fn tick(state: &Arc<AppState>, root: &Path, index: &mut RolloutIndex) -> Result<()> {
    // Active Codex windows (cwd → window_id). `agent` is set by classify_agent.
    let codex_cwds: Vec<(String, String)> = {
        let snap = state.windows.read().await;
        snap.values()
            .filter(|w| w.agent == "codex")
            .map(|w| (w.cwd.clone(), w.id.clone()))
            .collect()
    };
    if codex_cwds.is_empty() {
        return Ok(());
    }
    index.refresh(root);

    // Reuse the shared jsonl maps; codex file-path keys never collide with claude's.
    let mut cwd_map = state.cwd_to_jsonl.write().await;
    let mut sessions = state.jsonl.lock().await;
    let mut emit: Vec<(String, ClaudeMeta)> = Vec::new();

    for (cwd, window_id) in codex_cwds {
        if cwd.is_empty() {
            continue;
        }
        let Some(latest) = index.newest_for_cwd(&cwd) else {
            continue;
        };
        let key = latest.to_string_lossy().to_string();
        cwd_map.insert(cwd.clone(), key.clone());
        let entry = sessions.entry(key.clone()).or_insert_with(JsonlSession::default);
        if read_increment(&latest, entry)? {
            if entry.meta.session_id.is_none() {
                entry.meta.session_id = latest
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(String::from);
            }
            entry.meta.estimated_cost = entry
                .meta
                .model
                .as_deref()
                .map(|m| pricing::cost(m, &entry.meta.total_usage))
                .unwrap_or(0.0);
            emit.push((window_id.clone(), entry.meta.clone()));
        }
    }
    drop(sessions);
    drop(cwd_map);

    if !emit.is_empty() {
        let mut snap = state.windows.write().await;
        for (window_id, meta) in &emit {
            if let Some(w) = snap.get_mut(window_id) {
                w.claude = Some(meta.clone());
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

/// Read newly-appended bytes; update the running meta from `turn_context` (model)
/// and `token_count` events. `total_token_usage` is **cumulative** for the session,
/// so we overwrite (not add). Returns true if anything changed.
fn read_increment(path: &Path, entry: &mut JsonlSession) -> Result<bool> {
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len < entry.offset {
        entry.offset = 0;
        entry.meta = ClaudeMeta::default();
    }
    if len == entry.offset {
        return Ok(false);
    }
    f.seek(SeekFrom::Start(entry.offset))?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)?;
    let consumed = match buf.rfind('\n') {
        Some(i) => i + 1,
        None => return Ok(false),
    };
    entry.offset += consumed as u64;

    let mut changed = false;
    for line in buf[..consumed].lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "turn_context" => {
                if let Some(m) = v
                    .get("payload")
                    .and_then(|p| p.get("model"))
                    .and_then(Value::as_str)
                {
                    entry.meta.model = Some(m.to_string());
                    changed = true;
                }
            }
            "event_msg" => {
                let Some(p) = v.get("payload") else { continue };
                if p.get("type").and_then(Value::as_str) != Some("token_count") {
                    continue;
                }
                let Some(total) = p.get("info").and_then(|i| i.get("total_token_usage")) else {
                    continue;
                };
                let g = |k: &str| total.get(k).and_then(Value::as_u64).unwrap_or(0);
                let input_total = g("input_tokens");
                let cached = g("cached_input_tokens").min(input_total);
                entry.meta.total_usage = Usage {
                    input: input_total - cached, // uncached input
                    cache_read: cached,
                    output: g("output_tokens"), // reasoning already included
                    cache_creation: 0,
                };
                changed = true;
            }
            _ => {}
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn reads_model_and_cumulative_total() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("rollout-x.jsonl");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"type":"session_meta","payload":{{"cwd":"/proj","id":"sid1"}}}}"#).unwrap();
        writeln!(f, r#"{{"type":"turn_context","payload":{{"model":"gpt-5.5","cwd":"/proj"}}}}"#).unwrap();
        // first token_count (cumulative)
        writeln!(f, r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}}}}}"#).unwrap();
        // second token_count: cumulative grows — badge should reflect the LATEST, not the sum
        writeln!(f, r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":300,"cached_input_tokens":250,"output_tokens":25,"total_tokens":325}}}}}}}}"#).unwrap();

        let mut entry = JsonlSession::default();
        let changed = read_increment(&p, &mut entry).unwrap();
        assert!(changed);
        assert_eq!(entry.meta.model.as_deref(), Some("gpt-5.5"));
        // latest cumulative: input_total=300, cached=250 → uncached input=50, cache_read=250, output=25
        assert_eq!(entry.meta.total_usage.input, 50);
        assert_eq!(entry.meta.total_usage.cache_read, 250);
        assert_eq!(entry.meta.total_usage.output, 25);
    }

    #[test]
    fn extract_cwd_from_turn_context() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("rollout-y.jsonl");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"type":"response_item","payload":{{"type":"message"}}}}"#).unwrap();
        writeln!(f, r#"{{"type":"turn_context","payload":{{"model":"gpt-5.5","cwd":"/Users/me/app"}}}}"#).unwrap();
        assert_eq!(extract_cwd(&p).as_deref(), Some("/Users/me/app"));
    }
}
