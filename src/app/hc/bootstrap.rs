//! First-open projection: existing owner-local programs become tabs, never launch requests.
use super::*;
use crate::{
    hc::state::{Terminal, TerminalBinding},
    hc_entry::app_layout::{shown_spaces, ShownLayout},
    persist::{
        DirectionSnapshot, LayoutSnapshot, PaneSnapshot, SessionSnapshot, TabSnapshot,
        WorkspaceSnapshot,
    },
};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

/// A shown program's pane, by address.
struct Program {
    cwd: PathBuf,
    binding: serde_json::Value,
    space: Option<String>,
}

fn pane_snapshot(program: &Program) -> PaneSnapshot {
    PaneSnapshot {
        cwd: program.cwd.clone(),
        hc_binding: Some(program.binding.clone()),
        label: None,
        agent_name: None,
        managed_agent_kind: None,
        agent_session: None,
        agent_resume: None,
        launch_argv: None,
        full_zoom: false,
    }
}

fn too_many() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "too many hc terminals for a layout",
    )
}

/// One herdr-hc workspace being filled with tabs.
struct WorkspaceDraft {
    custom_name: Option<String>,
    identity_cwd: PathBuf,
    tabs: Vec<TabSnapshot>,
    last_pane: u32,
}

impl WorkspaceDraft {
    fn new(custom_name: Option<String>, identity_cwd: PathBuf) -> Self {
        Self {
            custom_name,
            identity_cwd,
            tabs: Vec::new(),
            last_pane: 0,
        }
    }

    fn pane(
        &mut self,
        program: &Program,
        panes: &mut HashMap<u32, PaneSnapshot>,
    ) -> io::Result<LayoutSnapshot> {
        self.last_pane = self.last_pane.checked_add(1).ok_or_else(too_many)?;
        if self.identity_cwd.as_os_str().is_empty() {
            self.identity_cwd = program.cwd.clone();
        }
        panes.insert(self.last_pane, pane_snapshot(program));
        Ok(LayoutSnapshot::Pane(self.last_pane))
    }

    /// herdr.app's rows and columns hold any number of shares; herdr splits
    /// in two, so the first part keeps its share of the whole and the rest
    /// split what is left the same way.
    fn layout(
        &mut self,
        shown: ShownLayout,
        programs: &HashMap<String, Program>,
        panes: &mut HashMap<u32, PaneSnapshot>,
    ) -> io::Result<Option<LayoutSnapshot>> {
        match shown {
            ShownLayout::Program(address) => match programs.get(&address) {
                Some(program) => self.pane(program, panes).map(Some),
                None => Ok(None),
            },
            ShownLayout::Split { row, parts } => {
                let mut built = Vec::new();
                for (share, part) in parts {
                    if let Some(layout) = self.layout(part, programs, panes)? {
                        built.push((share, layout));
                    }
                }
                let mut rest: Option<(f64, LayoutSnapshot)> = None;
                while let Some((share, first)) = built.pop() {
                    rest = Some(match rest {
                        None => (share, first),
                        Some((rest_share, second)) => {
                            let total = share + rest_share;
                            let ratio = (share / total).clamp(0.05, 0.95) as f32;
                            (
                                total,
                                LayoutSnapshot::Split {
                                    direction: if row {
                                        DirectionSnapshot::Horizontal
                                    } else {
                                        DirectionSnapshot::Vertical
                                    },
                                    ratio,
                                    first: Box::new(first),
                                    second: Box::new(second),
                                },
                            )
                        }
                    });
                }
                Ok(rest.map(|(_, layout)| layout))
            }
        }
    }

    fn tab(
        &mut self,
        custom_name: Option<String>,
        layout: LayoutSnapshot,
        panes: HashMap<u32, PaneSnapshot>,
    ) {
        let mut first = &layout;
        while let LayoutSnapshot::Split { first: inner, .. } = first {
            first = inner;
        }
        let LayoutSnapshot::Pane(focused) = *first else {
            return;
        };
        self.tabs.push(TabSnapshot {
            custom_name,
            layout,
            panes,
            zoomed: false,
            focused: Some(focused),
            root_pane: Some(focused),
        });
    }

    fn snapshot(self) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: None,
            custom_name: self.custom_name,
            identity_cwd: self.identity_cwd,
            worktree_space: None,
            public_pane_numbers: HashMap::new(),
            next_public_pane_number: 0,
            public_tab_numbers: Vec::new(),
            next_public_tab_number: 0,
            tabs: self.tabs,
            active_tab: 0,
        }
    }
}

