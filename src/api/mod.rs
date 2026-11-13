pub mod client;
mod event_hub;
pub mod schema;
mod server;
mod status;
mod subscriptions;
mod wait;

pub use event_hub::EventHub;
pub use server::ServerHandle;
pub(crate) use server::{api_method_name, start_server_with_stop_control};
pub use status::{read_runtime_status_at, RuntimeStatus};

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::api::schema::Request;

pub const SOCKET_PATH_ENV_VAR: &str = "HERDR_SOCKET_PATH";
/// This app's state directory, for tools beside it: where a client watches
/// `client/jump-request.json` and keeps `client/endpoint-selection.json`.
pub const STATE_DIR_ENV_VAR: &str = "HERDR_STATE_DIR";

/// The `HERDR_STATE_DIR` entry a child process of this app gets.
pub(crate) fn state_dir_env() -> (String, String) {
    (
        STATE_DIR_ENV_VAR.to_string(),
        crate::config::state_dir().display().to_string(),
    )
}

pub(crate) fn request_changes_ui(request: &Request) -> bool {
    request.method.operation().changes_ui
}

pub struct ApiRequestMessage {
    pub request: Request,
    pub respond_to: std::sync::mpsc::Sender<String>,
    pub response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
}

pub type ApiRequestSender = mpsc::UnboundedSender<ApiRequestMessage>;

pub fn socket_path() -> PathBuf {
    crate::session::active_api_socket_path()
}
