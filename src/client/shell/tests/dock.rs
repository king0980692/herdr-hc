use super::*;
use crate::protocol::session_terminal::{SessionTerminalStatus, SessionTerminalUpdate};

fn dock_update(text: &str) -> SessionTerminalUpdate {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 32, 20));
    buffer.set_string(0, 0, text, Style::default());
    SessionTerminalUpdate {
        terminal_id: "term_dock".into(),
        role: "files".into(),
        status: SessionTerminalStatus::Running,
        mouse_reporting: true,
        sgr_pixel_mouse: false,
        frame: Some(FrameData::from_ratatui_buffer(&buffer, None)),
    }
}

fn frame_lines(frame: &FrameData) -> Vec<String> {
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol.as_str()).collect())
        .collect()
}

fn click(state: &mut ClientShellState, column: u16, row: u16) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })])
}

#[test]
fn the_files_dock_shows_on_the_right_takes_its_clicks_and_keys_and_gives_them_back() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let full = state.surface_size(150, 20);
    // An endpoint without one: no dock, nothing to report.
    state.compose(150, 20).expect("frame");
    assert!(state.hits.dock.is_none());
    assert!(state.take_session_terminal_views(150, 20).is_empty());

    state.set_endpoint_session_terminal_supported(&ClientEndpointId::Local, true);
    let narrower = state.surface_size(150, 20);
    assert!(narrower.cols < full.cols, "the tabs give the dock room");
    // Shown: reported once, at the dock's size.
    let views = state.take_session_terminal_views(150, 20);
    let [(endpoint, ClientMessage::EndpointControl { kind, data })] = &views[..] else {
        panic!("one view: {views:?}");
    };
    assert_eq!(endpoint, &ClientEndpointId::Local);
    assert_eq!(kind, crate::protocol::session_terminal::VIEW_KIND);
    let view = crate::protocol::session_terminal::decode_view(data).unwrap();
    assert!(view.visible && !view.focused);
    assert_eq!((view.cols, view.rows), (32, 20));
    assert!(state.take_session_terminal_views(150, 20).is_empty());

    // Not running yet: a placeholder.
    let frame = state.compose(150, 20).expect("frame");
    assert!(frame_lines(&frame)
        .iter()
        .any(|line| line.contains("Files starting")));

    assert!(state.set_endpoint_session_terminal(&ClientEndpointId::Local, dock_update("my files")));
    let frame = state.compose(150, 20).expect("frame");
    let dock = state.hits.dock.clone().expect("dock hit");
    assert_eq!(dock.rect.right(), 150);
    assert_eq!(dock.rect.width, 32);
    assert!(frame_lines(&frame)[0].ends_with(&format!("{:<32}", "my files")));

    // A click in the dock: its program gets it, and the keys from then on.
    let input = click(&mut state, dock.rect.x + 2, dock.rect.y + 3);
    assert!(state.dock_focused);
    assert!(matches!(
        &input.requests[..],
        [ClientMessage::ClientShellPopupInput { terminal_id, .. }] if terminal_id == "term_dock"
    ));
    let views = state.take_session_terminal_views(150, 20);
    assert!(views.iter().any(|(_, message)| matches!(message,
        ClientMessage::EndpointControl { data, .. }
            if crate::protocol::session_terminal::decode_view(data).unwrap().focused)));
    let keys = state.handle_input_bytes(b"x");
    assert!(matches!(
        &keys.requests[..],
        [ClientMessage::ClientShellPopupInput { terminal_id, .. }] if terminal_id == "term_dock"
    ));

    // A click in a pane takes the keys back.
    state.compose(150, 20).expect("frame");
    let pane = state.hits.panes[0].clone();
    click(&mut state, pane.inner_rect.x, pane.inner_rect.y);
    assert!(!state.dock_focused);
    let keys = state.handle_input_bytes(b"x");
    assert!(!keys
        .requests
        .iter()
        .any(|request| matches!(request, ClientMessage::ClientShellPopupInput { .. })));
}

#[test]
fn a_window_too_narrow_for_tabs_and_dock_drops_the_dock() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.set_endpoint_session_terminal_supported(&ClientEndpointId::Local, true);
    state.compose(70, 20).expect("frame");
    assert!(state.hits.dock.is_none());
    let views = state.take_session_terminal_views(70, 20);
    assert!(views.iter().all(|(_, message)| matches!(message,
        ClientMessage::EndpointControl { data, .. }
            if !crate::protocol::session_terminal::decode_view(data).unwrap().visible)));
}
