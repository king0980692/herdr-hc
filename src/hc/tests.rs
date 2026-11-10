use super::state::{Change, Projection, Snapshot};
use super::*;
use tokio::io::{duplex, split, DuplexStream};

fn snapshot(seq: u64, terminals: Value) -> Snapshot {
    serde_json::from_value(json!({"seq":seq, "terminals":terminals})).unwrap()
}

fn event(seq: u64, kind: &str, data: Value) -> Value {
    json!({"method":"event", "params":{"seq":seq,"type":kind,"terminal":"t1","data":data}})
}

fn projection() -> Projection {
    let mut state = Projection::default();
    state.begin("core-a".into(), 1);
    state
        .snapshot(
            1,
            snapshot(
                8,
                json!([{"address":"t1","state":"live","pid":123,
        "foreground":{"cwd":"/remote"},"status":{"state":"working"}}]),
            ),
        )
        .unwrap();
    state
}

#[test]
fn hc_projection_lost_is_not_finished_and_restart_replaces_record() {
    let mut state = projection();
    state
        .notification(
            1,
            event(
                9,
                "lost",
                json!({"reason":"holder_lost","exit_code":null,"signal":null,"finished_ms":null}),
            ),
        )
        .unwrap();
    let record = &state.terminals["t1"].facts;
    assert_eq!(record["state"], "live");
    assert!(!record.contains_key("pid"));
    assert!(!record.contains_key("foreground"));
    state
        .notification(
            1,
            event(
                10,
                "restarted",
                json!({"address":"t1","state":"live","pid":456,"status":null}),
            ),
        )
        .unwrap();
    assert_eq!(state.terminals["t1"].facts["pid"], 456);
    assert!(!state.terminals["t1"].facts.contains_key("reason"));
}

#[test]
fn hc_projection_status_is_reported_not_screen_inferred() {
    let mut state = projection();
    state
        .notification(
            1,
            event(
                9,
                "status",
                json!({"status":null,"records":{},"reported":false}),
            ),
        )
        .unwrap();
    assert!(state.terminals["t1"].facts["status"].is_null());
    assert_eq!(state.terminals["t1"].facts["reported"], false);
    state
        .notification(1, event(10, "turn", json!({"turn":3,"state":"blocked"})))
        .unwrap();
    assert_eq!(state.terminals["t1"].facts["turn"], 3);
    assert!(state.terminals["t1"].facts["status"].is_null());
}

#[test]
fn hc_projection_missed_replaces_and_reconnect_accepts_reset_sequence() {
    let mut state = projection();
    let change = state
        .notification(
            1,
            json!({"method":"missed","params":{"seq":20,"terminals":[]}}),
        )
        .unwrap();
    assert_eq!(change, Change::Snapshot);
    assert!(state.terminals.is_empty());
    assert_eq!(
        state
            .notification(1, event(19, "created", json!({"address":"t1"})))
            .unwrap(),
        Change::Ignored
    );
    state.begin("core-a".into(), 2);
    state
        .snapshot(2, snapshot(0, json!([{"address":"t1"}])))
        .unwrap();
    assert_eq!(
        state
            .notification(1, event(100, "removed", json!({})))
            .unwrap(),
        Change::Ignored
    );
    assert!(state.terminals.contains_key("t1"));
    state.begin("different-core".into(), 3);
    assert!(state.terminals.is_empty());
}

#[test]
fn hc_projection_unknown_kinds_do_not_change_status_and_missing_records_need_snapshot() {
    let mut state = projection();
    assert_eq!(
        state
            .notification(1, event(9, "future-kind", json!({})))
            .unwrap(),
        Change::Ignored
    );
    state
        .notification(1, event(10, "removed", json!({})))
        .unwrap();
    assert_eq!(
        state
            .notification(1, event(11, "title", json!({"title":"x"})))
            .unwrap(),
        Change::NeedsSnapshot
    );
}

#[test]
fn hc_invalid_snapshot_is_atomic() {
    let mut state = projection();
    assert!(state
        .snapshot(1, snapshot(20, json!([{"address":"t2"},{"address":"t2"}])))
        .is_err());
    assert!(state.terminals.contains_key("t1"));
    assert!(!state.terminals.contains_key("t2"));
}

