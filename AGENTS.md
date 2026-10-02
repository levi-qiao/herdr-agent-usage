# Agent guide

Notes for agents working on `herdr-agent-usage`. Read this before touching
anything that talks to Herdr.

## Working method

1. Establish the exact requested scope and inspect the current diff before
   editing. Treat unrelated worktree changes as user-owned.
2. Separate observed facts, inferences, and unknowns. When evidence is missing,
   name the cheapest useful verification instead of guessing.
3. Prefer the smallest surgical change that creates a checkable behavior. Add
   abstractions only when two real callers or adapters need the same seam.
4. Give every implementation step a verification condition and run the
   repository gates before calling it complete.
5. For multi-goal work, keep the decision record and dispatch prompts under
   the ignored `.agents/` directory. Public documentation must describe shipped
   behavior, not private execution state.

Before changing dependencies, inspect `Cargo.toml`, `Cargo.lock`, and
`rust-toolchain.toml`. Use the pinned Rust toolchain and repository-local Cargo
artifacts; do not install project tooling globally.

## The rule that matters most: reading or writing a pane is not free

Read pane output only with `--source visible` or `detection`; `recent` and
`recent-unwrapped` rebuild scrollback and visibly repaint the agent TUI.
Metadata writes also carry repaint risk, so avoid no-op writes. Extract the topic
from the event pane while visible; if extraction fails, preserve its existing topic.

For scrolling reports or changes to pane-read behavior, read
[pane repaint diagnosis](docs/pane-repaint-diagnosis.md) before probing.
Scroll offsets and before/after content hashes cannot detect the transient repaint;
use the documented human observation rather than polling live panes.

Concretely, this means:

1. **Never read every pane of a provider.** An event names one pane; read only
   that one. Fanning out across panes multiplies the repaints by the number of
   panes the user has open for that agent.
2. **Publish once per invocation.** Two `publish` passes in a row means each
   pane can take two metadata writes for one user action.
3. **Keep `metadata_matches` honest** (`src/herdr.rs`). It is the only thing
   stopping a no-op refresh from repainting every pane. If you add a token,
   add it to `METADATA_TOKEN_NAMES` too, or the comparison silently stops
   covering it and every refresh becomes a write.
4. **Preserve, don't clear.** When a topic read fails or finds nothing, keep
   the previously published topic. Clearing it churns the token and triggers
   a write on the next refresh, which triggers a repaint.

## Event paths, and what each is allowed to do

| Entry point | Fired by | Allowed to read panes? |
|---|---|---|
| `startup` | Herdr's `[[startup]]` hook | No |
| `refresh` | manual action, `startup` | No |
| `event` | `pane.agent_detected`, `pane.agent_status_changed` | Only the pane named in `HERDR_PLUGIN_EVENT_JSON`, and never a Pi, omp, Muse, or Cursor pane — their transcripts carry the evidence |
| `focus` | `pane.focused`, `workspace.focused`, `tab.focused` | No |
| `watch` | detached from a working status event | No (agent metadata only) |

`startup` exists because Herdr drops plugin-owned Agent views when the server
exits, and startup hooks run again after a restart or a live handoff. It
restores plugin-owned views, forces one quota refresh, and restores the watcher.
It is also the second home of the one repair `configure --apply` can only make
once: when omp is selected and Herdr says its integration is still missing,
and omp's own agent directory exists, startup installs it. A machine that
installed omp after this plugin would otherwise keep detecting omp panes
without a session forever, and a pane in that state has nothing to publish and
says so (`unattributed_session_reason`) instead of rendering an empty row.
Plugin enable alone does not run startup; the configure action runs it after
repair. Server-owned event/refresh paths also record the current Herdr binary
and socket so an older watcher can adopt the new connection.

`pane.agent_status_changed` fires **twice per turn** (idle→working on submit,
working→idle on completion). Anything `event` does, the user pays for twice
every time they press Enter. Budget accordingly.

The working event starts one global `watch` pulse. It calls `herdr agent list`
once per configured interval for every supported harness, including Pi, OMP,
OpenCode, Muse, and Cursor. Event-spawned watchers defer their first poll. They resolve local
billing targets, refresh active/settling targets, and publish to siblings with
the same target without reading terminal output. A finishing target stays in
the pass until the 60-second debounce has elapsed. The interval defaults to
60 seconds and is bounded to 30 seconds–1 hour. While a pane is working or
has an unseen completion, the watcher also checks the metadata-only Herdr
snapshot once per second. Herdr 0.9 can miss TUI focus hooks; the snapshot
reconciles those changes without reading pane output or writing unchanged
metadata. The watcher stays alive for unseen completions until they are seen.
Local stop/connection checks interrupt sleeps. Uninstall writes a stop marker.

