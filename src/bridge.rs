//! The plugin bridge: a program herdr-native started on a remote machine (a
//! plugin pane there, such as herdr-sidebar) calls this presenter's API.
//!
//! Remote programs cannot reach this machine, so a relay runs on the remote
//! machine in a hidden hc terminal (`herdr-native.hidden=1`). It listens on a
//! unix socket there; each connection is one request line, written to the
//! terminal's output as `HNB Q <id> <base64>`. The presenter attached to that
//! terminal runs the request against its own API (only the methods a plugin
//! pane needs) and answers through the terminal's input as
//! `HNB R <id> <base64>`. The relay draws on the alternate screen, so the
//! terminal keeps no history of the traffic.

use base64::Engine as _;

/// API methods a remote plugin pane may call through the bridge: what reads
/// and arranges panes and tabs. Nothing that runs programs on this machine,
/// changes its configuration, or stops servers.
const ALLOWED_METHODS: &[&str] = &[
    "pane.list",
    "pane.get",
    "pane.layout",
    "pane.focus",
    "pane.rename",
    "pane.close",
    "pane.resize",
    "pane.move",
    "pane.split",
    "pane.read",
    "pane.send_input",
    "pane.send_text",
    "pane.send_keys",
    "pane.report_metadata",
    "tab.list",
    "tab.get",
    "tab.focus",
    "tab.close",
    "tab.create",
    "tab.rename",
    "workspace.list",
    "workspace.get",
];

/// A request line larger than this is refused.
const MAX_REQUEST_BYTES: usize = 1 << 20;

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()
}

/// One `HNB <kind> <id> <base64>` frame found in terminal output or input.
#[derive(Debug, PartialEq, Eq)]
struct Frame {
    kind: char,
    id: u64,
    body: Vec<u8>,
}

/// Frames in `buffer` up to its last complete line; the rest stays.
fn take_frames(buffer: &mut Vec<u8>) -> Vec<Frame> {
    let Some(end) = buffer.iter().rposition(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    let lines: Vec<u8> = buffer.drain(..=end).collect();
    lines
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let text = std::str::from_utf8(line).ok()?;
            let start = text.find("HNB ")?;
            let mut parts = text[start + 4..].trim_end_matches('\r').split(' ');
            let kind = parts.next()?.chars().next()?;
            let id = parts.next()?.parse().ok()?;
            let body = decode(parts.next()?)?;
            Some(Frame { kind, id, body })
        })
        .collect()
}

fn frame_line(kind: char, id: u64, body: &[u8], end: &str) -> Vec<u8> {
    format!("HNB {kind} {id} {}{end}", encode(body)).into_bytes()
}

/// The response the presenter sends for a request it does not run.
fn refusal(code: &str, message: &str) -> Vec<u8> {
    serde_json::json!({"id": null, "error": {"code": code, "message": message}})
        .to_string()
        .into_bytes()
}

/// Whether the bridge runs a request line, and its method.
fn allowed(request: &[u8]) -> Result<(), Vec<u8>> {
    let value: serde_json::Value = serde_json::from_slice(request)
        .map_err(|_| refusal("invalid_request", "the bridge takes one JSON request"))?;
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if ALLOWED_METHODS.contains(&method) {
        Ok(())
    } else {
        Err(refusal(
            "forbidden",
            &format!("{method} is not available to a remote plugin pane"),
        ))
    }
}

#[cfg(all(unix, not(test)))]
pub(crate) use host::serve;
#[cfg(unix)]
pub(crate) use relay::run_relay;

