//! Herdr agent integration setup and diagnostics.
//!
//! Quota attribution starts from the session id Herdr reports for a pane, and
//! Herdr only learns that id once its integration for that agent is installed.
//! Without it a pane is detected but carries no session, so this plugin can
//! never attribute it and silently shows nothing. That is the least obvious
//! way for a fresh install to look broken, so `configure` says so out loud.
//!
//! omp is installed automatically when the user enables omp in this plugin;
//! otherwise the pane can be detected but has no session path, which makes
//! every omp feature look broken. Existing integrations are left untouched.

use crate::model::Harness;
use anyhow::{bail, Result};
use std::process::Command;

/// Herdr's integration id for a harness, when it has one.
///
/// Agy quota comes from the statusLine hook, not Herdr's session id. Herdr
/// ships `antigravity-cli` for resume, but that id can be a subagent
/// conversation and is not what statusLine keys quota by, so this plugin
/// does not wait on it. Muse quota is account-level, so a Muse pane needs
/// no session id to be attributed.
fn integration_id(harness: Harness) -> Option<&'static str> {
    match harness {
        Harness::Claude => Some("claude"),
        Harness::Codex => Some("codex"),
        Harness::Grok => Some("grok"),
        Harness::OpenCode => Some("opencode"),
        Harness::Pi => Some("pi"),
        Harness::Omp => Some("omp"),
        Harness::Devin => Some("devin"),
        Harness::Cursor => Some("cursor"),
        Harness::Kilo => Some("kilo"),
        Harness::Kimi => Some("kimi"),
        Harness::Agy | Harness::Muse => None,
    }
}

pub fn report_missing(agents: &[Harness]) {
    let Some(status) = read_status() else {
        return;
    };
    for harness in agents {
        let Some(id) = integration_id(*harness) else {
            continue;
        };
        if !is_missing(&status, id) {
            continue;
        }
        println!(
            "Herdr's {id} integration is not installed, so Herdr reports no session id for {id} panes and their quota cannot be attributed. Install it with `herdr integration install {id}`, then restart that agent pane."
        );
    }
}

/// Install the omp integration when omp is selected and Herdr explicitly says
/// it is absent. A missing `herdr` binary or an unrecognized status format is
/// not guessed at; the existing advisory remains the fallback.
///
/// `full_selection` is the every-agent default a Herdr plugin action always
/// runs with. There, a machine without omp skips the omp collector and keeps
/// configuring the others; only an explicit omp selection fails.
pub fn ensure_omp(agents: &[Harness], full_selection: bool) -> Result<()> {
    let Some(status) = read_status() else {
        return Ok(());
    };
    if !needs_omp_install(agents, &status) {
        return Ok(());
    }
    if let Err(detail) = install_omp() {
        return omp_install_failed(full_selection, &detail);
    }
    println!("Installed Herdr's omp integration. Restart already-running omp panes once.");
    Ok(())
}

/// Repair Herdr's omp integration on the startup path.
///
/// `configure --apply` is a one-shot repair: a machine that installed this
/// plugin *before* it installed omp skipped the collector once, printed one
/// line, and from then on every omp pane was detected without a session — no
/// quota, no model, no explanation. Startup runs again after every Herdr
/// restart (and every upgrade), which is exactly when an omp that appeared
/// since the last configure can be picked up.
///
/// Nothing is printed when omp is not on the machine. Herdr names the
/// extension's own path beside the state, and that path always lives inside
/// the agent's directory, so a missing one is the ordinary "omp is not
/// installed yet" case rather than a failure worth a log line on every
/// restart.
pub fn repair_omp_at_startup(agents: &[Harness]) {
    // An install that never selected omp must not spawn Herdr here: startup
    // runs on every server restart, and the selection is what decides whether
    // omp's integration belongs on this machine at all.
    if !agents.contains(&Harness::Omp) {
        return;
    }
    let Some(status) = read_status() else {
        return;
    };
    if !needs_omp_install(agents, &status) || !agent_dir_present(&status, "omp") {
        return;
    }
    match install_omp() {
        Ok(()) => {
            println!("Installed Herdr's omp integration. Restart already-running omp panes once.")
        }
        Err(detail) => println!("Herdr's omp integration is still missing: {detail}"),
    }
}

/// The install Herdr's own CLI performs, with its stderr as the detail.
///
/// Returned as a string rather than an error so the caller decides whether a
/// failure is a skip line, a hard error, or startup noise.
fn install_omp() -> Result<(), String> {
    let executable = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let output = Command::new(executable)
        .args(["integration", "install", "omp"])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(())
}

/// Whether the agent an integration feeds has a directory on this machine.
///
/// Herdr prints the extension's own path in the status line, and the path's
/// depth is Herdr's own per-harness choice — `<root>/hooks/<file>` for the
/// shimmed CLIs, `<agent dir>/extensions/<file>` for omp — so the grandparent is
/// the agent directory for the shape this build reads, and a unit test pins it.
/// A deeper target still answers correctly, because every part of it lives under
/// the agent directory; a shallower one would quietly weaken the signal, so an
/// unrecognized status line installs nothing rather than guessing at paths this
/// plugin does not own.
fn agent_dir_present(status: &str, id: &str) -> bool {
    integration_target(status, id)
        .and_then(|path| Some(path.parent()?.parent()?.to_path_buf()))
        .is_some_and(|directory| directory.is_dir())
}

