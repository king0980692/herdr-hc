//! The files dock: the active endpoint's session terminal (its sidebar),
//! shown on the right of every tab. The endpoint runs it at the size this
//! shell reports; the shell clips or pads what it receives to its own rect.

use ratatui::layout::Rect;

use super::*;
use crate::protocol::session_terminal::{SessionTerminalStatus, SessionTerminalView};

pub(super) const DEFAULT_WIDTH: u16 = 32;
pub(super) const MIN_WIDTH: u16 = 24;
pub(super) const MAX_WIDTH: u16 = 80;
/// The tabs keep at least this many columns; a narrower window drops the dock.
const MIN_TABS_WIDTH: u16 = 20;

/// Splits `main` into the tabs' part, the dock and the column between.
pub(super) fn carve(main: Rect, dock_width: u16) -> (Rect, Rect, Rect) {
    let dock_width = if dock_width == 0 {
        0
    } else {
        dock_width.clamp(MIN_WIDTH, MAX_WIDTH)
    };
    let taken = dock_width.saturating_add(1);
    if dock_width == 0 || main.width < taken.saturating_add(MIN_TABS_WIDTH) {
        return (main, Rect::default(), Rect::default());
    }
    let tabs = Rect::new(main.x, main.y, main.width - taken, main.height);
    let separator = Rect::new(tabs.right(), main.y, 1, main.height);
    let dock = Rect::new(separator.right(), main.y, dock_width, main.height);
    (tabs, dock, separator)
}

