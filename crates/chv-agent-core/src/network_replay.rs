//! Startup topology replay from the Core authority (rc.8 blocker B9).
//!
//! `chv-nwd` now journals every bridge-only `TopologySpec` it applies, so an
//! nwd-only restart rebuilds its in-memory table. That journal only exists for
//! topologies applied by a journaling nwd. An upgrade from an earlier RC
//! starts with an empty journal while the Core store already holds the live
//! networks: the VM definitions carry each NIC's `cidr`/`gateway`. Without a
//! replay, the first policy update after the upgrade fails
//! `not found: topology <id>` until something happens to re-ensure the
//! network.
//!
//! This module re-ensures every distinct network referenced by a live Core VM
//! that `chv-nwd` does not already own, before the executor starts serving.
//! That both closes the upgrade gap and seeds the nwd journal for every
//! subsequent restart. A network nwd already owns is left untouched: nwd
//! recovers its journal before serving, and a VM definition is a snapshot
//! taken at create time, so re-applying it could roll a later network
//! CIDR/gateway change back. The replay is deliberately best-effort with
//! bounded retries: a down nwd must not prevent the agent from starting and
//! keeping running VMs reachable. A final failure is logged loudly, and the
//! next attach/ensure re-seeds the journal.

use crate::domain::VmDefinition;
use crate::resources::AgentResourceController;
use chv_errors::ChvError;
use chv_hypervisor_api::resources::HostResourceController;
use chv_hypervisor_api::DEFAULT_NIC_CIDR;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use tracing::{info, warn};

/// One bridge-only network derived from the Core VM set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayTopology {
    pub network_id: String,
    pub bridge_name: String,
    pub cidr: String,
    pub gateway: String,
}

/// Bounded retry budget for a topology replay. `chv-nwd.service` is ordered
/// before the agent, but a `Type=simple` daemon may not have bound its UDS on
/// the first connection attempt.
const REPLAY_ATTEMPTS: u32 = 6;
const REPLAY_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Derive the distinct bridge-only topologies referenced by `vms`.
///
/// Duplicate references to one network are merged deterministically: the
/// first non-default CIDR and the first non-empty gateway win. The attach path
/// uses the same fallbacks (`addressing == None` ⇒ `DEFAULT_NIC_CIDR` and no
/// gateway), so replay cannot materialize a different topology than a later
/// attach would.
pub fn topologies_from_vms(vms: &[VmDefinition]) -> Vec<ReplayTopology> {
    let mut topologies: BTreeMap<String, ReplayTopology> = BTreeMap::new();

    for vm in vms {
        for network in &vm.networks {
            let (cidr, gateway) = match &network.addressing {
                Some(addressing) => (
                    if addressing.cidr.is_empty() {
                        DEFAULT_NIC_CIDR.to_string()
                    } else {
                        addressing.cidr.clone()
                    },
                    addressing.gateway.clone(),
                ),
                None => (DEFAULT_NIC_CIDR.to_string(), String::new()),
            };

            let entry = topologies
                .entry(network.network_ref.clone())
                .or_insert_with(|| ReplayTopology {
                    network_id: network.network_ref.clone(),
                    bridge_name: chv_common::bridge_name_for_network(&network.network_ref),
                    cidr: cidr.clone(),
                    gateway: gateway.clone(),
                });

            if entry.cidr == DEFAULT_NIC_CIDR && cidr != DEFAULT_NIC_CIDR {
                entry.cidr = cidr;
            }
            if entry.gateway.is_empty() && !gateway.is_empty() {
                entry.gateway = gateway;
            }
        }
    }

    topologies.into_values().collect()
}

/// Drop the networks nwd already owns, preserving the deterministic order of
/// [`topologies_from_vms`]. A present id that no live VM references is simply
/// ignored: nwd may hold a topology whose last VM is gone until the network
/// delete tears it down.
pub fn missing_topologies(
    desired: Vec<ReplayTopology>,
    already_owned: &[String],
) -> Vec<ReplayTopology> {
    let owned: BTreeSet<&str> = already_owned.iter().map(String::as_str).collect();
    desired
        .into_iter()
        .filter(|topology| !owned.contains(topology.network_id.as_str()))
        .collect()
}