/// The path Herdr would install an integration's file to.
fn integration_target(status: &str, id: &str) -> Option<std::path::PathBuf> {
    status.lines().find_map(|line| {
        let rest = line
            .trim()
            .strip_prefix(id)?
            .strip_prefix(':')?
            .trim_start();
        let (_, target) = rest.split_once('(')?;
        let target = target.strip_suffix(')')?.trim();
        (!target.is_empty()).then(|| std::path::PathBuf::from(target))
    })
}

fn omp_install_failed(full_selection: bool, detail: &str) -> Result<()> {
    if full_selection {
        println!(
            "Skipped omp: {detail}. Once omp is installed, select it in the settings pane or run configure with --agent omp."
        );
        return Ok(());
    }
    bail!("install Herdr omp integration: {detail}");
}

fn needs_omp_install(agents: &[Harness], status: &str) -> bool {
    agents.contains(&Harness::Omp) && is_missing(status, "omp")
}

fn read_status() -> Option<String> {
    let executable = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let output = Command::new(executable)
        .args(["integration", "status"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Herdr prints one `<id>: <state> (<path>)` line per integration. Only an
/// explicit "not installed" is actionable; an unknown id or a reworded state
/// stays quiet rather than nagging about something that may be fine.
fn is_missing(status: &str, id: &str) -> bool {
    status.lines().any(|line| {
        line.trim()
            .strip_prefix(id)
            .and_then(|rest| rest.strip_prefix(':'))
            .is_some_and(|state| state.trim_start().starts_with("not installed"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "\
claude: current (v7) (/home/u/.claude/hooks/herdr-agent-state.sh)
codex: current (v7) (/home/u/.codex/herdr-agent-state.sh)
opencode: not installed (/home/u/.config/opencode/plugins/herdr-agent-state.js)
omp: not installed (/home/u/.omp/agent/extensions/herdr-agent-state.ts)
grok: outdated (v0) (/home/u/.grok/hooks/herdr-agent-state.sh)
";

    #[test]
    fn only_an_explicit_not_installed_line_is_reported() {
        assert!(is_missing(STATUS, "opencode"));
        assert!(is_missing(STATUS, "omp"));
        assert!(!is_missing(STATUS, "claude"));
        assert!(!is_missing(STATUS, "codex"));
        assert!(!is_missing(STATUS, "grok"));
        assert!(!is_missing(STATUS, "kimi"));
        assert!(!is_missing("", "opencode"));
    }

    #[test]
    fn a_prefix_match_is_not_a_hit() {
        // "open" must not match the "opencode:" line.
        assert!(!is_missing(STATUS, "open"));
    }

    #[test]
    fn session_backed_harnesses_report_their_integration_id() {
        assert_eq!(integration_id(Harness::Agy), None);
        assert_eq!(integration_id(Harness::Muse), None);
        assert_eq!(integration_id(Harness::Cursor), Some("cursor"));
        assert_eq!(integration_id(Harness::OpenCode), Some("opencode"));
        assert_eq!(integration_id(Harness::Pi), Some("pi"));
        assert_eq!(integration_id(Harness::Omp), Some("omp"));
        assert_eq!(integration_id(Harness::Devin), Some("devin"));
    }

    /// The marketplace `configure` action runs a fixed command line, so it
    /// always selects every supported agent. A machine without omp must still
    /// get its other collectors instead of a hard failure.
    #[test]
    fn a_failed_omp_install_only_aborts_an_explicit_omp_selection() {
        let detail =
            "omp extension directory not found at /home/u/.omp/agent/extensions. install omp first";
        assert!(omp_install_failed(true, detail).is_ok());
        let explicit =
            omp_install_failed(false, detail).expect_err("explicit omp must fail loudly");
        assert!(explicit.to_string().contains(detail), "{explicit}");
    }

    #[test]
    fn only_a_selected_and_explicitly_missing_omp_is_auto_installed() {
        assert!(needs_omp_install(&[Harness::Omp], STATUS));
        assert!(!needs_omp_install(&[Harness::Pi], STATUS));
        assert!(!needs_omp_install(
            &[Harness::Omp],
            "omp: current (v8) (/home/u/.omp/agent/extensions/herdr-agent-state.ts)"
        ));
    }

    #[test]
    fn the_extension_target_is_read_from_the_status_line() {
        assert_eq!(
            integration_target(STATUS, "omp").as_deref(),
            Some(std::path::Path::new(
                "/home/u/.omp/agent/extensions/herdr-agent-state.ts"
            ))
        );
        assert_eq!(integration_target(STATUS, "kimi"), None);
        assert_eq!(integration_target("omp: not installed\n", "omp"), None);
        assert_eq!(integration_target("omp: not installed ()", "omp"), None);
    }

    /// Herdr would write the extension into the agent's own directory, so that
    /// directory existing is what separates "omp is not installed yet" — the
    /// ordinary skip — from a machine where the integration needs repairing.
    #[test]
    fn the_agent_directory_decides_whether_startup_repairs_omp() {
        let directory = tempfile::tempdir().unwrap();
        let agent = directory.path().join(".omp/agent");
        let status = format!(
            "omp: not installed ({}/extensions/herdr-agent-state.ts)\n",
            agent.display()
        );
        assert!(!agent_dir_present(&status, "omp"));
        std::fs::create_dir_all(&agent).unwrap();
        assert!(agent_dir_present(&status, "omp"));
        assert!(!agent_dir_present("omp: current (v8)\n", "omp"));
    }
}
