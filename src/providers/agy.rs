use crate::cache::CacheStore;
use crate::model::{Provider, ProviderSnapshot, ResetAt, UsageWindow, WindowKind};
use crate::providers::statusline::{parse_context, parse_model};
use crate::providers::ProviderError;
use serde_json::Value;

/// Spellings whose value is already a `0..=1` fraction.
const FRACTION_KEYS: [&str; 2] = ["remaining_fraction", "remainingFraction"];
/// Spellings whose value is a `0..=100` percentage.
const PERCENT_KEYS: [&str; 2] = ["remaining_percent", "remainingPercentage"];

const GEMINI_FIVE_HOUR_KEYS: [&str; 1] = ["gemini-5h"];
const GEMINI_WEEKLY_KEYS: [&str; 1] = ["gemini-weekly"];
const GEMINI_ARGON_WEEKLY_KEYS: [&str; 1] = ["gemini-argon-wtu-weekly"];
const THIRD_PARTY_FIVE_HOUR_KEYS: [&str; 1] = ["3p-5h"];
const THIRD_PARTY_WEEKLY_KEYS: [&str; 1] = ["3p-weekly"];
/// Third-party (Claude/GPT) pool shown as `api` while Gemini is the active model.
const THIRD_PARTY_API_KEYS: [&str; 2] = ["3p-5h", "3p-weekly"];

/// The quota pool that the active model draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pool {
    Gemini,
    GeminiArgon,
    ThirdParty,
}

impl Pool {
    fn five_hour_keys(self) -> &'static [&'static str] {
        match self {
            Self::Gemini | Self::GeminiArgon => &GEMINI_FIVE_HOUR_KEYS,
            Self::ThirdParty => &THIRD_PARTY_FIVE_HOUR_KEYS,
        }
    }

    fn weekly_keys(self, quota: &serde_json::Map<String, Value>) -> &'static [&'static str] {
        match self {
            Self::Gemini => &GEMINI_WEEKLY_KEYS,
            Self::GeminiArgon => {
                if has_any_key(quota, &GEMINI_ARGON_WEEKLY_KEYS) {
                    &GEMINI_ARGON_WEEKLY_KEYS
                } else {
                    &GEMINI_WEEKLY_KEYS
                }
            }
            Self::ThirdParty => &THIRD_PARTY_WEEKLY_KEYS,
        }
    }
}

fn has_any_key(quota: &serde_json::Map<String, Value>, keys: &[&str]) -> bool {
    keys.iter().any(|key| quota.contains_key(*key))
}

/// Detect which quota pool the active model draws from.
///
/// Gemini-family model names contain `gemini`, `flash`, or `learnlm`.
/// Gemini Argon models contain `argon` and draw their weekly quota from
/// `gemini-argon-wtu-weekly`.
/// Third-party model names include Claude variants (`claude`, `sonnet`,
/// `haiku`, `opus`), GPT models, and OpenAI reasoning series (`o1`, `o3`,
/// `o4`). Unknown names require an unambiguous single-pool response.
fn active_pool(model: Option<&str>) -> Option<Pool> {
    let lower = model?.to_ascii_lowercase();
    if lower.contains("argon") {
        Some(Pool::GeminiArgon)
    } else if lower.contains("gemini") || lower.contains("flash") || lower.contains("learnlm") {
        Some(Pool::Gemini)
    } else if lower.contains("claude")
        || lower.contains("sonnet")
        || lower.contains("haiku")
        || lower.contains("opus")
        || lower.contains("gpt")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        Some(Pool::ThirdParty)
    } else {
        None
    }
}

