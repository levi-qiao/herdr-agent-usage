//! Kimi Code subscription quota, model, topic, context, and cache.
//!
//! Kimi Code (the Node CLI, `~/.kimi-code`, or `$KIMI_CODE_HOME`) signs in
//! with an OAuth device login stored at `credentials/kimi-code.json`. Quota is
//! the same `GET https://api.kimi.com/coding/v1/usages` call the CLI's own
//! `/usage` panel makes, authenticated with that access token. The host is
//! pinned: the token is never sent anywhere a config file could redirect it.
//!
//! The response carries the weekly allowance as a top-level `usage` object and
//! each rolling window under `limits[]` with an explicit duration. A window is
//! only placed in the 5h or 7d slot when its own duration says so; anything
//! else is left out rather than borrowed into a slot it does not measure.
//!
//! The access token is never refreshed here. Kimi rotates its refresh token
//! on use, so refreshing from a second process would sign the CLI out. An
//! expired token is a failed fetch, which keeps the last good snapshot.
//!
//! Cache identity is `sha256("kimi\0" || user_id)` from the access token's
//! claims, so a token rotation for the same login keeps its cached reading
//! while a different login cannot inherit it.
//!
//! Per-session evidence comes from Kimi's own files, never from the pane:
//!
//! - `session_index.jsonl` maps Herdr's session id (`session_<uuid>`) to its
//!   directory.
//! - `state.json` in that directory holds `lastPrompt`, used as the topic.
//! - `agents/main/wire.jsonl` records each request's model alias
//!   (`llm.request`), its token split (`usage.record`), and the measured
//!   context length (`token_counting.*`).
//! - `config.toml` `[models."<alias>"]` gives `display_name` and
//!   `max_context_size` for that alias.

use crate::cache::CacheStore;
use crate::model::{
    CacheTotals, CacheUsage, ContextUsage, Provider, ProviderSnapshot, ResetAt, UsageWindow,
    WindowKind,
};
use crate::providers::ProviderError;
use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

const USAGES_URL: &str = "https://api.kimi.com/coding/v1/usages";
const CREDENTIALS_FILE: &str = "credentials/kimi-code.json";
const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_STATE_BYTES: u64 = 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;
const WIRE_TAIL_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SUMMARY_CHARS: usize = 200;

struct KimiCredentials {
    access_token: String,
}

impl std::fmt::Debug for KimiCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KimiCredentials")
            .field("access_token", &"[redacted]")
            .finish()
    }
}

/// Fetch Kimi Code's quota and attach per-session model, topic and context
/// for every session Herdr reported on a Kimi pane.
pub fn fetch_for_sessions(session_ids: &[String]) -> Result<ProviderSnapshot> {
    let home = kimi_home().context("resolve Kimi Code home")?;
    let credentials =
        read_credentials(&home.join(CREDENTIALS_FILE)).map_err(anyhow::Error::from)?;
    let account_id = account_pin(&credentials.access_token);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .timeout_write(Duration::from_secs(10))
        .build();
    let response = agent
        .get(USAGES_URL)
        .set(
            "Authorization",
            &format!("Bearer {}", credentials.access_token),
        )
        .call()
        .map_err(|error| map_request_error(&error))?;
    let value: Value = response
        .into_json()
        .context("decode Kimi Code usages response")?;
    let mut snapshot = parse_usages(&value, CacheStore::now_unix()).map_err(anyhow::Error::from)?;
    let models = model_catalog(&home);
    snapshot.model = default_model(&home).map(|alias| display_name(&alias, &models));
    enrich_sessions_at(&mut snapshot, &home, session_ids, &models);
    Ok(snapshot.with_account_id(account_id))
}

/// Parse the `/coding/v1/usages` body into 5h and 7d windows.
pub fn parse_usages(
    value: &Value,
    fetched_at_unix: u64,
) -> std::result::Result<ProviderSnapshot, ProviderError> {
    let mut windows: Vec<UsageWindow> = Vec::new();
    for limit in value
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(kind) = limit.get("window").and_then(window_kind) else {
            continue;
        };
        if let Some(window) = limit
            .get("detail")
            .and_then(|detail| quota_window(kind, detail))
        {
            push_window(&mut windows, window);
        }
    }
    // The top-level `usage` is the weekly allowance; its reset is the end of
    // the 7d period. It carries no duration of its own, so it only fills the
    // weekly slot when `limits[]` did not already name one.
    if let Some(window) = value
        .get("usage")
        .and_then(|usage| quota_window(WindowKind::Weekly, usage))
    {
        push_window(&mut windows, window);
    }
    // Older responses only carry ratios.
    if let Some(usages) = value.get("usages") {
        for (key, kind) in [
            ("limit_5h", WindowKind::FiveHour),
            ("limit_7d", WindowKind::Weekly),
        ] {
            if let Some(window) = usages.get(key).and_then(|entry| ratio_window(kind, entry)) {
                push_window(&mut windows, window);
            }
        }
    }
    if windows.is_empty() {
        return Err(ProviderError::UnsupportedResponse(
            "Kimi Code usages carried no recognizable window".to_string(),
        ));
    }
    windows.sort_by_key(|window| window.kind.duration_seconds());
    Ok(ProviderSnapshot::new(
        Provider::Kimi,
        windows,
        fetched_at_unix,
    ))
}

