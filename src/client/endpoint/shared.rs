use crate::hc::machines::{Machine, MachineGraph};
pub(crate) use crate::hc::shared::SharedCoreKey;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SharedEndpoint {
    pub key: SharedCoreKey,
    pub machine: Machine,
}

impl SharedEndpoint {
    pub(crate) fn from_graph(graph: MachineGraph) -> Vec<Self> {
        graph
            .machines
            .into_iter()
            .map(|machine| Self {
                key: SharedCoreKey {
                    registry_core_id: graph.core.id.clone(),
                    core_id: machine.id.clone(),
                },
                machine,
            })
            .collect()
    }

    pub(crate) fn same_route(&self, other: &Self) -> bool {
        self.key == other.key
            && self.machine.target == other.machine.target
            && self.machine.session == other.machine.session
            && self.machine.via == other.machine.via
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(registry: &str, owner: &str, name: &str, target: &str) -> SharedEndpoint {
        SharedEndpoint::from_graph(MachineGraph::from_reply(crate::hc::CoreInfo {id:registry.into(),name:"local".into()}, serde_json::json!({"machines":[{
            "id":owner,"name":name,"target":target,"host":"host","version":"0.1.17","state":"connected","session":null
        }]})).unwrap()).remove(0)
    }
    #[test]
    fn shared_endpoint_rename_is_display_only_but_owner_registry_and_route_are_not() {
        let original = source("registry", "owner", "old", "target");
        let renamed = source("registry", "owner", "new", "target");
        assert_eq!(original.key, renamed.key);
        assert!(original.same_route(&renamed));
        for changed in [
            source("replacement", "owner", "old", "target"),
            source("registry", "other", "old", "target"),
            source("registry", "owner", "old", "elsewhere"),
        ] {
            assert!(!original.same_route(&changed));
        }
        let mut offline = original.clone();
        offline.machine.state = "unreachable".into();
        assert!(original.same_route(&offline));
    }
}
