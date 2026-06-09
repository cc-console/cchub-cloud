//! Session manager: scan / read / delete the local session logs that the AI CLIs
//! write — Claude Code (`~/.claude/projects/**/*.jsonl`), Codex
//! (`~/.codex/sessions|archived_sessions/**/*.jsonl`) and Gemini
//! (`~/.gemini/tmp/<project>/chats/*.json`). Backs the settings "History" tab so
//! you can browse past conversations, copy the resume command, reopen a session
//! as a live window, or delete it.
//!
//! Parsing logic adapted from cc-switch's `session_manager` module.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const TITLE_MAX_CHARS: usize = 80;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub provider_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteSessionRequest {
    pub provider_id: String,
    pub session_id: String,
    pub source_path: String,
}

/// Scan all providers concurrently, newest first.
pub fn scan_sessions() -> Vec<SessionMeta> {
    let (c, x, g) = std::thread::scope(|s| {
        let h1 = s.spawn(claude::scan_sessions);
        let h2 = s.spawn(codex::scan_sessions);
        let h3 = s.spawn(gemini::scan_sessions);
        (
            h1.join().unwrap_or_default(),
            h2.join().unwrap_or_default(),
            h3.join().unwrap_or_default(),
        )
    });
    let mut sessions = Vec::new();
    sessions.extend(c);
    sessions.extend(x);
    sessions.extend(g);
    sessions.sort_by(|a, b| {
        let a_ts = a.last_active_at.or(a.created_at).unwrap_or(0);
        let b_ts = b.last_active_at.or(b.created_at).unwrap_or(0);
        b_ts.cmp(&a_ts)
    });
    sessions
}

pub fn load_messages(provider_id: &str, source_path: &str) -> Result<Vec<SessionMessage>, String> {
    let path = Path::new(source_path);
    match provider_id {
        "claude" => claude::load_messages(path),
        "codex" => codex::load_messages(path),
        "gemini" => gemini::load_messages(path),
        _ => Err(format!("Unsupported provider: {provider_id}")),
    }
}

/// Delete a session file, but only if it lives under that provider's known roots
/// (canonicalized) — never delete an arbitrary path the client names.
pub fn delete_session(provider_id: &str, session_id: &str, source_path: &str) -> Result<bool, String> {
    let roots = provider_roots(provider_id)?;
    let source = Path::new(source_path);
    let validated_source = canonicalize_existing(source, "session source")?;

    let mut saw_root = false;
    for root in &roots {
        if !root.exists() {
            continue;
        }
        saw_root = true;
        let validated_root = canonicalize_existing(root, "session root")?;
        if validated_source.starts_with(&validated_root) {
            return match provider_id {
                "claude" => claude::delete_session(&validated_source, session_id),
                "codex" => codex::delete_session(&validated_source, session_id),
                "gemini" => gemini::delete_session(&validated_source, session_id),
                _ => Err(format!("Unsupported provider: {provider_id}")),
            };
        }
    }
    if !saw_root {
        return Err(format!("session root not found for provider {provider_id}"));
    }
    Err(format!("session path is outside provider roots: {}", source.display()))
}

fn provider_roots(provider_id: &str) -> Result<Vec<PathBuf>, String> {
    let home = crate::paths::home_dir().ok_or_else(|| "$HOME not set".to_string())?;
    Ok(match provider_id {
        "claude" => vec![home.join(".claude/projects")],
        "codex" => vec![home.join(".codex/sessions"), home.join(".codex/archived_sessions")],
        "gemini" => vec![home.join(".gemini/tmp")],
        _ => return Err(format!("Unsupported provider: {provider_id}")),
    })
}

