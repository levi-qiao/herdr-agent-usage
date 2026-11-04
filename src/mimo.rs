//! MiMo Code's local session evidence: model, context occupancy, and topic.
//!
//! MiMo keeps every session in one SQLite database under its data dir. The
//! same table that holds the conversation also holds per-step token counters,
//! so the sidebar's `cx` row can be computed without a provider API — which
//! Xiaomi does not publish. There is no MiMo quota collector.
//!
//! `mimocode.db` also stores live OAuth credentials in `account` and
//! `control_account`. Those tables are never opened for reading; only
//! `session`, `message`, and `part` are selected, and always with a bounded
//! `ORDER BY time … LIMIT`, never a full-table scan.

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Exact session-id lookup. Never a full-table scan.
const SESSION_BY_ID: &str = "SELECT id, directory, title, parent_id, time_updated
     FROM session WHERE id = ?1 LIMIT 1";
/// Bounded cwd fallback: root sessions only (`parent_id IS NULL`). Subagent
/// sessions carry their parent's id and must never be attributed to a pane.
const ROOT_SESSIONS_BY_DIRECTORY: &str = "SELECT id, title, time_updated
     FROM session
     WHERE directory = ?1 AND parent_id IS NULL
     ORDER BY time_updated DESC
     LIMIT 8";
/// Bounded same-session model identity from the **main** conversation only.
/// Subagent actors (`explore-*`, `checkpoint-writer-*`) share the session id
/// and must never name the pane's model or fill its `cx`.
const MESSAGE_DATA_FOR_SESSION: &str = "SELECT data FROM message
     WHERE session_id = ?1
       AND agent_id = 'main'
     ORDER BY time_created DESC
     LIMIT 8";
/// Bounded same-session context from the main agent's newest `step-finish`.
/// Joining `message` is required: parts themselves have no actor column.
const STEP_DATA_FOR_SESSION: &str = "SELECT part.data FROM part
     JOIN message ON message.id = part.message_id
     WHERE part.session_id = ?1
       AND message.agent_id = 'main'
       AND json_extract(part.data, '$.type') = 'step-finish'
     ORDER BY part.time_created DESC
     LIMIT 24";
/// Assistant-message fallback when no usable main-agent `step-finish` has
/// tokens yet. MiMo's footer uses the last assistant row with `tokens.output > 0`.
const ASSISTANT_MESSAGE_DATA_FOR_SESSION: &str = "SELECT data FROM message
     WHERE session_id = ?1
       AND agent_id = 'main'
       AND json_extract(data, '$.role') = 'assistant'
     ORDER BY time_created DESC
     LIMIT 16";
/// A table this layout needs. Absent means the layout is not in use.
const TABLE_BY_NAME: &str =
    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1";
const MAX_MODELS_BYTES: u64 = 8 * 1024 * 1024;
/// Two root sessions updated within this window of each other cannot be told
/// apart from cwd alone; prefer no `cx` over a confidently wrong one.
/// `session.time_updated` is milliseconds (`Date.now()`), so this is 120s.
const AMBIGUITY_MARGIN_MS: i64 = 120 * 1000;
/// SQLite busy timeout for a locked/mid-write `mimocode.db`.
const SQLITE_BUSY_TIMEOUT_MS: u64 = 100;
/// Default compaction trigger ratio MiMo uses when flooring the usable window
/// (`MIMOCODE_COMPACTION_TRIGGER_RATIO`, default 0.9).
const DEFAULT_COMPACTION_TRIGGER_RATIO: f64 = 0.9;

/// One on-disk layout of MiMo's session store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MimoPaths {
    pub db: PathBuf,
    pub models: PathBuf,
    pub config: PathBuf,
}

