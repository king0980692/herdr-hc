//! Saved SSH sources use a local presentation server; only hc control crosses SSH.
use crate::api::{
    client::{ApiClient, ConnectionTarget},
    schema::{EmptyParams, Method, Request},
};
use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub(crate) struct SavedSshBridge;
pub(crate) struct SavedSshStream {
    pub stream: crate::ipc::LocalStream,
    pub bridge: SavedSshBridge,
}

fn source_session(profile_id: &str) -> io::Result<String> {
    let id = crate::client::endpoint::ProfileId::parse(profile_id)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    Ok(format!("hc-{id}"))
}
fn presenter_command(exe: &Path, profile_id: &str) -> io::Result<std::process::Command> {
    let name = source_session(profile_id)?;
    let mut command = std::process::Command::new(exe);
    command
        .args(["--session", &name, "server"])
        .env(crate::hc::source::PROFILE_ENV, profile_id)
        .env_remove(crate::hc::shared::CORE_ENV)
        .env_remove(crate::hc::shared::REGISTRY_ENV)
        .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env_remove(crate::server::autodetect::STARTUP_CWD_ENV_VAR)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::platform::detach_server_daemon_command(&mut command);
    Ok(command)
}
fn ensure_source(
    profile_id: &str,
    target: &str,
    session: &str,
    hc_path: Option<&str>,
    cancelled: &AtomicBool,
) -> io::Result<(PathBuf, PathBuf, String)> {
    let name = source_session(profile_id)?;
    let api_path = crate::session::api_socket_path_for(Some(&name));
    let client_path = crate::session::client_socket_path_for(Some(&name));
    crate::remote::validate_remote_target(target)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    crate::session::validate_name(session)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let profiles =
        crate::client::endpoint::EndpointCatalog::load_profiles().map_err(io::Error::other)?;
    if !profiles.iter().any(|p| {
        p.id.as_str() == profile_id
            && p.enabled
            && p.target == target
            && p.session == session
            && p.hc_path.as_deref() == hc_path
    }) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "saved hc source changed or was disabled",
        ));
    }
    let client = ApiClient::for_target(ConnectionTarget::SocketPath(api_path.clone()));
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut spawned = false;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(super::process::cancelled());
        }
        let request = Request {
            id: "hc-source-preflight".into(),
            method: Method::RuntimeSource(EmptyParams {}),
        };
        match client.request_value_cancellable(&request, Duration::from_millis(500), cancelled) {
            Ok(value) => {
                if value["result"]["profile_id"].as_str() == Some(profile_id)
                    && value["result"]["target"].as_str() == Some(target)
                    && value["result"]["session"].as_str() == Some(session)
                    && value["result"]["hc_path"].as_str() == hc_path
                    && value["result"]["retired"].as_bool() == Some(false)
                {
                    let instance = value["result"]["host_instance"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::Unsupported,
                                "source host does not support fenced requests",
                            )
                        })?
                        .to_owned();
                    return Ok((api_path, client_path, instance));
                }
                if value.get("error").is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "existing local session is not an hc presentation source; it was not replaced",
                    ));
                }
            }
            Err(error) => {
                let absent = matches!(&error,crate::api::client::ApiClientError::Io(e) if matches!(e.kind(),io::ErrorKind::NotFound|io::ErrorKind::ConnectionRefused));
                if absent && !spawned {
                    let mut command = presenter_command(&std::env::current_exe()?, profile_id)?;
                    crate::platform::launch_server_daemon_command(&mut command)?;
                    spawned = true;
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "local hc presentation source was not ready; existing servers were not stopped",
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
pub(crate) fn connect_saved_ssh(
    profile_id: &str,
    target: &str,
    session: &str,
    hc_path: Option<&str>,
    cancelled: Arc<AtomicBool>,
) -> io::Result<SavedSshStream> {
    let (_, path, _) = ensure_source(profile_id, target, session, hc_path, &cancelled)?;
    let stream = crate::ipc::connect_local_stream_cancellable(
        &path,
        &cancelled,
        Instant::now() + Duration::from_secs(5),
    )?;
    Ok(SavedSshStream {
        stream,
        bridge: SavedSshBridge,
    })
}
pub(crate) fn saved_source_api_client(
    profile_id: &str,
    target: &str,
    session: &str,
    hc_path: Option<&str>,
) -> io::Result<ApiClient> {
    let (path, _, instance) = ensure_source(
        profile_id,
        target,
        session,
        hc_path,
        &AtomicBool::new(false),
    )?;
    Ok(ApiClient::for_target(ConnectionTarget::SocketPath(path)).with_source_instance(instance))
}
pub(crate) fn check_saved_ssh(
    target: &str,
    session: &str,
    hc_path: Option<&str>,
) -> io::Result<()> {
    let (command, ssh_policy) = crate::hc::source::ssh_command(target, session, hc_path)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let client = match crate::hc::Client::connect(command).await {
            Ok(client) => client,
            Err(error) => {
                if !crate::hc::source::repair_ssh(ssh_policy)
                    .await
                    .unwrap_or(false)
                {
                    return Err(error);
                }
                let (command, _ssh_policy) =
                    crate::hc::source::ssh_command(target, session, hc_path)?;
                crate::hc::Client::connect(command).await?
            }
        };
        client.disconnect();
        Ok(())
    })
}
pub(crate) struct SavedSshSetup {
    target: String,
    hc_path: Option<String>,
}
impl SavedSshSetup {
    pub(crate) fn connect(target: &str, hc_path: Option<&str>) -> io::Result<Self> {
        crate::remote::validate_remote_target(target)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        if let Some(path) = hc_path {
            crate::hc::source::validate_hc_path(path)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        }
        Ok(Self {
            target: target.into(),
            hc_path: hc_path.map(str::to_owned),
        })
    }
    pub(crate) fn running_sessions(&self) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }
    pub(crate) fn prepare(
        self,
        session: &str,
    ) -> io::Result<Option<crate::client::endpoint::SshMachineMetadata>> {
        check_saved_ssh(&self.target, session, self.hc_path.as_deref())?;
        Ok(None)
    }
}
#[cfg(test)]
pub(crate) fn saved_ssh_bootstrap_command(target: &str, session: &str) -> String {
    format!(
        "herdr machine add {} --remote-session {}",
        super::shell_quote(target),
        super::shell_quote(session)
    )
}
pub(crate) fn saved_ssh_failure_needs_attention(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::InvalidInput
            | io::ErrorKind::InvalidData
            | io::ErrorKind::NotFound
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::Unsupported
    ) {
        return true;
    }
    let message = error.to_string().to_ascii_lowercase();
    [
        "permission denied",
        "host key verification failed",
        "remote host identification has changed",
        "no matching host key",
        "protocol",
        "handshake",
        "hc: not found",
    ]
    .iter()
    .any(|s| message.contains(s))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_hc_presenter_is_local_isolated_and_does_not_seed_a_program() {
        let id = "0123456789abcdef0123456789abcdef";
        let command = presenter_command(Path::new("/local/herdr"), id).unwrap();
        assert_eq!(command.get_program(), "/local/herdr");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["--session", &format!("hc-{id}"), "server"]);
        let env: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new(crate::hc::source::PROFILE_ENV)],
            Some(std::ffi::OsStr::new(id))
        );
        assert_eq!(env[std::ffi::OsStr::new("HERDR_SOCKET_PATH")], None);
        assert_eq!(env[std::ffi::OsStr::new("HERDR_STARTUP_CWD")], None);
        assert!(source_session("../default").is_err());
    }
    #[test]
    fn guidance_never_starts_remote_herdr() {
        assert!(!saved_ssh_bootstrap_command("gx10", "default").contains("--remote "));
    }
}