#[test]
fn hc_finished_keeps_record_until_removed_and_label_filter_can_add_record() {
    let mut state = projection();
    state
        .notification(
            1,
            event(
                9,
                "finished",
                json!({"reason":"exited","exit_code":0,"signal":null,"finished_ms":1}),
            ),
        )
        .unwrap();
    assert_eq!(state.terminals["t1"].facts["state"], "finished");
    assert!(!state.terminals["t1"].facts.contains_key("foreground"));
    state
        .notification(1, event(10, "removed", json!({})))
        .unwrap();
    state
        .notification(
            1,
            event(
                11,
                "labels",
                json!({"labels":{"x":"y"},"record":{"address":"t1","labels":{"x":"y"}}}),
            ),
        )
        .unwrap();
    assert_eq!(state.terminals["t1"].facts["labels"]["x"], "y");
}

pub(super) async fn read_frame(stream: &mut DuplexStream) -> Frame {
    let mut header = [0u8; 9];
    stream.read_exact(&mut header).await.unwrap();
    let len = u32::from_le_bytes([header[5], header[6], header[7], header[8]]) as usize;
    let mut buffer = BytesMut::from(header.as_slice());
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.unwrap();
    buffer.extend_from_slice(&payload);
    wire::decode(&mut buffer).unwrap().unwrap()
}