/// Parse the quota object emitted by Agy/Antigravity's statusLine JSON.
///
/// Agy reports separate Gemini and third-party (Claude/GPT) pools. When the
/// active model can be identified, only its pool's quota is shown so the
/// sidebar reflects the limit that actually applies to the current session.
/// An unknown model with two possible pools supplies diagnostics only.
pub fn parse_statusline(
    value: &Value,
    fetched_at_unix: u64,
) -> std::result::Result<ProviderSnapshot, ProviderError> {
    let quota = value
        .get("quota")
        .and_then(Value::as_object)
        .ok_or_else(|| ProviderError::UnsupportedResponse("missing quota".to_string()))?;
    let model = parse_model(value);
    let has_gemini_five_hour = has_any_key(quota, &GEMINI_FIVE_HOUR_KEYS);
    let has_gemini_weekly = has_any_key(quota, &GEMINI_WEEKLY_KEYS);
    let has_argon_weekly = has_any_key(quota, &GEMINI_ARGON_WEEKLY_KEYS);
    let has_third_party = has_any_key(quota, &THIRD_PARTY_FIVE_HOUR_KEYS)
        || has_any_key(quota, &THIRD_PARTY_WEEKLY_KEYS);
    let has_any_pool =
        has_gemini_five_hour || has_gemini_weekly || has_argon_weekly || has_third_party;
    let fallback_pool = match (
        has_gemini_five_hour,
        has_gemini_weekly,
        has_argon_weekly,
        has_third_party,
    ) {
        (_, _, false, false) if has_gemini_five_hour || has_gemini_weekly => Some(Pool::Gemini),
        (_, false, true, false) => Some(Pool::GeminiArgon),
        (false, false, false, true) => Some(Pool::ThirdParty),
        _ => None,
    };
    let pool = active_pool(model.as_deref()).or(fallback_pool);
    let five_hour_keys: &[&str] = pool.map_or(&[] as &[&str], Pool::five_hour_keys);
    let weekly_keys: &[&str] = pool.map_or(&[] as &[&str], |p| p.weekly_keys(quota));
    let mut windows = Vec::new();
    for (kind, keys) in [
        (WindowKind::FiveHour, five_hour_keys),
        (WindowKind::Weekly, weekly_keys),
    ] {
        if let Some(window) = parse_window(quota, kind, keys, fetched_at_unix)? {
            windows.push(window);
        }
    }
    // Gemini sessions still have a third-party API pool. Publish it on the
    // spare monthly slot as `api` so it does not replace the Gemini 5h/7d
    // rows. A Claude/GPT session already uses that pool for 5h/7d.
    if matches!(pool, Some(Pool::Gemini | Pool::GeminiArgon)) {
        if let Some(window) = parse_window(
            quota,
            WindowKind::Monthly,
            &THIRD_PARTY_API_KEYS,
            fetched_at_unix,
        )? {
            windows.push(window.with_source_window("api", None));
        }
    }
    if windows.is_empty() && (pool.is_some() || !has_any_pool) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_both_agy_windows_from_the_active_pool() {
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
        assert_eq!(
            snapshot.window(WindowKind::FiveHour).unwrap().resets_at,
            Some(ResetAt::from_unix_seconds(1_786_798_800))
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
    fn ignores_missing_pool_without_marking_the_window_unavailable() {
        let value = json!({
            "quota": {"gemini-weekly": {"remaining_fraction": 0.61}}
        });
        let snapshot = parse_statusline(&value, 1).unwrap();
        assert!(snapshot.window(WindowKind::FiveHour).is_none());
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
    fn routes_claude_model_to_third_party_pool() {
        // gemini-5h is exhausted (0 %), but the active model is Claude so the
        // sidebar should show the 3p-5h value (52 %) not the minimum (0 %).
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
        // 3p-5h reset_in_seconds=5000 from base 0 → resets_at 5000
        assert_eq!(
            snapshot.window(WindowKind::FiveHour).unwrap().resets_at,
            Some(ResetAt::from_unix_seconds(5000))
        );
        assert_remaining_pct(&snapshot, WindowKind::FiveHour, 52.0);
        assert_remaining_pct(&snapshot, WindowKind::Weekly, 84.0);
    }

    #[test]
    fn routes_gemini_model_to_gemini_pool() {
        // 3p-5h is low (10 %), but the active model is Gemini so the sidebar
        // should show the gemini-5h value (75 %) not the minimum (10 %).
        let value = json!({
            "model": {"display_name": "Gemini Flash"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 0.75, "reset_in_seconds": 1000},
                "gemini-weekly": {"remaining_fraction": 0.9, "reset_in_seconds": 7200},
                "3p-5h": {"remaining_fraction": 0.1, "reset_in_seconds": 5000},
                "3p-weekly": {"remaining_fraction": 0.2, "reset_in_seconds": 90000}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        // gemini-5h reset_in_seconds=1000 from base 0 → resets_at 1000
        assert_eq!(
            snapshot.window(WindowKind::FiveHour).unwrap().resets_at,
            Some(ResetAt::from_unix_seconds(1000))
        );
        assert_remaining_pct(&snapshot, WindowKind::FiveHour, 75.0);
        assert_remaining_pct(&snapshot, WindowKind::Weekly, 90.0);
        assert_remaining_pct(&snapshot, WindowKind::Monthly, 10.0);
        assert_eq!(
            snapshot
                .window(WindowKind::Monthly)
                .unwrap()
                .display_label(),
            "api"
        );
    }

    #[test]
    fn routes_gemini_argon_model_to_argon_weekly_quota() {
        let value = json!({
            "model": {"display_name": "Gemini 4 Argon (High)"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 1.0, "reset_in_seconds": 17784},
                "gemini-weekly": {"remaining_fraction": 0.9477867, "reset_in_seconds": 371488},
                "gemini-argon-wtu-weekly": {"remaining_fraction": 0.5464704, "reset_in_seconds": 450186},
                "3p-5h": {"remaining_fraction": 1.0, "reset_in_seconds": 17784},
                "3p-weekly": {"remaining_fraction": 0.4997628, "reset_in_seconds": 473570}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert_remaining_pct(&snapshot, WindowKind::FiveHour, 100.0);
        assert_remaining_pct(&snapshot, WindowKind::Weekly, 54.64704);
        assert_eq!(
            snapshot.window(WindowKind::Weekly).unwrap().resets_at,
            Some(ResetAt::from_unix_seconds(450186))
        );
        assert_remaining_pct(&snapshot, WindowKind::Monthly, 49.97628);
        assert_eq!(
            snapshot
                .window(WindowKind::Monthly)
                .unwrap()
                .display_label(),
            "api"
        );
    }

    #[test]
    fn gemini_argon_falls_back_to_gemini_weekly_when_argon_bucket_missing() {
        let value = json!({
            "model": {"display_name": "Gemini 4 Argon"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 1.0, "reset_in_seconds": 1000},
                "gemini-weekly": {"remaining_fraction": 0.85, "reset_in_seconds": 7200}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert_remaining_pct(&snapshot, WindowKind::FiveHour, 100.0);
        assert_remaining_pct(&snapshot, WindowKind::Weekly, 85.0);
    }

    #[test]
    fn parses_unambiguous_argon_weekly_bucket_without_model() {
        let value = json!({
            "quota": {
                "gemini-argon-wtu-weekly": {"remaining_fraction": 0.46, "reset_in_seconds": 3600}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert!(snapshot.window(WindowKind::FiveHour).is_none());
        assert_remaining_pct(&snapshot, WindowKind::Weekly, 46.0);
    }

    #[test]
    fn an_unknown_model_does_not_guess_between_gemini_and_argon_weekly_pools() {
        let value = json!({
            "model": {"display_name": "Future Model XYZ"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 0.9, "reset_in_seconds": 1000},
                "gemini-weekly": {"remaining_fraction": 0.85, "reset_in_seconds": 7200},
                "gemini-argon-wtu-weekly": {"remaining_fraction": 0.46, "reset_in_seconds": 3600}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert!(snapshot.windows.is_empty());
    }

    #[test]
    fn an_integer_third_party_remaining_fraction_still_renders_api() {
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
        assert_remaining_pct(&snapshot, WindowKind::FiveHour, 85.21118);
        assert_remaining_pct(&snapshot, WindowKind::Weekly, 63.42774);
        assert_remaining_pct(&snapshot, WindowKind::Monthly, 100.0);
        assert_eq!(
            snapshot
                .window(WindowKind::Monthly)
                .unwrap()
                .display_label(),
            "api"
        );
    }

    #[test]
    fn a_third_party_session_does_not_relabel_gemini_as_api() {
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
        assert!(snapshot.window(WindowKind::Monthly).is_none());
        assert_remaining_pct(&snapshot, WindowKind::FiveHour, 52.0);
        assert_remaining_pct(&snapshot, WindowKind::Weekly, 84.0);
    }

    #[test]
    fn an_unknown_model_does_not_mix_two_quota_pools() {
        let value = json!({
            "model": {"display_name": "Future Model XYZ"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 0.9, "reset_in_seconds": 1000},
                "3p-5h": {"remaining_fraction": 0.3, "reset_in_seconds": 5000}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert!(snapshot.windows.is_empty());
    }

    #[test]
    fn routes_gpt_oss_to_third_party_pool() {
        let value = json!({
            "model": {"display_name": "GPT-OSS 120B (Medium)"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 0.0, "reset_in_seconds": 1000},
                "3p-5h": {"remaining_fraction": 0.52, "reset_in_seconds": 5000},
                "3p-weekly": {"remaining_fraction": 0.84, "reset_in_seconds": 90000}
            }
        });
        let snapshot = parse_statusline(&value, 0).unwrap();
        assert_eq!(
            snapshot.window(WindowKind::FiveHour).unwrap().resets_at,
            Some(ResetAt::from_unix_seconds(5000))
        );
        assert_remaining_pct(&snapshot, WindowKind::FiveHour, 52.0);
        assert_remaining_pct(&snapshot, WindowKind::Weekly, 84.0);
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

    fn assert_remaining_pct(snapshot: &ProviderSnapshot, kind: WindowKind, expected: f64) {
        let actual = snapshot.window(kind).unwrap().remaining_percent;
        assert!(
            (actual - expected).abs() < 1e-9,
            "{kind:?} remaining {actual} != {expected}"
        );
    }
}
