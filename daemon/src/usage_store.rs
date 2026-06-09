//! Persistent token-usage store + full-history scanner.
//!
//! Unlike the live JSONL watcher (which only follows currently-open claude windows for the
//! real-time badge), this module scans **every** `~/.claude/projects/<dir>/*.jsonl` file and
//! persists one row per assistant message into SQLite, keyed by the line's globally-unique
//! `uuid` (and deduped by `requestId` too). That gives true historical "tokens over a time
//! range" stats across every session ever, surviving daemon restarts.
//!
//! rusqlite is blocking, so the scanner runs on a dedicated OS thread and `/api/stats`
//! queries go through `spawn_blocking`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use cc_console_proto::Usage;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;

use crate::pricing;

const SCAN_INTERVAL: Duration = Duration::from_secs(3);

/// One persisted assistant-message usage row.
struct UsageEvent {
    uuid: String,
    request_id: String,
    ts_ms: i64,
    session_id: String,
    cwd: String,
    model: String,
    /// "claude" | "codex" — which agent produced this usage.
    provider: String,
    usage: Usage,
    cost: f64,
}

pub struct UsageStore {
    conn: Mutex<Connection>,
}

impl UsageStore {
    /// Open (creating if needed) the SQLite db at `path`, ensuring schema.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open usage db {}", path.display()))?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS usage_events (
                uuid           TEXT PRIMARY KEY,
                request_id     TEXT NOT NULL DEFAULT '',
                ts_ms          INTEGER NOT NULL,
                session_id     TEXT NOT NULL DEFAULT '',
                cwd            TEXT NOT NULL DEFAULT '',
                model          TEXT NOT NULL DEFAULT '',
                provider       TEXT NOT NULL DEFAULT 'claude',
                input          INTEGER NOT NULL DEFAULT 0,
                output         INTEGER NOT NULL DEFAULT 0,
                cache_read     INTEGER NOT NULL DEFAULT 0,
                cache_creation INTEGER NOT NULL DEFAULT 0,
                cost           REAL NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_usage_ts ON usage_events(ts_ms);
            -- Dedup streamed turns that share a requestId (mirrors the live watcher).
            CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_req
                ON usage_events(request_id) WHERE request_id <> '';
            -- Per-file read offsets so each scan only parses appended bytes.
            CREATE TABLE IF NOT EXISTS scan_offsets (
                path   TEXT PRIMARY KEY,
                offset INTEGER NOT NULL
            );
            -- Per-codex-session metadata (cwd + latest model), so token_count events in later
            -- appended chunks (read past the session_meta/turn_context header) still get tagged.
            CREATE TABLE IF NOT EXISTS codex_sessions (
                path  TEXT PRIMARY KEY,
                cwd   TEXT NOT NULL DEFAULT '',
                model TEXT NOT NULL DEFAULT '',
                sid   TEXT NOT NULL DEFAULT ''
            );
            "#,
        )
        .context("failed to init usage schema")?;
        // Migration for pre-existing dbs created before the provider column existed.
        let _ = conn.execute(
            "ALTER TABLE usage_events ADD COLUMN provider TEXT NOT NULL DEFAULT 'claude'",
            [],
        );
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn get_offset(conn: &Connection, path: &str) -> u64 {
        conn.query_row(
            "SELECT offset FROM scan_offsets WHERE path = ?1",
            [path],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n.max(0) as u64)
        .unwrap_or(0)
    }

    fn set_offset(conn: &Connection, path: &str, offset: u64) -> Result<()> {
        conn.execute(
            "INSERT INTO scan_offsets(path, offset) VALUES(?1, ?2)
             ON CONFLICT(path) DO UPDATE SET offset = excluded.offset",
            rusqlite::params![path, offset as i64],
        )?;
        Ok(())
    }

    fn insert_events(conn: &mut Connection, events: &[UsageEvent]) -> Result<usize> {
        if events.is_empty() {
            return Ok(0);
        }
        let tx = conn.transaction()?;
        let mut n = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO usage_events
                 (uuid, request_id, ts_ms, session_id, cwd, model, provider,
                  input, output, cache_read, cache_creation, cost)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            )?;
            for e in events {
                n += stmt.execute(rusqlite::params![
                    e.uuid,
                    e.request_id,
                    e.ts_ms,
                    e.session_id,
                    e.cwd,
                    e.model,
                    e.provider,
                    e.usage.input as i64,
                    e.usage.output as i64,
                    e.usage.cache_read as i64,
                    e.usage.cache_creation as i64,
                    e.cost,
                ])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Aggregate usage in `[from_ms, to_ms)` into totals + by-day(local) + by-model + by-project.
    pub fn query(&self, from_ms: i64, to_ms: i64) -> Result<StatsResult> {
        use chrono::{Local, TimeZone};
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ts_ms, model, cwd, provider, input, output, cache_read, cache_creation
             FROM usage_events WHERE ts_ms >= ?1 AND ts_ms < ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![from_ms, to_ms], |r| {
            Ok((
                r.get::<_, i64>(0)?,    // ts_ms
                r.get::<_, String>(1)?, // model
                r.get::<_, String>(2)?, // cwd
                r.get::<_, String>(3)?, // provider
                r.get::<_, i64>(4)?,    // input
                r.get::<_, i64>(5)?,    // output
                r.get::<_, i64>(6)?,    // cache_read
                r.get::<_, i64>(7)?,    // cache_creation
            ))
        })?;

        let mut totals = Bucket::default();
        let mut by_day: HashMap<String, Bucket> = HashMap::new();
        let mut by_model: HashMap<String, Bucket> = HashMap::new();
        let mut by_project: HashMap<String, Bucket> = HashMap::new();
        let mut by_provider: HashMap<String, Bucket> = HashMap::new();

        for row in rows {
            let (ts_ms, model, cwd, provider, input, output, cr, cc) = row?;
            // Cost is computed at query time from current pricing, so price-table fixes apply
            // retroactively without a re-scan.
            let cost = if model.is_empty() {
                0.0
            } else {
                pricing::cost(
                    &model,
                    &Usage {
                        input: input.max(0) as u64,
                        output: output.max(0) as u64,
                        cache_read: cr.max(0) as u64,
                        cache_creation: cc.max(0) as u64,
                    },
                )
            };
            let b = Cells {
                input,
                output,
                cache_read: cr,
                cache_creation: cc,
                cost,
            };
            totals.add(&b);
            let day = Local
                .timestamp_millis_opt(ts_ms)
                .single()
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "?".into());
            by_day.entry(day).or_default().add(&b);
            let model_key = if model.is_empty() { "unknown".into() } else { model };
            by_model.entry(model_key).or_default().add(&b);
            let proj_key = if cwd.is_empty() { "unknown".into() } else { cwd };
            by_project.entry(proj_key).or_default().add(&b);
            let prov_key = if provider.is_empty() { "claude".into() } else { provider };
            by_provider.entry(prov_key).or_default().add(&b);
        }

        Ok(StatsResult {
            from_ms,
            to_ms,
            totals: totals.into_row("total".into()),
            by_day: sorted_rows(by_day, |a, b| a.key.cmp(&b.key)),
            by_model: sorted_rows(by_model, |a, b| b.cost.total_cmp(&a.cost)),
            by_project: sorted_rows(by_project, |a, b| b.cost.total_cmp(&a.cost)),
            by_provider: sorted_rows(by_provider, |a, b| b.cost.total_cmp(&a.cost)),
        })
    }

    /// Per-agent conversation history over `[from_ms, to_ms)`: how many distinct
    /// conversations (sessions) you've had with claude / codex / gemini, across how
    /// many projects, the first/last activity, and the token + cost totals. Backs
    /// the settings "History" tab. A conversation = one distinct `session_id`.
    pub fn conversations(&self, from_ms: i64, to_ms: i64) -> Result<ConvStats> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT provider, session_id, ts_ms, model, cwd, input, output, cache_read, cache_creation
             FROM usage_events WHERE ts_ms >= ?1 AND ts_ms < ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![from_ms, to_ms], |r| {
            Ok((
                r.get::<_, String>(0)?, // provider
                r.get::<_, String>(1)?, // session_id
                r.get::<_, i64>(2)?,    // ts_ms
                r.get::<_, String>(3)?, // model
                r.get::<_, String>(4)?, // cwd
                r.get::<_, i64>(5)?,    // input
                r.get::<_, i64>(6)?,    // output
                r.get::<_, i64>(7)?,    // cache_read
                r.get::<_, i64>(8)?,    // cache_creation
            ))
        })?;

        #[derive(Default)]
        struct Acc {
            sessions: HashSet<String>,
            projects: HashSet<String>,
            first: Option<i64>,
            last: Option<i64>,
            input: i64,
            output: i64,
            cost: f64,
        }
        let mut by: HashMap<String, Acc> = HashMap::new();
        for row in rows {
            let (provider, session_id, ts_ms, model, cwd, input, output, cr, cc) = row?;
            let prov = if provider.is_empty() { "claude".to_string() } else { provider };
            let cost = if model.is_empty() {
                0.0
            } else {
                pricing::cost(
                    &model,
                    &Usage {
                        input: input.max(0) as u64,
                        output: output.max(0) as u64,
                        cache_read: cr.max(0) as u64,
                        cache_creation: cc.max(0) as u64,
                    },
                )
            };
            let a = by.entry(prov).or_default();
            if !session_id.is_empty() {
                a.sessions.insert(session_id);
            }
            if !cwd.is_empty() {
                a.projects.insert(cwd);
            }
            a.first = Some(a.first.map_or(ts_ms, |f| f.min(ts_ms)));
            a.last = Some(a.last.map_or(ts_ms, |l| l.max(ts_ms)));
            a.input += input;
            a.output += output;
            a.cost += cost;
        }

        let mut by_provider: Vec<ConvRow> = by
            .into_iter()
            .map(|(provider, a)| ConvRow {
                provider,
                conversations: a.sessions.len() as u64,
                projects: a.projects.len() as u64,
                first_ms: a.first.unwrap_or(0),
                last_ms: a.last.unwrap_or(0),
                input: a.input,
                output: a.output,
                cost: a.cost,
            })
            .collect();
        // Busiest agent first.
        by_provider.sort_by(|x, y| y.conversations.cmp(&x.conversations));
        let total_conversations = by_provider.iter().map(|r| r.conversations).sum();
        Ok(ConvStats {
            from_ms,
            to_ms,
            total_conversations,
            by_provider,
        })
    }
}

