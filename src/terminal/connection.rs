//! One reconnect loop per source. It reconnects control transport, never launches a program.
use crate::{
    events::AppEvent,
    hc::{
        source::{retry_delay, Source, SourceLocator},
        Client,
    },
};
use std::{
    future::Future,
    io,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};
use tokio::sync::{oneshot, watch};

#[derive(Clone)]
pub(crate) struct SourceState {
    pub host_instance: String,
    pub generation: u64,
    pub client: Option<Client>,
    pub locator: Option<SourceLocator>,
    pub error: Option<String>,
    pub retired: bool,
}

fn saved_route_changed(
    saved: bool,
    pinned: &Option<SourceLocator>,
    current: &Option<SourceLocator>,
) -> bool {
    saved && (pinned.is_none() || pinned != current)
}

async fn check_source_route(source: &Source, pinned: &mut Option<SourceLocator>) -> io::Result<()> {
    let current = source.locator().await?;
    if source.is_remote() {
        if pinned.is_some() && saved_route_changed(true, pinned, &current) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "hc source profile changed; local source host must restart",
            ));
        }
        if current.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "hc source route is unavailable",
            ));
        }
        if pinned.is_none() {
            *pinned = current;
        }
    }
    Ok(())
}

pub(crate) struct CoreHost {
    pub(super) source: watch::Receiver<SourceState>,
    pub(crate) executor: tokio::runtime::Handle,
    pub(crate) saved: bool,
    pub(crate) shared: bool,
    pub(super) machines: Arc<Mutex<Result<crate::hc::machines::MachineGraph, String>>>,
    stop: Option<oneshot::Sender<()>>,
    watcher: Mutex<Option<tokio::task::AbortHandle>>,
}
impl Drop for CoreHost {
    fn drop(&mut self) {
        if let Ok(watcher) = self.watcher.get_mut() {
            if let Some(task) = watcher.take() {
                task.abort();
            }
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
impl CoreHost {
    pub(crate) fn state(&self) -> SourceState {
        self.source.borrow().clone()
    }
    pub(crate) fn client(&self) -> io::Result<Client> {
        let state = self.source.borrow();
        state.client.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                state
                    .error
                    .clone()
                    .unwrap_or_else(|| "hc is reconnecting".into()),
            )
        })
    }
    pub(crate) fn watch(&self, events: tokio::sync::mpsc::Sender<AppEvent>) {
        let mut source = self.source.clone();
        let task = self.executor.spawn(async move {
            loop {
                let generation = source.borrow_and_update().generation;
                if events
                    .send(AppEvent::HcSourceChanged { generation })
                    .await
                    .is_err()
                {
                    break;
                }
                if source.changed().await.is_err() {
                    break;
                }
            }
        });
        if let Ok(mut watcher) = self.watcher.lock() {
            if let Some(old) = watcher.replace(task.abort_handle()) {
                old.abort();
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn test(client: Client) -> Self {
        Self::test_source(Some(client), false).0
    }
    #[cfg(test)]
    pub(crate) fn test_source(
        client: Option<Client>,
        saved: bool,
    ) -> (Self, watch::Sender<SourceState>) {
        let (send, source) = watch::channel(SourceState {
            host_instance: crate::client::endpoint::ProfileId::generate().to_string(),
            generation: 1,
            client,
            locator: None,
            error: None,
            retired: false,
        });
        (
            Self {
                source,
                executor: tokio::runtime::Handle::current(),
                saved,
                shared: false,
                machines: Arc::new(Mutex::new(Err("hc machine inventory is loading".into()))),
                stop: None,
                watcher: Mutex::new(None),
            },
            send,
        )
    }
    pub(crate) fn connect(source: Source) -> io::Result<Self> {
        let saved = source.is_remote();
        let shared = matches!(&source, Source::Shared(_));
        let host_instance = crate::client::endpoint::ProfileId::generate().to_string();
        let (changed, receiver) = watch::channel(SourceState {
            host_instance: host_instance.clone(),
            generation: 0,
            client: None,
            locator: None,
            error: Some("hc is connecting".into()),
            retired: false,
        });
        let (ready, receive) = mpsc::sync_channel(1);
        let (stop, mut stopped) = oneshot::channel();
        let machines = Arc::new(Mutex::new(Err("hc machine inventory is loading".into())));
        let inventory = machines.clone();
        crate::thread_spawn::spawn_named("herdr-hc", move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready.send(Err(e));
                    return;
                }
            };
            runtime.block_on(async move {
                // A retry may rebuild transport, never the host's source identity.
                let mut selected = None;
                let publish = |client, error, retired, locator: &Option<SourceLocator>| {
                    let generation = changed.borrow().generation.saturating_add(1);
                    changed.send_replace(SourceState {
                        host_instance: host_instance.clone(), generation, client,
                        locator: locator.clone(), error, retired,
                    });
                };
                let mut first = Some(ready);
                let mut attempt = 0u32;
                let mut known_core = source.expected_core().map(str::to_owned);
                loop {
                    let route = tokio::select! {
                        _ = &mut stopped => break,
                        result = check_source_route(&source, &mut selected) => result,
                    };
                    let (result,ssh_policy) = if let Err(error) = route {
                        (Err(error), None)
                    } else {
                        match source.connect_command(selected.as_ref()) {
                            Ok((command,policy)) => (tokio::select! {
                                _ = &mut stopped => break,
                                result = Client::connect(command) => result,
                            },policy),
                            Err(error) => (Err(error),None),
                        }
                    };
                    let mut client = result.as_ref().ok().cloned();
                    let route_error = tokio::select! {
                        _ = &mut stopped => break,
                        result = check_source_route(&source, &mut selected) => result.err(),
                    };
                    let changed_core = saved && client.as_ref().is_some_and(|c| known_core.as_ref().is_some_and(|id| id != &c.hello.core.id));
                    // herdr-hc shares programs with other viewers; a core that would let their
                    // terminal replies take the size is refused, not used in degraded form.
                    let unsupported = crate::hc_entry::ENABLED
                        && client.as_ref().is_some_and(|c| !crate::hc_entry::core_supported(&c.hello));
                    let error = if changed_core {
                        Some(io::Error::new(io::ErrorKind::PermissionDenied, "hc core identity changed; local source host must restart"))
                    } else if unsupported {
                        Some(io::Error::new(io::ErrorKind::Unsupported, "herdr-hc needs an hc core with automatic_input (hc 0.1.18 or newer)"))
                    } else { route_error.or_else(|| result.err()) };
                    let retired = error.as_ref().is_some_and(|error| error.kind() == io::ErrorKind::PermissionDenied);
                    if error.is_some() {
                        if let Some(client) = client.take() { client.disconnect(); }
                    } else if let Some(client) = client.as_ref() {
                        if known_core.is_none() { known_core = Some(client.hello.core.id.clone()); }
                    }
                    publish(client.clone(), error.map(|error| error.to_string()), retired, &selected);
                    if let Some(ready) = first.take() {
                        if ready.send(Ok(tokio::runtime::Handle::current())).is_err() { break; }
                    }
                    if retired { let _ = (&mut stopped).await; return; }
                    if let Some(client) = client {
                        let connected_at = tokio::time::Instant::now();
                        loop {
                            tokio::select! {
                                _ = &mut stopped => { client.disconnect(); return; },
                                _ = client.closed() => break,
                                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                                    if let Err(error) = check_source_route(&source, &mut selected).await {
                                        client.disconnect();
                                        let retired = error.kind() == io::ErrorKind::PermissionDenied;
                                        publish(None, Some(error.to_string()), retired, &selected);
                                        if retired { let _ = (&mut stopped).await; return; }
                                        break;
                                    }
                                    if !saved {
                                        let graph = tokio::select! {
                                            _ = &mut stopped => { client.disconnect(); return; },
                                            graph = client.machine_graph() => graph,
                                        };
                                        if let Ok(mut inventory) = inventory.lock() {
                                            *inventory = graph.map_err(|error| error.to_string());
                                        }
                                    }
                                }
                            }
                        }
                        client.disconnect();
                        if connected_at.elapsed() >= Duration::from_secs(60) { attempt = 0; }
                        publish(None, Some("hc disconnected; programs were not killed".into()), false, &selected);
                    } else if let Some(policy)=ssh_policy{
                        // Publish startup readiness first; mux repair must not block the App thread.
                        let changed_route={
                            let repair=crate::hc::source::repair_ssh(policy);
                            tokio::pin!(repair);
                            loop{tokio::select!{
                                _=&mut stopped=>return,
                                result=&mut repair=>{if let Err(error)=result{tracing::debug!(%error,"hc SSH transport repair failed");}break false;},
                                _=tokio::time::sleep(Duration::from_secs(1))=>{if let Err(error) = check_source_route(&source, &mut selected).await {break error.kind() == io::ErrorKind::PermissionDenied;}}
                            }}
                        };
                        if changed_route{publish(None,Some("hc source profile changed".into()),true,&selected);let _=(&mut stopped).await;return;}
                    }
                    let deadline = tokio::time::Instant::now() + retry_delay(attempt);
                    attempt = attempt.saturating_add(1);
                    loop {
                        tokio::select! {
                            _ = &mut stopped => return,
                            _ = tokio::time::sleep_until(deadline) => break,
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                                if let Err(error) = check_source_route(&source, &mut selected).await {
                                    if error.kind() == io::ErrorKind::PermissionDenied {
                                        publish(None, Some(error.to_string()), true, &selected);
                                        let _ = (&mut stopped).await;
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            });
        })?;
        let executor = receive
            .recv_timeout(Duration::from_secs(12))
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("hc executor startup failed: {e}"),
                )
            })??;
        Ok(Self {
            source: receiver,
            executor,
            saved,
            shared,
            machines,
            stop: Some(stop),
            watcher: Mutex::new(None),
        })
    }
    pub(crate) fn block_on<T: Send>(
        &self,
        future: impl Future<Output = io::Result<T>> + Send,
    ) -> io::Result<T> {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("herdr-hc-request".into())
                .spawn_scoped(scope, || self.executor.block_on(future))?
                .join()
                .map_err(|_| io::Error::other("hc request worker panicked"))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hc_saved_route_pin_rejects_offline_retargets_and_same_core_aliases() {
        let pinned = Some(SourceLocator::Saved(crate::hc::source::SavedLocator {
            profile_id: "profile".into(),
            target: "offline-a".into(),
            session: "agents".into(),
            hc_path: None,
        }));
        let Some(SourceLocator::Saved(original)) = pinned.as_ref() else {
            panic!("saved route")
        };
        let changed = |current: &crate::hc::source::SavedLocator| {
            saved_route_changed(true, &pinned, &Some(SourceLocator::Saved(current.clone())))
        };
        let mut current = original.clone();
        assert!(!changed(&current));
        current.target = "reachable-b".into();
        assert!(changed(&current));
        current.target = "same-core-alias".into();
        assert!(changed(&current));
        current = original.clone();
        current.session = "other".into();
        assert!(changed(&current));
        current = original.clone();
        current.hc_path = Some("/opt/hc-other".into());
        assert!(changed(&current));
        assert!(saved_route_changed(true, &pinned, &None));
        assert!(saved_route_changed(
            true,
            &None,
            &Some(SourceLocator::Saved(current))
        ));
        assert!(!saved_route_changed(false, &None, &None));
    }
}