impl MimoPaths {
    /// Prefer `MIMOCODE_HOME` (MiMo relocates `{data,cache,config}` under it),
    /// then catalog overrides `MIMOCODE_MODELS_PATH` / `MIMO_MODELS_PATH`,
    /// then XDG. `MIMO_DB_PATH` stays as a legacy alias for the database.
    pub fn from_env() -> Option<Self> {
        if let Ok(home) = std::env::var("MIMOCODE_HOME") {
            if !home.trim().is_empty() {
                let home = PathBuf::from(home);
                let models =
                    models_override()?.unwrap_or_else(|| home.join("cache").join("models.json"));
                return Some(Self {
                    db: home.join("data").join("mimocode.db"),
                    models,
                    config: home.join("config").join("mimocode.jsonc"),
                });
            }
        }
        if let Ok(db) = std::env::var("MIMO_DB_PATH") {
            if !db.trim().is_empty() {
                let db = PathBuf::from(db);
                let models = match models_override()? {
                    Some(path) => path,
                    None => default_models_path()?,
                };
                return Some(Self {
                    db,
                    models,
                    config: config_path()?,
                });
            }
        }
        let data = data_dir()?;
        let models = match models_override()? {
            Some(path) => path,
            None => cache_dir()?.join("models.json"),
        };
        Some(Self {
            db: data.join("mimocode.db"),
            models,
            config: config_path()?,
        })
    }

    /// Fixture layout: files live in one directory.
    pub fn from_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            db: dir.join("mimocode.db"),
            models: dir.join("models.json"),
            config: dir.join("mimocode.jsonc"),
        }
    }
}

/// `MIMOCODE_MODELS_PATH` first (MiMo’s own override), then legacy
/// `MIMO_MODELS_PATH`. `None` means “use the default cache path”.
fn models_override() -> Option<Option<PathBuf>> {
    for key in ["MIMOCODE_MODELS_PATH", "MIMO_MODELS_PATH"] {
        if let Ok(path) = std::env::var(key) {
            if !path.trim().is_empty() {
                return Some(Some(PathBuf::from(path)));
            }
        }
    }
    Some(None)
}

fn data_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(xdg).join("mimocode"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/share/mimocode"))
}

fn cache_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(xdg).join("mimocode"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".cache/mimocode"))
}

fn config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("mimocode"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/mimocode"))
}

fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("mimocode.jsonc"))
}

fn default_models_path() -> Option<PathBuf> {
    Some(cache_dir()?.join("models.json"))
}

/// What one pane needs from a MiMo session: model, context occupancy, title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEvidence {
    pub session_id: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub context_tokens: Option<u64>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLookup {
    Found(SessionEvidence),
    Missing,
    Unreadable,
    /// Two root sessions in the same directory are too close to tell apart.
    Ambiguous,
}

/// Resolve a MiMo pane's session without guessing across subagents.
///
/// Order:
/// 1. Exact `agent_session` id when Herdr (or a bridge such as mimo-code-herdr)
///    exposes one.
/// 2. Otherwise the newest root session (`parent_id IS NULL`) whose
///    `directory` equals the pane cwd, provided no second root session was
///    updated within [`AMBIGUITY_MARGIN_MS`] of it.
pub fn resolve_session(paths: &MimoPaths, session_id: Option<&str>, cwd: &str) -> SessionLookup {
    if let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
        return lookup_by_id(paths, id);
    }
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return SessionLookup::Missing;
    }
    let Ok(connection) = open_readonly(&paths.db) else {
        return SessionLookup::Unreadable;
    };
    match root_sessions_for_directory(&connection, cwd) {
        Ok(rows) => {
            let Some(chosen) = choose_root_session(&rows) else {
                return if rows.is_empty() {
                    SessionLookup::Missing
                } else {
                    SessionLookup::Ambiguous
                };
            };
            match read_session_evidence(&connection, &chosen) {
                Ok(Some(evidence)) => SessionLookup::Found(evidence),
                Ok(None) => SessionLookup::Missing,
                Err(_) => SessionLookup::Unreadable,
            }
        }
        Err(_) => SessionLookup::Unreadable,
    }
}

/// Newest root session wins unless a sibling is within the ambiguity margin.
fn choose_root_session(rows: &[SessionRow]) -> Option<String> {
    let newest = rows.first()?;
    if let Some(next) = rows.get(1) {
        if next.time_updated + AMBIGUITY_MARGIN_MS >= newest.time_updated {
            return None;
        }
    }
    Some(newest.id.clone())
}

struct SessionRow {
    id: String,
    time_updated: i64,
}