## omp's quota does not come from a provider endpoint

Every other collector either reads a local credential and calls the provider
(`codex`, `grok`, `opencode_go`, `devin`, `muse`, `cursor`) or waits for a
statusLine hook (`claude`, `agy`). omp is the exception: it keeps its own credential store and
ships its own usage layer, so `src/providers/omp.rs` shells out to
`omp usage --json --provider <id>` and reads the answer.

Three properties hold that together, and each one is load bearing:

1. **One provider, never the pool.** The call always names the provider the
   pane's transcript is talking to. Asking for everything would poll every
   subscription the user has in omp, on a pane event.
2. **Two caches, deliberately.** omp answers from its own five-minute usage
   cache in `agent.db`; on top of that this plugin debounces to 60 seconds per
   target and stores the sanitized report for all accounts returned by that one provider. Neither layer may be removed on
   the theory that the other covers it — omp's cache is what stops a provider
   request, ours is what stops a process spawn.
3. **`agent.db` is never opened.** It holds live OAuth tokens. Everything
   needed — the account identity and the quota — is in the CLI's output.
   `models.db` is opened read-only, because the context window is the one thing
   the CLI cannot give cheaply.

An omp pane is billed in `CredentialScope::OMP_STORE`, not the canonical scope.
An omp Claude pane and a Claude Code pane can be two different subscriptions,
so they must never share a cache file; `BillingTarget::cache_identity` is what
keeps them apart, and it is the reason that function appends a scope.

Attribution is by omp's `credential_pin`: the transcript records
`sha256(provider\0accountId\0email\0orgId\0projectId)` of the serving
account, and `providers::omp::account_pin` recomputes it from the usage
report's identity. That digest is omp's persisted contract — if it changes
upstream, every pin is orphaned and multi-account panes silently fall back to
"no quota". The pinned-digest test exists to make that a test failure rather
than a wrong number.

## Quota attribution and cache upgrades

- Direct API snapshots carry an account ID or credential hash. Unstamped old
  caches cannot prove a current login. A failed attempt is debounced by the
  attempted identity; a different login can refresh immediately.
- Codex rollouts provide diagnostics only. Fresh API windows replace old
  windows, including ones an older plugin borrowed from a rollout.
- Claude/Agy StatusLine has no reliable serving-account ID. New observations
  carry `session_quota_only`; they never share windows by profile directory.
  Rebuild old mailboxes from their raw payload, not merged profile windows.
- The Claude statusLine hook stamps `session_accounts` with
  `sha256(accountUuid\0organizationUuid)` from `oauthAccount` in the
  `.claude.json` of its own `CLAUDE_CONFIG_DIR` (else `~/.claude.json`). Only
  those fields are deserialized; the file also holds MCP `env` values.
  Sessions with one stamp are one account: they share a sidebar row and show
  the newest reading among them. No stamp means no sharing. Agy has no stamp
  and never shares a row.
- Sidebar row sharing goes through `herdr::QuotaGroups`, built from Herdr's
  inventory order. Under Herdr's own agent order only a run of adjacent panes
  is one group, and its first pane is the head. Do not derive group
  membership from `nest_group_key` alone.
- Agy must identify the active pool or receive only one possible pool. Do not
  combine Gemini and third-party quotas for an unknown model.
- OMP stores all accounts in one sanitized provider report so a second pin
  does not lose its quota during debounce. Select by pin; keep a failed
  account's old reading only while the report still identifies that account.
- Cursor stamps `sha256("cursor\0" || access token)`. Included is
  `planUsage.totalPercentUsed` when present — the CLI usage panel's Included
  row — and only then `includedSpend / limit`. The IDE `state.vscdb` mtime is
  not a credential gate. On macOS without `$CURSOR_HOME` / `$CURSOR_AUTH_FILE`
  / `$CURSOR_STATE_DB`, `auth_mtime_unix` is none — a `stat` of
  `~/.cursor/auth.json` is enough for Ghostty TCC, and the Keychain approval
  marker is not a login generation (`cursor-agent login` overwrites
  `cursor-access-token` in place). Re-read that Keychain item; do not keep the
  previous token in the watch process. On macOS, `cursor-agent login` stores
  the token in Keychain, not `auth.json`; do not fall back to the IDE token
  while the CLI still has `cli-config.json` `authInfo`.