/// One row of the History tab: a single agent's conversation totals.
#[derive(Serialize)]
pub struct ConvRow {
    pub provider: String,
    pub conversations: u64,
    pub projects: u64,
    pub first_ms: i64,
    pub last_ms: i64,
    pub input: i64,
    pub output: i64,
    pub cost: f64,
}

#[derive(Serialize)]
pub struct ConvStats {
    pub from_ms: i64,
    pub to_ms: i64,
    pub total_conversations: u64,
    pub by_provider: Vec<ConvRow>,
}

/// Internal accumulator with a running event count.
#[derive(Default)]
struct Bucket {
    cells: Cells,
    events: u64,
}

#[derive(Default, Clone, Copy)]
struct Cells {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
    cost: f64,
}

impl Bucket {
    fn add(&mut self, c: &Cells) {
        self.cells.input += c.input;
        self.cells.output += c.output;
        self.cells.cache_read += c.cache_read;
        self.cells.cache_creation += c.cache_creation;
        self.cells.cost += c.cost;
        self.events += 1;
    }
    fn into_row(self, key: String) -> StatRow {
        StatRow {
            key,
            input: self.cells.input,
            output: self.cells.output,
            cache_read: self.cells.cache_read,
            cache_creation: self.cells.cache_creation,
            cost: self.cells.cost,
            events: self.events,
        }
    }
}

