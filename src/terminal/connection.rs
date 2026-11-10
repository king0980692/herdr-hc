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
    sync::{mpsc, Mutex},
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

pub(crate) struct CoreHost {
    pub(super) source: watch::Receiver<SourceState>,
    pub(crate) executor: tokio::runtime::Handle,
    pub(crate) saved: bool,
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
                stop: None,
                watcher: Mutex::new(None),
            },
            send,
        )
    }
    pub(crate) fn connect(source: Source) -> io::Result<Self> {
        let saved = matches!(&source, Source::Saved(_));
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
                let selected = source.locator().ok().flatten();
                let publish = |client, error, retired| {
                    let generation = changed.borrow().generation.saturating_add(1);
                    changed.send_replace(SourceState {
                        host_instance: host_instance.clone(), generation, client,
                        locator: selected.clone(), error, retired,
                    });
                };
                let route_changed = || saved_route_changed(saved, &selected, &source.locator().ok().flatten());
                let mut first = Some(ready);
                let mut attempt = 0u32;
                let mut known_core: Option<String> = None;
                loop {
                    let (result,ssh_policy) = if route_changed() {
                        (Err(io::Error::new(io::ErrorKind::NotConnected, "hc source profile changed or was disabled")),None)
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
                    let changed_route = route_changed();
                    let changed_core = saved && client.as_ref().is_some_and(|c| known_core.as_ref().is_some_and(|id| id != &c.hello.core.id));
                    let retired = changed_route || changed_core;
                    if retired {
                        if let Some(client) = client.take() { client.disconnect(); }
                    } else if let Some(client) = client.as_ref() {
                        if known_core.is_none() { known_core = Some(client.hello.core.id.clone()); }
                    }
                    let error = if changed_route {
                        Some("hc source profile changed; local source host must restart".into())
                    } else if changed_core {
                        Some("hc core identity changed; local source host must restart".into())
                    } else { result.err().map(|error| error.to_string()) };
                    publish(client.clone(), error, retired);
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
                                    if route_changed() {
                                        client.disconnect();
                                        publish(None, Some("hc source profile changed".into()), true);
                                        let _ = (&mut stopped).await;
                                        return;
                                    }
                                }
                            }
                        }
                        client.disconnect();
                        if route_changed() {
                            publish(None, Some("hc source profile changed".into()), true);
                            let _ = (&mut stopped).await;
                            return;
                        }
                        if connected_at.elapsed() >= Duration::from_secs(60) { attempt = 0; }
                        publish(None, Some("hc disconnected; programs were not killed".into()), false);
                    } else if let Some(policy)=ssh_policy{
                        // Publish startup readiness first; mux repair must not block the App thread.
                        let changed_route={
                            let repair=crate::hc::source::repair_ssh(policy);
                            tokio::pin!(repair);
                            loop{tokio::select!{
                                _=&mut stopped=>return,
                                result=&mut repair=>{if let Err(error)=result{tracing::debug!(%error,"hc SSH transport repair failed");}break false;},
                                _=tokio::time::sleep(Duration::from_secs(1))=>{if route_changed(){break true;}}
                            }}
                        };
                        if changed_route{publish(None,Some("hc source profile changed".into()),true);let _=(&mut stopped).await;return;}
                    }
                    let deadline = tokio::time::Instant::now() + retry_delay(attempt);
                    attempt = attempt.saturating_add(1);
                    loop {
                        tokio::select! {
                            _ = &mut stopped => return,
                            _ = tokio::time::sleep_until(deadline) => break,
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                                if route_changed() {
                                    publish(None, Some("hc source profile changed".into()), true);
                                    let _ = (&mut stopped).await;
                                    return;
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
        let pinned = Some(SourceLocator {
            profile_id: "profile".into(),
            target: "offline-a".into(),
            session: "agents".into(),
            hc_path: None,
        });
        let mut current = pinned.clone();
        assert!(!saved_route_changed(true, &pinned, &current));
        current.as_mut().unwrap().target = "reachable-b".into();
        assert!(saved_route_changed(true, &pinned, &current));
        current.as_mut().unwrap().target = "same-core-alias".into();
        assert!(saved_route_changed(true, &pinned, &current));
        current = pinned.clone();
        current.as_mut().unwrap().session = "other".into();
        assert!(saved_route_changed(true, &pinned, &current));
        current = pinned.clone();
        current.as_mut().unwrap().hc_path = Some("/opt/hc-other".into());
        assert!(saved_route_changed(true, &pinned, &current));
        assert!(saved_route_changed(true, &pinned, &None));
        assert!(saved_route_changed(true, &None, &current));
        assert!(!saved_route_changed(false, &None, &None));
    }
}
