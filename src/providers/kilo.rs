//! Kilo Code's Kilo Pass allowance.
//!
//! Kilo has no rolling 5h or 7d bucket. Its subscription is a monthly credit
//! allowance, and it is the only allowance Kilo publishes for a signed-in
//! account, so this collector produces one [`WindowKind::Monthly`] window and
//! nothing else. A pane with no monthly window shows no quota rather than a
//! fabricated short window — see `providers::opencode_go` for a source that
//! does have 5h/7d/30d.
//!
//! The reading is the account's own Kilo Pass state:
//! `GET {KILO_API_URL}/api/trpc/kiloPass.getState`, the same tRPC procedure the
//! CLI calls for the "Kilo Pass" line in its account panel, authenticated with
//! the OAuth device login Kilo keeps in `auth.json`. That login is the serving
//! principal for a pane whose session names the `kilo` provider, so the reading
//! is stamped with that login's identity and a different account's cached value
//! can never answer for it.
//!
//! Two payloads answer "no meter", both of them normal:
//!
//! 1. `subscription: null` — the account pays from a shared credit balance
//!    instead of a plan. [`crate::providers::kilo`] has no balance window: the
//!    balance endpoint reports a dollar amount with no limit attached, and a
//!    percentage invented from it would be a guess.
//! 2. A status outside the CLI's own set (`active`, `past_due`, `trialing`) —
//!    a cancelled or unpaid plan has nothing left to meter.
//!
//! Everything fails closed: a missing, malformed, or unexpected field drops the
//! window instead of reading as 0% used, and a request failure is an error, so
//! the caller keeps the last good reading for this same account.

use crate::cache::CacheStore;
use crate::kilo::GatewayCredential;
use crate::model::{Provider, ProviderSnapshot, ResetAt, UsageWindow, WindowKind};
use crate::providers::ProviderError;
use anyhow::{Context, Result};
use serde_json::Value;

/// Official host and path. The credential is only ever sent here; a redirect
/// away from this host drops the request rather than following it.
///
/// Kilo lets `KILO_API_URL` move the API host for its own runs. This collector
/// does not honour it: an account's allowance lives on the official control
/// plane, and a configured override that pointed somewhere else would send the
/// login to a host this plugin cannot vouch for.
const KILO_PASS_URL: &str = "https://api.kilo.ai/api/trpc/kiloPass.getState";

/// tRPC batch envelope for a single zero-argument procedure.
const KILO_PASS_QUERY: &str = "batch=1&input=%7B%220%22%3Anull%7D";

/// Statuses the CLI itself treats as a live subscription. A status outside this
/// set is not metered: the plan is not paying for the session.
const LIVE_STATUSES: [&str; 3] = ["active", "past_due", "trialing"];

pub fn fetch(credential: &GatewayCredential) -> Result<PassOutcome> {
    let access = credential.access.trim();
    if access.is_empty() {
        return Err(ProviderError::MissingCredentials.into());
    }
    let agent = crate::providers::http_agent();
    let response = agent
        .get(&format!("{KILO_PASS_URL}?{KILO_PASS_QUERY}"))
        .set("Authorization", &format!("Bearer {access}"))
        .set("Accept", "application/json")
        .call()
        .map_err(|error| ProviderError::Request(http_error_status(&error)))?;
    let value: Value = response.into_json().context("decode Kilo Pass response")?;
    parse_pass_state(&value, CacheStore::now_unix())
        .map(|outcome| outcome.with_account_id(Some(credential.account_id.clone())))
        .map_err(anyhow::Error::from)
}

/// What one successful `kiloPass.getState` response means.
///
/// Both variants carry a snapshot that is safe to save for this account: the
/// first with a window, the second without one.
#[derive(Debug, Clone, PartialEq)]
pub enum PassOutcome {
    /// A metered allowance for the current period.
    Allowance(ProviderSnapshot),
    /// Kilo answered, and the account has no consumable Pass. The snapshot
    /// carries no window, so saving it clears whatever this account had.
    NoPass(ProviderSnapshot),
}

impl PassOutcome {
    /// The snapshot to persist for this account.
    pub fn snapshot(self) -> ProviderSnapshot {
        match self {
            Self::Allowance(snapshot) | Self::NoPass(snapshot) => snapshot,
        }
    }
}

