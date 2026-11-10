use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ratatui::layout::Direction;
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::{Node, PaneId, TileLayout};
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::render_signal::RenderSignal;
use crate::terminal::{TerminalId, TerminalRuntime, TerminalRuntimeRegistry, TerminalState};

pub(crate) type DetachedPane = (PaneId, TerminalId);

pub(crate) struct MovedPane {
    pub pane_id: PaneId,
    pub pane_state: PaneState,
}

pub struct NewPane {
    pub pane_id: PaneId,
    pub terminal: TerminalState,
    pub runtime: TerminalRuntime,
}

pub struct Tab {
    pub custom_name: Option<String>,
    pub number: usize,
    /// Identity source for this tab's pane tree.
    pub root_pane: PaneId,
    pub layout: TileLayout,
    /// Pane viewport state — always present, testable without PTYs.
    pub panes: HashMap<PaneId, PaneState>,
    #[cfg(test)]
    pub runtimes: HashMap<PaneId, TerminalRuntime>,
    pub zoomed: bool,
    pub events: mpsc::Sender<AppEvent>,
    pub(crate) render_notify: Arc<Notify>,
    pub(crate) render_dirty: Arc<RenderSignal>,
}

impl Tab {
    pub(crate) fn full_zoom(&self) -> bool {
        self.zoomed
            && self
                .panes
                .get(&self.layout.focused())
                .is_some_and(|pane| pane.full_zoom)
    }

    pub(crate) fn visible_panes(
        &self,
        area: ratatui::layout::Rect,
    ) -> Vec<crate::layout::PaneInfo> {
        if self.full_zoom() {
            return vec![crate::layout::PaneInfo {
                id: self.layout.focused(),
                rect: area,
                inner_rect: area,
                scrollbar_rect: None,
                borders: ratatui::widgets::Borders::NONE,
                is_focused: true,
            }];
        }
        if self.zoomed {
            self.layout.zoomed_panes(area)
        } else {
            self.layout.panes(area)
        }
    }

    pub(crate) fn visible_splits(
        &self,
        area: ratatui::layout::Rect,
    ) -> Vec<crate::layout::SplitBorder> {
        if self.full_zoom() {
            Vec::new()
        } else if self.zoomed {
            self.layout.zoomed_splits(area)
        } else {
            self.layout.splits(area)
        }
    }

    pub(crate) fn pane_visible(&self, pane: PaneId) -> bool {
        self.panes.contains_key(&pane)
            && (!self.zoomed
                || if self.full_zoom() {
                    self.layout.focused() == pane
                } else {
                    self.layout.zoomed_pane_visible(pane)
                })
    }

    pub(crate) fn visible_pane_ids(&self) -> Vec<PaneId> {
        if self.full_zoom() {
            vec![self.layout.focused()]
        } else if self.zoomed {
            self.layout.zoomed_pane_ids()
        } else {
            self.layout.pane_ids()
        }
    }

    // Construction threads source creation, geometry, host context, and render hooks.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_runtime(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
        argv: Option<&[String]>,
        factory: &crate::terminal::TerminalFactory,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        let (layout, root_id) = TileLayout::new();
        let command = argv.map_or(
            crate::terminal::TerminalCommand::Shell(shell_config),
            |argv| {
                crate::terminal::TerminalCommand::Argv(argv, crate::pane::AgentDetection::Enabled)
            },
        );
        let runtime = factory.spawn(
            root_id,
            rows,
            cols,
            initial_cwd.clone(),
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            command,
            launch_env,
            events.clone(),
            render_notify.clone(),
            render_dirty.clone(),
        )?;

        let terminal_id = TerminalId::alloc();
        let terminal = match argv {
            Some(argv) => {
                TerminalState::new(terminal_id.clone(), initial_cwd).with_launch_argv(argv.to_vec())
            }
            None => TerminalState::new(terminal_id.clone(), initial_cwd),
        };
        let terminal = terminal.with_runtime(&runtime);
        let mut panes = HashMap::new();
        panes.insert(root_id, PaneState::new(terminal_id));

