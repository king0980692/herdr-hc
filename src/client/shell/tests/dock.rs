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
    let paste = state.handle_raw_events(vec![RawInputEvent::Paste("a.txt".into())]);
    assert!(matches!(
        &paste.requests[..],
        [ClientMessage::ClientShellPopupInput { terminal_id, .. }] if terminal_id == "term_dock"
    ));
    // The prefix is still herdr's: prefix+shift+f gives the keys back.
    let prefix = state.handle_input_bytes(b"\x02");
    assert!(prefix.requests.is_empty());
    state.handle_input_bytes(b"F");
    assert!(!state.dock_focused, "prefix+shift+f gives the keys back");
    let mut back = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::FocusDock),
        &mut back,
    );
    assert!(state.dock_focused, "and takes them again");

    // Hidden and shown again by its shortcut; hidden, it has no keys.
    let mut toggle = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::ToggleDock),
        &mut toggle,
    );
    assert!(!state.dock_visible && !state.dock_focused);
    // The endpoint sends the tabs again at their new size.
    state.set_pane_surface(surface());
    state.compose(150, 20).expect("frame");
    assert!(state.hits.dock.is_none());
    assert_eq!(
        state.surface_size(150, 20),
        full,
        "the tabs take the room back"
    );
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::ToggleDock),
        &mut toggle,
    );
    assert!(state.dock_visible);

    // A click in a pane takes the keys back.
    state.set_pane_surface(surface());
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

#[test]
fn dragging_the_separator_resizes_the_dock_within_bounds() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.set_endpoint_session_terminal_supported(&ClientEndpointId::Local, true);
    state.compose(150, 20).expect("frame");
    let separator = state.hits.dock_separator;
    assert_eq!(separator.width, 1);
    let mouse = |kind, column| {
        RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row: 5,
            modifiers: KeyModifiers::empty(),
        })
    };
    state.handle_raw_events(vec![
        mouse(MouseEventKind::Down(MouseButton::Left), separator.x),
        mouse(MouseEventKind::Drag(MouseButton::Left), 99),
        mouse(MouseEventKind::Up(MouseButton::Left), 99),
    ]);
    assert_eq!(state.dock_width, 50, "150 - 99 - 1");
    state.set_pane_surface(surface());
    state.compose(150, 20).expect("frame");
    assert_eq!(state.hits.dock.as_ref().unwrap().rect.width, 50);
    // Never past the bounds.
    let separator = state.hits.dock_separator;
    state.handle_raw_events(vec![
        mouse(MouseEventKind::Down(MouseButton::Left), separator.x),
        mouse(MouseEventKind::Drag(MouseButton::Left), 145),
        mouse(MouseEventKind::Up(MouseButton::Left), 145),
    ]);
    assert_eq!(state.dock_width, super::super::dock::MIN_WIDTH);
}

fn files_settings_result(follow: bool, editor: &str) -> crate::api::schema::ResponseResult {
    use crate::api::schema::{ComponentSetting, ComponentSettingKind, SettingsComponent};
    crate::api::schema::ResponseResult::ComponentSettings {
        components: vec![SettingsComponent {
            id: "files".into(),
            title: "Files".into(),
            settings: vec![
                ComponentSetting {
                    key: "follow_cwd".into(),
                    label: "Follow the pane's folder".into(),
                    help: None,
                    kind: ComponentSettingKind::Toggle,
                    value: serde_json::Value::Bool(follow),
                    enabled: true,
                },
                ComponentSetting {
                    key: "color_theme".into(),
                    label: "Color theme".into(),
                    help: None,
                    kind: ComponentSettingKind::Choice {
                        options: vec!["vscode".into(), "light".into()],
                    },
                    value: serde_json::Value::String("vscode".into()),
                    enabled: true,
                },
                ComponentSetting {
                    key: "custom_editor".into(),
                    label: "Custom editor".into(),
                    help: None,
                    kind: ComponentSettingKind::Text,
                    value: serde_json::Value::String(editor.into()),
                    enabled: true,
                },
            ],
        }],
    }
}

fn the_set(input: &ClientShellInput) -> (String, crate::api::schema::ComponentSettingSetParams) {
    let [ClientShellAction::Endpoint { request, .. }] = &input.actions[..] else {
        panic!("one endpoint request: {:?}", input.actions.len());
    };
    let crate::api::schema::Method::ComponentSettingsSet(params) = &request.method else {
        panic!("a component setting change");
    };
    (request.id.clone(), params.clone())
}

#[test]
fn the_files_settings_section_changes_the_dock_and_the_endpoints_settings() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    // Without the endpoint method: no Files section.
    state.open_settings_overlay();
    let Some(ClientShellOverlay::Settings(settings)) = state.overlay.as_ref() else {
        panic!("settings");
    };
    assert!(!settings.sections().contains(&ClientSettingsSection::Files));
    state.cancel_settings_overlay();

    state.set_endpoint_methods(Some(vec![
        "component_settings.list".into(),
        "component_settings.set".into(),
    ]));
    state.open_settings_overlay();
    state.compose(150, 30).expect("settings");
    for _ in 0..4 {
        state.handle_input_bytes(b"\t");
    }
    let files = state.handle_input_bytes(b"\t");
    let [ClientShellAction::Endpoint { request, .. }] = &files.actions[..] else {
        panic!("the Files section asks for the settings");
    };
    assert!(matches!(
        request.method,
        crate::api::schema::Method::ComponentSettingsList(_)
    ));
    let request_id = request.id.clone();
    assert!(
        state
            .handle_endpoint_result("boot-1", &request_id, Ok(files_settings_result(true, "")))
            .0
    );
    let frame = state.compose(150, 30).expect("files section");
    let text = frame_lines(&frame).join("\n");
    for expected in [
        "Show the files dock",
        "Dock width",
        "Follow the pane's folder",
        "Color theme",
        "not set",
    ] {
        assert!(text.contains(expected), "{expected:?} in\n{text}");
    }

    // The dock's own rows change this client.
    let width = state.dock_width;
    state.handle_input_bytes(b"j");
    state.handle_input_bytes(b"\r");
    assert!(state.dock_width > width);

    // A toggle: the endpoint is asked to flip it.
    state.handle_input_bytes(b"j");
    let (id, params) = the_set(&state.handle_input_bytes(b"\r"));
    assert_eq!(
        (params.component.as_str(), params.key.as_str()),
        ("files", "follow_cwd")
    );
    assert_eq!(params.value, serde_json::Value::Bool(false));
    state.handle_endpoint_result("boot-1", &id, Ok(files_settings_result(false, "")));

    // A choice: the next option.
    state.handle_input_bytes(b"j");
    let (id, params) = the_set(&state.handle_input_bytes(b"\r"));
    assert_eq!(params.value, serde_json::Value::String("light".into()));
    state.handle_endpoint_result("boot-1", &id, Ok(files_settings_result(false, "")));

    // Text: typed in place, saved by Enter.
    state.handle_input_bytes(b"j");
    let edit = state.handle_input_bytes(b"\r");
    assert!(edit.actions.is_empty());
    state.handle_input_bytes(b"nvim");
    let (_, params) = the_set(&state.handle_input_bytes(b"\r"));
    assert_eq!(params.key, "custom_editor");
    assert_eq!(params.value, serde_json::Value::String("nvim".into()));
}
