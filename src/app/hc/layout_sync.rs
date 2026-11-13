//! Native viewers on one core share one layout through herdr.app's document
//! (`herdr-app`): what one viewer splits, closes, renames or moves, the
//! others show too. Focus, the active tab and zoom stay each viewer's own.
//!
//! A round runs off the app thread (`TerminalFactory::sync_herdr_app_layout`):
//! this viewer's changes since the layout it last synced go into the latest
//! document, which then comes back here to be shown. Panes a document no
//! longer has are detached, never their programs ended.
use super::*;
use crate::{
    hc::state::TerminalBinding,
    hc_entry::{
        app_layout::{shown_spaces, ShownLayout, ShownSpace, ShownTab},
        app_sync::{canonical, from_binary, restricted},
    },
    layout::{Node, PaneId},
    persist::{DirectionSnapshot, LayoutSnapshot},
    terminal::TerminalState,
};
use ratatui::layout::Direction;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

#[derive(Default)]
pub(crate) struct LayoutSync {
    /// herdr.app's space and tab ids for this viewer's workspaces and tabs
    /// (keyed by public workspace id, and tab number within it).
    space_ids: HashMap<String, String>,
    tab_ids: HashMap<(String, usize), String>,
    /// The layout as it was when last in step with the document; `None`
    /// until the first round.
    base: Option<Vec<ShownSpace>>,
    in_flight: bool,
    /// Names of this viewer's spaces and tabs (by id) when the running round
    /// was sent: one changed here since keeps its new name.
    sent_names: HashMap<String, String>,
    again: bool,
    backed_up: bool,
    reported: Option<String>,
}

fn snapshot_of(node: &Node) -> LayoutSnapshot {
    match node {
        Node::Pane(pane) => LayoutSnapshot::Pane(pane.raw()),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => LayoutSnapshot::Split {
            direction: match direction {
                Direction::Horizontal => DirectionSnapshot::Horizontal,
                Direction::Vertical => DirectionSnapshot::Vertical,
            },
            ratio: *ratio,
            first: Box::new(snapshot_of(first)),
            second: Box::new(snapshot_of(second)),
        },
    }
}

fn named(name: &str) -> Option<String> {
    (!name.trim().is_empty()).then(|| name.to_owned())
}

fn programs(layout: &ShownLayout, out: &mut Vec<String>) {
    match layout {
        ShownLayout::Program(address) => out.push(address.clone()),
        ShownLayout::Split { parts, .. } => parts.iter().for_each(|(_, p)| programs(p, out)),
    }
}

fn programs_of(layout: &ShownLayout) -> Vec<String> {
    let mut out = Vec::new();
    programs(layout, &mut out);
    out
}

/// herdr.app's shares as herdr's two-way splits: the first part keeps its
/// share of the whole, the rest split what is left the same way.
fn fold(layout: &ShownLayout, panes: &HashMap<String, PaneId>) -> Option<Node> {
    match layout {
        ShownLayout::Program(address) => panes.get(address).copied().map(Node::Pane),
        ShownLayout::Split { row, parts } => {
            let mut built: Vec<(f64, Node)> = parts
                .iter()
                .filter_map(|(share, part)| Some((*share, fold(part, panes)?)))
                .collect();
            let mut rest: Option<(f64, Node)> = None;
            while let Some((share, first)) = built.pop() {
                rest = Some(match rest {
                    None => (share, first),
                    Some((rest_share, second)) => {
                        let total = share + rest_share;
                        let ratio = if total > 0.0 { share / total } else { 0.5 };
                        (
                            total,
                            Node::Split {
                                direction: if *row {
                                    Direction::Horizontal
                                } else {
                                    Direction::Vertical
                                },
                                ratio: ratio.clamp(0.05, 0.95) as f32,
                                first: Box::new(first),
                                second: Box::new(second),
                            },
                        )
                    }
                });
            }
            rest.map(|(_, node)| node)
        }
    }
}

