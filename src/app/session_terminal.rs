//! The session terminal: one terminal of the whole session, outside every
//! tab (the files dock's program), that clients show beside their tabs. It
//! starts when a client first shows it and stays while the session runs;
//! when it exits it starts again after a short wait, and after exiting
//! three times within a minute it waits for a client to ask again.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::app::App;
use crate::layout::PaneId;
use crate::pane::PaneLaunchEnv;
use crate::protocol::session_terminal::SessionTerminalStatus;
use crate::terminal::{TerminalId, TerminalState};

/// What the session terminal runs, when this build has one.
#[derive(Clone, Debug)]
pub(crate) struct SessionTerminalSpec {
    pub role: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Its settings for the client's settings page: this command with
    /// `--settings-list`, or `--settings-set <key> <json>`.
    pub settings: Option<ComponentSettingsSpec>,
    /// A plugin it replaces: while that is still enabled, its first start
    /// says how to turn the plugin off.
    pub replaces_plugin: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ComponentSettingsSpec {
    pub title: String,
    pub argv: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionTerminalState {
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
    pub role: String,
    pub cols: u16,
    pub rows: u16,
}

const FAIL_WINDOW: Duration = Duration::from_secs(60);
const FAIL_AFTER: usize = 3;

/// Exits of the session terminal: when it may start again.
#[derive(Debug, Default)]
pub(crate) struct SessionTerminalLife {
    exits: VecDeque<Instant>,
    restart_at: Option<Instant>,
    failed: bool,
    replaced_plugin_noted: bool,
}

impl SessionTerminalLife {
    fn exited(&mut self, now: Instant) {
        self.exits.push_back(now);
        while self
            .exits
            .front()
            .is_some_and(|at| now.duration_since(*at) > FAIL_WINDOW)
        {
            self.exits.pop_front();
        }
        if self.exits.len() >= FAIL_AFTER {
            self.failed = true;
            self.restart_at = None;
        } else {
            // 1 s, then 2 s.
            let wait = Duration::from_secs(1 << (self.exits.len() - 1));
            self.restart_at = Some(now + wait);
        }
    }

    fn may_start(&self, now: Instant) -> bool {
        !self.failed && self.restart_at.is_none_or(|at| now >= at)
    }

    /// A client asked again for a terminal that kept exiting.
    fn retry(&mut self) {
        self.failed = false;
        self.exits.clear();
        self.restart_at = None;
    }
}

/// This app's session terminals are known by its app directory.
pub(crate) fn session_terminal_owner() -> String {
    crate::config::config_dir().display().to_string()
}

impl App {
    /// Ends the session terminal a previous run of this app left on its hc
    /// core, before showing a new one.
    pub(crate) fn end_stale_session_terminals(&self) {
        if let Some(spec) = &self.session_terminal_spec {
            self.terminal_factory
                .end_stale_session_terminals(&spec.role, &session_terminal_owner());
        }
    }

    pub(crate) fn session_terminal_available(&self) -> bool {
        self.session_terminal_spec.is_some()
    }

    pub(crate) fn session_terminal_status(&self) -> Option<SessionTerminalStatus> {
        self.session_terminal_spec.as_ref()?;
        Some(if self.state.session_terminal.is_some() {
            SessionTerminalStatus::Running
        } else if self.session_terminal_life.failed {
            SessionTerminalStatus::Failed
        } else {
            SessionTerminalStatus::Restarting
        })
    }

    /// When a stopped session terminal may start again.
    pub(crate) fn session_terminal_restart_deadline(&self) -> Option<Instant> {
        self.state
            .session_terminal
            .is_none()
            .then_some(self.session_terminal_life.restart_at)
            .flatten()
    }

    /// Starts the session terminal at `cols`×`rows` if it is not running and
    /// may start; resizes it otherwise. A failed one starts only when `retry`.
    pub(crate) fn ensure_session_terminal(
        &mut self,
        cols: u16,
        rows: u16,
        retry: bool,
        now: Instant,
    ) -> std::io::Result<()> {
        if cols == 0 || rows == 0 {
            return Ok(());
        }
        if self.state.session_terminal.is_some() {
            self.resize_session_terminal(cols, rows);
            return Ok(());
        }
        let Some(spec) = self.session_terminal_spec.clone() else {
            return Ok(());
        };
        if retry && self.session_terminal_life.failed {
            self.session_terminal_life.retry();
        }
        if !self.session_terminal_life.may_start(now) {
            return Ok(());
        }
        self.session_terminal_life.restart_at = None;
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        let mut env = spec.env.clone();
        env.push((
            crate::pane::SESSION_TERMINAL_ROLE_ENV_VAR.to_owned(),
            spec.role.clone(),
        ));
        env.push((
            crate::pane::SESSION_TERMINAL_OWNER_ENV_VAR.to_owned(),
            session_terminal_owner(),
        ));
        let launch_env = PaneLaunchEnv::from_extra(env).without_pane_identity();
        let cwd: PathBuf = self.resolve_new_terminal_cwd(None);
        let spawned = self.terminal_factory.spawn(
            pane_id,
            rows,
            cols,
            cwd.clone(),
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.pane_host_terminal_appearance(),
            crate::terminal::TerminalCommand::Argv(
                &spec.argv,
                crate::pane::AgentDetection::Disabled,
            ),
            &launch_env,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        );
        let runtime = match spawned {
            Ok(runtime) => runtime,
            Err(error) => {
                self.session_terminal_life.exited(now);
                return Err(error);
            }
        };
        let terminal = TerminalState::new(terminal_id.clone(), cwd)
            .with_launch_argv(spec.argv.clone())
            .with_runtime(&runtime);
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(terminal_id.clone(), terminal);
        self.state.session_terminal = Some(SessionTerminalState {
            pane_id,
            terminal_id,
            role: spec.role,
            cols,
            rows,
        });
        if let Some(plugin) = spec.replaces_plugin {
            self.notify_replaced_plugin(&plugin);
        }
        Ok(())
    }

    /// Once a session: the plugin this replaces is still enabled.
    fn notify_replaced_plugin(&mut self, plugin: &str) {
        if self.session_terminal_life.replaced_plugin_noted
            || !self
                .state
                .installed_plugins
                .get(plugin)
                .is_some_and(|info| info.enabled)
        {
            return;
        }
        self.session_terminal_life.replaced_plugin_noted = true;
        self.state.toast = Some(crate::app::state::ToastNotification {
            kind: crate::app::state::ToastKind::UpdateInstalled,
            title: format!("{plugin} is built in now"),
            context: format!("turn the plugin off: herdr plugin disable {plugin}"),
            position: None,
            target: None,
        });
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    pub(crate) fn resize_session_terminal(&mut self, cols: u16, rows: u16) {
        let Some(session) = self.state.session_terminal.as_mut() else {
            return;
        };
        if (session.cols, session.rows) == (cols, rows) || cols == 0 || rows == 0 {
            return;
        }
        session.cols = cols;
        session.rows = rows;
        if let Some(runtime) = self.terminal_runtimes.get(&session.terminal_id) {
            runtime.resize(rows, cols, 0, 0);
        }
    }

    pub(crate) fn is_session_terminal_pane(&self, pane_id: PaneId) -> bool {
        self.state
            .session_terminal
            .as_ref()
            .is_some_and(|session| session.pane_id == pane_id)
    }

    /// Its program exited (or its hc terminal finished): gone until it may
    /// start again.
    pub(crate) fn handle_session_terminal_exit(&mut self, now: Instant) {
        let Some(session) = self.state.session_terminal.take() else {
            return;
        };
        self.state.terminals.remove(&session.terminal_id);
        self.shutdown_terminal_runtime(session.terminal_id);
        self.session_terminal_life.exited(now);
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }
}

#[cfg(test)]
impl App {
    /// A running session terminal on a test runtime.
    pub(crate) fn install_test_session_terminal(
        &mut self,
        runtime: crate::terminal::TerminalRuntime,
    ) -> (PaneId, TerminalId) {
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        let (cols, rows) = runtime.current_size();
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id.clone(), PathBuf::from("/files")),
        );
        self.session_terminal_spec = Some(SessionTerminalSpec {
            role: "files".into(),
            argv: vec!["true".into()],
            env: Vec::new(),
            settings: None,
            replaces_plugin: None,
        });
        self.state.session_terminal = Some(SessionTerminalState {
            pane_id,
            terminal_id: terminal_id.clone(),
            role: "files".into(),
            cols,
            rows,
        });
        (pane_id, terminal_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_still_enabled_replaced_plugin_is_noted_once() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let plugin = |enabled: bool| -> crate::api::schema::InstalledPluginInfo {
            serde_json::from_value(serde_json::json!({
                "plugin_id": "old-sidebar", "name": "old", "version": "1",
                "manifest_path": "", "plugin_root": "", "enabled": enabled,
                "source": {"kind": "local", "path": "/x"}
            }))
            .expect("a plugin record")
        };
        // Disabled: nothing to say.
        app.state
            .installed_plugins
            .insert("old-sidebar".into(), plugin(false));
        app.notify_replaced_plugin("old-sidebar");
        assert!(app.state.toast.is_none());
        // Enabled: said once.
        app.state
            .installed_plugins
            .insert("old-sidebar".into(), plugin(true));
        app.notify_replaced_plugin("old-sidebar");
        let toast = app.state.toast.take().expect("a notice");
        assert!(
            toast.context.contains("plugin disable old-sidebar"),
            "{toast:?}"
        );
        app.notify_replaced_plugin("old-sidebar");
        assert!(app.state.toast.is_none());
    }

    #[test]
    fn exits_wait_longer_each_time_and_three_in_a_minute_fail_until_retried() {
        let start = Instant::now();
        let mut life = SessionTerminalLife::default();
        assert!(life.may_start(start));
        life.exited(start);
        assert!(!life.may_start(start));
        assert!(life.may_start(start + Duration::from_secs(1)));
        life.exited(start + Duration::from_secs(1));
        assert!(!life.may_start(start + Duration::from_secs(2)));
        assert!(life.may_start(start + Duration::from_secs(3)));
        life.exited(start + Duration::from_secs(3));
        assert!(life.failed);
        assert!(!life.may_start(start + Duration::from_secs(600)));
        life.retry();
        assert!(life.may_start(start + Duration::from_secs(3)));

        // Exits a minute apart never add up to a failure.
        let mut slow = SessionTerminalLife::default();
        for minute in 0..5 {
            slow.exited(start + Duration::from_secs(61 * minute));
        }
        assert!(!slow.failed);
    }
}
