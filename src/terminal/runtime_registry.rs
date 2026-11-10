use super::{TerminalId, TerminalRuntime};
use crate::events::AppEvent;
use std::{collections::HashMap, sync::Arc};

/// Runtime ownership and hc update subscriptions stay outside pure AppState.
#[derive(Default)]
pub(crate) struct TerminalRuntimeRegistry {
    runtimes: HashMap<TerminalId, TerminalRuntime>,
    events: Option<tokio::sync::mpsc::Sender<AppEvent>>,
    watchers: HashMap<TerminalId, (Arc<()>, tokio::task::AbortHandle)>,
}

impl TerminalRuntimeRegistry {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn with_events(events: tokio::sync::mpsc::Sender<AppEvent>) -> Self {
        Self {
            runtimes: HashMap::new(),
            events: Some(events),
            watchers: HashMap::new(),
        }
    }
    pub(crate) fn get(&self, terminal_id: &TerminalId) -> Option<&TerminalRuntime> {
        self.runtimes.get(terminal_id)
    }

    pub(crate) fn insert(
        &mut self,
        terminal_id: TerminalId,
        runtime: TerminalRuntime,
    ) -> Option<TerminalRuntime> {
        self.stop_watcher(&terminal_id);
        if let (Some(events), Some(executor), Some(mut updates)) = (
            self.events.clone(),
            runtime.hc_executor(),
            runtime.hc_updates(),
        ) {
            let token = Arc::new(());
            let event_token = token.clone();
            let id = terminal_id.clone();
            let task = executor.spawn(async move {
                loop {
                    let snapshot = updates.borrow_and_update().clone();
                    if events
                        .send(AppEvent::HcTerminalUpdated {
                            terminal_id: id.clone(),
                            runtime_token: event_token.clone(),
                            snapshot,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if updates.changed().await.is_err() {
                        break;
                    }
                }
            });
            self.watchers
                .insert(terminal_id.clone(), (token, task.abort_handle()));
        }
        runtime.hc_commit_events(true);
        self.runtimes.insert(terminal_id, runtime)
    }

    pub(crate) fn hc_token(&self, id: &TerminalId) -> Option<Arc<()>> {
        self.watchers.get(id).map(|(token, _)| token.clone())
    }

    pub(crate) fn matches_hc_update(&self, id: &TerminalId, token: &Arc<()>) -> bool {
        self.watchers
            .get(id)
            .is_some_and(|(current, _)| Arc::ptr_eq(current, token))
    }

    pub(crate) fn extend(&mut self, runtimes: HashMap<TerminalId, TerminalRuntime>) {
        for (id, runtime) in runtimes {
            self.insert(id, runtime);
        }
    }

    pub(crate) fn stop_watcher(&mut self, id: &TerminalId) {
        if let Some(runtime) = self.runtimes.get(id) {
            runtime.hc_commit_events(false);
        }
        if let Some((_, task)) = self.watchers.remove(id) {
            task.abort();
        }
    }
    fn stop_watchers(&mut self) {
        for runtime in self.runtimes.values() {
            runtime.hc_commit_events(false);
        }
        for (_, (_, task)) in self.watchers.drain() {
            task.abort();
        }
    }

    pub(crate) fn remove(&mut self, terminal_id: &TerminalId) -> Option<TerminalRuntime> {
        self.stop_watcher(terminal_id);
        self.runtimes.remove(terminal_id)
    }
    pub(crate) fn values(&self) -> impl Iterator<Item = &TerminalRuntime> {
        self.runtimes.values()
    }
    #[cfg(unix)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&TerminalId, &TerminalRuntime)> {
        self.runtimes.iter()
    }
    #[cfg(unix)]
    pub(crate) fn set_handoff_readers_paused(&self, paused: bool) {
        for runtime in self.runtimes.values() {
            runtime.set_handoff_reader_paused(paused);
        }
    }
    #[cfg(unix)]
    pub(crate) fn assume_handoff_ownership(&mut self) {
        for runtime in self.runtimes.values_mut() {
            runtime.assume_handoff_ownership();
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.runtimes.len()
    }
    #[cfg(unix)]
    pub(crate) fn nudge_child_redraw_after_handoff(&self) {
        for runtime in self.runtimes.values() {
            runtime.nudge_child_redraw_after_handoff();
        }
    }
    #[cfg(unix)]
    pub(crate) fn drain_for_handoff(
        &mut self,
    ) -> impl Iterator<Item = (TerminalId, TerminalRuntime)> + '_ {
        self.stop_watchers();
        self.runtimes.drain()
    }
    #[cfg(test)]
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = (TerminalId, TerminalRuntime)> + '_ {
        self.stop_watchers();
        self.runtimes.drain()
    }
}
impl Drop for TerminalRuntimeRegistry {
    fn drop(&mut self) {
        self.stop_watchers();
    }
}
impl From<HashMap<TerminalId, TerminalRuntime>> for TerminalRuntimeRegistry {
    fn from(runtimes: HashMap<TerminalId, TerminalRuntime>) -> Self {
        Self {
            runtimes,
            events: None,
            watchers: HashMap::new(),
        }
    }
}
