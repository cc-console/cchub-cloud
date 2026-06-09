//! JSONL incremental scanner for `~/.claude/projects/<encoded-cwd>/<session>.jsonl`.
//!
//! Strategy: every poll tick we find, for each known claude window's cwd, the
//! newest .jsonl file *whose internal `"cwd"` field matches that window's cwd*,
//! then read only the new bytes since last tick. usage gets deduped by
//! requestId so streamed multi-message turns don't double-count.
//!
//! We **don't** rely on Claude's `cwd → encoded-dir-name` convention here —
//! that breaks the moment the cwd contains anything unusual (symlinks,
//! `/private` prefix, moved dirs, etc.). Instead we read the `cwd` field
//! stored inside each jsonl file (Claude writes it on every line), cached
//! so we only parse the first line of each file once.

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

/// In-memory index of every `~/.claude/projects/*/*.jsonl` → its internal cwd
/// (the absolute path Claude was launched in). Built once per file then cached;
/// only the directory listing is re-walked each tick.
#[derive(Default)]
struct ProjectIndex {
    /// jsonl path → cwd string read from the file. `None` if we tried and the
    /// file had no readable cwd line yet (will retry on size change).
    file_cwd: HashMap<PathBuf, Option<String>>,
    /// Size at which we last tried to read cwd; if file grew, retry (claude
    /// sometimes writes a small bootstrap line then later real lines).
    file_size_at_read: HashMap<PathBuf, u64>,
}

impl ProjectIndex {
    /// Walk projects root, discover jsonl files, read cwd for new ones.
    fn refresh(&mut self, projects_root: &Path) {
        let Ok(subs) = fs::read_dir(projects_root) else { return };
        for sub in subs.flatten() {
            let subdir = sub.path();
            if !subdir.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&subdir) else { continue };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let size = f.metadata().ok().map(|m| m.len()).unwrap_or(0);
                let needs_read = match self.file_cwd.get(&p) {
                    None => true,
                    // Already got a cwd → never need to re-read (cwd doesn't change mid-file).
                    Some(Some(_)) => false,
                    // Tried but failed; retry only if file grew since.
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
    }

    /// Newest jsonl (by mtime) whose internal cwd matches `cwd`.
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

/// Read the first few lines of a jsonl, return the first `"cwd"` value found.
fn extract_cwd(path: &Path) -> Option<String> {
    let f = fs::File::open(path).ok()?;
    let rdr = BufReader::new(f);
    for line in rdr.lines().take(20).flatten() {
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
                return Some(cwd.to_string());
            }
        }
    }
    None
}

pub async fn run(state: Arc<AppState>) {
    let Some(projects_dir) = projects_root() else {
        tracing::error!("$HOME not set — JSONL watcher disabled");
        return;
    };
    tracing::info!(dir = %projects_dir.display(), "JSONL watcher started");

    let mut index = ProjectIndex::default();

    loop {
        sleep(POLL).await;
        if let Err(e) = tick(&state, &projects_dir, &mut index).await {
            tracing::warn!(?e, "jsonl tick failed");
        }
    }
}

fn projects_root() -> Option<PathBuf> {
    crate::paths::home_dir().map(|h| h.join(".claude/projects"))
}

