//! Pure persistence contracts: offline/invalid bindings must never become launch requests.

#[test]
fn hc_saved_launch_does_not_export_local_api_or_agent_proxy_paths() {
    let launch =
        crate::pane::PaneLaunchEnv::from_extra(vec![("SOURCE_ALLOWED".into(), "yes".into())])
            .with_identity("w1".into(), "w1:t1".into(), "w1:p1".into());
    let args = vec!["/source/program".into()];
    let (argv, env) = crate::pane::hc_launch_command(
        crate::terminal::TerminalCommand::Argv(&args, crate::pane::AgentDetection::Disabled),
        &launch,
        true,
    )
    .unwrap();
    assert_eq!(env.get("SOURCE_ALLOWED").map(String::as_str), Some("yes"));
    for key in [
        "HERDR_ENV",
        "HERDR_SOCKET_PATH",
        "HERDR_BIN_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_PANE_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_TAB_ID",
        "HERDR_HC_SOURCE_PROFILE",
        "HERDR_HC_SHARED_CORE",
        "HERDR_HC_SHARED_REGISTRY_CORE",
    ] {
        assert!(!env.contains_key(key));
        assert!(argv.windows(2).any(|a| a[0] == "-u" && a[1] == key));
    }
    assert!(!env.contains_key("SSH_AUTH_SOCK"));
}

#[tokio::test]
async fn hc_saved_source_has_no_local_worktree_or_git_fallback() {
    let mut app = app(TerminalFactory::test_saved_source());
    assert!(!app.ensure_default_workspace());
    restore_into(
        &mut app,
        &saved(json!({"core_id":"offline-core","address":"t1"})),
    );
    app.refresh_restored_workspace_git_metadata();
    app.request_git_identity_refresh(std::time::Instant::now());
    assert!(app.git_refresh_deadline().is_none());
    for (method, params) in [
        ("worktree.list", json!({})),
        ("worktree.open", json!({"path":"/source-only"})),
        (
            "worktree.create",
            json!({"cwd":"/source-only","path":"/source-only-new"}),
        ),
        (
            "worktree.remove",
            json!({"workspace_id":"missing","force":true}),
        ),
    ] {
        let request: crate::api::schema::Request =
            serde_json::from_value(json!({"id":"no-fs","method":method,"params":params})).unwrap();
        let response: Value =
            serde_json::from_str(&app.handle_api_request(request.clone())).unwrap();
        assert_eq!(response["error"]["code"], "unsupported_runtime");
        let (send, receive) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_worktree_api_request(request, send, false));
        let response: Value = serde_json::from_str(&receive.try_recv().unwrap()).unwrap();
        assert_eq!(response["error"]["code"], "unsupported_runtime");
    }
    assert_eq!(app.state.workspaces.len(), 1);
}

use super::*;
use crate::{
    persist,
    terminal::{TerminalFactory, TerminalRuntimeRegistry},
};

pub(super) fn saved(binding: Value) -> persist::SessionSnapshot {
    serde_json::from_value(json!({"version":3,"active":0,"selected":0,"workspaces":[{
        "id":"wA","identity_cwd":"/remote/unavailable","public_pane_numbers":{"41":9},
        "next_public_pane_number":10,"public_tab_numbers":[7],"next_public_tab_number":8,
        "tabs":[{"layout":{"Pane":41},"panes":{"41":{"cwd":"/remote/unavailable","hc_binding":binding,
            "launch_argv":["must-not-run"],"agent_resume":{"source":"custom","agent":"claude","argv":["must-not-resume"]}}},
            "zoomed":true,"focused":41,"root_pane":41}]}]})).unwrap()
}

