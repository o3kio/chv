//! Production [`HostResourceController`] implementation for the Core runtime.
//!
//! The single-effector Core runtime (M2.2a) performs every stord/nwd side
//! effect through the neutral `chv_hypervisor_api::HostResourceController`
//! trait; this module is the production adapter that speaks to the real
//! `chv-stord` / `chv-nwd` daemons.
//!
//! It also hosts [`NodeCacheAttachmentSource`], the production
//! [`ObservedAttachmentSource`] for the runtime's delete-time side-effect
//! fallback drain (#405).
//!
//! # Client connect model (Decision 3)
//! Each method connects a **fresh** `StordClient`/`NwdClient` over its socket
//! and immediately closes it when the call returns, exactly matching the
//! existing agent convention (`agent_server.rs` and `reconcile.rs` connect per
//! call every time; `Reconciler` holds socket `PathBuf`s, never persistent
//! clients). There are no persistent clients and no lazy caching.
//!
//! Construction never fails fast: a down stord/nwd is only observable at
//! `execute()` time as a failed connect, which the Core runtime maps to
//! `RuntimeFailure::RuntimeUnavailable`. That keeps the agent alive for
//! start/stop (which do not touch stord/nwd); only create/delete are affected
//! by a down provider.

use crate::cache::NodeCache;
use crate::daemon_clients::{NwdClient, StordClient};
use async_trait::async_trait;
use chv_errors::ChvError;
use chv_hypervisor_api::resources::HostResourceController;
use chv_hypervisor_api::resources::ObservedAttachmentSource;
use chv_hypervisor_api::resources::ObservedVmAttachments;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Production host-resource controller connecting `chv-stord` and `chv-nwd`
/// on demand, per call.
#[derive(Debug, Clone)]
pub struct AgentResourceController {
    stord_socket: PathBuf,
    nwd_socket: PathBuf,
}

impl AgentResourceController {
    pub fn new(stord_socket: PathBuf, nwd_socket: PathBuf) -> Self {
        Self {
            stord_socket,
            nwd_socket,
        }
    }
}

#[async_trait]
impl HostResourceController for AgentResourceController {
    async fn open_volume(
        &self,
        volume_id: &str,
        backend_class: &str,
        locator: &str,
        options: HashMap<String, String>,
        operation_id: Option<&str>,
    ) -> Result<(String, String, String), ChvError> {
        let mut client = StordClient::connect(&self.stord_socket).await?;
        client
            .open_volume_with_options(volume_id, backend_class, locator, options, operation_id)
            .await
    }

    async fn attach_volume_to_vm(
        &self,
        volume_id: &str,
        vm_id: &str,
        attachment_handle: &str,
        operation_id: Option<&str>,
    ) -> Result<(String, String), ChvError> {
        let mut client = StordClient::connect(&self.stord_socket).await?;
        client
            .attach_volume_to_vm(volume_id, vm_id, attachment_handle, operation_id)
            .await
    }

    async fn detach_volume_from_vm(
        &self,
        volume_id: &str,
        vm_id: &str,
        force: bool,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let mut client = StordClient::connect(&self.stord_socket).await?;
        client
            .detach_volume_from_vm(volume_id, vm_id, force, operation_id)
            .await
    }

    async fn close_volume(
        &self,
        volume_id: &str,
        attachment_handle: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let mut client = StordClient::connect(&self.stord_socket).await?;
        client
            .close_volume(volume_id, attachment_handle, operation_id)
            .await
    }

    async fn resolve_volume_handle(
        &self,
        volume_id: &str,
        vm_id: &str,
    ) -> Result<Option<String>, ChvError> {
        let mut client = StordClient::connect(&self.stord_socket).await?;
        client.resolve_volume_handle(volume_id, vm_id).await
    }

    async fn close_detached_volume_sessions(
        &self,
        volume_id: &str,
        operation_id: Option<&str>,
    ) -> Result<usize, ChvError> {
        let mut client = StordClient::connect(&self.stord_socket).await?;
        client
            .close_detached_volume_sessions(volume_id, operation_id)
            .await
    }

    async fn ensure_network_topology(
        &self,
        network_id: &str,
        bridge_name: &str,
        subnet_cidr: &str,
        gateway_ip: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let mut client = NwdClient::connect(&self.nwd_socket).await?;
        client
            .ensure_network_topology(
                network_id,
                bridge_name,
                subnet_cidr,
                gateway_ip,
                operation_id,
            )
            .await
    }

