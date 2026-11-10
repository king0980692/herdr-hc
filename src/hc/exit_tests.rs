//! Only an authoritative lifecycle transition may remove an hc pane.
use super::*;
use crate::terminal::{TerminalFactory, TerminalId};

async fn fixture() -> (
    crate::app::App,
    TerminalId,
    state::RuntimeSnapshot,
    tokio::io::DuplexStream,
) {
    let (client, mut server) = tests::test_client();
    let (runtime, _, _) = runtime_tests::attach(client, &mut server).await;
    let mut app = app_tests::app(TerminalFactory::Unavailable("test source".into()));
    app_tests::restore_into(
        &mut app,
        &app_tests::saved(json!({"core_id":"core-test","address":"t1"})),
    );
    let pane = app.state.workspaces[0].tabs[0].root_pane;
    let id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
    let mut snapshot = runtime.hc_snapshot().unwrap();
    snapshot.terminal.facts.insert("keep".into(), json!(false));
    app.state
        .terminals
        .get_mut(&id)
        .unwrap()
        .apply_hc_snapshot(snapshot.clone());
    app.terminal_runtimes.insert(id.clone(), runtime);
    (app, id, snapshot, server)
}

#[tokio::test]
async fn hc_terminal_finished_closes_only_confirmed_nonkept_live_transitions() {
    for case in [
        "finished",
        "viewer_closed",
        "disconnected",
        "agent_done",
        "lost",
        "kept",
        "unknown_keep",
        "historical_finished",
        "initial_snapshot",
        "wrong_core",
        "stale_runtime",
        "server_stop",
        "host_shutdown",
        "holder_lost",
        "stopped_by_restart",
        "killed",
        "unknown_reason",
    ] {
        let (mut app, id, mut snapshot, _server) = fixture().await;
        let mut token = app.terminal_runtimes.hc_token(&id).unwrap();
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let public = app.public_pane_id(0, pane).unwrap();
        snapshot
            .terminal
            .facts
            .insert("state".into(), json!("finished"));
        snapshot
            .terminal
            .facts
            .insert("reason".into(), json!("exited"));
        snapshot.attached = false;
        match case {
            "viewer_closed" => {
                app.state
                    .terminals
                    .get_mut(&id)
                    .unwrap()
                    .hc_snapshot
                    .as_mut()
                    .unwrap()
                    .attached = false
            }
            "disconnected" => snapshot.connected = false,
            "agent_done" => {
                snapshot
                    .terminal
                    .facts
                    .insert("state".into(), json!("live"));
                snapshot
                    .terminal
                    .facts
                    .insert("reported".into(), json!(true));
                snapshot
                    .terminal
                    .facts
                    .insert("status".into(), json!({"state":"done"}));
                snapshot.attached = true;
            }
            "lost" => {
                snapshot
                    .terminal
                    .facts
                    .insert("state".into(), json!("live"));
                snapshot.terminal.facts.remove("pid");
            }
            "kept" => {
                snapshot.terminal.facts.insert("keep".into(), json!(true));
            }
            "unknown_keep" => {
                snapshot.terminal.facts.remove("keep");
            }
            "historical_finished" => {
                app.state
                    .terminals
                    .get_mut(&id)
                    .unwrap()
                    .hc_snapshot
                    .as_mut()
                    .unwrap()
                    .terminal
                    .facts
                    .insert("state".into(), json!("finished"));
            }
            "initial_snapshot" => app.state.terminals.get_mut(&id).unwrap().hc_snapshot = None,
            "wrong_core" => snapshot.binding.core_id = "replacement-core".into(),
            "stale_runtime" => token = Arc::new(()),
            "server_stop" | "host_shutdown" | "holder_lost" | "stopped_by_restart" | "killed" => {
                snapshot.terminal.facts.insert("reason".into(), json!(case));
            }
            "unknown_reason" => {
                snapshot.terminal.facts.remove("reason");
            }
            _ => {}
        }
        app.apply_hc_update(id.clone(), token, snapshot);
        let closes = matches!(case, "finished" | "viewer_closed");
        assert_eq!(app.state.workspaces.is_empty(), closes, "{case}");
        assert_eq!(!app.state.terminals.contains_key(&id), closes, "{case}");
        assert_eq!(app.terminal_runtimes.get(&id).is_none(), closes, "{case}");
        let exits = app.event_hub.events_after(0).into_iter().filter(|(_, event)| {
            matches!(&event.data, crate::api::schema::EventData::PaneExited { pane_id, .. } if pane_id == &public)
        }).count();
        assert_eq!(exits, usize::from(closes), "{case}");
        app.state.assert_invariants_for_test();
    }
}

#[tokio::test]
async fn hc_terminal_finished_preserves_surviving_pane_and_updates_layout() {
    let (mut app, id, mut snapshot, _server) = fixture().await;
    let survivor = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    let survivor_id = app.state.workspaces[0]
        .terminal_id(survivor)
        .unwrap()
        .clone();
    app.state.terminals.insert(
        survivor_id.clone(),
        crate::terminal::TerminalState::new(survivor_id, "/survivor".into()),
    );
    snapshot
        .terminal
        .facts
        .insert("state".into(), json!("finished"));
    snapshot.attached = false;
    let token = app.terminal_runtimes.hc_token(&id).unwrap();
    snapshot
        .terminal
        .facts
        .insert("reason".into(), json!("exited"));
    app.apply_hc_update(id.clone(), token, snapshot);
    assert_eq!(app.state.workspaces.len(), 1);
    assert_eq!(app.state.workspaces[0].tabs[0].panes.len(), 1);
    assert!(app.state.workspaces[0].tabs[0]
        .panes
        .contains_key(&survivor));
    assert!(!app.state.terminals.contains_key(&id));
    assert!(app
        .event_hub
        .events_after(0)
        .iter()
        .any(|(_, event)| matches!(
            event.data,
            crate::api::schema::EventData::LayoutUpdated { .. }
        )));
    app.state.assert_invariants_for_test();
}

#[tokio::test]
async fn hc_terminal_finished_detaches_popup_without_terminating_a_source_program() {
    let (mut app, id, mut snapshot, _server) = fixture().await;
    let pane = app.state.workspaces[0].tabs[0].root_pane;
    app.state.workspaces.clear();
    app.state.active = None;
    app.state.selected = 0;
    app.state.popup_pane = Some(crate::app::state::PopupPaneState {
        pane_id: pane,
        terminal_id: id.clone(),
        width: None,
        height: None,
    });
    snapshot
        .terminal
        .facts
        .insert("state".into(), json!("finished"));
    snapshot.attached = false;
    let token = app.terminal_runtimes.hc_token(&id).unwrap();
    snapshot
        .terminal
        .facts
        .insert("reason".into(), json!("exited"));
    app.apply_hc_update(id.clone(), token, snapshot);
    assert!(app.state.popup_pane.is_none());
    assert!(!app.state.terminals.contains_key(&id));
    assert!(app.terminal_runtimes.get(&id).is_none());
    assert_eq!(app.state.mode, crate::app::Mode::Navigate);
    app.state.assert_invariants_for_test();
}
