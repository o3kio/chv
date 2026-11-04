use dashmap::DashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct TopologyState {
    pub network_id: String,
    pub tenant_id: String,
    pub bridge_name: String,
    pub namespace_name: String,
    pub subnet_cidr: String,
    pub gateway_ip: String,
    pub runtime_status: String,
    pub vni: Option<u32>,
    /// Tenant MTU applied by the fabric path (DHCP option 26, bridge/TAP
    /// MTU). `None` for bridge-only topologies.
    pub tenant_mtu: Option<u32>,
    /// Last applied fabric plan generation for stale-plan fencing
    /// (ADR-021 §4). `None` for bridge-only topologies.
    pub fabric_plan_generation: Option<u64>,
    /// VNI binding generation of the last applied fabric plan (from
    /// `vni_allocations.binding_generation`, ADR-021 §8). A VNI re-bind
    /// bumps this while `desired_generation` (and therefore
    /// `fabric_plan_generation`) stays put, so it fences stale bindings
    /// that the plan generation alone cannot detect. `None` for
    /// bridge-only topologies.
    pub binding_generation: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct TopologyTable {
    inner: Arc<DashMap<String, TopologyState>>,
}

impl TopologyTable {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    pub fn upsert(&self, state: TopologyState) {
        self.inner.insert(state.network_id.clone(), state);
    }

    pub fn remove(&self, network_id: &str) -> Option<TopologyState> {
        self.inner.remove(network_id).map(|(_, v)| v)
    }

    pub fn get(&self, network_id: &str) -> Option<TopologyState> {
        self.inner.get(network_id).map(|r| r.clone())
    }

    pub fn list(&self) -> Vec<TopologyState> {
        self.inner.iter().map(|r| r.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_state(network_id: &str) -> TopologyState {
        TopologyState {
            network_id: network_id.to_string(),
            tenant_id: "t1".to_string(),
            bridge_name: chv_common::bridge_name_for_network(network_id),
            namespace_name: format!("ns-{}", network_id),
            subnet_cidr: "10.0.0.0/24".to_string(),
            gateway_ip: "10.0.0.1".to_string(),
            runtime_status: "ensured".to_string(),
            vni: None,
            tenant_mtu: None,
            fabric_plan_generation: None,
            binding_generation: None,
        }
    }

    #[test]
    fn topology_upsert_and_get() {
        let table = TopologyTable::new();
        let s = dummy_state("net-1");
        table.upsert(s.clone());
        let got = table.get("net-1").unwrap();
        assert_eq!(got.network_id, "net-1");
    }

    #[test]
    fn topology_remove_missing_is_none() {
        let table = TopologyTable::new();
        assert!(table.remove("net-1").is_none());
    }

    #[test]
    fn topology_list_returns_all() {
        let table = TopologyTable::new();
        table.upsert(dummy_state("net-1"));
        table.upsert(dummy_state("net-2"));
        assert_eq!(table.list().len(), 2);
    }

    #[test]
    fn topology_idempotency_overwrite() {
        let table = TopologyTable::new();
        table.upsert(dummy_state("net-1"));
        table.upsert(TopologyState {
            runtime_status: "updating".to_string(),
            ..dummy_state("net-1")
        });
        assert_eq!(table.get("net-1").unwrap().runtime_status, "updating");
    }
}