impl ClientShellState {
    fn active_endpoint_has_dock(&self) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .is_some_and(|endpoint| endpoint.session_terminal_supported)
    }

    /// The dock's width in the layout: 0 when not shown.
    pub(super) fn shown_dock_width(&self) -> u16 {
        if self.dock_visible && self.active_endpoint_has_dock() {
            self.dock_width
        } else {
            0
        }
    }

    /// The view to report when it changed since last reported: to the active
    /// endpoint, and "hidden" to one that stopped being active.
    pub(crate) fn take_session_terminal_views(
        &mut self,
        cols: u16,
        rows: u16,
    ) -> Vec<(ClientEndpointId, crate::protocol::ClientMessage)> {
        let mut messages = Vec::new();
        let current = self.active_endpoint_has_dock().then(|| {
            let dock = self.layout(cols, rows).dock;
            (
                self.active_endpoint_id.clone(),
                SessionTerminalView {
                    visible: !dock.is_empty(),
                    cols: dock.width,
                    rows: dock.height,
                    focused: self.dock_focused && !dock.is_empty(),
                },
            )
        });
        if current == self.last_dock_view {
            return messages;
        }
        if let Some((previous, view)) = self.last_dock_view.take() {
            if current
                .as_ref()
                .is_none_or(|(endpoint_id, _)| endpoint_id != &previous)
                && view.visible
            {
                messages.push((
                    previous,
                    crate::protocol::session_terminal::view_message(SessionTerminalView::default()),
                ));
            }
        }
        if let Some((endpoint_id, view)) = &current {
            messages.push((
                endpoint_id.clone(),
                crate::protocol::session_terminal::view_message(*view),
            ));
        }
        self.last_dock_view = current;
        messages
    }

    /// The active endpoint's session terminal, as received.
    fn session_terminal(
        &self,
    ) -> Option<&crate::protocol::session_terminal::SessionTerminalUpdate> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)?
            .session_terminal
            .as_deref()
    }

    /// Draws the dock into `frame`; where it is, for the hit map.
    pub(super) fn render_dock(
        &self,
        frame: &mut crate::protocol::FrameData,
        layout: &ClientShellLayout,
    ) -> (Option<PaneHit>, Rect) {
        if layout.dock.is_empty() {
            return (None, Rect::default());
        }
        let separator_style = Style::default().fg(if self.dock_focused {
            self.config.palette.accent
        } else {
            self.config.palette.surface_dim
        });
        let mut chrome = Buffer::empty(Rect::new(0, 0, frame.width, frame.height));
        for y in layout.dock_separator.top()..layout.dock_separator.bottom() {
            chrome[(layout.dock_separator.x, y)]
                .set_symbol("│")
                .set_style(separator_style);
        }
        let update = self.session_terminal().cloned();
        let placeholder = match update.as_ref() {
            Some(update) if update.frame.is_some() => None,
            Some(update) if update.status == SessionTerminalStatus::Failed => {
                Some(" Files stopped. Click to restart.")
            }
            _ => Some(" Files starting…"),
        };
        if let Some(text) = placeholder {
            ratatui::widgets::Widget::render(
                ratatui::widgets::Paragraph::new(text)
                    .style(Style::default().fg(self.config.palette.overlay0)),
                layout.dock,
                &mut chrome,
            );
        }
        let cursor = frame.cursor.clone();
        let separator = FrameData::from_ratatui_buffer_with_hyperlinks(
            &buffer_part(&chrome, layout.dock_separator),
            None,
            &[],
        );
        blit_pane_surface(frame, &separator, layout.dock_separator);
        frame.cursor = cursor.clone();
        match update.as_ref().and_then(|update| update.frame.as_ref()) {
            Some(dock_frame) => {
                // A frame sized for another shell: padded with blanks first.
                let blank = FrameData::from_ratatui_buffer_with_hyperlinks(
                    &Buffer::empty(Rect::new(0, 0, layout.dock.width, layout.dock.height)),
                    None,
                    &[],
                );
                blit_pane_surface(frame, &blank, layout.dock);
                blit_pane_surface(frame, dock_frame, layout.dock);
                // The cursor is the dock's only while keys go there.
                if !self.dock_focused {
                    frame.cursor = cursor;
                }
            }
            None => {
                let text = FrameData::from_ratatui_buffer_with_hyperlinks(
                    &buffer_part(&chrome, layout.dock),
                    None,
                    &[],
                );
                blit_pane_surface(frame, &text, layout.dock);
                frame.cursor = cursor;
            }
        }
        let hit = PaneHit {
            rect: layout.dock,
            inner_rect: layout.dock,
            scrollbar_rect: None,
            scroll: None,
            pane_id: update
                .as_ref()
                .map(|update| update.terminal_id.clone())
                .unwrap_or_default(),
            popup: true,
            mouse_reporting: update.as_ref().is_some_and(|update| update.mouse_reporting),
            sgr_pixel_mouse: update.as_ref().is_some_and(|update| update.sgr_pixel_mouse),
            pixel_width: 0,
            pixel_height: 0,
        };
        (Some(hit), layout.dock_separator)
    }

    /// A mouse event over the dock goes to its program, and a click gives it
    /// the keys; a click anywhere else takes them back.
    pub(super) fn handle_dock_mouse(
        &mut self,
        point: (u16, u16),
        mouse: crossterm::event::MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use crossterm::event::MouseEventKind;
        let Some(hit) = self.hits.dock.clone() else {
            return false;
        };
        let inside = super::contains(hit.rect, point);
        if !inside {
            return false;
        }
        // A dock that stopped: a click asks for it again.
        if hit.pane_id.is_empty() {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.last_dock_view = None;
            }
            return true;
        }
        match mouse.kind {
            MouseEventKind::Down(button) => {
                self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome);
                if hit.mouse_reporting {
                    self.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                        last_position: self.pane_mouse_position(&hit, mouse),
                        hit,
                        button,
                        stripped_modifiers: crossterm::event::KeyModifiers::empty(),
                        last_event: mouse,
                    });
                }
            }
            MouseEventKind::Moved if hit.mouse_reporting => {
                self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome);
            }
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {
                self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome);
            }
            MouseEventKind::Up(_) | MouseEventKind::Drag(_) | MouseEventKind::Moved => {}
        }
        true
    }

    /// The terminal keys go to while the dock has them.
    pub(super) fn dock_input_terminal(&self) -> Option<String> {
        if !self.dock_focused {
            return None;
        }
        self.hits
            .dock
            .as_ref()
            .map(|hit| hit.pane_id.clone())
            .filter(|terminal_id| !terminal_id.is_empty())
    }
}

/// `area` of `buffer`, as its own buffer.
fn buffer_part(buffer: &Buffer, area: Rect) -> Buffer {
    let mut part = Buffer::empty(Rect::new(0, 0, area.width, area.height));
    for y in 0..area.height {
        for x in 0..area.width {
            part[(x, y)] = buffer[(area.x + x, area.y + y)].clone();
        }
    }
    part
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dock_takes_the_right_edge_only_while_the_tabs_keep_room() {
        let main = Rect::new(30, 0, 120, 40);
        let (tabs, dock, separator) = carve(main, 32);
        assert_eq!(tabs, Rect::new(30, 0, 87, 40));
        assert_eq!(separator, Rect::new(117, 0, 1, 40));
        assert_eq!(dock, Rect::new(118, 0, 32, 40));
        assert_eq!(dock.right(), main.right());

        // Too narrow for both: no dock.
        let (tabs, dock, _) = carve(Rect::new(30, 0, 50, 40), 32);
        assert_eq!(tabs, Rect::new(30, 0, 50, 40));
        assert!(dock.is_empty());
        // Hidden: no dock; widths are kept within bounds.
        assert!(carve(main, 0).1.is_empty());
        assert_eq!(carve(main, 5).1.width, MIN_WIDTH);
        assert_eq!(carve(Rect::new(0, 0, 300, 40), 200).1.width, MAX_WIDTH);
    }
}