pub(super) fn test_client() -> (Client, DuplexStream) {
    let (client, server) = duplex(4096);
    let (reader, writer) = split(client);
    let (outgoing, incoming) = mpsc::channel(QUEUE);
    let connection = tokio::spawn(async move {
        let _ = run_connection(reader, writer, incoming).await;
    });
    (
        Client {
            hello: Arc::new(Hello {
                generation: 1,
                framed: true,
                core: CoreInfo {
                    id: "core-test".into(),
                    name: "test".into(),
                },
                version: "test".into(),
                methods: [
                    "terminal.run",
                    "terminal.attach",
                    "terminal.list",
                    "events.subscribe",
                    "terminal.kill",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                features: ["automatic_input".into()].into_iter().collect(),
            }),
            next_id: Arc::new(AtomicU64::new(2)),
            outgoing,
            connection: connection.abort_handle(),
        },
        server,
    )
}

#[tokio::test]
async fn hc_registers_attach_before_coalesced_first_paint_and_preserves_input_provenance() {
    let (client, mut server) = test_client();
    let open = tokio::spawn(async move {
        client
            .open(
                "terminal.attach",
                json!({"terminal":"t1","cols":80,"rows":24}),
            )
            .await
            .unwrap()
    });
    let request = read_frame(&mut server).await;
    let request: Value = serde_json::from_slice(&request.payload).unwrap();
    let response = Frame {
        channel: 0,
        kind: DATA,
        payload: Bytes::from(
            json!({"id":request["id"],"result":{"channel":7,"automatic_input":true}}).to_string(),
        ),
    }
    .encode()
    .unwrap();
    let paint = Frame {
        channel: 7,
        kind: DATA,
        payload: Bytes::from_static(b"paint"),
    }
    .encode()
    .unwrap();
    server.write_all(&[response, paint].concat()).await.unwrap();
    let (_, mut stream) = open.await.unwrap();
    assert_eq!(stream.recv().await.unwrap().payload, "paint");
    stream
        .input(Bytes::from_static(b"human"), false)
        .await
        .unwrap();
    assert_eq!(read_frame(&mut server).await.kind, DATA);
    stream
        .input(Bytes::from_static(b"reply"), true)
        .await
        .unwrap();
    assert_eq!(read_frame(&mut server).await.kind, AUTOMATIC_INPUT);
    stream.consumed(5).await.unwrap();
    let credit = read_frame(&mut server).await;
    assert_eq!(credit.kind, CREDIT);
    assert_eq!(credit.payload.as_ref(), 5u32.to_le_bytes());
    drop(stream);
    let close = read_frame(&mut server).await;
    assert_eq!(close.kind, CLOSE);
    assert!(close.payload.is_empty());
}

#[tokio::test]
async fn hc_automatic_input_uses_legacy_data_when_not_negotiated() {
    let (outgoing, mut receive) = mpsc::channel(4);
    let (_, incoming) = mpsc::channel(1);
    let stream = Stream {
        channel: 1,
        incoming,
        outgoing,
        automatic_input: false,
        executor: tokio::runtime::Handle::current(),
    };
    stream
        .input(Bytes::from_static(b"reply"), true)
        .await
        .unwrap();
    let Some(Outgoing::Frame(frame)) = receive.recv().await else {
        panic!("frame expected");
    };
    assert_eq!(frame.kind, DATA);
}

#[tokio::test]
async fn hc_automatic_input_negotiation_requires_both_confirmations() {
    for hello_support in [false, true] {
        for attach_support in [false, true] {
            let (mut client, mut server) = test_client();
            if !hello_support {
                Arc::get_mut(&mut client.hello).unwrap().features.clear();
            }
            let open =
                tokio::spawn(
                    async move { client.open("terminal.attach", json!({})).await.unwrap() },
                );
            let request: Value =
                serde_json::from_slice(&read_frame(&mut server).await.payload).unwrap();
            let reply = Frame { channel: 0, kind: DATA, payload: Bytes::from(json!({
                "id": request["id"], "result": {"channel": 5, "automatic_input": attach_support}
            }).to_string()) };
            server.write_all(&reply.encode().unwrap()).await.unwrap();
            let (_, stream) = open.await.unwrap();
            assert_eq!(stream.automatic_input, hello_support && attach_support);
        }
    }
}

#[tokio::test]
async fn hc_event_json_can_span_frames_and_share_a_frame() {
    let (outgoing, _receive) = mpsc::channel(8);
    let (send, incoming) = mpsc::channel(8);
    let stream = Stream {
        channel: 8,
        incoming,
        outgoing,
        automatic_input: false,
        executor: tokio::runtime::Handle::current(),
    };
    let mut events = Events {
        stream,
        pending: BytesMut::new(),
        credit_pending: 0,
    };
    send.send(Frame {
        channel: 8,
        kind: DATA,
        payload: Bytes::from_static(b"{\"method\":\"heart"),
    })
    .await
    .unwrap();
    send.send(Frame {
        channel: 8,
        kind: DATA,
        payload: Bytes::from_static(b"beat\"}\n{\"method\":\"event\"}\n"),
    })
    .await
    .unwrap();
    assert_eq!(events.next().await.unwrap()["method"], "heartbeat");
    assert_eq!(events.next().await.unwrap()["method"], "event");
}

#[tokio::test]
async fn hc_stream_drop_delivers_detach_after_full_queue_even_outside_runtime() {
    let (outgoing, mut receive) = mpsc::channel(1);
    outgoing.send(Outgoing::Cancel(0)).await.unwrap();
    let (_, incoming) = mpsc::channel(1);
    let stream = Stream {
        channel: 3,
        incoming,
        outgoing,
        automatic_input: false,
        executor: tokio::runtime::Handle::current(),
    };
    std::thread::spawn(move || drop(stream)).join().unwrap();
    assert!(matches!(receive.recv().await, Some(Outgoing::Cancel(0))));
    let close = timeout(Duration::from_secs(1), receive.recv())
        .await
        .unwrap();
    assert!(matches!(
        close,
        Some(Outgoing::Frame(Frame {
            channel: 3,
            kind: CLOSE,
            ..
        }))
    ));
}

#[tokio::test]
async fn hc_cancelled_stream_rpc_closes_late_channel_without_touching_programs() {
    let (client, mut server) = test_client();
    let task = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe(json!({})).await }
    });
    let request: Value = serde_json::from_slice(&read_frame(&mut server).await.payload).unwrap();
    assert_eq!(request["method"], "events.subscribe");
    task.abort();
    assert!(matches!(task.await,Err(error) if error.is_cancelled()));
    super::runtime_tests::reply(
        &mut server,
        request["id"].clone(),
        json!({"channel":2,"seq":0,"terminals":[]}),
    )
    .await;
    let close = timeout(Duration::from_secs(1), read_frame(&mut server))
        .await
        .expect("cancelled stream was not closed");
    assert_eq!((close.channel, close.kind), (2, CLOSE));
}

