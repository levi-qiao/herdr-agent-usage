use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::BTreeMap;
use thiserror::Error;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

/// Original-four quota collector (the subscription billed for a pane).
///
/// Distinct from [`Harness`], the Herdr agent drawing the pane. A Herdr agent
/// name is not itself a collector: parse it as a harness first, then take
/// [`Harness::billing`]. Cache filenames and the `provider` serde tag stay
/// 1:1 with 0.2 snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Codex,
    Grok,
    Claude,
    Agy,
    /// OpenCode's Go subscription. Deliberately absent from [`Provider::ALL`]:
    /// it has no 1:1 harness mapping and is only ever fetched for a pane that
    /// resolved to it, so the original four keep their exact refresh behavior.
    OpenCodeGo,
    /// Kilo Code's Kilo Pass credit allowance, read from the account's own
    /// subscription state. Scoped like OpenCode Go: a Kilo pane can be served
    /// by any backend, so this is only ever fetched for a pane that resolved to
    /// the Kilo Gateway.
    Kilo,
    /// Quota reported by omp's own provider-agnostic usage layer. This is a
    /// scoped collector only; it is never part of a bare provider refresh.
    Omp,
    /// Quota reported by Devin CLI's Connect RPC API. A 1:1 harness→billing
    /// mapping like the original four, refreshed through `--provider all`.
    Devin,
    /// Muse Code's subscription windows, read from the key call the CLI makes.
    /// A 1:1 harness→billing mapping refreshed through `--provider all`.
    Muse,
    /// Cursor Agent CLI's included monthly pool, read from DashboardService.
    /// A 1:1 harness→billing mapping refreshed through `--provider all`.
    Cursor,
    /// Kimi Code's 5h and weekly windows, read from the `/usages` call its CLI
    /// makes. A 1:1 harness→billing mapping refreshed through `--provider all`.
    Kimi,
}

/// Quota collector identity. The original four keep the historical
/// [`Provider`] name so 0.2 cache files and CLI flags stay compatible.
pub type Billing = Provider;

impl Provider {
    /// The collectors a bare `--provider all` refreshes. OpenCode Go is not
    /// here on purpose; see the variant's note.
    pub const ALL: [Self; 8] = [
        Self::Codex,
        Self::Grok,
        Self::Claude,
        Self::Agy,
        Self::Devin,
        Self::Muse,
        Self::Cursor,
        Self::Kimi,
    ];

    /// Collectors fetched only for a pane that resolved to them.
    ///
    /// They are never refreshed through the provider list, so the dashboard
    /// shows one only once it has something cached — a permanent
    /// "unavailable" row for a subscription the user does not have would be
    /// noise, not information.
    pub const SCOPED: [Self; 2] = [Self::OpenCodeGo, Self::Kilo];

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Grok => "Grok",
            Self::Claude => "Claude",
            Self::Agy => "Agy",
            Self::OpenCodeGo => "OpenCode Go",
            Self::Kilo => "Kilo",
            Self::Omp => "OMP",
            Self::Devin => "Devin",
            Self::Muse => "Muse",
            Self::Cursor => "Cursor",
            Self::Kimi => "Kimi",
        }
    }

    pub fn source(self) -> &'static str {
        match self {
            Self::Codex => "codex-app-server",
            Self::Grok => "grok-cli-billing",
            Self::Claude => "claude-statusline",
            Self::Agy => "agy-statusline",
            // Scoped to the OpenCode credential store so it can never collide
            // with the original four's 0.2 filenames.
            Self::OpenCodeGo => "opencode-go.opencode-store",
            // Scoped to the Kilo credential store, for the same reason.
            Self::Kilo => "kilo-pass.kilo-store",
            Self::Omp => "omp-usage",
            Self::Devin => "devin-cli-billing",
            Self::Muse => "muse-code-subscription",
            Self::Cursor => "cursor-dashboard-usage",
            Self::Kimi => "kimi-code-usages",
        }
    }
}

/// The agent drawing a Herdr pane. Distinct from [`Billing`]: harnesses without
/// a 1:1 collector may still resolve an exact session to a scoped billing
/// target (for example, Pi to canonical Codex after an account-id match).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Harness {
    Codex,
    Grok,
    Claude,
    Agy,
    OpenCode,
    Pi,
    Omp,
    Devin,
    Muse,
    Cursor,
    Kilo,
    Kimi,
}

impl Harness {
    /// Classify a Herdr `agent` field. Unknown names are `None`, not a
    /// collector fallback.
    pub fn from_agent_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "codex" => Some(Self::Codex),
            "grok" => Some(Self::Grok),
            "claude" | "claude-code" | "anthropic" => Some(Self::Claude),
            "agy" | "antigravity" | "antigravity-cli" => Some(Self::Agy),
            "opencode" => Some(Self::OpenCode),
            "pi" => Some(Self::Pi),
            "omp" => Some(Self::Omp),
            "devin" | "devin-cli" => Some(Self::Devin),
            "muse" | "muse-code" => Some(Self::Muse),
            "cursor" | "cursor-agent" | "cursor-cli" => Some(Self::Cursor),
            "kilo" | "kilo-code" | "kilocode" => Some(Self::Kilo),
            "kimi" | "kimi-code" | "kimi-cli" => Some(Self::Kimi),
            _ => None,
        }
    }

    /// Original-four 1:1 map. Named harnesses without a collector, and
    /// unknown names, return `None`.
    pub fn billing(self) -> Option<Billing> {
        match self {
            Self::Codex => Some(Provider::Codex),
            Self::Grok => Some(Provider::Grok),
            Self::Claude => Some(Provider::Claude),
            Self::Agy => Some(Provider::Agy),
            Self::Devin => Some(Provider::Devin),
            Self::Muse => Some(Provider::Muse),
            Self::Cursor => Some(Provider::Cursor),
            Self::Kimi => Some(Provider::Kimi),
            Self::OpenCode | Self::Pi | Self::Omp | Self::Kilo => None,
        }
    }

    pub fn billing_for_agent(name: &str) -> Option<Billing> {
        Self::from_agent_name(name).and_then(Self::billing)
    }

    /// The name the sidebar shows when the harness is all that is known.
    ///
    /// Pi and omp carry their provider in the transcript, so a pane of theirs
    /// with no readable session has no provider id to render. The harness is
    /// still true, and it is what names the row and the remedy.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Grok => "Grok",
            Self::Claude => "Claude",
            Self::Agy => "Agy",
            Self::OpenCode => "OpenCode",
            Self::Pi => "Pi",
            Self::Omp => "OMP",
            Self::Devin => "Devin",
            Self::Muse => "Muse",
            Self::Cursor => "Cursor",
            Self::Kilo => "Kilo",
            Self::Kimi => "Kimi",
        }
    }
}

/// Opaque local identity for a credential store. Not a token, path, or account id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CredentialScope(&'static str);

impl CredentialScope {
    /// Canonical CLI stores for the original four collectors.
    pub const CANONICAL: Self = Self("canonical");
    /// OpenCode default data store (`$XDG_DATA_HOME/opencode` or `~/.local/share/opencode`).
    pub const OPENCODE_STORE: Self = Self("opencode-store");
    /// omp's own credential store (`<agent dir>/agent.db`). An omp pane is
    /// billed to a subscription this plugin can also collect canonically, so
    /// the scope is what keeps the two apart.
    pub const OMP_STORE: Self = Self("omp-store");
    /// Kilo's own credential store (`~/.local/share/kilo/auth.json`). A Kilo
    /// pane can run on the Kilo Gateway or on another provider entirely, so
    /// the scope is what keeps the Kilo Pass reading off everything else.
    pub const KILO_STORE: Self = Self("kilo-store");

    pub fn as_str(self) -> &'static str {
        self.0
    }
}

/// Subscription paying for a pane, scoped to one credential store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BillingTarget {
    pub billing: Provider,
    pub credential_scope: CredentialScope,
    /// Stable discriminator for dynamic scoped collectors. OMP provider ids
    /// are hashed so two providers never share a cache or debounce marker and
    /// an arbitrary upstream id never becomes a filesystem path.
    scope_hash: Option<[u8; 32]>,
}

impl BillingTarget {
    pub fn original_four(provider: Provider) -> Self {
        Self {
            billing: provider,
            credential_scope: CredentialScope::CANONICAL,
            scope_hash: None,
        }
    }

    pub fn opencode_go() -> Self {
        Self {
            billing: Provider::OpenCodeGo,
            credential_scope: CredentialScope::OPENCODE_STORE,
            scope_hash: None,
        }
    }

    /// A Kilo pane running on the Kilo Gateway, billed to the login in Kilo's
    /// own store. Scoped for the same reason as OpenCode Go: a Kilo session on
    /// another backend is a different subscription and must never share this
    /// cache file.
    pub fn kilo_gateway() -> Self {
        Self {
            billing: Provider::Kilo,
            credential_scope: CredentialScope::KILO_STORE,
            scope_hash: None,
        }
    }

    /// An omp-scoped target for a subscription omp routes a pane to.
    pub fn omp(provider_id: &str) -> Self {
        Self {
            billing: Provider::Omp,
            credential_scope: CredentialScope::OMP_STORE,
            scope_hash: Some(Sha256::digest(provider_id.as_bytes()).into()),
        }
    }

    /// The billing identity when it is one of the original four collectors.
    ///
    /// Those are refreshed through the provider list; anything else is fetched
    /// only for the pane that resolved to it.
    pub fn original_provider(self) -> Option<Provider> {
        Provider::ALL
            .contains(&self.billing)
            .then_some(self.billing)
    }

