//! Resolve shared machines by owner ID, independently of their display aliases.
use super::{machines::Machine, Client};
use std::{io, sync::Arc};
use tokio::sync::Mutex;

pub(crate) const CORE_ENV: &str = "HERDR_HC_SHARED_CORE";
pub(crate) const REGISTRY_ENV: &str = "HERDR_HC_SHARED_REGISTRY_CORE";

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub(crate) struct SharedCoreKey {
    pub registry_core_id: String,
    pub core_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SharedLocator {
    pub core_id: String,
    pub registry_core_id: String,
    pub target: String,
    pub via: bool,
    pub session: Option<String>,
}

pub(crate) struct SharedSource {
    pub core_id: String,
    registry_core_id: String,
    registry: Mutex<Option<Client>>,
}

impl SharedSource {
    pub(crate) fn new(core_id: String, registry_core_id: String) -> Arc<Self> {
        Arc::new(Self {
            core_id,
            registry_core_id,
            registry: Mutex::new(None),
        })
    }

    pub(crate) async fn locator(&self) -> io::Result<SharedLocator> {
        match tokio::time::timeout(std::time::Duration::from_secs(3), self.resolve()).await {
            Ok(result) => result,
            Err(_) => {
                if let Some(client) = self.registry.lock().await.take() {
                    client.disconnect();
                }
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "shared hc registry lookup timed out; binding was retained",
                ))
            }
        }
    }

    async fn resolve(&self) -> io::Result<SharedLocator> {
        let mut registry = self.registry.lock().await;
        if registry
            .as_ref()
            .is_some_and(|client| client.outgoing.is_closed())
        {
            registry.take();
        }
        if registry.is_none() {
            *registry = Some(Client::connect(registry_command()).await?);
        }
        let client = registry
            .as_ref()
            .ok_or_else(|| io::Error::other("shared hc registry is unavailable"))?;
        if client.hello.core.id != self.registry_core_id {
            client.disconnect();
            registry.take();
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "shared hc registry core changed; source was not retargeted",
            ));
        }
        let graph = client.machine_graph().await?;
        let machine = graph
            .machines
            .into_iter()
            .find(|machine| machine.id == self.core_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "shared hc machine was removed; binding was retained",
                )
            })?;
        if machine.state != "connected" {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!(
                    "shared hc machine is {}; programs were not started",
                    machine.state
                ),
            ));
        }
        Ok(self.route(machine))
    }

    fn route(&self, machine: Machine) -> SharedLocator {
        SharedLocator {
            core_id: machine.id,
            registry_core_id: self.registry_core_id.clone(),
            target: machine.target,
            via: machine.via,
            session: machine.session,
        }
    }
}

pub(crate) fn registry_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new("hc");
    command.arg("connect");
    for key in [
        "HC_SOCKET",
        "HC_SESSION",
        "HC_TERMINAL_ID",
        CORE_ENV,
        REGISTRY_ENV,
        super::source::PROFILE_ENV,
    ] {
        command.env_remove(key);
    }
    command
}

pub(crate) fn registry_graph() -> io::Result<super::machines::MachineGraph> {
    // A fresh CLI name lookup must not resolve through a stale presentation cache.
    std::thread::spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let client = Client::connect(registry_command()).await?;
                let graph =
                    tokio::time::timeout(std::time::Duration::from_secs(3), client.machine_graph())
                        .await;
                client.disconnect();
                graph.map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "shared hc registry lookup timed out",
                    )
                })?
            })
    })
    .join()
    .map_err(|_| io::Error::other("shared hc registry lookup worker failed"))?
}

impl Drop for SharedSource {
    fn drop(&mut self) {
        if let Some(client) = self.registry.get_mut().take() {
            client.disconnect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn default_registry_command_removes_inherited_hc_source_selectors() {
        let command = registry_command();
        assert_eq!(command.as_std().get_program(), "hc");
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            vec!["connect"]
        );
        for key in [
            "HC_SOCKET",
            "HC_SESSION",
            "HC_TERMINAL_ID",
            CORE_ENV,
            REGISTRY_ENV,
            super::super::source::PROFILE_ENV,
        ] {
            assert!(
                command
                    .as_std()
                    .get_envs()
                    .any(|(name, value)| name == key && value.is_none()),
                "{key}"
            );
        }
    }

    #[tokio::test]
    async fn shared_hc_source_resolves_owner_not_alias_and_ignores_display_only_rename() {
        let (mut client, mut server) = crate::hc::tests::test_client();
        Arc::get_mut(&mut client.hello)
            .unwrap()
            .methods
            .insert("machine.list".into());
        let source = Arc::new(SharedSource {
            core_id: "wanted".into(),
            registry_core_id: "core-test".into(),
            registry: Mutex::new(Some(client)),
        });
        let selector = crate::hc::source::Source::Shared(source.clone());
        assert!(selector.is_remote());
        assert_eq!(selector.expected_core(), Some("wanted"));
        let mut previous = None;
        for name in ["gx10", "renamed"] {
            let source = source.clone();
            let fetch = tokio::spawn(async move { source.locator().await });
            let frame = crate::hc::tests::read_frame(&mut server).await;
            let request: Value = serde_json::from_slice(&frame.payload).unwrap();
            assert_eq!(request["method"], "machine.list");
            crate::hc::runtime_tests::reply(&mut server, request["id"].clone(), json!({"machines":[
                {"id":"wanted","name":name,"target":"gx10","session":null,"host":"gx","version":"0.1.17","state":"connected"},
                {"id":"other","name":"other","target":"leon","session":null,"host":"leon","version":"0.1.17","state":"connected"}
            ]})).await;
            let route = fetch.await.unwrap().unwrap();
            assert_eq!(route.core_id, "wanted");
            assert_eq!(route.session, None);
            if let Some(previous) = previous.as_ref() {
                assert_eq!(previous, &route);
            }
            previous = Some(route);
        }
    }

    #[tokio::test]
    async fn shared_hc_source_offline_and_alias_replacement_do_not_become_connection_routes() {
        for (state, owner, expected) in [
            ("unreachable", "wanted", io::ErrorKind::NotConnected),
            ("connected", "replacement", io::ErrorKind::PermissionDenied),
        ] {
            let (mut client, mut server) = crate::hc::tests::test_client();
            Arc::get_mut(&mut client.hello)
                .unwrap()
                .methods
                .insert("machine.list".into());
            let source = SharedSource::new("wanted".into(), "core-test".into());
            *source.registry.lock().await = Some(client);
            let fetch = tokio::spawn(async move { source.locator().await });
            let request: Value =
                serde_json::from_slice(&crate::hc::tests::read_frame(&mut server).await.payload)
                    .unwrap();
            assert_eq!(request["method"], "machine.list");
            crate::hc::runtime_tests::reply(&mut server, request["id"].clone(), json!({"machines":[
                {"id":owner,"name":"wanted","target":"source","session":null,"host":"host","version":"0.1.17","state":state}
            ]})).await;
            assert_eq!(fetch.await.unwrap().unwrap_err().kind(), expected);
        }
    }

    #[tokio::test]
    async fn shared_hc_source_rejects_replacement_registry_before_any_rpc() {
        let (client, mut server) = crate::hc::tests::test_client();
        let source = SharedSource::new("wanted".into(), "different-registry".into());
        *source.registry.lock().await = Some(client);
        assert_eq!(
            source.locator().await.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(source.registry.lock().await.is_none());
        use tokio::io::AsyncReadExt;
        let mut byte = [0];
        assert_eq!(server.read(&mut byte).await.unwrap(), 0);
    }
}