/// The same spaces, tabs, names and arrangements, in the same order.
fn same(a: &[ShownSpace], b: &[ShownSpace]) -> bool {
    let tabs = |x: &ShownTab, y: &ShownTab| {
        x.id == y.id && x.name == y.name && canonical(&x.layout) == canonical(&y.layout)
    };
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.id == y.id
                && x.name == y.name
                && x.tabs.len() == y.tabs.len()
                && x.tabs.iter().zip(&y.tabs).all(|(s, t)| tabs(s, t))
        })
}

impl App {
    fn shared_core(&self) -> Option<String> {
        let source = self.terminal_factory.source_state()?;
        Some(source.client?.hello.core.id.clone())
    }

    fn shared_address(&self, terminal: &crate::terminal::TerminalId, core: &str) -> Option<String> {
        let binding = self.state.terminals.get(terminal)?.hc_binding.as_ref()?;
        (binding["core_id"].as_str()? == core)
            .then(|| binding["address"].as_str().map(str::to_owned))?
    }

    fn shared_addresses(&self, core: &str) -> HashMap<crate::terminal::TerminalId, String> {
        self.state
            .terminals
            .iter()
            .filter_map(|(id, terminal)| {
                let binding = terminal.hc_binding.as_ref()?;
                (binding["core_id"].as_str()? == core)
                    .then(|| Some((id.clone(), binding["address"].as_str()?.to_owned())))
                    .flatten()
            })
            .collect()
    }

    /// This viewer's layout in herdr.app's terms, giving workspaces and tabs
    /// not yet in the document new ids.
    fn project_shared_layout(&mut self) -> Vec<ShownSpace> {
        let Some(core) = self.shared_core() else {
            return Vec::new();
        };
        let addresses = self.shared_addresses(&core);
        let mut spaces = Vec::new();
        for workspace in &self.state.workspaces {
            let mut tabs = Vec::new();
            for tab in &workspace.tabs {
                let address = |raw: u32| {
                    let pane = tab.panes.get(&PaneId::from_raw(raw))?;
                    addresses.get(&pane.attached_terminal_id).cloned()
                };
                let Some(layout) = from_binary(&snapshot_of(tab.layout.root()), &address) else {
                    continue;
                };
                let id = self
                    .layout_sync
                    .tab_ids
                    .entry((workspace.id.clone(), tab.number))
                    .or_insert_with(|| crate::hc_entry::new_placement_id("tb"))
                    .clone();
                tabs.push(ShownTab {
                    id,
                    name: tab.custom_name.clone().unwrap_or_default(),
                    layout,
                });
            }
            if tabs.is_empty() {
                continue;
            }
            let id = self
                .layout_sync
                .space_ids
                .entry(workspace.id.clone())
                .or_insert_with(|| crate::hc_entry::new_placement_id("sp"))
                .clone();
            spaces.push(ShownSpace {
                id,
                name: workspace.custom_name.clone().unwrap_or_default(),
                folder: workspace.identity_cwd.display().to_string(),
                tabs,
            });
        }
        spaces
    }

    /// After a change here: a round, unless the layout is what was synced.
    pub(crate) fn layout_changed_here(&mut self) {
        if !self.terminal_factory.shares_herdr_app_layout() {
            return;
        }
        if let Some(base) = self.layout_sync.base.clone() {
            if same(&self.project_shared_layout(), &base) {
                return;
            }
        }
        self.request_layout_sync();
    }

    /// herdr.app's document changed on the core: a round shows it.
    pub(crate) fn layout_changed_elsewhere(&mut self, generation: u64) {
        if self
            .terminal_factory
            .source_state()
            .is_some_and(|source| source.generation == generation)
        {
            self.request_layout_sync();
        }
    }

