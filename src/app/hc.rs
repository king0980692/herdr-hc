//! hc source updates are fenced runtime events, not inferred pane exits or hooks.
mod bootstrap;
mod reconnect;
use super::{actions::PaneStateUpdate, App};
use crate::{events::AppEvent, hc::state::RuntimeSnapshot, terminal::TerminalId};
pub(crate) use reconnect::Recovery;
use std::sync::Arc;

impl App {
    pub(crate) fn saved_source_worktree_rejection(
        &self,
        request: &crate::api::schema::Request,
    ) -> Option<String> {
        use crate::api::schema::Method;
        (self.terminal_factory.is_saved_source() && matches!(request.method,Method::WorktreeList(_)|Method::WorktreeOpen(_)|Method::WorktreeCreate(_)|Method::WorktreeRemove(_))).then(||super::api::responses::encode_error(request.id.clone(),"unsupported_runtime","hc source filesystem operations are unavailable; no local worktree fallback was used"))
    }
    pub(crate) fn unwrap_runtime_request(
        &self,
        request: crate::api::schema::Request,
    ) -> Result<crate::api::schema::Request, String> {
        let crate::api::schema::Method::RuntimeRequest(params) = request.method else {
            return Ok(request);
        };
        if self
            .terminal_factory
            .source_state()
            .is_none_or(|s| s.host_instance != params.host_instance || s.retired)
        {
            return Err(super::api::responses::encode_error(
                request.id,
                "stale_runtime_source",
                "source host changed; request was not forwarded",
            ));
        }
        if matches!(
            params.request.method,
            crate::api::schema::Method::RuntimeRequest(_)
        ) {
            return Err(super::api::responses::encode_error(
                request.id,
                "invalid_request",
                "nested runtime requests are not supported",
            ));
        }
        let mut inner = *params.request;
        inner.id = request.id;
        Ok(inner)
    }

    pub(super) fn terminate_hc_terminals(
        &mut self,
        ids: impl IntoIterator<Item = TerminalId>,
    ) -> std::io::Result<()> {
        let ids: std::collections::HashSet<_> = ids.into_iter().collect();
        for id in ids {
            let Some(terminal) = self
                .state
                .terminals
                .get(&id)
                .filter(|terminal| terminal.hc_authority)
            else {
                continue;
            };
            // An unbound legacy layout has no hc program we are allowed to terminate.
            let Some(binding) = terminal.hc_binding.clone() else {
                continue;
            };
            let binding: crate::hc::state::TerminalBinding =
                serde_json::from_value(binding).map_err(std::io::Error::other)?;
            self.cancel_hc_recovery(&id);
            if let Err(error) = self
                .terminal_factory
                .terminate(&binding, self.terminal_runtimes.get(&id))
            {
                self.queue_hc_recovery(id.clone());
                return Err(error);
            }
            if let Some(runtime) = self.terminal_runtimes.remove(&id) {
                if let (Some(terminal), Some(mut snapshot)) =
                    (self.state.terminals.get_mut(&id), runtime.hc_snapshot())
                {
                    snapshot.attached = false;
                    snapshot.error = Some("hc program stopped for explicit close".into());
                    terminal.apply_hc_snapshot(snapshot);
                }
                drop(runtime);
            }
        }
        Ok(())
    }

    pub(super) fn terminate_hc_workspaces(&mut self, indices: &[usize]) -> std::io::Result<()> {
        let ids: Vec<_> = indices
            .iter()
            .flat_map(|ws| {
                self.state
                    .pane_ids_for_workspace(*ws)
                    .into_iter()
                    .filter_map(|pane| self.state.terminal_id_for_pane(*ws, pane))
            })
            .collect();
        self.terminate_hc_terminals(ids)
    }

    /// Where this program's pane sits in herdr, so new programs beside it carry
    /// the same herdr.app placement (see `hc_entry::Placements`).
    fn learn_hc_placement(&self, terminal_id: &TerminalId, labels: &serde_json::Value) {
        for workspace in &self.state.workspaces {
            for tab in &workspace.tabs {
                if tab
                    .panes
                    .values()
                    .any(|pane| &pane.attached_terminal_id == terminal_id)
                {
                    self.terminal_factory.learn_placement(
                        &workspace.id,
                        &crate::workspace::public_tab_id_for_number(&workspace.id, tab.number),
                        labels,
                    );
                    return;
                }
            }
        }
    }