fn sorted_rows(
    map: HashMap<String, Bucket>,
    cmp: impl Fn(&StatRow, &StatRow) -> std::cmp::Ordering,
) -> Vec<StatRow> {
    let mut v: Vec<StatRow> = map.into_iter().map(|(k, b)| b.into_row(k)).collect();
    v.sort_by(cmp);
    v
}

#[derive(Serialize)]
pub struct StatRow {
    pub key: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    pub cost: f64,
    pub events: u64,
}

#[derive(Serialize)]
pub struct StatsResult {
    pub from_ms: i64,
    pub to_ms: i64,
    pub totals: StatRow,
    pub by_day: Vec<StatRow>,
    pub by_model: Vec<StatRow>,
    pub by_project: Vec<StatRow>,
    pub by_provider: Vec<StatRow>,
}

// ---- scanner ----------------------------------------------------------------

fn projects_root() -> Option<PathBuf> {
    crate::paths::home_dir().map(|h| h.join(".claude/projects"))
}

/// Default db location: `~/.claude/cc-console/usage.db`.
pub fn default_db_path() -> Option<PathBuf> {
    crate::paths::home_dir().map(|h| h.join(".claude/cc-console/usage.db"))
}

/// Blocking scan loop — meant to run on its own OS thread. Does a full incremental sweep of all
/// project jsonl files every `SCAN_INTERVAL`, persisting new assistant-message usage rows.
pub fn run_scanner(store: std::sync::Arc<UsageStore>) {
    let Some(root) = projects_root() else {
        tracing::error!("$HOME not set — usage scanner disabled");
        return;
    };
    let codex = codex_root();
    let gemini = gemini_root();
    tracing::info!(dir = %root.display(), "usage scanner started (full history)");
    loop {
        if let Err(e) = scan_once(&store, &root) {
            tracing::warn!(?e, "usage scan tick failed");
        }
        if let Some(cx) = &codex {
            if let Err(e) = codex_scan_once(&store, cx) {
                tracing::warn!(?e, "codex usage scan tick failed");
            }
        }
        if let Some(gx) = &gemini {
            if let Err(e) = gemini_scan_once(&store, gx) {
                tracing::warn!(?e, "gemini usage scan tick failed");
            }
        }
        std::thread::sleep(SCAN_INTERVAL);
    }
}