## Devin's per-session model is local SQLite, not the quota API

`~/.local/share/devin/cli/sessions.db` is CLI session state. Open it
read-only and select only `id, model` — the same discipline as omp
`models.db`, not `agent.db`. A missing, locked, or unexpected schema skips
per-session attribution. `config.json` `agent.model` stays on
`snapshot.model` as the fallback and is never copied into `session_models`.

## Muse panes are matched through Muse's session lock

Herdr has no Muse session integration, so a Muse pane arrives without an
`agent_session`. `herdr::list_agent_state` fills it in from evidence Muse
writes itself: the `muse-bin` process inherits its pane's `HERDR_PANE_ID`, and
`sessions/<yyyy>/<mm>/<dd>/<id>/.session.lock` holds that process's
`pid=<n>`. Read only `comm` and the `HERDR_PANE_ID` entry of a process
environment, never anything else from it. A session Herdr does report always
wins. No `/proc` (macOS) means no session, never a guessed one.

The quota call (`muse-code/key`) also returns the account's API key and
identity. Only `subs_usage` is read. A `storage: "keychain"` login keeps the
OAuth token out of `auth.json`; the collector then reads that one item through
`security find-generic-password` (service `ai.meta.dev.credentials`, account
`meta`). Background processes never prompt: without a recorded approval marker
the keychain branch is skipped outright, and the user approves once via
`refresh --provider muse --keychain-approve` (click **Always Allow**, not
Allow). The marker lives beside the Muse config dir so every process — herdr
hook, daemon, or plain terminal — resolves the same path. A successful token
is kept in the watch process until the auth file's identity changes or
`muse-code/key` returns 401/403; a failed lookup is not cached and does not
clear the marker, so a transient failure retries on the next refresh. Only
`access_token` is taken from the payload; file-storage logins are unchanged.
No stored account
login (an API-key login) or an inactive subscription yields a snapshot without
windows, but only while a Muse session is refreshed, so its local fields still
publish. A rejected token or failed request stays an error, which keeps the
cached quota. Session-local fields come from the
bounded tail of `session.jsonl`: the last `model_completed` usage against the
`model-catalog` context limit, and the last prompt as topic: a main-surface
chat `runtime.user_intent.accepted`, with `user_prompt_display` accepted too
because Muse writes it only for some submits.
Muse publishes no prompt-cache lifetime, so there is no TTL estimate.

## Cursor's quota is DashboardService, not a browser cookie

Cursor Agent CLI is a separate install from the desktop app. Herdr's kind and
PATH command are `cursor` (alias `cursor-agent`). Never call a bare `agent` —
that name is Grok's on machines that have both. Herdr has a session
integration (`herdr integration install cursor`). Event does not read the
pane: the generated session title (`meta.json` `title`, else `store.db`
`name`) is the topic. Placeholder `New Agent` falls back to the last
`<user_query>` in the session jsonl.

Credentials, in order: `accessToken` in the CLI auth file (`$CURSOR_AUTH_FILE`,
else `~/.cursor/auth.json` on macOS, else `$XDG_CONFIG_HOME/cursor/auth.json`);
on macOS, Keychain item `cursor-access-token` / `cursor-user` (what
`cursor-agent login` writes when `AGENT_CLI_CREDENTIAL_STORE` is default);
then `cursorAuth/accessToken` in the desktop `state.vscdb` only when the CLI
has no login of its own **and** `$CURSOR_STATE_DB` is set. On macOS, do not
open `~/.cursor` or the default `~/Library/Application Support/Cursor/…/state.vscdb`
from event/watch/refresh/hook: those trees are Cursor-provenance, this binary
is ad-hoc, and TCC prompts Ghostty "would like to access data from other
apps" on every process. File reads resume only with `$CURSOR_HOME` /
`$CURSOR_AUTH_FILE` / `$CURSOR_STATE_DB`. Model/cache/context come from the
hook mailbox; the Keychain approval marker lives in plugin state. Do not
treat that marker as `auth_mtime_unix`, and do not process-cache the Keychain
secret against it: a `stat` of `~/.cursor/auth.json` is Ghostty TCC, and a
cached token keeps the previous account's quota while that token remains
valid. Never copy the IDE database, never use its mtime as a gate, never read
`refreshToken`, never send a `WorkosCursorSessionToken` cookie. Background
processes never prompt for Keychain: without a recorded approval marker the
keychain branch is skipped, and the user approves once via
`refresh --provider cursor --keychain-approve` (click **Always Allow**, not
Allow). The collector does not write, refresh, or exchange tokens; a 401
re-reads the current files and Keychain once.