fn canonicalize_existing(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.exists() {
        return Err(format!("{label} not found: {}", path.display()));
    }
    path.canonicalize()
        .map_err(|e| format!("failed to resolve {label} {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// shared parsing helpers
// ---------------------------------------------------------------------------

/// First `head_n` and last `tail_n` lines. Small files (<16 KB) are read whole.
fn read_head_tail_lines(path: &Path, head_n: usize, tail_n: usize) -> io::Result<(Vec<String>, Vec<String>)> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len < 16_384 {
        let all: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
        let head = all.iter().take(head_n).cloned().collect();
        let skip = all.len().saturating_sub(tail_n);
        let tail = all.into_iter().skip(skip).collect();
        return Ok((head, tail));
    }
    let head: Vec<String> = BufReader::new(file).lines().take(head_n).map_while(Result::ok).collect();
    let seek_pos = file_len.saturating_sub(16_384);
    let mut file2 = File::open(path)?;
    file2.seek(SeekFrom::Start(seek_pos))?;
    let all_tail: Vec<String> = BufReader::new(file2).lines().map_while(Result::ok).collect();
    let skip_first = if seek_pos > 0 { 1 } else { 0 };
    let usable: Vec<String> = all_tail.into_iter().skip(skip_first).collect();
    let skip = usable.len().saturating_sub(tail_n);
    let tail = usable.into_iter().skip(skip).collect();
    Ok((head, tail))
}

fn parse_timestamp_to_ms(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(if n > 1_000_000_000_000 { n } else { n * 1000 });
    }
    if let Some(n) = value.as_f64() {
        let n = n as i64;
        return Some(if n > 1_000_000_000_000 { n } else { n * 1000 });
    }
    let raw = value.as_str()?;
    DateTime::parse_from_rfc3339(raw).ok().map(|dt: DateTime<FixedOffset>| dt.timestamp_millis())
}

/// Flatten a message `content` (string | array of blocks | object) to display text.
/// `tool_use` blocks render as `[Tool: <name>]`; `tool_result` unwraps its content.
fn extract_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.to_string(),
        Value::Array(items) => items
            .iter()
            .filter_map(extract_text_from_item)
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
        _ => String::new(),
    }
}

