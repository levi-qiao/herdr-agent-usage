use crate::cache::CacheStore;
use crate::model::{Provider, ProviderSnapshot, ResetAt, UsageWindow, WindowKind};
use crate::providers::statusline::{parse_context, parse_model};
use crate::providers::ProviderError;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Spellings whose value is already a `0..=1` fraction.
const FRACTION_KEYS: [&str; 2] = ["remaining_fraction", "remainingFraction"];
/// Spellings whose value is a `0..=100` percentage.
const PERCENT_KEYS: [&str; 2] = ["remaining_percent", "remainingPercentage"];

const GEMINI_FIVE_HOUR_KEYS: [&str; 1] = ["gemini-5h"];
const GEMINI_WEEKLY_KEYS: [&str; 1] = ["gemini-weekly"];
const GEMINI_ARGON_WEEKLY_KEYS: [&str; 1] = ["gemini-argon-wtu-weekly"];
const THIRD_PARTY_FIVE_HOUR_KEYS: [&str; 1] = ["3p-5h"];
const THIRD_PARTY_WEEKLY_KEYS: [&str; 1] = ["3p-weekly"];
/// The sidebar has three window rows. Pools fill them in this order.
const SIDEBAR_SLOTS: [WindowKind; 3] = [
    WindowKind::FiveHour,
    WindowKind::Weekly,
    WindowKind::Monthly,
];

/// One Antigravity allowance. Pro exposes three of these: Gemini, Argon,
/// and the third-party pool shared by Claude and GPT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pool {
    Gemini,
    Argon,
    Claude,
}

#[derive(Clone)]
struct Reading {
    pool: Pool,
    /// The bucket's own period. The sidebar slot this reading lands in is
    /// separate: three pools share three rows, so a weekly bucket can sit
    /// in the first row.
    period: WindowKind,
    window: UsageWindow,
}

/// Parse the quota object emitted by Agy/Antigravity's statusLine JSON.
///
/// The payload carries one account's pools side by side. The active model
/// does not choose which pool is shown: a Carbon pane and a Claude pane on
/// the same login draw down different pools, and hiding the other one is
/// how a Pro account lost Argon or Claude. Up to three rows are published,
/// in pool order Gemini, Argon, Claude. Each pool first contributes the
/// bucket with the least remaining allowance. A spare row then shows a
/// weekly bucket that pass left out, so Claude's week is not dropped while
/// Gemini still has a second period. A row that is still empty after that
/// keeps the next period in pool order. Labels stay three characters:
/// `gem` / `arn` / `cla` for a pool's only row, and `g5h` / `g7d` / `c5h` /
/// `c7d` when that row has to be told apart from another period on screen.
pub fn parse_statusline(
    value: &Value,
    fetched_at_unix: u64,
) -> std::result::Result<ProviderSnapshot, ProviderError> {
    let quota = value
        .get("quota")
        .and_then(Value::as_object)
        .ok_or_else(|| ProviderError::UnsupportedResponse("missing quota".to_string()))?;
    let model = parse_model(value);
    let windows = sidebar_windows(quota, fetched_at_unix)?;
    if windows.is_empty() {
        return Err(ProviderError::UnsupportedResponse(
            "quota has no supported windows".to_string(),
        ));
    }

    let mut context = parse_context(
        value
            .get("context_window")
            .or_else(|| value.get("contextWindow")),
    )
    .unwrap_or(None);
    // Antigravity currently emits cache counter keys even when the active
    // model has no cache traffic. Keep this provider quirk local to Agy: the
    // shared statusLine parser must still represent a real zero-percent hit
    // for providers where zero read/create counters are meaningful.
    let idle_cache_counters = context
        .as_ref()
        .and_then(|context| context.cache.as_ref())
        .is_some_and(|cache| cache.read_tokens == 0 && cache.creation_tokens == 0);
    if idle_cache_counters {
        if let Some(context) = context.as_mut() {
            context.cache = None;
        }
    }

    Ok(
        ProviderSnapshot::new(Provider::Agy, windows, fetched_at_unix)
            // StatusLine evidence remains conversation-local in the cache.
            // `ProviderSnapshot` only bridges a mismatched Herdr subagent id
            // when exactly one Agy conversation is observable.
            .session_local()
            .with_model(model)
            .with_context(context),
    )
}

