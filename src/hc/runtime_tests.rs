use super::{
    tests::{read_frame, test_client},
    *,
};
use crate::{
    events::AppEvent, layout::PaneId, render_signal::RenderSignal, terminal::TerminalRuntime,
};
use tokio::io::DuplexStream;

pub(super) async fn reply(server: &mut DuplexStream, id: Value, result: Value) {
    let frame = Frame {
        channel: 0,
        kind: DATA,
        payload: Bytes::from(json!({"id":id,"result":result}).to_string()),
    };
    server.write_all(&frame.encode().unwrap()).await.unwrap();
}

pub(super) async fn attach(
    client: Client,
    server: &mut DuplexStream,
) -> (
    TerminalRuntime,
    Arc<tokio::sync::Notify>,
    mpsc::Receiver<AppEvent>,
) {
    let (event_tx, event_rx) = mpsc::channel(16);
    let notify = Arc::new(tokio::sync::Notify::new());
    let dirty = Arc::new(RenderSignal::default());
    let attach = TerminalRuntime::attach_hc(
        PaneId::from_raw(1),
        8,
        40,
        client,
        "named-terminal",
        0,
        1024,
        crate::terminal_theme::TerminalTheme::default(),
        None,
        event_tx,
        notify.clone(),
        dirty,
        true,
    );
    let serve = async {
        let request: Value = serde_json::from_slice(&read_frame(server).await.payload).unwrap();
        assert_eq!(request["method"], "events.subscribe");
        reply(
            server,
            request["id"].clone(),
            json!({"channel":2,"seq":7,"terminals":[{
                "address":"t1","state":"live","cwd":"/remote/shell","pid":999999,
                "foreground":{"cwd":"/remote/work"},"status":null,"reported":false,"records":{}
            }]}),
        )
        .await;
        let request: Value = serde_json::from_slice(&read_frame(server).await.payload).unwrap();
        assert_eq!(request["method"], "terminal.attach");
        assert_eq!(request["params"]["terminal"], "t1");
        reply(
            server,
            request["id"].clone(),
            json!({"channel":3,"automatic_input":true}),
        )
        .await;
        let paint = Frame {
            channel: 3,
            kind: DATA,
            payload: Bytes::from_static(b"\x1b[2J\x1b[Hnative viewer\x1b[c"),
        };
        server.write_all(&paint.encode().unwrap()).await.unwrap();
    };
    let (runtime, _) = tokio::join!(attach, serve);
    (runtime.unwrap(), notify, event_rx)
}

#[tokio::test]
async fn hc_existing_program_attach_preserves_source_size_until_explicit_view_resize() {
    let (client, mut server) = test_client();
    let (events, _receive) = mpsc::channel(8);
    let attach = TerminalRuntime::attach_hc(
        PaneId::from_raw(1),
        8,
        40,
        client,
        "t1",
        0,
        1024,
        crate::terminal_theme::TerminalTheme::default(),
        None,
        events,
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(RenderSignal::default()),
        true,
    );
    let serve = async {
        let request: Value =
            serde_json::from_slice(&read_frame(&mut server).await.payload).unwrap();
        assert_eq!(request["method"], "events.subscribe");
        reply(&mut server, request["id"].clone(), json!({"channel":2,"seq":1,"terminals":[{"address":"t1","state":"live","rows":43,"cols":132}]})).await;
        let request: Value =
            serde_json::from_slice(&read_frame(&mut server).await.payload).unwrap();
        assert_eq!(request["method"], "terminal.attach");
        assert_eq!(request["params"]["rows"], 43);
        assert_eq!(request["params"]["cols"], 132);
        reply(
            &mut server,
            request["id"].clone(),
            json!({"channel":3,"automatic_input":true}),
        )
        .await;
    };
    let (runtime, _) = tokio::join!(attach, serve);
    let runtime = runtime.unwrap();
    assert_eq!(runtime.current_size(), (43, 132));
    runtime.resize(10, 60, 0, 0);
    let frame = until_kind(&mut server, wire::RESIZE).await;
    assert_eq!(frame.payload.as_ref(), &[60, 0, 10, 0]);
}

