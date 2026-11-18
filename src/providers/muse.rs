//! Muse Code (Meta Muse Spark) subscription quota.
//!
//! Muse Code keeps its login in `~/.config/muse/auth.json` (or
//! `$XDG_CONFIG_HOME/muse/auth.json`, or `$MUSE_AUTH_PATH`, the same order its
//! launcher uses). A `storage: "keychain"` login (typical macOS `muse login`)
//! keeps the OAuth token out of that file; the collector then reads the CLI's
//! own Keychain item through `security find-generic-password`. Background
//! processes never prompt: without a recorded approval marker the keychain
//! branch is skipped outright, and the user approves once via
//! `refresh --provider muse --keychain-approve`. A successful token is kept
//! in-process so a watch pulse does not prompt Keychain for every Muse pane;
//! a failed lookup is not remembered. The quota is the `subs_usage`
//! block of the same `POST https://api.meta.ai/muse-code/key` call the CLI
//! makes at startup and for its `/usage` panel. That call is idempotent for a
//! signed-in account: it returns the key already stored for the login rather
//! than rotating it, so polling it cannot sign the CLI out. This is the
//! Grok/Devin pattern — local credential plus the official CLI contract — not
//! a browser scrape.
//!
//! The response also carries the account's API key, name, and email. Only
//! `subs_usage` is read; the rest is dropped with the response and never
//! reaches a snapshot, a log, or an error message. The access token is sent
//! only to the fixed Meta host, never to an `auth.json`-supplied URL.
//!
//! Everything fails closed. A missing or malformed window yields no window,
//! and the cache identity is `sha256("muse\0" || access token)` so another
//! login can never inherit the previous account's last-good snapshot. A pane
//! without a subscription (an API-key login, or an inactive plan) shows no
//! quota, never "0% used", but keeps its local session fields.
//!
//! The provider-level model is the CLI's default from `settings.json` `model`.
//! Herdr has no Muse session integration, so a pane's session is found through
//! Muse's own session lock (see [`session_ids_for_panes`]). That session's
//! `session.jsonl` tail gives the per-session model, context, cache, and the
//! last prompt. Muse has no generated session title, so that last prompt is
//! the topic. A pane without that evidence keeps the account quota and
//! nothing session-local.

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
use std::io::{IsTerminal, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

const SUBSCRIPTION_URL: &str = "https://api.meta.ai/muse-code/key";
/// The API version the Muse Code CLI sends with the same request.
const API_VERSION: &str = "1.0.0";
/// `auth.json` and `settings.json` are a few kilobytes. Anything larger is
/// not a file this collector understands.
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
/// Wall-clock budget for `security(1)`. A Keychain ACL prompt can block
/// forever; the Muse HTTP call already gives up at five seconds connect, and
/// a hung `security` would stall the watch pulse for every other provider.
const KEYCHAIN_COMMAND_BUDGET: Duration = Duration::from_secs(5);
const FIVE_HOUR_MINUTES: u64 = 5 * 60;
/// The last model call and the last prompt sit at the end of a transcript
/// that can grow to tens of megabytes, so only this much of it is read.
const SESSION_TAIL_BYTES: u64 = 2 * 1024 * 1024;
/// Session directories inspected per inventory read, newest day first.
const MAX_SESSION_DIRS: usize = 2048;
/// `.session.lock` holds `pid=<n>`.
const MAX_LOCK_BYTES: u64 = 64;
/// A process start time and a file mtime are both whole seconds. One second
/// absorbs that rounding; a lock older than this was written by an earlier
/// process that had the same pid.
const LOCK_CLOCK_SLACK_SECONDS: u64 = 1;
const MAX_SUMMARY_CHARS: usize = 200;

/// The OAuth login Muse Code stores under `providers.meta`.
#[derive(Clone)]
struct MuseCredentials {
    access_token: String,
}

impl std::fmt::Debug for MuseCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MuseCredentials")
            .field("access_token", &"[redacted]")
            .finish()
    }
}

/// Fetch Muse Code's subscription windows for the signed-in account, then
/// enrich the named sessions from their local transcripts.
pub fn fetch_for_sessions(session_ids: &[String]) -> Result<ProviderSnapshot> {
    let path = auth_path().context("resolve Muse Code auth path")?;
    let has_sessions = !session_ids.is_empty();
    let now = CacheStore::now_unix();
    let mut snapshot = match resolve_subscription(
        read_credentials(&path),
        has_sessions,
        now,
        fetch_subscription,
    ) {
        // A 401/403 maps to MissingCredentials — with a cached keychain token
        // that can mean the token rotated under us. Drop the cache and retry
        // once before believing the login is gone.
        Err(ProviderError::MissingCredentials) => {
            invalidate_credentials();
            resolve_subscription(
                read_credentials(&path),
                has_sessions,
                now,
                fetch_subscription,
            )
        }
        result => result,
    }
    .map_err(anyhow::Error::from)?
    .with_model(configured_model());
    if let Ok(data_dir) = muse_data_dir() {
        enrich_sessions_at(&mut snapshot, &data_dir, session_ids);
    }
    Ok(snapshot)
}

/// The quota snapshot for this refresh.
///
/// A Muse pane with no subscription still has local session diagnostics, so
/// two outcomes become a snapshot without quota windows instead of an error
/// (only when a Muse session is being refreshed, so nobody without Muse gets
/// an empty cached row):
/// - no account login is stored (an API-key login, or none), and
/// - the account has no active subscription.
///
/// A rejected token, a failed request, or an unreadable response stays an
/// error. That keeps the last cached quota for the same account instead of
/// replacing it with nothing.
fn resolve_subscription(
    credentials: std::result::Result<MuseCredentials, ProviderError>,
    has_sessions: bool,
    now: u64,
    fetch: impl FnOnce(&MuseCredentials, u64) -> std::result::Result<ProviderSnapshot, ProviderError>,
) -> std::result::Result<ProviderSnapshot, ProviderError> {
    let without_quota = || ProviderSnapshot::new(Provider::Muse, vec![], now);
    let credentials = match credentials {
        Ok(credentials) => credentials,
        Err(ProviderError::MissingCredentials) if has_sessions => return Ok(without_quota()),
        Err(error) => return Err(error),
    };
    let account_id = Some(account_pin(&credentials.access_token));
    match fetch(&credentials, now) {
        Ok(snapshot) => Ok(snapshot.with_account_id(account_id)),
        Err(ProviderError::Unavailable(_)) if has_sessions => {
            Ok(without_quota().with_account_id(account_id))
        }
        Err(error) => Err(error),
    }
}

/// One `muse-code/key` call, parsed down to `subs_usage`.
fn fetch_subscription(
    credentials: &MuseCredentials,
    now: u64,
) -> std::result::Result<ProviderSnapshot, ProviderError> {
    let agent = crate::providers::http_agent();
    let response = agent
        .post(SUBSCRIPTION_URL)
        .set(
            "Authorization",
            &format!("Bearer {}", credentials.access_token),
        )
        .set("x-api-version", API_VERSION)
        .set("Accept", "application/json")
        .set("Content-Type", "application/json")
        .send_string("{}")
        .map_err(|error| map_request_error(&error))?;
    let value: Value = response
        .into_json()
        .map_err(|_| ProviderError::UnsupportedResponse("Muse Code response is not JSON".into()))?;
    parse_subscription(&value, now)
}

/// Muse session ids for Herdr panes, keyed by pane id.
///
/// Herdr reports no session for a Muse pane, so the match goes through
/// evidence Muse itself writes: each `muse-bin` process inherits its pane's
/// `HERDR_PANE_ID`, and Muse records `pid=<n>` in the `.session.lock` of the
/// session that process owns. A lock only counts when it was written after
/// that process started, so a stale lock from an earlier process that had the
/// same pid is never inherited. When one process has held several sessions,
/// the most recently written transcript wins. Only `HERDR_PANE_ID` is taken
/// from a process environment. Without `/proc` (macOS) nothing resolves, and
/// the pane keeps its account quota without session-local fields.
pub fn session_ids_for_panes(pane_ids: &[String]) -> BTreeMap<String, String> {
    let Ok(data_dir) = muse_data_dir() else {
        return BTreeMap::new();
    };
    session_ids_for_panes_at(Path::new("/proc"), &data_dir.join("sessions"), pane_ids)
}