fn sidebar_windows(
    quota: &serde_json::Map<String, Value>,
    fetched_at_unix: u64,
) -> std::result::Result<Vec<UsageWindow>, ProviderError> {
    let mut pools = Vec::new();
    for (pool, buckets) in [
        (
            Pool::Gemini,
            [
                (WindowKind::FiveHour, &GEMINI_FIVE_HOUR_KEYS as &[&str]),
                (WindowKind::Weekly, &GEMINI_WEEKLY_KEYS),
            ]
            .as_slice(),
        ),
        (
            Pool::Argon,
            [(WindowKind::Weekly, &GEMINI_ARGON_WEEKLY_KEYS as &[&str])].as_slice(),
        ),
        (
            Pool::Claude,
            [
                (WindowKind::FiveHour, &THIRD_PARTY_FIVE_HOUR_KEYS as &[&str]),
                (WindowKind::Weekly, &THIRD_PARTY_WEEKLY_KEYS),
            ]
            .as_slice(),
        ),
    ] {
        let mut readings = Vec::new();
        for (period, keys) in buckets {
            if let Some(window) = parse_window(quota, *period, keys, fetched_at_unix)? {
                readings.push(Reading {
                    pool,
                    period: *period,
                    window,
                });
            }
        }
        if !readings.is_empty() {
            pools.push(readings);
        }
    }

    let mut chosen: Vec<Reading> = Vec::new();
    for readings in &pools {
        if let Some(reading) = tightest(readings) {
            chosen.push(reading.clone());
        }
        if chosen.len() == SIDEBAR_SLOTS.len() {
            break;
        }
    }
    if chosen.len() < SIDEBAR_SLOTS.len() {
        // The first pass can spend Claude's only row on the 5h bucket. The
        // spare row belongs to the weekly that pass skipped, ahead of a
        // second period from a pool whose week is already on screen.
        while chosen.len() < SIDEBAR_SLOTS.len() {
            let Some(reading) = pools
                .iter()
                .flatten()
                .filter(|reading| {
                    reading.period == WindowKind::Weekly && !already_chosen(&chosen, reading)
                })
                .min_by(|left, right| tighter(left, right))
            else {
                break;
            };
            chosen.push(reading.clone());
        }
    }
    if chosen.len() < SIDEBAR_SLOTS.len() {
        for readings in &pools {
            for reading in readings {
                if already_chosen(&chosen, reading) {
                    continue;
                }
                chosen.push(reading.clone());
                if chosen.len() == SIDEBAR_SLOTS.len() {
                    break;
                }
            }
            if chosen.len() == SIDEBAR_SLOTS.len() {
                break;
            }
        }
    }

    let beside_shorter = chosen
        .iter()
        .any(|reading| reading.period == WindowKind::FiveHour);
    Ok(chosen
        .iter()
        .enumerate()
        .map(|(index, reading)| {
            let rows_for_pool = chosen
                .iter()
                .filter(|other| other.pool == reading.pool)
                .count();
            let mut window = reading.window.clone();
            window.kind = SIDEBAR_SLOTS[index];
            window.with_source_window(
                row_label(reading.pool, rows_for_pool, reading.period, beside_shorter),
                Some(reading.period.duration_seconds()),
            )
        })
        .collect())
}

fn already_chosen(chosen: &[Reading], reading: &Reading) -> bool {
    chosen
        .iter()
        .any(|existing| existing.pool == reading.pool && existing.period == reading.period)
}

/// The bucket that runs out first. Equal remaining prefers the shorter
/// period, which resets sooner.
fn tightest(readings: &[Reading]) -> Option<&Reading> {
    readings.iter().min_by(|left, right| tighter(left, right))
}

fn tighter(left: &Reading, right: &Reading) -> std::cmp::Ordering {
    left.window
        .remaining_percent
        .total_cmp(&right.window.remaining_percent)
        .then(period_rank(left.period).cmp(&period_rank(right.period)))
}

fn period_rank(period: WindowKind) -> u8 {
    match period {
        WindowKind::FiveHour => 0,
        WindowKind::Weekly => 1,
        WindowKind::Monthly => 2,
    }
}