#[cfg(unix)]
mod relay {
    use super::*;
    use std::{
        collections::HashMap,
        io::{BufRead, BufReader, Read, Write},
        os::unix::{
            fs::{DirBuilderExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc, Arc, Mutex,
        },
        time::Duration,
    };

    type Waiters = Arc<Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>>;

    fn usage() -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: herdr-native bridge-relay --socket <path>",
        ))
    }

    fn listen(path: &Path) -> std::io::Result<Option<UnixListener>> {
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        if path.exists() {
            // Another relay already answers there: it stays the one.
            if UnixStream::connect(path).is_ok() {
                return Ok(None);
            }
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Some(listener))
    }

    /// `herdr-native bridge-relay --socket <path>`, run in a hidden terminal
    /// on a remote machine.
    pub(crate) fn run_relay(args: &[String]) -> std::io::Result<()> {
        let [flag, socket] = args else {
            return usage();
        };
        if flag != "--socket" {
            return usage();
        }
        let socket = PathBuf::from(socket);
        let Some(listener) = listen(&socket)? else {
            return Ok(());
        };
        // Raw input: response frames arrive as typed, unechoed, unbuffered.
        let _ = crossterm::terminal::enable_raw_mode();
        let out = Arc::new(Mutex::new(std::io::stdout()));
        if let Ok(mut out) = out.lock() {
            out.write_all(b"\x1b[?1049hHNB READY 1\r\n")?;
            out.flush()?;
        }
        let waiters: Waiters = Arc::default();
        {
            let waiters = waiters.clone();
            std::thread::spawn(move || read_responses(&waiters));
        }
        let next = Arc::new(AtomicU64::new(1));
        for connection in listener.incoming() {
            let Ok(connection) = connection else {
                continue;
            };
            let (out, waiters, next) = (out.clone(), waiters.clone(), next.clone());
            std::thread::spawn(move || {
                let id = next.fetch_add(1, Ordering::Relaxed);
                let _ = answer(connection, id, &out, &waiters);
            });
        }
        Ok(())
    }

    fn read_responses(waiters: &Waiters) {
        let mut stdin = std::io::stdin();
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1 << 16];
        loop {
            let read = match stdin.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.len() > 4 * MAX_REQUEST_BYTES && !buffer.contains(&b'\n') {
                buffer.clear();
            }
            for frame in take_frames(&mut buffer) {
                if frame.kind != 'R' {
                    continue;
                }
                if let Ok(mut waiters) = waiters.lock() {
                    if let Some(waiter) = waiters.remove(&frame.id) {
                        let _ = waiter.send(frame.body);
                    }
                }
            }
        }
    }

    fn answer(
        connection: UnixStream,
        id: u64,
        out: &Arc<Mutex<std::io::Stdout>>,
        waiters: &Waiters,
    ) -> std::io::Result<()> {
        connection.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut request = Vec::new();
        BufReader::new(connection.try_clone()?)
            .take(MAX_REQUEST_BYTES as u64 + 1)
            .read_until(b'\n', &mut request)?;
        let mut writer = connection;
        if request.len() > MAX_REQUEST_BYTES {
            writer.write_all(&refusal("too_large", "request is too large"))?;
            return writer.write_all(b"\n");
        }
        let request = request.trim_ascii_end().to_vec();
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut waiters) = waiters.lock() {
            waiters.insert(id, sender);
        }
        let line = frame_line('Q', id, &request, "\r\n");
        // Sent again while unanswered: the presenter may have been away.
        let mut response = None;
        for _ in 0..5 {
            if let Ok(mut out) = out.lock() {
                out.write_all(&line)?;
                out.flush()?;
            }
            if let Ok(body) = receiver.recv_timeout(Duration::from_secs(3)) {
                response = Some(body);
                break;
            }
        }
        if let Ok(mut waiters) = waiters.lock() {
            waiters.remove(&id);
        }
        let body = response.unwrap_or_else(|| {
            refusal(
                "unavailable",
                "herdr-native did not answer through the bridge",
            )
        });
        writer.write_all(&body)?;
        writer.write_all(b"\n")
    }
}