Quota is `POST https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage`
with `Connect-Protocol-Version: 1`, the same call the CLI makes. Included is
`planUsage.totalPercentUsed` when present — the CLI usage panel's "Included"
row — and only then `includedSpend / limit`. The three bars map onto at
(`autoPercentUsed`, 5h), api (`apiPercentUsed`, 7d), and 30d (Included).
`billingCycleEnd` is Unix milliseconds. Model is
`cli-config.json` `model.displayName` — the CLI footer after a model switch.
store.db meta `lastUsedModel` overrides that only when it names a specific
model; `default` / `auto` keep the catalog, because Cursor does not rewrite
the store field when you change models in an existing session (never the
encrypted blobs). Turn token counts are not in the
jsonl. Cache and context come from the interactive CLI's `afterAgentResponse`,
`stop`, and `preCompact` hooks: token counts map the same way the CLI
statusLine `current_usage` does (`fresh = input - cache_read - cache_write`);
Context percent is `store.db` `token_details.used_tokens / max_tokens`, the
same numbers the CLI footer prints (`Auto · 8.1%`). Only that protobuf field
is read. Cache still comes from the hooks; `context_usage_percent` wins when
present, otherwise last `input_tokens` against `context_window_size`,
Composer 2.x's documented 200k window, or Auto/`default`'s 256k window.
`configure` writes `herdr-agent-usage-hooks.sh` under plugin state — not
next to `hooks.json`. `bash ~/.cursor/…` is a Ghostty-attributed open of
Cursor-provenance files and prompts twice per turn (`afterAgentResponse`
then `stop`). It never replaces Herdr's `sessionStart`. Cursor CLI loads user
hooks at session start, so an already-running pane must be restarted. Do not
install a Cursor `statusLine` — that setting replaces the native CLI footer. Cursor publishes no prompt-cache lifetime, so there is
no TTL. Cache identity is `sha256("cursor\0" || token)`.

## Kilo's quota is the Kilo Pass credit period, not a rolling window

Kilo is already a Herdr harness — `herdr integration install kilo` gives it state
and session integration — so this plugin only had to add the readout. It has no
5h or 7h bucket for the Kilo Gateway. The allowance Kilo publishes is a monthly
credit total, so `src/providers/kilo.rs` produces exactly one
`WindowKind::Monthly` window and the 5h/7d sidebar tokens stay empty. Do not
"fill in" a short window from the monthly number.

Three sources, all read-only, in order of authority:

1. **Session evidence** — `~/.local/share/kilo/kilo.db`, opened read-only. Two
   bounded per-session reads: the newest assistant `message` row names the
   backend and model, and the newest `step-finish` `part` row gives the context.
   The step is the row Kilo's own Token Usage panel reads, and the one its
   partial index is built for. `session_message` and `session_input` exist in
   7.8.1 but are empty; `session_v2` does not exist, so nothing probes for it.
2. **Context window** — `~/.cache/kilo/models.json`,
   `kilo.models[model].limit.context`, same exact lookup as OpenCode's catalog.
3. **Quota** — `GET https://api.kilo.ai/api/trpc/kiloPass.getState`, the tRPC
   procedure the CLI itself calls for the "Kilo Pass" line in its account panel,
   authenticated with the OAuth device login in `auth.json`. The amounts are
   JSON numbers in US dollars. The host is pinned; `KILO_API_URL` is not
   honoured, because a configured override would move the login to a host this
   plugin cannot vouch for.