    /// Starts a round now, or right after the one running.
    pub(crate) fn request_layout_sync(&mut self) {
        if !self.terminal_factory.shares_herdr_app_layout()
            || self.hc_recovery.initial_inventory_due.is_some()
        {
            return;
        }
        if self.layout_sync.in_flight {
            self.layout_sync.again = true;
            return;
        }
        let Some(source) = self.terminal_factory.source_state() else {
            return;
        };
        let native = self.project_shared_layout();
        let names: HashMap<String, String> = native
            .iter()
            .flat_map(|space| {
                std::iter::once((space.id.clone(), space.name.clone())).chain(
                    space
                        .tabs
                        .iter()
                        .map(|tab| (tab.id.clone(), tab.name.clone())),
                )
            })
            .collect();
        if self.terminal_factory.sync_herdr_app_layout(
            source.generation,
            self.layout_sync.base.clone(),
            native,
            self.event_tx.clone(),
        ) {
            self.layout_sync.in_flight = true;
            self.layout_sync.sent_names = names;
            self.layout_sync.again = false;
        }
    }

    pub(crate) fn finish_layout_sync(
        &mut self,
        generation: u64,
        outcome: Result<(serde_json::Value, Vec<String>, Vec<ShownSpace>), String>,
    ) {
        self.layout_sync.in_flight = false;
        let current = self
            .terminal_factory
            .source_state()
            .map(|source| source.generation);
        match outcome {
            Err(error) => {
                if self.layout_sync.reported.as_deref() != Some(error.as_str()) {
                    tracing::warn!(%error, "the shared herdr.app layout could not be synced");
                    self.layout_sync.reported = Some(error);
                }
            }
            Ok(_) if current != Some(generation) => self.layout_sync.again = true,
            Ok((document, live, sent)) => {
                self.layout_sync.reported = None;
                let mut left: HashSet<String> = live.into_iter().collect();
                let view = shown_spaces(&document, |address| left.remove(address));
                if self.layout_sync.base.is_none() && !self.layout_sync.backed_up {
                    self.layout_sync.backed_up = true;
                    if self.policy.persist_session {
                        match crate::persist::backup_session("before-shared-layout") {
                            Ok(Some(path)) => {
                                tracing::info!(path = %path.display(), "saved session kept before the shared layout replaced it");
                            }
                            Ok(None) => {}
                            Err(error) => {
                                tracing::warn!(%error, "saved session could not be kept before the shared layout replaced it");
                            }
                        }
                    }
                }
                // What may be taken away here: what this viewer sent, what it
                // had synced (programs that ended since are left out of what it
                // sends), and any program that is no longer running.
                let mut known: HashSet<String> = self
                    .layout_sync
                    .base
                    .iter()
                    .flatten()
                    .chain(&sent)
                    .flat_map(|space| &space.tabs)
                    .flat_map(|tab| programs_of(&tab.layout))
                    .collect();
                if let Some(core) = self.shared_core() {
                    // Ended by its own facts: a list read before it started would
                    // miss a program made since, which is not an ended one.
                    let ended: Vec<String> = self
                        .shared_addresses(&core)
                        .into_iter()
                        .filter(|(id, _)| {
                            self.state.terminals.get(id).is_some_and(|terminal| {
                                terminal.hc_snapshot.as_ref().is_some_and(|snapshot| {
                                    snapshot.terminal.facts.get("state")
                                        == Some(&serde_json::Value::from("finished"))
                                })
                            })
                        })
                        .map(|(_, address)| address)
                        .collect();
                    known.extend(ended);
                }
                let changed = self.apply_shared_layout(&view, &known);
                // In step with the document, less what this viewer could not
                // show (its next round must not drop those from the document).
                let shown: HashSet<String> = self
                    .project_shared_layout()
                    .iter()
                    .flat_map(|space| &space.tabs)
                    .flat_map(|tab| programs_of(&tab.layout))
                    .collect();
                self.layout_sync.base = Some(restricted(&view, |address| shown.contains(address)));
                if changed {
                    self.state.session_dirty = true;
                    self.render_dirty.request_generic();
                    self.render_notify.notify_one();
                }
            }
        }
        if self.layout_sync.again {
            self.request_layout_sync();
        }
    }