fn push_window(windows: &mut Vec<UsageWindow>, window: UsageWindow) {
    if !windows.iter().any(|existing| existing.kind == window.kind) {
        windows.push(window);
    }
}

fn window_kind(window: &Value) -> Option<WindowKind> {
    let duration = json_u64(window.get("duration"))?;
    let unit_seconds = match window.get("timeUnit").and_then(Value::as_str)? {
        "TIME_UNIT_SECOND" => 1,
        "TIME_UNIT_MINUTE" => 60,
        "TIME_UNIT_HOUR" => 60 * 60,
        "TIME_UNIT_DAY" => 24 * 60 * 60,
        _ => return None,
    };
    let seconds = duration.checked_mul(unit_seconds)?;
    [WindowKind::FiveHour, WindowKind::Weekly]
        .into_iter()
        .find(|kind| kind.duration_seconds() == seconds)
}

/// `{limit, used?, remaining?, resetTime}` → a window. Both the limit and one
/// of used/remaining are required, so a missing count can never read as a
/// full allowance.
fn quota_window(kind: WindowKind, detail: &Value) -> Option<UsageWindow> {
    let limit = json_f64(detail.get("limit")).filter(|limit| *limit > 0.0)?;
    let used = json_f64(detail.get("used"))
        .or_else(|| json_f64(detail.get("remaining")).map(|remaining| limit - remaining))?;
    let percent = (used / limit * 100.0).clamp(0.0, 100.0);
    let reset = detail
        .get("resetTime")
        .or_else(|| detail.get("reset_time"))
        .and_then(Value::as_str)
        .and_then(ResetAt::parse);
    UsageWindow::new(kind, percent, reset).ok()
}

fn ratio_window(kind: WindowKind, entry: &Value) -> Option<UsageWindow> {
    let ratio = json_f64(entry.get("used_ratio"))?;
    let reset = entry
        .get("reset_time")
        .and_then(Value::as_str)
        .and_then(ResetAt::parse);
    UsageWindow::new(kind, (ratio * 100.0).clamp(0.0, 100.0), reset).ok()
}

/// Kimi serializes counts as strings (`"100"`); accept numbers too.
fn json_f64(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|number: &f64| number.is_finite())
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn map_request_error(error: &ureq::Error) -> ProviderError {
    match error {
        ureq::Error::Status(401 | 403, _) => ProviderError::MissingCredentials,
        ureq::Error::Status(code, _) => {
            ProviderError::Request(format!("Kimi Code usages returned HTTP {code}"))
        }
        ureq::Error::Transport(transport) => ProviderError::Request(format!(
            "Kimi Code usages request failed: {}",
            transport.kind()
        )),
    }
}

pub fn kimi_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("KIMI_CODE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".kimi-code"))
}

pub fn auth_path() -> Result<PathBuf> {
    Ok(kimi_home()?.join(CREDENTIALS_FILE))
}

pub fn current_account_id() -> Option<String> {
    let credentials = read_credentials(&auth_path().ok()?).ok()?;
    account_pin(&credentials.access_token)
}

/// Kimi rewrites the credential file on every token refresh, so its mtime is
/// not a login generation. The account pin alone decides.
pub fn auth_mtime_unix() -> Option<u64> {
    None
}

fn read_credentials(path: &Path) -> std::result::Result<KimiCredentials, ProviderError> {
    let value =
        read_bounded_json(path, MAX_CREDENTIAL_BYTES).ok_or(ProviderError::MissingCredentials)?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or(ProviderError::MissingCredentials)?
        .to_string();
    Ok(KimiCredentials { access_token })
}

/// The login's `user_id` (or `sub`) claim, digested. The signature is not
/// checked: this only pins a cached reading to the login that produced it.
fn account_pin(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&base64url_decode(payload)?).ok()?;
    let user = claims
        .get("user_id")
        .or_else(|| claims.get("sub"))
        .and_then(Value::as_str)
        .filter(|user| !user.is_empty())?;
    let mut hasher = Sha256::new();
    hasher.update(b"kimi\0");
    hasher.update(user.as_bytes());
    Some(format!("kimi:{:x}", hasher.finalize()))
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(output)
}