    /// Cache, lease, and refresh-marker filename stem.
    ///
    /// One authority for every target: the original four keep their 0.2 source
    /// ids, and a scoped target carries its credential scope in the stem so it
    /// cannot collide with them.
    pub fn cache_identity(self) -> String {
        if self.credential_scope == CredentialScope::OMP_STORE {
            let discriminator = self
                .scope_hash
                .map(|hash| {
                    hash.iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                })
                .unwrap_or_else(|| "unknown".to_string());
            return format!(
                "{}-{discriminator}.{}",
                self.billing.source(),
                self.credential_scope.as_str()
            );
        }
        let source = self.billing.source();
        // OpenCode Go's 0.2 source id already carries its scope; the original
        // four carry none because canonical is the absence of one. Anything
        // else appends its scope so an omp-billed Claude can never overwrite
        // the canonical Claude snapshot.
        if self.credential_scope == CredentialScope::CANONICAL
            || source.ends_with(self.credential_scope.as_str())
        {
            return source.to_string();
        }
        format!("{source}.{}", self.credential_scope.as_str())
    }
}

/// Result of attributing a pane to a subscription. Uncertain evidence never
/// guesses from the number of credentials on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Subscription(BillingTarget),
    NoSubscription,
    Indeterminate,
}

impl std::str::FromStr for Provider {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Harness::billing_for_agent(value)
            .ok_or_else(|| ModelError::UnknownProvider(value.trim().to_ascii_lowercase()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    FiveHour,
    Weekly,
    /// Cached and rendered in the dashboard only. The sidebar has no monthly
    /// token, and a 30d value must never be published through a weekly one.
    Monthly,
}

impl WindowKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::Weekly => "7d",
            Self::Monthly => "30d",
        }
    }

    pub fn duration_seconds(self) -> u64 {
        match self {
            Self::FiveHour => 5 * 60 * 60,
            Self::Weekly => 7 * 24 * 60 * 60,
            Self::Monthly => 30 * 24 * 60 * 60,
        }
    }

    /// Whether a provider's own label is another spelling of this slot's
    /// period.
    ///
    /// Providers name windows in their own vocabulary, and the sidebar's gauge
    /// column is three characters wide, so an omp `Monthly` cannot be drawn
    /// where `30d` fits. Recognising the spellings that mean *this* period —
    /// and only those — is what lets that row render as the slot instead of
    /// losing its meter. A label that names something else stays untouched:
    /// omp's `Daily` is not a five-hour window, and a provider-specific pool
    /// name is not a period at all.
    pub fn spells_period(self, label: &str) -> bool {
        let spelled = label.trim().to_ascii_lowercase();
        match self {
            Self::FiveHour => matches!(
                spelled.as_str(),
                "5h" | "5 hour" | "5 hours" | "5h rolling" | "rolling-5h"
            ),
            Self::Weekly => matches!(
                spelled.as_str(),
                "7d" | "7 day" | "7 days" | "1w" | "week" | "weekly"
            ),
            Self::Monthly => matches!(
                spelled.as_str(),
                "30d" | "30 day" | "30 days" | "1mo" | "month" | "monthly"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ResetAt(u64);

impl ResetAt {
    pub fn from_unix_seconds(seconds: u64) -> Self {
        Self(seconds)
    }

    pub fn parse_rfc3339(value: &str) -> Option<Self> {
        let timestamp = OffsetDateTime::parse(value, &Rfc3339)
            .ok()?
            .unix_timestamp();
        u64::try_from(timestamp).ok().map(Self)
    }

    pub fn parse(value: &str) -> Option<Self> {
        value
            .parse::<u64>()
            .ok()
            .map(Self)
            .or_else(|| Self::parse_rfc3339(value))
    }

    pub fn after(base_unix: u64, seconds: u64) -> Self {
        Self(base_unix.saturating_add(seconds))
    }

    pub fn unix_seconds(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ResetAt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Unix(u64),
            Text(String),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Unix(value) => Ok(Self(value)),
            Repr::Text(value) => Self::parse(&value).ok_or_else(|| {
                serde::de::Error::custom("reset time is not Unix seconds or RFC 3339")
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub kind: WindowKind,
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub resets_at: Option<ResetAt>,
    /// Provider-normalized period label. Most collectors use `kind`; omp
    /// supplies its own labels (`1d`, `7d`, `Monthly`) and those must survive
    /// without this plugin reinterpreting the upstream provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_label: Option<String>,
    /// Provider-normalized duration, when supplied. Kept for deterministic
    /// ordering of omp windows; it is not used to rewrite their labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u64>,
}

impl UsageWindow {
    pub fn new(
        kind: WindowKind,
        used_percent: f64,
        resets_at: Option<ResetAt>,
    ) -> Result<Self, ModelError> {
        if !used_percent.is_finite() || !(0.0..=100.0).contains(&used_percent) {
            return Err(ModelError::InvalidPercentage(used_percent));
        }
        Ok(Self {
            kind,
            used_percent,
            remaining_percent: (100.0 - used_percent).clamp(0.0, 100.0),
            resets_at,
            source_label: None,
            duration_seconds: None,
        })
    }

    pub fn with_source_window(
        mut self,
        label: impl Into<String>,
        duration_seconds: Option<u64>,
    ) -> Self {
        let label = label.into();
        self.source_label = (!label.trim().is_empty()).then_some(label);
        self.duration_seconds = duration_seconds;
        self
    }

    pub fn display_label(&self) -> &str {
        self.source_label
            .as_deref()
            .unwrap_or_else(|| self.kind.label())
    }

    /// Whether this window can still be shown as a live reading.
    ///
    /// A known `resets_at` in the past or present is expired. Missing reset
    /// times cannot be proven stale, so they stay visible.
    pub fn is_current(&self, now_unix: u64) -> bool {
        self.resets_at
            .is_none_or(|reset| reset.unix_seconds() > now_unix)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextUsage {
    pub used_percent: f64,
    #[serde(default)]
    pub cache: Option<CacheUsage>,
}

impl ContextUsage {
    pub fn new(used_percent: f64) -> Result<Self, ModelError> {
        if !used_percent.is_finite() || !(0.0..=100.0).contains(&used_percent) {
            return Err(ModelError::InvalidPercentage(used_percent));
        }
        Ok(Self {
            used_percent,
            cache: None,
        })
    }

    pub fn with_cache(mut self, cache: Option<CacheUsage>) -> Self {
        self.cache = cache;
        self
    }
}

/// Cache counters reported for the latest provider request.
///
/// The provider statusLine payloads expose uncached input, cache creation, and
/// cache reads. Keeping the raw counters alongside the derived percentage
/// makes the displayed ratio auditable and leaves room for richer diagnostics
/// without another provider request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheUsage {
    pub fresh_input_tokens: u64,
    pub read_tokens: u64,
    pub creation_tokens: u64,
    pub hit_percent: f64,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    #[serde(default)]
    pub last_activity_unix: Option<u64>,
    /// Absolute expiry from Claude Code's `prompt_cache.expires_at`.
    /// Preferred over `ttl_seconds` + `last_activity_unix` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix: Option<u64>,
    /// Cumulative cache counters for the current provider session.
    ///
    /// `current_usage` is a latest-request view for Claude/Agy, so the
    /// sidebar uses this optional aggregate when a local transcript gives us
    /// a trustworthy session boundary and offset.
    #[serde(default)]
    pub session_totals: Option<CacheTotals>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_offset: u64,
}

/// Cache counters accumulated across all completed requests in one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheTotals {
    pub fresh_input_tokens: u64,
    pub read_tokens: u64,
    pub creation_tokens: u64,
    pub hit_percent: f64,
}

impl CacheTotals {
    pub fn from_token_counts(
        fresh_input_tokens: u64,
        read_tokens: u64,
        creation_tokens: u64,
    ) -> Option<Self> {
        let total = fresh_input_tokens
            .saturating_add(read_tokens)
            .saturating_add(creation_tokens);
        if total == 0 {
            return None;
        }
        Some(Self {
            fresh_input_tokens,
            read_tokens,
            creation_tokens,
            hit_percent: read_tokens as f64 / total as f64 * 100.0,
        })
    }

    pub fn add_token_counts(
        &mut self,
        fresh_input_tokens: u64,
        read_tokens: u64,
        creation_tokens: u64,
    ) {
        self.fresh_input_tokens = self.fresh_input_tokens.saturating_add(fresh_input_tokens);
        self.read_tokens = self.read_tokens.saturating_add(read_tokens);
        self.creation_tokens = self.creation_tokens.saturating_add(creation_tokens);
        let total = self
            .fresh_input_tokens
            .saturating_add(self.read_tokens)
            .saturating_add(self.creation_tokens);
        self.hit_percent = if total == 0 {
            0.0
        } else {
            self.read_tokens as f64 / total as f64 * 100.0
        };
    }
}

impl CacheUsage {
    pub fn from_token_counts(
        fresh_input_tokens: u64,
        read_tokens: u64,
        creation_tokens: u64,
    ) -> Option<Self> {
        let total = fresh_input_tokens
            .saturating_add(read_tokens)
            .saturating_add(creation_tokens);
        if total == 0 {
            return None;
        }
        Some(Self {
            fresh_input_tokens,
            read_tokens,
            creation_tokens,
            hit_percent: read_tokens as f64 / total as f64 * 100.0,
            ttl_seconds: None,
            last_activity_unix: None,
            expires_at_unix: None,
            session_totals: None,
            session_id: None,
            transcript_offset: 0,
        })
    }

    pub fn with_ttl_estimate(mut self, ttl_seconds: u64, last_activity_unix: u64) -> Self {
        self.ttl_seconds = Some(ttl_seconds);
        self.last_activity_unix = Some(last_activity_unix);
        self
    }

    pub fn remaining_ttl_seconds(&self, now_unix: u64) -> Option<u64> {
        if let Some(expires_at) = self.expires_at_unix {
            return Some(expires_at.saturating_sub(now_unix));
        }
        let ttl = self.ttl_seconds?;
        let last_activity = self.last_activity_unix?;
        Some(last_activity.saturating_add(ttl).saturating_sub(now_unix))
    }

    pub fn with_session_totals(
        mut self,
        totals: Option<CacheTotals>,
        session_id: impl Into<String>,
        transcript_offset: u64,
    ) -> Self {
        self.session_totals = totals;
        self.session_id = Some(session_id.into());
        self.transcript_offset = transcript_offset;
        self
    }
}

/// Freshness evidence for one session-local quota window.
///
/// Claude statusLine may redraw without another provider response, so hook
/// arrival time is not evidence that the percentage was observed again.
/// `observed_at_unix = None` is deliberate for a legacy/replayed value whose
/// age cannot be established yet. `api_generation` is an opaque digest of
/// documented API-derived statusLine fields; it is not an account identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionQuotaObservation {
    pub kind: WindowKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_generation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    /// StatusLine quota has no serving-account proof and is session-local.
    /// False on old caches so they can be refreshed without trusting shared
    /// profile windows from an earlier plugin version.
    #[serde(default)]
    pub session_quota_only: bool,
    pub provider: Provider,
    pub source: String,
    pub fetched_at_unix: u64,
    pub windows: Vec<UsageWindow>,
    #[serde(default)]
    pub context: Option<ContextUsage>,
    /// Human-readable name of the model most recently reported by a provider.
    ///
    /// StatusLine providers also keep the per-session value below so panes
    /// running the same provider can be distinguished from one another.
    /// Devin stores the CLI `config.json` default here. A pane whose session
    /// is in `sessions.db` reads `session_models` instead; otherwise
    /// [`Self::model_for_session`] falls back to this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub session_summaries: BTreeMap<String, String>,
    #[serde(default)]
    pub session_models: BTreeMap<String, String>,
    /// Context/cache diagnostics keyed by the provider's session id. Keeping
    /// this per session prevents one provider pane from displaying another
    /// pane's local rollout usage.
    #[serde(default)]
    pub session_contexts: BTreeMap<String, ContextUsage>,
    /// StatusLine quota observations keyed by the exact provider session ID.
    /// Direct API collectors leave this map empty. Claude and Agy snapshots
    /// keep these windows conversation-local: Agy's gemini/3p allowance is an
    /// account pool, but which pool applies is selected by that conversation's
    /// active model. An unmatched Agy Herdr id may use a stored conversation
    /// only when exactly one conversation is observable.
    #[serde(default)]
    pub session_windows: BTreeMap<String, Vec<UsageWindow>>,
    /// Per-window freshness evidence for Claude's session-local quota.
    ///
    /// Kept separate from `session_windows` so older cache files remain
    /// readable and an omitted 7d window can keep its own age while 5h is
    /// freshly observed (and vice versa).
    #[serde(default)]
    pub session_quota_observations: BTreeMap<String, Vec<SessionQuotaObservation>>,
    /// Digest of the Claude account each session was signed in to when its
    /// statusLine hook last ran, keyed by session id.
    ///
    /// The hook reads `oauthAccount.accountUuid` from the `.claude.json` of the
    /// session's own `CLAUDE_CONFIG_DIR`. Sessions with equal digests are one
    /// subscription, so they share the newest reading and one sidebar row. A
    /// session without a digest proves nothing and stays on its own windows.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub session_accounts: BTreeMap<String, String>,
    /// Legacy Claude profile digests retained for cache format compatibility.
    /// Current session-local observations clear this map during migration.
    #[serde(default)]
    pub session_quota_scopes: BTreeMap<String, String>,
    /// Legacy profile-shared windows; not trusted by current StatusLine data.
    #[serde(default)]
    pub quota_scope_windows: BTreeMap<String, Vec<UsageWindow>>,
    /// Login identity the snapshot was fetched for (Grok `user_id`, Codex
    /// `tokens.account_id`). Used to drop another account's cached quota after
    /// `grok login` / Codex account switch. Absent on snapshots written before
    /// this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl ProviderSnapshot {
    pub fn new(provider: Provider, windows: Vec<UsageWindow>, fetched_at_unix: u64) -> Self {
        Self {
            session_quota_only: false,
            provider,
            source: provider.source().to_string(),
            fetched_at_unix,
            windows,
            context: None,
            model: None,
            session_summaries: BTreeMap::new(),
            session_models: BTreeMap::new(),
            session_contexts: BTreeMap::new(),
            session_windows: BTreeMap::new(),
            session_quota_observations: BTreeMap::new(),
            session_accounts: BTreeMap::new(),
            session_quota_scopes: BTreeMap::new(),
            quota_scope_windows: BTreeMap::new(),
            account_id: None,
        }
    }

    pub fn with_context(mut self, context: Option<ContextUsage>) -> Self {
        self.context = context;
        self
    }

    pub fn session_local(mut self) -> Self {
        self.session_quota_only = true;
        self
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    /// The only provider session represented by the session-local maps.
    ///
    /// Antigravity's PreInvocation hook may report a spawned subagent
    /// conversation while statusLine continues to describe the parent TUI
    /// conversation. Bridging that mismatch is safe only when all retained Agy
    /// diagnostics point at one conversation. Two distinct ids are ambiguous
    /// and deliberately return `None` rather than borrowing the latest pane.
    fn only_observed_session(&self) -> Option<&str> {
        let mut only = None;
        for id in self
            .session_windows
            .keys()
            .chain(self.session_models.keys())
            .chain(self.session_contexts.keys())
        {
            let id = id.as_str();
            match only {
                None => only = Some(id),
                Some(current) if current == id => {}
                Some(_) => return None,
            }
        }
        only
    }

    /// Resolve the session key used by per-session diagnostics.
    ///
    /// Non-Agy providers keep their exact existing lookup. Agy first honors an
    /// exact statusLine conversation id. If Herdr supplied a different
    /// subagent id, it may bridge to the sole observed conversation; once two
    /// conversations are present, the mismatch fails closed.
    fn session_for_lookup<'a>(&'a self, session_id: &'a str) -> Option<&'a str> {
        if self.provider != Provider::Agy {
            return Some(session_id);
        }
        if self.session_windows.contains_key(session_id)
            || self.session_models.contains_key(session_id)
            || self.session_contexts.contains_key(session_id)
        {
            return Some(session_id);
        }
        self.only_observed_session()
    }

    /// The Claude account digest a session's statusLine hook recorded.
    pub fn account_for_session(&self, session_id: &str) -> Option<&str> {
        if self.provider != Provider::Claude || !self.session_quota_only {
            return None;
        }
        self.session_accounts.get(session_id).map(String::as_str)
    }

    /// The session whose quota window a pane on `session_id` shows.
    ///
    /// A Claude session on a recorded account shows the newest reading of any
    /// session on that same account: the 5h/7d windows belong to the account,
    /// and an idle tab would otherwise keep a stale reading while a sibling has
    /// a fresh one. Compare each period independently: a newer 5h reading must
    /// not replace a fresher 7d reading. A tie keeps the pane's own session.
    /// A session without an account digest keeps its own windows, because
    /// nothing proves who pays.
    fn quota_session_for_lookup<'a>(
        &'a self,
        session_id: &'a str,
        kind: WindowKind,
    ) -> Option<&'a str> {
        let session_id = self.session_for_lookup(session_id)?;
        let Some(account) = self.account_for_session(session_id) else {
            return Some(session_id);
        };
        let freshness = |id: &str| {
            self.session_quota_observations
                .get(id)
                .into_iter()
                .flatten()
                .find(|observation| observation.kind == kind)
                .and_then(|observation| observation.observed_at_unix)
        };
        let own = freshness(session_id);
        let has_window = |id: &str| {
            self.session_windows
                .get(id)
                .is_some_and(|windows| window_in(windows, kind).is_some())
        };
        let own_has_window = has_window(session_id);
        let newest = self
            .session_accounts
            .iter()
            .filter(|(id, digest)| {
                digest.as_str() == account && id.as_str() != session_id && has_window(id)
            })
            .map(|(id, _)| (freshness(id), id.as_str()))
            .filter(|(seen, _)| !own_has_window || *seen > own)
            .max();
        Some(newest.map_or(session_id, |(_, id)| id))
    }

    /// Return the model for a pane's session.
    ///
    /// A known Claude/Codex/Grok session never borrows the provider-level
    /// value, because that may belong to another pane. Devin populates
    /// `session_models` from `sessions.db`, so a pane whose session id is found
    /// in the DB gets its per-session active model. A pane without a
    /// `session_models` entry — a brand-new session, or one whose id is not in
    /// the DB — falls back to `snapshot.model`, the `config.json` default.
    /// Agy may use the latest model only when every retained observation
    /// points at this same conversation. An exact hit in `session_windows`
    /// is not permission to borrow another pane's label.
    pub fn model_for_session(&self, session_id: Option<&str>) -> Option<&str> {
        let Some(session_id) = session_id else {
            return self.model.as_deref();
        };
        let session_id = self.session_for_lookup(session_id)?;
        if let Some(model) = self.session_models.get(session_id) {
            return Some(model);
        }
        match self.provider {
            // A Muse session with no completed model call yet runs the
            // `settings.json` default, like a fresh Devin session.
            Provider::Devin | Provider::Muse | Provider::Cursor | Provider::Kimi => {
                self.model.as_deref()
            }
            Provider::Agy if self.only_observed_session() == Some(session_id) => {
                self.model.as_deref()
            }
            _ => None,
        }
    }

    /// Return context/cache diagnostics for a pane's session. A known session
    /// never falls back to provider-level data, because an older snapshot may
    /// belong to another pane. The global value is used only when the caller
    /// has no session id at all. Agy may use the latest context only when
    /// every retained observation points at this same conversation.
    pub fn context_for_session(&self, session_id: Option<&str>) -> Option<&ContextUsage> {
        let Some(session_id) = session_id else {
            return self.context.as_ref();
        };
        let session_id = self.session_for_lookup(session_id)?;
        if let Some(context) = self.session_contexts.get(session_id) {
            return Some(context);
        }
        match self.provider {
            Provider::Agy if self.only_observed_session() == Some(session_id) => {
                self.context.as_ref()
            }
            _ => None,
        }
    }

    /// Return freshness evidence for one Claude session-local quota window.
    ///
    /// Other providers and non-session-local snapshots deliberately have no
    /// observation record here.
    pub fn quota_observation_for_session(
        &self,
        session_id: Option<&str>,
        kind: WindowKind,
    ) -> Option<&SessionQuotaObservation> {
        if self.provider != Provider::Claude || !self.session_quota_only {
            return None;
        }
        let session_id = session_id.and_then(|id| self.quota_session_for_lookup(id, kind))?;
        self.session_quota_observations
            .get(session_id)?
            .iter()
            .find(|observation| observation.kind == kind)
    }

    /// Return the quota windows for a pane's session.
    ///
    /// Context and model are session-local. Grok, Codex, and Devin have
    /// provider-level quota windows. Claude and Agy statusLine windows stay
    /// keyed by conversation: Agy's quota belongs to the account, but the
    /// active conversation's model selects the gemini or 3p pool.
    ///
    /// Lookup order:
    /// 1. Agy with no Herdr session id → latest top-level statusLine windows.
    /// 2. Session-local snapshot → exact session windows, or the newest
    ///    session on the same recorded Claude account; Agy may bridge an
    ///    unmatched subagent id only when exactly one conversation is stored.
    /// 3. Session has a legacy Claude profile scope → canonical scope windows.
    /// 4. Session has legacy `session_windows` → those.
    /// 5. Every keyed map is empty → top-level windows (Grok/Codex/Devin and a
    ///    StatusLine cache written before session maps existed).
    /// 6. Keyed maps exist but this session is unknown → empty.
    pub fn windows_for_session<'a>(&'a self, session_id: Option<&str>) -> Cow<'a, [UsageWindow]> {
        if let Some(id) = session_id.filter(|id| self.account_for_session(id).is_some()) {
            // This is a read-time projection. Raw session_windows stay intact
            // so a later cache write cannot attribute a sibling's sample to us.
            return Cow::Owned(
                [
                    WindowKind::FiveHour,
                    WindowKind::Weekly,
                    WindowKind::Monthly,
                ]
                .into_iter()
                .filter_map(|kind| {
                    let source = self.quota_session_for_lookup(id, kind)?;
                    window_in(self.session_windows.get(source)?, kind).cloned()
                })
                .collect(),
            );
        }
        Cow::Borrowed(self.raw_windows_for_session(session_id))
    }

    fn raw_windows_for_session(&self, session_id: Option<&str>) -> &[UsageWindow] {
        if self.provider == Provider::Agy && session_id.is_none() {
            return &self.windows;
        }
        if self.session_quota_only {
            let Some(session_id) = session_id.and_then(|id| self.session_for_lookup(id)) else {
                return &[];
            };
            return self
                .session_windows
                .get(session_id)
                .map(Vec::as_slice)
                .unwrap_or_default();
        }
        let Some(session_id) = session_id else {
            return &self.windows;
        };
        if let Some(scope) = self.session_quota_scopes.get(session_id) {
            if let Some(windows) = self.quota_scope_windows.get(scope) {
                return windows;
            }
        }
        if let Some(windows) = self.session_windows.get(session_id) {
            return windows;
        }
        if self.session_windows.is_empty()
            && self.session_quota_scopes.is_empty()
            && self.quota_scope_windows.is_empty()
        {
            return &self.windows;
        }
        &[]
    }

    pub fn with_account_id(mut self, account_id: Option<String>) -> Self {
        self.account_id = account_id;
        self
    }

    /// Whether this cached snapshot still belongs to the signed-in account.
    ///
    /// A failed refresh must keep the last good value for the *current*
    /// account, not a previous login. After an account switch:
    /// - snapshots stamped with another `account_id` are unusable;
    /// - unstamped snapshots are unusable when the credential file is newer
    ///   than `fetched_at_unix`, including Codex `auth.json` rewrites that do
    ///   not expose an id.
    pub fn usable_for_account(
        &self,
        current_account_id: Option<&str>,
        credentials_mtime_unix: Option<u64>,
    ) -> bool {
        if matches!(self.provider, Provider::Claude | Provider::Agy)
            && self.account_id.is_none()
            && !self.session_quota_only
        {
            return false;
        }
        match (self.account_id.as_deref(), current_account_id) {
            (Some(saved), Some(current)) => saved == current,
            (Some(_), None) => false,
            (None, Some(_)) => false,
            (None, None) => {
                credentials_mtime_unix.is_none_or(|mtime| mtime <= self.fetched_at_unix)
            }
        }
    }

    pub fn window(&self, kind: WindowKind) -> Option<&UsageWindow> {
        window_in(&self.windows, kind)
    }

    /// True when a stored quota window's reset is now in the past.
    ///
    /// Missing reset times cannot be proved expired, so they do not qualify.
    /// An empty window list is "no quota", not a lapsed window. Provider
    /// fetches use this; a pane uses [`Self::displayed_quota_has_expired`].
    pub fn has_expired_quota(&self, now_unix: u64) -> bool {
        quota_windows_expired(&self.windows, now_unix)
            || self
                .session_windows
                .values()
                .any(|windows| quota_windows_expired(windows, now_unix))
            || self
                .quota_scope_windows
                .values()
                .any(|windows| quota_windows_expired(windows, now_unix))
    }

    /// True when the windows this pane would render have already reset.
    ///
    /// Codex/Grok/Devin share provider-level windows. Claude keys windows by
    /// session. Agy uses an exact or unambiguous statusLine conversation, so an
    /// unrelated Agy pane cannot make this pane inherit a different pool.
    pub fn displayed_quota_has_expired(&self, session_id: Option<&str>, now_unix: u64) -> bool {
        quota_windows_expired(&self.windows_for_session(session_id), now_unix)
    }

    /// Keep a previously observed quota window when the latest payload omits
    /// it. Upstream often drops the short window for a tick (Claude statusLine
    /// without `five_hour`, Codex `secondary: null` after a reset credit).
    ///
    /// This never invents percentages. An omitted window is restored only when
    /// its reset is still in the future and a sibling window present in both
    /// snapshots has not itself reset. An empty `windows` list still means
    /// "rate limits were absent — clear stale quota".
    pub fn merge_omitted_windows(&mut self, previous: &Self) {
        if !same_quota_account(self, previous) {
            return;
        }
        merge_omitted_window_list(&mut self.windows, &previous.windows, self.fetched_at_unix);
    }

    pub fn severity(&self, now_unix: u64) -> Severity {
        Self::severity_for_windows(self.provider, &self.windows, now_unix)
    }

    /// Same runway-health calculation as [`Self::severity`], but over an
    /// explicit window slice so a pane can be scored against its own
    /// session/account windows instead of the provider-wide top-level ones.
    pub fn severity_for_windows(
        provider: Provider,
        windows: &[UsageWindow],
        now_unix: u64,
    ) -> Severity {
        let live = live_windows(windows, now_unix);
        let relevant = match provider {
            Provider::Grok | Provider::Cursor => long_window(&live),
            Provider::Codex
            | Provider::Claude
            | Provider::Agy
            | Provider::OpenCodeGo
            | Provider::Kilo
            | Provider::Omp
            | Provider::Devin
            | Provider::Muse
            | Provider::Kimi => {
                window_in(&live, WindowKind::FiveHour).or_else(|| long_window(&live))
            }
        };
        relevant
            .map(|window| Severity::for_window(window, now_unix))
            .unwrap_or(Severity::Unknown)
    }
}

fn quota_windows_expired(windows: &[UsageWindow], now_unix: u64) -> bool {
    windows.iter().any(|window| !window.is_current(now_unix))
}

pub(crate) fn window_in(windows: &[UsageWindow], kind: WindowKind) -> Option<&UsageWindow> {
    windows.iter().find(|window| window.kind == kind)
}

/// The recurring allowance shown in the sidebar's long-window slot.
///
/// Weekly is the usual shape. A plan billed monthly has no weekly bucket at
/// all, so the same slot carries its 30d window instead; the period label
/// travels inside the rendered value, so a monthly number is never displayed
/// as `7d`. Monthly is strictly a fallback — when both exist the weekly one
/// wins, because it is the limit that binds first.
pub(crate) fn long_window(windows: &[UsageWindow]) -> Option<&UsageWindow> {
    window_in(windows, WindowKind::Weekly).or_else(|| window_in(windows, WindowKind::Monthly))
}

/// Windows that can still be treated as a live provider reading.
pub(crate) fn live_windows(windows: &[UsageWindow], now_unix: u64) -> Vec<UsageWindow> {
    windows
        .iter()
        .filter(|window| window.is_current(now_unix))
        .cloned()
        .collect()
}

/// Restore an omitted 5h/weekly window from a previous observation of the
/// *same* account or session. Callers that key windows by session must pass
/// that session's previous list, not another account's top-level snapshot.
pub(crate) fn merge_omitted_window_list(
    windows: &mut Vec<UsageWindow>,
    previous: &[UsageWindow],
    fetched_at_unix: u64,
) {
    if windows.is_empty() {
        return;
    }
    if sibling_quota_reset_in(windows, previous) {
        return;
    }
    for kind in [WindowKind::FiveHour, WindowKind::Weekly] {
        if window_in(windows, kind).is_some() {
            continue;
        }
        let Some(previous_window) = window_in(previous, kind).cloned() else {
            continue;
        };
        let Some(reset) = previous_window.resets_at else {
            continue;
        };
        if reset.unix_seconds() <= fetched_at_unix {
            continue;
        }
        windows.push(previous_window);
    }
}

fn same_quota_account(current: &ProviderSnapshot, previous: &ProviderSnapshot) -> bool {
    match (
        current.account_id.as_deref(),
        previous.account_id.as_deref(),
    ) {
        (Some(current_id), Some(previous_id)) => current_id == previous_id,
        (None, None) => true,
        _ => false,
    }
}

pub(crate) fn sibling_quota_reset_in(current: &[UsageWindow], previous: &[UsageWindow]) -> bool {
    const USED_PERCENT_RESET_DROP: f64 = 5.0;
    [WindowKind::FiveHour, WindowKind::Weekly]
        .into_iter()
        .any(|kind| {
            let (Some(current_window), Some(previous_window)) =
                (window_in(current, kind), window_in(previous, kind))
            else {
                return false;
            };
            if let (Some(current_reset), Some(previous_reset)) =
                (current_window.resets_at, previous_window.resets_at)
            {
                if current_reset != previous_reset {
                    return true;
                }
            }
            current_window.used_percent + USED_PERCENT_RESET_DROP < previous_window.used_percent
        })
}

/// Runway health for one quota window.
///
/// These are exactly the bands [`Self::for_window`] can produce — there is no
/// variant the sidebar cannot reach, so every styled token this maps to is one
/// a pane can actually be given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Normal,
    Warning,
    Danger,
    Unknown,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "OK",
            Self::Warning => "WARN",
            Self::Danger => "LOW",
            Self::Unknown => "N/A",
        }
    }

    pub fn for_window(window: &UsageWindow, _now_unix: u64) -> Self {
        // Remaining quota only. Three sidebar bands so packed 5h/7d rows
        // do not mix two nearby greens. Classify the rounded integer shown.
        Self::for_headroom(window.remaining_percent)
    }

    /// Headroom left in the context window, on the same bands as
    /// [`Self::for_window`] — a sidebar row means the same thing whichever
    /// row it is.
    ///
    /// Never `Unknown`: a context row exists only when a percent was read.
    pub fn for_context_remaining(remaining_percent: f64) -> Self {
        Self::for_headroom(remaining_percent)
    }

    /// The one band table every sidebar row is coloured by. Classify the
    /// rounded integer, so a row's colour and its printed number can never
    /// disagree at a threshold.
    fn for_headroom(remaining_percent: f64) -> Self {
        let displayed_remaining = remaining_percent.round();
        if displayed_remaining >= 50.0 {
            Self::Normal
        } else if displayed_remaining >= 20.0 {
            Self::Warning
        } else {
            Self::Danger
        }
    }
}