fn lookup_by_id(paths: &MimoPaths, session_id: &str) -> SessionLookup {
    let Ok(connection) = open_readonly(&paths.db) else {
        return SessionLookup::Unreadable;
    };
    match read_session_evidence(&connection, session_id) {
        Ok(Some(evidence)) => SessionLookup::Found(evidence),
        Ok(None) => SessionLookup::Missing,
        Err(_) => SessionLookup::Unreadable,
    }
}

fn open_readonly(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // A short writer lock must not look like “no session”.
    connection.busy_timeout(std::time::Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))?;
    Ok(connection)
}

fn table_exists(connection: &Connection, name: &str) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare(TABLE_BY_NAME)?;
    let mut rows = statement.query([name])?;
    Ok(rows.next()?.is_some())
}

fn root_sessions_for_directory(
    connection: &Connection,
    directory: &str,
) -> rusqlite::Result<Vec<SessionRow>> {
    if !table_exists(connection, "session")? {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(ROOT_SESSIONS_BY_DIRECTORY)?;
    let mut rows = statement.query([directory])?;
    let mut collected = Vec::new();
    while let Some(row) = rows.next()? {
        collected.push(SessionRow {
            id: row.get(0)?,
            time_updated: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
        });
    }
    Ok(collected)
}

fn read_session_evidence(
    connection: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<SessionEvidence>> {
    if !table_exists(connection, "session")? {
        return Ok(None);
    }
    let (title, parent_id) = match session_meta(connection, session_id)? {
        Some(meta) => meta,
        None => return Ok(None),
    };
    // A subagent session is never the pane's conversation. An exact id that
    // names one still yields evidence-free Missing so the pane stays honest.
    if parent_id.is_some() {
        return Ok(None);
    }
    let (provider_id, model_id) = session_identity(connection, session_id)?;
    let context_tokens = session_context(connection, session_id)?;
    Ok(Some(SessionEvidence {
        session_id: session_id.to_string(),
        provider_id,
        model_id,
        context_tokens,
        title,
    }))
}

fn session_meta(
    connection: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<(Option<String>, Option<String>)>> {
    let mut statement = connection.prepare(SESSION_BY_ID)?;
    let mut rows = statement.query([session_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    // columns: id, directory, title, parent_id, time_updated
    let title: Option<String> = row.get(2)?;
    let parent_id: Option<String> = row.get(3)?;
    Ok(Some((title.filter(|t| !t.trim().is_empty()), parent_id)))
}

fn session_identity(
    connection: &Connection,
    session_id: &str,
) -> rusqlite::Result<(Option<String>, Option<String>)> {
    let mut statement = connection.prepare(MESSAGE_DATA_FOR_SESSION)?;
    let mut rows = statement.query([session_id])?;
    while let Some(row) = rows.next()? {
        let data: String = row.get(0)?;
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if let Some((provider_id, model_id)) = provider_from_message(&value) {
            return Ok((Some(provider_id), model_id));
        }
    }
    Ok((None, None))
}

fn session_context(connection: &Connection, session_id: &str) -> rusqlite::Result<Option<u64>> {
    let mut statement = connection.prepare(STEP_DATA_FOR_SESSION)?;
    let mut rows = statement.query([session_id])?;
    while let Some(row) = rows.next()? {
        let data: String = row.get(0)?;
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        // The footer counts the last completed turn. A step with no output
        // is still in progress and must not replace that turn.
        let Some(occupied) = context_tokens_from_step(&value) else {
            continue;
        };
        let output = value
            .get("tokens")
            .map(|tokens| token(tokens, "output"))
            .unwrap_or(0);
        if output == 0 {
            continue;
        }
        return Ok(Some(occupied));
    }
    drop(rows);
    drop(statement);
    let mut statement = connection.prepare(ASSISTANT_MESSAGE_DATA_FOR_SESSION)?;
    let mut rows = statement.query([session_id])?;
    while let Some(row) = rows.next()? {
        let data: String = row.get(0)?;
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if value.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(tokens) = value.get("tokens") else {
            continue;
        };
        // Match MiMoCode's computeContextUsage: last assistant row with output.
        if token(tokens, "output") == 0 {
            continue;
        }
        if let Some(occupied) = context_tokens_from_tokens(tokens) {
            return Ok(Some(occupied));
        }
    }
    Ok(None)
}

fn provider_from_message(value: &Value) -> Option<(String, Option<String>)> {
    // Assistant messages carry `providerID` / `modelID` at the top level.
    // User messages nest the same fields under `model`.
    let provider_id = string_field(value, "providerID").or_else(|| {
        value
            .get("model")
            .and_then(|model| string_field(model, "providerID"))
    })?;
    let model_id = string_field(value, "modelID").or_else(|| {
        value
            .get("model")
            .and_then(|model| string_field(model, "modelID"))
    });
    Some((provider_id, model_id))
}

/// Context occupancy of one completed model step.
///
/// Matches MiMoCode's `computeContextUsage` numerator:
/// `input + output + reasoning + cache.read + cache.write`. Do **not** prefer
/// `tokens.total` (its component set varies) and do **not** drop output or
/// reasoning — those are part of the prompt occupancy the footer reports.
/// A step with every counter at zero is a free-model placeholder and yields
/// no usage.
fn context_tokens_from_step(value: &Value) -> Option<u64> {
    if value.get("type").and_then(Value::as_str) != Some("step-finish") {
        return None;
    }
    context_tokens_from_tokens(value.get("tokens")?)
}

fn context_tokens_from_tokens(tokens: &Value) -> Option<u64> {
    let cache = tokens.get("cache").unwrap_or(&Value::Null);
    let occupied = token(tokens, "input")
        .saturating_add(token(tokens, "output"))
        .saturating_add(token(tokens, "reasoning"))
        .saturating_add(token(cache, "read"))
        .saturating_add(token(cache, "write"));
    (occupied > 0).then_some(occupied)
}

fn token(value: &Value, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or(0)
}

/// The context window MiMo's own model catalog publishes.
///
/// Same `provider.models[model].limit.context` shape OpenCode and Kilo use.
/// Prefer the exact `provider_id` from the session's message. When that key
/// is absent, search every top-level provider for the model; if more than one
/// distinct limit matches, return `None` rather than pick a wrong window.
pub fn model_context_window(paths: &MimoPaths, provider_id: &str, model_id: &str) -> Option<u64> {
    let value = read_models_json(&paths.models)?;
    resolve_limit(&value, provider_id, model_id)
}

/// Denominator that matches MiMoCode's own context bar (`contextWindow` /
/// `computeContextUsage`).
///
/// `effective` is `compaction.max_context` when that budget is set and below
/// the provider cap, otherwise `limit.input || limit.context`.
/// `usable = floor(effective * trigger ratio)` with default 0.9.
/// If the catalog cannot supply a cap, return `None` so the caller omits `cx`.
pub fn display_context_window(paths: &MimoPaths, provider_id: &str, model_id: &str) -> Option<u64> {
    let value = read_models_json(&paths.models)?;
    let catalog_cap = resolve_display_cap(&value, provider_id, model_id)?;
    let budget = config_max_context(&paths.config)?;
    let effective = match budget {
        Some(max_context) if max_context > 0 && max_context < catalog_cap => max_context,
        _ => catalog_cap,
    };
    let ratio = compaction_trigger_ratio();
    let usable = (effective as f64 * ratio).floor() as u64;
    (usable > 0).then_some(usable)
}

/// `compaction.max_context` from mimocode config / `MIMOCODE_COMPACTION_MAX_CONTEXT`.
fn config_max_context(config_path: &Path) -> Option<Option<u64>> {
    if let Ok(raw) = std::env::var("MIMOCODE_COMPACTION_MAX_CONTEXT") {
        if let Ok(n) = raw.trim().parse::<u64>() {
            return Some(Some(n));
        }
    }
    let Ok(bytes) = fs::read(config_path) else {
        // No config on disk: no budget override (catalog cap only).
        return Some(None);
    };
    let text = String::from_utf8_lossy(&bytes);
    let stripped = strip_jsonc_comments(&text);
    let Ok(value) = serde_json::from_str::<Value>(&stripped) else {
        return Some(None);
    };
    let max_context = value
        .get("compaction")
        .and_then(|c| c.get("max_context"))
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
        });
    Some(max_context)
}

#[allow(clippy::while_let_on_iterator)]
fn strip_jsonc_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c == '/' && chars.peek() == Some(&'/') {
            while let Some(next) = chars.next() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(next) = chars.next() {
                if next == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn compaction_trigger_ratio() -> f64 {
    std::env::var("MIMOCODE_COMPACTION_TRIGGER_RATIO")
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|ratio| *ratio > 0.0 && *ratio <= 1.0)
        .unwrap_or(DEFAULT_COMPACTION_TRIGGER_RATIO)
}

fn resolve_limit(value: &Value, provider_id: &str, model_id: &str) -> Option<u64> {
    resolve_window(value, provider_id, model_id, limit_at)
}

/// Catalog cap for the context bar: `limit.input` when it is set, otherwise
/// `limit.context`. `model_context_window` stays on `limit.context` alone.
fn resolve_display_cap(value: &Value, provider_id: &str, model_id: &str) -> Option<u64> {
    resolve_window(value, provider_id, model_id, display_cap_at)
}

fn resolve_window(
    value: &Value,
    provider_id: &str,
    model_id: &str,
    pick: fn(&Value, &str, &str) -> Option<u64>,
) -> Option<u64> {
    if let Some(window) = pick(value, provider_id, model_id) {
        return Some(window);
    }
    let object = value.as_object()?;
    let mut found: Option<u64> = None;
    for provider in object.keys() {
        let Some(window) = pick(value, provider, model_id) else {
            continue;
        };
        match found {
            None => found = Some(window),
            Some(existing) if existing == window => {}
            Some(_) => return None,
        }
    }
    found
}

fn read_models_json(path: &Path) -> Option<Value> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_MODELS_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_MODELS_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn limit_at(value: &Value, provider_id: &str, model_id: &str) -> Option<u64> {
    positive_limit(value, provider_id, model_id, "context")
}

fn display_cap_at(value: &Value, provider_id: &str, model_id: &str) -> Option<u64> {
    positive_limit(value, provider_id, model_id, "input")
        .or_else(|| positive_limit(value, provider_id, model_id, "context"))
}

fn positive_limit(value: &Value, provider_id: &str, model_id: &str, field: &str) -> Option<u64> {
    value
        .get(provider_id)?
        .get("models")?
        .get(model_id)?
        .get("limit")?
        .get(field)?
        .as_u64()
        .filter(|window| *window > 0)
}

fn string_field(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// SQL this module is allowed to run. Credential tables must never appear.
#[cfg(test)]
const READABLE_TABLES: &[&str] = &["session", "message", "part", "sqlite_master"];

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn context_sums_input_output_reasoning_and_cache() {
        let step: Value = serde_json::from_str(
            r#"{"type":"step-finish","tokens":{
                "total":77635,"input":5299,"output":715,"reasoning":197,
                "cache":{"write":0,"read":71424}
            }}"#,
        )
        .unwrap();
        // Sum of all occupancy components, including output and reasoning.
        assert_eq!(context_tokens_from_step(&step), Some(77635));
        // Absent total is fine; components still sum.
        let no_total: Value = serde_json::from_str(
            r#"{"type":"step-finish","tokens":{
                "input":5299,"output":715,"reasoning":197,
                "cache":{"write":0,"read":71424}
            }}"#,
        )
        .unwrap();
        assert_eq!(context_tokens_from_step(&no_total), Some(77635));
    }

    #[test]
    fn display_window_is_trigger_ratio_of_the_effective_cap() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        fs::write(
            &paths.models,
            r#"{"xiaomi":{"models":{"mimo-v2.6-pro":{"limit":{"context":1048576}}}}}"#,
        )
        .unwrap();
        // No max_context budget: usable = floor(limit.context * 0.9).
        assert_eq!(
            display_context_window(&paths, "xiaomi", "mimo-v2.6-pro"),
            Some((1_048_576.0_f64 * 0.9).floor() as u64)
        );
    }

    #[test]
    fn display_window_prefers_limit_input_over_context() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        fs::write(
            &paths.models,
            r#"{"xiaomi":{"models":{"mimo-v2.6-pro":{"limit":{"context":1048576,"input":200000}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            display_context_window(&paths, "xiaomi", "mimo-v2.6-pro"),
            Some((200_000.0_f64 * 0.9).floor() as u64)
        );
        assert_eq!(
            model_context_window(&paths, "xiaomi", "mimo-v2.6-pro"),
            Some(1_048_576)
        );
    }

    #[test]
    fn a_lower_compaction_budget_shrinks_the_usable_window() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        fs::write(
            &paths.models,
            r#"{"xiaomi":{"models":{"mimo-v2.6-pro":{"limit":{"context":1048576,"output":131072}}}}}"#,
        )
        .unwrap();
        fs::write(&paths.config, r#"{"compaction":{"max_context":260000}}"#).unwrap();
        // usable = floor(260000 * 0.9) = 234000
        assert_eq!(
            display_context_window(&paths, "xiaomi", "mimo-v2.6-pro"),
            Some(234_000)
        );
    }

    #[test]
    fn subagent_steps_do_not_fill_context() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        write_fixture(
            &paths.db,
            &[("ses_1", "/proj", None, 100)],
            &[(
                "ses_1",
                r#"{"role":"assistant","providerID":"xiaomi","modelID":"mimo-v2.6-pro"}"#,
            )],
            &[],
        );
        let connection = Connection::open(&paths.db).unwrap();
        connection
            .execute(
                "INSERT INTO message (id, session_id, agent_id, time_created, time_updated, data)
                 VALUES ('msg_explore', 'ses_1', 'explore-9', 100, 100, '{}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES ('prt_main', 'msg_0', 'ses_1', 10, 10,
                    '{\"type\":\"step-finish\",\"tokens\":{\"input\":2000,\"output\":10,\"reasoning\":0,\"cache\":{\"read\":0,\"write\":0}}}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES ('prt_explore', 'msg_explore', 'ses_1', 20, 20,
                    '{\"type\":\"step-finish\",\"tokens\":{\"input\":400,\"output\":0,\"reasoning\":0,\"cache\":{\"read\":0,\"write\":0}}}')",
                [],
            )
            .unwrap();
        let lookup = lookup_by_id(&paths, "ses_1");
        let SessionLookup::Found(evidence) = lookup else {
            panic!("expected Found, got {lookup:?}");
        };
        // Newest overall is explore (400); must use main (2010) instead.
        assert_eq!(evidence.context_tokens, Some(2_010));
    }

    #[test]
    fn zero_placeholder_step_yields_no_usage() {
        let step: Value = serde_json::from_str(
            r#"{"type":"step-finish","tokens":{
                "input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}
            }}"#,
        )
        .unwrap();
        assert_eq!(context_tokens_from_step(&step), None);
    }

    #[test]
    fn output_counts_toward_context_occupancy() {
        let tokens: Value =
            serde_json::from_str(r#"{"input":0,"output":40,"cache":{"read":0,"write":0}}"#)
                .unwrap();
        assert_eq!(context_tokens_from_tokens(&tokens), Some(40));
    }

    #[test]
    fn catalog_prefers_exact_provider_and_returns_integer() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        fs::write(
            &paths.models,
            r#"{
                "xiaomi": {"models": {"mimo-v2.6-pro": {"limit": {"context": 1048576}}}},
                "xiaomi-token-plan-cn": {"models": {"mimo-v2.6-pro": {"limit": {"context": 1048576}}}}
            }"#,
        )
        .unwrap();
        assert_eq!(
            model_context_window(&paths, "xiaomi", "mimo-v2.6-pro"),
            Some(1_048_576)
        );
    }

    #[test]
    fn catalog_with_conflicting_limits_returns_none() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        fs::write(
            &paths.models,
            r#"{
                "a": {"models": {"m": {"limit": {"context": 1000}}}},
                "b": {"models": {"m": {"limit": {"context": 2000}}}}
            }"#,
        )
        .unwrap();
        assert_eq!(model_context_window(&paths, "missing", "m"), None);
    }

    #[test]
    fn catalog_with_matching_limits_returns_that_limit() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        fs::write(
            &paths.models,
            r#"{
                "a": {"models": {"m": {"limit": {"context": 1000}}}},
                "b": {"models": {"m": {"limit": {"context": 1000}}}}
            }"#,
        )
        .unwrap();
        assert_eq!(model_context_window(&paths, "missing", "m"), Some(1000));
    }

    #[test]
    fn sql_constants_never_name_credential_tables() {
        for sql in [
            SESSION_BY_ID,
            ROOT_SESSIONS_BY_DIRECTORY,
            MESSAGE_DATA_FOR_SESSION,
            STEP_DATA_FOR_SESSION,
            ASSISTANT_MESSAGE_DATA_FOR_SESSION,
            TABLE_BY_NAME,
        ] {
            let lower = sql.to_ascii_lowercase();
            assert!(!lower.contains("account"), "{sql}");
            assert!(!lower.contains("control_account"), "{sql}");
            assert!(!lower.contains("access_token"), "{sql}");
            assert!(!lower.contains("refresh_token"), "{sql}");
        }
        // The readable-table allowlist is the full set this module may touch.
        assert_eq!(
            READABLE_TABLES,
            &["session", "message", "part", "sqlite_master"]
        );
    }

    fn write_fixture(
        path: &Path,
        sessions: &[(&str, &str, Option<&str>, i64)],
        messages: &[(&str, &str)],
        parts: &[(&str, &str)],
    ) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    project_id TEXT NOT NULL DEFAULT 'proj',
                    parent_id TEXT,
                    slug TEXT NOT NULL DEFAULT 's',
                    directory TEXT NOT NULL DEFAULT '',
                    title TEXT NOT NULL DEFAULT '',
                    version TEXT NOT NULL DEFAULT '1',
                    time_created INTEGER NOT NULL DEFAULT 1,
                    time_updated INTEGER NOT NULL DEFAULT 1
                );
                CREATE TABLE message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    agent_id TEXT NOT NULL DEFAULT 'main',
                    time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL,
                    data TEXT NOT NULL
                );
                CREATE TABLE part (
                    id TEXT PRIMARY KEY,
                    message_id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL,
                    data TEXT NOT NULL
                );
                CREATE TABLE account (
                    id TEXT PRIMARY KEY,
                    access_token TEXT NOT NULL
                );
                INSERT INTO account VALUES ('a', 'super-secret');",
            )
            .unwrap();
        for (index, (id, directory, parent, time_updated)) in sessions.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO session (id, directory, parent_id, title, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        *id,
                        *directory,
                        parent,
                        format!("title-{id}"),
                        *time_updated
                    ],
                )
                .unwrap();
            let _ = index;
        }
        for (index, (session_id, data)) in messages.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO message (id, session_id, time_created, time_updated, data)
                     VALUES (?1, ?2, ?3, ?3, ?4)",
                    rusqlite::params![format!("msg_{index}"), *session_id, index as i64 + 1, *data],
                )
                .unwrap();
        }
        for (index, (session_id, data)) in parts.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                     VALUES (?1, 'msg_0', ?2, ?3, ?3, ?4)",
                    rusqlite::params![format!("prt_{index}"), *session_id, index as i64 + 1, *data],
                )
                .unwrap();
        }
    }

    #[test]
    fn exact_session_lookup_reads_model_context_and_title() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        write_fixture(
            &paths.db,
            &[("ses_1", "/proj", None, 100)],
            &[(
                "ses_1",
                r#"{"role":"assistant","providerID":"xiaomi","modelID":"mimo-v2.6-pro","tokens":{"input":1,"output":1,"cache":{"read":0,"write":0}}}"#,
            )],
            &[(
                "ses_1",
                r#"{"type":"step-finish","tokens":{"input":5299,"output":10,"reasoning":5,"cache":{"read":71424,"write":0}}}"#,
            )],
        );
        let lookup = lookup_by_id(&paths, "ses_1");
        let SessionLookup::Found(evidence) = lookup else {
            panic!("expected Found, got {lookup:?}");
        };
        assert_eq!(evidence.provider_id.as_deref(), Some("xiaomi"));
        assert_eq!(evidence.model_id.as_deref(), Some("mimo-v2.6-pro"));
        assert_eq!(evidence.context_tokens, Some(76_738));
        assert_eq!(evidence.title.as_deref(), Some("title-ses_1"));
    }

    #[test]
    fn placeholder_rows_are_skipped_in_favour_of_the_older_real_step() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        write_fixture(
            &paths.db,
            &[("ses_1", "/proj", None, 100)],
            &[("ses_1", r#"{"role":"assistant"}"#)],
            &[
                // Older completed step, then a newer step that has input but no
                // output yet. The scan must keep the completed step.
                (
                    "ses_1",
                    r#"{"type":"step-finish","tokens":{"input":100,"output":10,"reasoning":0,"cache":{"read":0,"write":0}}}"#,
                ),
                (
                    "ses_1",
                    r#"{"type":"step-finish","tokens":{"input":9000,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}"#,
                ),
            ],
        );
        let lookup = lookup_by_id(&paths, "ses_1");
        let SessionLookup::Found(evidence) = lookup else {
            panic!("expected Found, got {lookup:?}");
        };
        assert_eq!(evidence.context_tokens, Some(110));
    }

    #[test]
    fn subagent_session_is_never_attribute_to_a_pane() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        write_fixture(
            &paths.db,
            &[
                ("ses_root", "/proj", None, 200),
                ("ses_child", "/proj", Some("ses_root"), 300),
            ],
            &[],
            &[],
        );
        // Exact id that names a subagent: Missing (not a silent wrong session).
        assert!(matches!(
            lookup_by_id(&paths, "ses_child"),
            SessionLookup::Missing
        ));
        // cwd fallback picks only the root session.
        let lookup = resolve_session(&paths, None, "/proj");
        let SessionLookup::Found(evidence) = lookup else {
            panic!("expected Found root, got {lookup:?}");
        };
        assert_eq!(evidence.session_id, "ses_root");
    }

    #[test]
    fn two_recent_root_sessions_are_ambiguous() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        write_fixture(
            &paths.db,
            &[
                ("ses_a", "/proj", None, 1_792_320_000_000),
                ("ses_b", "/proj", None, 1_792_320_000_000 + 30_000),
            ],
            &[],
            &[],
        );
        assert!(matches!(
            resolve_session(&paths, None, "/proj"),
            SessionLookup::Ambiguous
        ));
    }

    #[test]
    fn a_clearly_older_sibling_does_not_make_the_newest_ambiguous() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        write_fixture(
            &paths.db,
            &[
                ("ses_old", "/proj", None, 1_792_320_000_000),
                ("ses_new", "/proj", None, 1_792_320_000_000 + 300_000),
            ],
            &[],
            &[],
        );
        let lookup = resolve_session(&paths, None, "/proj");
        let SessionLookup::Found(evidence) = lookup else {
            panic!("expected Found, got {lookup:?}");
        };
        assert_eq!(evidence.session_id, "ses_new");
    }

    #[test]
    fn missing_db_is_unreadable_and_empty_cwd_is_missing() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        assert!(matches!(
            resolve_session(&paths, Some("ses_1"), ""),
            SessionLookup::Unreadable
        ));
        assert!(matches!(
            resolve_session(&paths, None, ""),
            SessionLookup::Missing
        ));
    }

    #[test]
    fn exact_agent_session_id_wins_over_cwd() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        write_fixture(
            &paths.db,
            &[
                ("ses_a", "/proj", None, 1000),
                ("ses_b", "/other", None, 2000),
            ],
            &[],
            &[],
        );
        let lookup = resolve_session(&paths, Some("ses_b"), "/proj");
        let SessionLookup::Found(evidence) = lookup else {
            panic!("expected Found, got {lookup:?}");
        };
        assert_eq!(evidence.session_id, "ses_b");
    }

    #[test]
    fn assistant_message_tokens_are_a_context_fallback() {
        let directory = tempdir().unwrap();
        let paths = MimoPaths::from_dir(directory.path());
        write_fixture(
            &paths.db,
            &[("ses_1", "/proj", None, 100)],
            &[(
                "ses_1",
                r#"{"role":"assistant","providerID":"xiaomi","modelID":"mimo-v2.6-pro","tokens":{"input":200,"output":10,"reasoning":0,"cache":{"read":300,"write":0}}}"#,
            )],
            &[],
        );
        let lookup = lookup_by_id(&paths, "ses_1");
        let SessionLookup::Found(evidence) = lookup else {
            panic!("expected Found, got {lookup:?}");
        };
        assert_eq!(evidence.context_tokens, Some(510));
    }
}
