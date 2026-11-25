use super::*;
use crate::protocol::session_terminal::{
    decode_update, SessionTerminalStatus, SessionTerminalView, UPDATE_KIND,
};

fn session_updates(
    receiver: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> Vec<crate::protocol::session_terminal::SessionTerminalUpdate> {
    let mut updates = Vec::new();
    // The writer hands messages on from its own thread.
    while let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(200)) {
        if let ServerMessage::EndpointControl { kind, data } = read_server_message(bytes) {
            if kind == UPDATE_KIND {
                updates.push(decode_update(&data).expect("a session terminal update"));
            }
        }
    }
    updates
}

fn shown(cols: u16, rows: u16) -> SessionTerminalView {
    SessionTerminalView {
        visible: true,
        cols,
        rows,
        focused: false,
    }
}

#[tokio::test]
async fn session_terminal_reaches_only_the_shells_showing_it_and_never_redraws_tabs() {
    let (mut server, control_rx, render_rx, _pane_id) =
        retained_test_server_with_control(b"tab text");
    let (session_pane, _) = server.app.install_test_session_terminal(
        crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 6, b"dock text"),
    );
    server.render_and_stream();
    while render_rx.try_recv().is_ok() {}
    // Not asked for: nothing.
    server.stream_session_terminal();
    assert!(session_updates(&control_rx).is_empty());

    // Shown: the whole screen, at the shell's size.
    server.set_session_terminal_view(1, shown(20, 6));
    let updates = session_updates(&control_rx);
    let [update] = &updates[..] else {
        panic!("one update: {updates:?}");
    };
    assert_eq!(update.status, SessionTerminalStatus::Running);
    assert_eq!(update.role, "files");
    let frame = update.frame.as_ref().expect("a screen");
    assert_eq!((frame.width, frame.height), (20, 6));
    assert!(frame_text(frame).contains("dock text"));

    // Nothing new: nothing sent.
    server.stream_session_terminal();
    assert!(session_updates(&control_rx).is_empty());

    // Its output: one update, and no tab surface for it.
    server
        .app
        .terminal_runtimes
        .get(
            &server
                .app
                .state
                .session_terminal
                .as_ref()
                .unwrap()
                .terminal_id,
        )
        .unwrap()
        .test_process_pty_bytes(b"\r\nmore");
    let _ = session_pane;
    server.stream_session_terminal();
    let updates = session_updates(&control_rx);
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert!(frame_text(updates[0].frame.as_ref().unwrap()).contains("more"));
    assert!(
        render_rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "the session terminal's output must not redraw the tab"
    );

    // Hidden again: nothing more.
    server.set_session_terminal_view(
        1,
        SessionTerminalView {
            visible: false,
            ..shown(20, 6)
        },
    );
    server
        .app
        .terminal_runtimes
        .get(
            &server
                .app
                .state
                .session_terminal
                .as_ref()
                .unwrap()
                .terminal_id,
        )
        .unwrap()
        .test_process_pty_bytes(b"\r\nhidden");
    server.stream_session_terminal();
    assert!(session_updates(&control_rx).is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn an_exited_session_terminal_reports_its_status_to_the_shells_showing_it() {
    let (mut server, control_rx, _render_rx, _pane_id) =
        retained_test_server_with_control(b"tab text");
    server.app.install_test_session_terminal(
        crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 6, b"dock"),
    );
    server.set_session_terminal_view(1, shown(20, 6));
    session_updates(&control_rx);

    server
        .app
        .handle_session_terminal_exit(std::time::Instant::now());
    server.stream_session_terminal();
    let updates = session_updates(&control_rx);
    let [update] = &updates[..] else {
        panic!("one update: {updates:?}");
    };
    assert_eq!(update.status, SessionTerminalStatus::Restarting);
    assert!(update.frame.is_none());
    assert!(server.app.session_terminal_restart_deadline().is_some());
    assert!(server.app.state.session_terminal.is_none());
    assert!(server
        .app
        .state
        .terminals
        .values()
        .all(|terminal| terminal.cwd != std::path::Path::new("/files")));
    shutdown_test_runtimes(&mut server);
}
