//! Shared hc inventory. Names and transport hints never become program identity.
use super::{invalid, Client, CoreInfo};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashSet, io};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub target: String,
    #[serde(default)]
    pub via: bool,
    #[serde(default)]
    pub temporary: bool,
    pub session: Option<String>,
    pub host: String,
    pub version: String,
    // Unknown future states remain visible, but are not treated as connected.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MachineGraph {
    pub core: CoreInfo,
    pub machines: Vec<Machine>,
}

fn identity(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

impl MachineGraph {
    pub fn from_reply(core: CoreInfo, reply: Value) -> io::Result<Self> {
        #[derive(Deserialize)]
        struct Reply {
            machines: Vec<Machine>,
        }
        let Reply { machines } = serde_json::from_value(reply).map_err(super::invalid_json)?;
        let mut ids = HashSet::from([core.id.as_str()]);
        let mut names = HashSet::new();
        if !identity(&core.id)
            || machines.iter().any(|machine| {
                !identity(&machine.id)
                    || !identity(&machine.name)
                    || !ids.insert(machine.id.as_str())
                    || !names.insert(machine.name.as_str())
                    || machine.target.trim().is_empty()
                    || machine.target.chars().any(char::is_control)
                    || (!machine.via
                        && (machine.target.starts_with('-')
                            || machine.target.chars().any(char::is_whitespace)))
                    || machine
                        .session
                        .as_ref()
                        .is_some_and(|session| !identity(session))
            })
        {
            return Err(invalid(
                "invalid shared hc machine inventory; previous inventory must be retained",
            ));
        }
        Ok(Self { core, machines })
    }
}

impl Client {
    pub async fn machine_graph(&self) -> io::Result<MachineGraph> {
        MachineGraph::from_reply(
            self.hello.core.clone(),
            self.call("machine.list", json!({})).await?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core() -> CoreInfo {
        CoreInfo {
            id: "local-owner".into(),
            name: "local".into(),
        }
    }
    fn machine(id: &str, name: &str) -> Value {
        json!({"id":id,"name":name,"target":"user@host","session":null,
            "host":"host","version":"0.1.17","state":"connected"})
    }
    fn graph(machines: Value) -> io::Result<MachineGraph> {
        MachineGraph::from_reply(core(), json!({"machines":machines}))
    }

    #[tokio::test]
    async fn shared_hc_graph_reads_only_machine_inventory_not_program_operations() {
        let (mut client, mut server) = crate::hc::tests::test_client();
        std::sync::Arc::get_mut(&mut client.hello)
            .unwrap()
            .methods
            .insert("machine.list".into());
        let fetch = tokio::spawn(async move { client.machine_graph().await });
        let frame = crate::hc::tests::read_frame(&mut server).await;
        let request: Value = serde_json::from_slice(&frame.payload).unwrap();
        assert_eq!(request["method"], "machine.list");
        assert_eq!(request["params"], json!({}));
        crate::hc::runtime_tests::reply(
            &mut server,
            request["id"].clone(),
            json!({"machines":[machine("owner", "remote")]}),
        )
        .await;
        let graph = fetch.await.unwrap().unwrap();
        assert_eq!(graph.core.id, "core-test");
        assert_eq!(graph.machines[0].id, "owner");
    }

    #[tokio::test]
    async fn shared_hc_graph_api_uses_only_live_local_cache_and_never_stale_owner_data() {
        let (client, mut server) = crate::hc::tests::test_client();
        let (factory, source) = crate::terminal::TerminalFactory::test_source(Some(client));
        let mut app = crate::hc::app_tests::app(factory);
        let query = |app: &mut crate::app::App| -> Value {
            serde_json::from_str(
                &app.handle_api_request(
                    serde_json::from_value(
                        json!({"id":"graph", "method":"runtime.machines", "params":{}}),
                    )
                    .unwrap(),
                ),
            )
            .unwrap()
        };
        assert_eq!(query(&mut app)["error"]["code"], "unavailable_runtime");
        let mut graph = graph(json!([machine("remote-owner", "remote")])).unwrap();
        graph.core.id = "core-test".into();
        app.terminal_factory.test_machine_graph(Ok(graph.clone()));
        let response = query(&mut app);
        assert_eq!(response["result"]["type"], "runtime_machines");
        assert_eq!(response["result"]["machines"][0]["id"], "remote-owner");
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            crate::hc::tests::read_frame(&mut server)
        )
        .await
        .is_err());
        graph.core.id = "previous-local-owner".into();
        app.terminal_factory.test_machine_graph(Ok(graph));
        assert_eq!(query(&mut app)["error"]["code"], "unavailable_runtime");
        source.send_modify(|state| state.client = None);
        assert_eq!(query(&mut app)["error"]["code"], "unavailable_runtime");
        app.terminal_factory = crate::terminal::TerminalFactory::test_saved_source();
        assert_eq!(query(&mut app)["error"]["code"], "unavailable_runtime");
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn shared_hc_graph_keeps_owner_identity_across_rename_and_name_reuse() {
        let before = graph(json!([
            machine("gx-owner", "gx10"),
            machine("leon-owner", "leon")
        ]))
        .unwrap();
        let after = graph(json!([
            machine("gx-owner", "renamed"),
            machine("replacement", "gx10")
        ]))
        .unwrap();
        assert_eq!(before.machines[0].id, after.machines[0].id);
        assert_ne!(before.machines[0].id, after.machines[1].id);
        assert_eq!(before.core, after.core);
        assert_eq!(before.machines[0].session, None);
    }

    #[test]
    fn shared_hc_graph_rejects_duplicate_or_malformed_membership_without_partial_success() {
        for members in [
            json!([machine("same", "a"), machine("same", "b")]),
            json!([machine("a", "same"), machine("b", "same")]),
            json!([machine("local-owner", "remote")]),
            json!([machine("alias/t1", "a")]),
            json!([machine(" ", "a")]),
            json!([machine("a", "bad/name")]),
            json!([machine("a","good"), {"id":"partial"}]),
            Value::Null,
        ] {
            assert!(graph(members).is_err());
        }
        assert!(MachineGraph::from_reply(core(), json!({})).is_err());
        assert!(graph(json!([])).unwrap().machines.is_empty());
    }

    #[test]
    fn shared_hc_graph_preserves_offline_and_future_states_and_provider_hints() {
        for state in ["unreachable", "needs_update", "future_state"] {
            let mut record = machine("remote", "remote");
            record["state"] = json!(state);
            record["why"] = json!("retained diagnostic");
            record["target"] = json!("provider --no-pty remote");
            record["via"] = json!(true);
            record["temporary"] = json!(true);
            let graph = graph(json!([record])).unwrap();
            let machine = &graph.machines[0];
            assert_eq!(machine.state, state);
            assert_eq!(machine.why.as_deref(), Some("retained diagnostic"));
            assert!(machine.via && machine.temporary);
        }
    }

    #[test]
    fn shared_hc_graph_validates_transport_hints_before_any_connection() {
        for (field, value) in [
            ("target", ""),
            ("target", "-Fbad"),
            ("target", "host extra"),
            ("target", "host\ncommand"),
            ("session", "../other"),
            ("session", ""),
        ] {
            let mut record = machine("remote", "remote");
            record[field] = json!(value);
            assert!(graph(json!([record])).is_err(), "{field}/{value:?}");
        }
    }
}
