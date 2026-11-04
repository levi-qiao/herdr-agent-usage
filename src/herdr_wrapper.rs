use crate::model::Harness;
use anyhow::Result;

pub use crate::herdr_base::*;
pub(crate) use crate::herdr_base::{plugin_quota_present, quota_rows_have_drifted};

/// Agy statusLine observations are keyed by the Herdr pane id, not by
/// Antigravity's conversation id. Herdr's own `agent_session` remains untouched
/// on the server for resume; only this plugin's local pane copy uses the pane
/// id so quota/model/context lookup cannot cross Agy panes.
fn bind_agy_quota_session(pane: &mut AgentPane) {
    if pane.harness != Harness::Agy {
        return;
    }
    pane.session = Some(AgentSession {
        kind: Some("id".to_string()),
        value: pane.pane_id.clone(),
    });
}

fn bind_agy_quota_sessions(panes: &mut [AgentPane]) {
    for pane in panes {
        bind_agy_quota_session(pane);
    }
}

pub fn list_agent_state() -> Result<AgentState> {
    let mut state = crate::herdr_base::list_agent_state()?;
    bind_agy_quota_sessions(&mut state.panes);
    Ok(state)
}

pub fn list_agent_panes() -> Result<Vec<AgentPane>> {
    let mut panes = crate::herdr_base::list_agent_panes()?;
    bind_agy_quota_sessions(&mut panes);
    Ok(panes)
}

pub fn find_agent_pane(pane_id: &str) -> Result<Option<AgentPane>> {
    let mut pane = crate::herdr_base::find_agent_pane(pane_id)?;
    if let Some(pane) = pane.as_mut() {
        bind_agy_quota_session(pane);
    }
    Ok(pane)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn pane(harness: Harness, pane_id: &str, session: Option<&str>) -> AgentPane {
        AgentPane {
            pane_id: pane_id.to_string(),
            workspace_id: "w1".to_string(),
            cwd: String::new(),
            title: String::new(),
            harness,
            session: session.map(|value| AgentSession {
                kind: Some("id".to_string()),
                value: value.to_string(),
            }),
            session_summary: String::new(),
            topic: String::new(),
            tokens: BTreeMap::new(),
            status: AgentStatus::Idle,
            focused: false,
            state_change_seq: 0,
        }
    }

    #[test]
    fn agy_uses_pane_id_only_for_plugin_snapshot_lookup() {
        let mut pane = pane(Harness::Agy, "w1:p7", Some("subagent-conversation"));
        bind_agy_quota_session(&mut pane);
        assert_eq!(
            pane.session.as_ref().and_then(AgentSession::id),
            Some("w1:p7")
        );
    }

    #[test]
    fn other_harness_sessions_are_unchanged() {
        for harness in [
            Harness::Claude,
            Harness::Codex,
            Harness::Grok,
            Harness::OpenCode,
            Harness::Pi,
            Harness::Omp,
            Harness::Devin,
            Harness::Muse,
            Harness::Cursor,
        ] {
            let mut pane = pane(harness, "w1:p7", Some("provider-session"));
            bind_agy_quota_session(&mut pane);
            assert_eq!(
                pane.session.as_ref().and_then(AgentSession::id),
                Some("provider-session"),
                "{harness:?} session changed"
            );
        }
    }
}
