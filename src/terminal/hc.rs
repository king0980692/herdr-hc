//! Pure projection of hc-owned facts; no hooks, process inspection, or UI inference.
use super::{EffectivePresentation, EffectiveStateChange, TerminalState};
use crate::{detect::AgentState, hc::state::RuntimeSnapshot};
use serde_json::{json, Value};

impl TerminalState {
    pub(crate) fn with_hc_binding(mut self, binding: Option<Value>) -> Self {
        self.claim_hc_authority();
        self.hc_binding = binding;
        self.pending_agent_resume_plan = None;
        self.respawn_shell_on_exit = false;
        self
    }

    pub(crate) fn with_runtime(mut self, runtime: &super::TerminalRuntime) -> Self {
        if let Some(snapshot) = runtime.hc_snapshot() {
            self.apply_hc_snapshot(snapshot);
        }
        self
    }

    pub(crate) fn apply_hc_snapshot(
        &mut self,
        snapshot: RuntimeSnapshot,
    ) -> Option<EffectiveStateChange> {
        let previous_agent_label = self.effective_agent_label().map(str::to_owned);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        let previous_presentation = self.effective_presentation();
        self.claim_hc_authority();
        self.hc_binding =
            Some(json!({"core_id":snapshot.binding.core_id,"address":snapshot.binding.address}));
        self.hook_authority = None;
        self.detected_agent = None;
        self.pending_agent_resume_plan = None;
        self.respawn_shell_on_exit = false;
        if let Some(cwd) = snapshot
            .terminal
            .facts
            .get("foreground")
            .and_then(|v| v.get("cwd"))
            .and_then(Value::as_str)
            .or_else(|| snapshot.terminal.facts.get("cwd").and_then(Value::as_str))
        {
            self.cwd = cwd.into();
        }
        self.terminal_title = snapshot
            .terminal
            .facts
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        self.restore_error = if snapshot.connected && snapshot.attached {
            None
        } else {
            Some(snapshot.error.clone().unwrap_or_else(|| {
                "hc terminal attachment is unavailable; program was not restarted".into()
            }))
        };
        self.hc_snapshot = Some(snapshot);
        self.state = self.hc_agent_state();
        let agent_label = self.effective_agent_label().map(str::to_owned);
        let presentation = self.effective_presentation();
        (previous_agent_label != agent_label
            || previous_state != self.state
            || previous_presentation != presentation)
            .then(|| EffectiveStateChange {
                previous_agent_label,
                previous_known_agent,
                previous_state,
                previous_presentation,
                agent_label,
                known_agent: self.effective_known_agent(),
                state: self.state,
                presentation,
            })
    }

    pub(crate) fn hc_status(&self) -> Option<&Value> {
        self.hc_snapshot.as_ref()?.reported_status()
    }

    pub(crate) fn hc_agent_state(&self) -> AgentState {
        match self
            .hc_status()
            .and_then(|status| status.get("state"))
            .and_then(Value::as_str)
        {
            Some("idle" | "done") => AgentState::Idle,
            Some("working") => AgentState::Working,
            Some("blocked" | "error") => AgentState::Blocked,
            _ => AgentState::Unknown,
        }
    }

    pub(super) fn hc_presentation(&self) -> EffectivePresentation {
        let status = self.hc_status();
        let mut state_labels = std::collections::HashMap::new();
        if let Some(raw) = status
            .and_then(|status| status.get("state"))
            .and_then(Value::as_str)
        {
            let key = match self.hc_agent_state() {
                AgentState::Idle => "idle",
                AgentState::Working => "working",
                AgentState::Blocked => "blocked",
                AgentState::Unknown => "unknown",
            };
            // Canonical states keep client-configured labels; preserve hc-only states verbatim.
            if key != raw {
                state_labels.insert(key.into(), raw.into());
            }
        }
        EffectivePresentation {
            title: status
                .and_then(|s| s.get("title"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            display_agent: status
                .and_then(|s| s.get("app"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            state_labels,
        }
    }
}