fn scan_once(store: &UsageStore, root: &Path) -> Result<()> {
    let Ok(dirs) = fs::read_dir(root) else {
        return Ok(());
    };
    for dir in dirs.filter_map(Result::ok) {
        let p = dir.path();
        if !p.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&p) else { continue };
        for f in files.filter_map(Result::ok) {
            let path = f.path();
            if path.extension().is_some_and(|x| x == "jsonl") {
                if let Err(e) = scan_file(store, &path) {
                    tracing::debug!(?e, file = %path.display(), "scan_file failed");
                }
            }
        }
    }
    Ok(())
}

fn scan_file(store: &UsageStore, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy().to_string();
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();

    let mut conn = store.conn.lock().unwrap();
    let mut offset = UsageStore::get_offset(&conn, &path_str);
    if len < offset {
        offset = 0; // truncated/rotated — re-read (INSERT OR IGNORE keeps it idempotent).
    }
    if len == offset {
        return Ok(());
    }

    file.seek(SeekFrom::Start(offset))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;

    // Only consume up to the last complete line; keep a half-written tail for next tick.
    let consumed = match buf.rfind('\n') {
        Some(i) => i + 1,
        None => return Ok(()), // no complete line yet
    };

    let mut events = Vec::new();
    for line in buf[..consumed].lines() {
        if let Some(ev) = parse_event(line) {
            events.push(ev);
        }
    }

    UsageStore::insert_events(&mut conn, &events)?;
    UsageStore::set_offset(&conn, &path_str, offset + consumed as u64)?;
    Ok(())
}

/// Parse one JSONL line into a UsageEvent, or None if it isn't a billable assistant message.
fn parse_event(line: &str) -> Option<UsageEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    let msg = v.get("message")?;
    if msg.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let usage_v = msg.get("usage")?;
    let usage = Usage {
        input: usage_v.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
        output: usage_v.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
        cache_read: usage_v
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_creation: usage_v
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    };
    // Skip empty rows (e.g. some tool-only assistant entries with all-zero usage).
    if usage.input == 0 && usage.output == 0 && usage.cache_read == 0 && usage.cache_creation == 0 {
        return None;
    }

    let uuid = v.get("uuid").and_then(Value::as_str).unwrap_or("").to_string();
    let request_id = v
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if uuid.is_empty() {
        return None; // need a primary key
    }
    let ts_ms = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_ts_ms)
        .unwrap_or(0);
    let session_id = v
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
    let model = msg.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let cost = if model.is_empty() {
        0.0
    } else {
        pricing::cost(&model, &usage)
    };

    Some(UsageEvent {
        uuid,
        request_id,
        ts_ms,
        session_id,
        cwd,
        model,
        provider: "claude".into(),
        usage,
        cost,
    })
}

fn parse_ts_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

// ---- codex scanner ----------------------------------------------------------
//
// Codex CLI writes one rollout JSONL per session under ~/.codex/sessions/YYYY/MM/DD/. Token
// usage lives in `event_msg`→`token_count` events (`info.last_token_usage` is the per-turn
// delta — we sum those). The model id is on `turn_context` events and cwd on `session_meta`/
// `turn_context`, both of which precede the token_count events, so we track the latest seen
// (persisted per-file in `codex_sessions` so appended chunks read past the header stay tagged).

fn codex_root() -> Option<PathBuf> {
    crate::paths::home_dir().map(|h| h.join(".codex/sessions"))
}

