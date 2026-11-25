//! Pure projection of hc-owned facts; no hooks, process inspection, or UI inference.
use super::{EffectivePresentation, EffectiveStateChange, TerminalState};
use crate::{detect::AgentState, hc::state::RuntimeSnapshot};
use serde_json::{json, Value};

/// The hc label holding a pane's shared metadata tokens (JSON object).
pub(crate) const SHARED_TOKENS_LABEL: &str = "herdr-native.tokens";
/// hc's agent pin, as Herdr.app writes it (`"1"`): the authority on whether
/// an hc pane's agent is pinned.
pub(crate) const PINNED_LABEL: &str = "herdr.pinned";
/// The last turn someone looked at, kept by every app (herdr.app included).
pub(crate) const SEEN_LABEL: &str = "herdr.seen";
/// When this app pinned it (Unix ms), for the pin order.
pub(crate) const PINNED_AT_LABEL: &str = "herdr-native.pinned_at";
/// The token a pane's agent pin is shown to clients as (its value the pin
/// time, empty when unknown).
pub(crate) const PIN_TOKEN: &str = "herdr-pin";

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

    /// Tokens another viewer of this hc terminal shared (see
    /// `app::hc::shared_tokens`); this viewer's own reports take precedence.
    pub(crate) fn shared_tokens(&self) -> std::collections::HashMap<String, String> {
        self.hc_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.terminal.facts.get("labels"))
            .and_then(|labels| labels.get(SHARED_TOKENS_LABEL))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .and_then(|value| serde_json::from_str(value).ok())
            .unwrap_or_default()
    }

    /// A terminal herdr-native runs for itself (a plugin's pane): shown by
    /// this viewer only, never part of the shared layout.
    pub(crate) fn is_hc_hidden(&self) -> bool {
        self.hc_snapshot
            .as_ref()
            .is_some_and(|snapshot| crate::hc_entry::is_hidden(&snapshot.terminal.facts))
    }

    /// An hc program this viewer has lost touch with (its core or the
    /// connection to it is gone): what it shows is its last screen.
    pub(crate) fn hc_disconnected(&self) -> bool {
        self.hc_authority
            && self
                .hc_snapshot
                .as_ref()
                .is_some_and(|snapshot| !snapshot.connected || !snapshot.attached)
    }

    /// An hc pane's pin, from the core's labels: `None` off hc, else whether
    /// it is pinned and when (empty when another app pinned it).
    pub(crate) fn hc_pin(&self) -> Option<Option<String>> {
        let labels = self.hc_snapshot.as_ref()?.terminal.facts.get("labels");
        let label = |key: &str| {
            labels
                .and_then(|labels| labels.get(key))
                .and_then(Value::as_str)
        };
        Some(
            (label(PINNED_LABEL) == Some("1"))
                .then(|| label(PINNED_AT_LABEL).unwrap_or_default().to_owned()),
        )
    }

    /// An hc program's turns: how many it has finished, and the last one
    /// someone looked at (`herdr.seen`, kept by every app that shows it).
    pub(crate) fn hc_turns(&self) -> Option<(u64, u64)> {
        let facts = &self.hc_snapshot.as_ref()?.terminal.facts;
        let turn = facts.get("turn").and_then(Value::as_u64)?;
        let seen = facts
            .get("labels")
            .and_then(|labels| labels.get(SEEN_LABEL))
            .and_then(Value::as_str)
            .and_then(|seen| seen.parse().ok())
            .unwrap_or(0);
        Some((turn, seen))
    }

    /// Done with a turn nobody has looked at yet, as herdr.app shows it:
    /// hc says done and its turn is past `herdr.seen` (a core reporting no
    /// turn: done as it says).
    pub(crate) fn hc_unseen_done(&self) -> bool {
        self.hc_status()
            .and_then(|status| status.get("state"))
            .and_then(Value::as_str)
            == Some("done")
            && self.hc_turns().is_none_or(|(turn, seen)| turn > seen)
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