fn session_ids_for_panes_at(
    proc_root: &Path,
    sessions_root: &Path,
    pane_ids: &[String],
) -> BTreeMap<String, String> {
    if pane_ids.is_empty() {
        return BTreeMap::new();
    }
    let processes = muse_process_panes(proc_root, pane_ids);
    let Some(earliest_start) = processes.values().map(|process| process.started_at).min() else {
        return BTreeMap::new();
    };
    let mut newest: BTreeMap<String, (u64, String)> = BTreeMap::new();
    for session_dir in session_dirs(sessions_root) {
        let lock = session_dir.join(".session.lock");
        // A stat is enough to skip every lock older than all live processes.
        let Some(locked_at) = CacheStore::file_mtime_unix(&lock)
            .map(|mtime| mtime.saturating_add(LOCK_CLOCK_SLACK_SECONDS))
            .filter(|locked_at| *locked_at >= earliest_start)
        else {
            continue;
        };
        let Some(pane_id) = read_lock_pid(&lock)
            .and_then(|pid| processes.get(&pid))
            .filter(|process| locked_at >= process.started_at)
            .map(|process| &process.pane_id)
        else {
            continue;
        };
        let Some(session_id) = session_dir
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|id| is_session_id(id))
        else {
            continue;
        };
        let modified =
            CacheStore::file_mtime_unix(&session_dir.join("session.jsonl")).unwrap_or_default();
        if newest
            .get(pane_id)
            .is_none_or(|(current, _)| modified > *current)
        {
            newest.insert(pane_id.clone(), (modified, session_id.to_string()));
        }
    }
    newest
        .into_iter()
        .map(|(pane_id, (_, session_id))| (pane_id, session_id))
        .collect()
}

struct MuseProcess {
    pane_id: String,
    started_at: u64,
}

/// `muse-bin` processes running in one of the requested panes, by pid.
fn muse_process_panes(proc_root: &Path, pane_ids: &[String]) -> BTreeMap<u32, MuseProcess> {
    let mut processes = BTreeMap::new();
    let Some(boot_unix) = boot_time_unix(proc_root) else {
        return processes;
    };
    for entry in fs::read_dir(proc_root).into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let is_muse = fs::read_to_string(entry.path().join("comm"))
            .is_ok_and(|comm| comm.trim().starts_with("muse-bin"));
        if !is_muse {
            continue;
        }
        let Ok(environ) = fs::read(entry.path().join("environ")) else {
            continue;
        };
        let Some(pane_id) = pane_id_from_environ(&environ).filter(|pane| pane_ids.contains(pane))
        else {
            continue;
        };
        if let Some(started_at) = process_start_unix(&entry.path(), boot_unix) {
            processes.insert(
                pid,
                MuseProcess {
                    pane_id,
                    started_at,
                },
            );
        }
    }
    processes
}

fn boot_time_unix(proc_root: &Path) -> Option<u64> {
    fs::read_to_string(proc_root.join("stat"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}

/// Field 22 of `/proc/<pid>/stat` is the start time in clock ticks after
/// boot. The command name before it may contain spaces and parentheses, so
/// fields are counted after the last `)`, where field 3 is the first.
fn process_start_unix(process_dir: &Path, boot_unix: u64) -> Option<u64> {
    let stat = fs::read_to_string(process_dir.join("stat")).ok()?;
    let ticks: u64 = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    Some(boot_unix.saturating_add(ticks / clock_ticks_per_second()))
}

fn clock_ticks_per_second() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: sysconf takes no pointers and only reads a system constant.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if ticks > 0 {
            return ticks as u64;
        }
    }
    100
}

fn pane_id_from_environ(environ: &[u8]) -> Option<String> {
    environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(b"HERDR_PANE_ID="))
        .and_then(|value| std::str::from_utf8(value).ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn read_lock_pid(path: &Path) -> Option<u32> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_LOCK_BYTES {
        return None;
    }
    fs::read_to_string(path)
        .ok()?
        .trim()
        .strip_prefix("pid=")?
        .trim()
        .parse()
        .ok()
}

fn muse_data_dir() -> Result<PathBuf> {
    if let Some(xdg) = non_empty_env("XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg).join("muse"));
    }
    let home = non_empty_env("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share/muse"))
}

/// Session directories under `sessions/<yyyy>/<mm>/<dd>/`, newest day first.
fn session_dirs(sessions_root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for day in day_dirs(sessions_root) {
        for session in sorted_child_dirs(&day) {
            if dirs.len() >= MAX_SESSION_DIRS {
                return dirs;
            }
            dirs.push(session);
        }
    }
    dirs
}

fn day_dirs(sessions_root: &Path) -> Vec<PathBuf> {
    sorted_child_dirs(sessions_root)
        .iter()
        .flat_map(|year| sorted_child_dirs(year))
        .flat_map(|month| sorted_child_dirs(&month))
        .collect()
}

/// Child directories in descending name order; dot directories are Muse's
/// own bookkeeping, not sessions.
fn sorted_child_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut children: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .collect();
    children.sort_by(|left, right| right.cmp(left));
    children
}

/// Session ids are one path segment: a UUID today. Anything else is never
/// joined onto a path.
fn is_session_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn find_session_log(day_dirs: &[PathBuf], session_id: &str) -> Option<PathBuf> {
    if !is_session_id(session_id) {
        return None;
    }
    day_dirs
        .iter()
        .map(|day| day.join(session_id).join("session.jsonl"))
        .find(|log| log.is_file())
}

fn enrich_sessions_at(snapshot: &mut ProviderSnapshot, data_dir: &Path, session_ids: &[String]) {
    if session_ids.is_empty() {
        return;
    }
    let limits = context_limits(&data_dir.join("model-catalog"));
    let days = day_dirs(&data_dir.join("sessions"));
    for session_id in session_ids {
        let Some(tail) =
            find_session_log(&days, session_id).and_then(|log| read_tail(&log, SESSION_TAIL_BYTES))
        else {
            continue;
        };
        let observation = observe_session_tail(&tail);
        if let Some(context) = observation.context(session_id, &limits) {
            snapshot
                .session_contexts
                .insert(session_id.clone(), context);
        }
        if let Some(model) = observation.model {
            snapshot.session_models.insert(session_id.clone(), model);
        }
        if let Some(prompt) = observation.prompt {
            snapshot
                .session_summaries
                .insert(session_id.clone(), prompt);
        }
    }
}