fn read_bounded(path: &Path, max_bytes: u64) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > max_bytes {
        return None;
    }
    let mut text = String::new();
    file.take(max_bytes).read_to_string(&mut text).ok()?;
    Some(text)
}

fn read_bounded_json(path: &Path, max_bytes: u64) -> Option<Value> {
    serde_json::from_str(&read_bounded(path, max_bytes)?).ok()
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ModelInfo {
    display_name: Option<String>,
    max_context_size: Option<u64>,
}

fn read_config(home: &Path) -> Option<toml::Table> {
    read_bounded(&home.join("config.toml"), MAX_CONFIG_BYTES)?
        .parse::<toml::Table>()
        .ok()
}

fn default_model(home: &Path) -> Option<String> {
    read_config(home)?
        .get("default_model")?
        .as_str()
        .map(str::to_string)
}

/// `[models."<alias>"]` → display name and context window.
fn model_catalog(home: &Path) -> BTreeMap<String, ModelInfo> {
    let Some(config) = read_config(home) else {
        return BTreeMap::new();
    };
    config
        .get("models")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
        .map(|(alias, entry)| {
            let info = ModelInfo {
                display_name: entry
                    .get("display_name")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string),
                max_context_size: entry
                    .get("max_context_size")
                    .and_then(toml::Value::as_integer)
                    .and_then(|size| u64::try_from(size).ok()),
            };
            (alias.clone(), info)
        })
        .collect()
}

fn display_name(alias: &str, models: &BTreeMap<String, ModelInfo>) -> String {
    models
        .get(alias)
        .and_then(|info| info.display_name.clone())
        .unwrap_or_else(|| alias.rsplit('/').next().unwrap_or(alias).to_string())
}