fn row_label(
    pool: Pool,
    rows_for_pool: usize,
    period: WindowKind,
    beside_shorter: bool,
) -> &'static str {
    // A lone weekly sitting next to a 5h row has to say `g7d` / `c7d`. The
    // pool name is only enough when every row on screen is that pool's one
    // bucket, which is the Pro case (`gem` / `arn` / `cla`).
    let period_label = rows_for_pool > 1 || (beside_shorter && period != WindowKind::FiveHour);
    match (pool, period_label, period) {
        (Pool::Gemini, false, _) => "gem",
        (Pool::Gemini, _, WindowKind::Weekly) => "g7d",
        (Pool::Gemini, _, _) => "g5h",
        (Pool::Argon, _, _) => "arn",
        (Pool::Claude, false, _) => "cla",
        (Pool::Claude, _, WindowKind::Weekly) => "c7d",
        (Pool::Claude, _, _) => "c5h",
    }
}

fn parse_window(
    quota: &serde_json::Map<String, Value>,
    kind: WindowKind,
    keys: &[&str],
    fetched_at_unix: u64,
) -> std::result::Result<Option<UsageWindow>, ProviderError> {
    let mut lowest_remaining: Option<f64> = None;
    let mut reset = None;
    for key in keys {
        let Some(bucket) = quota.get(*key) else {
            continue;
        };
        let Some(remaining) = parse_remaining(bucket) else {
            continue;
        };
        if lowest_remaining.is_none_or(|current| remaining < current) {
            lowest_remaining = Some(remaining);
            reset = parse_reset(bucket, fetched_at_unix);
        }
    }
    let Some(remaining) = lowest_remaining else {
        return Ok(None);
    };
    let used = (100.0 - remaining * 100.0).clamp(0.0, 100.0);
    UsageWindow::new(kind, used, reset)
        .map(Some)
        .map_err(|error| ProviderError::UnsupportedResponse(error.to_string()))
}

fn parse_reset(value: &Value, fetched_at_unix: u64) -> Option<ResetAt> {
    value
        .get("reset_time")
        .or_else(|| value.get("resetTime"))
        .and_then(Value::as_str)
        .and_then(ResetAt::parse_rfc3339)
        .or_else(|| {
            value
                .get("reset_in_seconds")
                .or_else(|| value.get("resetInSeconds"))
                .and_then(Value::as_u64)
                .map(|seconds| ResetAt::after(fetched_at_unix, seconds))
        })
}

/// Remaining allowance as a `0..=1` fraction.
///
/// The scale comes from the key, never from the value. Inferring it from the
/// magnitude reads a `remaining_percent` of `1.0` as a full pool instead of a
/// nearly exhausted one — the same 100x error, in the same dangerous
/// direction, that [`crate::providers::opencode_go`] exists to avoid.
fn parse_remaining(value: &Value) -> Option<f64> {
    let object = value.as_object()?;
    let (raw, per_unit) = FRACTION_KEYS
        .iter()
        .find_map(|key| {
            object
                .get(*key)
                .and_then(Value::as_f64)
                .map(|raw| (raw, 1.0))
        })
        .or_else(|| {
            PERCENT_KEYS.iter().find_map(|key| {
                object
                    .get(*key)
                    .and_then(Value::as_f64)
                    .map(|raw| (raw, 100.0))
            })
        })?;
    raw.is_finite().then(|| (raw / per_unit).clamp(0.0, 1.0))
}

pub fn run_statusline(input: &[u8]) -> std::result::Result<ProviderSnapshot, ProviderError> {
    let value: Value = serde_json::from_slice(input).map_err(|_| {
        ProviderError::UnsupportedResponse("statusLine input is not JSON".to_string())
    })?;
    parse_statusline(&value, CacheStore::now_unix())
}

/// Antigravity CLI home. `AGY_SETTINGS_FILE` points at `settings.json` inside
/// it; otherwise the CLI's own directory is `~/.gemini/antigravity-cli`.
pub fn cli_dir() -> Option<PathBuf> {
    if let Some(settings) = std::env::var_os("AGY_SETTINGS_FILE") {
        return Path::new(&settings).parent().map(Path::to_path_buf);
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".gemini/antigravity-cli"))
}