    async fn set_firewall_policy(
        &self,
        network_id: &str,
        policy_version: &str,
        policy_json: &[u8],
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let mut client = NwdClient::connect(&self.nwd_socket).await?;
        client
            .set_firewall_policy(
                network_id,
                policy_version,
                policy_json.to_vec(),
                operation_id,
            )
            .await
    }

    async fn delete_network_topology(
        &self,
        network_id: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let mut client = NwdClient::connect(&self.nwd_socket).await?;
        client
            .delete_network_topology(network_id, operation_id)
            .await
    }

    async fn attach_vm_nic(
        &self,
        nic_id: &str,
        vm_id: &str,
        network_id: &str,
        mac_address: &str,
        ip_address: &str,
        operation_id: Option<&str>,
    ) -> Result<(String, String), ChvError> {
        let mut client = NwdClient::connect(&self.nwd_socket).await?;
        client
            .attach_vm_nic(
                nic_id,
                vm_id,
                network_id,
                mac_address,
                ip_address,
                operation_id,
            )
            .await
    }

    async fn detach_vm_nic(
        &self,
        nic_id: &str,
        vm_id: &str,
        network_id: &str,
        operation_id: Option<&str>,
    ) -> Result<(), ChvError> {
        let mut client = NwdClient::connect(&self.nwd_socket).await?;
        client
            .detach_vm_nic(nic_id, vm_id, network_id, operation_id)
            .await
    }
}

/// Production [`ObservedAttachmentSource`] for the Core runtime's
/// delete-time side-effect fallback drain (#405): the NodeCache
/// compatibility projection, shared with the agent server and the
/// `ProjectingCoreRuntime` wrapper.
///
/// Why the cache is a sound fallback source: in core-managed mode its VM
/// axis is a projection of the durable Core store — the startup rebuild
/// (`NodeCache::rebuild_from_core`) re-seeds `vm_fragments` and
/// `vm_attachments` from the authority's VM list BEFORE the executor
/// starts, so a VM created before an agent restart still has its
/// attachments recorded here even though the runtime's in-memory handle
/// map died with the process.
///
/// Derivation mirrors the agent-server legacy delete cleanup
/// (`reconcile.rs::cleanup_vm_resources`) so the two authorities cannot
/// diverge on the same data: the observed `vm_attachments` axis wins; a
/// missing observation falls back to deriving volume ids and
/// `{vm_id}-{network_id}` nic ids from the projected `vm_fragments`
/// spec; stord attachment handles ride `volume_handles` when recorded
/// (legacy-mode RPCs record them; the Core create path does not). When a
/// handle is absent the fallback drain resolves it from stord's durable
/// session table before detaching (#711), so the detach-then-close pair
/// runs on the post-restart path too instead of leaking an open session.
///
/// Read-only by contract: nothing is removed here. The delete's own
/// projection (`ProjectingCoreRuntime` → `remove_vm_state`) owns cache
/// mutation, so a FAILED delete can retry the fallback drain
/// idempotently against the same observed state.
pub struct NodeCacheAttachmentSource {
    cache: Arc<tokio::sync::Mutex<NodeCache>>,
}

impl NodeCacheAttachmentSource {
    pub fn new(cache: Arc<tokio::sync::Mutex<NodeCache>>) -> Self {
        Self { cache }
    }
}