fn extract_text_from_item(item: &Value) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    if item_type == "tool_use" {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("unknown");
        return Some(format!("[Tool: {name}]"));
    }
    if item_type == "tool_result" {
        if let Some(content) = item.get("content") {
            let text = extract_text(content);
            if !text.is_empty() {
                return Some(text);
            }
        }
        return None;
    }
    for key in ["text", "input_text", "output_text"] {
        if let Some(text) = item.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    if let Some(content) = item.get("content") {
        let text = extract_text(content);
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

fn truncate_summary(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut result = trimmed.chars().take(max_chars).collect::<String>();
    result.push_str("...");
    result
}

fn path_basename(value: &str) -> Option<String> {
    let normalized = value.trim().trim_end_matches(['/', '\\']);
    normalized
        .split(['/', '\\'])
        .next_back()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn collect_files(root: &Path, ext: &str, files: &mut Vec<PathBuf>) {
    if !root.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, ext, files);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            files.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Claude — ~/.claude/projects/**/*.jsonl
// ---------------------------------------------------------------------------
mod claude {
    use super::*;

    pub fn scan_sessions() -> Vec<SessionMeta> {
        let Some(home) = crate::paths::home_dir() else { return Vec::new() };
        let root = home.join(".claude/projects");
        let mut files = Vec::new();
        collect_files(&root, "jsonl", &mut files);
        files.iter().filter_map(|p| parse_session(p)).collect()
    }

    pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
        let file = File::open(path).map_err(|e| format!("open: {e}"))?;
        let mut messages = Vec::new();
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
            if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let Some(message) = value.get("message") else { continue };
            let mut role = message.get("role").and_then(Value::as_str).unwrap_or("unknown").to_string();
            // Claude nests tool_result inside user messages — reclassify pure ones as "tool".
            if role == "user" {
                if let Some(Value::Array(items)) = message.get("content") {
                    let all_tool = !items.is_empty()
                        && items.iter().all(|i| i.get("type").and_then(Value::as_str) == Some("tool_result"));
                    if all_tool {
                        role = "tool".to_string();
                    }
                }
            }
            let content = message.get("content").map(extract_text).unwrap_or_default();
            if content.trim().is_empty() {
                continue;
            }
            let ts = value.get("timestamp").and_then(parse_timestamp_to_ms);
            messages.push(SessionMessage { role, content, ts });
        }
        Ok(messages)
    }

    pub fn delete_session(path: &Path, session_id: &str) -> Result<bool, String> {
        let meta = parse_session(path).ok_or_else(|| "failed to parse session".to_string())?;
        if meta.session_id != session_id {
            return Err(format!("session id mismatch: expected {session_id}, found {}", meta.session_id));
        }
        // Remove the sibling sidecar dir (same name, no extension) if present.
        if let Some(stem) = path.file_stem() {
            let sidecar = path.parent().unwrap_or_else(|| Path::new("")).join(stem);
            if let Ok(m) = std::fs::metadata(&sidecar) {
                let r = if m.is_dir() { std::fs::remove_dir_all(&sidecar) } else { std::fs::remove_file(&sidecar) };
                r.map_err(|e| format!("delete sidecar: {e}"))?;
            }
        }
        std::fs::remove_file(path).map_err(|e| format!("delete file: {e}"))?;
        Ok(true)
    }

    fn parse_session(path: &Path) -> Option<SessionMeta> {
        // Skip subagent transcripts (agent-*.jsonl).
        if path.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with("agent-")).unwrap_or(false) {
            return None;
        }
        let (head, tail) = read_head_tail_lines(path, 10, 30).ok()?;

        let mut session_id = None;
        let mut project_dir = None;
        let mut created_at = None;
        let mut first_user = None;
        for line in &head {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            if session_id.is_none() {
                session_id = v.get("sessionId").and_then(Value::as_str).map(str::to_string);
            }
            if project_dir.is_none() {
                project_dir = v.get("cwd").and_then(Value::as_str).map(str::to_string);
            }
            if created_at.is_none() {
                created_at = v.get("timestamp").and_then(parse_timestamp_to_ms);
            }
            if first_user.is_none() {
                let is_user = v.get("type").and_then(Value::as_str) == Some("user")
                    || v.get("message").and_then(|m| m.get("role")).and_then(Value::as_str) == Some("user");
                if is_user {
                    if let Some(message) = v.get("message") {
                        let text = message.get("content").map(extract_text).unwrap_or_default();
                        let trimmed = text.trim();
                        if !trimmed.is_empty()
                            && !trimmed.contains("<local-command-caveat>")
                            && !trimmed.starts_with("<command-name>")
                        {
                            first_user = Some(trimmed.to_string());
                        }
                    }
                }
            }
            if session_id.is_some() && project_dir.is_some() && created_at.is_some() && first_user.is_some() {
                break;
            }
        }

        let mut last_active_at = None;
        let mut summary: Option<String> = None;
        let mut custom_title = None;
        for line in tail.iter().rev() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            if last_active_at.is_none() {
                last_active_at = v.get("timestamp").and_then(parse_timestamp_to_ms);
            }
            if custom_title.is_none() && v.get("type").and_then(Value::as_str) == Some("custom-title") {
                custom_title = v
                    .get("customTitle")
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
            }
            if summary.is_none() && v.get("isMeta").and_then(Value::as_bool) != Some(true) {
                if let Some(message) = v.get("message") {
                    let text = message.get("content").map(extract_text).unwrap_or_default();
                    if !text.trim().is_empty() {
                        summary = Some(text);
                    }
                }
            }
            if last_active_at.is_some() && summary.is_some() && custom_title.is_some() {
                break;
            }
        }

        let session_id = session_id
            .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_string))?;
        let title = custom_title
            .map(|t| truncate_summary(&t, TITLE_MAX_CHARS))
            .or_else(|| first_user.map(|t| truncate_summary(&t, TITLE_MAX_CHARS)))
            .or_else(|| project_dir.as_deref().and_then(path_basename));

        Some(SessionMeta {
            provider_id: "claude".into(),
            session_id: session_id.clone(),
            title,
            summary: summary.map(|t| truncate_summary(&t, 160)),
            project_dir,
            created_at,
            last_active_at,
            source_path: Some(path.to_string_lossy().to_string()),
            resume_command: Some(format!("claude --resume {session_id}")),
        })
    }
}

// ---------------------------------------------------------------------------
// Codex — ~/.codex/sessions + archived_sessions/**/*.jsonl
// ---------------------------------------------------------------------------
mod codex {
    use super::*;

    pub fn scan_sessions() -> Vec<SessionMeta> {
        let Some(home) = crate::paths::home_dir() else { return Vec::new() };
        let mut files = Vec::new();
        for sub in [".codex/sessions", ".codex/archived_sessions"] {
            collect_files(&home.join(sub), "jsonl", &mut files);
        }
        files.iter().filter_map(|p| parse_session(p)).collect()
    }

    pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
        let file = File::open(path).map_err(|e| format!("open: {e}"))?;
        let mut messages = Vec::new();
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v.get("type").and_then(Value::as_str) != Some("response_item") {
                continue;
            }
            let Some(payload) = v.get("payload") else { continue };
            let (role, content) = match payload.get("type").and_then(Value::as_str).unwrap_or("") {
                "message" => {
                    let role = payload.get("role").and_then(Value::as_str).unwrap_or("unknown").to_string();
                    (role, payload.get("content").map(extract_text).unwrap_or_default())
                }
                "function_call" => {
                    let name = payload.get("name").and_then(Value::as_str).unwrap_or("unknown");
                    ("assistant".to_string(), format!("[Tool: {name}]"))
                }
                "function_call_output" => (
                    "tool".to_string(),
                    payload.get("output").and_then(Value::as_str).unwrap_or("").to_string(),
                ),
                _ => continue,
            };
            if content.trim().is_empty() {
                continue;
            }
            let ts = v.get("timestamp").and_then(parse_timestamp_to_ms);
            messages.push(SessionMessage { role, content, ts });
        }
        Ok(messages)
    }

    pub fn delete_session(path: &Path, session_id: &str) -> Result<bool, String> {
        let meta = parse_session(path).ok_or_else(|| "failed to parse session".to_string())?;
        if meta.session_id != session_id {
            return Err(format!("session id mismatch: expected {session_id}, found {}", meta.session_id));
        }
        std::fs::remove_file(path).map_err(|e| format!("delete file: {e}"))?;
        Ok(true)
    }

    fn parse_session(path: &Path) -> Option<SessionMeta> {
        let (head, tail) = read_head_tail_lines(path, 10, 30).ok()?;
        let mut session_id = None;
        let mut project_dir = None;
        let mut created_at = None;
        let mut first_user = None;
        for line in &head {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            if created_at.is_none() {
                created_at = v.get("timestamp").and_then(parse_timestamp_to_ms);
            }
            if v.get("type").and_then(Value::as_str) == Some("session_meta") {
                if let Some(payload) = v.get("payload") {
                    // Skip subagent rollouts.
                    if payload.get("source").and_then(Value::as_object).map(|s| s.contains_key("subagent")).unwrap_or(false) {
                        return None;
                    }
                    if session_id.is_none() {
                        session_id = payload.get("id").and_then(Value::as_str).map(str::to_string);
                    }
                    if project_dir.is_none() {
                        project_dir = payload.get("cwd").and_then(Value::as_str).map(str::to_string);
                    }
                    if let Some(ts) = payload.get("timestamp").and_then(parse_timestamp_to_ms) {
                        created_at.get_or_insert(ts);
                    }
                }
            }
            if first_user.is_none() && v.get("type").and_then(Value::as_str) == Some("response_item") {
                if let Some(payload) = v.get("payload") {
                    if payload.get("type").and_then(Value::as_str) == Some("message")
                        && payload.get("role").and_then(Value::as_str) == Some("user")
                    {
                        let text = payload.get("content").map(extract_text).unwrap_or_default();
                        let trimmed = text.trim();
                        if !trimmed.is_empty()
                            && !trimmed.starts_with("# AGENTS.md")
                            && !trimmed.starts_with("<environment_context>")
                        {
                            first_user = Some(trimmed.to_string());
                        }
                    }
                }
            }
            if session_id.is_some() && project_dir.is_some() && created_at.is_some() && first_user.is_some() {
                break;
            }
        }

        let mut last_active_at = None;
        let mut summary: Option<String> = None;
        for line in tail.iter().rev() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            if last_active_at.is_none() {
                last_active_at = v.get("timestamp").and_then(parse_timestamp_to_ms);
            }
            if summary.is_none() && v.get("type").and_then(Value::as_str) == Some("response_item") {
                if let Some(payload) = v.get("payload") {
                    if payload.get("type").and_then(Value::as_str) == Some("message") {
                        let text = payload.get("content").map(extract_text).unwrap_or_default();
                        if !text.trim().is_empty() {
                            summary = Some(text);
                        }
                    }
                }
            }
            if last_active_at.is_some() && summary.is_some() {
                break;
            }
        }

        let session_id = session_id.or_else(|| uuid_from_filename(path))?;
        let title = first_user
            .map(|t| truncate_summary(&t, TITLE_MAX_CHARS))
            .or_else(|| project_dir.as_deref().and_then(path_basename));

        Some(SessionMeta {
            provider_id: "codex".into(),
            session_id: session_id.clone(),
            title,
            summary: summary.map(|t| truncate_summary(&t, 160)),
            project_dir,
            created_at,
            last_active_at,
            source_path: Some(path.to_string_lossy().to_string()),
            resume_command: Some(format!("codex resume {session_id}")),
        })
    }

    /// Find a `8-4-4-4-12` hex UUID inside the filename (no regex dep).
    fn uuid_from_filename(path: &Path) -> Option<String> {
        let name = path.file_name()?.to_str()?;
        let bytes = name.as_bytes();
        let seg = [8usize, 4, 4, 4, 12];
        let total: usize = seg.iter().sum::<usize>() + 4; // + dashes
        if bytes.len() < total {
            return None;
        }
        for start in 0..=bytes.len() - total {
            let cand = &name[start..start + total];
            if is_uuid(cand) {
                return Some(cand.to_string());
            }
        }
        None
    }

    fn is_uuid(s: &str) -> bool {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 5 {
            return false;
        }
        let lens = [8, 4, 4, 4, 12];
        parts.iter().zip(lens).all(|(p, l)| p.len() == l && p.bytes().all(|b| b.is_ascii_hexdigit()))
    }
}