/// Conversation title Agy stores for this session.
///
/// The file is `annotations/<conversation>.pbtxt` and its only field is
/// `title`. A Herdr session that is a subagent has no annotation of its own;
/// the subagent record names the parent conversation, and that annotation is
/// the summary. Two parents that disagree are not a title. Transcripts are
/// not read.
pub fn conversation_title(session_id: &str) -> Option<String> {
    conversation_title_in(&cli_dir()?, session_id)
}

pub(crate) fn conversation_title_in(home: &Path, session_id: &str) -> Option<String> {
    let session_id = session_file_id(session_id)?;
    if let Some(title) = annotation_title_at(&annotation_path(home, session_id)) {
        return Some(title);
    }
    let parent = parent_conversation(home, session_id)?;
    annotation_title_at(&annotation_path(home, &parent))
}

fn session_file_id(session_id: &str) -> Option<&str> {
    let safe = !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-');
    safe.then_some(session_id)
}

fn annotation_path(home: &Path, conversation_id: &str) -> PathBuf {
    home.join("annotations")
        .join(format!("{conversation_id}.pbtxt"))
}

fn annotation_title_at(path: &Path) -> Option<String> {
    let text = read_capped(path, 4 * 1024)?;
    annotation_title(&text)
}

fn annotation_title(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("title:"))?;
    let rest = line["title:".len()..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let raw = unescape_quoted(&rest[quote.len_utf8()..], quote)?;
    let title = raw.trim();
    (!title.is_empty()).then(|| title.to_string())
}

fn unescape_quoted(input: &str, quote: char) -> Option<String> {
    let mut out = String::new();
    let mut chars = input.chars();
    while let Some(character) = chars.next() {
        if character == quote {
            return Some(out);
        }
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '\'' => out.push('\''),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            other => out.push(other),
        }
    }
    None
}

/// Parent conversation id for a subagent session.
///
/// The record lives at `brain/<parent>/.system_generated/subagents/<id>.json`.
/// Its `conversationId` is the subagent's own id, so the parent is the brain
/// directory. Two directories that both claim the id are ambiguity, and the
/// pane keeps whatever topic it already had. The file body is not a title.
fn parent_conversation(home: &Path, session_id: &str) -> Option<String> {
    let brains = std::fs::read_dir(home.join("brain")).ok()?;
    let mut parent = None;
    for entry in brains.flatten() {
        let Some(dir_name) = entry.file_name().into_string().ok() else {
            continue;
        };
        if session_file_id(&dir_name).is_none() || dir_name == session_id {
            continue;
        }
        let path = entry
            .path()
            .join(".system_generated/subagents")
            .join(format!("{session_id}.json"));
        let Some(text) = read_capped(&path, 64 * 1024) else {
            continue;
        };
        let claims_this_session = serde_json::from_str::<Value>(&text)
            .ok()
            .as_ref()
            .and_then(|value| value.get("conversationId"))
            .and_then(Value::as_str)
            == Some(session_id);
        if !claims_this_session {
            continue;
        }
        match &parent {
            None => parent = Some(dir_name),
            Some(existing) if existing != &dir_name => return None,
            Some(_) => {}
        }
    }
    parent
}