async fn until_kind(server: &mut DuplexStream, kind: u8) -> Frame {
    timeout(Duration::from_secs(2), async {
        loop {
            let frame = read_frame(server).await;
            if frame.kind == kind {
                return frame;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn hc_terminal_runtime_uses_existing_parser_renderer_and_native_controls() {
    let (client, mut server) = test_client();
    let (runtime, notify, _) = attach(client, &mut server).await;
    timeout(Duration::from_secs(2), async {
        while !runtime.visible_text().contains("native viewer") {
            notify.notified().await;
        }
    })
    .await
    .unwrap();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8)).unwrap();
    terminal
        .draw(|frame| runtime.render(frame, ratatui::layout::Rect::new(0, 0, 40, 8), true))
        .unwrap();
    assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "n");
    assert_eq!(runtime.child_pid(), None);
    assert_eq!(runtime.cwd(), Some("/remote/shell".into()));
    assert_eq!(runtime.foreground_cwd(), Some("/remote/work".into()));
    assert_eq!(runtime.follow_cwd(), Some("/remote/work".into()));
    let binding = runtime.hc_snapshot().unwrap().binding;
    assert_eq!(binding.core_id, "core-test");
    assert_eq!(binding.address, "t1");
    let automatic = until_kind(&mut server, AUTOMATIC_INPUT).await;
    assert_eq!(automatic.channel, 3);
    assert!(!automatic.payload.is_empty());
    runtime
        .try_send_bytes(Bytes::from_static(b"human"))
        .unwrap();
    assert_eq!(until_kind(&mut server, DATA).await.payload, "human");
    runtime.resize(12, 42, 10, 20);
    assert_eq!(
        until_kind(&mut server, RESIZE).await.payload.as_ref(),
        &[42, 0, 12, 0]
    );
    assert!(runtime.try_send_focus_event(crate::ghostty::FocusEvent::Gained));
    assert_eq!(until_kind(&mut server, FOCUS).await.payload.as_ref(), &[1]);
    #[cfg(unix)]
    assert_eq!(
        runtime.duplicate_handoff_fd().unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
}

#[tokio::test]
async fn hc_terminal_runtime_theme_updates_source_colors_without_losing_cell_geometry() {
    let (client, mut server) = test_client();
    let (runtime, _, _) = attach(client, &mut server).await;
    runtime.resize(12, 42, 10, 20);
    until_kind(&mut server, RESIZE).await;
    runtime.apply_host_terminal_theme(crate::terminal_theme::TerminalTheme {
        foreground: Some(crate::terminal_theme::RgbColor { r: 1, g: 2, b: 3 }),
        background: Some(crate::terminal_theme::RgbColor { r: 4, g: 5, b: 6 }),
        ..Default::default()
    });
    let facts = until_kind(&mut server, FACTS).await;
    assert_eq!(facts.channel, 3);
    let display: Value = serde_json::from_slice(&facts.payload).unwrap();
    assert_eq!(display["fg"], "#010203");
    assert_eq!(display["bg"], "#040506");
    assert_eq!(display["cell"], json!([10, 20]));
}

#[tokio::test]
async fn hc_terminal_runtime_receives_authoritative_status_and_outage_is_not_pane_died() {
    let (client, mut server) = test_client();
    let (runtime, _, mut app_events) = attach(client, &mut server).await;
    let mut updates = runtime.hc_updates().unwrap();
    updates.borrow_and_update();
    let event = Frame {channel:2,kind:DATA,payload:Bytes::from(json!({"method":"event","params":{
        "seq":8,"type":"status","terminal":"t1","data":{"status":{"state":"blocked"},"records":{},"reported":true}
    }}).to_string()+"\n")};
    server.write_all(&event.encode().unwrap()).await.unwrap();
    timeout(Duration::from_secs(2), updates.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updates.borrow_and_update().terminal.facts["status"]["state"],
        "blocked"
    );
    drop(server);
    timeout(Duration::from_secs(2), async {
        while updates.borrow_and_update().connected {
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(updates.borrow().error.is_some());
    assert!(app_events.try_recv().is_err());
    assert!(runtime
        .try_send_bytes(Bytes::from_static(b"must not queue"))
        .is_err());
}

#[tokio::test]
async fn hc_terminal_runtime_submission_drains_output_and_events_during_enter_delay() {
    let (client, mut server) = test_client();
    let (runtime, _, _) = attach(client, &mut server).await;
    until_kind(&mut server, CREDIT).await; // Finish processing the initial paint.
    let ack = runtime
        .queue_user_input_submission(
            Bytes::from_static(b"submit"),
            Bytes::from_static(b"\r"),
            Duration::from_secs(3600),
            None,
        )
        .unwrap();
    assert_eq!(until_kind(&mut server, DATA).await.payload, "submit");
    for index in 0..40 {
        let frame = Frame {
            channel: 3,
            kind: DATA,
            payload: Bytes::from(format!("\rflow-{index:02}")),
        };
        server.write_all(&frame.encode().unwrap()).await.unwrap();
        // CREDIT proves parsing continues, without relying on a speed threshold.
        let credit = until_kind(&mut server, CREDIT).await;
        assert_eq!(credit.channel, 3);
    }
    assert!(runtime.visible_text().contains("flow-39"));
    let mut updates = runtime.hc_updates().unwrap();
    let event = Frame {
        channel: 2,
        kind: DATA,
        payload: Bytes::from(
            json!({"method":"event","params":{
                "seq":8,"type":"status","terminal":"t1","data":{"status":{"state":"working"},"records":{},"reported":true}
            }})
            .to_string()
                + "\n",
        ),
    };
    server.write_all(&event.encode().unwrap()).await.unwrap();
    timeout(Duration::from_secs(2), async {
        while updates.borrow_and_update().terminal.facts["status"]["state"] != "working" {
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        ack.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn hc_terminal_runtime_submission_preserves_order_and_deadlines() {
    let (client, mut server) = test_client();
    let (runtime, _, _) = attach(client, &mut server).await;
    until_kind(&mut server, CREDIT).await;
    let ack = runtime
        .queue_user_input_submission(
            Bytes::from_static(b"text"),
            Bytes::from_static(b"\r"),
            Duration::ZERO,
            None,
        )
        .unwrap();
    runtime
        .try_send_bytes(Bytes::from_static(b"later"))
        .unwrap();
    for expected in ["text", "\r", "later"] {
        assert_eq!(until_kind(&mut server, DATA).await.payload, expected);
    }
    ack.try_recv().unwrap().unwrap();
    assert_eq!(
        runtime
            .queue_user_input_submission(
                Bytes::from_static(b"invalid delay"),
                Bytes::from_static(b"\r"),
                Duration::MAX,
                None
            )
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    let ack = runtime
        .queue_user_input_submission(
            Bytes::from_static(b"expired"),
            Bytes::from_static(b"must not send enter"),
            Duration::from_secs(3600),
            Some(std::time::Instant::now()),
        )
        .unwrap();
    runtime
        .try_send_bytes(Bytes::from_static(b"after expired"))
        .unwrap();
    assert_eq!(until_kind(&mut server, DATA).await.payload, "after expired");
    assert_eq!(
        ack.try_recv().unwrap().unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    let ack = runtime
        .queue_user_input_submission(
            Bytes::from_static(b"deadline text"),
            Bytes::from_static(b"must not send enter"),
            Duration::from_secs(3600),
            Some(std::time::Instant::now() + Duration::from_secs(1)),
        )
        .unwrap();
    assert_eq!(until_kind(&mut server, DATA).await.payload, "deadline text");
    runtime
        .try_send_bytes(Bytes::from_static(b"after active deadline"))
        .unwrap();
    assert_eq!(
        until_kind(&mut server, DATA).await.payload,
        "after active deadline"
    );
    assert_eq!(
        ack.try_recv().unwrap().unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
}

#[tokio::test]
async fn hc_terminal_runtime_drop_detaches_but_explicit_shutdown_kills() {
    let (client, mut server) = test_client();
    let (runtime, _, _) = attach(client.clone(), &mut server).await;
    let mut updates = runtime.hc_updates().unwrap();
    updates.borrow_and_update();
    drop(runtime);
    timeout(Duration::from_secs(2), async {
        while updates.borrow_and_update().connected {
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let call = client.call("terminal.list", json!({}));
    let serve = async {
        loop {
            let frame = read_frame(&mut server).await;
            if frame.channel == 0 {
                let request: Value = serde_json::from_slice(&frame.payload).unwrap();
                assert_eq!(
                    request["method"], "terminal.list",
                    "drop must never send terminal.kill"
                );
                reply(&mut server, request["id"].clone(), json!({"terminals":[]})).await;
                break;
            }
        }
    };
    let (result, _) = tokio::join!(call, serve);
    result.unwrap();

    let (client, mut server) = test_client();
    let (runtime, _, _) = attach(client, &mut server).await;
    runtime.shutdown();
    timeout(Duration::from_secs(2), async {
        loop {
            let frame = read_frame(&mut server).await;
            if frame.channel == 0 {
                let request: Value = serde_json::from_slice(&frame.payload).unwrap();
                assert_eq!(request["method"], "terminal.kill");
                assert_eq!(request["params"]["terminal"], "t1");
                reply(&mut server, request["id"].clone(), json!({})).await;
                break;
            }
        }
    })
    .await
    .unwrap();
}