/// Build a snapshot from the account's Kilo Pass state.
///
/// The tRPC envelope is unwrapped exactly the way the CLI unwraps it: a batch
/// reply is an array, `result.data` holds the payload, and a single-object reply
/// with no `json` wrapper is the payload itself. Any other shape is an error
/// rather than a reading.
pub fn parse_pass_state(value: &Value, now_unix: u64) -> Result<PassOutcome, ProviderError> {
    // A null subscription, or a shape this collector does not understand. Only
    // the first is an answer; the second has to stay an error so it clears
    // nothing.
    let subscription = match pass_state(value) {
        Some(subscription) => subscription,
        None if has_null_subscription(value) => {
            return Ok(PassOutcome::NoPass(no_pass_snapshot(now_unix)));
        }
        None => {
            return Err(ProviderError::UnsupportedResponse(
                "missing kiloPass subscription".to_string(),
            ));
        }
    };
    if let Some(status) = subscription.get("status").and_then(Value::as_str) {
        if !LIVE_STATUSES.contains(&status) {
            // The CLI drops the reading here too, and reads the account as
            // having nothing to meter — which is an answer, not a failure.
            return Ok(PassOutcome::NoPass(no_pass_snapshot(now_unix)));
        }
    }

    // A meter needs both halves of its ratio. The CLI drops the reading unless
    // at least one of the two amounts is present; a meter needs both, so this is
    // stricter: a missing spend would read as an untouched period, which
    // presents as a full allowance. A plan that named amounts but not a usable
    // ratio has not said it lost its Pass, so it errors rather than clearing.
    let used = usd(subscription.get("currentPeriodUsageUsd")).ok_or_else(|| {
        ProviderError::UnsupportedResponse("missing kiloPass current-period usage".to_string())
    })?;
    let base = usd(subscription.get("currentPeriodBaseCreditsUsd")).ok_or_else(|| {
        ProviderError::UnsupportedResponse("missing kiloPass current-period allowance".to_string())
    })?;
    // Bonus credits are granted into the same period and expire with it, so
    // they are allowance, not a top-up outside the window. A plan with none
    // reads as its base alone.
    let bonus = usd(subscription.get("currentPeriodBonusCreditsUsd")).unwrap_or(0.0);
    let limit = base + bonus;
    if limit <= 0.0 {
        return Err(ProviderError::UnsupportedResponse(
            "kiloPass current-period allowance is not positive".to_string(),
        ));
    }
    let used_percent = (used / limit * 100.0).clamp(0.0, 100.0);
    let reset = subscription
        .get("nextBillingAt")
        .or_else(|| subscription.get("nextRenewalAt"))
        .and_then(Value::as_str)
        .and_then(ResetAt::parse);

    let window = UsageWindow::new(WindowKind::Monthly, used_percent, reset)
        .map_err(|error| ProviderError::UnsupportedResponse(error.to_string()))?;
    Ok(PassOutcome::Allowance(ProviderSnapshot::new(
        Provider::Kilo,
        vec![window],
        now_unix,
    )))
}

/// The empty snapshot saved for an account Kilo says has no consumable Pass.
///
/// It names the provider so the cache file for this account still exists, and
/// carries no window, which is what clears the `30d` this account had. Saving
/// it is scoped to the same login: `fetch` stamps the account identity, and
/// `usable_for_account` refuses a snapshot whose stamp does not match.
fn no_pass_snapshot(now_unix: u64) -> ProviderSnapshot {
    ProviderSnapshot::new(Provider::Kilo, Vec::new(), now_unix)
}

impl PassOutcome {
    /// Stamps the reading with the login it came from, so a cached snapshot
    /// can be refused when the account changes.
    fn with_account_id(self, account_id: Option<String>) -> Self {
        let stamp = |snapshot: ProviderSnapshot| snapshot.with_account_id(account_id.clone());
        match self {
            Self::Allowance(snapshot) => Self::Allowance(stamp(snapshot)),
            Self::NoPass(snapshot) => Self::NoPass(stamp(snapshot)),
        }
    }
}