fn read_capped(path: &Path, cap: u64) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > cap {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_both_pools_without_asking_which_model_is_active() {
        let value = json!({
            "model": {"display_name": "Claude Sonnet"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 0.9969, "reset_time": "2026-08-15T12:00:00Z"},
                "gemini-weekly": {"remaining_fraction": 0.8, "reset_time": "2026-08-22T12:00:00Z"},
                "3p-5h": {"remaining_fraction": 0.72, "reset_time": "2026-08-15T13:00:00Z"},
                "3p-weekly": {"remaining_fraction": 0.91}
            }
        });
        let snapshot = parse_statusline(&value, 1).unwrap();
        assert_eq!(snapshot.provider, Provider::Agy);
        // Weekly Gemini leads. Claude's 5h is tighter than its week, so the
        // spare row is that week rather than Gemini's looser 5h.
        assert_rows(&snapshot, &[("g7d", 80.0), ("c5h", 72.0), ("c7d", 91.0)]);
        assert_eq!(
            snapshot.window(WindowKind::FiveHour).unwrap().resets_at,
            Some(ResetAt::from_unix_seconds(1_787_400_000))
        );
    }

    #[test]
    fn parses_optional_context_window_usage() {
        let value = json!({
            "context_window": {
                "used_percentage": 41.0,
                "current_usage": {
                    "input_tokens": 50,
                    "cache_read_input_tokens": 150,
                    "cache_creation_input_tokens": 0
                }
            },
            "quota": {
                "gemini-weekly": {"remaining_fraction": 0.8}
            }
        });
        let snapshot = parse_statusline(&value, 1).unwrap();
        assert_eq!(
            snapshot
                .context
                .as_ref()
                .map(|context| context.used_percent),
            Some(41.0)
        );
        assert_eq!(
            snapshot
                .context
                .as_ref()
                .unwrap()
                .cache
                .as_ref()
                .unwrap()
                .hit_percent,
            75.0
        );
    }

    #[test]
    fn zero_cache_counters_are_not_a_zero_percent_hit() {
        let value = json!({
            "context_window": {
                "used_percentage": 3.4,
                "current_usage": {
                    "input_tokens": 25943,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0
                }
            },
            "quota": {"gemini-weekly": {"remaining_fraction": 0.8}}
        });
        let snapshot = parse_statusline(&value, 1).unwrap();
        assert_eq!(
            snapshot
                .context
                .as_ref()
                .map(|context| context.used_percent),
            Some(3.4)
        );
        assert!(snapshot.context.as_ref().unwrap().cache.is_none());
    }

    #[test]
    fn parses_the_human_readable_active_model_name() {
        let value = json!({
            "model": {"id": "gemini-3.5-flash", "display_name": "Gemini Flash"},
            "quota": {"gemini-weekly": {"remaining_fraction": 0.8}}
        });
        let snapshot = parse_statusline(&value, 1).unwrap();
        assert_eq!(snapshot.model.as_deref(), Some("Gemini Flash"));
    }

    #[test]
    fn a_single_weekly_bucket_uses_the_pool_name_not_a_five_hour_label() {
        let value = json!({
            "quota": {"gemini-weekly": {"remaining_fraction": 0.61}}
        });
        let snapshot = parse_statusline(&value, 1).unwrap();
        assert_rows(&snapshot, &[("gem", 61.0)]);
    }

    #[test]
    fn derives_absolute_agy_reset_from_relative_seconds() {
        let value = json!({
            "quota": {"gemini-5h": {
                "remaining_fraction": 0.5,
                "reset_in_seconds": 900
            }}
        });
        let snapshot = parse_statusline(&value, 1_000).unwrap();
        assert_eq!(
            snapshot.window(WindowKind::FiveHour).unwrap().resets_at,
            Some(ResetAt::from_unix_seconds(1_900))
        );
    }

    #[test]
    fn rejects_payload_without_quota_windows() {
        assert!(parse_statusline(&json!({"quota": {}}), 1).is_err());
    }

    #[test]
    fn pro_shows_gemini_argon_and_claude_whatever_model_is_selected() {
        let quota = json!({
            "gemini-5h": {"remaining_fraction": 1.0, "reset_in_seconds": 17784},
            "gemini-weekly": {"remaining_fraction": 0.9477867, "reset_in_seconds": 371488},
            "gemini-argon-wtu-weekly": {"remaining_fraction": 0.5464704, "reset_in_seconds": 450186},
            "3p-5h": {"remaining_fraction": 1.0, "reset_in_seconds": 17784},
            "3p-weekly": {"remaining_fraction": 0.4997628, "reset_in_seconds": 473570}
        });
        for model in [
            None,
            Some("Orchestrator (Argon)"),
            Some("Claude Sonnet 4.5"),
        ] {
            let mut value = json!({"quota": quota});
            if let Some(model) = model {
                value["model"] = json!({"display_name": model});
            }
            let snapshot = parse_statusline(&value, 0).unwrap();
            assert_rows(
                &snapshot,
                &[("gem", 94.77867), ("arn", 54.64704), ("cla", 49.97628)],
            );
            let gemini = snapshot.window(WindowKind::FiveHour).unwrap();
            assert_eq!(
                gemini.duration_seconds,
                Some(WindowKind::Weekly.duration_seconds())
            );
            assert_eq!(gemini.resets_at, Some(ResetAt::from_unix_seconds(371488)));
        }
    }

    #[test]
    fn a_plan_without_argon_keeps_both_gemini_periods_and_claude() {
        let quota = json!({
            "gemini-5h": {"remaining_fraction": 0.68758285, "reset_in_seconds": 8438},
            "gemini-weekly": {"remaining_fraction": 0.9218957, "reset_in_seconds": 595238},
            "3p-5h": {"remaining_fraction": 1, "reset_in_seconds": 17426},
            "3p-weekly": {"remaining_fraction": 1, "reset_in_seconds": 604226}
        });
        for model in [
            None,
            Some("Orchestrator (Carbon)"),
            Some("Gemini 4 Carbon (High)"),
        ] {
            let mut value = json!({"quota": quota});
            if let Some(model) = model {
                value["model"] = json!({"id": model, "display_name": model});
            }
            let snapshot = parse_statusline(&value, 0).unwrap();
            assert_rows(
                &snapshot,
                &[("g5h", 68.758285), ("cla", 100.0), ("g7d", 92.18957)],
            );
        }
    }

    #[test]
    fn both_gemini_periods_show_when_the_argon_bucket_is_absent() {
        let value = json!({
            "model": {"display_name": "Gemini 4 Argon"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 1.0, "reset_in_seconds": 1000},
                "gemini-weekly": {"remaining_fraction": 0.85, "reset_in_seconds": 7200}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        // Weekly is the tighter Gemini bucket, so it leads. Both periods
        // still have a row when Argon is not in the payload.
        assert_rows(&snapshot, &[("g7d", 85.0), ("g5h", 100.0)]);
    }

    #[test]
    fn argon_keeps_its_own_row_beside_both_gemini_periods() {
        let value = json!({
            "model": {"display_name": "Future Model XYZ"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 0.9, "reset_in_seconds": 1000},
                "gemini-weekly": {"remaining_fraction": 0.85, "reset_in_seconds": 7200},
                "gemini-argon-wtu-weekly": {"remaining_fraction": 0.46, "reset_in_seconds": 3600}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert_rows(&snapshot, &[("g7d", 85.0), ("arn", 46.0), ("g5h", 90.0)]);
    }

    #[test]
    fn an_argon_only_bucket_is_labeled_arn() {
        let value = json!({
            "quota": {
                "gemini-argon-wtu-weekly": {"remaining_fraction": 0.46, "reset_in_seconds": 3600}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert_rows(&snapshot, &[("arn", 46.0)]);
    }

    #[test]
    fn an_integer_third_party_fraction_is_a_full_claude_pool() {
        let value = json!({
            "model": {"display_name": "Gemini 3.8 Flash (High)"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 0.8521118, "reset_in_seconds": 16917},
                "gemini-weekly": {"remaining_fraction": 0.6342774, "reset_in_seconds": 490475},
                "3p-5h": {"remaining_fraction": 1, "reset_in_seconds": 17908},
                "3p-weekly": {"remaining_fraction": 1, "reset_in_seconds": 604708}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert_rows(
            &snapshot,
            &[("g7d", 63.42774), ("c5h", 100.0), ("c7d", 100.0)],
        );
    }

    #[test]
    fn a_tighter_claude_five_hour_keeps_the_claude_week() {
        let value = json!({
            "model": {"display_name": "Orchestrator (Carbon)"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 0.96, "reset_in_seconds": 17040},
                "gemini-weekly": {"remaining_fraction": 0.86, "reset_in_seconds": 577800},
                "3p-5h": {"remaining_fraction": 0.70, "reset_in_seconds": 5160},
                "3p-weekly": {"remaining_fraction": 0.88, "reset_in_seconds": 577800}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert_rows(&snapshot, &[("g7d", 86.0), ("c5h", 70.0), ("c7d", 88.0)]);
    }

    #[test]
    fn claude_and_gemini_rows_stay_labeled_as_their_own_pools() {
        let value = json!({
            "model": {"display_name": "Claude Sonnet 4.5"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 0.0, "reset_in_seconds": 1000},
                "gemini-weekly": {"remaining_fraction": 0.8, "reset_in_seconds": 7200},
                "3p-5h": {"remaining_fraction": 0.52, "reset_in_seconds": 5000},
                "3p-weekly": {"remaining_fraction": 0.84, "reset_in_seconds": 90000}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert_rows(&snapshot, &[("g5h", 0.0), ("cla", 52.0), ("g7d", 80.0)]);
        assert!(snapshot
            .windows
            .iter()
            .all(|window| window.display_label() != "api"));
    }

    #[test]
    fn an_unnamed_model_still_shows_each_pool() {
        let value = json!({
            "model": {"display_name": "Future Model XYZ"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 0.9, "reset_in_seconds": 1000},
                "3p-5h": {"remaining_fraction": 0.3, "reset_in_seconds": 5000}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert_rows(&snapshot, &[("gem", 90.0), ("cla", 30.0)]);
    }

    #[test]
    fn a_third_party_pool_with_room_shows_both_of_its_periods() {
        let value = json!({
            "model": {"display_name": "GPT-OSS 120B (Medium)"},
            "quota": {
                "3p-5h": {"remaining_fraction": 0.52, "reset_in_seconds": 5000},
                "3p-weekly": {"remaining_fraction": 0.84, "reset_in_seconds": 90000}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert_rows(&snapshot, &[("c5h", 52.0), ("c7d", 84.0)]);
        assert_eq!(
            snapshot.window(WindowKind::FiveHour).unwrap().resets_at,
            Some(ResetAt::from_unix_seconds(5000))
        );
    }

    /// A percentage below 1 must not be mistaken for a fraction. Reading
    /// `remaining_percent: 1.0` as "full" paints a nearly exhausted pool green.
    #[test]
    fn a_small_remaining_percent_is_not_rescaled_to_a_full_pool() {
        for (key, raw, expected_remaining) in [
            ("remaining_percent", 1.0, 1.0),
            ("remaining_percent", 0.5, 0.5),
            ("remainingPercentage", 5.0, 5.0),
            ("remaining_fraction", 1.0, 100.0),
            ("remainingFraction", 0.5, 50.0),
        ] {
            let value = json!({"quota": {"gemini-5h": {key: raw}}});
            let snapshot = parse_statusline(&value, 0).unwrap();
            assert_remaining_pct(&snapshot, WindowKind::FiveHour, expected_remaining);
        }
    }

    #[test]
    fn a_fraction_key_wins_over_a_percent_key_in_the_same_bucket() {
        let value = json!({"quota": {"gemini-5h": {
            "remaining_fraction": 0.25,
            "remaining_percent": 25.0
        }}});
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert_remaining_pct(&snapshot, WindowKind::FiveHour, 25.0);
    }

    #[test]
    fn out_of_range_and_non_finite_remaining_values_fail_closed() {
        let clamped = parse_statusline(
            &json!({"quota": {"gemini-5h": {"remaining_percent": 150.0}}}),
            0,
        )
        .unwrap();
        assert_remaining_pct(&clamped, WindowKind::FiveHour, 100.0);

        let negative = parse_statusline(
            &json!({"quota": {"gemini-5h": {"remaining_fraction": -1.0}}}),
            0,
        )
        .unwrap();
        assert_remaining_pct(&negative, WindowKind::FiveHour, 0.0);

        assert!(parse_statusline(
            &json!({"quota": {"gemini-5h": {"remaining_fraction": "lots"}}}),
            0
        )
        .is_err());
    }

    fn assert_rows(snapshot: &ProviderSnapshot, rows: &[(&str, f64)]) {
        assert_eq!(
            snapshot.windows.len(),
            rows.len(),
            "labels {:?}",
            snapshot
                .windows
                .iter()
                .map(UsageWindow::display_label)
                .collect::<Vec<_>>()
        );
        for (window, (label, remaining)) in snapshot.windows.iter().zip(rows) {
            assert_eq!(window.display_label(), *label);
            assert!(
                (window.remaining_percent - remaining).abs() < 1e-9,
                "{label} remaining {} != {remaining}",
                window.remaining_percent
            );
        }
    }

    fn assert_remaining_pct(snapshot: &ProviderSnapshot, kind: WindowKind, expected: f64) {
        let actual = snapshot.window(kind).unwrap().remaining_percent;
        assert!(
            (actual - expected).abs() < 1e-9,
            "{kind:?} remaining {actual} != {expected}"
        );
    }

    #[test]
    fn annotation_title_is_the_conversation_summary() {
        let home = tempfile::tempdir().unwrap();
        let id = "aee18af1-1316-4f4e-a62e-a8e74423329a";
        write_annotation(home.path(), id, "Miao story");
        assert_eq!(
            conversation_title_in(home.path(), id).as_deref(),
            Some("Miao story")
        );
    }

    #[test]
    fn annotation_title_accepts_the_unspaced_field_the_cli_writes() {
        let home = tempfile::tempdir().unwrap();
        let id = "22222222-2222-2222-2222-222222222222";
        let dir = home.path().join("annotations");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{id}.pbtxt")), "title:\"Unspaced\"\n").unwrap();
        assert_eq!(
            conversation_title_in(home.path(), id).as_deref(),
            Some("Unspaced")
        );
    }

    #[test]
    fn annotation_title_unescapes_quotes_and_rejects_an_empty_title() {
        let home = tempfile::tempdir().unwrap();
        let id = "11111111-1111-1111-1111-111111111111";
        write_annotation(home.path(), id, r#"say \"hi\""#);
        // The helper writes the value raw inside quotes, so the file contains
        // the backslash escapes and the parser turns them back into quotes.
        assert_eq!(
            conversation_title_in(home.path(), id).as_deref(),
            Some("say \"hi\"")
        );
        write_annotation(home.path(), id, "   ");
        assert_eq!(conversation_title_in(home.path(), id), None);
    }

    #[test]
    fn a_subagent_uses_its_parent_conversation_title() {
        let home = tempfile::tempdir().unwrap();
        let parent = "f300c5e0-1616-456b-9076-15058b0203e5";
        let child = "249f6bd0-8a10-40f4-a58b-afd6225629f5";
        write_annotation(home.path(), parent, "Parent summary");
        // The CLI stores the subagent's own id in conversationId. The parent
        // conversation is the brain directory that holds the record.
        write_subagent(home.path(), parent, child, child);
        assert_eq!(
            conversation_title_in(home.path(), child).as_deref(),
            Some("Parent summary")
        );
        // The child's own annotation wins when Agy has written one.
        write_annotation(home.path(), child, "Child summary");
        assert_eq!(
            conversation_title_in(home.path(), child).as_deref(),
            Some("Child summary")
        );
    }

    #[test]
    fn disagreeing_subagent_parents_are_not_a_title() {
        let home = tempfile::tempdir().unwrap();
        let child = "249f6bd0-8a10-40f4-a58b-afd6225629f5";
        let first = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let second = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        write_annotation(home.path(), first, "One");
        write_annotation(home.path(), second, "Two");
        write_subagent(home.path(), first, child, first);
        write_subagent(home.path(), second, child, second);
        assert_eq!(conversation_title_in(home.path(), child), None);
    }

    #[test]
    fn a_session_id_cannot_escape_the_cli_directory() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            conversation_title_in(home.path(), "../annotations/secret"),
            None
        );
        assert_eq!(conversation_title_in(home.path(), ""), None);
    }

    fn write_annotation(home: &std::path::Path, id: &str, title: &str) {
        let dir = home.join("annotations");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.pbtxt")),
            format!("title: \"{title}\"\n"),
        )
        .unwrap();
    }

    fn write_subagent(home: &std::path::Path, parent: &str, child: &str, conversation_id: &str) {
        let dir = home
            .join("brain")
            .join(parent)
            .join(".system_generated/subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{child}.json")),
            format!(
                r#"{{"conversationId":"{conversation_id}","subagentDescriptor":{{"role":"task"}}}}"#
            ),
        )
        .unwrap();
    }
}