Attribution is by the login, not by the harness. A Kilo pane can be served by
OpenRouter, OpenCode Go, or anything else Kilo can drive, so `classify_kilo`
resolves to the Kilo Pass target only when the session's provider is `kilo`
**and** the store holds that provider's `oauth` entry. A gateway API key
(`KILO_API_KEY`, or a `kilo` entry of type `api`) is deliberately not accepted:
it bills the same account but cannot name it, so it can never be the attribution
for a reading. The identity stamped on the snapshot is
`credential_id(access)`, the same token-hash stamp Cursor uses; Kilo rotates
that token, so a rotation invalidates the cached snapshot and the next refresh
re-reads it. Never the refresh token, and no keychain or TCC path.

Two answers are normal and must degrade quietly rather than become a number:

- `subscription: null` — the account has no Kilo Pass and pays from a shared
  credit balance. There is no balance window: `/api/profile/balance` reports a
  dollar amount with no limit attached, and a percentage invented from it would
  be a guess.
- A `status` outside `active`/`past_due`/`trialing` — the same set the CLI uses.
  A cancelled or unpaid plan has nothing left to meter.

The allowance is `currentPeriodBaseCreditsUsd` plus
`currentPeriodBonusCreditsUsd`: bonus credits are granted into the same period
and expire with it, so they are allowance rather than a top-up outside the
window. Both halves of the ratio are required — a missing spend would read as
an untouched period, which presents as a full allowance.

Context is `input + cache.read + cache.write` on the step row. Output and
reasoning are excluded on purpose: Kilo folds them into the next request's
input, so counting them double-counts the window. `tokens.total` is not a
shortcut — Kilo computes it as input+output on 7291 of 9475 step rows and as
input+output+reasoning on the other 2185, so it means different things in
different versions.

## Kimi's quota is the CLI's own `/usages` call

Kimi Code (the Node CLI in `~/.kimi-code`, or `$KIMI_CODE_HOME`) has a Herdr
integration, so its panes arrive with a `session_<uuid>` id. Quota is
`GET https://api.kimi.com/coding/v1/usages` with the `access_token` from
`credentials/kimi-code.json` — the call behind the CLI's `/usage` panel. The
host is pinned. The token is never refreshed here: Kimi rotates its refresh
token on use, so a second refresher would sign the CLI out. An expired token
is a failed fetch and keeps the last good snapshot.

`limits[]` entries carry their own `window.duration`/`timeUnit`; only one that
measures exactly 5h or 7d fills that slot. The top-level `usage` is the weekly
allowance. Counts arrive as strings. Both `limit` and `used` (or `remaining`)
are required. The cache identity is `sha256("kimi\0" || user_id)` from the
access token's claims, so a rotation keeps the reading and another login
cannot inherit it.

Session fields never come from the pane: `session_index.jsonl` maps the id to
its directory (only under `<home>/sessions`), `state.json` `lastPrompt` is the
topic, and the bounded tail of `agents/main/wire.jsonl` gives the model alias
(`llm.request`), the token split (`usage.record`), and the measured context
(`token_counting.*`). `config.toml` `[models."<alias>"]` maps the alias to
`display_name` and `max_context_size`. Kimi publishes no prompt-cache
lifetime, so there is no TTL.

## Herdr state this plugin owns outside a pane

Two things reach past the pane metadata, and both are global to the Herdr
session rather than scoped to a pane. Low-quota notifications stay off until
the user sets a threshold. The Agent view is on by default (`--agent-order
quota`): Space grouping plus least-headroom ranking inside each space.

**The Agent view** (`agent.view.set`, `src/herdr.rs`). Herdr keeps exactly
one, and setting it replaces the user's own `ui.agent_panel_sort`. Rules:

1. **Always scope a clear to `plugin:herdr-agent-usage`.** An unscoped
   `agent.view.clear` would drop a view another plugin owns. `startup` goes
   further and does not call clear at all when the order is `default` — there
   is nothing of ours to restore, and silence is the only way to be sure a
   foreign view survives.
2. **Re-apply it from `startup` and a forced refresh, never from event.**
   Herdr drops a plugin-owned view on disable; enable does not run startup.
   The refresh action (`--force`) is the same repair that respawns the
   watcher. Event/focus/watch stay off this path so a turn does not spend a
   socket call.
3. It is the only thing in the plugin that speaks the raw socket protocol
   (`HERDR_SOCKET_PATH`), because `agent.view.*` has no CLI subcommand in
   Herdr 0.8. One request, one reply, one connection — nothing subscribes, so
   the `events.subscribe` replay and focus-storm problems do not apply.
