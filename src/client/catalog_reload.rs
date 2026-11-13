use super::*;

pub(super) fn watch_profiles(
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
) {
    // One bounded read per second per client, independent of rendering and pane count.
    std::thread::spawn(move || {
        let mut previous = None;
        let mut previous_shared = None;
        let source_api = crate::api::client::ApiClient::local();
        // A server without a machine inventory answers with an error each
        // time; asking less often until it has one keeps its log readable.
        let mut inventory_delay = Duration::from_secs(1);
        let mut inventory_due = std::time::Instant::now();
        while !should_quit.load(Ordering::Acquire) {
            let current = endpoint::EndpointCatalog::load_profiles();
            if previous.as_ref() != Some(&current) {
                previous = Some(current.clone());
                if event_tx
                    .blocking_send(ClientLoopEvent::EndpointCatalog(current))
                    .is_err()
                {
                    break;
                }
            }
            let discovery = crate::api::schema::Request {
                id: "shared-machine-inventory".into(),
                method: crate::api::schema::Method::RuntimeMachines(
                    crate::api::schema::EmptyParams {},
                ),
            };
            let now = std::time::Instant::now();
            let reply = if now >= inventory_due {
                let reply = source_api
                    .request_value_cancellable(&discovery, Duration::from_millis(500), &should_quit)
                    .ok()
                    .filter(|value| value.get("error").is_none());
                inventory_delay = if reply.is_some() {
                    Duration::from_secs(1)
                } else {
                    (inventory_delay * 2).min(Duration::from_secs(30))
                };
                inventory_due = now + inventory_delay;
                reply
            } else {
                None
            };
            if let Some(value) = reply {
                let graph = serde_json::from_value(value["result"]["core"].clone())
                    .map_err(io::Error::other)
                    .and_then(|core| {
                        crate::hc::machines::MachineGraph::from_reply(core, value["result"].clone())
                    });
                if let Ok(graph) = graph {
                    let sources = endpoint::SharedEndpoint::from_graph(graph);
                    if previous_shared.as_ref() != Some(&sources) {
                        previous_shared = Some(sources.clone());
                        if event_tx
                            .blocking_send(ClientLoopEvent::SharedMachines(sources))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

// Only called between surface handoffs: removing a source must not invalidate an in-flight
// rollback. Connection attempts are independent and fenced by supervisor generations.
pub(super) fn apply_profiles(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    commands: &mut endpoint_commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    catalog: &mut endpoint::EndpointCatalog,
    profiles: Vec<endpoint::SavedSshEndpoint>,
    now: std::time::Instant,
) -> bool {
    if catalog.ssh == profiles {
        return false;
    }
    let previous_size = state
        .shell
        .as_ref()
        .map(|shell| shell.surface_size(state.reported_size.0, state.reported_size.1));
    let retired = supervisors.reconcile_profiles(&profiles, now);
    let active_removed = retired.contains(endpoints.active_id());
    for endpoint_id in retired {
        endpoints.disconnect(&endpoint_id);
        let cancelled = commands.disconnect(&endpoint_id);
        #[cfg(unix)]
        state.forget_endpoint_graphics(&endpoint_id);
        if let Some(shell) = state.shell.as_mut() {
            for request_id in cancelled {
                shell.cancel_endpoint_request(&request_id);
            }
            shell.retire_endpoint(&endpoint_id);
        }
    }
    catalog.ssh = profiles;
    if active_removed {
        endpoints.select_unavailable_local();
        catalog.select_local();
        state.freeze_presentation();
        if let Some(shell) = state.shell.as_mut() {
            shell.select_unavailable_local();
        }
    } else if catalog.selected_profile.as_ref().is_some_and(|selected| {
        !catalog
            .ssh
            .iter()
            .any(|profile| &profile.id == selected && profile.enabled)
    }) {
        catalog.select_local();
    }
    let pixel_mouse_available = state.pixel_mouse_available();
    if let Some(shell) = state.shell.as_mut() {
        shell.set_endpoint_catalog(&catalog.ssh);
        if endpoints.active_surface_available()
            && previous_size
                != Some(shell.surface_size(state.reported_size.0, state.reported_size.1))
        {
            shell.invalidate_pane_surface();
            endpoints.send(&client_shell_resize_message(
                shell,
                state.reported_size.0,
                state.reported_size.1,
                state.reported_cell_size.0,
                state.reported_cell_size.1,
                pixel_mouse_available,
            ));
        }
    }
    active_removed
}

pub(super) fn apply_shared(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    commands: &mut endpoint_commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    catalog: &mut endpoint::EndpointCatalog,
    sources: Vec<endpoint::SharedEndpoint>,
    now: std::time::Instant,
) -> bool {
    let selected_removed = catalog
        .selected_shared
        .as_ref()
        .is_some_and(|key| !sources.iter().any(|source| &source.key == key));
    if catalog.shared == sources && !selected_removed {
        return false;
    }
    let previous_size = state
        .shell
        .as_ref()
        .map(|shell| shell.surface_size(state.reported_size.0, state.reported_size.1));
    let retired = supervisors.reconcile_shared(&sources, now);
    let active_removed = retired.contains(endpoints.active_id()) || selected_removed;
    for id in retired {
        endpoints.disconnect(&id);
        let cancelled = commands.disconnect(&id);
        #[cfg(unix)]
        state.forget_endpoint_graphics(&id);
        if let Some(shell) = state.shell.as_mut() {
            for request in cancelled {
                shell.cancel_endpoint_request(&request);
            }
            shell.retire_endpoint(&id);
        }
    }
    catalog.shared = sources;
    if active_removed {
        endpoints.select_unavailable_local();
        catalog.select_local();
        state.freeze_presentation();
        if let Some(shell) = state.shell.as_mut() {
            shell.select_unavailable_local();
        }
    }
    let pixel_mouse_available = state.pixel_mouse_available();
    if let Some(shell) = state.shell.as_mut() {
        shell.set_shared_endpoint_catalog(&catalog.shared);
        if endpoints.active_surface_available()
            && previous_size
                != Some(shell.surface_size(state.reported_size.0, state.reported_size.1))
        {
            shell.invalidate_pane_surface();
            endpoints.send(&client_shell_resize_message(
                shell,
                state.reported_size.0,
                state.reported_size.1,
                state.reported_cell_size.0,
                state.reported_cell_size.1,
                pixel_mouse_available,
            ));
        }
    }
    active_removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use endpoint::{ClientEndpointId, EndpointCatalog, EndpointRegistry, EndpointSupervisors};
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    struct Transport(Arc<AtomicUsize>);

    impl endpoint::EndpointTransport for Transport {
        fn send(&mut self, _: &ClientMessage) -> io::Result<()> {
            Ok(())
        }

        fn disconnect(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn state() -> ClientState {
        ClientState::test_new()
    }

    fn shared_source(owner: &str) -> endpoint::SharedEndpoint {
        endpoint::SharedEndpoint::from_graph(crate::hc::machines::MachineGraph::from_reply(crate::hc::CoreInfo {id:"registry".into(),name:"local".into()}, serde_json::json!({"machines":[{
            "id":owner,"name":owner,"target":owner,"session":null,"host":"host","version":"0.1.17","state":"connected"
        }]})).unwrap()).remove(0)
    }

    #[test]
    fn offline_but_present_shared_owner_keeps_pending_selection() {
        let now = Instant::now();
        let mut state = state();
        let mut endpoints = EndpointRegistry::empty();
        let mut supervisors = EndpointSupervisors::new(&[], now);
        let mut commands = endpoint_commands::EndpointCommands::default();
        let mut catalog = EndpointCatalog::default();
        let mut source = shared_source("owner");
        catalog.selected_shared = Some(source.key.clone());
        source.machine.state = "offline".into();
        for _ in 0..2 {
            assert!(!apply_shared(
                &mut state,
                &mut endpoints,
                &mut commands,
                &mut supervisors,
                &mut catalog,
                vec![source.clone()],
                now
            ));
            assert_eq!(
                catalog.selected_endpoint(),
                ClientEndpointId::Hc(source.key.clone())
            );
        }
    }

    #[test]
    fn shared_catalog_is_ephemeral_and_rename_preserves_connections_but_retarget_retires_only_its_owner(
    ) {
        let now = Instant::now();
        let mut state = state();
        let mut endpoints = EndpointRegistry::empty();
        let mut supervisors = EndpointSupervisors::new(&[], now);
        let mut commands = endpoint_commands::EndpointCommands::default();
        let mut catalog = EndpointCatalog::default();
        let mut a = shared_source("owner-a");
        let b = shared_source("owner-b");
        let id_a = ClientEndpointId::Hc(a.key.clone());
        let id_b = ClientEndpointId::Hc(b.key.clone());
        assert!(!apply_shared(
            &mut state,
            &mut endpoints,
            &mut commands,
            &mut supervisors,
            &mut catalog,
            vec![a.clone(), b.clone()],
            now
        ));
        let a_drops = Arc::new(AtomicUsize::new(0));
        let b_drops = Arc::new(AtomicUsize::new(0));
        endpoints.insert(
            id_a.clone(),
            Transport(a_drops.clone()),
            2,
            Default::default(),
            true,
        );
        endpoints.insert(
            id_b.clone(),
            Transport(b_drops.clone()),
            3,
            Default::default(),
            false,
        );
        endpoints.set_active(&id_a);
        assert!(catalog.select_endpoint(&id_a));
        assert!(catalog.ssh.is_empty());
        let stored = serde_json::to_value(&catalog).unwrap();
        assert!(stored.get("shared").is_none() && stored.get("selected_shared").is_none());
        a.machine.name = "renamed".into();
        assert!(!apply_shared(
            &mut state,
            &mut endpoints,
            &mut commands,
            &mut supervisors,
            &mut catalog,
            vec![a.clone(), b.clone()],
            now
        ));
        assert_eq!(endpoints.active_id(), &id_a);
        assert_eq!(endpoints.connection(&id_a).unwrap().generation, 2);
        assert_eq!(
            state.shell.as_ref().unwrap().endpoint_label(&id_a),
            "renamed"
        );
        assert_eq!(a_drops.load(Ordering::Relaxed), 0);
        a.machine.target = "replacement-route".into();
        assert!(apply_shared(
            &mut state,
            &mut endpoints,
            &mut commands,
            &mut supervisors,
            &mut catalog,
            vec![a, b],
            now
        ));
        assert!(endpoints.connection(&id_a).is_none());
        assert_eq!(endpoints.connection(&id_b).unwrap().generation, 3);
        assert_eq!(a_drops.load(Ordering::Relaxed), 1);
        assert_eq!(b_drops.load(Ordering::Relaxed), 0);
        assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);
        assert!(!endpoints.active_surface_available());
    }

    #[test]
    fn live_catalog_add_and_rename_keep_local_connection_and_selection() {
        let now = Instant::now();
        let mut state = state();
        let disconnected = Arc::new(AtomicUsize::new(0));
        let mut endpoints =
            EndpointRegistry::new(Transport(disconnected.clone()), 1, Default::default());
        let mut supervisors = EndpointSupervisors::new(&[], now);
        let mut commands = endpoint_commands::EndpointCommands::default();
        let mut catalog = EndpointCatalog::default();
        let mut profile = endpoint::SavedSshEndpoint::new("Build", "build", "main").unwrap();
        for label in ["Build", "Renamed"] {
            profile.label = label.into();
            assert!(!apply_profiles(
                &mut state,
                &mut endpoints,
                &mut commands,
                &mut supervisors,
                &mut catalog,
                vec![profile.clone()],
                now
            ));
            assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);
            assert!(endpoints.active_surface_available());
            assert_eq!(
                endpoints
                    .connection(&ClientEndpointId::Local)
                    .unwrap()
                    .generation,
                1
            );
            assert_eq!(catalog.selected_profile, None);
            assert_eq!(
                state
                    .shell
                    .as_ref()
                    .unwrap()
                    .endpoint_label(&ClientEndpointId::Ssh(profile.id.clone())),
                label
            );
        }
        assert_eq!(disconnected.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn live_catalog_remove_or_disable_active_machine_selects_local_without_input() {
        for local_online in [false, true] {
            for disable in [false, true] {
                let now = Instant::now();
                let mut state = state();
                let mut catalog = EndpointCatalog::default();
                let id = catalog.add_ssh("Build", "build", "main", None).unwrap();
                let remote = ClientEndpointId::Ssh(id.clone());
                catalog.select_ssh(&id);
                state
                    .shell
                    .as_mut()
                    .unwrap()
                    .set_endpoint_catalog(&catalog.ssh);
                let mut supervisors = EndpointSupervisors::new(&catalog.ssh, now);
                let mut endpoints = EndpointRegistry::empty();
                let local_disconnects = Arc::new(AtomicUsize::new(0));
                if local_online {
                    endpoints.insert(
                        ClientEndpointId::Local,
                        Transport(local_disconnects.clone()),
                        1,
                        Default::default(),
                        false,
                    );
                }
                let remote_disconnects = Arc::new(AtomicUsize::new(0));
                endpoints.insert(
                    remote.clone(),
                    Transport(remote_disconnects.clone()),
                    2,
                    Default::default(),
                    true,
                );
                endpoints.set_active(&remote);
                endpoints.unfreeze_input();
                let mut commands = endpoint_commands::EndpointCommands::default();
                let profiles = if disable {
                    let mut profiles = catalog.ssh.clone();
                    profiles[0].enabled = false;
                    profiles
                } else {
                    Vec::new()
                };
                assert!(apply_profiles(
                    &mut state,
                    &mut endpoints,
                    &mut commands,
                    &mut supervisors,
                    &mut catalog,
                    profiles,
                    now
                ));
                assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);
                assert!(!endpoints.active_surface_available());
                assert!(endpoints.connection(&remote).is_none());
                assert_eq!(
                    endpoints.connection(&ClientEndpointId::Local).is_some(),
                    local_online
                );
                assert!(state
                    .shell
                    .as_ref()
                    .unwrap()
                    .endpoint_is_active(&ClientEndpointId::Local));
                assert!(!state.shell.as_ref().unwrap().has_presented_surface());
                assert!(state.presentation_frozen);
                assert_eq!(catalog.selected_profile, None);
                assert_eq!(remote_disconnects.load(Ordering::Relaxed), 1);
                assert_eq!(local_disconnects.load(Ordering::Relaxed), 0);
                assert!(!supervisors.record_status(
                    &remote,
                    2,
                    endpoint::ClientEndpointStatus::Online,
                    now
                ));
            }
        }
    }
}
