//! First-open projection: existing owner-local programs become tabs, never launch requests.
use super::*;
use crate::{
    hc::state::{Terminal, TerminalBinding},
    persist::{LayoutSnapshot, PaneSnapshot, SessionSnapshot, TabSnapshot, WorkspaceSnapshot},
};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

fn initial_layout(
    core: &str,
    records: Vec<Terminal>,
    excluded_pids: &HashSet<u32>,
) -> io::Result<Option<SessionSnapshot>> {
    let mut addresses = HashSet::new();
    let mut tabs = Vec::new();
    let mut identity_cwd = PathBuf::new();
    for terminal in records {
        let state = terminal
            .facts
            .get("state")
            .and_then(serde_json::Value::as_str)
            .filter(|state| !state.is_empty())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "hc terminal discovery has incomplete lifecycle facts",
                )
            })?;
        if terminal.address.is_empty() || !addresses.insert(terminal.address.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "hc terminal discovery has duplicate or missing addresses",
            ));
        }
        // Aggregate remote records are not owned by this connection's hello core.
        if terminal.address.contains('/') {
            continue;
        }
        let binding = TerminalBinding {
            core_id: core.to_owned(),
            address: terminal.address,
        };
        binding.validate()?;
        if state == "finished" {
            continue;
        }
        if state != "live" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown hc lifecycle; initial layout was not created",
            ));
        }
        // The terminal a launching client runs in is never shown inside itself.
        if terminal
            .facts
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok())
            .is_some_and(|pid| excluded_pids.contains(&pid))
        {
            continue;
        }
        let pane = u32::try_from(tabs.len())
            .ok()
            .and_then(|index| index.checked_add(1))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "too many hc terminals for a layout",
                )
            })?;
        let cwd = PathBuf::from(
            terminal
                .facts
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
        );
        if tabs.is_empty() {
            identity_cwd = cwd.clone();
        }
        tabs.push(TabSnapshot {
            custom_name: None,
            layout: LayoutSnapshot::Pane(pane),
            panes: HashMap::from([(
                pane,
                PaneSnapshot {
                    cwd,
                    hc_binding: Some(serde_json::to_value(binding).map_err(io::Error::other)?),
                    label: None,
                    agent_name: None,
                    managed_agent_kind: None,
                    agent_session: None,
                    agent_resume: None,
                    launch_argv: None,
                    full_zoom: false,
                },
            )]),
            zoomed: false,
            focused: Some(pane),
            root_pane: Some(pane),
        });
    }
    if tabs.is_empty() {
        return Ok(None);
    }
    Ok(Some(SessionSnapshot {
        version: crate::persist::SNAPSHOT_VERSION,
        workspaces: vec![WorkspaceSnapshot {
            id: None,
            custom_name: None,
            identity_cwd,
            worktree_space: None,
            public_pane_numbers: HashMap::new(),
            next_public_pane_number: 0,
            public_tab_numbers: Vec::new(),
            next_public_tab_number: 0,
            tabs,
            active_tab: 0,
        }],
        active: Some(0),
        selected: 0,
        sidebar_width: None,
        sidebar_section_split: None,
        collapsed_space_keys: HashSet::new(),
    }))
}

impl App {
    pub(crate) fn refresh_initial_hc_inventory_on_focus(&mut self) {
        if self.hc_recovery.initial_inventory_due.is_some() {
            self.hc_recovery.initial_inventory_due = Some(Instant::now());
        }
    }