4. **Quota order keeps Spaces contiguous.** The sort is
   `workspace_order` ascending, then `quota_headroom` ascending — never a
   flat headroom list that scatters one project's panes across the panel.
   `$quota_group` names the Space on the tightest pane in that workspace;
   `$quota_icon` is the vendor mark on every identity row (bundled icon font;
   Muse uses a text glyph). Working/done colour is an invisible suffix matched
   by sidebar `rules`, not a later twin token — a later `$quota_icon_done`
   hang-indents one cell under the Space name. Colour replaces Herdr's `state_icon`
   ring: yellow while working, teal for an unseen completion, white after
   focusing that pane or moving focus away from it. Do not trust CLI `agent_status` for the teal
   step — same-tab siblings finish as server `idle` while the TUI ring is
   still teal. Persist working/unseen pane ids in plugin state
   (`icon-attention.json`) and never call `herdr pane current` from
   `event`: status hooks set `HERDR_PANE_ID` to the finisher. Focus hooks
   mark only the previous and newly focused panes seen. A workspace or Tab
   switch may not emit `pane.focused`; resolve its pane from that location's
   layout in `herdr api snapshot`. Ignore a delayed event whose workspace or
   Tab is no longer focused.

**`quota_headroom`** is the token that view sorts on: the remaining percent of
the tightest of the pane's 5h, 7d, and 30d windows, zero-padded to three digits
so Herdr's ordering of the text is its numeric ordering. Two properties are
load bearing:

- It is published **unconditionally**, not only when the order is enabled. No
  sidebar row renders it, so it costs no screen space; publishing it always is
  what makes toggling the order a Herdr-side change instead of a metadata write
  to every pane, and it adds no writes, because it only moves when a quota
  token beside it moves anyway.
- It is scoped to the windows the sidebar actually **shows**. A window without
  a token never decides the sort or an alert.

**Low quota notifications** fire from both publish paths (`publish_resolved`
and `handle_named_pane`) so a warning lands at the end of the turn that spent
the quota. The state is a set of provider names, not a timestamp: a provider
stays quiet while it stays low and is re-armed only by recovering above the
threshold. A provider with **no pane in the pass keeps its entry** — dropping
it would make closing and reopening a pane a way to be warned twice.

## A plugin action cannot see the caller's environment

Herdr runs `[[actions]]` with a fixed command line **in the server's own
environment**. A variable exported around `herdr plugin action invoke` does not
reach the action. Measured with a temporary `printenv` action: of 61 variables,
the only Herdr-related ones present were `HERDR_PLUGIN_STATE_DIR` and
`HERDR_PLUGIN_CONFIG_DIR`, both injected by Herdr; neither the probe marker nor
`HERDR_AGENT_QUOTA_AGENTS` survived.

So `src/prefs.rs` — small files under `HERDR_PLUGIN_CONFIG_DIR` — is the only
channel an installer has for passing a choice to `configure`. Environment
variables still work for a **direct CLI run** and are read first, but anything
that must survive `install.sh` / `uninstall.sh` has to be written as a
preference. This bit once: `./uninstall.sh --agent grok` passed the selection
through `env`, it never arrived, and the default selection is *every* agent, so
a partial uninstall removed everything.

To re-check this on a new Herdr version, append a throwaway action running
`printenv > /tmp/probe.txt`, reload with `herdr plugin disable && herdr plugin
enable`, invoke it with a marker variable set, and read the file.

## Event payload shapes

`HERDR_PLUGIN_EVENT_JSON` is nested and not uniform across events. `pane.focused`
carries no `agent`: `focus` uses its pane ID and resolves the harness from one
agent inventory read. Only a direct `focus` invocation without event JSON uses
`herdr pane current`. Workspace and Tab focus events carry no pane ID, so
`focus` resolves the matching layout from `herdr api snapshot`. This keeps
delayed events from redirecting a refresh to another pane:

```json
{"event":"pane_focused","data":{"type":"pane_focused","pane_id":"w1:p9","workspace_id":"w1"}}
```

`find_agent` and `find_pane_id` in `src/refresh.rs` walk the tree rather than
assuming a fixed path. Keep them tolerant — the shapes differ per event and are
not part of a stable contract.

## Adding a harness