    /// Shows the document's spaces, tabs and arrangements: matching
    /// workspaces and tabs are rearranged in place (their programs keep their
    /// panes), missing ones are made by attaching, and ones the document no
    /// longer has are detached. Focus, active tab and zoom stay.
    ///
    /// Three-way like a push: only programs in `known` can be
    /// taken away, and only names it has not changed since sending are taken;
    /// what it made since stays for its next round.
    fn apply_shared_layout(&mut self, view: &[ShownSpace], known: &HashSet<String>) -> bool {
        // A name renamed here since the round was sent stays; any other takes
        // the document's (a viewer that just joined has none of its own).
        let sent_names = std::mem::take(&mut self.layout_sync.sent_names);
        let renamed_here = |id: &str, now: &Option<String>| {
            sent_names
                .get(id)
                .is_some_and(|at_send| at_send != now.as_deref().unwrap_or_default())
        };
        let Some(core) = self.shared_core() else {
            return false;
        };
        let current = self.project_shared_layout();
        if same(&current, view) {
            return false;
        }
        let mut changed = false;
        let mut claimed: HashSet<String> = HashSet::new();
        for space in view {
            let Some(ws_id) = self.shared_workspace_for(space, &claimed, &core) else {
                continue;
            };
            changed |= self.layout_sync.space_ids.get(&ws_id) != Some(&space.id);
            self.layout_sync
                .space_ids
                .insert(ws_id.clone(), space.id.clone());
            claimed.insert(ws_id.clone());
            let Some(ws_idx) = self.state.workspaces.iter().position(|w| w.id == ws_id) else {
                continue;
            };
            let name = named(&space.name);
            let workspace = &self.state.workspaces[ws_idx];
            if !renamed_here(&space.id, &workspace.custom_name) && workspace.custom_name != name {
                self.state.workspaces[ws_idx].custom_name = name;
                changed = true;
            }
            let mut order = Vec::new();
            for tab in &space.tabs {
                let Some(number) = self.shared_tab_for(&ws_id, tab, &order, &core) else {
                    continue;
                };
                order.push(number);
                self.layout_sync
                    .tab_ids
                    .insert((ws_id.clone(), number), tab.id.clone());
                let Some(ws_idx) = self.state.workspaces.iter().position(|w| w.id == ws_id) else {
                    break;
                };
                let Some(tab_idx) = self.state.workspaces[ws_idx]
                    .tabs
                    .iter()
                    .position(|t| t.number == number)
                else {
                    continue;
                };
                let name = named(&tab.name);
                let workspace = &mut self.state.workspaces[ws_idx];
                let current = &workspace.tabs[tab_idx].custom_name;
                if !renamed_here(&tab.id, current) && *current != name {
                    workspace.tabs[tab_idx].custom_name = name;
                    changed = true;
                }
                changed |= self.rearrange_shared_tab(ws_idx, tab_idx, &tab.layout, &core, known);
            }
            // Tabs the document no longer has here: detached.
            let dropped: Vec<PaneId> = self
                .state
                .workspaces
                .iter()
                .find(|w| w.id == ws_id)
                .into_iter()
                .flat_map(|w| &w.tabs)
                .filter(|t| !order.contains(&t.number))
                .flat_map(|t| t.panes.iter())
                .filter(|(_, pane)| {
                    self.shared_address(&pane.attached_terminal_id, &core)
                        .is_some_and(|a| known.contains(&a))
                })
                .map(|(pane, _)| *pane)
                .collect();
            changed |= !dropped.is_empty();
            self.detach_panes(&dropped);
            if let Some(workspace) = self.state.workspaces.iter_mut().find(|w| w.id == ws_id) {
                for (target, number) in order.iter().enumerate() {
                    let Some(index) = workspace.tabs.iter().position(|t| t.number == *number)
                    else {
                        continue;
                    };
                    if index != target {
                        let insert = if index > target { target } else { target + 1 };
                        changed |= workspace.move_tab(index, insert);
                    }
                }
            }
        }
        // Workspaces the document no longer has: detached.
        let dropped: Vec<PaneId> = self
            .state
            .workspaces
            .iter()
            .filter(|w| !claimed.contains(&w.id))
            .flat_map(|w| &w.tabs)
            .flat_map(|t| t.panes.iter())
            .filter(|(_, pane)| {
                self.shared_address(&pane.attached_terminal_id, &core)
                    .is_some_and(|a| known.contains(&a))
            })
            .map(|(pane, _)| *pane)
            .collect();
        changed |= !dropped.is_empty();
        self.detach_panes(&dropped);
        changed
    }