    pub(crate) fn bootstrap_existing_hc_terminals(&mut self, now: Instant) {
        if self
            .hc_recovery
            .initial_inventory_due
            .is_none_or(|due| due > now)
        {
            return;
        }
        if !self.state.workspaces.is_empty() {
            self.hc_recovery.initial_inventory_due = None;
            return;
        }
        let discovery = self
            .terminal_factory
            .initial_terminals()
            .and_then(|(core, terminals)| {
                initial_layout(&core, terminals, &crate::hc_entry::excluded_program_pids())
            });
        let snapshot = match discovery {
            Ok(snapshot) => {
                self.hc_recovery.initial_inventory_due = None;
                snapshot
            }
            Err(error) => {
                self.hc_recovery.initial_inventory_due = Some(now + Duration::from_secs(30));
                tracing::warn!(%error, "initial hc inventory unavailable; no programs were launched");
                return;
            }
        };
        let Some(snapshot) = snapshot else {
            if self.terminal_factory.is_saved_source() {
                self.hc_recovery.initial_inventory_due = Some(now + Duration::from_secs(30));
            }
            return;
        };
        let (workspaces, terminals, runtimes) = crate::persist::restore_with_factory(
            &snapshot,
            None,
            24,
            80,
            self.state.pane_scrollback_limit_bytes,
            &self.state.default_shell,
            self.state.shell_mode,
            false,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
            &self.terminal_factory,
        );
        self.state.workspaces = workspaces;
        self.state.terminals = terminals;
        self.terminal_runtimes.extend(runtimes);
        self.state.active = (!self.state.workspaces.is_empty()).then_some(0);
        self.state.selected = 0;
        self.state.mode = crate::app::state::Mode::Terminal;
        self.default_workspace_initialized = true;
        self.state.session_dirty = true;
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn terminal(address: &str, state: &str) -> Terminal {
        serde_json::from_value(serde_json::json!({"address":address,"state":state,"cwd":"/source"}))
            .unwrap()
    }
    #[test]
    fn shared_initial_layout_is_live_owner_local_tabs_with_canonical_bindings_only() {
        let snapshot = initial_layout(
            "owner",
            vec![
                terminal("t1", "live"),
                terminal("other/t1", "live"),
                terminal("t2", "finished"),
                terminal("t1b", "live"),
            ],
            &HashSet::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(snapshot.workspaces.len(), 1);
        let workspace = &snapshot.workspaces[0];
        assert_eq!(workspace.tabs.len(), 2);
        for (index, address) in ["t1", "t1b"].iter().enumerate() {
            let pane = workspace.tabs[index].panes.values().next().unwrap();
            assert_eq!(
                pane.hc_binding.as_ref().unwrap(),
                &serde_json::json!({"core_id":"owner","address":address})
            );
            assert!(pane.launch_argv.is_none() && pane.agent_resume.is_none());
        }
        assert_ne!(workspace.tabs[0].focused, workspace.tabs[1].focused);
        assert!(initial_layout("owner", vec![], &HashSet::new())
            .unwrap()
            .is_none());
        for records in [
            vec![terminal("t1", "live"), terminal("t1", "live")],
            vec![terminal("t01", "live")],
            vec![terminal("t1", "future")],
        ] {
            assert!(initial_layout("owner", records, &HashSet::new()).is_err());
        }
    }

    #[test]
    fn initial_layout_leaves_out_the_program_hosting_the_launching_client() {
        let with_pid = |address: &str, pid: u32| -> Terminal {
            serde_json::from_value(serde_json::json!({
                "address":address,"state":"live","cwd":"/source","pid":pid
            }))
            .unwrap()
        };
        let snapshot = initial_layout(
            "owner",
            vec![
                with_pid("t1", 100),
                with_pid("t2", 200),
                with_pid("t3", 300),
            ],
            &HashSet::from([200]),
        )
        .unwrap()
        .unwrap();
        let addresses: Vec<_> = snapshot.workspaces[0]
            .tabs
            .iter()
            .map(|tab| {
                tab.panes
                    .values()
                    .next()
                    .unwrap()
                    .hc_binding
                    .clone()
                    .unwrap()["address"]
                    .clone()
            })
            .collect();
        assert_eq!(addresses, ["t1", "t3"]);
        // Excluding the only program leaves nothing to project, not an error.
        assert!(
            initial_layout("owner", vec![with_pid("t2", 200)], &HashSet::from([200]))
                .unwrap()
                .is_none()
        );
    }
}
