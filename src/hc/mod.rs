//! Native upstream hc API boundary. hc's generation 1 is not Herdr's endpoint generation 1.
//!
//! Connection/attachment lifetime never owns core's programs. Only an explicit
//! terminal.kill request may end one. No Python bridge, PTY, or hc source dependency.

pub(crate) mod source;
pub mod state;
pub mod wire;

use bytes::{Bytes, BytesMut};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot},
    time::timeout,
};
use wire::{Frame, AUTOMATIC_INPUT, CLOSE, CREDIT, DATA, FACTS, FOCUS, RESIZE};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const QUEUE: usize = 32;
const MAX_PENDING: usize = 64;

#[derive(Debug, Clone, Deserialize)]
pub struct CoreInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct Hello {
    pub generation: u32,
    pub framed: bool,
    pub core: CoreInfo,
    pub version: String,
    pub methods: HashSet<String>,
    #[serde(default)]
    pub features: HashSet<String>,
}

struct Pending {
    reply: oneshot::Sender<io::Result<Value>>,
    stream: Option<mpsc::Sender<Frame>>,
}

enum Outgoing {
    Request {
        id: u64,
        method: String,
        params: Value,
        pending: Pending,
    },
    Frame(Frame),
    Cancel(u64),
}

#[derive(Clone)]
pub struct Client {
    pub hello: Arc<Hello>,
    next_id: Arc<AtomicU64>,
    outgoing: mpsc::Sender<Outgoing>,
    connection: tokio::task::AbortHandle,
}

pub struct Events {
    stream: Stream,
    pending: BytesMut,
    credit_pending: usize,
}

pub struct Stream {
    channel: u32,
    incoming: mpsc::Receiver<Frame>,
    outgoing: mpsc::Sender<Outgoing>,
    automatic_input: bool,
    executor: tokio::runtime::Handle,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn disconnected() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "hc transport disconnected; terminal was not killed",
    )
}

fn result(reply: Value) -> io::Result<Value> {
    if let Some(error) = reply.get("error") {
        return Err(io::Error::other(format!("hc RPC rejected: {error}")));
    }
    reply
        .get("result")
        .filter(|value| value.is_object())
        .cloned()
        .ok_or_else(|| invalid("hc RPC response has no result object"))
}

impl Client {
    /// `command` is hc connect, or ssh -T ... hc connect. It is not a shell or PTY.
    pub async fn connect(mut command: Command) -> io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| invalid("hc has no stdin"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| invalid("hc has no stdout"))?;
        let mut reader = BufReader::new(output);
        let hello: Hello = timeout(REQUEST_TIMEOUT, async {
            let request = json!({"jsonrpc":"2.0", "id":1, "method":"hello", "params":{
                "generation":1, "framed":true, "features":["automatic_input"]
            }});
            input.write_all(format!("{request}\n").as_bytes()).await?;
            let mut line = Vec::new();
            loop {
                let byte = reader.read_u8().await?;
                if byte == b'\n' {
                    break;
                }
                if line.len() >= wire::MAX_PAYLOAD {
                    return Err(invalid("hc hello exceeds 16 MiB"));
                }
                line.push(byte);
            }
            let reply: Value = serde_json::from_slice(&line).map_err(invalid_json)?;
            if reply.get("id").and_then(Value::as_u64) != Some(1) {
                return Err(invalid("hc hello response has the wrong id"));
            }
            let hello: Hello = serde_json::from_value(result(reply)?).map_err(invalid_json)?;
            if hello.generation != 1 || !hello.framed || hello.core.id.is_empty() {
                return Err(invalid(
                    "hc did not negotiate framed generation 1 with a core identity",
                ));
            }
            Ok(hello)
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "hc hello timed out"))??;
        let (outgoing, receiver) = mpsc::channel(QUEUE);
        let connection = tokio::spawn(async move {
            let outcome = run_connection(reader, input, receiver).await;
            if let Err(error) = outcome {
                tracing::debug!(%error, "hc connection ended");
            }
            // This process is only the transport. Never stop the core server.
            stop_transport(&mut child).await;
        });
        Ok(Self {
            hello: Arc::new(hello),
            next_id: Arc::new(AtomicU64::new(2)),
            outgoing,
            connection: connection.abort_handle(),
        })
    }

    pub(crate) fn disconnect(&self) {
        self.connection.abort();
    }
    pub(crate) async fn closed(&self) {
        self.outgoing.closed().await;
    }

    pub async fn call(&self, method: &str, params: Value) -> io::Result<Value> {
        self.request(method, params, None).await
    }

    pub async fn subscribe(&self, params: Value) -> io::Result<(state::Snapshot, Events)> {
        let (reply, stream) = self.open("events.subscribe", params).await?;
        let snapshot = serde_json::from_value(reply).map_err(invalid_json)?;
        Ok((
            snapshot,
            Events {
                stream,
                pending: BytesMut::new(),
                credit_pending: 0,
            },
        ))
    }

    /// Registers a new stream before waking the RPC caller: first paint/events
    /// may follow the response in the same transport read.
    pub async fn open(&self, method: &str, params: Value) -> io::Result<(Value, Stream)> {
        if method != "terminal.attach" && method != "events.subscribe" {
            return Err(invalid("only attach/subscribe open hc channels"));
        }
        let (sender, incoming) = mpsc::channel(QUEUE);
        let reply = self.request(method, params, Some(sender)).await?;
        let channel = channel_number(&reply)?;
        let automatic_input = method == "terminal.attach"
            && self.hello.features.contains("automatic_input")
            && reply.get("automatic_input").and_then(Value::as_bool) == Some(true);
        Ok((
            reply,
            Stream {
                channel,
                incoming,
                outgoing: self.outgoing.clone(),
                automatic_input,
                executor: tokio::runtime::Handle::current(),
            },
        ))
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        stream: Option<mpsc::Sender<Frame>>,
    ) -> io::Result<Value> {
        if !self.hello.methods.contains(method) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("hc does not advertise {method}"),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, receive) = oneshot::channel();
        let command = Outgoing::Request {
            id,
            method: method.to_owned(),
            params,
            pending: Pending { reply, stream },
        };
        let outcome = timeout(REQUEST_TIMEOUT, async {
            self.outgoing
                .send(command)
                .await
                .map_err(|_| disconnected())?;
            receive.await.map_err(|_| disconnected())?
        })
        .await;
        match outcome {
            Ok(value) => value,
            Err(_) => {
                // Prune without waiting on a stalled writer. Never retry an uncertain run/kill.
                let _ = self.outgoing.try_send(Outgoing::Cancel(id));
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "hc {method} timed out; outcome may be unknown, do not retry non-idempotent requests"
                    ),
                ))
            }
        }
    }
}