fn named(name: String) -> Option<String> {
    (!name.trim().is_empty()).then_some(name)
}

/// herdr-hc's first view of the core's live programs. With herdr.app's
/// layout (`app_layout`), its spaces, tabs and splits are kept; any program
/// it does not show opens as a tab of its space, else of one more workspace.
fn initial_layout(
    core: &str,
    records: Vec<Terminal>,
    excluded_pids: &HashSet<u32>,
    app_layout: Option<&serde_json::Value>,
) -> io::Result<Option<SessionSnapshot>> {
    let mut addresses = HashSet::new();
    let mut order = Vec::new();
    let mut programs = HashMap::new();
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
            address: terminal.address.clone(),
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
        // herdr-native's own helpers are not programs.
        if crate::hc_entry::is_hidden(&terminal.facts) {
            continue;
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
        let program = Program {
            cwd: PathBuf::from(
                terminal
                    .facts
                    .get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
            ),
            binding: serde_json::to_value(binding).map_err(io::Error::other)?,
            space: terminal
                .facts
                .get("labels")
                .and_then(|labels| labels.get(crate::hc_entry::SPACE_LABEL))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        };
        order.push(terminal.address.clone());
        programs.insert(terminal.address, program);
    }
    if order.is_empty() {
        return Ok(None);
    }
    let mut left: HashSet<&str> = order.iter().map(String::as_str).collect();
    let spaces = app_layout.map_or_else(Vec::new, |document| {
        shown_spaces(document, |address| left.remove(address))
    });
    let mut workspaces = Vec::new();
    let mut by_space = HashMap::new();
    for space in spaces {
        // Like any herdr workspace, it is identified by its first pane's folder.
        let mut draft = WorkspaceDraft::new(named(space.name), PathBuf::new());
        for tab in space.tabs {
            let mut panes = HashMap::new();
            if let Some(layout) = draft.layout(tab.layout, &programs, &mut panes)? {
                draft.tab(named(tab.name), layout, panes);
            }
        }
        if !draft.tabs.is_empty() {
            by_space.insert(space.id, workspaces.len());
            workspaces.push(draft);
        }
    }
    let mut fallback = None;
    for address in order
        .iter()
        .filter(|address| left.contains(address.as_str()))
    {
        let Some(program) = programs.get(address) else {
            continue;
        };
        let index = match program.space.as_ref().and_then(|space| by_space.get(space)) {
            Some(index) => *index,
            None => *fallback.get_or_insert_with(|| {
                workspaces.push(WorkspaceDraft::new(None, PathBuf::new()));
                workspaces.len() - 1
            }),
        };
        let Some(draft) = workspaces.get_mut(index) else {
            continue;
        };
        let mut panes = HashMap::new();
        let layout = draft.pane(program, &mut panes)?;
        draft.tab(None, layout, panes);
    }
    Ok(Some(SessionSnapshot {
        version: crate::persist::SNAPSHOT_VERSION,
        workspaces: workspaces
            .into_iter()
            .map(WorkspaceDraft::snapshot)
            .collect(),
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
            self.request_layout_sync();
            return;
        }
        let discovery =
            self.terminal_factory
                .initial_terminals()
                .and_then(|(core, terminals, app_layout)| {
                    initial_layout(
                        &core,
                        terminals,
                        &crate::hc_entry::excluded_program_pids(),
                        app_layout.as_ref(),
                    )
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
            self.request_layout_sync();
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
            None,
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
        assert!(initial_layout("owner", vec![], &HashSet::new(), None)
            .unwrap()
            .is_none());
        for records in [
            vec![terminal("t1", "live"), terminal("t1", "live")],
            vec![terminal("t01", "live")],
            vec![terminal("t1", "future")],
        ] {
            assert!(initial_layout("owner", records, &HashSet::new(), None).is_err());
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
            None,
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
        assert!(initial_layout(
            "owner",
            vec![with_pid("t2", 200)],
            &HashSet::from([200]),
            None
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn initial_layout_follows_the_herdr_app_layout_and_keeps_every_program() {
        let labeled = |address: &str, space: &str| -> Terminal {
            serde_json::from_value(serde_json::json!({
                "address":address,"state":"live","cwd":format!("/{address}"),
                "labels":{"herdr.space":space}
            }))
            .unwrap()
        };
        let app = serde_json::json!({"spaces": [
            {"id": "sp1", "name": "Work", "folder": "/work", "tabs": [
                {"id": "tb1", "name": "main", "root": {"id": "tb1", "type": "row", "children": [
                    {"id": "p1", "type": "block", "content": {"terminal": "t1"}, "size": {"weight": 1.0}},
                    {"id": "s1", "type": "column", "size": {"weight": 3.0}, "children": [
                        {"id": "p2", "type": "block", "content": {"terminal": "t2"}},
                        {"id": "p3", "type": "block", "content": {"terminal": "t3"}},
                        {"id": "p4", "type": "block", "content": {"terminal": "t4"}},
                    ]},
                ]}},
            ]},
            {"id": "sp2", "name": "", "folder": "", "tabs": [
                {"id": "tb2", "name": "", "root": {"id": "tb2", "type": "row", "children": [
                    {"id": "p5", "type": "block", "content": {"terminal": "t9"}},
                ]}},
            ]},
        ]});
        let snapshot = initial_layout(
            "owner",
            vec![
                labeled("t1", "sp1"),
                labeled("t2", "sp1"),
                labeled("t3", "sp1"),
                labeled("t4", "sp1"),
                // Not in the layout: a tab in its labeled space, else one more workspace.
                labeled("t5", "sp1"),
                labeled("t6", "sp-unknown"),
            ],
            &HashSet::new(),
            Some(&app),
        )
        .unwrap()
        .unwrap();
        let address = |tab: &TabSnapshot, pane: u32| {
            tab.panes[&pane].hc_binding.as_ref().unwrap()["address"].clone()
        };
        assert_eq!(snapshot.workspaces.len(), 2);
        let work = &snapshot.workspaces[0];
        assert_eq!(work.custom_name.as_deref(), Some("Work"));
        assert_eq!(work.identity_cwd, PathBuf::from("/t1"));
        assert_eq!(work.tabs.len(), 2);
        assert_eq!(work.tabs[0].custom_name.as_deref(), Some("main"));
        let LayoutSnapshot::Split {
            direction: DirectionSnapshot::Horizontal,
            ratio,
            first,
            second,
        } = &work.tabs[0].layout
        else {
            panic!("main tab is not a row");
        };
        assert!((ratio - 0.25).abs() < 1e-6);
        let LayoutSnapshot::Pane(left) = **first else {
            panic!("left of the row is not t1");
        };
        assert_eq!(address(&work.tabs[0], left), "t1");
        assert_eq!(work.tabs[0].focused, Some(left));
        // Three equal shares stacked: a third, then half of the rest.
        let LayoutSnapshot::Split {
            direction: DirectionSnapshot::Vertical,
            ratio,
            second: lower,
            ..
        } = &**second
        else {
            panic!("right of the row is not a column");
        };
        assert!((ratio - 1.0 / 3.0).abs() < 1e-6);
        let LayoutSnapshot::Split { ratio, .. } = &**lower else {
            panic!("the column's lower two are not split");
        };
        assert!((ratio - 0.5).abs() < 1e-6);
        assert_eq!(work.tabs[0].panes.len(), 4);
        assert_eq!(address(&work.tabs[1], 5), "t5");
        let rest = &snapshot.workspaces[1];
        assert_eq!(rest.custom_name, None);
        assert_eq!(rest.identity_cwd, PathBuf::from("/t6"));
        assert_eq!(rest.tabs.len(), 1);
        let pane = rest.tabs[0].root_pane.unwrap();
        assert_eq!(address(&rest.tabs[0], pane), "t6");
    }

    #[test]
    fn first_open_leaves_out_herdr_natives_hidden_helpers() {
        let hidden: Terminal = serde_json::from_value(serde_json::json!({
            "address": "t2", "state": "live", "cwd": "/source",
            "labels": {"herdr-native.hidden": "1"}
        }))
        .unwrap();
        let snapshot = initial_layout(
            "owner",
            vec![terminal("t1", "live"), hidden],
            &HashSet::new(),
            None,
        )
        .unwrap()
        .unwrap();
        let tabs = &snapshot.workspaces[0].tabs;
        assert_eq!(tabs.len(), 1);
        assert_eq!(
            tabs[0]
                .panes
                .values()
                .next()
                .unwrap()
                .hc_binding
                .as_ref()
                .unwrap()["address"],
            "t1"
        );
    }
}