    /// Lets go of panes without ending their programs. Dropping an hc
    /// runtime only closes its streams; `shutdown` (which pane removal
    /// would run next) is what ends the program, so the runtime goes first.
    fn detach_terminals(&mut self, terminals: &[crate::terminal::TerminalId]) {
        for id in terminals {
            if let Some(runtime) = self.terminal_runtimes.remove(id) {
                drop(runtime);
            }
        }
    }

    fn detach_panes(&mut self, panes: &[PaneId]) {
        let terminals: Vec<_> = self
            .state
            .workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
            .filter(|(pane, _)| panes.contains(pane))
            .map(|(_, state)| state.attached_terminal_id.clone())
            .collect();
        self.detach_terminals(&terminals);
        for pane in panes {
            self.state.handle_pane_died(*pane);
        }
    }

    fn binding(core: &str, address: &str) -> TerminalBinding {
        TerminalBinding {
            core_id: core.to_owned(),
            address: address.to_owned(),
        }
    }

    /// The workspace showing `space`: the one synced with it, else the one
    /// showing most of its programs, else a new one.
    fn shared_workspace_for(
        &mut self,
        space: &ShownSpace,
        claimed: &HashSet<String>,
        core: &str,
    ) -> Option<String> {
        let open = |id: &String| {
            !claimed.contains(id) && self.state.workspaces.iter().any(|w| &w.id == id)
        };
        if let Some((id, _)) = self
            .layout_sync
            .space_ids
            .iter()
            .find(|(id, s)| **s == space.id && open(id))
        {
            return Some(id.clone());
        }
        let wanted: HashSet<String> = space
            .tabs
            .iter()
            .flat_map(|t| programs_of(&t.layout))
            .collect();
        let addresses = self.shared_addresses(core);
        let best = self
            .state
            .workspaces
            .iter()
            .filter(|w| !claimed.contains(&w.id))
            .map(|w| {
                let shared = w
                    .tabs
                    .iter()
                    .flat_map(|t| t.panes.values())
                    .filter(|p| {
                        addresses
                            .get(&p.attached_terminal_id)
                            .is_some_and(|a| wanted.contains(a))
                    })
                    .count();
                (shared, w.id.clone())
            })
            .filter(|(shared, _)| *shared > 0)
            .max_by_key(|(shared, _)| *shared);
        if let Some((_, id)) = best {
            return Some(id);
        }
        let first = space
            .tabs
            .first()
            .map(|t| programs_of(&t.layout))?
            .into_iter()
            .next()?;
        let (rows, cols) = self.state.new_pane_size(crate::ui::NewPanePlacement::Alone);
        match crate::workspace::Workspace::new_attached(
            PathBuf::from(&space.folder),
            rows,
            cols,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.pane_host_terminal_appearance(),
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
            &Self::binding(core, &first),
            &self.terminal_factory,
        ) {
            Ok((workspace, terminal, runtime)) => {
                self.terminal_runtimes.insert(terminal.id.clone(), runtime);
                self.state.terminals.insert(terminal.id.clone(), terminal);
                let id = workspace.id.clone();
                self.state.workspaces.push(workspace);
                if self.state.active.is_none() {
                    self.state.active = Some(self.state.workspaces.len() - 1);
                }
                Some(id)
            }
            Err(error) => {
                tracing::warn!(%error, address = %first, "a shared space could not be shown");
                None
            }
        }
    }