    pub(crate) fn apply_hc_update(
        &mut self,
        terminal_id: TerminalId,
        token: Arc<()>,
        snapshot: RuntimeSnapshot,
    ) -> (bool, Vec<PaneStateUpdate>) {
        if !self
            .terminal_runtimes
            .matches_hc_update(&terminal_id, &token)
        {
            return (false, Vec::new());
        }
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return (false, Vec::new());
        };
        if !terminal.hc_binding.as_ref().is_some_and(|binding| {
            binding["core_id"].as_str() == Some(&snapshot.binding.core_id)
                && binding["address"].as_str() == Some(&snapshot.binding.address)
        }) {
            return (false, Vec::new());
        }
        if crate::hc_entry::ENABLED {
            if let Some(labels) = snapshot.terminal.facts.get("labels") {
                self.learn_hc_placement(&terminal_id, labels);
            }
        }
        let finished = snapshot.is_nonkept_exit()
            && terminal.hc_snapshot.as_ref().is_some_and(|old| {
                old.terminal
                    .facts
                    .get("state")
                    .and_then(serde_json::Value::as_str)
                    == Some("live")
            });
        if finished {
            let mut panes: Vec<_> = self
                .state
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .flat_map(|tab| &tab.panes)
                .filter(|(_, pane)| pane.attached_terminal_id == terminal_id)
                .map(|(id, _)| *id)
                .collect();
            if let Some(popup) = self
                .state
                .popup_pane
                .as_ref()
                .filter(|popup| popup.terminal_id == terminal_id)
            {
                panes.push(popup.pane_id);
            }
            if !panes.is_empty() {
                self.cancel_hc_recovery(&terminal_id);
                let mut updates = Vec::new();
                for pane in panes {
                    updates.extend(self.handle_confirmed_hc_pane_exit(pane));
                }
                self.render_dirty.request_generic();
                self.render_notify.notify_one();
                return (true, updates);
            }
        }
        let changed = terminal.hc_snapshot.as_ref().is_none_or(|old| {
            old.connected != snapshot.connected
                || old.attached != snapshot.attached
                || old.error != snapshot.error
                || ["status", "reported", "title", "cwd", "state"]
                    .iter()
                    .any(|key| old.terminal.facts.get(*key) != snapshot.terminal.facts.get(*key))
                || old
                    .terminal
                    .facts
                    .get("foreground")
                    .and_then(|f| f.get("cwd"))
                    != snapshot
                        .terminal
                        .facts
                        .get("foreground")
                        .and_then(|f| f.get("cwd"))
        });
        let previous_toast = self.state.toast.clone();
        let updates = self.state.handle_app_event(AppEvent::HcTerminalUpdated {
            terminal_id: terminal_id.clone(),
            runtime_token: token,
            snapshot,
        });
        for update in &updates {
            self.refresh_new_herdr_toast_context_for_update(update, &previous_toast);
            self.emit_pane_state_update(update);
        }
        if changed {
            if updates.is_empty() {
                if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                    terminal.revision = terminal.revision.saturating_add(1);
                }
                let panes: Vec<_> = self
                    .state
                    .workspaces
                    .iter()
                    .enumerate()
                    .flat_map(|(ws, workspace)| {
                        workspace.tabs.iter().flat_map(move |tab| {
                            tab.panes
                                .iter()
                                .map(move |(pane, state)| (ws, *pane, &state.attached_terminal_id))
                        })
                    })
                    .filter(|(_, _, id)| **id == terminal_id)
                    .map(|(ws, pane, _)| (ws, pane))
                    .collect();
                for (ws, pane) in panes {
                    self.emit_pane_updated(ws, pane);
                }
            }
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
        self.sync_toast_deadline(previous_toast);
        if self
            .state
            .terminals
            .get(&terminal_id)
            .and_then(|t| t.hc_snapshot.as_ref())
            .is_some_and(|s| !s.connected || !s.attached)
        {
            self.queue_hc_recovery(terminal_id);
        }
        (changed, updates)
    }
}