impl Events {
    /// Notifications are newline-delimited JSON bytes, not one JSON per DATA frame.
    pub async fn next(&mut self) -> io::Result<Value> {
        loop {
            // Keep the debt if select! cancels while the transport queue is full.
            if self.credit_pending != 0 {
                self.stream.consumed(self.credit_pending).await?;
                self.credit_pending = 0;
            }
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                if end > wire::MAX_PAYLOAD {
                    return Err(invalid("hc event exceeds 16 MiB"));
                }
                let line = self.pending.split_to(end + 1);
                return serde_json::from_slice(&line).map_err(invalid_json);
            }
            if self.pending.len() > wire::MAX_PAYLOAD {
                return Err(invalid("hc event exceeds 16 MiB"));
            }
            let frame = self.stream.recv().await?;
            match frame.kind {
                DATA => {
                    self.pending.extend_from_slice(&frame.payload);
                    self.credit_pending += frame.payload.len();
                }
                CREDIT => {}
                CLOSE => return Err(disconnected()),
                _ => return Err(invalid("unexpected hc event stream frame")),
            }
        }
    }
}

impl Stream {
    pub async fn recv(&mut self) -> io::Result<Frame> {
        self.incoming.recv().await.ok_or_else(disconnected)
    }

    async fn send(&self, kind: u8, payload: Bytes) -> io::Result<()> {
        if payload.len() > wire::MAX_PAYLOAD {
            return Err(invalid("hc frame exceeds 16 MiB"));
        }
        self.outgoing
            .send(Outgoing::Frame(Frame {
                channel: self.channel,
                kind,
                payload,
            }))
            .await
            .map_err(|_| disconnected())
    }

    pub async fn input(&self, bytes: Bytes, automatic: bool) -> io::Result<()> {
        let kind = if automatic && self.automatic_input {
            AUTOMATIC_INPUT
        } else {
            DATA
        };
        self.send(kind, bytes).await
    }

    /// Credit is returned only after the viewer has consumed the payload.
    pub async fn consumed(&self, bytes: usize) -> io::Result<()> {
        let amount = u32::try_from(bytes).map_err(|_| invalid("invalid hc credit"))?;
        self.send(CREDIT, Bytes::copy_from_slice(&amount.to_le_bytes()))
            .await
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        if cols == 0 || rows == 0 {
            return Err(invalid("hc terminal dimensions must be nonzero"));
        }
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&cols.to_le_bytes());
        payload.extend_from_slice(&rows.to_le_bytes());
        self.send(RESIZE, Bytes::from(payload)).await
    }

    pub async fn focus(&self, focused: bool) -> io::Result<()> {
        self.send(FOCUS, Bytes::from(vec![u8::from(focused)])).await
    }

    pub async fn display(&self, facts: Value) -> io::Result<()> {
        if !facts.is_object() {
            return Err(invalid("hc display facts must be an object"));
        }
        self.send(
            FACTS,
            Bytes::from(serde_json::to_vec(&facts).map_err(invalid_json)?),
        )
        .await
    }

    pub async fn detach(&self) -> io::Result<()> {
        self.send(CLOSE, Bytes::new()).await
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let close = Outgoing::Frame(Frame {
            channel: self.channel,
            kind: CLOSE,
            payload: Bytes::new(),
        });
        if let Err(mpsc::error::TrySendError::Full(close)) = self.outgoing.try_send(close) {
            let outgoing = self.outgoing.clone();
            // Keep the detach until capacity returns or this exact transport closes.
            self.executor.spawn(async move {
                let _ = outgoing.send(close).await;
            });
        }
    }
}