    /// The tab (by number) showing `tab` in workspace `ws_id`: the one synced
    /// with it, else the one showing most of its programs, else a new one.
    fn shared_tab_for(
        &mut self,
        ws_id: &str,
        tab: &ShownTab,
        taken: &[usize],
        core: &str,
    ) -> Option<usize> {
        let ws_idx = self.state.workspaces.iter().position(|w| w.id == ws_id)?;
        let workspace = &self.state.workspaces[ws_idx];
        if let Some(found) = workspace.tabs.iter().find(|t| {
            !taken.contains(&t.number)
                && self.layout_sync.tab_ids.get(&(ws_id.to_owned(), t.number)) == Some(&tab.id)
        }) {
            return Some(found.number);
        }
        let wanted: HashSet<String> = programs_of(&tab.layout).into_iter().collect();
        let addresses = self.shared_addresses(core);
        let best = workspace
            .tabs
            .iter()
            .filter(|t| !taken.contains(&t.number))
            .map(|t| {
                let shared = t
                    .panes
                    .values()
                    .filter(|p| {
                        addresses
                            .get(&p.attached_terminal_id)
                            .is_some_and(|a| wanted.contains(a))
                    })
                    .count();
                (shared, t.number)
            })
            .filter(|(shared, _)| *shared > 0)
            .max_by_key(|(shared, _)| *shared);
        if let Some((_, number)) = best {
            return Some(number);
        }
        let first = programs_of(&tab.layout).into_iter().next()?;
        let (rows, cols) = self.state.new_pane_size(crate::ui::NewPanePlacement::Alone);
        let scrollback = self.state.pane_scrollback_limit_bytes;
        let theme = self.state.host_terminal_theme;
        let appearance = self.pane_host_terminal_appearance();
        let workspace = &mut self.state.workspaces[ws_idx];
        match workspace.create_tab_attached(
            rows,
            cols,
            PathBuf::new(),
            scrollback,
            theme,
            appearance,
            &Self::binding(core, &first),
            &self.terminal_factory,
        ) {
            Ok((tab_idx, terminal, runtime)) => {
                let number = workspace.tabs[tab_idx].number;
                self.terminal_runtimes.insert(terminal.id.clone(), runtime);
                self.state.terminals.insert(terminal.id.clone(), terminal);
                Some(number)
            }
            Err(error) => {
                tracing::warn!(%error, address = %first, "a shared tab could not be shown");
                None
            }
        }
    }

