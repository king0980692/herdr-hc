//! Programs another viewer (or a shell) starts on this viewer's core after it
//! opened: each becomes a tab here, attached only, never launched.
use super::*;
use crate::hc::state::{Terminal, TerminalBinding};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// The live programs among `terminals` this viewer should add: owner-local,
/// not hosting the launching client, and not already shown by any pane.
pub(super) fn programs_to_add(
    core: &str,
    terminals: Vec<Terminal>,
    shown: &HashSet<(String, String)>,
    excluded_pids: &HashSet<u32>,
) -> Vec<(TerminalBinding, PathBuf)> {
    terminals
        .into_iter()
        .filter(|terminal| !crate::hc_entry::is_hidden(&terminal.facts))
        .filter(|terminal| {
            terminal
                .facts
                .get("state")
                .and_then(serde_json::Value::as_str)
                == Some("live")
        })
        .filter(|terminal| {
            !terminal
                .facts
                .get("pid")
                .and_then(serde_json::Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok())
                .is_some_and(|pid| excluded_pids.contains(&pid))
        })
        .filter_map(|terminal| {
            let binding = TerminalBinding {
                core_id: core.to_owned(),
                address: terminal.address,
            };
            (!binding.address.contains('/')
                && binding.validate().is_ok()
                && !shown.contains(&(binding.core_id.clone(), binding.address.clone())))
            .then(|| {
                let cwd = terminal
                    .facts
                    .get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                (binding, PathBuf::from(cwd))
            })
        })
        .collect()
}

/// The workspace a new program joins: the one opened in its folder, else
/// the active one.
pub(super) fn workspace_for(
    identities: impl Iterator<Item = PathBuf>,
    cwd: &Path,
    active: Option<usize>,
) -> Option<usize> {
    let mut identities = identities.enumerate();
    identities
        .find(|(_, identity)| !cwd.as_os_str().is_empty() && identity == cwd)
        .map(|(index, _)| index)
        .or(active)
}

impl App {
    pub(crate) fn add_appeared_hc_programs(
        &mut self,
        generation: u64,
        core: String,
        terminals: Vec<Terminal>,
        deferred: bool,
    ) {
        // With a shared layout, the viewer that started it places it there;
        // only a program still unplaced after a while (a shell's) is added.
        if !deferred && self.terminal_factory.shares_herdr_app_layout() {
            self.terminal_factory.send_later(
                AppEvent::HcProgramsAppeared {
                    generation,
                    core_id: core,
                    terminals,
                    deferred: true,
                },
                std::time::Duration::from_secs(2),
                self.event_tx.clone(),
            );
            return;
        }
        let Some(source) = self.terminal_factory.source_state() else {
            return;
        };
        if source.generation != generation
            || source
                .client
                .as_ref()
                .is_none_or(|client| client.hello.core.id != core)
        {
            return;
        }
        // Before the first view exists, first-open discovery shows them.
        if self.state.workspaces.is_empty() || self.hc_recovery.initial_inventory_due.is_some() {
            return;
        }
        let shown: HashSet<(String, String)> = self
            .state
            .terminals
            .values()
            .filter_map(|terminal| {
                let binding = terminal.hc_binding.as_ref()?;
                Some((
                    binding["core_id"].as_str()?.to_owned(),
                    binding["address"].as_str()?.to_owned(),
                ))
            })
            .collect();
        let programs = programs_to_add(
            &core,
            terminals,
            &shown,
            &crate::hc_entry::excluded_program_pids(),
        );
        for (binding, cwd) in programs {
            let Some(ws_idx) = workspace_for(
                self.state
                    .workspaces
                    .iter()
                    .map(|workspace| workspace.identity_cwd.clone()),
                &cwd,
                self.state.active,
            ) else {
                return;
            };
            let (rows, cols) = self.state.new_pane_size(crate::ui::NewPanePlacement::Alone);
            let scrollback = self.state.pane_scrollback_limit_bytes;
            let theme = self.state.host_terminal_theme;
            let appearance = self.pane_host_terminal_appearance();
            let Some(workspace) = self.state.workspaces.get_mut(ws_idx) else {
                return;
            };
            match workspace.create_tab_attached(
                rows,
                cols,
                cwd,
                scrollback,
                theme,
                appearance,
                &binding,
                &self.terminal_factory,
            ) {
                Ok((tab_idx, terminal, runtime)) => {
                    self.terminal_runtimes.insert(terminal.id.clone(), runtime);
                    self.state.terminals.insert(terminal.id.clone(), terminal);
                    self.state.remove_alias_shadowed_by_new_pane(
                        self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
                    );
                    self.schedule_session_save();
                    self.emit_tab_created_events(ws_idx, tab_idx);
                    tracing::info!(address = %binding.address, "showing a program another viewer started");
                }
                Err(error) => {
                    tracing::warn!(%error, address = %binding.address, "a new program on the core could not be shown");
                }
            }
        }
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal(address: &str, state: &str, pid: u64, cwd: &str) -> Terminal {
        serde_json::from_value(serde_json::json!({
            "address": address, "state": state, "pid": pid, "cwd": cwd
        }))
        .unwrap()
    }

    #[test]
    fn only_new_live_local_programs_are_added() {
        let shown = HashSet::from([("core".to_owned(), "t1".to_owned())]);
        let added = programs_to_add(
            "core",
            vec![
                terminal("t1", "live", 10, "/a"),
                terminal("t2", "live", 20, "/b"),
                terminal("t3", "finished", 30, "/c"),
                terminal("gx10/t4", "live", 40, "/d"),
                terminal("t5", "live", 50, "/e"),
                terminal("t06", "live", 60, "/f"),
                serde_json::from_value(serde_json::json!({
                    "address": "t7", "state": "live", "pid": 70, "cwd": "/g",
                    "labels": {"herdr-native.hidden": "1"}
                }))
                .unwrap(),
            ],
            &shown,
            &HashSet::from([50]),
        );
        let addresses: Vec<_> = added
            .iter()
            .map(|(binding, cwd)| (binding.address.as_str(), cwd.clone()))
            .collect();
        assert_eq!(addresses, [("t2", PathBuf::from("/b"))]);
        assert_eq!(added[0].0.core_id, "core");
    }

    #[test]
    fn a_new_program_joins_the_workspace_opened_in_its_folder_else_the_active_one() {
        let identities = || [PathBuf::from("/a"), PathBuf::from("/b")].into_iter();
        assert_eq!(
            workspace_for(identities(), Path::new("/b"), Some(0)),
            Some(1)
        );
        assert_eq!(
            workspace_for(identities(), Path::new("/z"), Some(0)),
            Some(0)
        );
        assert_eq!(workspace_for(identities(), Path::new(""), Some(1)), Some(1));
        assert_eq!(workspace_for(identities(), Path::new("/z"), None), None);
    }
}