#[async_trait]
impl ObservedAttachmentSource for NodeCacheAttachmentSource {
    async fn observed_attachments(&self, vm_id: &str) -> ObservedVmAttachments {
        let cache = self.cache.lock().await;
        // Observed attachment axis first; the projected spec fragment is
        // the fallback (same precedence as cleanup_vm_resources).
        let (volume_ids, nics): (Vec<String>, Vec<String>) = match cache.vm_attachment_state(vm_id)
        {
            Some(state) => (
                state.volume_ids.clone(),
                state.nics.iter().map(|nic| nic.nic_id.clone()).collect(),
            ),
            None => cache
                .vm_fragments
                .get(vm_id)
                .and_then(|fragment| std::str::from_utf8(&fragment.spec_json).ok())
                .and_then(|raw| crate::spec::VmSpec::from_json(raw).ok())
                .map(|spec| {
                    let volume_ids = spec
                        .disks
                        .iter()
                        .map(|disk| disk.volume_id.clone())
                        .collect();
                    // The legacy-compatible nic id `{vm_id}-{network_id}`
                    // is the same deterministic function the Core runtime
                    // used at create time (`chv_hypervisor_api::nic_id`).
                    let nics = spec
                        .nics
                        .iter()
                        .map(|nic| format!("{vm_id}-{}", nic.network_id))
                        .collect();
                    (volume_ids, nics)
                })
                .unwrap_or_default(),
        };
        let volumes = volume_ids
            .into_iter()
            .map(|volume_id| {
                let handle = cache.volume_handles.get(&volume_id).cloned();
                (volume_id, handle)
            })
            .collect();
        ObservedVmAttachments { volumes, nics }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{DesiredStateFragment, VmNicAttachment};

    fn fragment(vm_id: &str, spec_json: &str) -> DesiredStateFragment {
        DesiredStateFragment {
            id: vm_id.to_string(),
            kind: "vm".to_string(),
            generation: "1".to_string(),
            spec_json: spec_json.as_bytes().to_vec(),
            policy_json: Vec::new(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
            updated_by: "cp".to_string(),
        }
    }

    fn source_with(cache: NodeCache) -> NodeCacheAttachmentSource {
        NodeCacheAttachmentSource::new(Arc::new(tokio::sync::Mutex::new(cache)))
    }

    #[tokio::test]
    async fn observed_attachment_axis_wins_and_carries_handles() {
        // The exact shape `project_vm` (live projection AND the startup
        // rebuild) records for a Core-created VM: vm_attachments with the
        // Core attachment ids, plus a legacy-mode volume handle.
        let mut cache = NodeCache::new("node-1");
        cache.observe_vm_attachment(
            "vm-1",
            &["vol-0".to_string(), "vol-1".to_string()],
            &[
                VmNicAttachment {
                    nic_id: "vm-1-net-0".to_string(),
                    network_id: "net-0".to_string(),
                },
                VmNicAttachment {
                    nic_id: "vm-1-net-1".to_string(),
                    network_id: "net-1".to_string(),
                },
            ],
        );
        cache
            .volume_handles
            .insert("vol-0".to_string(), "handle-vol-0".to_string());
        let attachments = source_with(cache).observed_attachments("vm-1").await;
        assert_eq!(
            attachments.volumes,
            vec![
                ("vol-0".to_string(), Some("handle-vol-0".to_string())),
                ("vol-1".to_string(), None),
            ]
        );
        assert_eq!(
            attachments.nics,
            vec!["vm-1-net-0".to_string(), "vm-1-net-1".to_string()]
        );
    }

    #[tokio::test]
    async fn missing_observation_derives_from_the_projected_spec_fragment() {
        // vm_attachments can be absent (a restored legacy cache the
        // startup rebuild refused to overwrite, or a hand-written cache);
        // the projected spec fragment still names the disks and networks,
        // and the derived nic id is the same `{vm}-{network}` function the
        // Core runtime used at create time.
        let mut cache = NodeCache::new("node-1");
        cache.store_fragment(
            "vm",
            "vm-2",
            fragment(
                "vm-2",
                r#"{"name":"vm-2","cpus":1,"memory_bytes":1024,"kernel_path":"/dev/null",
                    "disks":[{"volume_id":"vol-a"},{"volume_id":"vol-b"}],
                    "nics":[{"network_id":"net-x","mac_address":"02:00:00:00:00:01","ip_address":"10.0.0.2"}]}"#,
            ),
        );
        let attachments = source_with(cache).observed_attachments("vm-2").await;
        assert_eq!(
            attachments.volumes,
            vec![("vol-a".to_string(), None), ("vol-b".to_string(), None),]
        );
        assert_eq!(attachments.nics, vec!["vm-2-net-x".to_string()]);
    }

    #[tokio::test]
    async fn unknown_vm_observes_nothing() {
        // Neither axis knows the VM: the honest empty answer, which the
        // runtime turns into the logged crash residual (never a wrong
        // drain).
        let attachments = source_with(NodeCache::new("node-1"))
            .observed_attachments("vm-unknown")
            .await;
        assert!(attachments.is_empty());
    }
}
