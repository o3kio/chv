use crate::error::ControlPlaneServiceError;
use async_trait::async_trait;
use chv_controlplane_store::{
    NodeInventoryInput, NodeRepository, NodeVersionInput, VtepRepository,
};
use chv_controlplane_types::domain::NodeId;
use control_plane_node_api::control_plane_node_api as proto;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::enrollment::derive_underlay_endpoint;

#[async_trait]
pub trait InventoryService: Send + Sync {
    /// `peer_addr` is the transport-level remote address of the gRPC call
    /// (from `tonic::Request::remote_addr`). It is used to derive the
    /// node's fabric underlay endpoint when the request does not carry
    /// one explicitly.
    async fn report_node_inventory(
        &self,
        request: proto::ReportNodeInventoryRequest,
        peer_addr: Option<SocketAddr>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn report_service_versions(
        &self,
        request: proto::ReportServiceVersionsRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;
}

#[derive(Clone)]
pub struct InventoryServiceImplementation {
    node_repo: NodeRepository,
    vtep_repo: VtepRepository,
}

impl InventoryServiceImplementation {
    pub fn new(node_repo: NodeRepository, vtep_repo: VtepRepository) -> Self {
        Self {
            node_repo,
            vtep_repo,
        }
    }

    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }
}

use chv_controlplane_types::constants::{
    COMPONENT_AGENT, COMPONENT_CHV, COMPONENT_HOST, COMPONENT_NWD, COMPONENT_STORD,
    SOURCE_PERIODIC, STATUS_OK, SUMMARY_INVENTORY_REPORTED, SUMMARY_VERSIONS_REPORTED,
};

/// Map a reported `AuthorityMode` onto the store's kebab-case spelling
/// (#378). `UNSPECIFIED` — pre-field agents — and any unrecognized value
/// map to `None` so the `node_inventory.authority_mode` column keeps its
/// fail-open NULL semantics for nodes that have not reported a mode.
pub(crate) fn authority_mode_text(mode: i32) -> Option<String> {
    match proto::AuthorityMode::try_from(mode) {
        Ok(proto::AuthorityMode::Legacy) => {
            Some(chv_controlplane_store::AUTHORITY_MODE_LEGACY.into())
        }
        Ok(proto::AuthorityMode::CoreManaged) => {
            Some(chv_controlplane_store::AUTHORITY_MODE_CORE_MANAGED.into())
        }
        Ok(proto::AuthorityMode::CoreNative) => {
            Some(chv_controlplane_store::AUTHORITY_MODE_CORE_NATIVE.into())
        }
        _ => None,
    }
}

#[async_trait]
impl InventoryService for InventoryServiceImplementation {
    async fn report_node_inventory(
        &self,
        request: proto::ReportNodeInventoryRequest,
        peer_addr: Option<SocketAddr>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let inventory = request
            .inventory
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing inventory".into()))?;

        let meta = request
            .meta
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing meta".into()))?;

        let node_id = NodeId::new(inventory.node_id.clone()).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;

        let now = self.now_ms();

        self.node_repo
            .ensure_node_record(
                &node_id,
                Some(inventory.hostname.as_str()),
                Some(inventory.hostname.as_str()),
                now,
            )
            .await?;

        // Convert lists to JSONB
        let storage_classes = if inventory.storage_classes.is_empty() {
            None
        } else {
            Some(
                serde_json::to_value(&inventory.storage_classes).map_err(|e| {
                    ControlPlaneServiceError::Internal(format!(
                        "failed to serialize storage_classes: {}",
                        e
                    ))
                })?,
            )
        };

        let network_capabilities = if inventory.network_capabilities.is_empty() {
            None
        } else {
            Some(
                serde_json::to_value(&inventory.network_capabilities).map_err(|e| {
                    ControlPlaneServiceError::Internal(format!(
                        "failed to serialize network_capabilities: {}",
                        e
                    ))
                })?,
            )
        };

        let hypervisor_capabilities = if inventory.hypervisor_capabilities.is_empty() {
            None
        } else {
            Some(
                serde_json::to_value(&inventory.hypervisor_capabilities).map_err(|e| {
                    ControlPlaneServiceError::Internal(format!(
                        "failed to serialize hypervisor_capabilities: {}",
                        e
                    ))
                })?,
            )
        };

        let labels = if inventory.labels.is_empty() {
            None
        } else {
            Some(serde_json::to_value(&inventory.labels).map_err(|e| {
                ControlPlaneServiceError::Internal(format!("failed to serialize labels: {}", e))
            })?)
        };

        self.node_repo
            .upsert_inventory(&NodeInventoryInput {
                node_id: node_id.clone(),
                architecture: inventory.architecture.clone(),
                kernel_version: None,
                os_release: None,
                cpu_count: inventory.cpu_threads as i32,
                memory_bytes: inventory.memory_bytes as i64,
                disk_bytes: None,
                cloud_hypervisor_version: None,
                chv_agent_version: None,
                chv_stord_version: None,
                chv_nwd_version: None,
                host_bundle_version: None,
                inventory_status: Some(SOURCE_PERIODIC.into()),
                storage_classes,
                network_capabilities,
                labels,
                hypervisor_capabilities,
                authority_mode: authority_mode_text(inventory.authority_mode),
                reported_unix_ms: now,
            })
            .await?;

        // Re-sync the node's fabric identity (ADR-021 §5) on every periodic
        // inventory report — the agent re-reports every 30 s, so a key
        // rotation or a fabric-IP wipe converges without re-enrollment.
        // Fails closed like the rest of this handler's store writes; the
        // agent retries the report.
        if !inventory.wireguard_public_key.is_empty() {
            self.vtep_repo
                .register_fabric_identity(
                    node_id.as_str(),
                    &inventory.wireguard_public_key,
                    inventory.underlay_mtu,
                    // The inventory report carries no explicit underlay
                    // endpoint, so derive one from the transport-level peer
                    // address the control plane actually observed, pinned
                    // to the fabric WireGuard port. First registration
                    // wins at the store layer, so the endpoint derived at
                    // enrollment (or the first report) is never rotated by
                    // a later re-report. NAT / LB / proxy caveat: behind
                    // NAT this is the NAT's mapped address — usually
                    // exactly what remote peers must dial — but a NAT,
                    // load balancer, or proxy that maps the gRPC
                    // connection differently from the node's WireGuard
                    // listener yields an unreachable endpoint; in
                    // particular, a transient LB/proxy/VPN reconnection
                    // must not replace a previously-good endpoint (that
                    // is exactly what the first-registration-wins policy
                    // prevents). The fabric plan compiler surfaces
                    // unreachable peers (fail closed) instead of guessing.
                    peer_addr.map(derive_underlay_endpoint).as_deref(),
                )
                .await?;
        }

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id,
                status: STATUS_OK.into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: SUMMARY_INVENTORY_REPORTED.into(),
            }),
        })
    }

    async fn report_service_versions(
        &self,
        request: proto::ReportServiceVersionsRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let versions = request
            .versions
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing versions".into()))?;

        let meta = request
            .meta
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing meta".into()))?;

        let node_id = NodeId::new(versions.node_id.clone()).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;

        let now = self.now_ms();

        self.node_repo
            .ensure_node_record(&node_id, None, None, now)
            .await?;

        let components = [
            (COMPONENT_AGENT, versions.chv_agent_version),
            (COMPONENT_STORD, versions.chv_stord_version),
            (COMPONENT_NWD, versions.chv_nwd_version),
            (COMPONENT_CHV, versions.cloud_hypervisor_version),
            (COMPONENT_HOST, versions.host_bundle_version),
        ];

        for (name, version) in components {
            if !version.is_empty() {
                self.node_repo
                    .append_version(&NodeVersionInput {
                        node_id: node_id.clone(),
                        component_name: name.into(),
                        version: version.clone(),
                        source: Some(SOURCE_PERIODIC.into()),
                        reported_unix_ms: now,
                    })
                    .await?;

                // The node row's `agent_version` is what the node API and
                // cluster summary display. Enrollment is otherwise its only
                // writer, so an in-place upgrade would keep showing the
                // enrollment version until the next enrollment. The agent's
                // own periodic version report is the current truth.
                if name == COMPONENT_AGENT {
                    self.node_repo
                        .refresh_agent_version(&node_id, &version, now)
                        .await?;
                }
            }
        }

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id,
                status: STATUS_OK.into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: SUMMARY_VERSIONS_REPORTED.into(),
            }),
        })
    }
}
