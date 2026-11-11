//! Shared hc owners get local presentation only; the machine registry remains in hc.
use crate::{
    api::{
        client::{ApiClient, ConnectionTarget},
        schema::{EmptyParams, Method, Request},
    },
    hc::shared::SharedCoreKey,
};
use sha2::{Digest, Sha256};
use std::{
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn shared_session(key: &SharedCoreKey) -> io::Result<String> {
    for id in [&key.registry_core_id, &key.core_id] {
        if id.is_empty()
            || id.contains('/')
            || id.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid shared hc core identity",
            ));
        }
    }
    let digest = Sha256::digest(format!("{}\0{}", key.registry_core_id, key.core_id));
    Ok(format!(
        "hcs-{}",
        digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn matching_shared_source(result: &serde_json::Value, key: &SharedCoreKey) -> bool {
    result["type"].as_str() == Some("runtime_source")
        && result
            .get("profile_id")
            .is_some_and(serde_json::Value::is_null)
        && result["owner_core_id"].as_str() == Some(key.core_id.as_str())
        && result["registry_core_id"].as_str() == Some(key.registry_core_id.as_str())
        && result
            .get("core_id")
            .is_some_and(|core| core.is_null() || core.as_str() == Some(key.core_id.as_str()))
        && result["generation"]
            .as_u64()
            .is_some_and(|generation| generation > 0)
        && result["connected"].as_bool().is_some()
        && (!result["connected"].as_bool().unwrap_or(false)
            || result["core_id"].as_str() == Some(key.core_id.as_str()))
}

fn shared_source_paths(
    key: &SharedCoreKey,
    cancelled: &AtomicBool,
) -> io::Result<(PathBuf, PathBuf, String)> {
    let name = shared_session(key)?;
    let api_path = crate::session::api_socket_path_for(Some(&name));
    let client_path = crate::session::client_socket_path_for(Some(&name));
    let client = ApiClient::for_target(ConnectionTarget::SocketPath(api_path.clone()));
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut spawned = false;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(super::process::cancelled());
        }
        let discovery = Request {
            id: "shared-hc-source".into(),
            method: Method::RuntimeSource(EmptyParams {}),
        };
        match client.request_value_cancellable(&discovery, Duration::from_millis(500), cancelled) {
            Ok(value) => {
                let result = &value["result"];
                let matches = value.get("error").is_none() && matching_shared_source(result, key);
                if matches {
                    let instance = result["host_instance"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::Unsupported,
                                "shared source does not support fenced requests",
                            )
                        })?;
                    if result["retired"].as_bool() == Some(false) {
                        return Ok((api_path, client_path, instance.to_owned()));
                    }
                    if result["retired"].as_bool() == Some(true) {
                        return Err(io::Error::new(io::ErrorKind::NotConnected, "shared source route retired; restart its presentation without stopping hc programs"));
                    }
                }
                if value.get("error").is_some()
                    || result["owner_core_id"]
                        .as_str()
                        .is_some_and(|id| id != key.core_id)
                    || result["registry_core_id"]
                        .as_str()
                        .is_some_and(|id| id != key.registry_core_id)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "existing presentation is not this shared hc source; it was not replaced",
                    ));
                }
            }
            Err(error) => {
                let absent = matches!(&error, crate::api::client::ApiClientError::Io(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused));
                if absent && !spawned {
                    let mut command = std::process::Command::new(std::env::current_exe()?);
                    command
                        .args(["--session", &name, "server"])
                        .env(crate::hc::shared::CORE_ENV, &key.core_id)
                        .env(crate::hc::shared::REGISTRY_ENV, &key.registry_core_id)
                        .env_remove(crate::hc::source::PROFILE_ENV)
                        .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
                        .env_remove("HERDR_CLIENT_SOCKET_PATH")
                        .env_remove(crate::server::autodetect::STARTUP_CWD_ENV_VAR)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null());
                    crate::platform::detach_server_daemon_command(&mut command);
                    crate::platform::launch_server_daemon_command(&mut command)?;
                    spawned = true;
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "shared hc presentation is not ready; existing programs were not changed",
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) fn connect_shared_core(
    key: &SharedCoreKey,
    cancelled: Arc<AtomicBool>,
) -> io::Result<super::SavedSshStream> {
    let (_, path, _) = shared_source_paths(key, &cancelled)?;
    let stream = crate::ipc::connect_local_stream_cancellable(
        &path,
        &cancelled,
        Instant::now() + Duration::from_secs(5),
    )?;
    Ok(super::SavedSshStream {
        stream,
        bridge: super::SavedSshBridge,
    })
}

pub(crate) fn shared_core_api_client(key: &SharedCoreKey) -> io::Result<ApiClient> {
    let (path, _, instance) = shared_source_paths(key, &AtomicBool::new(false))?;
    Ok(ApiClient::for_target(ConnectionTarget::SocketPath(path)).with_source_instance(instance))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_presenter_names_and_preflight_preserve_both_owner_and_registry() {
        let key = SharedCoreKey {
            registry_core_id: "registry".into(),
            core_id: "owner".into(),
        };
        let name = shared_session(&key).unwrap();
        assert_eq!(name, shared_session(&key).unwrap());
        for other in [
            SharedCoreKey {
                registry_core_id: "other".into(),
                ..key.clone()
            },
            SharedCoreKey {
                core_id: "other".into(),
                ..key.clone()
            },
        ] {
            assert_ne!(name, shared_session(&other).unwrap());
        }
        assert!(shared_session(&SharedCoreKey {
            core_id: "bad/path".into(),
            ..key.clone()
        })
        .is_err());
        let valid = serde_json::json!({"type":"runtime_source","profile_id":null,"owner_core_id":"owner","registry_core_id":"registry","core_id":"owner","connected":true,"generation":1});
        assert!(matching_shared_source(&valid, &key));
        for field in [
            "type",
            "profile_id",
            "owner_core_id",
            "registry_core_id",
            "core_id",
            "connected",
            "generation",
        ] {
            let mut partial = valid.clone();
            partial.as_object_mut().unwrap().remove(field);
            assert!(!matching_shared_source(&partial, &key), "{field}");
        }
        for field in ["owner_core_id", "registry_core_id", "core_id"] {
            let mut replaced = valid.clone();
            replaced[field] = serde_json::json!("replacement");
            assert!(!matching_shared_source(&replaced, &key), "{field}");
        }
        let mut offline = valid;
        offline["core_id"] = serde_json::Value::Null;
        offline["connected"] = serde_json::json!(false);
        assert!(matching_shared_source(&offline, &key));
    }
}