fn codex_scan_once(store: &UsageStore, root: &Path) -> Result<()> {
    let mut files = Vec::new();
    collect_codex_files(root, &mut files, 0);
    for path in files {
        if let Err(e) = codex_scan_file(store, &path) {
            tracing::debug!(?e, file = %path.display(), "codex scan_file failed");
        }
    }
    Ok(())
}

/// Recursively collect `rollout-*.jsonl` files (the YYYY/MM/DD tree is shallow; cap depth).
fn collect_codex_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 5 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.filter_map(Result::ok) {
        let p = e.path();
        if p.is_dir() {
            collect_codex_files(&p, out, depth + 1);
        } else if p.extension().is_some_and(|x| x == "jsonl")
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-"))
        {
            out.push(p);
        }
    }
}

fn codex_get_meta(conn: &Connection, path: &str) -> (String, String, String) {
    conn.query_row(
        "SELECT cwd, model, sid FROM codex_sessions WHERE path = ?1",
        [path],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap_or_else(|_| (String::new(), String::new(), String::new()))
}

fn codex_set_meta(conn: &Connection, path: &str, cwd: &str, model: &str, sid: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO codex_sessions(path, cwd, model, sid) VALUES(?1,?2,?3,?4)
         ON CONFLICT(path) DO UPDATE SET cwd=excluded.cwd, model=excluded.model, sid=excluded.sid",
        rusqlite::params![path, cwd, model, sid],
    )?;
    Ok(())
}

fn codex_scan_file(store: &UsageStore, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy().to_string();
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();

    let mut conn = store.conn.lock().unwrap();
    let mut offset = UsageStore::get_offset(&conn, &path_str);
    if len < offset {
        offset = 0;
    }
    if len == offset {
        return Ok(());
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let consumed = match buf.rfind('\n') {
        Some(i) => i + 1,
        None => return Ok(()),
    };

    // Seed running cwd/model/sid from any previously-persisted header for this file.
    let (mut cwd, mut model, mut sid) = codex_get_meta(&conn, &path_str);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("cx").to_string();
    let mut events = Vec::new();
    for line in buf[..consumed].lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let typ = v.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = v.get("payload");
        match typ {
            "session_meta" => {
                if let Some(p) = payload {
                    if let Some(c) = p.get("cwd").and_then(Value::as_str) {
                        cwd = c.to_string();
                    }
                    if let Some(id) = p.get("id").and_then(Value::as_str) {
                        sid = id.to_string();
                    }
                }
            }
            "turn_context" => {
                if let Some(p) = payload {
                    if let Some(m) = p.get("model").and_then(Value::as_str) {
                        model = m.to_string();
                    }
                    if let Some(c) = p.get("cwd").and_then(Value::as_str) {
                        cwd = c.to_string();
                    }
                }
            }
            "event_msg" => {
                let Some(p) = payload else { continue };
                if p.get("type").and_then(Value::as_str) != Some("token_count") {
                    continue;
                }
                let Some(last) = p.get("info").and_then(|i| i.get("last_token_usage")) else {
                    continue;
                };
                let g = |k: &str| last.get(k).and_then(Value::as_u64).unwrap_or(0);
                let input_total = g("input_tokens");
                let cached = g("cached_input_tokens").min(input_total);
                let usage = Usage {
                    input: input_total - cached,    // uncached input
                    cache_read: cached,
                    output: g("output_tokens"),     // reasoning tokens already included
                    cache_creation: 0,
                };
                if usage.input == 0 && usage.output == 0 && usage.cache_read == 0 {
                    continue;
                }
                let ts_str = v.get("timestamp").and_then(Value::as_str).unwrap_or("");
                let ts_ms = parse_ts_ms(ts_str).unwrap_or(0);
                let total = g("total_tokens");
                let cost = if model.is_empty() { 0.0 } else { pricing::cost(&model, &usage) };
                events.push(UsageEvent {
                    // Stable, content-derived key so re-reads (after rotation) stay idempotent.
                    uuid: format!("cx:{}|{}|{}", stem, ts_str, total),
                    request_id: String::new(),
                    ts_ms,
                    session_id: sid.clone(),
                    cwd: cwd.clone(),
                    model: model.clone(),
                    provider: "codex".into(),
                    usage,
                    cost,
                });
            }
            _ => {}
        }
    }

    UsageStore::insert_events(&mut conn, &events)?;
    UsageStore::set_offset(&conn, &path_str, offset + consumed as u64)?;
    codex_set_meta(&conn, &path_str, &cwd, &model, &sid)?;
    Ok(())
}

