//! Pinned agents: listed first in the agents panel, in the order they were
//! pinned. The server sets the pin (`pane.pin`: on hc panes hc's own
//! `herdr.pinned`, shared with Herdr.app) and shows it as the pane's
//! `herdr-pin` token, its value the pin time.
use super::*;
use crate::protocol::ClientShellAgent;

pub(super) const PIN_TOKEN: &str = "herdr-pin";

/// When the agent was pinned (Unix ms), if it is; a pin of unknown time
/// (another app's) sorts after the timed ones.
pub(super) fn pinned_at(agent: &ClientShellAgent) -> Option<u64> {
    agent
        .tokens
        .iter()
        .find(|(key, _)| key == PIN_TOKEN)
        .map(|(_, value)| value.parse().unwrap_or(u64::MAX))
}

/// Pinned rows first, earliest pin on top; the others keep their order.
pub(super) fn pin_first<T>(rows: &mut [T], pinned: impl Fn(&T) -> Option<u64>) {
    rows.sort_by_key(|row| pinned(row).map_or((1, 0), |at| (0, at)));
}

impl ClientShellState {
    fn endpoint_agent(
        &self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
    ) -> Option<&ClientShellAgent> {
        let snapshot = if endpoint_id == &self.active_endpoint_id {
            self.snapshot.as_deref()
        } else {
            self.endpoints
                .iter()
                .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?
                .snapshot
                .as_deref()
        }?;
        snapshot
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id)
    }

    pub(super) fn open_agent_context_menu(
        &mut self,
        endpoint_id: ClientEndpointId,
        pane_id: String,
        x: u16,
        y: u16,
    ) {
        let Some(agent) = self.endpoint_agent(&endpoint_id, &pane_id) else {
            return;
        };
        let pinned = pinned_at(agent).is_some();
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Agent {
                endpoint_id,
                pane_id,
                pinned,
            },
            x,
            y,
            highlighted: 0,
        }));
    }

    /// Pins the agent, or unpins it, on the machine it runs on.
    pub(super) fn toggle_agent_pin(
        &mut self,
        endpoint_id: ClientEndpointId,
        pane_id: String,
        outcome: &mut ClientShellInput,
    ) {
        let pinned = self
            .endpoint_agent(&endpoint_id, &pane_id)
            .is_some_and(|agent| pinned_at(agent).is_some());
        let method = crate::api::schema::Method::PanePin(crate::api::schema::PanePinParams {
            pane_id,
            pinned: !pinned,
        });
        self.push_method_to_endpoint(endpoint_id, method, outcome);
    }

    /// The focused pane's agent, pinned or unpinned (the keyboard's way).
    pub(super) fn toggle_focused_agent_pin(&mut self, outcome: &mut ClientShellInput) {
        let Some(pane_id) = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.focused_pane_id.clone())
        else {
            return;
        };
        let endpoint_id = self.active_endpoint_id.clone();
        if self.endpoint_agent(&endpoint_id, &pane_id).is_none() {
            return;
        }
        self.toggle_agent_pin(endpoint_id, pane_id, outcome);
    }

    /// Sends `method` to the endpoint that owns its target, viewed or not:
    /// viewing does not change. Another endpoint's answer is not awaited.
    fn push_method_to_endpoint(
        &mut self,
        endpoint_id: ClientEndpointId,
        method: crate::api::schema::Method,
        outcome: &mut ClientShellInput,
    ) {
        if endpoint_id == self.active_endpoint_id {
            self.push_endpoint_method(method, outcome);
            return;
        }
        let Some(endpoint) = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        else {
            return;
        };
        let Some(boot_id) = endpoint
            .snapshot
            .as_deref()
            .map(|snapshot| snapshot.boot_id.clone())
        else {
            return;
        };
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        outcome.actions.push(ClientShellAction::Endpoint {
            endpoint_id,
            boot_id,
            request: Box::new(crate::api::schema::Request {
                id: format!("client-shell:{request_id}"),
                method,
            }),
        });
        outcome.repaint = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_rows_come_first_in_pin_order_and_the_rest_keep_theirs() {
        let mut rows = vec![
            ("a", None),
            ("b", Some(30)),
            ("c", None),
            ("d", Some(10)),
            ("e", None),
        ];
        pin_first(&mut rows, |row| row.1);
        assert_eq!(
            rows.iter().map(|row| row.0).collect::<Vec<_>>(),
            ["d", "b", "a", "c", "e"]
        );
    }
}