#[test]
fn hc_app_restore_keeps_offline_binding_and_public_identity_through_autosave() {
    let binding = json!({"core_id":"saved-core","address":"t17"});
    let mut snapshot = saved(binding.clone());
    let pane = snapshot.workspaces[0].tabs[0]
        .panes
        .values_mut()
        .next()
        .unwrap();
    pane.agent_name = Some("reviewer".into());
    pane.managed_agent_kind = Some("pi".into());
    let (events, _receive) = mpsc::channel(16);
    let (workspaces, terminals, runtimes) = persist::restore_with_factory(
        &snapshot,
        None,
        24,
        80,
        1024,
        "/must-not-spawn",
        crate::config::ShellModeConfig::NonLogin,
        true,
        events,
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(crate::render_signal::RenderSignal::new()),
        &TerminalFactory::Unavailable("offline core".into()),
    );
    assert!(runtimes.is_empty());
    let workspace = &workspaces[0];
    let pane = workspace.tabs[0].root_pane;
    assert_eq!(workspace.id, "wA");
    assert_eq!(workspace.public_pane_numbers[&pane], 9);
    assert_eq!(workspace.tabs[0].number, 7);
    assert!(workspace.tabs[0].zoomed);
    let terminal = &terminals[workspace.tabs[0].terminal_id(pane).unwrap()];
    assert_eq!(terminal.hc_binding.as_ref(), Some(&binding));
    assert_eq!(terminal.agent_name.as_deref(), Some("reviewer"));
    assert!(terminal.managed_agent_kind().is_none());
    assert!(!terminal.is_agent_terminal());
    assert!(terminal
        .restore_error
        .as_deref()
        .unwrap()
        .contains("offline core"));
    assert!(terminal.pending_agent_resume_plan.is_none());
    let captured = persist::capture(
        &workspaces,
        &terminals,
        &TerminalRuntimeRegistry::new(),
        Some(0),
        0,
    );
    let encoded = serde_json::to_value(captured).unwrap();
    let panes = encoded["workspaces"][0]["tabs"][0]["panes"]
        .as_object()
        .unwrap();
    assert_eq!(panes.values().next().unwrap()["hc_binding"], binding);
    assert_eq!(panes.values().next().unwrap()["agent_name"], "reviewer");
    let mut state = crate::app::AppState::test_new();
    state.workspaces = workspaces;
    state.terminals = terminals;
    state.active = Some(0);
    state.assert_invariants_for_test();
}

#[test]
fn hc_submitted_agent_intent_does_not_fabricate_source_status_or_readiness() {
    let mut app = app(TerminalFactory::Unavailable("offline".into()));
    restore_into(&mut app, &saved(json!({"core_id":"source","address":"t1"})));
    let terminal = app.state.terminals.values_mut().next().unwrap();
    terminal.begin_managed_agent(
        "pending".into(),
        crate::detect::Agent::Pi,
        std::time::Instant::now(),
        std::time::Duration::from_secs(3),
        std::time::Duration::from_secs(5),
    );
    assert!(!terminal.is_agent_terminal());
    assert_eq!(terminal.state, crate::detect::AgentState::Unknown);
    assert!(terminal.effective_known_agent().is_none());
    let request =
        serde_json::from_value(json!({"id":"intent","method":"agent.list","params":{}})).unwrap();
    let response: Value = serde_json::from_str(&app.handle_api_request(request)).unwrap();
    let agents = response["result"]["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "{response}");
    let intent: crate::api::schema::AgentInfo = serde_json::from_value(agents[0].clone()).unwrap();
    assert_eq!(intent.name.as_deref(), Some("pending"));
    assert!(intent.agent.is_none());
    assert_eq!(
        intent.agent_status,
        crate::api::schema::AgentStatus::Unknown
    );
    assert!(intent.launch_pending);
    assert!(!intent.interactive_ready);
    assert!(intent.completion_seq.is_none());
    app.state.assert_invariants_for_test();
}

#[test]
fn hc_app_restore_retains_malformed_binding_instead_of_falling_back_to_a_shell() {
    let snapshot = saved(json!({"core_id":42,"address":"named-alias"}));
    let (events, _receive) = mpsc::channel(16);
    let (workspaces, terminals, runtimes) = persist::restore_with_factory(
        &snapshot,
        None,
        24,
        80,
        1024,
        "/must-not-spawn",
        crate::config::ShellModeConfig::NonLogin,
        true,
        events,
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(crate::render_signal::RenderSignal::new()),
        &TerminalFactory::Unavailable("offline core".into()),
    );
    assert_eq!(terminals.len(), 1);
    assert!(runtimes.is_empty());
    let terminal = terminals.values().next().unwrap();
    assert!(terminal
        .restore_error
        .as_deref()
        .unwrap()
        .contains("binding"));
    assert_eq!(terminal.hc_binding.as_ref().unwrap()["core_id"], 42);
    let captured = persist::capture(
        &workspaces,
        &terminals,
        &TerminalRuntimeRegistry::new(),
        Some(0),
        0,
    );
    let encoded = serde_json::to_value(captured).unwrap();
    let panes = encoded["workspaces"][0]["tabs"][0]["panes"]
        .as_object()
        .unwrap();
    assert_eq!(panes.values().next().unwrap()["hc_binding"]["core_id"], 42);
}

#[tokio::test]
async fn shared_first_open_inventory_reads_only_and_rejects_a_source_generation_change() {
    for change_source in [false, true] {
        let (client, mut server) = super::tests::test_client();
        let (factory, source) = TerminalFactory::test_source(Some(client));
        let fetch = tokio::task::spawn_blocking(move || factory.initial_terminals());
        let request: Value =
            serde_json::from_slice(&super::tests::read_frame(&mut server).await.payload).unwrap();
        assert_eq!(request["method"], "terminal.list");
        assert_eq!(request["params"]["all"], false);
        if change_source {
            source.send_modify(|state| {
                state.generation += 1;
                state.client = None;
            });
        }
        super::runtime_tests::reply(
            &mut server,
            request["id"].clone(),
            json!({"terminals":[{"address":"t1","state":"live","cwd":"/source"}]}),
        )
        .await;
        let result = fetch.await.unwrap();
        if change_source {
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotConnected);
        } else {
            let (owner, terminals, layout) = result.unwrap();
            assert_eq!(owner, "core-test");
            assert!(layout.is_none());
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].address, "t1");
        }
    }
}

