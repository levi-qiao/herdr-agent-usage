pub mod agy;
pub mod claude;
pub mod codex;
pub mod cursor;
pub mod devin;
pub mod grok;
pub mod kilo;
pub mod kimi;
pub mod muse;
pub mod omp;
pub mod opencode_go;
pub mod statusline;

use sha2::{Digest, Sha256};
use thiserror::Error;

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    /// Environment variables are process-global. Keep provider tests that
    /// redirect credential paths from observing one another's values.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn env_guard() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }
}

pub(crate) fn credential_id(key: &str) -> String {
    format!("key:{:x}", Sha256::digest(key.trim().as_bytes()))
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider credentials are unavailable")]
    MissingCredentials,
    #[error("provider quota is unavailable: {0}")]
    Unavailable(String),
    #[error("provider response is not supported: {0}")]
    UnsupportedResponse(String),
    #[error("provider request failed: {0}")]
    Request(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Provider, ProviderSnapshot};
    #[test]
    fn all_direct_collectors_reject_unknown_or_other_account_snapshots() {
        for provider in [
            Provider::Codex,
            Provider::Grok,
            Provider::Devin,
            Provider::Muse,
            Provider::Cursor,
            Provider::OpenCodeGo,
            Provider::Kilo,
        ] {
            let mut cached = ProviderSnapshot::new(provider, vec![], 100);
            assert!(!cached.usable_for_account(Some("new"), Some(1)));
            cached.account_id = Some("old".into());
            assert!(!cached.usable_for_account(Some("new"), Some(1)));
            assert!(cached.usable_for_account(Some("old"), Some(200)));
        }
        let a = credential_id("key-a");
        assert_ne!(a, credential_id("key-b"));
        assert!(!a.contains("key-a"));
    }
}