#[tokio::test]
async fn hc_cancelled_event_read_keeps_credit_debt() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let (outgoing, mut receive) = mpsc::channel(1);
    outgoing.send(Outgoing::Cancel(0)).await.unwrap();
    let (send, incoming) = mpsc::channel(1);
    let mut events = Events {
        stream: Stream {
            channel: 8,
            incoming,
            outgoing,
            automatic_input: false,
            executor: tokio::runtime::Handle::current(),
        },
        pending: BytesMut::new(),
        credit_pending: 0,
    };
    let payload = Bytes::from_static(b"{\"method\":\"heartbeat\"}\n");
    send.send(Frame {
        channel: 8,
        kind: DATA,
        payload: payload.clone(),
    })
    .await
    .unwrap();
    let mut next = Box::pin(events.next());
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(next.as_mut().poll(&mut context), Poll::Pending));
    drop(next); // The worker may select input/resize while CREDIT is backpressured.
    assert_eq!(events.credit_pending, payload.len());
    receive.recv().await.unwrap();
    assert_eq!(events.next().await.unwrap()["method"], "heartbeat");
    let Some(Outgoing::Frame(frame)) = receive.recv().await else {
        panic!("CREDIT expected");
    };
    assert_eq!(frame.kind, CREDIT);
    assert_eq!(frame.payload.as_ref(), (payload.len() as u32).to_le_bytes());
}

#[tokio::test]
async fn hc_transport_drop_does_not_emit_terminal_kill() {
    let (client, mut server) = test_client();
    drop(client);
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(1), server.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes.is_empty());
}

#[tokio::test]
async fn hc_slow_stream_does_not_block_control_rpc() {
    let (client, mut server) = test_client();
    let caller = client.clone();
    let open =
        tokio::spawn(async move { caller.open("terminal.attach", json!({})).await.unwrap() });
    let request: Value = serde_json::from_slice(&read_frame(&mut server).await.payload).unwrap();
    let reply = Frame {
        channel: 0,
        kind: DATA,
        payload: Bytes::from(
            json!({
                "id": request["id"], "result": {"channel": 9}
            })
            .to_string(),
        ),
    };
    server.write_all(&reply.encode().unwrap()).await.unwrap();
    let (_, _stream) = open.await.unwrap();
    let frame = Frame {
        channel: 9,
        kind: DATA,
        payload: Bytes::from_static(b"x"),
    }
    .encode()
    .unwrap();
    for _ in 0..=QUEUE {
        server.write_all(&frame).await.unwrap();
    }
    let close = timeout(Duration::from_secs(1), read_frame(&mut server))
        .await
        .unwrap();
    assert_eq!((close.channel, close.kind), (9, CLOSE));
    let list = tokio::spawn(async move { client.call("terminal.list", json!({})).await });
    let request: Value = serde_json::from_slice(&read_frame(&mut server).await.payload).unwrap();
    let reply = Frame {
        channel: 0,
        kind: DATA,
        payload: Bytes::from(
            json!({
                "id": request["id"], "result": {"terminals": []}
            })
            .to_string(),
        ),
    };
    server.write_all(&reply.encode().unwrap()).await.unwrap();
    assert!(list.await.unwrap().is_ok());
}

#[tokio::test]
async fn hc_rejected_rpc_keeps_connection_usable() {
    let (client, mut server) = test_client();
    let caller = client.clone();
    let failed = tokio::spawn(async move { caller.call("terminal.list", json!({})).await });
    let request: Value = serde_json::from_slice(&read_frame(&mut server).await.payload).unwrap();
    let error = Frame {
        channel: 0,
        kind: DATA,
        payload: Bytes::from(
            json!({"id":request["id"],"error":{"message":"not available"}}).to_string(),
        ),
    };
    server.write_all(&error.encode().unwrap()).await.unwrap();
    assert!(failed
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("not available"));
    let good = tokio::spawn(async move { client.call("terminal.list", json!({})).await });
    let request: Value = serde_json::from_slice(&read_frame(&mut server).await.payload).unwrap();
    let response = Frame {
        channel: 0,
        kind: DATA,
        payload: Bytes::from(json!({"id":request["id"],"result":{"terminals":[]}}).to_string()),
    };
    server.write_all(&response.encode().unwrap()).await.unwrap();
    assert_eq!(good.await.unwrap().unwrap()["terminals"], json!([]));
}