/// Whether the response carried a `subscription` key that was explicitly null,
/// as opposed to one that was never there or was a shape this does not know.
fn has_null_subscription(value: &Value) -> bool {
    let Some(container) = subscription_container(value) else {
        return false;
    };
    matches!(container.get("subscription"), Some(Value::Null))
}

/// The object the `subscription` key lives on, through the tRPC envelope.
fn subscription_container(value: &Value) -> Option<&Value> {
    let root = match value {
        Value::Array(items) => items.first()?,
        other => other,
    };
    let data = root.get("result")?.get("data")?;
    Some(data.get("json").unwrap_or(data))
}

/// The `subscription` object, through whichever tRPC envelope it arrives in.
///
/// Returns `None` for an account on a shared balance instead of a plan — the
/// same "no subscription" the CLI reads, and the case that must degrade quietly
/// rather than show a number.
fn pass_state(value: &Value) -> Option<&Value> {
    let subscription = subscription_container(value)?.get("subscription")?;
    subscription.is_object().then_some(subscription)
}

/// Kilo sends these amounts as JSON numbers in US dollars; a string spelling is
/// accepted because the same value has been spelled both ways upstream. A
/// non-finite or negative amount is not a reading.
fn usd(value: Option<&Value>) -> Option<f64> {
    let amount = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    (amount.is_finite() && amount >= 0.0).then_some(amount)
}

