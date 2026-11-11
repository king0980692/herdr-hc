//! External "show this machine and focus this pane" requests.
//!
//! Which machine a client displays is client-local state, so no server API can change it. An
//! external tool (for example a pane picker running outside herdr) asks instead by atomically
//! replacing `<state dir>/client/jump-request.json`:
//!
//! ```json
//! {"version": 1, "endpoint": "<profile id or local>", "pane_id": "w30:p1", "ts": 1794291489675}
//! ```
//!
//! `workspace_id` may be given instead of `pane_id`. `ts` is Unix milliseconds; requests older
//! than [`MAX_AGE_MS`] are ignored so a leftover file never moves a client that starts later.
//! The request takes the same path as selecting a machine in the client itself.

use super::*;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_AGE_MS: u64 = 3_000;
/// Tolerated clock skew for a request stamped slightly ahead of this process.
const MAX_FUTURE_MS: u64 = 1_000;
const MAX_REQUEST_BYTES: u64 = 4 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct JumpRequest {
    pub(super) endpoint_id: endpoint::ClientEndpointId,
    pub(super) target: shell::ClientEndpointFocusTarget,
    ts: u64,
}

#[derive(serde::Deserialize)]
struct StoredJumpRequest {
    version: u32,
    endpoint: String,
    #[serde(default)]
    pane_id: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    ts: u64,
}

fn request_path() -> std::path::PathBuf {
    crate::config::state_dir()
        .join("client")
        .join("jump-request.json")
}

pub(super) fn parse(bytes: &[u8], now_ms: u64) -> Result<JumpRequest, String> {
    let stored: StoredJumpRequest =
        serde_json::from_slice(bytes).map_err(|error| format!("invalid jump request: {error}"))?;
    if stored.version != 1 {
        return Err(format!(
            "unsupported jump request version {}",
            stored.version
        ));
    }
    if now_ms.saturating_sub(stored.ts) > MAX_AGE_MS {
        return Err("jump request is stale".into());
    }
    if stored.ts.saturating_sub(now_ms) > MAX_FUTURE_MS {
        return Err("jump request is stamped in the future".into());
    }
    let endpoint_id = endpoint::ClientEndpointId::parse_storage_key(&stored.endpoint)?;
    let non_empty = |value: Option<String>| value.filter(|value| !value.is_empty());
    let target = match (non_empty(stored.pane_id), non_empty(stored.workspace_id)) {
        (Some(pane_id), _) => shell::ClientEndpointFocusTarget::Pane(pane_id),
        (None, Some(workspace_id)) => shell::ClientEndpointFocusTarget::Workspace(workspace_id),
        (None, None) => return Err("jump request names no pane or workspace".into()),
    };
    Ok(JumpRequest {
        endpoint_id,
        target,
        ts: stored.ts,
    })
}

/// Remembers the last accepted request so one file is acted on once.
#[derive(Default)]
pub(super) struct JumpRequestFilter {
    last_ts: Option<u64>,
}

impl JumpRequestFilter {
    pub(super) fn accept(&mut self, bytes: &[u8], now_ms: u64) -> Result<JumpRequest, String> {
        let request = parse(bytes, now_ms)?;
        if self.last_ts == Some(request.ts) {
            return Err("jump request was already handled".into());
        }
        self.last_ts = Some(request.ts);
        Ok(request)
    }
}