pub(super) fn app(factory: TerminalFactory) -> crate::app::App {
    crate::app::App::try_new_with_terminal_factory(
        &crate::config::Config::default(),
        crate::app::AppPolicy::TEST,
        None,
        tokio::sync::mpsc::unbounded_channel().1,
        crate::api::EventHub::default(),
        factory,
    )
    .unwrap()
}

pub(super) fn restore_into(app: &mut crate::app::App, snapshot: &persist::SessionSnapshot) {
    let (workspaces, terminals, runtimes) = persist::restore_with_factory(
        snapshot,
        None,
        24,
        80,
        1024,
        "/must-not-launch",
        crate::config::ShellModeConfig::NonLogin,
        true,
        app.event_tx.clone(),
        app.render_notify.clone(),
        app.render_dirty.clone(),
        &app.terminal_factory,
    );
    app.state.workspaces = workspaces;
    app.state.terminals = terminals;
    app.state.active = Some(0);
    app.state.selected = 0;
    app.terminal_runtimes.extend(runtimes);
    assert!(!app.ensure_default_workspace());
    app.state.assert_invariants_for_test();
}

#[test]
fn hc_app_offline_close_keeps_binding_and_does_not_report_success() {
    let binding = json!({"core_id":"saved-core","address":"t17"});
    let mut app = app(TerminalFactory::Unavailable("offline".into()));
    restore_into(&mut app, &saved(binding.clone()));
    let public = app
        .public_pane_id(0, app.state.workspaces[0].tabs[0].root_pane)
        .unwrap();
    let response: Value = serde_json::from_str(
        &app.handle_api_request(
            serde_json::from_value(json!({
                "id":"close","method":"pane.close","params":{"pane_id":public}
            }))
            .unwrap(),
        ),
    )
    .unwrap();
    assert_eq!(response["error"]["code"], "hc_close_failed");
    assert_eq!(app.state.terminals.len(), 1);
    assert_eq!(
        app.state
            .terminals
            .values()
            .next()
            .unwrap()
            .hc_binding
            .as_ref(),
        Some(&binding)
    );
    app.state.assert_invariants_for_test();
}

#[test]
fn hc_app_unreported_status_and_legacy_hooks_remain_unknown() {
    use crate::detect::{Agent, AgentState};
    let mut terminal =
        crate::terminal::TerminalState::new(crate::terminal::TerminalId::alloc(), "/saved".into());
    terminal.set_detected_state(Some(Agent::Claude), AgentState::Working);
    let snapshot = state::RuntimeSnapshot { binding:state::TerminalBinding {core_id:"core".into(),address:"t1".into()},
        terminal:serde_json::from_value(json!({"address":"t1","reported":false,"status":{"app":"claude","state":"working"},"cwd":"/source"})).unwrap(),
        connected:true,attached:true,error:None };
    terminal.apply_hc_snapshot(snapshot.clone());
    assert_eq!(terminal.state, AgentState::Unknown);
    assert!(!terminal.is_agent_terminal());
    terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
    assert_eq!(terminal.state, AgentState::Unknown);
    let mut reported = snapshot;
    reported
        .terminal
        .facts
        .insert("reported".into(), json!(true));
    terminal.apply_hc_snapshot(reported.clone());
    assert_eq!(terminal.state, AgentState::Working);
    assert!(terminal.effective_presentation().state_labels.is_empty());
    reported
        .terminal
        .facts
        .insert("status".into(), json!({"state":"done"}));
    terminal.apply_hc_snapshot(reported.clone());
    assert_eq!(
        terminal.effective_presentation().state_labels["idle"],
        "done"
    );
    reported.connected = false;
    terminal.apply_hc_snapshot(reported);
    assert_eq!(terminal.state, AgentState::Unknown);
}

