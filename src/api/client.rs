use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;
use serde::de::DeserializeOwned;

use crate::api::schema::{
    ErrorResponse, Method, PingParams, Request, ResponseResult, SuccessResponse,
};
use crate::ipc::LocalStream;

/// API connection target resolved by clients at the process edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionTarget {
    LocalSession(Option<String>),
    SocketPath(PathBuf),
}

impl ConnectionTarget {
    fn socket_path(&self) -> PathBuf {
        match self {
            Self::LocalSession(None) => crate::api::socket_path(),
            Self::LocalSession(Some(name)) => crate::session::api_socket_path_for(Some(name)),
            Self::SocketPath(path) => path.clone(),
        }
    }
}

/// Reusable client for Herdr's newline-delimited JSON API.
#[derive(Debug, Clone)]
pub struct ApiClient {
    target: ConnectionTarget,
    source_instance: Option<String>,
}

impl ApiClient {
    pub fn local() -> Self {
        Self::for_target(ConnectionTarget::LocalSession(None))
    }

    pub fn for_target(target: ConnectionTarget) -> Self {
        Self {
            target,
            source_instance: None,
        }
    }

    pub(crate) fn with_source_instance(mut self, instance: String) -> Self {
        self.source_instance = Some(instance);
        self
    }

    pub fn socket_path(&self) -> PathBuf {
        self.target.socket_path()
    }

    pub fn request(&self, request: Request) -> Result<SuccessResponse, ApiClientError> {
        let value = self.request_value(&request)?;
        parse_response_value(value)
    }

    pub fn request_value(&self, request: &Request) -> Result<serde_json::Value, ApiClientError> {
        let mut stream = self.connect()?;
        write_request(&mut stream, request, self.source_instance.as_deref())?;

        let mut reader = BufReader::new(stream);
        read_json_line(&mut reader)
    }

    pub fn request_value_with_timeout(
        &self,
        request: &Request,
        timeout: Duration,
    ) -> Result<serde_json::Value, ApiClientError> {
        self.request_value_cancellable(request, timeout, &AtomicBool::new(false))
    }

    pub(crate) fn request_value_cancellable(
        &self,
        request: &Request,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<serde_json::Value, ApiClientError> {
        let deadline = Instant::now() + timeout;
        let mut stream =
            crate::ipc::connect_local_stream_cancellable(&self.socket_path(), cancelled, deadline)?;
        stream.set_nonblocking(true)?;
        let mut stream = DeadlineStream {
            stream: &mut stream,
            deadline,
            cancelled,
        };
        write_request(&mut stream, request, self.source_instance.as_deref())?;
        read_json_line(&mut BufReader::new(stream))
    }

    pub fn status(&self) -> Result<crate::api::RuntimeStatus, ApiClientError> {
        self.read_status(None)
    }

    pub(crate) fn status_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<crate::api::RuntimeStatus, ApiClientError> {
        self.read_status(Some(timeout))
    }

    fn read_status(
        &self,
        timeout: Option<Duration>,
    ) -> Result<crate::api::RuntimeStatus, ApiClientError> {
        let request = Request {
            id: "api-client:status".into(),
            method: Method::Ping(PingParams::default()),
        };
        let response = match timeout {
            Some(timeout) => {
                parse_response_value(self.request_value_with_timeout(&request, timeout)?)?
            }
            None => self.request(request)?,
        };
        match response.result {
            ResponseResult::Pong {
                version,
                protocol,
                capabilities,
            } => Ok(crate::api::RuntimeStatus {
                version: Some(version),
                protocol: Some(protocol),
                capabilities,
            }),
            result => Err(ApiClientError::UnexpectedResult(format!("{result:?}"))),
        }
    }

    fn connect(&self) -> io::Result<LocalStream> {
        crate::ipc::connect_local_stream(&self.socket_path())
    }
}

#[derive(Debug)]
pub enum ApiClientError {
    Io(io::Error),
    Json(serde_json::Error),
    ErrorResponse(ErrorResponse),
    EmptyResponse,
    UnexpectedResult(String),
}

impl fmt::Display for ApiClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::ErrorResponse(response) => write!(f, "{}", response.error.message),
            Self::EmptyResponse => write!(f, "empty api response"),
            Self::UnexpectedResult(result) => write!(f, "unexpected api result: {result}"),
        }
    }
}

impl std::error::Error for ApiClientError {}

impl From<io::Error> for ApiClientError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for ApiClientError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