/// `model_id` → `context_limit` from Muse's local model catalog.
fn context_limits(catalog_dir: &Path) -> BTreeMap<String, u64> {
    let mut limits = BTreeMap::new();
    for entry in fs::read_dir(catalog_dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(catalog) = read_bounded_json(&path) else {
            continue;
        };
        for row in catalog
            .get("rows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let model = json_text(row.get("model_id"));
            let limit = row.get("context_limit").and_then(Value::as_u64);
            if let (Some(model), Some(limit)) = (model, limit) {
                limits.insert(model, limit);
            }
        }
    }
    limits
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
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SessionObservation {
    model: Option<String>,
    usage: Option<TokenUsage>,
    prompt: Option<String>,
}

impl SessionObservation {
    /// Context is the last request's prompt plus its reply, against the
    /// model's catalog window. Cache is that request's read share. Muse
    /// publishes no prompt-cache lifetime, so no TTL is estimated.
    fn context(&self, session_id: &str, limits: &BTreeMap<String, u64>) -> Option<ContextUsage> {
        let usage = self.usage?;
        let limit = *limits.get(self.model.as_deref()?)?;
        if limit == 0 {
            return None;
        }
        let used = usage.input.saturating_add(usage.output) as f64 / limit as f64 * 100.0;
        let fresh = usage.input.saturating_sub(usage.cache_read);
        let cache = CacheUsage::from_token_counts(fresh, usage.cache_read, usage.cache_write).map(
            |cache| {
                let totals = CacheTotals::from_token_counts(
                    cache.fresh_input_tokens,
                    cache.read_tokens,
                    cache.creation_tokens,
                );
                cache.with_session_totals(totals, session_id, 0)
            },
        );
        ContextUsage::new(used.clamp(0.0, 100.0))
            .ok()
            .map(|context| context.with_cache(cache))
    }
}

/// Walk the tail newest-first for the last completed model call and the last
/// prompt the user submitted. Lines are only parsed when they name one of the
/// event kinds that carry either.
///
/// A typed prompt is recorded as a `runtime.user_intent.accepted` chat intent
/// on the main surface; `user_prompt_display` appears only for some submits.
fn observe_session_tail(tail: &str) -> SessionObservation {
    let mut observation = SessionObservation::default();
    for line in tail.lines().rev() {
        if observation.usage.is_some() && observation.prompt.is_some() {
            break;
        }
        let wants_usage = observation.usage.is_none() && line.contains("\"model_completed\"");
        let wants_prompt = observation.prompt.is_none()
            && (line.contains("\"runtime.user_intent.accepted\"")
                || line.contains("\"user_prompt_display\""));
        if !wants_usage && !wants_prompt {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record.get("payload_type").and_then(Value::as_str)
            == Some("runtime.user_intent.accepted")
        {
            if wants_prompt {
                observation.prompt = record.get("payload").and_then(chat_intent_prompt);
            }
            continue;
        }
        let Some(event) = record.pointer("/payload/event") else {
            continue;
        };
        match event.get("kind").and_then(Value::as_str) {
            Some("model_completed") if wants_usage => {
                observation.usage = parse_token_usage(event.get("usage"));
                if observation.usage.is_some() {
                    observation.model = json_text(event.get("model"));
                }
            }
            Some("user_prompt_display") if wants_prompt => {
                observation.prompt = event
                    .get("prompt")
                    .and_then(Value::as_str)
                    .and_then(summary_line);
            }
            _ => {}
        }
    }
    observation
}

/// `input_tokens` already includes the cache read, as the recorded calls show
/// (`cache_read_tokens <= input_tokens`).
fn parse_token_usage(value: Option<&Value>) -> Option<TokenUsage> {
    let usage = value?.as_object()?;
    let count = |name: &str| usage.get(name).and_then(Value::as_u64);
    let input = count("input_tokens")?;
    let cache_read = count("cache_read_tokens")
        .or_else(|| count("cached_tokens"))
        .unwrap_or_default();
    if cache_read > input {
        return None;
    }
    Some(TokenUsage {
        input,
        output: count("output_tokens").unwrap_or_default(),
        cache_read,
        cache_write: count("cache_write_tokens").unwrap_or_default(),
    })
}

/// The text of a chat intent the user submitted in the main session. Other
/// surfaces and intent kinds (side chats, commands, automated wakes) are not
/// what the pane is working on.
fn chat_intent_prompt(payload: &Value) -> Option<String> {
    if payload
        .get("surface")
        .and_then(Value::as_str)
        .is_some_and(|surface| surface != "main")
    {
        return None;
    }
    if payload
        .pointer("/semantic_kind/kind")
        .and_then(Value::as_str)
        != Some("chat")
    {
        return None;
    }
    let blocks = payload
        .get("refill_blocks")
        .and_then(Value::as_array)
        .or_else(|| payload.pointer("/model_messages/0/content")?.as_array())?;
    blocks
        .iter()
        .filter(|block| block.get("kind").and_then(Value::as_str) == Some("text"))
        .find_map(|block| {
            block
                .get("text")
                .and_then(Value::as_str)
                .and_then(summary_line)
        })
}

fn summary_line(prompt: &str) -> Option<String> {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    Some(line.chars().take(MAX_SUMMARY_CHARS).collect())
}

fn json_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// `$MUSE_AUTH_PATH`, then `$XDG_CONFIG_HOME/muse/auth.json`, then
/// `~/.config/muse/auth.json` — the Muse Code launcher's own order.
pub fn auth_path() -> Result<PathBuf> {
    if let Some(path) = non_empty_env("MUSE_AUTH_PATH") {
        return Ok(PathBuf::from(path));
    }
    Ok(muse_config_dir()?.join("auth.json"))
}

/// Stable cache identity for the stored login. The token itself never enters
/// the snapshot; a different login is a different account.
pub fn current_account_id() -> Option<String> {
    let credentials = read_credentials(&auth_path().ok()?).ok()?;
    Some(account_pin(&credentials.access_token))
}

pub fn auth_mtime_unix() -> Option<u64> {
    CacheStore::file_mtime_unix(&auth_path().ok()?)
}

/// `sha256("muse\0" || trimmed access token)`. Pinned in tests so a hash
/// change is a test failure rather than a silent last-good leak.
fn account_pin(access_token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"muse\0");
    hasher.update(access_token.trim().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn muse_config_dir() -> Result<PathBuf> {
    if let Some(xdg) = non_empty_env("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("muse"));
    }
    let home = non_empty_env("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/muse"))
}

fn non_empty_env(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn read_bounded_json(path: &Path) -> Option<Value> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return None;
    }
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Wall-clock budget for an interactive `--keychain-approve` read. The user
/// is watching and approving, so waiting minutes beats the background budget
/// no human can approve inside of.
const KEYCHAIN_APPROVE_BUDGET: Duration = Duration::from_secs(300);
/// A keychain read that returns within this long showed no prompt — the
/// grant is already trusted. Anything slower means the user handled a
/// prompt, which says nothing about which button they clicked.
const KEYCHAIN_NO_PROMPT_THRESHOLD: Duration = Duration::from_secs(2);

/// Set by `refresh --keychain-approve`: this process may attempt a keychain
/// read even without the approval marker below (and waits for the user).
static KEYCHAIN_APPROVE_ATTEMPT: AtomicBool = AtomicBool::new(false);

/// Enable a keychain approval attempt for this process. Called once from
/// `refresh --keychain-approve`; event hooks, the daemon, and plain refreshes
/// never set it, so background processes can never trigger a prompt.
pub fn set_keychain_approve_attempt() {
    KEYCHAIN_APPROVE_ATTEMPT.store(true, Ordering::Relaxed);
}

#[cfg(test)]
fn set_keychain_approve_attempt_for_test(value: bool) {
    KEYCHAIN_APPROVE_ATTEMPT.store(value, Ordering::Relaxed);
}

/// Marker proving a keychain read succeeded at least once — i.e. macOS trusts
/// this lookup and future reads return instantly without prompting. Anchored
/// beside the Muse config dir, not the plugin state dir: `CacheStore::from_env`
/// resolves differently under `HERDR_PLUGIN_STATE_DIR` (herdr hooks) versus a
/// plain terminal (where the user runs `--keychain-approve`), and a marker only
/// one side can see splits the approval. `muse_config_dir()` is env-stable.
fn keychain_approval_marker() -> Option<PathBuf> {
    muse_config_dir()
        .ok()
        .map(|dir| dir.join(".herdr-keychain-approved"))
}

/// The approval marker's mtime, for the credential cache key: creating the
/// marker must invalidate a cached skip so the daemon re-attempts the keychain
/// on its next poll without a restart.
fn keychain_approval_mtime() -> Option<u64> {
    fs::metadata(keychain_approval_marker()?)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// Record a successful keychain read. The content is the approval time, for
/// debugging only; existence of the file is the signal. Written once: the
/// marker's mtime is part of the credential cache key, so re-writing it on
/// every success would invalidate the cache on every poll. Returns whether
/// the marker exists afterward, so callers can gate their success message on
/// the approval actually persisting.
fn record_keychain_approval() -> bool {
    let Some(marker) = keychain_approval_marker() else {
        return false;
    };
    if let Some(parent) = marker.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // `create_new` so two concurrent approvers cannot race a partial
    // write; the content is idempotent either way.
    if let Ok(mut file) = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        use std::io::Write;
        let _ = write!(file, "{}", CacheStore::now_unix());
    }
    marker.exists()
}

/// Credentials for the current auth file, cached in-process.
///
/// The keychain fallback below shells out to `security(1)`, and every call
/// from this unsigned binary can trigger a macOS keychain ACL prompt. The
/// watch daemon would otherwise prompt once per poll per pane — a storm the
/// user cannot approve away. So background processes never prompt: without
/// the approval marker (see [`record_keychain_approval`]) the keychain branch
/// is skipped outright, and the user approves once via
/// `refresh --provider muse --keychain-approve`. Caching keyed on the file's
/// identity (path, mtime, length) plus the marker's mtime bounds `security`
/// to one successful call per daemon lifetime while still picking up an
/// approval without a restart; a 401 from the subscription API busts the
/// cache once so a rotated token still self-heals (`fetch_for_sessions`).
/// Failed lookups are not stored, so a Keychain prompt that timed out is
/// retried next refresh.
fn read_credentials(path: &Path) -> std::result::Result<MuseCredentials, ProviderError> {
    // The keychain helper binary is part of the lookup's identity: tests
    // stub it through the environment, and a cached result must not outlive
    // the stub it was read with.
    let security_bin = std::env::var_os("HERDR_AGENT_QUOTA_SECURITY_BIN");
    let key = fs::metadata(path).ok().and_then(|metadata| {
        Some((
            path.to_path_buf(),
            metadata
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_secs(),
            metadata.len(),
            security_bin.clone(),
            keychain_approval_mtime(),
        ))
    });
    let mut cache = CREDENTIAL_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some((cached_key, credentials)) = cache.as_ref() {
        if Some(cached_key) == key.as_ref() {
            return Ok(credentials.clone());
        }
    }
    // Hold the lock across the uncached lookup so two panes in one pass
    // cannot each spawn `security` and each raise a Keychain prompt.
    let credentials = read_credentials_uncached(path);
    if let (Some(key), Ok(credentials)) = (key, &credentials) {
        *cache = Some((key, credentials.clone()));
    }
    credentials
}

/// Forget the cached credentials so the next read re-runs the keychain
/// lookup. Called when the subscription API rejects the cached token.
fn invalidate_credentials() {
    *CREDENTIAL_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = None;
}

/// Cache key: the auth file's identity (path, mtime, length) plus the
/// keychain helper override, so a cached result never outlives the stub it
/// was read with, plus the approval marker's mtime, so recording an
/// approval invalidates a cached skip.
type CredentialCacheKey = (PathBuf, u64, u64, Option<std::ffi::OsString>, Option<u64>);

static CREDENTIAL_CACHE: LazyLock<Mutex<Option<(CredentialCacheKey, MuseCredentials)>>> =
    LazyLock::new(|| Mutex::new(None));

/// Interactive approval (`refresh --keychain-approve`): run the lookup with
/// a human-scale budget, then record the marker only if the grant provably
/// stuck. A one-time Allow also succeeds — but recording it would poison
/// the daemon into re-prompting, so a slow success works for this run only
/// and says so. A failed attempt never clears an existing marker: deny and
/// timeout are indistinguishable, and a transient failure must not revoke a
/// grant that already worked.
///
/// The `eprintln!` calls are a deliberate exception to the provider layer's
/// no-printing convention: this path only runs under an explicit CLI flag
/// with a human watching, and the approval flow needs progress output.
fn approve_keychain_interactive() -> Option<MuseCredentials> {
    // No human can see the prompt or the guidance without a terminal —
    // refuse rather than burn the 300s budget on a read nobody can approve.
    // Tests stub `security` and have no tty, so the check is skipped there.
    #[cfg(not(test))]
    if !std::io::stderr().is_terminal() {
        return None;
    }
    eprintln!(
        "muse: macOS will prompt for keychain access — click Always Allow (not Allow). You have up to 5 minutes."
    );
    let started = Instant::now();
    let Some(access_token) = read_keychain_access_token(KEYCHAIN_APPROVE_BUDGET) else {
        eprintln!("muse: keychain approval denied or timed out — no changes made.");
        return None;
    };
    let credentials = MuseCredentials { access_token };
    if started.elapsed() < KEYCHAIN_NO_PROMPT_THRESHOLD {
        // No prompt appeared: a persistent grant already covers this
        // lookup, so future background reads are safe to attempt.
        if record_keychain_approval() {
            eprintln!("muse: keychain approval recorded — future refreshes won't prompt.");
        } else {
            eprintln!(
                "muse: keychain read works, but the approval marker could not be written — future refreshes will prompt again."
            );
        }
        return Some(credentials);
    }
    // A prompt was handled; only a second instant read proves Always Allow
    // stuck. If this one prompts, the first was one-time — let it expire.
    eprintln!("muse: verifying the grant stuck (no prompt expected)...");
    let verified = Instant::now();
    if read_keychain_access_token(KEYCHAIN_COMMAND_BUDGET).is_some()
        && verified.elapsed() < KEYCHAIN_NO_PROMPT_THRESHOLD
    {
        if record_keychain_approval() {
            eprintln!("muse: Always Allow confirmed — future refreshes won't prompt.");
        } else {
            eprintln!(
                "muse: Always Allow confirmed, but the approval marker could not be written — future refreshes will prompt again."
            );
        }
    } else {
        eprintln!(
            "muse: that approval didn't stick (one-time Allow?). Continuing with one-time access; re-run with --keychain-approve and click Always Allow to stop future prompts."
        );
    }
    Some(credentials)
}

fn read_credentials_uncached(path: &Path) -> std::result::Result<MuseCredentials, ProviderError> {
    let value = read_bounded_json(path).ok_or(ProviderError::MissingCredentials)?;
    match credentials_from_auth(&value) {
        Ok(credentials) => Ok(credentials),
        Err(error) => {
            // `storage: "keychain"` logins keep the OAuth token out of the
            // file entirely; the CLI reads it from macOS Keychain instead.
            if auth_uses_keychain(&value) {
                let force = KEYCHAIN_APPROVE_ATTEMPT.load(Ordering::Relaxed);
                if force {
                    // A failed ceremony is Unavailable, not MissingCredentials:
                    // the latter would either wipe the last-good snapshot
                    // (has_sessions maps it to Ok empty) or re-run the whole
                    // 300s ceremony via the MissingCredentials retry arm.
                    return approve_keychain_interactive().ok_or_else(|| {
                        ProviderError::Unavailable(
                            "keychain approval denied or timed out".to_string(),
                        )
                    });
                }
                if keychain_approval_mtime().is_none() {
                    // No prompt from background processes, ever: without a
                    // recorded approval the lookup would only flash a prompt
                    // no human can approve inside of the background budget.
                    // The skip is a distinct error — not MissingCredentials —
                    // so `resolve_subscription` propagates it and refresh
                    // keeps the last-good snapshot instead of overwriting it
                    // with an empty one.
                    if std::io::stderr().is_terminal() {
                        eprintln!(
                            "muse: macOS Keychain approval needed — run `{}` and click Always Allow (not Allow) on the prompt.",
                            crate::identity::keychain_approve_command("muse")
                        );
                    }
                    return Err(ProviderError::Unavailable(format!(
                        "macOS Keychain approval needed — run `{}`",
                        crate::identity::keychain_approve_command("muse")
                    )));
                }
                match read_keychain_access_token(KEYCHAIN_COMMAND_BUDGET) {
                    Some(access_token) => Ok(MuseCredentials { access_token }),
                    // A failed read — timeout, hiccup, or an explicit Deny —
                    // is indistinguishable here, so the marker stays: the
                    // next poll retries, and a transient failure cannot
                    // permanently de-authorize background reads.
                    None => Err(error),
                }
            } else {
                Err(error)
            }
        }
    }
}

/// Whether the auth file declares `providers.meta.storage: "keychain"`.
fn auth_uses_keychain(value: &Value) -> bool {
    value
        .get("providers")
        .and_then(|providers| providers.get("meta"))
        .and_then(|meta| meta.get("storage"))
        .and_then(Value::as_str)
        == Some("keychain")
}

/// The OAuth token for a `storage: "keychain"` login, from the item the CLI
/// itself writes (`ai.meta.dev.credentials`, account `meta`). `security(1)`
/// is the documented interface — no keychain crate, and on platforms without
/// it the command simply fails and the login reports as missing.
fn read_keychain_access_token(budget: Duration) -> Option<String> {
    let executable =
        std::env::var_os("HERDR_AGENT_QUOTA_SECURITY_BIN").unwrap_or_else(|| "security".into());
    let mut command = Command::new(executable);
    command
        .args([
            "find-generic-password",
            "-s",
            "ai.meta.dev.credentials",
            "-a",
            "meta",
            "-w",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let stdout = run_command_with_deadline(&mut command, budget)?;
    if stdout.len() as u64 > MAX_CONFIG_BYTES {
        return None;
    }
    let value: Value = serde_json::from_slice(&stdout).ok()?;
    value
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// Return stdout if `command` exits successfully within `budget`. A timeout
/// kills the process group so a Keychain ACL prompt (or a helper it started)
/// cannot block the caller.
fn run_command_with_deadline(command: &mut Command, budget: Duration) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout
            .take(MAX_CONFIG_BYTES.saturating_add(1))
            .read_to_end(&mut buf);
        buf
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < budget => thread::sleep(Duration::from_millis(20)),
            _ => {
                terminate_command(&mut child);
                let _ = reader.join();
                return None;
            }
        }
    };
    let stdout = reader.join().ok()?;
    status.success().then_some(stdout)
}

fn terminate_command(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        let _ = libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Only an OAuth account login has a subscription. An API-key login bills
/// usage to the Model API instead, so it has no windows to report.
fn credentials_from_auth(value: &Value) -> std::result::Result<MuseCredentials, ProviderError> {
    let meta = value
        .get("providers")
        .and_then(|providers| providers.get("meta"))
        .ok_or(ProviderError::MissingCredentials)?;
    if meta.get("mechanism").and_then(Value::as_str) != Some("oauth") {
        return Err(ProviderError::MissingCredentials);
    }
    let access_token = meta
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or(ProviderError::MissingCredentials)?;
    Ok(MuseCredentials {
        access_token: access_token.to_string(),
    })
}

/// `settings.json` `model`, the default model for new Muse Code sessions.
fn configured_model() -> Option<String> {
    configured_model_from(&read_bounded_json(
        &muse_config_dir().ok()?.join("settings.json"),
    )?)
}

fn configured_model_from(settings: &Value) -> Option<String> {
    settings
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

/// 401/403 means the stored login is no longer valid. The token and the
/// response body are never included.
fn map_request_error(error: &ureq::Error) -> ProviderError {
    match error {
        ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => {
            ProviderError::MissingCredentials
        }
        ureq::Error::Status(code, _) => ProviderError::Request(format!("HTTP {code}")),
        ureq::Error::Transport(error) => ProviderError::Request(error.kind().to_string()),
    }
}

/// Parse the `subs_usage` block of a `muse-code/key` response.
///
/// `window` is the rolling session window (300 minutes today) and `weekly`
/// the calendar week. Both carry a used percentage and a Unix-seconds reset.
pub fn parse_subscription(
    value: &Value,
    now: u64,
) -> std::result::Result<ProviderSnapshot, ProviderError> {
    if value.get("is_subs_active").and_then(Value::as_bool) == Some(false) {
        return Err(ProviderError::Unavailable(
            "no active Muse Code subscription".to_string(),
        ));
    }
    let usage = value
        .get("subs_usage")
        .filter(|usage| usage.is_object())
        .ok_or_else(|| ProviderError::UnsupportedResponse("missing subs_usage".to_string()))?;

    let mut windows = Vec::new();
    if let Some(window) = parse_rolling_window(usage.get("window"))? {
        windows.push(window);
    }
    if let Some(window) = parse_window(usage.get("weekly"), WindowKind::Weekly)? {
        windows.push(window);
    }
    if windows.is_empty() {
        return Err(ProviderError::UnsupportedResponse(
            "no readable quota windows in Muse Code response".to_string(),
        ));
    }
    Ok(ProviderSnapshot::new(Provider::Muse, windows, now))
}

/// The rolling window takes the 5h slot. A different advertised length keeps
/// its own label (`8h`, `1d`) instead of being published as 5h.
fn parse_rolling_window(
    value: Option<&Value>,
) -> std::result::Result<Option<UsageWindow>, ProviderError> {
    let Some(window) = parse_window(value, WindowKind::FiveHour)? else {
        return Ok(None);
    };
    let minutes = value
        .and_then(|value| value.get("window_duration_mins"))
        .and_then(Value::as_u64)
        .filter(|minutes| *minutes > 0);
    Ok(Some(match minutes {
        Some(minutes) if minutes != FIVE_HOUR_MINUTES => {
            window.with_source_window(duration_label(minutes), Some(minutes.saturating_mul(60)))
        }
        _ => window,
    }))
}

fn parse_window(
    value: Option<&Value>,
    kind: WindowKind,
) -> std::result::Result<Option<UsageWindow>, ProviderError> {
    let Some(used) = value
        .and_then(|value| value.get("used_percent"))
        .and_then(Value::as_f64)
    else {
        return Ok(None);
    };
    let reset = value
        .and_then(|value| value.get("resets_at"))
        .and_then(parse_reset);
    UsageWindow::new(kind, used, reset)
        .map(Some)
        .map_err(|error| ProviderError::UnsupportedResponse(error.to_string()))
}

fn parse_reset(value: &Value) -> Option<ResetAt> {
    match value {
        Value::Number(number) => number.as_u64().map(ResetAt::from_unix_seconds),
        Value::String(text) => ResetAt::parse(text.trim()),
        _ => None,
    }
}

fn duration_label(minutes: u64) -> String {
    if minutes.is_multiple_of(24 * 60) {
        format!("{}d", minutes / (24 * 60))
    } else if minutes.is_multiple_of(60) {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    /// Serializes tests that mutate the process-global credential environment,
    /// including `HERDR_AGENT_QUOTA_SECURITY_BIN` and `XDG_CONFIG_HOME`.
    fn security_env_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::providers::test_support::env_guard()
    }

    /// Point `muse_config_dir` at a tempdir and return the approval marker's
    /// path inside it. The marker lives beside the Muse config dir (not the
    /// plugin state dir), so tests drive it through `XDG_CONFIG_HOME`.
    fn keychain_marker_in(config_home: &std::path::Path) -> PathBuf {
        std::env::set_var("XDG_CONFIG_HOME", config_home);
        config_home.join("muse").join(".herdr-keychain-approved")
    }

    /// Reset `KEYCHAIN_APPROVE_ATTEMPT` on drop so a panicking test cannot
    /// leak a 300s-budget approval attempt into the next test.
    struct KeychainApproveGuard;
    impl KeychainApproveGuard {
        fn new(value: bool) -> Self {
            set_keychain_approve_attempt_for_test(value);
            Self
        }
    }
    impl Drop for KeychainApproveGuard {
        fn drop(&mut self) {
            set_keychain_approve_attempt_for_test(false);
        }
    }

    fn power_fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/muse/subscription-power.json"
        ))
        .expect("fixture is valid JSON")
    }

    #[test]
    fn power_fixture_maps_the_rolling_and_weekly_windows() {
        let snapshot = parse_subscription(&power_fixture(), 1).expect("snapshot");
        assert_eq!(snapshot.provider, Provider::Muse);
        assert_eq!(snapshot.windows.len(), 2);

        let rolling = snapshot.window(WindowKind::FiveHour).expect("5h window");
        assert_eq!(rolling.used_percent, 4.0);
        assert_eq!(rolling.remaining_percent, 96.0);
        assert_eq!(rolling.display_label(), "5h");
        assert_eq!(
            rolling.resets_at.map(ResetAt::unix_seconds),
            Some(1_789_068_250)
        );

        let weekly = snapshot.window(WindowKind::Weekly).expect("weekly window");
        assert_eq!(weekly.used_percent, 28.0);
        assert_eq!(
            weekly.resets_at.map(ResetAt::unix_seconds),
            Some(1_789_344_000)
        );
    }

    #[test]
    fn the_fixture_carries_no_credentials_or_identity() {
        let text = include_str!("../../tests/fixtures/muse/subscription-power.json");
        for field in ["api_key", "user_email", "user_full_name"] {
            assert!(
                power_fixture().get(field).is_none_or(Value::is_null),
                "{field} must stay out of the fixture"
            );
        }
        assert!(!text.contains('@'));
    }

    #[test]
    fn a_different_rolling_length_keeps_its_own_label() {
        let value = json!({"subs_usage": {"window": {
            "used_percent": 10, "window_duration_mins": 480, "resets_at": 5
        }}});
        let snapshot = parse_subscription(&value, 1).unwrap();
        let window = snapshot.window(WindowKind::FiveHour).unwrap();
        assert_eq!(window.display_label(), "8h");
        assert_eq!(window.duration_seconds, Some(8 * 3600));
    }

    #[test]
    fn an_inactive_or_missing_subscription_reports_no_windows() {
        let inactive = json!({"is_subs_active": false, "subs_usage": null});
        assert!(matches!(
            parse_subscription(&inactive, 1),
            Err(ProviderError::Unavailable(_))
        ));
        assert!(matches!(
            parse_subscription(&json!({"is_subs_active": true}), 1),
            Err(ProviderError::UnsupportedResponse(_))
        ));
        assert!(matches!(
            parse_subscription(&json!({"subs_usage": {"window": {}, "weekly": {}}}), 1),
            Err(ProviderError::UnsupportedResponse(_))
        ));
    }

    #[test]
    fn an_out_of_range_percentage_is_rejected_not_clamped() {
        let value = json!({"subs_usage": {"weekly": {"used_percent": 140, "resets_at": 5}}});
        assert!(matches!(
            parse_subscription(&value, 1),
            Err(ProviderError::UnsupportedResponse(_))
        ));
    }

    #[test]
    fn only_an_oauth_login_is_a_subscription_credential() {
        let oauth = json!({"providers": {"meta": {
            "mechanism": "oauth", "access_token": " token ", "api_key": "key"
        }}});
        assert_eq!(credentials_from_auth(&oauth).unwrap().access_token, "token");
        for value in [
            json!({"providers": {"meta": {"mechanism": "api_key", "api_key": "key"}}}),
            json!({"providers": {"meta": {"mechanism": "oauth", "access_token": ""}}}),
            json!({"providers": {}}),
        ] {
            assert!(matches!(
                credentials_from_auth(&value),
                Err(ProviderError::MissingCredentials)
            ));
        }
    }

    #[test]
    fn a_missing_or_oversized_auth_file_is_missing_credentials() {
        let dir = tempdir().unwrap();
        assert!(matches!(
            read_credentials(&dir.path().join("auth.json")),
            Err(ProviderError::MissingCredentials)
        ));
        let big = dir.path().join("big.json");
        fs::write(&big, vec![b' '; (MAX_CONFIG_BYTES + 1) as usize]).unwrap();
        assert!(matches!(
            read_credentials(&big),
            Err(ProviderError::MissingCredentials)
        ));
    }

    #[test]
    fn only_a_keychain_storage_declaration_reads_the_keychain() {
        for (value, expected) in [
            (
                json!({"providers": {"meta": {"storage": "keychain"}}}),
                true,
            ),
            (json!({"providers": {"meta": {"storage": "file"}}}), false),
            (json!({"providers": {"meta": {}}}), false),
            (json!({"providers": {}}), false),
        ] {
            assert_eq!(auth_uses_keychain(&value), expected);
        }
    }

    /// The whole keychain path, against a `security` stub that answers with a
    /// token payload: a keychain-storage auth file with no inline token has to
    /// resolve through the keychain once approved, and a file-storage one must
    /// not.
    #[test]
    fn a_keychain_login_reads_its_token_from_the_keychain() {
        let _guard = security_env_guard();
        set_keychain_approve_attempt_for_test(false);
        let config_home = tempdir().unwrap();
        let marker = keychain_marker_in(config_home.path());
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, "1").unwrap();
        let dir = tempdir().unwrap();
        let stub = dir.path().join("security-stub");
        fs::write(
            &stub,
            "#!/bin/sh\nprintf '%s' '{\"access_token\":\" keychain-token \"}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("HERDR_AGENT_QUOTA_SECURITY_BIN", &stub);

        let keychain_auth = dir.path().join("auth.json");
        fs::write(
            &keychain_auth,
            r#"{"providers":{"meta":{"mechanism":"oauth","storage":"keychain"}}}"#,
        )
        .unwrap();
        assert_eq!(
            read_credentials(&keychain_auth).unwrap().access_token,
            "keychain-token"
        );
        // A success must not rewrite the marker: its mtime is cache-key
        // material, and churning it would re-run `security` every poll.
        assert_eq!(fs::read_to_string(&marker).unwrap(), "1");

        let file_auth = dir.path().join("auth-file.json");
        fs::write(
            &file_auth,
            r#"{"providers":{"meta":{"mechanism":"oauth","storage":"file"}}}"#,
        )
        .unwrap();
        std::env::remove_var("HERDR_AGENT_QUOTA_SECURITY_BIN");
        assert!(matches!(
            read_credentials(&file_auth),
            Err(ProviderError::MissingCredentials)
        ));
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    /// A failed `security` lookup must not stick: the first call is allowed to
    /// miss (a Keychain prompt that timed out), the next refresh has to try
    /// again, and only a successful token is reused without spawning.
    #[test]
    fn a_failed_keychain_lookup_is_retried_and_a_success_is_reused() {
        let _guard = security_env_guard();
        invalidate_credentials();
        let config_home = tempdir().unwrap();
        let marker = keychain_marker_in(config_home.path());
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, "1").unwrap();
        let dir = tempdir().unwrap();
        let count = dir.path().join("calls");
        let stub = dir.path().join("security-stub");
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\n\
                 count_file='{count}'\n\
                 n=0\n\
                 [ -f \"$count_file\" ] && n=$(cat \"$count_file\")\n\
                 n=$((n + 1))\n\
                 printf '%s\\n' \"$n\" > \"$count_file\"\n\
                 [ \"$n\" -eq 1 ] && exit 1\n\
                 printf '%s' '{{\"access_token\":\"cached-token\"}}'\n",
                count = count.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("HERDR_AGENT_QUOTA_SECURITY_BIN", &stub);

        let auth = dir.path().join("auth.json");
        fs::write(
            &auth,
            r#"{"providers":{"meta":{"mechanism":"oauth","storage":"keychain"}}}"#,
        )
        .unwrap();
        assert!(matches!(
            read_credentials(&auth),
            Err(ProviderError::MissingCredentials)
        ));
        assert_eq!(
            read_credentials(&auth).unwrap().access_token,
            "cached-token"
        );
        assert_eq!(
            read_credentials(&auth).unwrap().access_token,
            "cached-token"
        );
        assert_eq!(fs::read_to_string(&count).unwrap().trim(), "2");

        invalidate_credentials();
        assert_eq!(
            read_credentials(&auth).unwrap().access_token,
            "cached-token"
        );
        std::env::remove_var("HERDR_AGENT_QUOTA_SECURITY_BIN");
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn a_keychain_prompt_that_does_not_return_is_abandoned() {
        let _guard = security_env_guard();
        let dir = tempdir().unwrap();
        let stub = dir.path().join("security-stub");
        fs::write(&stub, "#!/bin/sh\nexec sleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("HERDR_AGENT_QUOTA_SECURITY_BIN", &stub);
        let started = Instant::now();
        assert!(read_keychain_access_token(Duration::from_millis(200)).is_none());
        std::env::remove_var("HERDR_AGENT_QUOTA_SECURITY_BIN");
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    /// Without a recorded approval, background reads skip the keychain
    /// without spawning `security` at all: no subprocess, no prompt.
    #[test]
    fn keychain_reads_are_skipped_without_an_approval() {
        let _guard = security_env_guard();
        set_keychain_approve_attempt_for_test(false);
        let config_home = tempdir().unwrap();
        let marker = keychain_marker_in(config_home.path());
        let dir = tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let stub = dir.path().join("security-stub");
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho called >> '{}'\nprintf '%s' '{{\"access_token\":\" keychain-token \"}}'\n",
                log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("HERDR_AGENT_QUOTA_SECURITY_BIN", &stub);
        let auth = dir.path().join("auth.json");
        fs::write(
            &auth,
            r#"{"providers":{"meta":{"mechanism":"oauth","storage":"keychain"}}}"#,
        )
        .unwrap();
        // The skip is a distinct error, not MissingCredentials, so refresh
        // keeps the last-good snapshot instead of overwriting it with empty.
        assert!(matches!(
            read_credentials(&auth),
            Err(ProviderError::Unavailable(_))
        ));
        assert!(!log.exists());
        assert!(!marker.exists());
        std::env::remove_var("HERDR_AGENT_QUOTA_SECURITY_BIN");
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    /// An explicit approval attempt runs the lookup and records the marker,
    /// so later background reads trust the keychain.
    #[test]
    fn a_keychain_approve_attempt_records_its_approval() {
        let _guard = security_env_guard();
        let _approve = KeychainApproveGuard::new(true);
        let config_home = tempdir().unwrap();
        let marker = keychain_marker_in(config_home.path());
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        let dir = tempdir().unwrap();
        let stub = dir.path().join("security-stub");
        fs::write(
            &stub,
            "#!/bin/sh\nprintf '%s' '{\"access_token\":\" keychain-token \"}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("HERDR_AGENT_QUOTA_SECURITY_BIN", &stub);
        let auth = dir.path().join("auth.json");
        fs::write(
            &auth,
            r#"{"providers":{"meta":{"mechanism":"oauth","storage":"keychain"}}}"#,
        )
        .unwrap();
        assert_eq!(
            read_credentials(&auth).unwrap().access_token,
            "keychain-token"
        );
        assert!(marker.exists());
        assert!(fs::read_to_string(&marker).unwrap().parse::<u64>().is_ok());
        std::env::remove_var("HERDR_AGENT_QUOTA_SECURITY_BIN");
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    /// A failed lookup keeps the marker: a timeout, a hiccup, and an explicit
    /// Deny are indistinguishable here, so a transient failure must not
    /// permanently de-authorize background reads. The next poll retries.
    #[test]
    fn a_failed_keychain_read_keeps_its_approval() {
        let _guard = security_env_guard();
        set_keychain_approve_attempt_for_test(false);
        let config_home = tempdir().unwrap();
        let marker = keychain_marker_in(config_home.path());
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, "1").unwrap();
        let dir = tempdir().unwrap();
        let stub = dir.path().join("security-stub");
        fs::write(&stub, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("HERDR_AGENT_QUOTA_SECURITY_BIN", &stub);
        let auth = dir.path().join("auth.json");
        fs::write(
            &auth,
            r#"{"providers":{"meta":{"mechanism":"oauth","storage":"keychain"}}}"#,
        )
        .unwrap();
        assert!(matches!(
            read_credentials(&auth),
            Err(ProviderError::MissingCredentials)
        ));
        assert!(marker.exists());
        std::env::remove_var("HERDR_AGENT_QUOTA_SECURITY_BIN");
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    /// Recording an approval invalidates the cached skip: the daemon picks
    /// the keychain back up on its next poll without a restart.
    #[test]
    fn recording_an_approval_invalidates_the_cached_skip() {
        let _guard = security_env_guard();
        set_keychain_approve_attempt_for_test(false);
        let config_home = tempdir().unwrap();
        let marker = keychain_marker_in(config_home.path());
        let dir = tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let stub = dir.path().join("security-stub");
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho called >> '{}'\nprintf '%s' '{{\"access_token\":\" keychain-token \"}}'\n",
                log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("HERDR_AGENT_QUOTA_SECURITY_BIN", &stub);
        let auth = dir.path().join("auth.json");
        fs::write(
            &auth,
            r#"{"providers":{"meta":{"mechanism":"oauth","storage":"keychain"}}}"#,
        )
        .unwrap();
        assert!(matches!(
            read_credentials(&auth),
            Err(ProviderError::Unavailable(_))
        ));
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, "1").unwrap();
        assert_eq!(
            read_credentials(&auth).unwrap().access_token,
            "keychain-token"
        );
        assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
        std::env::remove_var("HERDR_AGENT_QUOTA_SECURITY_BIN");
        std::env::remove_var("XDG_CONFIG_HOME");
    }
    /// A slow approval (a prompt was handled) records the marker only after
    /// a second instant read proves the grant stuck.
    #[test]
    fn a_slow_approval_verifies_before_recording() {
        let _guard = security_env_guard();
        let _approve = KeychainApproveGuard::new(true);
        let config_home = tempdir().unwrap();
        let marker = keychain_marker_in(config_home.path());
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        let dir = tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let flag = dir.path().join("slow-once.flag");
        let stub = dir.path().join("security-stub");
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho called >> '{}'\nif [ -f '{}' ]; then\nprintf '%s' '{{\"access_token\":\" keychain-token \"}}'\nelse\ntouch '{}'\nsleep 3\nprintf '%s' '{{\"access_token\":\" keychain-token \"}}'\nfi\n",
                log.display(),
                flag.display(),
                flag.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("HERDR_AGENT_QUOTA_SECURITY_BIN", &stub);
        let auth = dir.path().join("auth.json");
        fs::write(
            &auth,
            r#"{"providers":{"meta":{"mechanism":"oauth","storage":"keychain"}}}"#,
        )
        .unwrap();
        assert_eq!(
            read_credentials(&auth).unwrap().access_token,
            "keychain-token"
        );
        assert!(marker.exists());
        assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 2);
        std::env::remove_var("HERDR_AGENT_QUOTA_SECURITY_BIN");
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    /// A one-time approval works for that run but records nothing: a marker
    /// without a persistent grant would poison the daemon into re-prompting.
    #[test]
    fn a_one_time_approval_works_for_that_run_only() {
        let _guard = security_env_guard();
        let _approve = KeychainApproveGuard::new(true);
        let config_home = tempdir().unwrap();
        let marker = keychain_marker_in(config_home.path());
        let dir = tempdir().unwrap();
        let flag = dir.path().join("slow-once.flag");
        let stub = dir.path().join("security-stub");
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\nif [ -f '{}' ]; then\nexit 1\nelse\ntouch '{}'\nsleep 3\nprintf '%s' '{{\"access_token\":\" keychain-token \"}}'\nfi\n",
                flag.display(),
                flag.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("HERDR_AGENT_QUOTA_SECURITY_BIN", &stub);
        let auth = dir.path().join("auth.json");
        fs::write(
            &auth,
            r#"{"providers":{"meta":{"mechanism":"oauth","storage":"keychain"}}}"#,
        )
        .unwrap();
        assert_eq!(
            read_credentials(&auth).unwrap().access_token,
            "keychain-token"
        );
        assert!(!marker.exists());
        std::env::remove_var("HERDR_AGENT_QUOTA_SECURITY_BIN");
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn the_credentials_debug_output_is_redacted() {
        let credentials = MuseCredentials {
            access_token: "secret-token".into(),
        };
        assert!(!format!("{credentials:?}").contains("secret-token"));
    }

    #[test]
    fn account_pin_is_scoped_and_pinned() {
        assert_eq!(
            account_pin(" token "),
            format!("{:x}", Sha256::digest(b"muse\0token"))
        );
        assert_ne!(
            account_pin("token"),
            crate::providers::credential_id("token")
        );
    }

    #[test]
    fn the_configured_model_comes_from_settings() {
        assert_eq!(
            configured_model_from(&json!({"model": " muse-spark-1.3 "})).as_deref(),
            Some("muse-spark-1.3")
        );
        assert_eq!(configured_model_from(&json!({"model": ""})), None);
        assert_eq!(configured_model_from(&json!({})), None);
    }

    const SESSION_ID: &str = "01a00000-0000-7000-8000-000000000001";

    fn session_tail_fixture() -> &'static str {
        include_str!("../../tests/fixtures/muse/session-tail.jsonl")
    }

    fn limits() -> BTreeMap<String, u64> {
        BTreeMap::from([("muse-spark-1.3".to_string(), 1_007_997)])
    }

    #[test]
    fn the_tail_yields_the_last_call_and_the_last_prompt() {
        let observation = observe_session_tail(session_tail_fixture());
        assert_eq!(observation.model.as_deref(), Some("muse-spark-1.3"));
        assert_eq!(
            observation.usage,
            Some(TokenUsage {
                input: 321_385,
                output: 429,
                cache_read: 320_369,
                cache_write: 0,
            })
        );
        assert_eq!(
            observation.prompt.as_deref(),
            Some("Add Muse support to the quota plugin")
        );
    }

    #[test]
    fn only_a_main_surface_chat_intent_is_a_topic() {
        let intent = |surface: &str, kind: &str| {
            json!({
                "surface": surface,
                "semantic_kind": {"kind": kind},
                "refill_blocks": [{"kind": "image", "text": "ignored"}, {"kind": "text", "text": "  \n Fix the build "}]
            })
        };
        assert_eq!(
            chat_intent_prompt(&intent("main", "chat")).as_deref(),
            Some("Fix the build")
        );
        assert_eq!(chat_intent_prompt(&intent("side", "chat")), None);
        assert_eq!(chat_intent_prompt(&intent("main", "wake")), None);
        let from_messages = json!({
            "semantic_kind": {"kind": "chat"},
            "model_messages": [{"content": [{"kind": "text", "text": "From messages"}]}]
        });
        assert_eq!(
            chat_intent_prompt(&from_messages).as_deref(),
            Some("From messages")
        );
    }

    #[test]
    fn context_and_cache_come_from_the_last_call_against_the_catalog_window() {
        let observation = observe_session_tail(session_tail_fixture());
        let context = observation.context(SESSION_ID, &limits()).unwrap();
        let expected = (321_385.0 + 429.0) / 1_007_997.0 * 100.0;
        assert!((context.used_percent - expected).abs() < 1e-9);
        let cache = context.cache.unwrap();
        assert_eq!(cache.read_tokens, 320_369);
        assert_eq!(cache.fresh_input_tokens, 1_016);
        assert_eq!(cache.ttl_seconds, None);
        assert_eq!(cache.session_id.as_deref(), Some(SESSION_ID));
    }

    #[test]
    fn an_unknown_context_window_or_impossible_usage_yields_no_context() {
        let observation = observe_session_tail(session_tail_fixture());
        assert_eq!(observation.context(SESSION_ID, &BTreeMap::new()), None);
        assert_eq!(
            parse_token_usage(Some(&json!({"input_tokens": 5, "cache_read_tokens": 9}))),
            None
        );
        assert_eq!(parse_token_usage(Some(&json!({"output_tokens": 5}))), None);
    }

    #[test]
    fn a_lock_and_an_environment_are_read_strictly() {
        let dir = tempdir().unwrap();
        let lock = dir.path().join(".session.lock");
        fs::write(&lock, "pid=4242\n").unwrap();
        assert_eq!(read_lock_pid(&lock), Some(4242));
        fs::write(&lock, "4242").unwrap();
        assert_eq!(read_lock_pid(&lock), None);
        assert_eq!(
            pane_id_from_environ(b"PATH=/bin\0HERDR_PANE_ID=w1:p2\0HOME=/h\0"),
            Some("w1:p2".to_string())
        );
        assert_eq!(pane_id_from_environ(b"XHERDR_PANE_ID=w1:p2\0"), None);
    }

    fn fake_boot(proc_root: &Path, boot_unix: u64) {
        fs::create_dir_all(proc_root).unwrap();
        fs::write(
            proc_root.join("stat"),
            format!("cpu 0 0 0\nbtime {boot_unix}\n"),
        )
        .unwrap();
    }

    /// `stat` puts the start time (field 22) at zero ticks, so the process
    /// started at boot.
    fn fake_process(proc_root: &Path, pid: u32, comm: &str, pane: &str) {
        let dir = proc_root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
        fs::write(dir.join("environ"), format!("HERDR_PANE_ID={pane}\0")).unwrap();
        let fields = "0 ".repeat(18);
        fs::write(dir.join("stat"), format!("{pid} ({comm}) S {fields}0")).unwrap();
    }

    fn fake_session(sessions_root: &Path, day: &str, id: &str, pid: u32) -> PathBuf {
        let dir = sessions_root.join(day).join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".session.lock"), format!("pid={pid}")).unwrap();
        fs::write(dir.join("session.jsonl"), session_tail_fixture()).unwrap();
        dir
    }

    #[test]
    fn a_pane_resolves_only_through_its_own_muse_process_lock() {
        let dir = tempdir().unwrap();
        let proc_root = dir.path().join("proc");
        let sessions = dir.path().join("sessions");
        fake_boot(&proc_root, 1);
        fake_process(&proc_root, 100, "muse-bin-1.1.1-", "w1:p1");
        // Same pane variable, but not a Muse process.
        fake_process(&proc_root, 200, "bash", "w1:p2");
        fake_process(&proc_root, 300, "muse-bin-1.1.1-", "w9:p9");
        fake_session(&sessions, "2026/09/10", SESSION_ID, 100);
        fake_session(
            &sessions,
            "2026/09/10",
            "01a0aaaa-0000-7000-8000-000000000002",
            200,
        );
        fake_session(
            &sessions,
            "2026/09/09",
            "01a0aaaa-0000-7000-8000-000000000003",
            300,
        );
        fs::create_dir_all(sessions.join(".msp-view-v1").join("ignored")).unwrap();

        let resolved = session_ids_for_panes_at(
            &proc_root,
            &sessions,
            &[
                "w1:p1".to_string(),
                "w1:p2".to_string(),
                "w1:p3".to_string(),
            ],
        );
        assert_eq!(
            resolved,
            BTreeMap::from([("w1:p1".to_string(), SESSION_ID.to_string())])
        );
        assert!(session_ids_for_panes_at(&proc_root, &sessions, &[]).is_empty());
    }

    /// A recycled pid must not inherit the session of the process that wrote
    /// the lock before it.
    #[test]
    fn a_lock_written_before_its_process_started_is_stale() {
        let dir = tempdir().unwrap();
        let proc_root = dir.path().join("proc");
        let sessions = dir.path().join("sessions");
        fake_boot(&proc_root, CacheStore::now_unix() + 3600);
        fake_process(&proc_root, 100, "muse-bin-1.1.1-", "w1:p1");
        fake_session(&sessions, "2026/09/10", SESSION_ID, 100);
        assert!(session_ids_for_panes_at(&proc_root, &sessions, &["w1:p1".to_string()]).is_empty());
    }

    fn set_mtime(path: &Path, unix: u64) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(unix))
            .unwrap();
    }

    #[test]
    fn a_lock_counts_only_within_clock_resolution_of_the_process_start() {
        let dir = tempdir().unwrap();
        let proc_root = dir.path().join("proc");
        let sessions = dir.path().join("sessions");
        let started = 1_000_000;
        fake_boot(&proc_root, started);
        fake_process(&proc_root, 100, "muse-bin-1.1.1-", "w1:p1");
        let lock = fake_session(&sessions, "2026/09/10", SESSION_ID, 100).join(".session.lock");
        let panes = ["w1:p1".to_string()];

        set_mtime(&lock, started - LOCK_CLOCK_SLACK_SECONDS);
        assert_eq!(
            session_ids_for_panes_at(&proc_root, &sessions, &panes).get("w1:p1"),
            Some(&SESSION_ID.to_string())
        );
        set_mtime(&lock, started - LOCK_CLOCK_SLACK_SECONDS - 1);
        assert!(session_ids_for_panes_at(&proc_root, &sessions, &panes).is_empty());
    }

    #[test]
    fn each_pane_keeps_its_own_process_and_its_newest_transcript() {
        let dir = tempdir().unwrap();
        let proc_root = dir.path().join("proc");
        let sessions = dir.path().join("sessions");
        fake_boot(&proc_root, 1);
        fake_process(&proc_root, 100, "muse-bin-1.1.1-", "w1:p1");
        fake_process(&proc_root, 200, "muse-bin-1.1.1-", "w1:p2");
        let older = fake_session(
            &sessions,
            "2026/09/10",
            "01a0aaaa-0000-7000-8000-00000000000a",
            100,
        );
        // Filed under an earlier day, but written last: the transcript decides.
        let newer = fake_session(
            &sessions,
            "2026/09/09",
            "01a0aaaa-0000-7000-8000-00000000000b",
            100,
        );
        fake_session(
            &sessions,
            "2026/09/10",
            "01a0aaaa-0000-7000-8000-00000000000c",
            200,
        );
        set_mtime(&older.join("session.jsonl"), 5_000);
        set_mtime(&newer.join("session.jsonl"), 6_000);

        let resolved = session_ids_for_panes_at(
            &proc_root,
            &sessions,
            &["w1:p1".to_string(), "w1:p2".to_string()],
        );
        assert_eq!(
            resolved,
            BTreeMap::from([
                (
                    "w1:p1".to_string(),
                    "01a0aaaa-0000-7000-8000-00000000000b".to_string()
                ),
                (
                    "w1:p2".to_string(),
                    "01a0aaaa-0000-7000-8000-00000000000c".to_string()
                ),
            ])
        );
    }

    #[test]
    fn process_start_counts_fields_after_the_last_parenthesis() {
        let dir = tempdir().unwrap();
        let fields = "0 ".repeat(18);
        let ticks = clock_ticks_per_second() * 7;
        fs::write(
            dir.path().join("stat"),
            format!("42 (muse (x) bin) S {fields}{ticks} 99"),
        )
        .unwrap();
        assert_eq!(process_start_unix(dir.path(), 1000), Some(1007));
        assert_eq!(boot_time_unix(dir.path()), None);
    }

    #[test]
    fn session_ids_are_never_joined_as_paths() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        fake_session(&sessions, "2026/09/10", SESSION_ID, 1);
        let days = day_dirs(&sessions);
        assert!(find_session_log(&days, SESSION_ID).is_some());
        assert!(find_session_log(&days, "../2026").is_none());
        assert!(find_session_log(&days, "").is_none());
    }

    #[test]
    fn a_seeked_tail_drops_the_cut_first_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        fs::write(&path, "first line\nsecond\nthird\n").unwrap();
        assert_eq!(read_tail(&path, 13).as_deref(), Some("third\n"));
        assert_eq!(
            read_tail(&path, 1024).as_deref(),
            Some("first line\nsecond\nthird\n")
        );
    }

    #[test]
    fn enrichment_fills_only_the_named_sessions() {
        let dir = tempdir().unwrap();
        fake_session(&dir.path().join("sessions"), "2026/09/10", SESSION_ID, 1);
        let catalog = dir.path().join("model-catalog");
        fs::create_dir_all(&catalog).unwrap();
        fs::write(
            catalog.join("6d657461__p746268.json"),
            r#"{"rows": [{"model_id": "muse-spark-1.3", "context_limit": 1007997}]}"#,
        )
        .unwrap();

        let mut snapshot = ProviderSnapshot::new(Provider::Muse, vec![], 1);
        enrich_sessions_at(
            &mut snapshot,
            dir.path(),
            &[SESSION_ID.to_string(), "missing-session".to_string()],
        );
        assert_eq!(
            snapshot.session_models.get(SESSION_ID).map(String::as_str),
            Some("muse-spark-1.3")
        );
        assert!(snapshot.session_contexts.contains_key(SESSION_ID));
        assert_eq!(
            snapshot
                .session_summaries
                .get(SESSION_ID)
                .map(String::as_str),
            Some("Add Muse support to the quota plugin")
        );
        assert_eq!(snapshot.session_models.len(), 1);
    }

    fn oauth() -> std::result::Result<MuseCredentials, ProviderError> {
        Ok(MuseCredentials {
            access_token: "token".into(),
        })
    }

    fn subscribed(
        _: &MuseCredentials,
        now: u64,
    ) -> std::result::Result<ProviderSnapshot, ProviderError> {
        parse_subscription(&power_fixture(), now)
    }

    #[test]
    fn an_account_without_a_subscription_keeps_local_fields_only_for_a_session() {
        let no_login = || Err(ProviderError::MissingCredentials);
        let never =
            |_: &MuseCredentials, _| -> std::result::Result<ProviderSnapshot, ProviderError> {
                panic!("no account login means no subscription request")
            };

        let api_key = resolve_subscription(no_login(), true, 7, never).unwrap();
        assert!(api_key.windows.is_empty());
        assert_eq!(api_key.account_id, None);
        assert!(matches!(
            resolve_subscription(no_login(), false, 7, never),
            Err(ProviderError::MissingCredentials)
        ));

        let inactive = |_: &MuseCredentials, _| Err(ProviderError::Unavailable("inactive".into()));
        let lapsed = resolve_subscription(oauth(), true, 7, inactive).unwrap();
        assert!(lapsed.windows.is_empty());
        assert_eq!(lapsed.account_id, Some(account_pin("token")));
        assert!(resolve_subscription(oauth(), false, 7, inactive).is_err());

        let active = resolve_subscription(oauth(), true, 7, subscribed).unwrap();
        assert_eq!(active.windows.len(), 2);
        assert_eq!(active.account_id, Some(account_pin("token")));
    }

    /// A failed or rejected request must not replace a cached reading with an
    /// empty one, so it stays an error even while a session is refreshed.
    #[test]
    fn a_failed_or_rejected_request_is_never_a_missing_subscription() {
        for error in [
            ProviderError::MissingCredentials,
            ProviderError::Request("HTTP 503".into()),
            ProviderError::UnsupportedResponse("not JSON".into()),
        ] {
            let mut error = Some(error);
            let failing = |_: &MuseCredentials, _| Err(error.take().unwrap());
            assert!(resolve_subscription(oauth(), true, 7, failing).is_err());
        }
    }

    #[test]
    fn a_pane_without_quota_still_renders_its_session_fields() {
        let dir = tempdir().unwrap();
        fake_session(&dir.path().join("sessions"), "2026/09/10", SESSION_ID, 1);
        let catalog = dir.path().join("model-catalog");
        fs::create_dir_all(&catalog).unwrap();
        fs::write(
            catalog.join("catalog.json"),
            r#"{"rows": [{"model_id": "muse-spark-1.3", "context_limit": 1007997}]}"#,
        )
        .unwrap();
        let mut snapshot =
            resolve_subscription(Err(ProviderError::MissingCredentials), true, 1, |_, _| {
                panic!("no request")
            })
            .unwrap();
        enrich_sessions_at(&mut snapshot, dir.path(), &[SESSION_ID.to_string()]);

        let tokens = crate::presentation::MetadataTokens::from_snapshot_for_pane(
            &snapshot,
            1,
            Some(SESSION_ID),
            crate::cli::PercentStyle::default(),
            crate::presentation::SidebarShape::default(),
        );
        assert_eq!(tokens.quota_model, "muse-spark-1.3");
        assert!(!tokens.quota_context.is_empty());
        assert!(!tokens.quota_cache.is_empty());
        assert_eq!(tokens.quota_5h, "");
        assert_eq!(tokens.quota_week, "");
        assert_eq!(tokens.quota_error, None);
        assert_eq!(tokens.quota_headroom, None);
    }
}
