//! Recovery never launches or moves uncertain input between source generations.
use super::*;
use super::{
    app_tests::{app, restore_into, saved},
    runtime_tests::reply,
    tests::{read_frame, test_client},
};
use crate::{events::AppEvent, terminal::TerminalFactory};
use tokio::io::DuplexStream;

#[tokio::test]
async fn hc_initial_inventory_retry_wakes_an_empty_headless_source() {
    let mut app = app(TerminalFactory::test_saved_source());
    let due = std::time::Instant::now();
    app.hc_recovery.initial_inventory_due = Some(due);
    assert_eq!(app.hc_recovery.deadline(), Some(due));
    app.hc_recovery.initial_inventory_due = None;
    assert!(app.hc_recovery.deadline().is_none());
}

fn publish(
    source: &tokio::sync::watch::Sender<crate::terminal::connection::SourceState>,
    generation: u64,
    client: Option<Client>,
) {
    let mut state = source.borrow().clone();
    state.generation = generation;
    state.client = client;
    state.error = state.client.is_none().then(|| "offline".into());
    source.send_replace(state);
}
async fn rpc(server: &mut DuplexStream) -> Value {
    loop {
        let frame = read_frame(server).await;
        if frame.channel == 0 && frame.kind == DATA {
            return serde_json::from_slice(&frame.payload).unwrap();
        }
    }
}
async fn subscribe(server: &mut DuplexStream, terminals: Value) {
    let request = rpc(server).await;
    assert_eq!(request["method"], "events.subscribe");
    reply(
        server,
        request["id"].clone(),
        json!({"channel":2,"seq":7,"terminals":terminals}),
    )
    .await;
}
fn record() -> Value {
    json!([{"address":"t1","state":"live","cwd":"/remote/home","pid":999999,"status":{"state":"blocked"},"reported":true,"records":{}}])
}
async fn attach(server: &mut DuplexStream) {
    subscribe(server, record()).await;
    let request = rpc(server).await;
    assert_eq!(request["method"], "terminal.attach");
    assert_eq!(request["params"]["terminal"], "t1");
    reply(
        server,
        request["id"].clone(),
        json!({"channel":3,"automatic_input":true}),
    )
    .await;
}
async fn finish(app: &mut crate::app::App, id: &crate::terminal::TerminalId) {
    timeout(Duration::from_secs(2), async {
        while app.terminal_runtimes.get(id).is_none() {
            let event = app.event_rx.recv().await.unwrap();
            app.handle_internal_event(event);
        }
    })
    .await
    .expect("asynchronous recovery did not commit");
}
#[tokio::test]
async fn hc_recovery_initial_offline_attaches_only_saved_binding_and_keeps_public_ids() {
    let (factory, source) = TerminalFactory::test_source(None);
    let mut app = app(factory);
    let binding = json!({"core_id":"core-test","address":"t1"});
    restore_into(&mut app, &saved(binding.clone()));
    let pane = app.state.workspaces[0].tabs[0].root_pane;
    let id = app.state.workspaces[0].tabs[0]
        .terminal_id(pane)
        .unwrap()
        .clone();
    let public = app.public_pane_id(0, pane).unwrap();
    assert!(app.terminal_runtimes.iter().next().is_none());
    app.hc_source_changed(1);
    let (client, mut server) = test_client();
    publish(&source, 2, Some(client.clone()));
    app.hc_source_changed(2);
    app.run_hc_recovery(std::time::Instant::now());
    tokio::join!(finish(&mut app, &id), attach(&mut server));
    assert_eq!(client.next_id.load(Ordering::Relaxed), 4); // subscribe + attach, never run
    assert_eq!(
        app.public_pane_id(0, pane).as_deref(),
        Some(public.as_str())
    );
    assert_eq!(app.state.terminals[&id].hc_binding.as_ref(), Some(&binding));
    assert!(app.state.terminals[&id].restore_error.is_none());
    app.state.assert_invariants_for_test();
}
#[tokio::test]
async fn hc_recovery_replaces_attachment_without_replaying_input_or_accepting_old_updates() {
    let (factory, source) = TerminalFactory::test_source(None);
    let mut app = app(factory);
    restore_into(
        &mut app,
        &saved(json!({"core_id":"core-test","address":"t1"})),
    );
    let pane = app.state.workspaces[0].tabs[0].root_pane;
    let id = app.state.workspaces[0].tabs[0]
        .terminal_id(pane)
        .unwrap()
        .clone();
    let (old_client, mut old_server) = test_client();
    publish(&source, 2, Some(old_client.clone()));
    app.hc_source_changed(2);
    app.run_hc_recovery(std::time::Instant::now());
    tokio::join!(finish(&mut app, &id), attach(&mut old_server));
    let old_token = app.terminal_runtimes.hc_token(&id).unwrap();
    let old_snapshot = app
        .terminal_runtimes
        .get(&id)
        .unwrap()
        .hc_snapshot()
        .unwrap();
    old_server
        .write_all(
            &Frame {
                channel: 3,
                kind: DATA,
                payload: Bytes::from_static(b"\x07"),
            }
            .encode()
            .unwrap(),
        )
        .await
        .unwrap();
    let old_effect = timeout(Duration::from_secs(1), async {
        loop {
            let event = app.event_rx.recv().await.unwrap();
            if matches!(event, AppEvent::HcTerminalEffects { .. }) {
                break event;
            }
            app.handle_internal_event(event);
        }
    })
    .await
    .unwrap();
    app.terminal_runtimes
        .get(&id)
        .unwrap()
        .try_send_bytes(Bytes::from_static(b"uncertain-old-input"))
        .unwrap();
    old_client.disconnect();
    publish(&source, 3, None);
    app.hc_source_changed(3);
    assert!(!app.terminal_runtimes.matches_hc_update(&id, &old_token));
    let (new_client, mut new_server) = test_client();
    publish(&source, 4, Some(new_client.clone()));
    app.hc_source_changed(4);
    app.run_hc_recovery(std::time::Instant::now());
    let wait = async {
        timeout(Duration::from_secs(2), async {
            loop {
                let event = app.event_rx.recv().await.unwrap();
                let ready = matches!(event, AppEvent::HcRecoveryReady);
                app.handle_internal_event(event);
                if ready {
                    break;
                }
            }
        })
        .await
        .unwrap();
    };
    tokio::join!(wait, attach(&mut new_server));
    assert!(!app.handle_internal_event_with_render_impact(old_effect));
    assert!(
        !app.handle_internal_event_with_render_impact(AppEvent::HcTerminalUpdated {
            terminal_id: id.clone(),
            runtime_token: old_token,
            snapshot: old_snapshot
        })
    );
    app.terminal_runtimes
        .get(&id)
        .unwrap()
        .try_send_bytes(Bytes::from_static(b"new-input"))
        .unwrap();
    timeout(Duration::from_secs(1), async {
        loop {
            let frame = read_frame(&mut new_server).await;
            if frame.channel == 3 && frame.kind == DATA {
                assert_eq!(&frame.payload[..], b"new-input");
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(new_client.next_id.load(Ordering::Relaxed), 4);
}
#[tokio::test]
async fn hc_recovery_source_change_cancels_inflight_attach_and_closes_late_stream() {
    let (factory, source) = TerminalFactory::test_source(None);
    let mut app = app(factory);
    restore_into(
        &mut app,
        &saved(json!({"core_id":"core-test","address":"t1"})),
    );
    let (client, mut server) = test_client();
    publish(&source, 2, Some(client.clone()));
    app.hc_source_changed(2);
    app.run_hc_recovery(std::time::Instant::now());
    subscribe(&mut server, record()).await;
    let request = rpc(&mut server).await;
    assert_eq!(request["method"], "terminal.attach");
    publish(&source, 3, None);
    app.hc_source_changed(3);
    // Await cancellation indirectly: a new source event task gets its turn before this reply.
    tokio::task::yield_now().await;
    reply(
        &mut server,
        request["id"].clone(),
        json!({"channel":3,"automatic_input":true}),
    )
    .await;
    timeout(Duration::from_secs(1), async {
        loop {
            let frame = read_frame(&mut server).await;
            if frame.kind == CLOSE && frame.channel == 3 {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(!app.finish_hc_recovery());
    assert!(app.terminal_runtimes.iter().next().is_none());
}
#[tokio::test]
async fn hc_recovery_mismatched_or_malformed_identity_never_sends_rpc() {
    for binding in [
        json!({"core_id":"different-core","address":"t1"}),
        json!({"core_id":42,"address":"alias"}),
    ] {
        let (factory, source) = TerminalFactory::test_source(None);
        let mut app = app(factory);
        restore_into(&mut app, &saved(binding.clone()));
        let (client, _server) = test_client();
        publish(&source, 2, Some(client.clone()));
        app.hc_source_changed(2);
        app.run_hc_recovery(std::time::Instant::now());
        assert_eq!(client.next_id.load(Ordering::Relaxed), 2);
        assert!(app.terminal_runtimes.iter().next().is_none());
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
    }
}
#[tokio::test]
async fn hc_recovery_focus_is_sent_only_after_commit_without_selection_events() {
    let (factory, source) = TerminalFactory::test_source(None);
    let mut app = app(factory);
    restore_into(
        &mut app,
        &saved(json!({"core_id":"core-test","address":"t1"})),
    );
    app.state.outer_terminal_focus = Some(false);
    let (client, mut server) = test_client();
    publish(&source, 2, Some(client));
    app.hc_source_changed(2);
    app.run_hc_recovery(std::time::Instant::now());
    attach(&mut server).await;
    let ready = timeout(Duration::from_secs(1), async {
        loop {
            let event = app.event_rx.recv().await.unwrap();
            if matches!(event, AppEvent::HcRecoveryReady) {
                break event;
            }
            app.handle_internal_event(event);
        }
    })
    .await
    .unwrap();
    assert!(
        timeout(Duration::from_millis(20), read_frame(&mut server))
            .await
            .is_err(),
        "uncommitted attachment sent focus"
    );
    app.handle_internal_event(ready);
    let focus = timeout(Duration::from_secs(1), async {
        loop {
            let frame = read_frame(&mut server).await;
            if frame.channel == 3 && frame.kind == FOCUS {
                break frame;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(&focus.payload[..], &[0]);
}
#[tokio::test]
async fn hc_recovery_caps_attempts_at_four_and_releases_capacity_on_cancel() {
    let mut value =
        serde_json::to_value(saved(json!({"core_id":"core-test","address":"t1"}))).unwrap();
    let template = value["workspaces"][0].clone();
    value["workspaces"] = Value::Array(
        (1..=5)
            .map(|n| {
                let mut ws = template.clone();
                ws["id"] = json!(format!("w{}", ["A", "B", "C", "D", "E"][n - 1]));
                ws["tabs"][0]["panes"]["41"]["hc_binding"]["address"] = json!(format!("t{n}"));
                ws
            })
            .collect(),
    );
    let snapshot = serde_json::from_value(value).unwrap();
    let (factory, source) = TerminalFactory::test_source(None);
    let mut app = app(factory);
    restore_into(&mut app, &snapshot);
    let (client, mut server) = test_client();
    publish(&source, 2, Some(client.clone()));
    app.hc_source_changed(2);
    app.run_hc_recovery(std::time::Instant::now());
    let mut addresses = Vec::new();
    for _ in 0..4 {
        let request = rpc(&mut server).await;
        assert_eq!(request["method"], "events.subscribe");
        addresses.push(request["params"]["terminal"].as_str().unwrap().to_owned());
    }
    assert_eq!(client.next_id.load(Ordering::Relaxed), 6);
    assert!(app.hc_recovery.deadline().is_none());
    app.run_hc_recovery(std::time::Instant::now());
    assert_eq!(client.next_id.load(Ordering::Relaxed), 6);
    let id = app
        .state
        .terminals
        .iter()
        .find(|(_, t)| t.hc_binding.as_ref().unwrap()["address"] == addresses[0])
        .unwrap()
        .0
        .clone();
    app.cancel_hc_recovery(&id);
    assert!(app.hc_recovery.deadline().is_some());
    app.run_hc_recovery(std::time::Instant::now());
    let fifth = rpc(&mut server).await;
    assert_eq!(fifth["method"], "events.subscribe");
    assert!(!addresses
        .iter()
        .any(|a| fifth["params"]["terminal"].as_str() == Some(a)));
    assert_eq!(client.next_id.load(Ordering::Relaxed), 7);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hc_recovery_queued_candidate_cannot_resurrect_an_explicitly_closed_pane() {
    let (factory, source) = TerminalFactory::test_source(None);
    let mut app = app(factory);
    restore_into(
        &mut app,
        &saved(json!({"core_id":"core-test","address":"t1"})),
    );
    let public = app
        .public_pane_id(0, app.state.workspaces[0].tabs[0].root_pane)
        .unwrap();
    let (client, mut server) = test_client();
    publish(&source, 2, Some(client.clone()));
    app.hc_source_changed(2);
    app.run_hc_recovery(std::time::Instant::now());
    attach(&mut server).await;
    timeout(Duration::from_secs(1), async {
        loop {
            let event = app.event_rx.recv().await.unwrap();
            if matches!(event, AppEvent::HcRecoveryReady) {
                break;
            }
            app.handle_internal_event(event);
        }
    })
    .await
    .unwrap();
    let serve = tokio::spawn(async move {
        let list = rpc(&mut server).await;
        assert_eq!(list["method"], "terminal.list");
        reply(
            &mut server,
            list["id"].clone(),
            json!({"terminals":record()}),
        )
        .await;
        let kill = rpc(&mut server).await;
        assert_eq!(kill["method"], "terminal.kill");
        assert_eq!(kill["params"]["terminal"], "t1");
        reply(
            &mut server,
            kill["id"].clone(),
            json!({"address":"t1","state":"finished"}),
        )
        .await;
        server
    });
    let request = serde_json::from_value(
        json!({"id":"close","method":"pane.close","params":{"pane_id":public}}),
    )
    .unwrap();
    let response: Value =
        serde_json::from_str(&app.handle_api_request_after_internal_events_drained(request))
            .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    let _server = serve.await.unwrap();
    assert!(!app.finish_hc_recovery());
    assert!(app.state.workspaces.is_empty());
    assert_eq!(app.terminal_runtimes.len(), 0);
    assert!(!app.ensure_default_workspace());
    assert_eq!(client.next_id.load(Ordering::Relaxed), 6);
}

#[tokio::test]
async fn hc_source_request_rejects_obsolete_host_before_any_launch_or_input() {
    let (client, _server) = test_client();
    let mut app = app(TerminalFactory::test_hc(client.clone()));
    let request:crate::api::schema::Request=serde_json::from_value(json!({"id":"old","method":"runtime.request","params":{"host_instance":"obsolete-host","request":{"id":"inner","method":"workspace.create","params":{}}}})).unwrap();
    let response: Value = serde_json::from_str(&app.handle_api_request(request)).unwrap();
    assert_eq!(response["id"], "old");
    assert_eq!(response["error"]["code"], "stale_runtime_source");
    assert!(app.state.workspaces.is_empty());
    assert_eq!(client.next_id.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn hc_recovery_missing_terminal_backs_off_without_run_or_substitute_attach() {
    let (factory, source) = TerminalFactory::test_source(None);
    let mut app = app(factory);
    restore_into(
        &mut app,
        &saved(json!({"core_id":"core-test","address":"t1"})),
    );
    let (client, mut server) = test_client();
    publish(&source, 2, Some(client.clone()));
    app.hc_source_changed(2);
    app.run_hc_recovery(std::time::Instant::now());
    subscribe(&mut server, json!([])).await;
    timeout(Duration::from_secs(1), async {
        loop {
            let event = app.event_rx.recv().await.unwrap();
            let ready = matches!(event, AppEvent::HcRecoveryReady);
            app.handle_internal_event(event);
            if ready {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(app.terminal_runtimes.iter().next().is_none());
    assert_eq!(client.next_id.load(Ordering::Relaxed), 3);
    assert!(app.hc_recovery.deadline().unwrap() > std::time::Instant::now());
}