pub fn format_percent(value: f64) -> String {
    let rounded = format!("{value:.0}");
    // `{:.0}` rounds 99.5–99.9 to 100. Agy gemini-5h at remaining_fraction
    // 0.9986 then paints a full bar while headroom (floored) is already 99.
    if value < 100.0 && rounded == "100" {
        "99".to_string()
    } else {
        rounded
    }
}

/// The whole number the sidebar prints, for callers that must agree with it —
/// the gauges meter derives its cell count from this.
///
/// Read back out of [`format_percent`] rather than rounded again: `{:.0}`
/// rounds half to even while `f64::round` rounds half away from zero, so an
/// exactly-reachable 18.5% would otherwise draw a two-cell bar beside `18%`.
pub fn printed_percent(value: f64) -> u32 {
    format_percent(value).parse().unwrap_or(0)
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("unknown provider: {0}")]
    UnknownProvider(String),
    #[error("percentage must be finite and between 0 and 100, got {0}")]
    InvalidPercentage(f64),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(kind: WindowKind, used: f64) -> UsageWindow {
        UsageWindow::new(kind, used, None).expect("fixture percentage is valid")
    }

    #[test]
    fn remaining_percentage_is_derived_from_used_percentage() {
        let value = window(WindowKind::Weekly, 42.5);
        assert_eq!(value.remaining_percent, 57.5);
        assert_eq!(format_percent(value.remaining_percent), "58");
        assert_eq!(
            format_percent(window(WindowKind::FiveHour, 0.14411).remaining_percent),
            "99"
        );
    }

    #[test]
    fn format_percent_does_not_round_a_partial_pool_up_to_full() {
        assert_eq!(format_percent(99.85589), "99");
        assert_eq!(format_percent(99.5), "99");
        assert_eq!(format_percent(100.0), "100");
        assert_eq!(format_percent(0.0), "0");
    }

    #[test]
    fn cache_hit_ratio_uses_fresh_creation_and_read_tokens() {
        let cache = CacheUsage::from_token_counts(100, 800, 100).unwrap();
        assert_eq!(cache.hit_percent, 80.0);
        assert_eq!(CacheUsage::from_token_counts(0, 0, 0), None);
        assert_eq!(
            CacheUsage::from_token_counts(100, 0, 0)
                .unwrap()
                .hit_percent,
            0.0
        );
    }

    #[test]
    fn session_cache_totals_accumulate_and_recompute_hit_ratio() {
        let mut totals = CacheTotals::from_token_counts(100, 800, 100).unwrap();
        totals.add_token_counts(100, 0, 0);
        assert_eq!(totals.fresh_input_tokens, 200);
        assert_eq!(totals.read_tokens, 800);
        assert_eq!(totals.creation_tokens, 100);
        assert_eq!(totals.hit_percent, 72.72727272727273);
    }

    #[test]
    fn old_context_snapshots_deserialize_without_cache_fields() {
        let context: ContextUsage = serde_json::from_str(r#"{"used_percent":23.5}"#).unwrap();
        assert_eq!(context.used_percent, 23.5);
        assert!(context.cache.is_none());
    }

    #[test]
    fn legacy_statusline_context_is_not_reused_for_an_unknown_session() {
        let cache = CacheUsage::from_token_counts(10, 90, 0)
            .unwrap()
            .with_session_totals(CacheTotals::from_token_counts(10, 90, 0), "old-session", 0);
        let snapshot = ProviderSnapshot::new(Provider::Claude, vec![], 0).with_context(Some(
            ContextUsage::new(23.5).unwrap().with_cache(Some(cache)),
        ));
        assert!(snapshot.context_for_session(Some("new-session")).is_none());
    }

    #[test]
    fn legacy_local_context_is_not_reused_for_an_unknown_session() {
        let cache = CacheUsage::from_token_counts(10, 90, 0)
            .unwrap()
            .with_session_totals(CacheTotals::from_token_counts(10, 90, 0), "old-session", 0);
        for provider in [Provider::Codex, Provider::Grok] {
            let snapshot = ProviderSnapshot::new(provider, vec![], 0).with_context(Some(
                ContextUsage::new(23.5)
                    .unwrap()
                    .with_cache(Some(cache.clone())),
            ));
            assert!(
                snapshot.context_for_session(Some("new-session")).is_none(),
                "{provider:?} leaked provider-level context into a new session"
            );
        }
    }

    #[test]
    fn cached_snapshot_from_another_account_is_not_usable() {
        let snapshot = ProviderSnapshot::new(Provider::Grok, vec![], 100)
            .with_account_id(Some("account-a".to_string()));
        assert!(!snapshot.usable_for_account(Some("account-b"), Some(50)));
        assert!(snapshot.usable_for_account(Some("account-a"), Some(200)));
    }

    #[test]
    fn legacy_snapshot_is_dropped_when_credentials_are_newer_than_the_fetch() {
        let snapshot = ProviderSnapshot::new(Provider::Grok, vec![], 100);
        assert!(!snapshot.usable_for_account(Some("account-b"), Some(150)));
        assert!(!snapshot.usable_for_account(Some("account-b"), Some(100)));
        assert!(!snapshot.usable_for_account(Some("account-b"), Some(50)));
        assert!(!snapshot.usable_for_account(None, Some(150)));
        assert!(snapshot.usable_for_account(None, Some(50)));
    }

    #[test]
    fn old_snapshots_deserialize_without_account_id() {
        let snapshot: ProviderSnapshot = serde_json::from_str(
            r#"{"provider":"grok","source":"grok-cli-billing","fetched_at_unix":1,"windows":[]}"#,
        )
        .unwrap();
        assert_eq!(snapshot.account_id, None);
    }

    #[test]
    fn approximate_cache_ttl_saturates_after_expiry() {
        let cache = CacheUsage::from_token_counts(1, 1, 0)
            .unwrap()
            .with_ttl_estimate(300, 1_000);
        assert_eq!(cache.remaining_ttl_seconds(1_100), Some(200));
        assert_eq!(cache.remaining_ttl_seconds(1_301), Some(0));
    }

    #[test]
    fn recorded_expiry_is_preferred_over_a_ttl_estimate() {
        let mut cache = CacheUsage::from_token_counts(1, 1, 0)
            .unwrap()
            .with_ttl_estimate(300, 1_000);
        cache.expires_at_unix = Some(1_500);
        assert_eq!(cache.remaining_ttl_seconds(1_400), Some(100));
        assert_eq!(cache.remaining_ttl_seconds(1_500), Some(0));
    }

    #[test]
    fn reset_time_deserializes_new_unix_and_legacy_rfc3339_cache_values() {
        let unix: ResetAt = serde_json::from_str("1787400000").unwrap();
        let legacy: ResetAt = serde_json::from_str("\"2026-08-22T12:00:00Z\"").unwrap();
        assert_eq!(unix, ResetAt::from_unix_seconds(1_787_400_000));
        assert_eq!(legacy, unix);
        assert_eq!(serde_json::to_string(&unix).unwrap(), "1787400000");
    }

    #[test]
    fn severity_follows_remaining_quota_bands() {
        let now = 1_000_000;
        let reset = ResetAt::after(now, WindowKind::Weekly.duration_seconds() / 2);
        for (used_percent, expected) in [
            (20.0, Severity::Normal),
            (20.6, Severity::Normal),
            (50.0, Severity::Normal),
            (50.6, Severity::Warning),
            (80.0, Severity::Warning),
            (80.6, Severity::Danger),
        ] {
            let window = UsageWindow::new(WindowKind::Weekly, used_percent, Some(reset)).unwrap();
            assert_eq!(Severity::for_window(&window, now), expected);
        }
    }

    /// Context severity reads headroom, exactly like a window's: it bands on
    /// the context left, so every sidebar row means the same thing.
    #[test]
    fn context_severity_is_thresholded_on_remaining_at_fifty_and_twenty() {
        for (used_percent, expected) in [
            (0.0, Severity::Normal),
            (31.0, Severity::Normal),
            (49.0, Severity::Normal),
            (49.4, Severity::Normal),
            (50.0, Severity::Normal),
            (51.0, Severity::Warning),
            (53.0, Severity::Warning),
            (79.0, Severity::Warning),
            (79.4, Severity::Warning),
            (80.0, Severity::Warning),
            (81.0, Severity::Danger),
            (85.0, Severity::Danger),
            (100.0, Severity::Danger),
        ] {
            assert_eq!(
                Severity::for_context_remaining(100.0 - used_percent),
                expected,
                "{used_percent} used"
            );
        }
    }

    #[test]
    fn codex_severity_prefers_the_five_hour_window_when_available() {
        let now = 1_000_000;
        let snapshot = ProviderSnapshot::new(
            Provider::Codex,
            vec![
                UsageWindow::new(
                    WindowKind::FiveHour,
                    90.0,
                    Some(ResetAt::after(
                        now,
                        WindowKind::FiveHour.duration_seconds() / 2,
                    )),
                )
                .unwrap(),
                UsageWindow::new(
                    WindowKind::Weekly,
                    10.0,
                    Some(ResetAt::after(
                        now,
                        WindowKind::Weekly.duration_seconds() / 2,
                    )),
                )
                .unwrap(),
            ],
            now,
        );
        assert_eq!(snapshot.severity(now), Severity::Danger);
    }

    #[test]
    fn low_remaining_quota_is_danger_even_when_reset_is_close() {
        let now = 1_000_000;
        let reset = ResetAt::after(now, WindowKind::Weekly.duration_seconds() / 10);
        let window = UsageWindow::new(WindowKind::Weekly, 85.0, Some(reset)).unwrap();

        assert_eq!(Severity::for_window(&window, now), Severity::Danger);
    }

    #[test]
    fn remaining_quota_still_colors_a_window_without_a_reset_time() {
        let window = UsageWindow::new(WindowKind::Weekly, 85.0, None).unwrap();
        assert_eq!(Severity::for_window(&window, 1_000_000), Severity::Danger);

        let expired = UsageWindow::new(
            WindowKind::Weekly,
            40.0,
            Some(ResetAt::from_unix_seconds(999_999)),
        )
        .unwrap();
        assert_eq!(Severity::for_window(&expired, 1_000_000), Severity::Normal);
    }

    #[test]
    fn provider_aliases_are_explicit() {
        assert_eq!("claude-code".parse::<Provider>().unwrap(), Provider::Claude);
        assert_eq!("antigravity".parse::<Provider>().unwrap(), Provider::Agy);
        assert!("opencode".parse::<Provider>().is_err());
        assert!("OpenCode".parse::<Provider>().is_err());
        assert!("pi".parse::<Provider>().is_err());
    }

    /// `source` names the file a snapshot lives in, so a constructor that
    /// overrides it makes the cache and the snapshot disagree about identity.
    #[test]
    fn every_snapshot_reports_the_source_of_its_own_cache_file() {
        for provider in Provider::ALL.into_iter().chain(Provider::SCOPED) {
            assert_eq!(
                ProviderSnapshot::new(provider, vec![], 0).source,
                provider.source(),
                "{provider:?}"
            );
        }
    }

    #[test]
    fn opencode_go_cache_identity_cannot_borrow_original_four_files() {
        let target = BillingTarget::opencode_go();
        assert_eq!(target.cache_identity(), "opencode-go.opencode-store");
        assert_eq!(target.credential_scope, CredentialScope::OPENCODE_STORE);
        assert!(target.original_provider().is_none());
        for provider in Provider::ALL {
            let original = BillingTarget::original_four(provider);
            assert_eq!(original.cache_identity(), provider.source());
            assert_eq!(original.credential_scope, CredentialScope::CANONICAL);
            assert_ne!(target.cache_identity(), original.cache_identity());
            assert!(!target.cache_identity().contains(provider.source()));
        }
    }

    #[test]
    fn harness_identity_is_not_a_quota_collector() {
        assert_eq!(
            Harness::from_agent_name("OpenCode"),
            Some(Harness::OpenCode)
        );
        assert_eq!(
            Harness::from_agent_name("opencode"),
            Some(Harness::OpenCode)
        );
        assert_eq!(Harness::billing_for_agent("opencode"), None);
        assert_eq!(Harness::billing_for_agent("pi"), None);
        assert_eq!(Harness::billing_for_agent("cursor"), Some(Provider::Cursor));
        assert_eq!(
            Harness::billing_for_agent("cursor-agent"),
            Some(Provider::Cursor)
        );
        assert_eq!(
            Harness::billing_for_agent("claude-code"),
            Some(Provider::Claude)
        );
        assert_eq!(
            Harness::billing_for_agent("antigravity"),
            Some(Provider::Agy)
        );
        assert_eq!(Harness::billing_for_agent("codex"), Some(Provider::Codex));
        assert_eq!(Harness::billing_for_agent("grok"), Some(Provider::Grok));
        assert_eq!(Harness::billing_for_agent("devin"), Some(Provider::Devin));
        assert_eq!(
            Harness::billing_for_agent("devin-cli"),
            Some(Provider::Devin)
        );
        assert_eq!(Harness::billing_for_agent("muse"), Some(Provider::Muse));
        assert_eq!(
            Harness::billing_for_agent("muse-code"),
            Some(Provider::Muse)
        );
    }

    #[test]
    fn original_four_v0_2_snapshots_deserialize_with_canonical_sources() {
        let cases = [
            (
                r#"{"provider":"codex","source":"codex-app-server","fetched_at_unix":1,"windows":[]}"#,
                Provider::Codex,
                "codex-app-server",
            ),
            (
                r#"{"provider":"grok","source":"grok-cli-billing","fetched_at_unix":1,"windows":[]}"#,
                Provider::Grok,
                "grok-cli-billing",
            ),
            (
                r#"{"provider":"claude","source":"claude-statusline","fetched_at_unix":1,"windows":[]}"#,
                Provider::Claude,
                "claude-statusline",
            ),
            (
                r#"{"provider":"agy","source":"agy-statusline","fetched_at_unix":1,"windows":[]}"#,
                Provider::Agy,
                "agy-statusline",
            ),
        ];
        for (json, provider, source) in cases {
            let snapshot: ProviderSnapshot = serde_json::from_str(json).unwrap();
            assert_eq!(snapshot.provider, provider);
            assert_eq!(snapshot.source, source);
            assert_eq!(provider.source(), source);
        }
    }

    fn quota_window(kind: WindowKind, used: f64, reset: u64) -> UsageWindow {
        UsageWindow::new(kind, used, Some(ResetAt::from_unix_seconds(reset))).unwrap()
    }

    #[test]
    fn omitted_five_hour_window_is_kept_when_weekly_did_not_reset() {
        let previous = ProviderSnapshot::new(
            Provider::Claude,
            vec![
                quota_window(WindowKind::FiveHour, 22.0, 2_000),
                quota_window(WindowKind::Weekly, 65.0, 10_000),
            ],
            1_000,
        );
        let mut current = ProviderSnapshot::new(
            Provider::Claude,
            vec![quota_window(WindowKind::Weekly, 66.0, 10_000)],
            1_100,
        );
        current.merge_omitted_windows(&previous);
        assert_eq!(
            current.window(WindowKind::FiveHour).unwrap().used_percent,
            22.0
        );
        assert_eq!(
            current.window(WindowKind::Weekly).unwrap().used_percent,
            66.0
        );
    }

    #[test]
    fn omitted_five_hour_window_is_not_kept_for_a_different_account() {
        let previous = ProviderSnapshot::new(
            Provider::Codex,
            vec![
                quota_window(WindowKind::FiveHour, 80.0, 2_000),
                quota_window(WindowKind::Weekly, 31.0, 10_000),
            ],
            1_000,
        )
        .with_account_id(Some("account-a".to_string()));
        let mut current = ProviderSnapshot::new(
            Provider::Codex,
            vec![quota_window(WindowKind::Weekly, 12.0, 10_000)],
            1_100,
        )
        .with_account_id(Some("account-b".to_string()));
        current.merge_omitted_windows(&previous);
        assert!(current.window(WindowKind::FiveHour).is_none());
        assert_eq!(
            current.window(WindowKind::Weekly).unwrap().used_percent,
            12.0
        );
    }

    #[test]
    fn omitted_five_hour_window_is_not_kept_when_only_one_snapshot_has_an_account_id() {
        let previous = ProviderSnapshot::new(
            Provider::Codex,
            vec![
                quota_window(WindowKind::FiveHour, 80.0, 2_000),
                quota_window(WindowKind::Weekly, 31.0, 10_000),
            ],
            1_000,
        )
        .with_account_id(Some("account-a".to_string()));
        let mut current = ProviderSnapshot::new(
            Provider::Codex,
            vec![quota_window(WindowKind::Weekly, 31.0, 10_000)],
            1_100,
        );
        current.merge_omitted_windows(&previous);
        assert!(current.window(WindowKind::FiveHour).is_none());
    }

    #[test]
    fn omitted_five_hour_window_is_dropped_when_weekly_resets() {
        let previous = ProviderSnapshot::new(
            Provider::Codex,
            vec![
                quota_window(WindowKind::FiveHour, 99.0, 8_000),
                quota_window(WindowKind::Weekly, 49.0, 10_000),
            ],
            1_000,
        );
        let mut current = ProviderSnapshot::new(
            Provider::Codex,
            vec![quota_window(WindowKind::Weekly, 0.0, 9_700)],
            1_200,
        );
        current.merge_omitted_windows(&previous);
        assert!(current.window(WindowKind::FiveHour).is_none());
        assert_eq!(
            current.window(WindowKind::Weekly).unwrap().used_percent,
            0.0
        );
    }

    #[test]
    fn empty_windows_still_clear_stale_quota() {
        let previous = ProviderSnapshot::new(
            Provider::Claude,
            vec![
                quota_window(WindowKind::FiveHour, 22.0, 2_000),
                quota_window(WindowKind::Weekly, 65.0, 10_000),
            ],
            1_000,
        );
        let mut current = ProviderSnapshot::new(Provider::Claude, vec![], 1_100);
        current.merge_omitted_windows(&previous);
        assert!(current.windows.is_empty());
    }

    #[test]
    fn expired_five_hour_window_is_not_preserved() {
        let previous = ProviderSnapshot::new(
            Provider::Claude,
            vec![
                quota_window(WindowKind::FiveHour, 22.0, 1_050),
                quota_window(WindowKind::Weekly, 65.0, 10_000),
            ],
            1_000,
        );
        let mut current = ProviderSnapshot::new(
            Provider::Claude,
            vec![quota_window(WindowKind::Weekly, 65.0, 10_000)],
            1_100,
        );
        current.merge_omitted_windows(&previous);
        assert!(current.window(WindowKind::FiveHour).is_none());
    }

    #[test]
    fn claude_known_session_does_not_borrow_the_provider_model() {
        let mut snapshot = ProviderSnapshot::new(Provider::Claude, vec![], 0)
            .with_model(Some("latest".to_string()));
        snapshot
            .session_models
            .insert("session-1".to_string(), "Sonnet".to_string());
        assert_eq!(
            snapshot.model_for_session(Some("session-1")),
            Some("Sonnet")
        );
        assert_eq!(snapshot.model_for_session(Some("session-2")), None);
        assert_eq!(snapshot.model_for_session(None), Some("latest"));
    }

    #[test]
    fn claude_session_with_windows_does_not_borrow_provider_model_or_context() {
        let mut snapshot = ProviderSnapshot::new(
            Provider::Claude,
            vec![quota_window(WindowKind::FiveHour, 10.0, 10_000)],
            0,
        )
        .session_local()
        .with_model(Some("Opus".to_string()))
        .with_context(Some(ContextUsage::new(40.0).unwrap()));
        snapshot.session_windows.insert(
            "session-1".to_string(),
            vec![quota_window(WindowKind::FiveHour, 10.0, 10_000)],
        );
        snapshot.session_windows.insert(
            "session-2".to_string(),
            vec![quota_window(WindowKind::FiveHour, 80.0, 10_000)],
        );
        snapshot
            .session_models
            .insert("session-2".to_string(), "Opus".to_string());
        snapshot
            .session_contexts
            .insert("session-2".to_string(), ContextUsage::new(40.0).unwrap());

        assert_eq!(snapshot.model_for_session(Some("session-1")), None);
        assert!(snapshot.context_for_session(Some("session-1")).is_none());
        assert_eq!(snapshot.model_for_session(Some("session-2")), Some("Opus"));
        assert_eq!(
            snapshot
                .context_for_session(Some("session-2"))
                .map(|context| context.used_percent),
            Some(40.0)
        );
    }

    #[test]
    fn devin_new_session_without_model_switch_uses_config_default() {
        let mut snapshot = ProviderSnapshot::new(Provider::Devin, vec![], 0)
            .with_model(Some("SWE-1.7 Medium".to_string()));
        // A brand-new Devin session has a Herdr session id but no /model entry.
        assert_eq!(
            snapshot.model_for_session(Some("session-a")),
            Some("SWE-1.7 Medium")
        );
        assert_eq!(
            snapshot.model_for_session(Some("session-b")),
            Some("SWE-1.7 Medium")
        );
        snapshot
            .session_models
            .insert("session-a".to_string(), "Opus 4.6".to_string());
        assert_eq!(
            snapshot.model_for_session(Some("session-a")),
            Some("Opus 4.6")
        );
        assert_eq!(
            snapshot.model_for_session(Some("session-b")),
            Some("SWE-1.7 Medium")
        );
    }

    #[test]
    fn account_level_windows_are_shared_when_no_session_has_reported() {
        let snapshot = ProviderSnapshot::new(
            Provider::Grok,
            vec![quota_window(WindowKind::Weekly, 31.0, 10_000)],
            1,
        );
        assert_eq!(
            snapshot
                .windows_for_session(Some("session-1"))
                .first()
                .map(|window| window.used_percent),
            Some(31.0)
        );
        assert_eq!(
            snapshot
                .windows_for_session(None)
                .first()
                .unwrap()
                .used_percent,
            31.0
        );
    }

    #[test]
    fn statusline_windows_do_not_leak_across_sessions() {
        let mut snapshot = ProviderSnapshot::new(
            Provider::Claude,
            vec![quota_window(WindowKind::Weekly, 90.0, 10_000)],
            1,
        );
        snapshot.session_windows.insert(
            "work".to_string(),
            vec![quota_window(WindowKind::Weekly, 10.0, 10_000)],
        );
        snapshot.session_windows.insert(
            "personal".to_string(),
            vec![quota_window(WindowKind::Weekly, 90.0, 10_000)],
        );

        assert_eq!(
            snapshot
                .windows_for_session(Some("work"))
                .first()
                .unwrap()
                .used_percent,
            10.0
        );
        assert_eq!(
            snapshot
                .windows_for_session(Some("personal"))
                .first()
                .unwrap()
                .used_percent,
            90.0
        );
        assert!(snapshot.windows_for_session(Some("unknown")).is_empty());
        assert_eq!(
            snapshot
                .windows_for_session(None)
                .first()
                .unwrap()
                .used_percent,
            90.0
        );
    }

    #[test]
    fn agy_subagent_id_uses_the_only_observed_statusline_conversation() {
        // Live failure: statusLine keys the parent conversation while Herdr's
        // PreInvocation hook can report a spawned DeepCoderWorkerL0 id. With
        // one observed Agy conversation the attribution is unambiguous.
        let mut snapshot = ProviderSnapshot::new(
            Provider::Agy,
            vec![quota_window(WindowKind::FiveHour, 0.14411, 10_000)],
            1,
        )
        .session_local()
        .with_model(Some("Gemini 3.8 Flash (High)".to_string()));
        snapshot.session_windows.insert(
            "ed02b39b-7ea3-46c9-9855-1672f4ef7e91".to_string(),
            snapshot.windows.clone(),
        );
        snapshot.session_models.insert(
            "ed02b39b-7ea3-46c9-9855-1672f4ef7e91".to_string(),
            "Gemini 3.8 Flash (High)".to_string(),
        );
        snapshot.context = Some(ContextUsage::new(3.427886962890625).unwrap());
        snapshot.session_contexts.insert(
            "ed02b39b-7ea3-46c9-9855-1672f4ef7e91".to_string(),
            ContextUsage::new(3.427886962890625).unwrap(),
        );

        let herdr_id = "6a4d6f77-88be-4704-adcc-a51401ad7c03";
        assert_eq!(
            snapshot
                .windows_for_session(Some(herdr_id))
                .first()
                .unwrap()
                .used_percent,
            0.14411
        );
        assert_eq!(
            snapshot.model_for_session(Some(herdr_id)),
            Some("Gemini 3.8 Flash (High)")
        );
        assert_eq!(
            snapshot
                .context_for_session(Some(herdr_id))
                .map(|context| context.used_percent),
            Some(3.427886962890625)
        );
    }

    #[test]
    fn agy_unknown_subagent_does_not_borrow_when_two_conversations_exist() {
        let mut snapshot = ProviderSnapshot::new(
            Provider::Agy,
            vec![quota_window(WindowKind::FiveHour, 80.0, 10_000)],
            1,
        )
        .session_local()
        .with_model(Some("Claude Sonnet".to_string()))
        .with_context(Some(ContextUsage::new(70.0).unwrap()));
        snapshot.session_windows.insert(
            "parent-gemini".to_string(),
            vec![quota_window(WindowKind::FiveHour, 10.0, 10_000)],
        );
        snapshot.session_windows.insert(
            "parent-claude".to_string(),
            vec![quota_window(WindowKind::FiveHour, 80.0, 10_000)],
        );
        snapshot
            .session_models
            .insert("parent-gemini".to_string(), "Gemini Flash".to_string());
        snapshot
            .session_models
            .insert("parent-claude".to_string(), "Claude Sonnet".to_string());
        snapshot.session_contexts.insert(
            "parent-gemini".to_string(),
            ContextUsage::new(20.0).unwrap(),
        );
        snapshot.session_contexts.insert(
            "parent-claude".to_string(),
            ContextUsage::new(70.0).unwrap(),
        );

        assert_eq!(
            snapshot
                .windows_for_session(Some("parent-gemini"))
                .first()
                .unwrap()
                .used_percent,
            10.0
        );
        assert_eq!(
            snapshot.model_for_session(Some("parent-gemini")),
            Some("Gemini Flash")
        );
        assert_eq!(
            snapshot
                .context_for_session(Some("parent-gemini"))
                .map(|context| context.used_percent),
            Some(20.0)
        );

        assert!(snapshot
            .windows_for_session(Some("unknown-subagent"))
            .is_empty());
        assert_eq!(snapshot.model_for_session(Some("unknown-subagent")), None);
        assert!(snapshot
            .context_for_session(Some("unknown-subagent"))
            .is_none());
    }

    #[test]
    fn agy_windows_hit_does_not_borrow_another_panes_model_or_context() {
        let mut snapshot = ProviderSnapshot::new(
            Provider::Agy,
            vec![quota_window(WindowKind::FiveHour, 80.0, 10_000)],
            1,
        )
        .session_local()
        .with_model(Some("Claude Sonnet".to_string()))
        .with_context(Some(ContextUsage::new(70.0).unwrap()));
        snapshot.session_windows.insert(
            "w1:p1".to_string(),
            vec![quota_window(WindowKind::FiveHour, 10.0, 10_000)],
        );
        snapshot.session_windows.insert(
            "w1:p2".to_string(),
            vec![quota_window(WindowKind::FiveHour, 80.0, 10_000)],
        );
        snapshot
            .session_models
            .insert("w1:p2".to_string(), "Claude Sonnet".to_string());
        snapshot
            .session_contexts
            .insert("w1:p2".to_string(), ContextUsage::new(70.0).unwrap());

        assert_eq!(
            snapshot
                .windows_for_session(Some("w1:p1"))
                .first()
                .unwrap()
                .used_percent,
            10.0
        );
        assert_eq!(snapshot.model_for_session(Some("w1:p1")), None);
        assert!(snapshot.context_for_session(Some("w1:p1")).is_none());

        assert_eq!(
            snapshot.model_for_session(Some("w1:p2")),
            Some("Claude Sonnet")
        );
        assert_eq!(
            snapshot
                .context_for_session(Some("w1:p2"))
                .map(|context| context.used_percent),
            Some(70.0)
        );
    }

    #[test]
    fn agy_single_observation_may_use_latest_model_when_session_maps_miss() {
        let mut snapshot = ProviderSnapshot::new(
            Provider::Agy,
            vec![quota_window(WindowKind::FiveHour, 10.0, 10_000)],
            1,
        )
        .session_local()
        .with_model(Some("Gemini Flash".to_string()))
        .with_context(Some(ContextUsage::new(20.0).unwrap()));
        snapshot.session_windows.insert(
            "w1:p7".to_string(),
            vec![quota_window(WindowKind::FiveHour, 10.0, 10_000)],
        );

        assert_eq!(
            snapshot.model_for_session(Some("w1:p7")),
            Some("Gemini Flash")
        );
        assert_eq!(
            snapshot
                .context_for_session(Some("w1:p7"))
                .map(|context| context.used_percent),
            Some(20.0)
        );
        assert_eq!(
            snapshot.model_for_session(Some("unknown-subagent")),
            Some("Gemini Flash")
        );
        assert_eq!(
            snapshot
                .context_for_session(Some("unknown-subagent"))
                .map(|context| context.used_percent),
            Some(20.0)
        );
    }

    #[test]
    fn claude_profile_scope_shares_the_latest_quota_across_sessions() {
        let mut snapshot = ProviderSnapshot::new(
            Provider::Claude,
            vec![quota_window(WindowKind::FiveHour, 92.0, 16_000)],
            1,
        );
        snapshot
            .session_quota_scopes
            .insert("session-a".to_string(), "scope-w".to_string());
        snapshot
            .session_quota_scopes
            .insert("session-b".to_string(), "scope-w".to_string());
        snapshot.session_windows.insert(
            "session-a".to_string(),
            vec![quota_window(WindowKind::FiveHour, 5.0, 16_000)],
        );
        snapshot.session_windows.insert(
            "session-b".to_string(),
            vec![quota_window(WindowKind::FiveHour, 92.0, 16_000)],
        );
        snapshot.quota_scope_windows.insert(
            "scope-w".to_string(),
            vec![quota_window(WindowKind::FiveHour, 92.0, 16_000)],
        );

        assert_eq!(
            snapshot
                .windows_for_session(Some("session-a"))
                .first()
                .unwrap()
                .used_percent,
            92.0
        );
        assert_eq!(
            snapshot
                .windows_for_session(Some("session-b"))
                .first()
                .unwrap()
                .used_percent,
            92.0
        );
    }

    #[test]
    fn claude_profile_scopes_stay_isolated_when_reset_times_match() {
        let mut snapshot = ProviderSnapshot::new(
            Provider::Claude,
            vec![quota_window(WindowKind::FiveHour, 82.0, 16_000)],
            1,
        );
        snapshot
            .session_quota_scopes
            .insert("work".to_string(), "scope-w".to_string());
        snapshot
            .session_quota_scopes
            .insert("personal".to_string(), "scope-p".to_string());
        snapshot.quota_scope_windows.insert(
            "scope-w".to_string(),
            vec![quota_window(WindowKind::FiveHour, 18.0, 16_000)],
        );
        snapshot.quota_scope_windows.insert(
            "scope-p".to_string(),
            vec![quota_window(WindowKind::FiveHour, 82.0, 16_000)],
        );

        assert_eq!(
            snapshot
                .windows_for_session(Some("work"))
                .first()
                .unwrap()
                .used_percent,
            18.0
        );
        assert_eq!(
            snapshot
                .windows_for_session(Some("personal"))
                .first()
                .unwrap()
                .used_percent,
            82.0
        );
        assert!(snapshot.windows_for_session(Some("unknown")).is_empty());
    }

    #[test]
    fn legacy_snapshots_deserialize_without_quota_scope_maps() {
        let snapshot: ProviderSnapshot = serde_json::from_str(
            r#"{
                "provider":"claude",
                "source":"claude-statusline",
                "fetched_at_unix":1,
                "windows":[{"kind":"five_hour","used_percent":90.0,"remaining_percent":10.0}],
                "session_windows":{
                    "old":[{"kind":"five_hour","used_percent":20.0,"remaining_percent":80.0}]
                }
            }"#,
        )
        .unwrap();
        assert!(snapshot.session_quota_scopes.is_empty());
        assert!(snapshot.quota_scope_windows.is_empty());
        assert_eq!(
            snapshot
                .windows_for_session(Some("old"))
                .first()
                .unwrap()
                .used_percent,
            20.0
        );
        assert!(snapshot.windows_for_session(Some("unknown")).is_empty());
    }

    #[test]
    fn an_expired_window_is_not_current() {
        let live = quota_window(WindowKind::FiveHour, 20.0, 1_001);
        let expired = quota_window(WindowKind::FiveHour, 20.0, 1_000);
        assert!(live.is_current(1_000));
        assert!(!expired.is_current(1_000));
        assert!(!expired.is_current(1_001));
        assert!(UsageWindow::new(WindowKind::FiveHour, 20.0, None)
            .unwrap()
            .is_current(1_001));
    }

    #[test]
    fn a_snapshot_has_expired_quota_only_when_a_reset_is_in_the_past() {
        let live = ProviderSnapshot::new(
            Provider::Codex,
            vec![
                quota_window(WindowKind::FiveHour, 96.0, 2_000),
                quota_window(WindowKind::Weekly, 48.0, 10_000),
            ],
            1_000,
        );
        assert!(!live.has_expired_quota(1_999));
        let expired = ProviderSnapshot::new(
            Provider::Codex,
            vec![
                quota_window(WindowKind::FiveHour, 96.0, 1_000),
                quota_window(WindowKind::Weekly, 48.0, 10_000),
            ],
            900,
        );
        assert!(expired.has_expired_quota(1_000));
        let undated = ProviderSnapshot::new(
            Provider::Codex,
            vec![UsageWindow::new(WindowKind::Weekly, 48.0, None).unwrap()],
            1_000,
        );
        assert!(!undated.has_expired_quota(5_000));
        assert!(!ProviderSnapshot::new(Provider::Codex, vec![], 1_000).has_expired_quota(5_000));
        let mut session = ProviderSnapshot::new(Provider::Claude, vec![], 1_000);
        session.session_windows.insert(
            "s1".to_string(),
            vec![quota_window(WindowKind::FiveHour, 90.0, 1_000)],
        );
        assert!(session.has_expired_quota(1_001));
        assert!(session.displayed_quota_has_expired(Some("s1"), 1_001));
        assert!(!session.displayed_quota_has_expired(Some("other"), 1_001));
    }
}
