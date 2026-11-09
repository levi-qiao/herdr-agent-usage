use super::statusline::{settings_path, Adapter};
use crate::cache::CacheStore;
use crate::model::Provider;
use crate::providers::agy::parse_statusline;
use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::Path;

const CONFIG: Adapter = Adapter {
    label: "Agy",
    subcommand: "agy-statusline",
    backup_file: "agy-statusline.original.json",
};

pub fn check() -> Result<()> {
    let cache = CacheStore::from_env()?;
    let executable = std::env::current_exe().context("resolve plugin executable")?;
    CONFIG.check(
        &settings_path("AGY_SETTINGS_FILE", ".gemini/antigravity-cli/settings.json")?,
        cache.root(),
        &executable,
    )
}

pub fn apply() -> Result<()> {
    let cache = CacheStore::from_env()?;
    let executable = std::env::current_exe().context("resolve plugin executable")?;
    apply_at(
        &settings_path("AGY_SETTINGS_FILE", ".gemini/antigravity-cli/settings.json")?,
        cache.root(),
        &executable,
    )
}

pub fn uninstall() -> Result<()> {
    let cache = CacheStore::from_env()?;
    uninstall_at(
        &settings_path("AGY_SETTINGS_FILE", ".gemini/antigravity-cli/settings.json")?,
        cache.root(),
    )
}

pub fn apply_at(settings: &Path, state: &Path, executable: &Path) -> Result<()> {
    CONFIG.apply(settings, state, executable)
}

pub fn uninstall_at(settings: &Path, state: &Path) -> Result<()> {
    CONFIG.uninstall(settings, state)
}

/// Keep the provider payload untouched for a user-owned statusLine command,
/// while keying the plugin's private mailbox by the Herdr pane that owns the
/// Agy process. Antigravity's PreInvocation hook can report a spawned
/// subagent conversation while statusLine still describes the parent; the
/// pane id is inherited by both and stays stable across that mismatch.
fn observation_for_cache(value: &Value, pane_id: Option<&str>) -> Value {
    let Some(pane_id) = pane_id.filter(|pane_id| !pane_id.is_empty()) else {
        return value.clone();
    };
    let mut observation = value.clone();
    if let Some(object) = observation.as_object_mut() {
        object.insert("session_id".to_string(), Value::String(pane_id.to_string()));
    }
    observation
}

/// Consume one Agy statusLine payload, cache quota silently, then preserve a
/// user-owned statusLine command when one existed before installation.
pub fn run_statusline_hook() -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    if let Ok(value) = serde_json::from_slice::<Value>(&input) {
        if let Ok(snapshot) = parse_statusline(&value, CacheStore::now_unix()) {
            if let Ok(cache) = CacheStore::from_env() {
                let pane_id = std::env::var("HERDR_PANE_ID").ok();
                let observation = observation_for_cache(&value, pane_id.as_deref());
                if cache
                    .save_statusline_observation(Provider::Agy, snapshot, &observation)
                    .is_ok()
                {
                    crate::refresh::nudge_quota_watch();
                }
            }
        }
    }
    let cache = CacheStore::from_env()?;
    let Some(output) = CONFIG.run_previous(cache.root(), &input)? else {
        return Ok(());
    };
    if output.timed_out {
        return Ok(());
    }
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stdout().flush()?;
    if output.exit_code != Some(0) {
        std::process::exit(output.exit_code.unwrap_or(1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_observation_uses_the_herdr_pane_without_mutating_provider_identity() {
        let original = json!({
            "session_id": "parent-conversation",
            "conversation_id": "parent-conversation",
            "model": {"display_name": "Gemini Flash"}
        });
        let cached = observation_for_cache(&original, Some("w1:p7"));

        assert_eq!(
            cached.get("session_id").and_then(Value::as_str),
            Some("w1:p7")
        );
        assert_eq!(
            cached.get("conversation_id").and_then(Value::as_str),
            Some("parent-conversation")
        );
        assert_eq!(
            original.get("session_id").and_then(Value::as_str),
            Some("parent-conversation")
        );
    }

    #[test]
    fn cache_observation_keeps_native_session_outside_herdr() {
        let original = json!({
            "session_id": "parent-conversation",
            "conversation_id": "parent-conversation"
        });
        assert_eq!(observation_for_cache(&original, None), original);
        assert_eq!(observation_for_cache(&original, Some("")), original);
    }
}
