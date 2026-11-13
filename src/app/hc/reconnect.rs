//! App-owned recovery: attach existing identities off the App thread, then conditionally commit.
use super::*;
use crate::{hc::state::TerminalBinding, terminal::TerminalRuntime};
use std::{collections::HashMap, io, sync::mpsc, time::Instant};

struct Pending {
    binding: TerminalBinding,
    token: Arc<()>,
    old_runtime: Option<Arc<()>>,
    due: Instant,
    attempts: u32,
    task: Option<tokio::task::AbortHandle>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
struct Recovered {
    id: TerminalId,
    binding: TerminalBinding,
    generation: u64,
    token: Arc<()>,
    result: io::Result<TerminalRuntime>,
}
pub(crate) struct Recovery {
    pub generation: u64,
    pub initial_inventory_due: Option<Instant>,
    pending: HashMap<TerminalId, Pending>,
    send: mpsc::Sender<Recovered>,
    receive: mpsc::Receiver<Recovered>,
}
impl Default for Recovery {
    fn default() -> Self {
        let (send, receive) = mpsc::channel();
        Self {
            generation: 0,
            initial_inventory_due: None,
            pending: HashMap::new(),
            send,
            receive,
        }
    }
}
impl Recovery {
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let pane_due = if self.pending.values().filter(|p| p.task.is_some()).count() >= 4 {
            None
        } else {
            self.pending
                .values()
                .filter(|p| p.task.is_none())
                .map(|p| p.due)
                .min()
        };
        pane_due.into_iter().chain(self.initial_inventory_due).min()
    }
}
impl App {
    fn hc_pane_for_terminal(&self, id: &TerminalId) -> Option<crate::layout::PaneId> {
        self.state
            .workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
            .find_map(|(pane, state)| (&state.attached_terminal_id == id).then_some(*pane))
            .or_else(|| {
                self.state
                    .popup_pane
                    .as_ref()
                    .filter(|p| &p.terminal_id == id)
                    .map(|p| p.pane_id)
            })
    }
    fn hc_recovery_binding(&self, id: &TerminalId) -> Option<TerminalBinding> {
        let terminal = self.state.terminals.get(id)?;
        let binding: TerminalBinding = serde_json::from_value(terminal.hc_binding.clone()?).ok()?;
        binding.validate().ok()?;
        Some(binding)
    }
    pub(crate) fn cancel_hc_recovery(&mut self, id: &TerminalId) {
        self.hc_recovery.pending.remove(id);
    }
    pub(crate) fn queue_hc_recovery(&mut self, id: TerminalId) {
        if self
            .state
            .terminals
            .get(&id)
            .and_then(|t| t.hc_snapshot.as_ref())
            .is_some_and(|s| s.connected && s.attached)
        {
            return;
        }
        let Some(binding) = self.hc_recovery_binding(&id) else {
            return;
        };
        let generation = match self.terminal_factory.attach_source(&binding) {
            Ok((generation, _, _)) => generation,
            Err(error) => {
                if let Some(terminal) = self.state.terminals.get_mut(&id) {
                    terminal.restore_error = Some(error.to_string());
                }
                return;
            }
        };
        if generation != self.hc_recovery.generation {
            return;
        }
        if self
            .state
            .terminals
            .get(&id)
            .and_then(|t| t.hc_snapshot.as_ref())
            .is_some_and(|s| s.terminal.facts["state"] == "finished")
        {
            return;
        }
        let old_runtime = self.terminal_runtimes.hc_token(&id);
        self.hc_recovery
            .pending
            .entry(id)
            .or_insert_with(|| Pending {
                binding,
                token: Arc::new(()),
                old_runtime,
                due: Instant::now(),
                attempts: 0,
                task: None,
            });
    }
    pub(crate) fn hc_source_changed(&mut self, generation: u64) -> bool {
        let Some(source) = self.terminal_factory.source_state() else {
            return false;
        };
        if source.generation != generation || generation <= self.hc_recovery.generation {
            return false;
        }
        if source.client.is_some() && self.hc_recovery.initial_inventory_due.is_some() {
            self.hc_recovery.initial_inventory_due = Some(Instant::now());
        }
        if source.client.is_some() {
            // Connected (again): the shared layout is synced once now.
            self.request_layout_sync();
        }
        let initial = self.hc_recovery.generation == 0;
        self.hc_recovery.generation = generation;
        self.hc_recovery.pending.clear();
        let ids: Vec<_> = self
            .state
            .terminals
            .iter()
            .filter(|(_, t)| t.hc_authority)
            .map(|(id, _)| id.clone())
            .collect();
        let mut changed = false;
        for id in ids {
            if initial
                && self
                    .terminal_runtimes
                    .get(&id)
                    .and_then(|r| r.hc_snapshot())
                    .is_some_and(|s| s.connected && s.attached)
            {
                continue;
            }
            self.terminal_runtimes.stop_watcher(&id);
            if let Some(terminal) = self.state.terminals.get_mut(&id) {
                if let Some(mut snapshot) = terminal.hc_snapshot.clone() {
                    snapshot.connected = false;
                    snapshot.attached = false;
                    snapshot.error = source.error.clone();
                    terminal.apply_hc_snapshot(snapshot);
                }
                if source.client.is_none() {
                    terminal.restore_error = Some(
                        source
                            .error
                            .clone()
                            .unwrap_or_else(|| "hc source is unavailable".into()),
                    );
                }
                changed = true;
            }
            if source.client.is_some() {
                self.queue_hc_recovery(id);
            }
        }
        // Retargeting must change presentation boot identity before any new source commands.
        if source.retired {
            self.state.should_quit = true;
        }
        if changed {
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
        changed
    }
    pub(crate) fn run_hc_recovery(&mut self, now: Instant) {
        self.bootstrap_existing_hc_terminals(now);
        let capacity = 4usize.saturating_sub(
            self.hc_recovery
                .pending
                .values()
                .filter(|p| p.task.is_some())
                .count(),
        );
        let ids: Vec<_> = self
            .hc_recovery
            .pending
            .iter()
            .filter(|(_, p)| p.task.is_none() && p.due <= now)
            .take(capacity)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let Some(pane) = self.hc_pane_for_terminal(&id) else {
                self.hc_recovery.pending.remove(&id);
                continue;
            };
            let binding = self.hc_recovery.pending[&id].binding.clone();
            if self.hc_recovery_binding(&id).as_ref() != Some(&binding) {
                self.hc_recovery.pending.remove(&id);
                continue;
            }
            let Ok((generation, client, executor)) = self.terminal_factory.attach_source(&binding)
            else {
                self.hc_recovery.pending.remove(&id);
                continue;
            };
            let token = self.hc_recovery.pending[&id].token.clone();
            let send = self.hc_recovery.send.clone();
            let events = self.event_tx.clone();
            let notify = self.render_notify.clone();
            let dirty = self.render_dirty.clone();
            let (rows, cols) = self
                .terminal_runtimes
                .get(&id)
                .map(TerminalRuntime::current_size)
                .unwrap_or_else(|| {
                    let placement = self
                        .state
                        .workspaces
                        .iter()
                        .position(|w| w.tabs.iter().any(|t| t.panes.contains_key(&pane)))
                        .map(|ws_idx| crate::ui::NewPanePlacement::Existing { ws_idx, pane })
                        .unwrap_or(crate::ui::NewPanePlacement::Alone);
                    self.state.new_pane_size(placement)
                });
            let scrollback = self.state.pane_scrollback_limit_bytes;
            let theme = self.state.host_terminal_theme;
            let appearance = self.pane_host_terminal_appearance();
            let result_id = id.clone();
            let task = executor.spawn(async move {
                let result = TerminalRuntime::attach_hc(
                    pane,
                    rows,
                    cols,
                    client,
                    &binding.address,
                    crate::pane::REATTACH_HISTORY_LINES,
                    scrollback,
                    theme,
                    appearance,
                    events.clone(),
                    notify,
                    dirty,
                    false,
                )
                .await;
                if send
                    .send(Recovered {
                        id: result_id,
                        binding,
                        generation,
                        token,
                        result,
                    })
                    .is_ok()
                {
                    let _ = events.send(AppEvent::HcRecoveryReady).await;
                }
            });
            if let Some(pending) = self.hc_recovery.pending.get_mut(&id) {
                pending.task = Some(task.abort_handle());
            }
        }
    }
    pub(crate) fn finish_hc_recovery(&mut self) -> bool {
        let mut changed = false;
        while let Ok(recovered) = self.hc_recovery.receive.try_recv() {
            let valid = self
                .hc_recovery
                .pending
                .get(&recovered.id)
                .is_some_and(|p| {
                    Arc::ptr_eq(&p.token, &recovered.token)
                        && p.binding == recovered.binding
                        && tokens_match(
                            &p.old_runtime,
                            &self.terminal_runtimes.hc_token(&recovered.id),
                        )
                })
                && self
                    .terminal_factory
                    .source_state()
                    .is_some_and(|s| s.generation == recovered.generation)
                && self.hc_recovery_binding(&recovered.id).as_ref() == Some(&recovered.binding)
                && self.hc_pane_for_terminal(&recovered.id).is_some();
            if !valid {
                if self
                    .hc_recovery
                    .pending
                    .get(&recovered.id)
                    .is_some_and(|p| Arc::ptr_eq(&p.token, &recovered.token))
                {
                    self.hc_recovery.pending.remove(&recovered.id);
                }
                continue;
            }
            match recovered.result {
                Ok(runtime) => {
                    if runtime
                        .hc_snapshot()
                        .is_none_or(|s| s.binding != recovered.binding)
                    {
                        self.hc_recovery.pending.remove(&recovered.id);
                        continue;
                    }
                    self.hc_recovery.pending.remove(&recovered.id);
                    if let Some(terminal) = self.state.terminals.get_mut(&recovered.id) {
                        if let Some(snapshot) = runtime.hc_snapshot() {
                            terminal.apply_hc_snapshot(snapshot);
                        }
                        terminal.restore_error = None;
                    }
                    let id = recovered.id;
                    drop(self.terminal_runtimes.insert(id.clone(), runtime));
                    let focused = self.state.outer_terminal_focus != Some(false)
                        && match self.state.popup_pane.as_ref() {
                            Some(popup) => popup.terminal_id == id,
                            None => self
                                .state
                                .active
                                .and_then(|ws| self.state.workspaces.get(ws))
                                .and_then(|ws| {
                                    ws.focused_pane_id().and_then(|pane| ws.terminal_id(pane))
                                })
                                .is_some_and(|focused| focused == &id),
                        };
                    if let Some(runtime) = self.terminal_runtimes.get(&id) {
                        runtime.try_send_focus_event(if focused {
                            crate::ghostty::FocusEvent::Gained
                        } else {
                            crate::ghostty::FocusEvent::Lost
                        });
                    }
                    changed = true;
                }
                Err(error) => {
                    if let Some(terminal) = self.state.terminals.get_mut(&recovered.id) {
                        let error = error.to_string();
                        changed |= terminal.restore_error.as_ref() != Some(&error);
                        terminal.restore_error = Some(error);
                    }
                    if let Some(pending) = self.hc_recovery.pending.get_mut(&recovered.id) {
                        pending.task = None;
                        pending.due =
                            Instant::now() + crate::hc::source::retry_delay(pending.attempts);
                        pending.attempts = pending.attempts.saturating_add(1);
                    }
                }
            }
        }
        if changed {
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
        changed
    }
}
fn tokens_match(a: &Option<Arc<()>>, b: &Option<Arc<()>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}