#[test]
fn hc_app_binding_accepts_only_canonical_source_local_addresses() {
    for address in [
        "",
        "named",
        "t0",
        "t01",
        "t+1",
        "t-1",
        "machine/t1",
        "t18446744073709551616",
        "tb",
        "t1a",
        "t1l",
        "T1",
        "t1B",
        "t9zzzzzzzzzz",
        "t6kf3jk6", // u32::MAX + 1 in upstream's counter encoding.
        "t1é",
    ] {
        assert!(
            state::TerminalBinding {
                core_id: "core".into(),
                address: address.into()
            }
            .validate()
            .is_err(),
            "{address}"
        );
    }
    for address in [
        "t1", "t9", "t10", "t19", "t1b", "t1z", "t20", "t9z", "t100", "t9zz", "t1000", "t65jk",
        "t6kf3jk5", // u32::MAX.
    ] {
        assert!(
            state::TerminalBinding {
                core_id: "core".into(),
                address: address.into()
            }
            .validate()
            .is_ok(),
            "{address}"
        );
    }
}

#[test]
fn hc_app_launch_preserves_explicit_overrides_and_source_environment_removals() {
    let launch = crate::pane::PaneLaunchEnv::from_extra(vec![
        ("CLAUDECODE".into(), "intentional".into()),
        ("VISIBLE".into(), "yes".into()),
    ])
    .without_pane_identity();
    let command = vec!["/bin/cat".into(), "a b".into()];
    let (argv, env) = crate::pane::hc_launch_command(
        crate::terminal::TerminalCommand::Argv(&command, crate::pane::AgentDetection::Enabled),
        &launch,
        false,
    )
    .unwrap();
    assert_eq!(env["CLAUDECODE"], "intentional");
    assert_eq!(env["VISIBLE"], "yes");
    assert!(!argv.windows(2).any(|pair| pair == ["-u", "CLAUDECODE"]));
    assert!(argv
        .windows(2)
        .any(|pair| pair == ["-u", "CODEX_THREAD_ID"]));
    assert!(argv
        .windows(2)
        .any(|pair| pair == ["-u", crate::integration::HERDR_PANE_ID_ENV_VAR]));
    assert_eq!(&argv[argv.len() - 2..], command);
    assert!(!env.contains_key("PATH"));
}