/// Session ids are one path segment (`session_<uuid>`). Anything else is
/// never looked up.
fn is_session_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Directory of each requested session, from Kimi's own index. The newest
/// line for an id wins. Only directories under `<home>/sessions` are trusted.
fn session_dirs(home: &Path, session_ids: &[String]) -> BTreeMap<String, PathBuf> {
    let mut found = BTreeMap::new();
    let wanted = session_ids
        .iter()
        .filter(|id| is_session_id(id))
        .collect::<Vec<_>>();
    if wanted.is_empty() {
        return found;
    }
    let sessions_root = home.join("sessions");
    let Some(index) = read_bounded(&home.join("session_index.jsonl"), MAX_INDEX_BYTES) else {
        return found;
    };
    for line in index.lines() {
        if !wanted.iter().any(|id| line.contains(id.as_str())) {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let (Some(id), Some(dir)) = (
            entry.get("sessionId").and_then(Value::as_str),
            entry.get("sessionDir").and_then(Value::as_str),
        ) else {
            continue;
        };
        let dir = PathBuf::from(dir);
        if wanted.iter().any(|wanted| wanted.as_str() == id)
            && dir.starts_with(&sessions_root)
            && dir.file_name().is_some_and(|name| name == id)
        {
            found.insert(id.to_string(), dir);
        }
    }
    found
}

fn enrich_sessions_at(
    snapshot: &mut ProviderSnapshot,
    home: &Path,
    session_ids: &[String],
    models: &BTreeMap<String, ModelInfo>,
) {
    for (session_id, dir) in session_dirs(home, session_ids) {
        if let Some(prompt) = last_prompt(&dir.join("state.json")) {
            snapshot
                .session_summaries
                .insert(session_id.clone(), prompt);
        }
        let Some(tail) = read_tail(&dir.join("agents/main/wire.jsonl"), WIRE_TAIL_BYTES) else {
            continue;
        };
        let observation = observe_wire_tail(&tail);
        if let Some(alias) = observation.model.as_deref() {
            snapshot
                .session_models
                .insert(session_id.clone(), display_name(alias, models));
        }
        if let Some(context) = observation.context(&session_id, models) {
            snapshot
                .session_contexts
                .insert(session_id.clone(), context);
        }
    }
}

fn last_prompt(path: &Path) -> Option<String> {
    let state = read_bounded_json(path, MAX_STATE_BYTES)?;
    let prompt = state
        .get("lastPrompt")
        .or_else(|| state.get("title"))
        .and_then(Value::as_str)?;
    let prompt = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if prompt.is_empty() {
        return None;
    }
    Some(prompt.chars().take(MAX_SUMMARY_CHARS).collect())
}

fn read_tail(path: &Path, max_bytes: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes).ok()?;
    let tail = String::from_utf8_lossy(&bytes);
    if start == 0 {
        return Some(tail.into_owned());
    }
    // The first line was cut by the seek.
    tail.split_once('\n').map(|(_, lines)| lines.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TokenUsage {
    fresh_input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct WireObservation {
    model: Option<String>,
    usage: Option<TokenUsage>,
    context_tokens: Option<u64>,
}

impl WireObservation {
    /// Context is Kimi's own measured context length against the model's
    /// configured window; without a measurement, the last request's prompt
    /// plus reply. Cache is that request's read share. Kimi publishes no
    /// prompt-cache lifetime, so no TTL is estimated.
    fn context(
        &self,
        session_id: &str,
        models: &BTreeMap<String, ModelInfo>,
    ) -> Option<ContextUsage> {
        let limit = models
            .get(self.model.as_deref()?)?
            .max_context_size
            .filter(|limit| *limit > 0)?;
        let used_tokens = self.context_tokens.or_else(|| {
            self.usage.map(|usage| {
                usage
                    .fresh_input
                    .saturating_add(usage.cache_read)
                    .saturating_add(usage.cache_write)
                    .saturating_add(usage.output)
            })
        })?;
        let used = used_tokens as f64 / limit as f64 * 100.0;
        let cache = self.usage.and_then(|usage| {
            CacheUsage::from_token_counts(usage.fresh_input, usage.cache_read, usage.cache_write)
                .map(|cache| {
                    let totals = CacheTotals::from_token_counts(
                        cache.fresh_input_tokens,
                        cache.read_tokens,
                        cache.creation_tokens,
                    );
                    cache.with_session_totals(totals, session_id, 0)
                })
        });
        ContextUsage::new(used.clamp(0.0, 100.0))
            .ok()
            .map(|context| context.with_cache(cache))
    }
}

/// Walk the main agent's wire log newest-first for the last request's model,
/// its token split, and the latest measured context length.
fn observe_wire_tail(tail: &str) -> WireObservation {
    let mut observation = WireObservation::default();
    for line in tail.lines().rev() {
        if observation.model.is_some()
            && observation.usage.is_some()
            && observation.context_tokens.is_some()
        {
            break;
        }
        let wants_model = observation.model.is_none() && line.contains("\"llm.request\"");
        let wants_usage = observation.usage.is_none() && line.contains("\"usage.record\"");
        let wants_context =
            observation.context_tokens.is_none() && line.contains("\"token_counting.");
        if !(wants_model || wants_usage || wants_context) {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event
            .get("agentId")
            .and_then(Value::as_str)
            .is_some_and(|id| id != "main")
        {
            continue;
        }
        match event.get("type").and_then(Value::as_str) {
            Some("llm.request") if wants_model => {
                observation.model = event
                    .get("modelAlias")
                    .or_else(|| event.get("model"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Some("usage.record") if wants_usage => {
                let usage = event.get("usage");
                let count = |key: &str| {
                    usage
                        .and_then(|usage| usage.get(key))
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                };
                observation.usage = Some(TokenUsage {
                    fresh_input: count("inputOther"),
                    output: count("output"),
                    cache_read: count("inputCacheRead"),
                    cache_write: count("inputCacheCreation"),
                });
                if observation.model.is_none() {
                    observation.model = event
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            Some("token_counting.measured" | "token_counting.turn_recorded") if wants_context => {
                observation.context_tokens = event.get("tokens").and_then(Value::as_u64);
            }
            _ => {}
        }
    }
    observation
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const USAGES: &str = r#"{"usage":{"limit":"100","remaining":"100","resetTime":"2026-10-04T20:15:35.767686Z"},"limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},"detail":{"limit":"100","used":"2","remaining":"98","resetTime":"2026-10-02T22:15:35.767686Z"}}],"usages":{"limit_5h":{"used_ratio":0,"reset_time":"2026-10-02T22:15:35Z"},"limit_7d":{"used_ratio":0,"reset_time":"2026-10-04T20:15:35Z"}}}"#;

    #[test]
    fn usages_fill_five_hour_from_limits_and_weekly_from_usage() {
        let snapshot = parse_usages(&serde_json::from_str(USAGES).unwrap(), 1).unwrap();
        assert_eq!(snapshot.provider, Provider::Kimi);
        let five = snapshot.window(WindowKind::FiveHour).unwrap();
        assert_eq!(five.used_percent, 2.0);
        assert_eq!(
            five.resets_at,
            ResetAt::parse("2026-10-02T22:15:35.767686Z")
        );
        let weekly = snapshot.window(WindowKind::Weekly).unwrap();
        assert_eq!(weekly.used_percent, 0.0);
        assert!(weekly.resets_at.is_some());
    }

    #[test]
    fn a_window_of_another_length_is_not_placed_in_a_slot() {
        let value = serde_json::json!({
            "limits": [{"window": {"duration": 1, "timeUnit": "TIME_UNIT_DAY"},
                        "detail": {"limit": "10", "used": "5"}}]
        });
        assert!(parse_usages(&value, 1).is_err());
    }

    #[test]
    fn a_missing_count_is_not_a_full_allowance() {
        let value = serde_json::json!({"usage": {"limit": "100"}});
        assert!(parse_usages(&value, 1).is_err());
    }

    #[test]
    fn ratio_only_responses_still_parse() {
        let value = serde_json::json!({"usages": {"limit_5h": {"used_ratio": 0.25}}});
        let snapshot = parse_usages(&value, 1).unwrap();
        assert_eq!(
            snapshot.window(WindowKind::FiveHour).unwrap().used_percent,
            25.0
        );
    }

    #[test]
    fn account_pin_follows_the_user_not_the_token() {
        // {"user_id":"u1","exp":1} and {"user_id":"u1","exp":2}
        let first = "h.eyJ1c2VyX2lkIjoidTEiLCJleHAiOjF9.s";
        let rotated = "h.eyJ1c2VyX2lkIjoidTEiLCJleHAiOjJ9.s";
        // {"user_id":"u2"}
        let other = "h.eyJ1c2VyX2lkIjoidTIifQ.s";
        assert!(account_pin(first).is_some());
        assert_eq!(account_pin(first), account_pin(rotated));
        assert_ne!(account_pin(first), account_pin(other));
        assert_eq!(account_pin("not-a-jwt"), None);
    }

    #[test]
    fn sessions_publish_model_topic_and_context_from_kimi_files() {
        let home = tempdir().unwrap();
        let root = home.path();
        let session = "session_04af15c6-2df5-4134-9be6-09887f24425d";
        let dir = root.join("sessions/wd_demo_1").join(session);
        fs::create_dir_all(dir.join("agents/main")).unwrap();
        fs::write(
            root.join("config.toml"),
            "default_model = \"kimi-code/fast\"\n\
             [models.\"kimi-code/fast\"]\nmax_context_size = 200000\ndisplay_name = \"K2.7 Fast\"\n",
        )
        .unwrap();
        fs::write(
            root.join("session_index.jsonl"),
            format!(
                "{{\"sessionId\":\"{session}\",\"sessionDir\":\"{}\"}}\n\
                 {{\"sessionId\":\"session_evil\",\"sessionDir\":\"/etc\"}}\n",
                dir.display()
            ),
        )
        .unwrap();
        fs::write(
            dir.join("state.json"),
            r#"{"lastPrompt":"can you see\n the crm","title":"ignored"}"#,
        )
        .unwrap();
        fs::write(
            dir.join("agents/main/wire.jsonl"),
            concat!(
                r#"{"type":"llm.request","agentId":"main","model":"fast","modelAlias":"kimi-code/fast"}"#, "\n",
                r#"{"type":"usage.record","agentId":"main","usage":{"inputOther":1000,"output":100,"inputCacheRead":9000,"inputCacheCreation":0}}"#, "\n",
                r#"{"type":"token_counting.turn_recorded","agentId":"main","tokens":20000}"#, "\n",
                r#"{"type":"llm.request","agentId":"sub","modelAlias":"kimi-code/other"}"#, "\n",
            ),
        )
        .unwrap();

        let models = model_catalog(root);
        let mut snapshot = ProviderSnapshot::new(Provider::Kimi, vec![], 1);
        enrich_sessions_at(
            &mut snapshot,
            root,
            &[session.to_string(), "session_evil".to_string()],
            &models,
        );
        assert_eq!(snapshot.session_models.get(session).unwrap(), "K2.7 Fast");
        assert_eq!(
            snapshot.session_summaries.get(session).unwrap(),
            "can you see the crm"
        );
        let context = snapshot.session_contexts.get(session).unwrap();
        assert!((context.used_percent - 10.0).abs() < 1e-9, "{context:?}");
        let cache = context.cache.as_ref().unwrap();
        assert!((cache.hit_percent - 90.0).abs() < 1e-9, "{cache:?}");
        assert!(!snapshot.session_models.contains_key("session_evil"));
        assert_eq!(default_model(root).as_deref(), Some("kimi-code/fast"));
    }
}