        Ok((
            Self {
                custom_name: None,
                number,
                root_pane: root_id,
                layout,
                panes,
                #[cfg(test)]
                runtimes: HashMap::new(),
                zoomed: false,
                events,
                render_notify,
                render_dirty,
            },
            terminal,
            runtime,
        ))
    }

    pub fn is_auto_named(&self) -> bool {
        self.custom_name.is_none()
    }

    pub fn set_custom_name(&mut self, name: String) {
        self.custom_name = Some(name);
    }

    /// A failed source launch rolls layout back without touching focus or its history.
    // Split construction threads geometry, host context, launch policy, and command state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn split_pane_with_runtime(
        &mut self,
        target: PaneId,
        focus_new_pane: bool,
        direction: Direction,
        ratio: Option<f32>,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        command: Option<crate::terminal::TerminalCommand<'_>>,
        factory: &crate::terminal::TerminalFactory,
    ) -> std::io::Result<NewPane> {
        let Some(new_id) = self
            .layout
            .split_pane(target, direction, ratio.unwrap_or(0.5))
        else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "split target pane is not in the layout",
            ));
        };
        let actual_cwd = cwd.unwrap_or_else(|| {
            if factory.is_saved_source() {
                PathBuf::new()
            } else {
                std::env::current_dir().unwrap_or_else(|_| "/".into())
            }
        });
        let launch_argv = if let Some(crate::terminal::TerminalCommand::Argv(argv, _)) = &command {
            Some((*argv).to_vec())
        } else {
            None
        };
        let runtime = factory.spawn(
            new_id,
            rows,
            cols,
            actual_cwd.clone(),
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            command.unwrap_or(crate::terminal::TerminalCommand::Shell(shell_config)),
            launch_env,
            self.events.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        );
        let runtime = match runtime {
            Ok(runtime) => runtime,
            Err(err) => {
                self.layout.close_pane(new_id);
                return Err(err);
            }
        };
        let terminal_id = TerminalId::alloc();
        let terminal = match launch_argv {
            Some(argv) => {
                TerminalState::new(terminal_id.clone(), actual_cwd).with_launch_argv(argv)
            }
            None => TerminalState::new(terminal_id.clone(), actual_cwd),
        };
        let terminal = terminal.with_runtime(&runtime);
        if focus_new_pane {
            self.layout.focus_pane(new_id);
        }
        self.panes.insert(new_id, PaneState::new(terminal_id));
        self.zoomed = false;
        Ok(NewPane {
            pane_id: new_id,
            terminal,
            runtime,
        })
    }

    #[cfg(test)]
    pub fn close_focused(&mut self) -> Option<DetachedPane> {
        let pane_id = self.layout.focused();
        self.detach_pane(pane_id)
    }

    pub fn close_pane(&mut self, pane_id: PaneId) -> Option<DetachedPane> {
        self.detach_pane(pane_id)
    }

    pub fn remove_pane(&mut self, pane_id: PaneId) -> Option<DetachedPane> {
        self.detach_pane(pane_id)
    }

    pub(crate) fn from_existing_pane(
        number: usize,
        custom_name: Option<String>,
        moved: MovedPane,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> Self {
        let mut panes = HashMap::new();
        let pane_id = moved.pane_id;
        panes.insert(pane_id, moved.pane_state);
        Self {
            custom_name,
            number,
            root_pane: pane_id,
            layout: TileLayout::from_saved(Node::Pane(pane_id), pane_id),
            panes,
            #[cfg(test)]
            runtimes: HashMap::new(),
            zoomed: false,
            events,
            render_notify,
            render_dirty,
        }
    }

    pub(crate) fn take_pane_for_move(&mut self, pane_id: PaneId) -> Option<MovedPane> {
        if !self.panes.contains_key(&pane_id) {
            return None;
        }

        if self.layout.pane_count() > 1 {
            let next_root = self.promoted_root_if_needed(pane_id);
            self.layout.close_pane(pane_id);
            if let Some(next_root) = next_root {
                self.root_pane = next_root;
            }
        }

        let pane_state = self.panes.remove(&pane_id)?;
        self.zoomed = false;
        Some(MovedPane {
            pane_id,
            pane_state,
        })
    }

    pub(crate) fn insert_existing_pane(
        &mut self,
        target_pane_id: PaneId,
        moved: MovedPane,
        direction: Direction,
        ratio: f32,
        focus: bool,
    ) -> Result<PaneId, MovedPane> {
        if !self
            .layout
            .insert_pane_near(target_pane_id, moved.pane_id, direction, ratio, focus)
        {
            return Err(moved);
        }
        let pane_id = moved.pane_id;
        self.panes.insert(pane_id, moved.pane_state);
        self.zoomed = false;
        Ok(pane_id)
    }

    fn detach_pane(&mut self, pane_id: PaneId) -> Option<DetachedPane> {
        if self.layout.pane_count() <= 1 {
            return None;
        }

        let next_root = self.promoted_root_if_needed(pane_id);

        self.layout.close_pane(pane_id);

        let pane = self.panes.remove(&pane_id)?;
        let terminal_id = pane.attached_terminal_id;
        self.zoomed = false;
        if let Some(next_root) = next_root {
            self.root_pane = next_root;
        }
        Some((pane_id, terminal_id))
    }

    fn promoted_root_if_needed(&self, closing: PaneId) -> Option<PaneId> {
        if self.root_pane != closing {
            return None;
        }
        self.layout.pane_ids().into_iter().find(|id| *id != closing)
    }

    pub fn terminal_id(&self, pane_id: PaneId) -> Option<&TerminalId> {
        self.panes
            .get(&pane_id)
            .map(|pane| &pane.attached_terminal_id)
    }

    pub fn cwd_for_pane(
        &self,
        pane_id: PaneId,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<PathBuf> {
        let terminal_id = self.terminal_id(pane_id)?;
        terminal_runtimes
            .get(terminal_id)
            .and_then(|rt| rt.cwd())
            .or_else(|| {
                terminals
                    .get(terminal_id)
                    .map(|terminal| terminal.cwd.clone())
            })
    }

    pub fn foreground_cwd_for_pane(
        &self,
        pane_id: PaneId,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<PathBuf> {
        let terminal_id = self.terminal_id(pane_id)?;
        terminal_runtimes
            .get(terminal_id)
            .and_then(|rt| rt.foreground_cwd())
    }
}
