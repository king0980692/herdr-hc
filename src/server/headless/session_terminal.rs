//! The session terminal for client-owned shells: each shell says how it
//! shows it (`endpoint.session-terminal.view.v1`); the server runs it at the
//! size of the shell in front, and sends each new screen once, encoded once,
//! to every shell showing it. Its output never re-renders a tab surface.

use std::time::Instant;

use ratatui::layout::Rect;
use tracing::warn;

use super::{
    apply_client_terminal_input_events, client_pane_input_has_interaction, ClientConnection,
    ClientShellInputTarget, HeadlessServer,
};
use crate::protocol::session_terminal::{
    self, SessionTerminalStatus, SessionTerminalUpdate, SessionTerminalView,
};
use crate::protocol::ClientPaneInputEvent;

/// What a shell last received of the session terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionTerminalSent {
    terminal_id: String,
    content_seq: u64,
    cols: u16,
    rows: u16,
    status: Option<SessionTerminalStatus>,
    mouse: (bool, bool),
}

impl HeadlessServer {
    fn session_terminal_viewers(&self) -> impl Iterator<Item = (u64, &ClientConnection)> {
        self.clients.iter().filter_map(|(&id, client)| {
            (client.is_active_shell_client()
                && client.writer.is_some()
                && client
                    .session_terminal_view
                    .is_some_and(|view| view.visible))
            .then_some((id, client))
        })
    }

    /// Whether any shell shows the session terminal now.
    pub(super) fn session_terminal_shown(&self) -> bool {
        self.session_terminal_viewers().next().is_some()
    }

    /// The size the session terminal runs at: the shell in front, else the
    /// one used last, among those showing it.
    fn session_terminal_size(&self) -> Option<(u16, u16)> {
        let view = |client: &ClientConnection| {
            client
                .session_terminal_view
                .filter(|view| view.visible && view.cols > 0 && view.rows > 0)
        };
        self.foreground_client_id
            .and_then(|id| self.clients.get(&id))
            .and_then(view)
            .or_else(|| {
                self.session_terminal_viewers()
                    .max_by_key(|(_, client)| client.last_activity)
                    .and_then(|(_, client)| view(client))
            })
            .map(|view| (view.cols, view.rows))
    }

    pub(super) fn set_session_terminal_view(&mut self, client_id: u64, view: SessionTerminalView) {
        if !self.app.session_terminal_available() {
            return;
        }
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        let asked_again = view.visible
            && client
                .session_terminal_view
                .is_none_or(|before| !before.visible);
        client.session_terminal_view = Some(view);
        // A shell showing it anew gets the whole screen.
        client.session_terminal_sent = None;
        self.maintain_session_terminal(Instant::now(), asked_again);
        self.stream_session_terminal();
    }

    /// Starts, restarts or resizes the session terminal for the shells
    /// showing it. `retry`: one asked again, so a failed one starts again.
    pub(super) fn maintain_session_terminal(&mut self, now: Instant, retry: bool) {
        let Some((cols, rows)) = self.session_terminal_size() else {
            return;
        };
        if let Err(error) = self.app.ensure_session_terminal(cols, rows, retry, now) {
            warn!(%error, "the session terminal could not start");
        }
    }

    /// The session terminal's pane, if it is running and shown: its output
    /// is presentation work of its own.
    pub(super) fn shown_session_terminal_pane(&self) -> Option<crate::layout::PaneId> {
        let session = self.app.state.session_terminal.as_ref()?;
        self.session_terminal_shown().then_some(session.pane_id)
    }

    /// What the shells showing the session terminal should have now. Cheap:
    /// no rendering.
    fn session_terminal_key(&self) -> Option<SessionTerminalSent> {
        let status = self.app.session_terminal_status();
        let Some(session) = self.app.state.session_terminal.as_ref() else {
            return Some(SessionTerminalSent {
                terminal_id: String::new(),
                content_seq: 0,
                cols: 0,
                rows: 0,
                status: Some(status.unwrap_or(SessionTerminalStatus::Restarting)),
                mouse: (false, false),
            });
        };
        let runtime = self.app.terminal_runtimes.get(&session.terminal_id)?;
        // Mid-frame: wait for the program to finish drawing.
        if runtime.synchronized_output_active() {
            return None;
        }
        Some(SessionTerminalSent {
            terminal_id: session.terminal_id.to_string(),
            content_seq: runtime.content_seq(),
            cols: session.cols,
            rows: session.rows,
            status,
            mouse: (
                runtime.mouse_reporting_enabled(),
                runtime.sgr_pixel_mouse_enabled(),
            ),
        })
    }