Append to `AgentSelection::SUPPORTED`. Never insert. A saved complete agent
list is a proper prefix of that array, and `parse_list` still reads an unmarked
prefix of length ≥ 6 as every agent. That is what #81 was: Muse grew
`SUPPORTED`, a settings-saved
`claude,codex,grok,agy,opencode,pi,omp,devin` became "partial", `ensure_omp`
took the hard-failure path, and `configure` aborted on a machine without omp.
The first six entries are the first complete list the settings pane wrote; do
not reorder them.

You do not add a historical snapshot by hand when you append — the prefix
rule covers the new tail. Settings and `install.sh --agent` write `all` or
`only,<names>` so a later "everything except the newest one" is not mistaken
for a legacy full list.

Wiring the new name is not enough. Also:

1. `Harness`, `from_agent_name`, `AgentSelection` (the enum, `parse`,
   `harness`, `harness_name`), clap `--agent` help, `install.sh` comments,
   both READMEs, and the plugin description.
2. A `PROVIDER_STYLES` row in `src/configure/herdr.rs`, in `SUPPORTED` order.
3. Settings popup `height` in `herdr-plugin.toml` — one more row. The
   `rows().len()` check fails if this is skipped.
4. If it has a subscription collector: `Provider`, `Provider::ALL`,
   `ProviderSelection`, the fetch path, and a cache identity. If Herdr has
   no integration for it, `integration_id` returns `None` (Agy, Muse).
5. If its own transcript is the evidence, `event` must not read the pane
   (Pi, omp, Muse, Cursor, Kimi).
6. Tests that name agents must walk `SUPPORTED`, not a copied list. A copied
   list is how Muse missed the watcher-alive check and the "installs
   everything" sidebar assertions.

Adding a **sidebar field** is the same shape as #76: a saved "everything on"
list will not name the new field. `FieldSet::parse` has to keep reading that
exact legacy list as `all()`, and `as_list` needs a marker for the one new
selection that would collide with it.

## Code Review Rules

For pull-request review, prioritize semantic correctness over whether the happy-path
tests pass. Treat the following as repository-specific invariants and call out
violations explicitly:

1. **Current means current.** Model, context, quota, cache, topic, and session
   fields presented as live/current must come from the newest applicable
   observation. Do not substitute a rollout/file head, an old cache entry, or a
   historical session value just because it is easier to read.
2. **Attribution must be provable.** Never merge or reuse quota, cache, model, or
   session data across accounts, credential scopes, providers, or sessions
   unless the code has evidence that they are the same billing/serving target.
   On ambiguity, prefer missing data over confidently wrong data.
3. **Bounded reads must preserve the requested semantics.** Prefix reads are fine
   for immutable metadata written at the start of a file; latest/current fields
   require a bounded tail or reverse scan, an equivalent seekable snapshot, or
   no value. A performance bound must not silently change "latest" into "first".
4. **Multiple files can be one logical record.** When upstream storage has plain,
   compressed, migrated, temporary, or otherwise alternate representations of
   the same logical object, follow upstream's canonical precedence and make
   transition behavior deterministic. Do not let directory iteration order or
   mtime ties decide correctness.
5. **Pane reads and metadata writes are side effects.** Reject changes that add
   broad pane scans, duplicate publish passes, or no-op metadata writes to
   event/watch/focus paths unless the behavior is explicitly required and
   measured. Preserve the one-pane/event and publish-once invariants above.
6. **Credential stores stay least-privilege.** New code must not broaden secret
   reads, copy credential databases, prompt from background processes, or use a
   less-specific credential scope just to make attribution easier.
7. **Compatibility lists are append-only contracts.** Changes to harnesses,
   providers, sidebar fields, token names, or persisted selection formats must
   preserve ordering/backward-compatibility rules documented in this file and
   include regression coverage for old saved state.
8. **Test representation transitions and stale-data traps.** For storage/cache
   changes, cover both representations when applicable, ambiguous/equal
   timestamps, corrupt or partial data, reused sessions/worktrees, and cases
   where an older value exists but must not be reported as current.

A review should distinguish blocking correctness/attribution/privacy regressions
from non-blocking cleanup or performance suggestions. Green CI is evidence, not
a substitute for checking these invariants.

## Verifying

```
cargo fmt
cargo test
cargo clippy --release
```

Reloading the plugin after a rebuild:

```
herdr plugin disable herdr-agent-usage && herdr plugin enable herdr-agent-usage
```