#[cfg(all(unix, not(test)))]
mod host {
    use super::*;
    use crate::hc::Client;
    use serde_json::json;
    use std::{
        collections::{HashMap, VecDeque},
        io,
        path::Path,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    /// Labels of the relay terminal: hidden from every layout, and found again
    /// by a presenter that restarts.
    const ROLE_LABEL: &str = "herdr-native.role";
    const ROLE_BRIDGE: &str = "bridge";

    /// Starts the relay on this core, or finds the one running, and answers
    /// its requests until the transport ends.
    pub(crate) async fn serve(client: &Client, api_socket: &Path) -> io::Result<()> {
        let core = client.hello.core.id.clone();
        let address = match find_relay(client).await? {
            Some(address) => address,
            None => start_relay(client, &core).await?,
        };
        let (_, mut stream) = client
            .open(
                "terminal.attach",
                json!({"terminal": address, "cols": 200, "rows": 24, "history": 0}),
            )
            .await?;
        let mut buffer = Vec::new();
        let mut answered: HashMap<u64, Vec<u8>> = HashMap::new();
        let mut order: VecDeque<u64> = VecDeque::new();
        loop {
            let frame = stream.recv().await?;
            match frame.kind {
                crate::hc::wire::DATA => {
                    stream.consumed(frame.payload.len()).await?;
                    buffer.extend_from_slice(&frame.payload);
                    if buffer.len() > 4 * MAX_REQUEST_BYTES && !buffer.contains(&b'\n') {
                        buffer.clear();
                    }
                }
                crate::hc::wire::CLOSE => {
                    return Err(io::Error::other("the bridge relay ended"));
                }
                _ => continue,
            }
            for request in take_frames(&mut buffer) {
                if request.kind != 'Q' {
                    continue;
                }
                let response = match answered.get(&request.id) {
                    Some(response) => response.clone(),
                    None => {
                        let response = match allowed(&request.body) {
                            Ok(()) => call_api(api_socket, &request.body)
                                .await
                                .unwrap_or_else(|error| refusal("unavailable", &error.to_string())),
                            Err(refusal) => refusal,
                        };
                        answered.insert(request.id, response.clone());
                        order.push_back(request.id);
                        if order.len() > 256 {
                            if let Some(old) = order.pop_front() {
                                answered.remove(&old);
                            }
                        }
                        response
                    }
                };
                stream
                    .input(frame_line('R', request.id, &response, "\n").into(), false)
                    .await?;
            }
        }
    }

    async fn find_relay(client: &Client) -> io::Result<Option<String>> {
        let list = client.call("terminal.list", json!({"all": false})).await?;
        Ok(list["terminals"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|terminal| terminal["state"] == "live")
            .find(|terminal| {
                terminal["labels"][ROLE_LABEL] == ROLE_BRIDGE
                    && terminal["labels"][crate::hc_entry::HIDDEN_LABEL] == "1"
            })
            .and_then(|terminal| terminal["address"].as_str())
            .map(str::to_owned))
    }

    /// The relay is herdr-native's own build on that machine (installed under
    /// ~/.local/share/herdr-native/bin); its socket is the one plugin panes
    /// there are given.
    const RELAY_LAUNCH: &str = r#"bin="$HOME/.local/share/herdr-native/bin/herdr-native"
if [ ! -x "$bin" ]; then
    printf 'herdr-native: the bridge needs %s on this machine\n' "$bin" >&2
    exit 127
fi
exec "$bin" bridge-relay --socket "$HOME/.local/state/herdr-native/run/$1.sock""#;

    async fn start_relay(client: &Client, core: &str) -> io::Result<String> {
        let mut labels = serde_json::Map::new();
        labels.insert(crate::hc_entry::HIDDEN_LABEL.into(), "1".into());
        labels.insert(ROLE_LABEL.into(), ROLE_BRIDGE.into());
        let reply = client
            .call(
                "terminal.run",
                json!({
                    "argv": ["sh", "-c", RELAY_LAUNCH, "herdr-native-bridge", core],
                    "size": {"rows": 24, "cols": 200},
                    "no_parent": true, "no_notify": true, "keep": false,
                    "labels": labels,
                }),
            )
            .await?;
        reply["address"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("hc did not start the bridge relay"))
    }

    /// One request line to this presenter's own API.
    async fn call_api(api_socket: &Path, request: &[u8]) -> io::Result<Vec<u8>> {
        let exchange = async {
            let mut stream = tokio::net::UnixStream::connect(api_socket).await?;
            stream.write_all(request).await?;
            stream.write_all(b"\n").await?;
            let mut reader = tokio::io::BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            Ok::<_, io::Error>(line.trim_ascii_end().to_vec())
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), exchange)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "herdr API timed out"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_survive_terminal_noise_and_partial_lines() {
        let mut buffer = b"\x1b[?1049hHNB READY 1\r\n".to_vec();
        buffer.extend(frame_line('Q', 7, b"{\"method\":\"pane.list\"}", "\r\n"));
        buffer.extend(b"HNB R 8 ");
        let frames = take_frames(&mut buffer);
        assert_eq!(
            frames,
            vec![Frame {
                kind: 'Q',
                id: 7,
                body: b"{\"method\":\"pane.list\"}".to_vec()
            }]
        );
        assert_eq!(buffer, b"HNB R 8 ");
    }

    #[test]
    fn only_methods_a_plugin_pane_needs_go_through() {
        assert!(allowed(br#"{"id":"1","method":"pane.list","params":{}}"#).is_ok());
        for method in [
            "server.stop",
            "plugin.action.invoke",
            "config.set",
            "worktree.remove",
        ] {
            let refused = allowed(format!(r#"{{"method":"{method}"}}"#).as_bytes()).unwrap_err();
            assert!(
                String::from_utf8_lossy(&refused).contains("forbidden"),
                "{method}"
            );
        }
        assert!(allowed(b"not json").is_err());
    }
}