    fn session_terminal_update(&self, key: &SessionTerminalSent) -> SessionTerminalUpdate {
        let running = self
            .app
            .state
            .session_terminal
            .as_ref()
            .and_then(|session| {
                let runtime = self.app.terminal_runtimes.get(&session.terminal_id)?;
                let area = Rect::new(0, 0, session.cols, session.rows);
                let (buffer, cursor) =
                    crate::server::render_stream::render_terminal_virtual(runtime, area);
                let hyperlinks = runtime.visible_hyperlinks(area);
                Some((
                    session.role.clone(),
                    crate::protocol::FrameData::from_ratatui_buffer_with_hyperlinks(
                        &buffer,
                        cursor,
                        &hyperlinks,
                    ),
                ))
            });
        let (role, frame) =
            running.map_or((String::new(), None), |(role, frame)| (role, Some(frame)));
        SessionTerminalUpdate {
            terminal_id: key.terminal_id.clone(),
            role,
            status: key.status.unwrap_or(SessionTerminalStatus::Restarting),
            mouse_reporting: key.mouse.0,
            sgr_pixel_mouse: key.mouse.1,
            frame,
        }
    }

    /// Sends the session terminal's screen to every shell showing it that
    /// does not have it yet: rendered and encoded once, and only then.
    pub(super) fn stream_session_terminal(&mut self) {
        if !self.session_terminal_shown() {
            return;
        }
        let Some(key) = self.session_terminal_key() else {
            return;
        };
        let due: Vec<u64> = self
            .session_terminal_viewers()
            .filter(|(_, client)| client.session_terminal_sent.as_ref() != Some(&key))
            .map(|(id, _)| id)
            .collect();
        if due.is_empty() {
            return;
        }
        let update = self.session_terminal_update(&key);
        let framed = match session_terminal::update_message(&update)
            .map_err(std::io::Error::other)
            .and_then(|message| Self::frame_server_message(&message).map_err(std::io::Error::other))
        {
            Ok(framed) => framed,
            Err(error) => {
                warn!(%error, "the session terminal update could not be encoded");
                return;
            }
        };
        let mut gone = Vec::new();
        for id in due {
            let Some(client) = self.clients.get_mut(&id) else {
                continue;
            };
            let sent = client
                .writer
                .as_ref()
                .is_some_and(|writer| writer.control.send(framed.clone()).is_ok());
            if sent {
                client.session_terminal_sent = Some(key.clone());
            } else {
                gone.push(id);
            }
        }
        for id in gone {
            self.remove_client_and_resize_if_needed(id);
        }
    }

    /// Input a shell sent to the session terminal.
    pub(super) fn session_terminal_input(
        &mut self,
        client_id: u64,
        terminal_id: &str,
        mut events: Vec<ClientPaneInputEvent>,
    ) -> bool {
        let pixel_mouse = self.clients.get(&client_id).is_some_and(|client| {
            client.pixel_mouse && client.host_sgr_pixels_active == Some(true)
        });
        let Some(session) = self.app.state.session_terminal.as_ref() else {
            return false;
        };
        let (session_terminal_id, pane_id) = (session.terminal_id.clone(), session.pane_id);
        let Some(runtime) = self.app.terminal_runtimes.get(&session_terminal_id) else {
            return false;
        };
        super::super::pane_input::downgrade_ineligible_pixel_mouse(
            &mut events,
            pixel_mouse,
            runtime.current_size(),
            runtime.pixel_size(),
        );
        let interaction = client_pane_input_has_interaction(&events);
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.track_shell_input(
                ClientShellInputTarget::SessionTerminal(terminal_id.to_owned()),
                &events,
            );
        }
        let foreground_changed = interaction && self.promote_client_to_foreground(client_id);
        let Some(runtime) = self.app.terminal_runtimes.get(&session_terminal_id) else {
            return foreground_changed;
        };
        let mut keyboard_forwarded = false;
        if let Err(error) =
            apply_client_terminal_input_events(runtime, &events, false, &mut keyboard_forwarded)
        {
            warn!(client_id, terminal_id, %error, "session terminal input failed");
        }
        if keyboard_forwarded {
            self.note_interactive_input(pane_id, Instant::now());
        }
        // The shell in front sizes it.
        if foreground_changed {
            self.maintain_session_terminal(Instant::now(), false);
        }
        false
    }
}