/// Re-ensure every topology derived from the live Core VMs that nwd does not
/// already own, retrying the failed subset a bounded number of times. Returns
/// the last failure after the budget is exhausted so the caller can log it;
/// callers must not turn that into a startup abort (running VMs and the
/// agent's own control plane do not depend on nwd being reachable at this
/// instant).
pub async fn replay_network_topologies(
    resources: &AgentResourceController,
    vms: &[VmDefinition],
) -> Result<(), ChvError> {
    let desired = topologies_from_vms(vms);
    if desired.is_empty() {
        return Ok(());
    }

    // Ask nwd what it already owns. On the rc.7 → rc.8 upgrade the journal is
    // empty, so every live network replays and seeds it. On later restarts
    // nwd has already recovered the journal before serving, so its live
    // topologies are skipped. A failed query falls back to replaying
    // everything: `ensure` is idempotent, and a down nwd fails both paths.
    let mut pending = match resources.network_topology_ids().await {
        Ok(owned) => {
            let missing = missing_topologies(desired, &owned);
            if missing.is_empty() {
                info!(
                    owned = owned.len(),
                    "startup network topology replay: every Core network is already owned by nwd"
                );
                return Ok(());
            }
            missing
        }
        Err(e) => {
            warn!(
                error = %e,
                "startup network topology replay could not query nwd; replaying every Core network"
            );
            desired
        }
    };

    let mut last_error = None;
    for attempt in 1..=REPLAY_ATTEMPTS {
        let mut retry = Vec::new();
        for topology in pending {
            match resources
                .ensure_network_topology(
                    &topology.network_id,
                    &topology.bridge_name,
                    &topology.cidr,
                    &topology.gateway,
                    Some("startup-network-replay"),
                )
                .await
            {
                Ok(()) => info!(
                    network_id = %topology.network_id,
                    bridge = %topology.bridge_name,
                    cidr = %topology.cidr,
                    gateway = %topology.gateway,
                    "startup network topology replay ensured"
                ),
                // The gateway may legitimately be empty (no DHCP); nwd
                // still owns the bridge and default-deny table.
                Err(e) => {
                    warn!(
                        attempt,
                        network_id = %topology.network_id,
                        bridge = %topology.bridge_name,
                        error = %e,
                        "startup network topology replay failed; retrying"
                    );
                    last_error = Some(e);
                    retry.push(topology);
                }
            }
        }

        if retry.is_empty() {
            return Ok(());
        }
        pending = retry;
        if attempt < REPLAY_ATTEMPTS {
            tokio::time::sleep(REPLAY_RETRY_DELAY).await;
        }
    }

    Err(last_error.unwrap_or_else(|| ChvError::Internal {
        reason: "startup network topology replay failed".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        BootSpec, ComputeSpec, NetworkAttachmentRef, NicAddressing, ObservedPowerState,
        RequestedPowerState, ResourceVersion, StorageAttachmentRef, VmDefinition, VmId,
    };

    fn definition(vm_id: &str, networks: Vec<NetworkAttachmentRef>) -> VmDefinition {
        VmDefinition {
            id: VmId::new(vm_id).unwrap(),
            name: vm_id.to_string(),
            boot: BootSpec::new("kernel").unwrap(),
            compute: ComputeSpec::new(1, 536_870_912).unwrap(),
            storage: vec![StorageAttachmentRef {
                attachment_id: "disk-0".to_string(),
                storage_ref: "volume-1".to_string(),
                read_only: false,
                size_bytes: None,
                seed_from: None,
                backend_class: None,
            }],
            networks,
            requested_power_state: RequestedPowerState::Running,
            observed_power_state: ObservedPowerState::Unknown,
            resource_version: ResourceVersion::new(1).unwrap(),
            cloud_init_userdata: None,
            hypervisor_tuning: None,
            vsock: None,
        }
    }

    fn nic(network_id: &str, addressing: Option<NicAddressing>) -> NetworkAttachmentRef {
        NetworkAttachmentRef {
            attachment_id: format!("{network_id}-nic"),
            network_ref: network_id.to_string(),
            mac_address: None,
            addressing,
            firewall_policy_json: None,
        }
    }

    fn addressing(cidr: &str, gateway: &str) -> NicAddressing {
        NicAddressing {
            ip_address: "10.200.0.50".to_string(),
            cidr: cidr.to_string(),
            gateway: gateway.to_string(),
        }
    }

    #[test]
    fn empty_authority_yields_no_topologies() {
        assert!(topologies_from_vms(&[]).is_empty());
    }

    #[test]
    fn derives_bridge_and_carries_addressing_from_the_core_definition() {
        let vms = vec![definition(
            "vm-1",
            vec![nic(
                "eac319350ba9fff7b0b957aedef20ba6",
                Some(addressing("10.200.0.0/24", "10.200.0.1")),
            )],
        )];

        let topologies = topologies_from_vms(&vms);
        assert_eq!(
            topologies,
            vec![ReplayTopology {
                network_id: "eac319350ba9fff7b0b957aedef20ba6".to_string(),
                bridge_name: chv_common::bridge_name_for_network(
                    "eac319350ba9fff7b0b957aedef20ba6"
                ),
                cidr: "10.200.0.0/24".to_string(),
                gateway: "10.200.0.1".to_string(),
            }]
        );
    }

    #[test]
    fn one_entry_per_network_even_when_many_vms_share_it() {
        let vms = vec![
            definition(
                "vm-1",
                vec![nic(
                    "net-1",
                    Some(addressing("10.200.0.0/24", "10.200.0.1")),
                )],
            ),
            definition(
                "vm-2",
                vec![nic(
                    "net-1",
                    Some(addressing("10.200.0.0/24", "10.200.0.1")),
                )],
            ),
            definition(
                "vm-3",
                vec![nic(
                    "net-2",
                    Some(addressing("10.201.0.0/24", "10.201.0.1")),
                )],
            ),
        ];

        let topologies = topologies_from_vms(&vms);
        assert_eq!(topologies.len(), 2);
        assert_eq!(topologies[0].network_id, "net-1");
        assert_eq!(topologies[1].network_id, "net-2");
    }

    #[test]
    fn duplicate_entries_merge_the_most_informative_addressing() {
        let vms = vec![
            definition("vm-1", vec![nic("net-1", None)]),
            definition(
                "vm-2",
                vec![nic(
                    "net-1",
                    Some(addressing("10.200.0.0/24", "10.200.0.1")),
                )],
            ),
        ];

        let topologies = topologies_from_vms(&vms);
        assert_eq!(topologies.len(), 1);
        assert_eq!(topologies[0].cidr, "10.200.0.0/24");
        assert_eq!(topologies[0].gateway, "10.200.0.1");
    }

    #[test]
    fn absent_addressing_uses_the_attach_path_defaults() {
        let vms = vec![definition("vm-1", vec![nic("net-1", None)])];

        let topologies = topologies_from_vms(&vms);
        assert_eq!(topologies.len(), 1);
        assert_eq!(topologies[0].cidr, DEFAULT_NIC_CIDR);
        assert_eq!(topologies[0].gateway, "");
    }

    #[test]
    fn empty_cidr_defaults_like_the_attach_path() {
        let vms = vec![definition(
            "vm-1",
            vec![nic("net-1", Some(addressing("", "10.200.0.1")))],
        )];

        let topologies = topologies_from_vms(&vms);
        assert_eq!(topologies[0].cidr, DEFAULT_NIC_CIDR);
        assert_eq!(topologies[0].gateway, "10.200.0.1");
    }

    #[test]
    fn missing_topologies_skips_networks_nwd_already_owns() {
        let vms = vec![
            definition(
                "vm-1",
                vec![nic(
                    "net-1",
                    Some(addressing("10.200.0.0/24", "10.200.0.1")),
                )],
            ),
            definition(
                "vm-2",
                vec![nic(
                    "net-2",
                    Some(addressing("10.201.0.0/24", "10.201.0.1")),
                )],
            ),
        ];
        let missing = missing_topologies(topologies_from_vms(&vms), &["net-1".to_string()]);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].network_id, "net-2");
    }

    #[test]
    fn missing_topologies_replays_everything_for_an_empty_nwd() {
        let vms = vec![definition("vm-1", vec![nic("net-1", None)])];
        let missing = missing_topologies(topologies_from_vms(&vms), &[]);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].network_id, "net-1");
    }

    #[test]
    fn unrelated_owned_topologies_do_not_suppress_a_replay() {
        let vms = vec![definition("vm-1", vec![nic("net-1", None)])];
        let missing = missing_topologies(
            topologies_from_vms(&vms),
            &["a-network-with-no-live-vm".to_string()],
        );
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].network_id, "net-1");
    }
}