/// Never let an auth or transport failure reach the cache as a quota value.
/// The caller keeps the last good snapshot for this same account instead.
fn http_error_status(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Status(401 | 403, _) => "HTTP 401/403 (invalid credentials)".to_string(),
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(error) => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_787_000_000;

    /// The snapshot out of an allowance outcome, for the window assertions.
    fn allowance(value: &Value) -> ProviderSnapshot {
        match parse_pass_state(value, NOW).unwrap() {
            PassOutcome::Allowance(snapshot) => snapshot,
            PassOutcome::NoPass(_) => panic!("expected an allowance, got no Pass"),
        }
    }

    /// The empty snapshot a no-Pass answer produces.
    fn empty(now: u64) -> ProviderSnapshot {
        ProviderSnapshot::new(Provider::Kilo, Vec::new(), now)
    }

    /// The deployed shape, with the amounts kept as the API sends them: JSON
    /// numbers in US dollars. `batch=1` replies as a one-item array.
    fn subscribed() -> Value {
        json!([{"result": {"data": {"subscription": {
            "currentPeriodBaseCreditsUsd": 20.0,
            "currentPeriodUsageUsd": 3.42,
            "currentPeriodBonusCreditsUsd": 10.0,
            "nextBillingAt": "2026-10-11T10:09:35.000Z",
            "status": "active"
        }}}}])
    }

    #[test]
    fn a_pass_account_reads_one_monthly_window() {
        let snapshot = allowance(&subscribed());
        assert_eq!(snapshot.provider, Provider::Kilo);
        assert_eq!(snapshot.source, Provider::Kilo.source());
        assert_eq!(snapshot.windows.len(), 1);
        let month = snapshot.window(WindowKind::Monthly).unwrap();
        // 3.42 of a 30.00 allowance (20 base + 10 bonus).
        assert!((month.used_percent - 11.4).abs() < 0.01);
        assert!((month.remaining_percent - 88.6).abs() < 0.01);
        assert_eq!(month.resets_at, ResetAt::parse("2026-10-11T10:09:35.000Z"));
    }

    /// Kilo has no 5h or 7d bucket, so neither token may ever appear for a
    /// Kilo pane. A monthly-only reading is the shape, not a gap.
    #[test]
    fn kilo_publishes_no_short_windows() {
        let snapshot = allowance(&subscribed());
        assert!(snapshot.window(WindowKind::FiveHour).is_none());
        assert!(snapshot.window(WindowKind::Weekly).is_none());
    }

    /// Kilo answered, and answered that this account has nothing to meter.
    /// That is a reading, not a failure, so it must not be reported as an error.
    #[test]
    fn an_account_on_a_shared_balance_reads_as_no_pass() {
        // What Kilo actually answers for an account with no plan: HTTP 200 and
        // a null subscription. That is not an error to surface and not a 0%.
        let free = json!([{"result": {"data": {
            "subscription": null,
            "isEligibleForFirstMonthPromo": false
        }}}]);
        assert_eq!(
            parse_pass_state(&free, NOW).unwrap(),
            PassOutcome::NoPass(empty(NOW))
        );
    }

    #[test]
    fn a_plan_that_is_not_paying_yet_is_not_metered() {
        for status in ["canceled", "incomplete", "unpaid", ""] {
            let value = json!([{"result": {"data": {"subscription": {
                "currentPeriodBaseCreditsUsd": 20.0,
                "currentPeriodUsageUsd": 1.0,
                "status": status
            }}}}]);
            assert_eq!(
                parse_pass_state(&value, NOW).unwrap(),
                PassOutcome::NoPass(empty(NOW)),
                "status {status}"
            );
        }
        // An absent status is not a rejected status: the plan is metered.
        let unstated = json!([{"result": {"data": {"subscription": {
            "currentPeriodBaseCreditsUsd": 20.0,
            "currentPeriodUsageUsd": 1.0
        }}}}]);
        assert_eq!(
            parse_pass_state(&unstated, NOW)
                .unwrap()
                .snapshot()
                .windows
                .len(),
            1
        );
    }

    #[test]
    fn a_plan_without_bonus_credits_reads_against_its_base() {
        let value = json!([{"result": {"data": {"subscription": {
            "currentPeriodBaseCreditsUsd": 20.0,
            "currentPeriodUsageUsd": 5.0,
            "currentPeriodBonusCreditsUsd": 0.0,
            "nextRenewalAt": "2026-10-01T00:00:00.000Z",
            "status": "trialing"
        }}}}]);
        let snapshot = allowance(&value);
        let month = snapshot.window(WindowKind::Monthly).unwrap();
        assert!((month.used_percent - 25.0).abs() < 0.01);
        assert_eq!(month.resets_at, ResetAt::parse("2026-10-01T00:00:00.000Z"));
    }

    #[test]
    fn the_envelope_is_unwrapped_the_way_the_cli_unwraps_it() {
        // Single-object reply with no batch array.
        let bare = json!({"result": {"data": {"subscription": {
            "currentPeriodBaseCreditsUsd": 10.0,
            "currentPeriodUsageUsd": 5.0
        }}}, });
        assert!(parse_pass_state(&bare, NOW).is_ok());
        // `json` wrapper inside `result.data`.
        let wrapped = json!([{"result": {"data": {"json": {"subscription": {
            "currentPeriodBaseCreditsUsd": 10.0,
            "currentPeriodUsageUsd": 5.0
        }}}}}]);
        assert!(parse_pass_state(&wrapped, NOW).is_ok());
    }

    #[test]
    fn every_unknown_shape_fails_closed() {
        for payload in [
            json!({}),
            json!([]),
            json!(null),
            json!([{"error": {"json": {"message": "unauthorized"}}}]),
            json!([{"result": {"data": {}}}]),
            json!([{"result": {"data": {"subscription": {}}}}]),
            json!([{"result": {"data": {"subscription": []}}}]),
            json!([{"result": {"data": {"subscription": {
                "currentPeriodBaseCreditsUsd": 0.0,
                "currentPeriodUsageUsd": 4.0
            }}}}]),
            json!([{"result": {"data": {"subscription": {
                "currentPeriodBaseCreditsUsd": "lots",
                "currentPeriodUsageUsd": 1.0
            }}}}]),
            json!([{"result": {"data": {"subscription": {
                "currentPeriodBaseCreditsUsd": -20.0,
                "currentPeriodUsageUsd": 1.0
            }}}}]),
        ] {
            assert!(
                parse_pass_state(&payload, NOW).is_err(),
                "accepted {payload}"
            );
        }
    }

    /// A missing allowance must never read as a full one, and a missing usage
    /// must never read as an untouched period.
    #[test]
    fn a_half_reported_period_drops_the_window_rather_than_defaulting_it() {
        let no_usage = json!([{"result": {"data": {"subscription": {
            "currentPeriodBaseCreditsUsd": 20.0
        }}}}]);
        assert!(parse_pass_state(&no_usage, NOW).is_err());

        let no_allowance = json!([{"result": {"data": {"subscription": {
            "currentPeriodUsageUsd": 3.0
        }}}}]);
        assert!(parse_pass_state(&no_allowance, NOW).is_err());

        // Bonus alone with no base is not an allowance either.
        let bonus_only = json!([{"result": {"data": {"subscription": {
            "currentPeriodBonusCreditsUsd": 10.0,
            "currentPeriodUsageUsd": 3.0
        }}}}]);
        assert!(parse_pass_state(&bonus_only, NOW).is_err());
    }

    #[test]
    fn amounts_are_accepted_as_strings_too_and_clamped_not_trusted() {
        let strings = json!([{"result": {"data": {"subscription": {
            "currentPeriodBaseCreditsUsd": "20.00",
            "currentPeriodUsageUsd": "3.42",
            "currentPeriodBonusCreditsUsd": "0"
        }}}}]);
        let snapshot = allowance(&strings);
        assert!((snapshot.window(WindowKind::Monthly).unwrap().used_percent - 17.1).abs() < 0.01);

        // Spending past the allowance is 100% used, not a negative remainder.
        let over = json!([{"result": {"data": {"subscription": {
            "currentPeriodBaseCreditsUsd": 20.0,
            "currentPeriodUsageUsd": 44.0
        }}}}]);
        let snapshot = allowance(&over);
        let month = snapshot.window(WindowKind::Monthly).unwrap();
        assert_eq!(month.used_percent, 100.0);
        assert_eq!(month.remaining_percent, 0.0);
    }

    #[test]
    fn credentials_never_appear_in_an_error() {
        let credential = GatewayCredential {
            access: "   ".to_string(),
            account_id: "acct".to_string(),
        };
        let error = fetch(&credential).unwrap_err().to_string();
        assert!(error.contains("credentials"), "{error}");
    }

    #[test]
    fn every_transport_and_status_failure_maps_to_an_error_not_a_quota_value() {
        let rate_limited = http_error_status(&ureq::Error::Status(
            429,
            ureq::Response::new(429, "Too Many Requests", "").unwrap(),
        ));
        assert_eq!(rate_limited, "HTTP 429");

        for code in [401, 403] {
            let status = http_error_status(&ureq::Error::Status(
                code,
                ureq::Response::new(code, "denied", "").unwrap(),
            ));
            assert!(status.contains("invalid credentials"), "{code}: {status}");
        }

        for code in [400, 429, 500, 503] {
            let status = http_error_status(&ureq::Error::Status(
                code,
                ureq::Response::new(code, "x", "").unwrap(),
            ));
            assert!(!status.contains('%'), "{code}: {status}");
        }
    }

    #[test]
    fn the_endpoint_is_the_official_host_only() {
        assert!(KILO_PASS_URL.starts_with("https://api.kilo.ai/api/trpc/"));
        assert_eq!(
            KILO_PASS_URL,
            format!("https://api.kilo.ai/api/trpc/kiloPass.getState")
        );
        // The procedure is named in the URL and the query carries only the
        // zero-argument envelope: no account id travels in the request.
        assert!(KILO_PASS_QUERY.contains("%220%22"));
    }
}