fn modified(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn read_bounded(path: &std::path::Path) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_REQUEST_BYTES)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(super) fn watch(
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
) {
    // One metadata read per poll per client, independent of rendering and pane count. The file
    // is only opened when its modification time changes.
    std::thread::spawn(move || {
        let path = request_path();
        let mut filter = JumpRequestFilter::default();
        // A request written before this client started was not addressed to it.
        let mut previous = modified(&path);
        while !should_quit.load(Ordering::Acquire) {
            std::thread::sleep(POLL_INTERVAL);
            let current = modified(&path);
            if current == previous {
                continue;
            }
            previous = current;
            if current.is_none() {
                continue;
            }
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or_default();
            let request = match read_bounded(&path)
                .map_err(|error| format!("failed to read jump request: {error}"))
                .and_then(|bytes| filter.accept(&bytes, now_ms))
            {
                Ok(request) => request,
                Err(error) => {
                    debug!(%error, "ignored client jump request");
                    continue;
                }
            };
            if event_tx
                .blocking_send(ClientLoopEvent::JumpRequest {
                    endpoint_id: request.endpoint_id,
                    target: request.target,
                })
                .is_err()
            {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use endpoint::ClientEndpointId;
    use shell::ClientEndpointFocusTarget;

    const NOW: u64 = 1_794_291_489_675;
    const PROFILE: &str = "0123456789abcdef0123456789abcdef";

    fn request(endpoint: &str, target: &str, ts: u64) -> Vec<u8> {
        format!(r#"{{"version":1,"endpoint":"{endpoint}",{target},"ts":{ts}}}"#).into_bytes()
    }

    #[test]
    fn parses_remote_pane_and_local_workspace_requests() {
        let remote = parse(&request(PROFILE, r#""pane_id":"w30:p1""#, NOW), NOW).unwrap();
        assert_eq!(
            remote.endpoint_id,
            ClientEndpointId::Ssh(endpoint::ProfileId::parse(PROFILE).unwrap())
        );
        assert_eq!(
            remote.target,
            ClientEndpointFocusTarget::Pane("w30:p1".into())
        );

        let local = parse(&request("local", r#""workspace_id":"w4""#, NOW), NOW).unwrap();
        assert_eq!(local.endpoint_id, ClientEndpointId::Local);
        assert_eq!(
            local.target,
            ClientEndpointFocusTarget::Workspace("w4".into())
        );
    }

    #[test]
    fn pane_takes_precedence_over_workspace() {
        let both = r#""pane_id":"w1:p2","workspace_id":"w1""#;
        assert_eq!(
            parse(&request("local", both, NOW), NOW).unwrap().target,
            ClientEndpointFocusTarget::Pane("w1:p2".into())
        );
    }

    #[test]
    fn rejects_stale_and_future_requests() {
        let pane = r#""pane_id":"w1:p1""#;
        assert!(parse(&request("local", pane, NOW - MAX_AGE_MS), NOW).is_ok());
        assert!(parse(&request("local", pane, NOW - MAX_AGE_MS - 1), NOW).is_err());
        assert!(parse(&request("local", pane, NOW + MAX_FUTURE_MS), NOW).is_ok());
        assert!(parse(&request("local", pane, NOW + MAX_FUTURE_MS + 1), NOW).is_err());
    }

    #[test]
    fn rejects_malformed_requests() {
        let pane = r#""pane_id":"w1:p1""#;
        for bytes in [
            b"".to_vec(),
            b"{".to_vec(),
            br#"{"version":1,"endpoint":"local"}"#.to_vec(),
            br#"{"version":2,"endpoint":"local","pane_id":"w1:p1","ts":1794291489675}"#.to_vec(),
            request("leon", pane, NOW),
            request("local", r#""pane_id":"""#, NOW),
            request("local", r#""other":1"#, NOW),
        ] {
            assert!(
                parse(&bytes, NOW).is_err(),
                "{:?}",
                String::from_utf8(bytes)
            );
        }
    }

    #[test]
    fn handles_one_request_once() {
        let mut filter = JumpRequestFilter::default();
        let first = request("local", r#""pane_id":"w1:p1""#, NOW);
        assert!(filter.accept(&first, NOW).is_ok());
        assert!(filter.accept(&first, NOW + 100).is_err());
        let second = request("local", r#""pane_id":"w1:p1""#, NOW + 200);
        assert!(filter.accept(&second, NOW + 200).is_ok());
        // A rejected request does not consume the slot of the one that follows it.
        assert!(filter.accept(b"{", NOW + 300).is_err());
        assert!(filter.accept(&second, NOW + 300).is_err());
    }
}