// ---------------------------------------------------------------------------
// Gemini — ~/.gemini/tmp/<project>/chats/*.json (whole-file JSON)
// ---------------------------------------------------------------------------
mod gemini {
    use super::*;

    pub fn scan_sessions() -> Vec<SessionMeta> {
        let Some(home) = crate::paths::home_dir() else { return Vec::new() };
        let tmp = home.join(".gemini/tmp");
        if !tmp.exists() {
            return Vec::new();
        }
        let Ok(projects) = std::fs::read_dir(&tmp) else { return Vec::new() };
        let mut sessions = Vec::new();
        for project in projects.flatten() {
            let chats = project.path().join("chats");
            if !chats.is_dir() {
                continue;
            }
            let project_dir = std::fs::read_to_string(project.path().join(".project_root"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let Ok(files) = std::fs::read_dir(&chats) else { continue };
            for f in files.flatten() {
                let path = f.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Some(meta) = parse_session(&path) {
                    sessions.push(SessionMeta { project_dir: project_dir.clone(), ..meta });
                }
            }
        }
        sessions
    }

    pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
        let data = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
        let value: Value = serde_json::from_str(&data).map_err(|e| format!("parse: {e}"))?;
        let msgs = value.get("messages").and_then(Value::as_array).ok_or("no messages array")?;
        let mut result = Vec::new();
        for msg in msgs {
            let role = match msg.get("type").and_then(Value::as_str) {
                Some("gemini") => "assistant",
                Some("user") => "user",
                _ => continue,
            };
            let mut content = match msg.get("content") {
                Some(Value::String(s)) => s.to_string(),
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|i| i.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            if let Some(Value::Array(calls)) = msg.get("toolCalls") {
                for call in calls {
                    if let Some(name) = call.get("name").and_then(Value::as_str) {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&format!("[Tool: {name}]"));
                    }
                }
            }
            if content.trim().is_empty() {
                continue;
            }
            let ts = msg.get("timestamp").and_then(parse_timestamp_to_ms);
            result.push(SessionMessage { role: role.into(), content, ts });
        }
        Ok(result)
    }

    pub fn delete_session(path: &Path, session_id: &str) -> Result<bool, String> {
        let meta = parse_session(path).ok_or_else(|| "failed to parse session".to_string())?;
        if meta.session_id != session_id {
            return Err(format!("session id mismatch: expected {session_id}, found {}", meta.session_id));
        }
        std::fs::remove_file(path).map_err(|e| format!("delete file: {e}"))?;
        Ok(true)
    }

    fn parse_session(path: &Path) -> Option<SessionMeta> {
        let data = std::fs::read_to_string(path).ok()?;
        let value: Value = serde_json::from_str(&data).ok()?;
        let session_id = value.get("sessionId").and_then(Value::as_str)?.to_string();
        let created_at = value.get("startTime").and_then(parse_timestamp_to_ms);
        let last_active_at = value.get("lastUpdated").and_then(parse_timestamp_to_ms);
        let title = value
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|msgs| {
                msgs.iter()
                    .find(|m| m.get("type").and_then(Value::as_str) == Some("user"))
                    .and_then(|m| m.get("content").and_then(Value::as_str))
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| truncate_summary(s, TITLE_MAX_CHARS))
            });
        Some(SessionMeta {
            provider_id: "gemini".into(),
            session_id: session_id.clone(),
            title: title.clone(),
            summary: title,
            project_dir: None,
            created_at,
            last_active_at: last_active_at.or(created_at),
            source_path: Some(path.to_string_lossy().to_string()),
            resume_command: Some(format!("gemini --resume {session_id}")),
        })
    }
}