/// The regression set for a stale `30d` outliving the plan that produced it.
///
/// Kilo upstream reads a null subscription and a non-live status as "no
/// consumable Pass" rather than as a failed request. Treating those as fetch
/// failures left the previous window on screen for the same account, which is
/// the confidently-wrong case this collector exists to avoid. A request that
/// genuinely failed must still leave the last good reading alone.
#[cfg(test)]
mod clearing {
    use super::*;
    use crate::cache::CacheStore;
    use crate::model::{BillingTarget, Provider, UsageWindow, WindowKind};
    use serde_json::json;
    use std::path::Path;
    use tempfile::tempdir;

    const NOW: u64 = 1_787_000_000;

    fn saved_quota(account: &str, now: u64) -> ProviderSnapshot {
        let mut snapshot = ProviderSnapshot::new(
            Provider::Kilo,
            vec![UsageWindow::new(WindowKind::Monthly, 42.0, None).unwrap()],
            now,
        );
        snapshot.account_id = Some(account.to_string());
        snapshot
    }

    /// A cached reading is only readable by the account it was stamped with.
    fn cached_month(cache: &CacheStore, account: &str) -> Option<f64> {
        cache
            .load_target(&BillingTarget::kilo_gateway())
            .ok()
            .flatten()
            .filter(|snapshot| snapshot.usable_for_account(Some(account), Some(NOW)))
            .and_then(|snapshot| snapshot.windows.first().map(|w| w.used_percent))
    }