    /// Arranges one tab as `layout`: its programs keep their panes, new ones
    /// attach, ones the layout no longer has are detached.
    fn rearrange_shared_tab(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        layout: &ShownLayout,
        core: &str,
        known: &HashSet<String>,
    ) -> bool {
        let addresses = self.shared_addresses(core);
        let Some(tab) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|w| w.tabs.get(tab_idx))
        else {
            return false;
        };
        let mut by_address: HashMap<String, PaneId> = HashMap::new();
        let mut terminal_of: HashMap<PaneId, crate::terminal::TerminalId> = HashMap::new();
        for (pane, state) in &tab.panes {
            // A pane this round cannot place: leave the tab as it is.
            let Some(address) = addresses.get(&state.attached_terminal_id) else {
                return false;
            };
            by_address.insert(address.clone(), *pane);
            terminal_of.insert(*pane, state.attached_terminal_id.clone());
        }
        let address = |raw: u32| {
            let pane = PaneId::from_raw(raw);
            by_address
                .iter()
                .find(|(_, p)| **p == pane)
                .map(|(a, _)| a.clone())
        };
        if from_binary(&snapshot_of(tab.layout.root()), &address)
            .is_some_and(|current| canonical(&current) == canonical(layout))
        {
            return false;
        }
        let wanted = programs_of(layout);
        // Made here since the round was sent: this tab waits for the next one.
        if by_address
            .keys()
            .any(|address| !wanted.contains(address) && !known.contains(address))
        {
            return false;
        }
        let removed: Vec<PaneId> = by_address
            .iter()
            .filter(|(address, _)| !wanted.contains(address))
            .map(|(_, pane)| *pane)
            .collect();
        let (events, notify, dirty) = (
            tab.events.clone(),
            tab.render_notify.clone(),
            tab.render_dirty.clone(),
        );
        let (rows, cols) = self.state.new_pane_size(crate::ui::NewPanePlacement::Alone);
        let mut added = Vec::new();
        let missing: Vec<String> = wanted
            .iter()
            .filter(|a| !by_address.contains_key(*a))
            .cloned()
            .collect();
        for address in &missing {
            let pane = PaneId::alloc();
            let binding = Self::binding(core, address);
            match self.terminal_factory.restore(
                &binding,
                pane,
                rows,
                cols,
                self.state.pane_scrollback_limit_bytes,
                self.state.host_terminal_theme,
                self.pane_host_terminal_appearance(),
                events.clone(),
                notify.clone(),
                dirty.clone(),
            ) {
                Ok(runtime) => {
                    let id = crate::terminal::TerminalId::alloc();
                    let Ok(raw) = serde_json::to_value(&binding) else {
                        continue;
                    };
                    let terminal = TerminalState::new(id.clone(), PathBuf::new())
                        .with_hc_binding(Some(raw))
                        .with_runtime(&runtime);
                    self.terminal_runtimes.insert(id.clone(), runtime);
                    self.state.terminals.insert(id.clone(), terminal);
                    added.push((pane, crate::pane::PaneState::new(id)));
                    by_address.insert(address.clone(), pane);
                }
                Err(error) => {
                    tracing::warn!(%error, %address, "a program in the shared layout could not be shown");
                }
            }
        }
        let Some(root) = fold(layout, &by_address) else {
            return false;
        };
        let gone: Vec<_> = removed
            .iter()
            .filter_map(|p| terminal_of.get(p).cloned())
            .collect();
        if !self.state.workspaces[ws_idx].rearrange_tab(tab_idx, root, added, &removed) {
            return false;
        }
        self.state.forget_pane_aliases(&removed);
        self.detach_terminals(&gone);
        self.state.remove_unattached_terminal_ids(gone);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(address: &str) -> ShownLayout {
        ShownLayout::Program(address.into())
    }

    #[test]
    fn shares_fold_back_into_the_same_arrangement() {
        let layout = ShownLayout::Split {
            row: true,
            parts: vec![
                (0.25, p("t1")),
                (
                    0.75,
                    ShownLayout::Split {
                        row: false,
                        parts: vec![(1.0, p("t2")), (1.0, p("t3")), (2.0, p("t4"))],
                    },
                ),
            ],
        };
        let panes: HashMap<String, PaneId> = ["t1", "t2", "t3", "t4"]
            .iter()
            .enumerate()
            .map(|(i, a)| ((*a).to_owned(), PaneId::from_raw(100 + i as u32)))
            .collect();
        let node = fold(&layout, &panes).unwrap();
        let address = |raw: u32| {
            panes
                .iter()
                .find(|(_, p)| p.raw() == raw)
                .map(|(a, _)| a.clone())
        };
        let back = from_binary(&snapshot_of(&node), &address).unwrap();
        assert_eq!(canonical(&back), canonical(&layout));
    }

    #[test]
    fn layouts_differing_in_name_order_or_shares_are_not_the_same() {
        let tab = |id: &str, layout| ShownTab {
            id: id.into(),
            name: String::new(),
            layout,
        };
        let space = |tabs| ShownSpace {
            id: "sp".into(),
            name: String::new(),
            folder: "/a".into(),
            tabs,
        };
        let a = vec![space(vec![tab("x", p("t1")), tab("y", p("t2"))])];
        let moved = vec![space(vec![tab("y", p("t2")), tab("x", p("t1"))])];
        let mut renamed = a.clone();
        renamed[0].tabs[0].name = "work".into();
        let mut moved_folder = a.clone();
        moved_folder[0].folder = "/b".into();
        assert!(same(&a, &a.clone()));
        assert!(same(&a, &moved_folder));
        assert!(!same(&a, &moved));
        assert!(!same(&a, &renamed));
    }
}