#[tokio::test]
async fn hc_app_registry_fences_queued_updates_after_replacement_and_rejects_legacy_reports() {
    let (client, mut server) = tests::test_client();
    let (runtime, _, _) = runtime_tests::attach(client, &mut server).await;
    let mut app = app(TerminalFactory::Unavailable("test source".into()));
    restore_into(
        &mut app,
        &saved(json!({"core_id":"core-test","address":"t1"})),
    );
    let pane = app.state.workspaces[0].tabs[0].root_pane;
    let id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
    app.state
        .terminals
        .get_mut(&id)
        .unwrap()
        .apply_hc_snapshot(runtime.hc_snapshot().unwrap());
    app.terminal_runtimes.insert(id.clone(), runtime);
    let old = timeout(Duration::from_secs(2), app.event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let crate::events::AppEvent::HcTerminalUpdated {
        runtime_token: old_token,
        mut snapshot,
        ..
    } = old
    else {
        panic!("missing source update");
    };
    snapshot
        .terminal
        .facts
        .insert("status".into(), json!({"state":"blocked"}));
    snapshot
        .terminal
        .facts
        .insert("reported".into(), json!(true));
    let (client, mut other_server) = tests::test_client();
    let (replacement, _, _) = runtime_tests::attach(client, &mut other_server).await;
    drop(app.terminal_runtimes.insert(id.clone(), replacement));
    assert!(!app.handle_internal_event_with_render_impact(
        crate::events::AppEvent::HcTerminalUpdated {
            terminal_id: id.clone(),
            runtime_token: old_token,
            snapshot: snapshot.clone()
        }
    ));
    assert_eq!(
        app.state.terminals[&id].state,
        crate::detect::AgentState::Unknown
    );
    let current = timeout(Duration::from_secs(2), app.event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let crate::events::AppEvent::HcTerminalUpdated { runtime_token, .. } = current else {
        panic!("missing replacement update");
    };
    let mut wrong = snapshot.clone();
    wrong.binding.core_id = "another-core".into();
    assert!(!app.handle_internal_event_with_render_impact(
        crate::events::AppEvent::HcTerminalUpdated {
            terminal_id: id.clone(),
            runtime_token: runtime_token.clone(),
            snapshot: wrong
        }
    ));
    assert!(app.handle_internal_event_with_render_impact(
        crate::events::AppEvent::HcTerminalUpdated {
            terminal_id: id.clone(),
            runtime_token: runtime_token.clone(),
            snapshot
        }
    ));
    assert_eq!(
        app.state.terminals[&id].state,
        crate::detect::AgentState::Blocked
    );
    assert!(!app.handle_internal_event_with_render_impact(
        crate::events::AppEvent::HookStateReported {
            pane_id: pane,
            source: "old-hook".into(),
            agent_label: "claude".into(),
            state: crate::detect::AgentState::Idle,
            message: None,
            seq: Some(1),
            session_ref: None
        }
    ));
    assert_eq!(
        app.state.terminals[&id].state,
        crate::detect::AgentState::Blocked
    );
    app.state.pane_id_aliases.insert(999999, pane);
    assert!(!app.handle_internal_event_with_render_impact(
        crate::events::AppEvent::HookStateReported {
            pane_id: crate::layout::PaneId::from_raw(999999),
            source: "old-alias-hook".into(),
            agent_label: "claude".into(),
            state: crate::detect::AgentState::Idle,
            message: None,
            seq: Some(2),
            session_ref: None
        }
    ));
    assert_eq!(
        app.state.terminals[&id].state,
        crate::detect::AgentState::Blocked
    );
    let public = app.public_pane_id(0, pane).unwrap();
    let response: Value = serde_json::from_str(&app.handle_api_request(serde_json::from_value(json!({
        "id":"hook","method":"pane.report_agent","params":{"pane_id":public,"source":"old-hook","agent":"claude","state":"idle"}
    })).unwrap())).unwrap();
    assert_eq!(response["error"]["code"], "unsupported_runtime");
    assert!(
        !app.handle_internal_event_with_render_impact(crate::events::AppEvent::PaneDied {
            pane_id: pane,
            exit_reason: crate::platform::ChildExitReason::Exited,
        })
    );
    assert_eq!(app.state.workspaces[0].tabs[0].root_pane, pane);
    let mut done = app.state.terminals[&id].hc_snapshot.clone().unwrap();
    done.terminal
        .facts
        .insert("status".into(), json!({"state":"done"}));
    app.handle_internal_event_with_render_impact(crate::events::AppEvent::HcTerminalUpdated {
        terminal_id: id.clone(),
        runtime_token,
        snapshot: done,
    });
    assert_eq!(
        app.state.terminals[&id].state,
        crate::detect::AgentState::Idle
    );
    assert!(app.state.terminals[&id].last_agent_completion_seq.is_none());
    app.state.assert_invariants_for_test();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hc_app_unknown_run_outcome_is_not_retried_by_default_workspace_creation() {
    let (client, mut server) = tests::test_client();
    let count = client.next_id.clone();
    let source = tokio::spawn(async move {
        let request: Value =
            serde_json::from_slice(&tests::read_frame(&mut server).await.payload).unwrap();
        assert_eq!(request["method"], "terminal.run");
        // The program may already exist; disconnect without acknowledging run.
        drop(server);
    });
    let mut app = app(TerminalFactory::test_hc(client));
    assert!(app
        .create_workspace_with_options("/var/tmp".into(), true)
        .is_err());
    assert_eq!(count.load(Ordering::Relaxed), 3);
    assert!(!app.ensure_default_workspace());
    assert_eq!(
        count.load(Ordering::Relaxed),
        3,
        "the event loop retried an ambiguous run"
    );
    assert!(app.state.workspaces.is_empty());
    source.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hc_app_unconfirmed_launch_rollback_reports_the_known_new_identity() {
    let (client, mut server) = tests::test_client();
    let source = tokio::spawn(async move {
        let request: Value =
            serde_json::from_slice(&tests::read_frame(&mut server).await.payload).unwrap();
        assert_eq!(request["method"], "terminal.run");
        runtime_tests::reply(&mut server, request["id"].clone(), json!({"address":"t1"})).await;
        let request: Value =
            serde_json::from_slice(&tests::read_frame(&mut server).await.payload).unwrap();
        assert_eq!(request["method"], "events.subscribe");
        let refusal = Frame {
            channel: 0,
            kind: DATA,
            payload: Bytes::from(
                json!({"id":request["id"],"error":{"code":"refused","message":"subscribe failed"}})
                    .to_string(),
            ),
        };
        server.write_all(&refusal.encode().unwrap()).await.unwrap();
        let request: Value =
            serde_json::from_slice(&tests::read_frame(&mut server).await.payload).unwrap();
        assert_eq!(request["method"], "terminal.kill");
        assert_eq!(request["params"]["terminal"], "t1");
        runtime_tests::reply(
            &mut server,
            request["id"].clone(),
            json!({"address":"t1","state":"live"}),
        )
        .await;
    });
    let mut app = app(TerminalFactory::test_hc(client));
    let error = app
        .create_workspace_with_options("/var/tmp".into(), true)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("cleanup") && error.contains("core-test/t1"),
        "{error}"
    );
    assert!(app.state.workspaces.is_empty());
    source.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hc_app_unconfirmed_or_mismatched_kill_keeps_runtime_binding_and_layout() {
    for killed in [
        json!({"address":"t1","state":"live"}),
        json!({"address":"t2","state":"finished"}),
    ] {
        let (client, mut server) = tests::test_client();
        let (runtime, _, _) = runtime_tests::attach(client, &mut server).await;
        let mut app = app(TerminalFactory::Unavailable("fixture".into()));
        restore_into(
            &mut app,
            &saved(json!({"core_id":"core-test","address":"t1"})),
        );
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
        app.state
            .terminals
            .get_mut(&id)
            .unwrap()
            .apply_hc_snapshot(runtime.hc_snapshot().unwrap());
        app.terminal_runtimes.insert(id.clone(), runtime);
        let (control, mut control_server) = tests::test_client();
        app.terminal_factory = TerminalFactory::test_hc(control);
        let source = tokio::spawn(async move {
            let request: Value =
                serde_json::from_slice(&tests::read_frame(&mut control_server).await.payload)
                    .unwrap();
            assert_eq!(request["method"], "terminal.list");
            runtime_tests::reply(
                &mut control_server,
                request["id"].clone(),
                json!({"terminals":[{"address":"t1","state":"live"}]}),
            )
            .await;
            let request: Value =
                serde_json::from_slice(&tests::read_frame(&mut control_server).await.payload)
                    .unwrap();
            assert_eq!(request["method"], "terminal.kill");
            runtime_tests::reply(&mut control_server, request["id"].clone(), killed).await;
        });
        let public = app.public_pane_id(0, pane).unwrap();
        let response: Value = serde_json::from_str(
            &app.handle_api_request(
                serde_json::from_value(json!({
                    "id":"close","method":"pane.close","params":{"pane_id":public}
                }))
                .unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(response["error"]["code"], "hc_close_failed");
        assert!(app.terminal_runtimes.get(&id).is_some());
        assert_eq!(app.state.workspaces[0].tabs[0].root_pane, pane);
        assert_eq!(
            app.state.terminals[&id].hc_binding.as_ref().unwrap()["address"],
            "t1"
        );
        app.state.assert_invariants_for_test();
        source.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hc_app_malformed_or_duplicate_list_is_not_confirmation_of_absence() {
    for terminals in [
        json!([{}]),
        json!([{"address":"t1"}]),
        json!([{"address":"t1","state":"finished"},{"address":"t1","state":"live"}]),
    ] {
        let (client, mut server) = tests::test_client();
        let calls = client.next_id.clone();
        let factory = TerminalFactory::test_hc(client);
        let source = tokio::spawn(async move {
            let request: Value =
                serde_json::from_slice(&tests::read_frame(&mut server).await.payload).unwrap();
            assert_eq!(request["method"], "terminal.list");
            runtime_tests::reply(
                &mut server,
                request["id"].clone(),
                json!({"terminals":terminals}),
            )
            .await;
        });
        let error = factory
            .terminate(
                &crate::hc::state::TerminalBinding {
                    core_id: "core-test".into(),
                    address: "t1".into(),
                },
                None,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            3,
            "invalid list must not trigger a kill"
        );
        source.await.unwrap();
    }
}

#[test]
fn hc_app_deferred_worktree_removal_rejects_hc_before_filesystem_or_shutdown() {
    let mut app = app(TerminalFactory::Unavailable("fixture".into()));
    restore_into(
        &mut app,
        &saved(json!({"core_id":"core-test","address":"t1"})),
    );
    let request = serde_json::from_value(json!({"id":"remove","method":"worktree.remove","params":{"workspace_id":"wA","force":true}})).unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    app.handle_deferred_worktree_api_request(request, send, false);
    let response: Value = serde_json::from_str(&receive.try_recv().unwrap()).unwrap();
    assert_eq!(response["error"]["code"], "unsupported_runtime");
    assert_eq!(app.state.terminals.len(), 1);
    app.state.assert_invariants_for_test();
}

#[tokio::test]
async fn hc_app_losing_popup_presentation_detaches_instead_of_killing_another_program() {
    let mut app = app(TerminalFactory::Unavailable("fixture".into()));
    let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
    let (_, id) = app.install_test_popup_runtime(runtime);
    let terminal = app
        .state
        .terminals
        .remove(&id)
        .unwrap()
        .with_hc_binding(Some(json!({"core_id":"core-test","address":"t1"})));
    app.state.terminals.insert(id.clone(), terminal);
    assert!(
        app.close_popup_pane().is_err(),
        "offline explicit close must retain the binding"
    );
    assert!(app.state.popup_pane.is_some());
    assert!(
        app.detach_hc_popup_pane(),
        "presentation loss must not require a kill RPC"
    );
    assert!(app.state.popup_pane.is_none());
    assert!(app.terminal_runtimes.get(&id).is_none());
}

#[test]
fn hc_app_unbound_legacy_layout_can_be_discarded_without_claiming_or_killing_a_program() {
    let mut app = app(TerminalFactory::Unavailable("fixture".into()));
    restore_into(&mut app, &saved(Value::Null));
    let pane = app.state.workspaces[0].tabs[0].root_pane;
    let public = app.public_pane_id(0, pane).unwrap();
    let response: Value = serde_json::from_str(
        &app.handle_api_request(
            serde_json::from_value(json!({
                "id":"discard-unbound","method":"pane.close","params":{"pane_id":public}
            }))
            .unwrap(),
        ),
    )
    .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    assert!(app.state.workspaces.is_empty());
    assert!(app.state.terminals.is_empty());
    assert!(!app.ensure_default_workspace());
}

#[test]
fn hc_source_launch_names_the_pane_only_to_plugin_panes_routed_back_to_herdr() {
    let command = vec!["/bin/cat".into()];
    let launch = |extra: Vec<(String, String)>| {
        crate::pane::hc_launch_command(
            crate::terminal::TerminalCommand::Argv(&command, crate::pane::AgentDetection::Enabled),
            &crate::pane::PaneLaunchEnv::from_extra(extra).with_identity(
                "w1".into(),
                "w1:t2".into(),
                "w1:p3".into(),
            ),
            true,
        )
        .unwrap()
    };
    let (argv, env) = launch(vec![(
        crate::pane::PLUGIN_ID_ENV_VAR.into(),
        "herdr-sidebar".into(),
    )]);
    assert_eq!(env[crate::integration::HERDR_PANE_ID_ENV_VAR], "w1:p3");
    assert_eq!(env[crate::integration::HERDR_TAB_ID_ENV_VAR], "w1:t2");
    assert_eq!(env[crate::integration::HERDR_WORKSPACE_ID_ENV_VAR], "w1");
    assert!(!argv
        .windows(2)
        .any(|pair| pair == ["-u", crate::integration::HERDR_PANE_ID_ENV_VAR]));

    // No route back: an id would only point it at a pane it cannot reach.
    let (argv, env) = launch(Vec::new());
    assert!(!env.contains_key(crate::integration::HERDR_PANE_ID_ENV_VAR));
    assert!(argv
        .windows(2)
        .any(|pair| pair == ["-u", crate::integration::HERDR_PANE_ID_ENV_VAR]));
}