    fn cache_for(directory: &Path) -> CacheStore {
        CacheStore::new(directory.join("state"))
    }

    fn with_cache<T>(run: impl FnOnce(&CacheStore) -> T) -> T {
        let directory = tempdir().unwrap();
        let cache = cache_for(directory.path());
        run(&cache)
    }

    fn pass_now() -> u64 {
        CacheStore::now_unix()
    }

    /// A saved window disappears when the API next says the account has none.
    ///
    /// `subscription: null` is how Kilo reports an account that pays from a
    /// shared credit balance, so it is an answer about the account, not an
    /// error about the request.
    #[test]
    fn an_old_quota_clears_when_the_api_reports_no_pass() {
        with_cache(|cache| {
            let account = "key:alice";
            cache.save(&saved_quota(account, pass_now())).unwrap();
            assert_eq!(cached_month(cache, account), Some(42.0));

            let outcome = parse_pass_state(
                &json!([{"result": {"data": {"subscription": null}}}]),
                pass_now(),
            )
            .unwrap()
            .with_account_id(Some(account.to_string()));
            crate::refresh::apply_kilo_outcome(cache, Ok(outcome));

            // The stale 30d is gone, and the cache still names this account.
            assert_eq!(cached_month(cache, account), None);
        });
    }

    /// A plan that was live and is now cancelled must lose its window.
    #[test]
    fn a_cancelled_plan_clears_a_window_that_was_there() {
        with_cache(|cache| {
            let account = "key:bob";
            cache.save(&saved_quota(account, pass_now())).unwrap();

            for status in ["canceled", "unpaid", "incomplete"] {
                let value = json!([{"result": {"data": {"subscription": {
                    "currentPeriodBaseCreditsUsd": 20.0,
                    "currentPeriodUsageUsd": 1.0,
                    "status": status
                }}}}]);
                let outcome = parse_pass_state(&value, pass_now())
                    .unwrap()
                    .with_account_id(Some(account.to_string()));
                crate::refresh::apply_kilo_outcome(cache, Ok(outcome));
                assert_eq!(cached_month(cache, account), None, "status {status}");
            }
        });
    }

    /// The other half of the rule: when the request fails, nothing is written,
    /// so the last good reading survives. A server error is not evidence that
    /// the plan ended.
    #[test]
    fn a_failed_request_preserves_the_old_quota() {
        with_cache(|cache| {
            let account = "key:carol";
            cache.save(&saved_quota(account, pass_now())).unwrap();
            assert_eq!(cached_month(cache, account), Some(42.0));

            // Exactly what refresh_kilo does with a failed fetch.
            for error in [
                ProviderError::Request("HTTP 500".to_string()),
                ProviderError::Request("HTTP 429".to_string()),
                ProviderError::Request("HTTP 401/403 (invalid credentials)".to_string()),
                ProviderError::Request("connection refused".to_string()),
                ProviderError::UnsupportedResponse("malformed JSON response".to_string()),
                ProviderError::UnsupportedResponse("kiloPass current-period usage".to_string()),
            ] {
                crate::refresh::apply_kilo_outcome(cache, Err(anyhow::Error::from(error)));
                assert_eq!(
                    cached_month(cache, account),
                    Some(42.0),
                    "a failed request cleared the pane"
                );
            }
        });
    }

    /// `fetch` stamps whichever outcome it returns with the login, so a cleared
    /// snapshot is still scoped to the account it was measured for.
    #[test]
    fn a_cleared_snapshot_is_stamped_with_the_same_login() {
        let credential = GatewayCredential {
            access: "st_access".to_string(),
            account_id: "key:dave".to_string(),
        };
        let outcome =
            parse_pass_state(&json!([{"result": {"data": {"subscription": null}}}]), NOW).unwrap();
        let snapshot = outcome
            .with_account_id(Some(credential.account_id.clone()))
            .snapshot();

        assert!(snapshot.usable_for_account(Some(&credential.account_id), Some(NOW)));
        assert!(!snapshot.usable_for_account(Some("key:someone-else"), Some(NOW)));
    }
}