// ---- gemini scanner ---------------------------------------------------------
//
// Gemini CLI writes one whole-file JSON per chat at ~/.gemini/tmp/<slug>/chats/
// session-*.json (rewritten each turn, NOT appended). Each assistant message
// (`type:"gemini"`) has a stable `id`, `model`, `timestamp`, and per-message
// `tokens{input,output,cached,thoughts,tool}`. Because the file is rewritten we
// can't use byte offsets; instead we re-read only when the file's mtime changes
// (stored in scan_offsets as ms) and dedup messages by their `id`.

fn gemini_root() -> Option<PathBuf> {
    crate::paths::home_dir().map(|h| h.join(".gemini"))
}

/// slug → cwd from ~/.gemini/projects.json (reverse of the cwd→slug map), so
/// gemini usage rows get a project (cwd) for the by-project breakdown.
fn gemini_slug_cwd(root: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(s) = fs::read_to_string(root.join("projects.json")) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<Value>(&s) else {
        return out;
    };
    if let Some(obj) = v.get("projects").and_then(Value::as_object) {
        for (cwd, slug) in obj {
            if let Some(slug) = slug.as_str() {
                out.insert(slug.to_string(), cwd.clone());
            }
        }
    }
    out
}

fn gemini_scan_once(store: &UsageStore, root: &Path) -> Result<()> {
    let slug_cwd = gemini_slug_cwd(root);
    let Ok(slugs) = fs::read_dir(root.join("tmp")) else {
        return Ok(());
    };
    for slug_dir in slugs.filter_map(Result::ok) {
        let chats = slug_dir.path().join("chats");
        let Ok(files) = fs::read_dir(&chats) else { continue };
        for f in files.filter_map(Result::ok) {
            let path = f.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("session-") && path.extension().is_some_and(|x| x == "json") {
                if let Err(e) = gemini_scan_file(store, &path, &slug_cwd) {
                    tracing::debug!(?e, file = %path.display(), "gemini scan_file failed");
                }
            }
        }
    }
    Ok(())
}

fn gemini_scan_file(
    store: &UsageStore,
    path: &Path,
    slug_cwd: &HashMap<String, String>,
) -> Result<()> {
    let path_str = path.to_string_lossy().to_string();
    let mtime_ms = fs::metadata(path)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let mut conn = store.conn.lock().unwrap();
    // Reuse scan_offsets to store the last-seen mtime (ms) instead of a byte offset.
    let seen = UsageStore::get_offset(&conn, &path_str) as i64;
    if seen == mtime_ms {
        return Ok(());
    }

    let s = fs::read_to_string(path)?;
    let Ok(v) = serde_json::from_str::<Value>(&s) else {
        return Ok(()); // partially-written / invalid JSON; retry next mtime change
    };
    let session_id = v.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
    let slug = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let cwd = slug_cwd.get(slug).cloned().unwrap_or_default();

    let mut events = Vec::new();
    if let Some(msgs) = v.get("messages").and_then(Value::as_array) {
        for m in msgs {
            if m.get("type").and_then(Value::as_str) != Some("gemini") {
                continue;
            }
            let Some(t) = m.get("tokens") else { continue };
            let g = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
            let input = g("input");
            let cached = g("cached").min(input);
            let usage = Usage {
                input: input - cached,
                cache_read: cached,
                output: g("output") + g("thoughts") + g("tool"),
                cache_creation: 0,
            };
            if usage.input == 0 && usage.output == 0 && usage.cache_read == 0 {
                continue;
            }
            let id = m.get("id").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() {
                continue;
            }
            let ts_ms = m
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_ts_ms)
                .unwrap_or(0);
            let model = m.get("model").and_then(Value::as_str).unwrap_or("").to_string();
            let cost = if model.is_empty() { 0.0 } else { pricing::cost(&model, &usage) };
            events.push(UsageEvent {
                uuid: format!("gm:{id}"),
                request_id: String::new(),
                ts_ms,
                session_id: session_id.clone(),
                cwd: cwd.clone(),
                model,
                provider: "gemini".into(),
                usage,
                cost,
            });
        }
    }

    UsageStore::insert_events(&mut conn, &events)?;
    UsageStore::set_offset(&conn, &path_str, mtime_ms as u64)?;
    Ok(())
}
