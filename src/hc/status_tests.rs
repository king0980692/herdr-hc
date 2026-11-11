//! Source facts drive both status APIs and notifications, never legacy completion inference.
use super::*;
use crate::terminal::TerminalFactory;

#[tokio::test]
async fn hc_border_titles_preserve_original_metadata_and_label_precedence() {
    let (client, mut server) = tests::test_client();
    let (runtime, _, _) = runtime_tests::attach(client, &mut server).await;
    let mut snapshot = runtime.hc_snapshot().unwrap();
    snapshot
        .terminal
        .facts
        .insert("title".into(), json!("shell host: ~"));
    let mut terminal =
        crate::terminal::TerminalState::new(crate::terminal::TerminalId::alloc(), "/tmp".into());
    terminal.apply_hc_snapshot(snapshot.clone());
    assert_eq!(
        terminal.terminal_title_stripped().as_deref(),
        Some("shell host: ~")
    );
    assert_eq!(terminal.effective_title(), None);
    assert_eq!(terminal.border_label(false), None);
    assert_eq!(terminal.border_label(true), None);

    snapshot
        .terminal
        .facts
        .insert("reported".into(), json!(true));
    for title in [None, Some(""), Some("Reviewing patch")] {
        let mut status = json!({"state":"working", "app":"pi"});
        if let Some(title) = title {
            status["title"] = json!(title);
        }
        snapshot.terminal.facts.insert("status".into(), status);
        terminal.apply_hc_snapshot(snapshot.clone());
        let explicit_title = title.filter(|value| !value.is_empty());
        assert_eq!(terminal.effective_title().as_deref(), explicit_title);
        assert_eq!(terminal.border_label(false).as_deref(), explicit_title);
        assert_eq!(
            terminal.border_label(true).as_deref(),
            explicit_title.or(Some("pi"))
        );
        terminal.set_manual_label("reviewer".into());
        for show_agent in [false, true] {
            assert_eq!(
                terminal.border_label(show_agent).as_deref(),
                explicit_title.or(Some("reviewer"))
            );
        }
        terminal.set_manual_label(String::new());
        assert_eq!(
            terminal.terminal_title_stripped().as_deref(),
            Some("shell host: ~")
        );
    }
}

#[tokio::test]
async fn hc_status_events_match_queries_and_notify_only_live_explicit_transitions() {
    use crate::api::schema::{AgentStatus, EventData};
    let (client, mut server) = tests::test_client();
    let (runtime, _, _) = runtime_tests::attach(client, &mut server).await;
    let mut app = app_tests::app(TerminalFactory::Unavailable("test source".into()));
    app_tests::restore_into(
        &mut app,
        &app_tests::saved(json!({"core_id":"core-test","address":"t1"})),
    );
    let pane = app.state.workspaces[0].tabs[0].root_pane;
    let id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
    let base = runtime.hc_snapshot().unwrap();
    app.state
        .terminals
        .get_mut(&id)
        .unwrap()
        .apply_hc_snapshot(base.clone());
    app.terminal_runtimes.insert(id.clone(), runtime);
    let token = app.terminal_runtimes.hc_token(&id).unwrap();
    let public = app.public_pane_id(0, pane).unwrap();
    for (state, connected, expected, suppressed) in [
        ("working", true, AgentStatus::Working, true),
        ("blocked", true, AgentStatus::Blocked, false),
        ("done", true, AgentStatus::Done, false),
        ("idle", true, AgentStatus::Idle, true),
        ("working", true, AgentStatus::Working, true),
        ("blocked", false, AgentStatus::Unknown, true),
        ("blocked", true, AgentStatus::Blocked, true),
    ] {
        let mut snapshot = base.clone();
        snapshot.connected = connected;
        snapshot
            .terminal
            .facts
            .insert("reported".into(), json!(true));
        snapshot
            .terminal
            .facts
            .insert("status".into(), json!({"state":state,"app":"pi"}));
        let (changed, updates) = app.apply_hc_update(id.clone(), token.clone(), snapshot);
        assert!(changed);
        assert_eq!(updates.len(), 1, "{state}/{connected}");
        assert_eq!(
            updates[0].suppress_completion, suppressed,
            "{state}/{connected}"
        );
        let response: Value = serde_json::from_str(
            &app.handle_api_request(
                serde_json::from_value(
                    json!({"id":"status","method":"pane.get","params":{"pane_id":public}}),
                )
                .unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(
            response["result"]["pane"]["agent_status"],
            serde_json::to_value(expected).unwrap()
        );
        let event_status =
            app.event_hub
                .events_after(0)
                .into_iter()
                .rev()
                .find_map(|(_, event)| match event.data {
                    EventData::PaneAgentStatusChanged { agent_status, .. } => Some(agent_status),
                    _ => None,
                });
        assert_eq!(event_status, Some(expected), "{state}/{connected}");
        assert!(app.state.terminals[&id].last_agent_completion_seq.is_none());
        assert!(app.state.workspaces[0].pane_state(pane).unwrap().seen);
    }
    app.state.assert_invariants_for_test();
}