async fn tick(
    state: &Arc<AppState>,
    projects_dir: &PathBuf,
    index: &mut ProjectIndex,
) -> Result<()> {
    // Gather active claude windows (cwd → window_id).
    let claude_cwds: Vec<(String, String)> = {
        let snap = state.windows.read().await;
        snap.values()
            // Match on the classified agent (set from start_command too), not the
            // live process name: Claude Code installed via npm runs as `node`, so
            // `is_claude()` (current_command-based) would miss it and never attach
            // usage. codex/gemini already filter this way.
            .filter(|w| w.agent == "claude")
            .map(|w| (w.cwd.clone(), w.id.clone()))
            .collect()
    };

    if claude_cwds.is_empty() {
        return Ok(());
    }

    index.refresh(projects_dir);

    let mut cwd_jsonl_map = state.cwd_to_jsonl.write().await;
    let mut jsonl_state = state.jsonl.lock().await;
    let mut emit: Vec<(String, ClaudeMeta)> = Vec::new();

    for (cwd, window_id) in claude_cwds {
        let Some(latest) = index.newest_for_cwd(&cwd) else { continue };
        let jsonl_path = latest.to_string_lossy().to_string();
        cwd_jsonl_map.insert(cwd.clone(), jsonl_path.clone());

        let entry = jsonl_state
            .entry(jsonl_path.clone())
            .or_insert_with(JsonlSession::default);

        let changed = read_increment(&latest, entry)?;
        if changed {
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

    drop(jsonl_state);
    drop(cwd_jsonl_map);

    // Push UsageDelta + update windows snapshot's `.claude` field.
    if !emit.is_empty() {
        let mut snap = state.windows.write().await;
        for (window_id, meta) in &emit {
            if let Some(w) = snap.get_mut(window_id) {
                w.claude = Some(meta.clone());
            }
        }
        drop(snap);

        for (window_id, claude) in emit {
            let _ = state.event_tx.send(ServerMessage::UsageDelta {
                window_id,
                claude,
            });
        }
    }
    Ok(())
}

/// Read newly-appended bytes since `entry.offset`, parse line by line.
/// Returns true if anything changed.
fn read_increment(path: &PathBuf, entry: &mut JsonlSession) -> Result<bool> {
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    // File rotated / truncated (rare for Claude Code, but defensive): reset.
    if len < entry.offset {
        entry.offset = 0;
        entry.seen_request_ids.clear();
        entry.meta = ClaudeMeta {
            session_id: None,
            model: None,
            total_usage: Usage::default(),
            estimated_cost: 0.0,
            last_message_preview: None,
        };
    }
    if len == entry.offset {
        return Ok(false);
    }
    f.seek(SeekFrom::Start(entry.offset))?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)?;
    entry.offset = len;

    let mut any = false;
    for line in buf.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        any |= consume_line(&v, entry);
    }
    Ok(any)
}

fn consume_line(v: &Value, entry: &mut JsonlSession) -> bool {
    let msg = v.get("message").cloned().unwrap_or(Value::Null);
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
    if role != "assistant" {
        return false;
    }
    let request_id = v
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !request_id.is_empty() && !entry.seen_request_ids.insert(request_id) {
        return false;
    }

    let usage = msg.get("usage").cloned().unwrap_or(Value::Null);
    let input = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cache_read = usage.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cache_creation = usage.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0);

    entry.meta.total_usage.input += input;
    entry.meta.total_usage.output += output;
    entry.meta.total_usage.cache_read += cache_read;
    entry.meta.total_usage.cache_creation += cache_creation;

    if let Some(m) = msg.get("model").and_then(Value::as_str) {
        entry.meta.model = Some(m.to_string());
    }

    // Preview: first text chunk of this message, if any.
    if let Some(content) = msg.get("content").and_then(Value::as_array) {
        for c in content {
            if c.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = c.get("text").and_then(Value::as_str) {
                    let trimmed: String = t.chars().take(100).collect();
                    entry.meta.last_message_preview = Some(trimmed);
                    break;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn extracts_cwd_from_first_line() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"type":"user","cwd":"/Users/me/proj a","message":{{"role":"user"}}}}"#).unwrap();
        writeln!(f, r#"{{"type":"assistant","cwd":"/Users/me/proj a","message":{{"role":"assistant"}}}}"#).unwrap();
        assert_eq!(extract_cwd(&p).as_deref(), Some("/Users/me/proj a"));
    }

    #[test]
    fn index_matches_arbitrary_cwd() {
        // The whole point: cwd with characters that Claude's old encoding rule
        // would mangle (spaces, underscores, moved dirs, symlinks) still match.
        let dir = tempdir().unwrap();
        let sub = dir.path().join("anything-doesnt-matter");
        fs::create_dir(&sub).unwrap();
        let p = sub.join("session.jsonl");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"cwd":"/Users/me/my project_v2","message":{{"role":"user"}}}}"#).unwrap();

        let mut idx = ProjectIndex::default();
        idx.refresh(dir.path());
        assert_eq!(idx.newest_for_cwd("/Users/me/my project_v2"), Some(p));
        assert_eq!(idx.newest_for_cwd("/Users/me/other"), None);
    }

    #[test]
    fn index_picks_newest_by_mtime() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("p");
        fs::create_dir(&sub).unwrap();
        let older = sub.join("a.jsonl");
        let newer = sub.join("b.jsonl");
        fs::write(&older, r#"{"cwd":"/x","message":{"role":"user"}}"#).unwrap();
        // Ensure mtime ordering is observable even on coarse FS clocks.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&newer, r#"{"cwd":"/x","message":{"role":"user"}}"#).unwrap();

        let mut idx = ProjectIndex::default();
        idx.refresh(dir.path());
        assert_eq!(idx.newest_for_cwd("/x"), Some(newer));
    }
}