fn invalid_json(error: serde_json::Error) -> io::Error {
    invalid(error.to_string())
}

fn channel_number(reply: &Value) -> io::Result<u32> {
    reply
        .get("channel")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n != 0)
        .ok_or_else(|| invalid("hc returned an invalid channel"))
}

async fn write_transport(writer: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> io::Result<()> {
    timeout(REQUEST_TIMEOUT, writer.write_all(bytes))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "hc transport write stalled"))?
}

async fn stop_transport(child: &mut Child) {
    if let Err(error) = child.kill().await {
        tracing::debug!(%error, "hc transport already exited");
    }
}

async fn run_connection(
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    mut outgoing: mpsc::Receiver<Outgoing>,
) -> io::Result<()> {
    let mut pending: HashMap<u64, Pending> = HashMap::new();
    let mut streams: HashMap<u32, mpsc::Sender<Frame>> = HashMap::new();
    let mut buffer = BytesMut::new();
    loop {
        tokio::select! {
            command = outgoing.recv() => match command {
                None => return Ok(()),
                Some(Outgoing::Cancel(id)) => { pending.remove(&id); }
                Some(Outgoing::Frame(frame)) => {
                    if frame.kind==CLOSE {streams.remove(&frame.channel);}
                    write_transport(&mut writer, &frame.encode()?).await?;
                }
                Some(Outgoing::Request { id, method, params, pending: request }) => {
                    pending.retain(|_, request| !request.reply.is_closed());
                    if pending.len() >= MAX_PENDING {
                        let _ = request.reply.send(Err(io::Error::new(io::ErrorKind::WouldBlock, "too many pending hc requests")));
                        continue;
                    }
                    if request.reply.is_closed() { continue; }
                    let value = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
                    let frame = Frame { channel: 0, kind: DATA, payload: Bytes::from(serde_json::to_vec(&value).map_err(invalid_json)?) };
                    let encoded = match frame.encode() {
                        Ok(encoded) => encoded,
                        Err(error) => { let _ = request.reply.send(Err(error)); continue; }
                    };
                    write_transport(&mut writer, &encoded).await?;
                    pending.insert(id, request);
                }
            },
            read = reader.read_buf(&mut buffer) => {
                if read? == 0 { return Err(disconnected()); }
                while let Some(frame) = wire::decode(&mut buffer)? {
                    if frame.channel == 0 {
                        if frame.kind != DATA { return Err(invalid("unexpected hc control frame kind")); }
                        let reply: Value = serde_json::from_slice(&frame.payload).map_err(invalid_json)?;
                        let id = reply.get("id").and_then(Value::as_u64).ok_or_else(|| invalid("hc control reply has no request id"))?;
                        let Some(request) = pending.remove(&id) else {
                            // A cancelled/timed-out stream RPC can still allocate a remote channel.
                            if let Ok(value)=result(reply) {
                                if let Ok(channel)=channel_number(&value) {
                                    if !streams.contains_key(&channel) {write_transport(&mut writer,&Frame{channel,kind:CLOSE,payload:Bytes::new()}.encode()?).await?;}
                                }
                            }
                            continue;
                        };
                        let mut registered=None;
                        let value = result(reply).and_then(|value| {
                            if let Some(stream) = request.stream {
                                let channel = channel_number(&value)?;
                                if streams.contains_key(&channel) { return Err(invalid("hc reused a live channel")); }
                                streams.insert(channel, stream);
                                registered=Some(channel);
                            }
                            Ok(value)
                        });
                        if request.reply.send(value).is_err() {
                            if let Some(channel)=registered {
                                streams.remove(&channel);
                                write_transport(&mut writer,&Frame{channel,kind:CLOSE,payload:Bytes::new()}.encode()?).await?;
                            }
                        }
                    } else if let Some(stream) = streams.get(&frame.channel) {
                        let channel = frame.channel;
                        let closed = frame.kind == CLOSE;
                        if stream.try_send(frame).is_err() {
                            // A slow viewer never blocks RPC or other channels. No discarded DATA is credited.
                            streams.remove(&channel);
                            write_transport(&mut writer, &Frame { channel, kind: CLOSE, payload: Bytes::new() }.encode()?).await?;
                        } else if closed {
                            streams.remove(&channel);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod app_tests;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod reconnect_tests;
#[cfg(test)]
mod runtime_tests;
#[cfg(test)]
mod status_tests;
#[cfg(test)]
mod tests;
