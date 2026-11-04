# Security policy

## Reporting

Report vulnerabilities privately through
[GitHub Security Advisories](https://github.com/levi-qiao/herdr-agent-usage/security/advisories/new).
Do not include credentials in public issues. The expected initial response time
is seven days. Security fixes target the latest release.

## Data handling

The plugin reads configured CLI credentials, local session metadata/transcripts,
Claude/Agy StatusLine input, and Cursor CLI hook payloads (token counts and
context-window fields only). Codex quota is obtained through its app-server;
OMP quota through its usage CLI. OMP's credential database is never opened.
Local SQLite reads are read-only and limited to session/model data, plus Cursor
IDE's `state.vscdb` key `cursorAuth/accessToken` when the CLI has no login of
its own and `$CURSOR_STATE_DB` is set (macOS never opens the default
Cursor.app Application Support path). On macOS, Cursor Agent CLI and Muse Code both keep OAuth tokens in
Keychain (`cursor-access-token` / `cursor-user`, and Muse
`ai.meta.dev.credentials` / `meta`); the collector reads only those CLI items
through `security find-generic-password`. Background processes never prompt:
without a recorded approval marker the keychain branch is skipped outright, and
the user approves once via `refresh --provider cursor --keychain-approve` or
`refresh --provider muse --keychain-approve`. A successful token is kept in
the process until that file changes or the quota API rejects it; it is never
written to plugin state. Cursor credentials are never written, refreshed, or
exchanged.

Authenticated quota requests use the relevant CLI/provider's usage contract.
The plugin sends no model prompts and does not upload usage to another service.
It does not read browser cookies, other Keychain items, or manage provider
logins. Invoked CLIs remain responsible for their own credential lifecycle.

Plugin state can contain quota, account/session identifiers, model and cache
statistics, session summaries, preferences, and watcher coordination files.
Herdr metadata can contain a short visible prompt topic. Credentials are never
written to these files, logs, or pane metadata; credential-derived identifiers
are hashed. Account IDs supplied by a CLI may be retained as identifiers.

Installation modifies only managed Herdr configuration and selected agents'
hooks/integrations. User configuration is preserved or backed up for restoration;
uninstall removes the selected plugin-owned settings and stops background work.