fn write_request(
    stream: &mut impl Write,
    request: &Request,
    source_instance: Option<&str>,
) -> Result<(), ApiClientError> {
    // Source discovery is unscoped; every subsequent request is pinned to that host instance.
    let special = matches!(request.method, Method::RuntimeSource(_));
    let encoded = match source_instance.filter(|_| !special) {
        Some(instance) => serde_json::to_string(
            &serde_json::json!({"id":request.id,"method":"runtime.request","params":{"host_instance":instance,"request":request}}),
        )?,
        None => serde_json::to_string(request)?,
    };
    stream.write_all(encoded.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

struct DeadlineStream<'a> {
    stream: &'a mut LocalStream,
    deadline: Instant,
    cancelled: &'a AtomicBool,
}

impl DeadlineStream<'_> {
    fn check(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "API request cancelled",
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "API request timed out",
            ));
        }
        Ok(())
    }
}

impl Write for DeadlineStream<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            self.check()?;
            match self.stream.write(buffer) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(2))
                }
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // Writes are unbuffered; pipe flushing would wait for the peer to read.
        self.check()
    }
}

impl Read for DeadlineStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            self.check()?;
            // Windows named pipes have no read timeout; peek-before-read keeps
            // both idle and partial responses subject to the same deadline.
            match crate::ipc::poll_local_stream_read_count(self.stream, buffer)? {
                crate::ipc::LocalStreamReadCount::Data(count) => return Ok(count),
                crate::ipc::LocalStreamReadCount::Closed => return Ok(0),
                crate::ipc::LocalStreamReadCount::Pending => {
                    std::thread::sleep(Duration::from_millis(2))
                }
            }
        }
    }
}

fn read_json_line<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<T, ApiClientError> {
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 || line.trim().is_empty() {
        return Err(ApiClientError::EmptyResponse);
    }
    serde_json::from_str(&line).map_err(ApiClientError::Json)
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WireResponse {
    Success(Box<SuccessResponse>),
    Error(ErrorResponse),
}

pub(crate) fn parse_response_value(
    value: serde_json::Value,
) -> Result<SuccessResponse, ApiClientError> {
    match serde_json::from_value(value)? {
        WireResponse::Success(response) => Ok(*response),
        WireResponse::Error(response) => Err(ApiClientError::ErrorResponse(response)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_session_target_resolves_named_session_socket() {
        let client = ApiClient::for_target(ConnectionTarget::LocalSession(Some("work".into())));
        assert!(client.socket_path().ends_with("sessions/work/herdr.sock"));
    }

    #[test]
    fn api_cancellation_closes_a_partial_response_before_its_deadline() {
        #[cfg(unix)]
        use interprocess::local_socket::traits::Listener as _;
        let path =
            std::env::temp_dir().join(format!("herdr-api-cancel-{}.sock", std::process::id()));
        let listener = crate::ipc::bind_private_local_listener(&path).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream).read_line(&mut line).unwrap();
            stream.write_all(b"{\"id\":").unwrap();
            ready_tx.send(()).unwrap();
            let _ = done_rx.recv_timeout(Duration::from_secs(2));
        });
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let worker = std::thread::spawn(move || {
            client.request_value_cancellable(
                &Request {
                    id: "cancelled-probe".into(),
                    method: Method::Ping(PingParams::default()),
                },
                Duration::from_secs(60),
                &stop,
            )
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        cancelled.store(true, Ordering::Release);
        let result = worker.join().unwrap();
        let _ = done_tx.send(());
        server.join().unwrap();
        assert!(
            matches!(result, Err(ApiClientError::Io(error)) if error.kind() == io::ErrorKind::ConnectionAborted)
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn status_timeout_closes_a_stalled_probe() {
        #[cfg(unix)]
        use interprocess::local_socket::traits::Listener as _;
        let path =
            std::env::temp_dir().join(format!("herdr-status-timeout-{}.sock", std::process::id()));
        let listener = crate::ipc::bind_private_local_listener(&path).unwrap();
        let server = std::thread::spawn(move || {
            let stream = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap()["method"],
                "ping"
            );
            std::thread::sleep(Duration::from_millis(300));
        });
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let error = client
            .status_with_timeout(Duration::from_millis(100))
            .unwrap_err();
        assert!(
            matches!(error, ApiClientError::Io(error) if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock))
        );
        server.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn socket_path_target_uses_explicit_path() {
        let path = PathBuf::from("/tmp/herdr-test.sock");
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        assert_eq!(client.socket_path(), path);
    }
}
