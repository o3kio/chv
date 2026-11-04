use crate::error::ControlPlaneServiceError;
use async_trait::async_trait;
use chv_controlplane_store::{
    DesiredStateRepository, EventAppendInput, EventRepository, NetworkStatusPatchInput,
    NodeDrainIntentInput, NodeRepository, NodeSchedulingPatchInput, NodeStatePatchInput,
    OperationCreateInput, OperationRepository, OperationStatusUpdateInput, VmDesiredStateInput,
    VmPowerStatePatchInput, VmResourcesPatchInput, VolumeAttachmentPatchInput,
    VolumeResizePatchInput,
};
use chv_controlplane_types::domain::{
    EventSeverity, EventType, Generation, NodeId, NodeState, OperationId, OperationStatus,
    ResourceId, ResourceKind,
};
use chv_controlplane_types::fragment::VmSpec;
use chv_errors::ChvError;
use control_plane_node_api::control_plane_node_api as proto;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

#[async_trait]
pub trait LifecycleService: Send + Sync {
    async fn create_vm(
        &self,
        request: proto::CreateVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn start_vm(
        &self,
        request: proto::StartVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn stop_vm(
        &self,
        request: proto::StopVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn reboot_vm(
        &self,
        request: proto::RebootVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn delete_vm(
        &self,
        request: proto::DeleteVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn resize_vm(
        &self,
        request: proto::ResizeVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn attach_volume(
        &self,
        request: proto::AttachVolumeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn detach_volume(
        &self,
        request: proto::DetachVolumeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn resize_volume(
        &self,
        request: proto::ResizeVolumeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn snapshot_volume(
        &self,
        request: proto::SnapshotVolumeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn restore_volume(
        &self,
        request: proto::RestoreVolumeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn delete_volume_snapshot(
        &self,
        request: proto::DeleteVolumeSnapshotRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn clone_volume(
        &self,
        request: proto::CloneVolumeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn pause_node_scheduling(
        &self,
        request: proto::PauseNodeSchedulingRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn resume_node_scheduling(
        &self,
        request: proto::ResumeNodeSchedulingRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn drain_node(
        &self,
        request: proto::DrainNodeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn enter_maintenance(
        &self,
        request: proto::EnterMaintenanceRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn exit_maintenance(
        &self,
        request: proto::ExitMaintenanceRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn pause_vm(
        &self,
        request: proto::PauseVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn resume_vm(
        &self,
        request: proto::ResumeVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn power_button_vm(
        &self,
        request: proto::PowerButtonVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn add_disk(
        &self,
        request: proto::AddDiskRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn remove_device(
        &self,
        request: proto::RemoveDeviceRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn add_net(
        &self,
        request: proto::AddNetRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn resize_disk(
        &self,
        request: proto::ResizeDiskRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn snapshot_vm(
        &self,
        request: proto::SnapshotVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn restore_snapshot(
        &self,
        request: proto::RestoreSnapshotRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn coredump_vm(
        &self,
        request: proto::CoredumpVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn start_network(
        &self,
        request: proto::StartNetworkRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn stop_network(
        &self,
        request: proto::StopNetworkRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn restart_network(
        &self,
        request: proto::RestartNetworkRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn migrate_vm(
        &self,
        request: proto::MigrateVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn update_overlay(
        &self,
        request: proto::UpdateOverlayRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn send_gratuitous_arp(
        &self,
        request: proto::SendGratuitousArpRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    /// Node-scoped operator action for restart-interrupted
    /// (InspectRequired) core-journal operations. The control plane RELAYS
    /// it to the owning agent's LifecycleService (the agent validates and
    /// terminal-persists against the journal); it never journals on the
    /// node's behalf. Node-local invocation against the agent directly (or
    /// the node's Core API) remains the fallback when the agent socket is
    /// unreachable from the control plane.
    async fn resolve_inspect_required_operation(
        &self,
        request: proto::ResolveInspectRequiredOperationRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;
}

#[derive(Clone)]
pub struct LifecycleServiceImplementation {
    node_repo: NodeRepository,
    operation_repo: OperationRepository,
    event_repo: EventRepository,
    desired_state_repo: DesiredStateRepository,
    /// Node egress for operator relays (resolve-inspect-required): the
    /// client pool plus the agent socket pattern used to reach a node.
    /// Absent in unit-test constructions — the relay then fails closed
    /// with `Unsupported` instead of silently mis-relaying.
    node_egress: Option<NodeEgress>,
}

/// Control-plane → agent egress for operator relays.
#[derive(Clone)]
struct NodeEgress {
    pool: crate::node_client_pool::NodeClientPool,
    agent_socket_pattern: String,
}

impl LifecycleServiceImplementation {
    pub fn new(
        node_repo: NodeRepository,
        operation_repo: OperationRepository,
        event_repo: EventRepository,
        desired_state_repo: DesiredStateRepository,
    ) -> Self {
        Self {
            node_repo,
            operation_repo,
            event_repo,
            desired_state_repo,
            node_egress: None,
        }
    }

    /// Wires the control-plane → agent egress used to relay operator
    /// actions (resolve-inspect-required) to the owning node's agent.
    pub fn with_node_egress(
        mut self,
        pool: crate::node_client_pool::NodeClientPool,
        agent_socket_pattern: String,
    ) -> Self {
        self.node_egress = Some(NodeEgress {
            pool,
            agent_socket_pattern,
        });
        self
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }

    fn meta_from_request(
        &self,
        meta: Option<proto::RequestMeta>,
    ) -> Result<proto::RequestMeta, ControlPlaneServiceError> {
        meta.ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing meta".into()))
    }

    fn normalize_requested_by(meta: &proto::RequestMeta) -> Option<String> {
        if meta.requested_by.is_empty() {
            None
        } else {
            Some(meta.requested_by.clone())
        }
    }

    fn desired_generation_from_meta(
        meta: &proto::RequestMeta,
    ) -> Result<Generation, ControlPlaneServiceError> {
        if meta.desired_state_version.is_empty() {
            return Err(ControlPlaneServiceError::InvalidArgument(
                "desired_state_version is required".into(),
            ));
        }
        Generation::from_str(&meta.desired_state_version).map_err(|_| {
            ControlPlaneServiceError::InvalidArgument("generation must be numeric".into())
        })
    }

    fn ok_ack(operation_id: &OperationId, summary: &str) -> proto::AckResponse {
        proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: operation_id.to_string(),
                status: "OK".into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: summary.into(),
            }),
        }
    }

    fn parse_node_id(s: String) -> Result<NodeId, ControlPlaneServiceError> {
        let node_id = NodeId::new(s).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;
        // Request-supplied node ids eventually reach agent-socket pattern
        // substitution (orchestrator dispatch, migration, the resolve relay):
        // reject traversal-shaped ids at this single request boundary.
        // `is_safe_path_component` (not the stricter `is_safe_id`) so ids a
        // deployment legitimately uses keep working — only the traversal
        // vectors (separators, dot components, control characters) fail.
        if !chv_common::is_safe_path_component(node_id.as_str()) {
            return Err(ControlPlaneServiceError::InvalidArgument(format!(
                "invalid node_id {:?}: must be a single path component",
                node_id.as_str()
            )));
        }
        Ok(node_id)
    }

    fn parse_vm_id(s: String) -> Result<ResourceId, ControlPlaneServiceError> {
        ResourceId::new(s)
            .map_err(|e| ControlPlaneServiceError::InvalidArgument(format!("invalid vm_id: {}", e)))
    }

    fn parse_volume_id(s: String) -> Result<ResourceId, ControlPlaneServiceError> {
        ResourceId::new(s).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid volume_id: {}", e))
        })
    }

    fn parse_network_id(s: String) -> Result<ResourceId, ControlPlaneServiceError> {
        ResourceId::new(s).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid network_id: {}", e))
        })
    }

    /// #378: accept-time rejection of the volume snapshot family
    /// (snapshot / restore / delete-snapshot / clone) on core-managed
    /// nodes. The agent's fail-closed dispatch (`Unimplemented`) remains
    /// the enforcement boundary; this check is UX hardening so an
    /// operator gets an immediate `InvalidArgument` (HTTP 400 through the
    /// BFF's `map_ack`) instead of a 200-accepted operation that burns
    /// ~70 s of dispatch retries against a node that will never execute
    /// it. Must be called BEFORE any journaling: a rejection leaves no
    /// operations row and no desired-state intent. Fails OPEN on unknown
    /// mode (NULL / never reported / legacy / core-native) — a node whose
    /// inventory has not landed keeps accepting exactly as before.
    ///
    /// #495 review: the node checked is the node the dispatch actually
    /// uses. The orchestrator resolves the dispatch node from the VOLUMES
    /// row — `(SELECT node_id FROM volumes WHERE volume_id =
    /// operations.resource_id)` in the claim queries — never from the
    /// request, so this resolves `volumes.node_id` for the volume the
    /// request names; the request's own `node_id` field is advisory for
    /// this check on the snapshot/restore/delete-snapshot surfaces (the
    /// BFF always sends the volume's node anyway). Fails OPEN when the
    /// volume's node is unknown (no `volumes` row yet, a NULL
    /// `volumes.node_id` — the column is `ON DELETE SET NULL` — or an
    /// unparseable stored value): the mode check is hardening only, the
    /// agent-side enforcement is the backstop, and a missing volume row
    /// must not change the pre-#378 accept-then-fail-later shape.
    async fn ensure_volume_snapshot_family_supported(
        &self,
        volume_id: &ResourceId,
        surface: &str,
    ) -> Result<(), ControlPlaneServiceError> {
        let volume_node = self
            .desired_state_repo
            .get_volume_summary(volume_id)
            .await?
            .and_then(|summary| summary.node_id)
            .and_then(|node| NodeId::new(node).ok());
        match volume_node {
            Some(node_id) => self.ensure_node_not_core_managed(&node_id, surface).await,
            None => Ok(()),
        }
    }

    /// The mode check itself, on an already-resolved node. Clone calls
    /// this directly with its placement node (the source volume's node —
    /// the node the clone journals, materializes the target on, and the
    /// orchestrator therefore dispatches to).
    async fn ensure_node_not_core_managed(
        &self,
        node_id: &NodeId,
        surface: &str,
    ) -> Result<(), ControlPlaneServiceError> {
        let mode = self.node_repo.get_authority_mode(node_id).await?;
        if mode.as_deref() == Some(chv_controlplane_store::AUTHORITY_MODE_CORE_MANAGED) {
            return Err(ControlPlaneServiceError::InvalidArgument(format!(
                "volume {surface} is not supported on core-managed nodes"
            )));
        }
        Ok(())
    }

    /// #379 DP4: accept-time storage-class capability check, on an
    /// already-resolved node. The node's last reported
    /// `NodeInventory.storage_classes` — the REAL stord backend class
    /// since PR 3's inventory change, no longer the pre-#379 directory
    /// probe — must cover the requested class, compared with DP3
    /// normalization (`localdisk`/`local-file` fold to `local`; a NULL
    /// request class is `local`).
    ///
    /// Same discipline as [`Self::ensure_node_not_core_managed`] (#495
    /// shape): UX hardening, not the enforcement boundary — an operator
    /// gets an immediate `InvalidArgument` (HTTP 400 through the BFF's
    /// `map_ack`) instead of a 200-accepted operation that burns dispatch
    /// retries against a node whose stord can never open the volume.
    /// Fails OPEN when the node has never reported classes (empty list:
    /// inventory not landed yet, or a pre-#379 agent still reporting the
    /// empty probe) — the agent's open and stord's own backend
    /// validation remain the backstop. Must be called BEFORE any
    /// journaling: a rejection leaves no operations row and no
    /// desired-state intent.
    async fn ensure_node_offers_storage_class(
        &self,
        node_id: &NodeId,
        requested: Option<&str>,
        surface: &str,
    ) -> Result<(), ControlPlaneServiceError> {
        // The fetch + decide + message composition lives on the store's
        // NodeRepository (`node_storage_class_rejection`) so the BFF's
        // create surface — the production create path, which cannot
        // reach this crate's service — shares this exact decision
        // (#512 second-pass follow-up). This wrapper only maps the
        // rejection to the RPC's `InvalidArgument` (HTTP 400 through
        // the BFF's `map_ack`).
        match self
            .node_repo
            .node_storage_class_rejection(node_id, requested, surface)
            .await?
        {
            Some(reason) => Err(ControlPlaneServiceError::InvalidArgument(reason)),
            None => Ok(()),
        }
    }

    /// The distinct storage classes a create_vm request's disk specs
    /// name (#379 DP4): `disks[].backend_class` from the raw agent-spec
    /// JSON, `None` for a classless disk (NULL = local). A spec without
    /// disks — the common BFF shape, where volumes attach separately and
    /// the dispatch tier assembles disks from the volume rows — yields
    /// nothing, as does a disks entry of unexpected shape: the typed
    /// `VmSpec` parse at the call site already rejects malformed JSON,
    /// and anything else is left to the dispatch tier's own validation
    /// (fail-open, the #495 discipline).
    fn vm_spec_disk_classes(vm_spec_json: &[u8]) -> Vec<Option<String>> {
        let Ok(spec) = serde_json::from_slice::<serde_json::Value>(vm_spec_json) else {
            return Vec::new();
        };
        let Some(disks) = spec.get("disks").and_then(|d| d.as_array()) else {
            return Vec::new();
        };
        let mut classes: Vec<Option<String>> = Vec::new();
        for disk in disks {
            let class = disk
                .get("backend_class")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string());
            if !classes.contains(&class) {
                classes.push(class);
            }
        }
        classes
    }

    fn resource_id_from_node_id(node_id: &NodeId) -> Result<ResourceId, ControlPlaneServiceError> {
        ResourceId::new(node_id.as_str())
            .map_err(|e| ControlPlaneServiceError::Internal(format!("invalid resource_id: {}", e)))
    }

    async fn require_node_exists(&self, node_id: &NodeId) -> Result<(), ControlPlaneServiceError> {
        let row = sqlx::query("SELECT 1 FROM nodes WHERE node_id = ?")
            .bind(node_id.as_str())
            .fetch_optional(self.node_repo.pool())
            .await
            .map_err(|e| {
                ControlPlaneServiceError::Internal(format!("failed to check node: {}", e))
            })?;
        if row.is_none() {
            return Err(ControlPlaneServiceError::NotFound(format!(
                "node {} not found",
                node_id
            )));
        }
        Ok(())
    }

    async fn create_operation_and_emit(
        &self,
        operation_type: &str,
        node_id: NodeId,
        resource_kind: ResourceKind,
        resource_id: Option<ResourceId>,
        meta: &proto::RequestMeta,
        idempotency_discriminator: Option<String>,
    ) -> Result<(OperationId, Generation, bool), ControlPlaneServiceError> {
        self.require_node_exists(&node_id).await?;
        let now = Self::now_ms();
        let desired_generation = Self::desired_generation_from_meta(meta)?;
        let desired_generation_str = desired_generation.to_string();
        let resource_id_str = resource_id
            .as_ref()
            .map(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        let idempotency_key = if meta.operation_id.trim().is_empty() {
            match idempotency_discriminator {
                Some(discriminator) => format!(
                    "{}:{}:{}:{}:{}",
                    operation_type, node_id, resource_id_str, desired_generation_str, discriminator
                ),
                None => format!(
                    "{}:{}:{}:{}",
                    operation_type, node_id, resource_id_str, desired_generation_str
                ),
            }
        } else {
            format!("request:{}", meta.operation_id.trim())
        };

        let operation_id = OperationId::new(format!(
            "{}-{}",
            operation_type,
            chv_common::gen_resource_id()
        ))
        .map_err(|e| ControlPlaneServiceError::Internal(format!("invalid operation_id: {}", e)))?;

        let caller_correlation_id = if meta.operation_id.trim().is_empty() {
            None
        } else {
            Some(meta.operation_id.trim().to_string())
        };

        let receipt = self
            .operation_repo
            .create_or_get(&OperationCreateInput {
                operation_id,
                idempotency_key,
                resource_kind,
                resource_id: resource_id.clone(),
                operation_type: operation_type.into(),
                status: OperationStatus::Pending,
                requested_by: Self::normalize_requested_by(meta),
                updated_by: None,
                desired_generation: Some(desired_generation),
                observed_generation: None,
                correlation_id: caller_correlation_id.clone(),
                requested_unix_ms: now,
            })
            .await?;

        self.event_repo
            .append(&EventAppendInput {
                occurred_unix_ms: now,
                event_type: EventType::OperationStarted,
                severity: EventSeverity::Info,
                resource_kind: Some(resource_kind),
                resource_id,
                node_id: Some(node_id),
                operation_id: Some(receipt.operation_id.clone()),
                actor_id: None,
                requested_by: Self::normalize_requested_by(meta),
                correlation_id: caller_correlation_id,
                message: format!("{} started", operation_type),
                details: None,
            })
            .await?;

        Ok((receipt.operation_id, desired_generation, receipt.created))
    }

    async fn accept_operation(
        &self,
        operation_id: &OperationId,
    ) -> Result<(), ControlPlaneServiceError> {
        self.operation_repo
            .update_status(&OperationStatusUpdateInput {
                operation_id: operation_id.clone(),
                status: OperationStatus::Accepted,
                error_code: None,
                error_message: None,
                observed_generation: None,
                updated_by: None,
                updated_unix_ms: Self::now_ms(),
            })
            .await
            .map_err(|e| {
                ControlPlaneServiceError::Internal(format!(
                    "failed to update operation status: {}",
                    e
                ))
            })
    }

    async fn fail_operation(
        &self,
        operation_id: &OperationId,
        original_error: &ControlPlaneServiceError,
    ) -> Result<(), ControlPlaneServiceError> {
        self.operation_repo
            .update_status(&OperationStatusUpdateInput {
                operation_id: operation_id.clone(),
                status: OperationStatus::Failed,
                error_code: Some("INTENT_PERSISTENCE_FAILED".into()),
                error_message: Some(original_error.to_string()),
                observed_generation: None,
                updated_by: None,
                updated_unix_ms: Self::now_ms(),
            })
            .await
            .map_err(|e| {
                ControlPlaneServiceError::Internal(format!(
                    "failed to update operation status: {}",
                    e
                ))
            })?;

        self.event_repo
            .append(&EventAppendInput {
                occurred_unix_ms: Self::now_ms(),
                event_type: EventType::OperationFailed,
                severity: EventSeverity::Error,
                resource_kind: None,
                resource_id: None,
                node_id: None,
                operation_id: Some(operation_id.clone()),
                actor_id: None,
                requested_by: None,
                correlation_id: None,
                message: format!("operation failed: {}", original_error),
                details: None,
            })
            .await?;

        Ok(())
    }

    async fn persist_intent_and_accept<F, Fut, E>(
        &self,
        operation_id: &OperationId,
        persist: F,
    ) -> Result<(), ControlPlaneServiceError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(), E>>,
        E: Into<ControlPlaneServiceError>,
    {
        match persist().await {
            Ok(()) => self.accept_operation(operation_id).await,
            Err(e) => {
                let err = e.into();
                self.fail_operation(operation_id, &err).await?;
                Err(err)
            }
        }
    }
}

#[async_trait]
impl LifecycleService for LifecycleServiceImplementation {
    async fn create_vm(
        &self,
        request: proto::CreateVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm = request
            .vm
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing vm".into()))?;
        let vm_id = Self::parse_vm_id(vm.vm_id)?;

        let spec: VmSpec = if vm.vm_spec_json.is_empty() {
            VmSpec {
                cpu_count: None,
                memory_bytes: None,
                image_ref: None,
                boot_mode: None,
                desired_power_state: None,
            }
        } else {
            serde_json::from_slice(&vm.vm_spec_json).map_err(|e| {
                ControlPlaneServiceError::InvalidArgument(format!("invalid vm_spec_json: {}", e))
            })?
        };

        // #379 DP4: accept-time capability check — every class the
        // request's disk specs name (a classless disk names `local`) must
        // be offered by the node this create journals on, the same node
        // the orchestrator later dispatches to. Before any journaling:
        // a rejection leaves no operations row and no desired-state
        // intent. Fails OPEN on a node that never reported classes.
        for class in Self::vm_spec_disk_classes(&vm.vm_spec_json) {
            self.ensure_node_offers_storage_class(&node_id, class.as_deref(), "vm create")
                .await?;
        }

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "CreateVm",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(String::from_utf8_lossy(&vm.vm_spec_json).into_owned()),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .upsert_vm(&VmDesiredStateInput {
                    vm_id: vm_id.clone(),
                    node_id: Some(node_id.clone()),
                    display_name: vm_id.as_str().into(),
                    tenant_id: None,
                    placement_policy: None,
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    target_node_id: Some(node_id.clone()),
                    cpu_count: spec.cpu_count,
                    memory_bytes: spec.memory_bytes,
                    image_ref: spec.image_ref,
                    boot_mode: spec.boot_mode,
                    desired_power_state: Some("Created".into()),
                    vsock_enabled: None,
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "create vm accepted"))
    }

    async fn start_vm(
        &self,
        request: proto::StartVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "StartVm",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                None,
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_vm_power_state(&VmPowerStatePatchInput {
                    vm_id: vm_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    target_node_id: Some(node_id.clone()),
                    desired_power_state: Some("Running".into()),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "start vm accepted"))
    }

    async fn stop_vm(
        &self,
        request: proto::StopVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let operation_type = if request.force {
            "ForceStopVm"
        } else {
            "StopVm"
        };
        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                operation_type,
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(format!("force={}", request.force)),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_vm_power_state(&VmPowerStatePatchInput {
                    vm_id: vm_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    target_node_id: Some(node_id.clone()),
                    desired_power_state: Some("Stopped".into()),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "stop vm accepted"))
    }

    async fn reboot_vm(
        &self,
        request: proto::RebootVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "RebootVm",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(format!("force={}", request.force)),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_vm_power_state(&VmPowerStatePatchInput {
                    vm_id: vm_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    target_node_id: Some(node_id.clone()),
                    // A reboot is an operation, not a destination: the
                    // desired end-state of a successful reboot is a running
                    // VM. Writing the transient "Rebooting" here stranded
                    // the VM's desired state forever (nothing ever rewrote
                    // it after the operation completed), and because the
                    // BFF renders COALESCE(desired_power_state,
                    // runtime_status), a rebooted VM displayed "Rebooting"
                    // indefinitely — observed in the M2.5 qualification
                    // where the journal showed RebootVm succeeded while the
                    // API kept reporting Rebooting minutes later. The
                    // in-flight reboot remains visible through the
                    // operation/task surfaces; the power state reflects the
                    // desired destination.
                    desired_power_state: Some("Running".into()),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "reboot vm accepted"))
    }

    async fn delete_vm(
        &self,
        request: proto::DeleteVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "DeleteVm",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(format!("force={}", request.force)),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_vm_power_state(&VmPowerStatePatchInput {
                    vm_id: vm_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    target_node_id: Some(node_id.clone()),
                    desired_power_state: Some("Deleted".into()),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "delete vm accepted"))
    }

    async fn resize_vm(
        &self,
        request: proto::ResizeVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "ResizeVm",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(format!(
                    "vcpus={:?},memory_bytes={:?}",
                    request.desired_vcpus, request.desired_memory_bytes
                )),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_vm_resources(&VmResourcesPatchInput {
                    vm_id: vm_id.clone(),
                    cpu_count: request.desired_vcpus.map(|v| v as i32),
                    memory_bytes: request.desired_memory_bytes.map(|v| v as i64),
                    desired_generation,
                    requested_by: Self::normalize_requested_by(&meta),
                    target_node_id: Some(node_id.clone()),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "resize vm accepted"))
    }

    async fn attach_volume(
        &self,
        request: proto::AttachVolumeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let volume = request
            .volume
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing volume".into()))?;
        let volume_id = Self::parse_volume_id(volume.volume_id)?;
        let vm_id = Self::parse_vm_id(volume.vm_id)?;

        // #379 DP4: accept-time capability check in the #495 shape. The
        // node checked is the node the dispatch actually uses — the
        // VOLUMES row's node, which the orchestrator's claim query
        // resolves, never the request's advisory `node_id` — and the
        // class checked is the VOLUMES row's `storage_class` (the A8
        // dispatch producer's source), not the request's spec_json.
        // Fails OPEN when the volume row or its node is unknown (no row
        // yet, NULL `volumes.node_id`, unparseable id): a missing volume
        // must not change the accept-then-fail-later shape, and the
        // agent's open remains the enforcement boundary.
        if let Some(summary) = self
            .desired_state_repo
            .get_volume_summary(&volume_id)
            .await?
        {
            if let Some(node) = summary
                .node_id
                .as_deref()
                .and_then(|node| NodeId::new(node).ok())
            {
                self.ensure_node_offers_storage_class(
                    &node,
                    summary.storage_class.as_deref(),
                    "volume attach",
                )
                .await?;
            }
        }

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "AttachVolume",
                node_id.clone(),
                ResourceKind::Volume,
                Some(volume_id.clone()),
                &meta,
                Some(format!("vm={}", vm_id)),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_volume_attachment(&VolumeAttachmentPatchInput {
                    volume_id: volume_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    attached_vm_id: Some(vm_id),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "attach volume accepted"))
    }

    async fn detach_volume(
        &self,
        request: proto::DetachVolumeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let attached_vm_id = Self::parse_vm_id(request.vm_id)?;
        let volume_id = Self::parse_volume_id(request.volume_id)?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "DetachVolume",
                node_id.clone(),
                ResourceKind::Volume,
                Some(volume_id.clone()),
                &meta,
                Some(format!("vm={}:force={}", attached_vm_id, request.force)),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_volume_attachment(&VolumeAttachmentPatchInput {
                    volume_id: volume_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    attached_vm_id: None,
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "detach volume accepted"))
    }

    async fn resize_volume(
        &self,
        request: proto::ResizeVolumeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let volume_id = Self::parse_volume_id(request.volume_id)?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "ResizeVolume",
                node_id.clone(),
                ResourceKind::Volume,
                Some(volume_id.clone()),
                &meta,
                Some(format!("size={}", request.new_size_bytes)),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_volume_resize(&VolumeResizePatchInput {
                    volume_id: volume_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    resize_to_bytes: Some(request.new_size_bytes as i64),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "resize volume accepted"))
    }

    async fn snapshot_volume(
        &self,
        request: proto::SnapshotVolumeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let volume_id = Self::parse_volume_id(request.volume_id)?;

        // #378: reject before journaling. #495 review: the helper checks
        // the VOLUME's node (volumes.node_id — the node the orchestrator
        // dispatches to), not the request's node_id, which is advisory
        // here for direct-gRPC callers.
        self.ensure_volume_snapshot_family_supported(&volume_id, "snapshot")
            .await?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "SnapshotVolume",
                node_id.clone(),
                ResourceKind::Volume,
                Some(volume_id.clone()),
                &meta,
                Some(format!("snapshot_name={}", request.snapshot_name)),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_volume_snapshot(&chv_controlplane_store::VolumeSnapshotPatchInput {
                    volume_id: volume_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    snapshot_op: Some("create".to_string()),
                    snapshot_name: Some(request.snapshot_name),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "snapshot volume accepted"))
    }

    async fn restore_volume(
        &self,
        request: proto::RestoreVolumeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let volume_id = Self::parse_volume_id(request.volume_id)?;

        // #378: reject before journaling (same volume-node resolution the
        // dispatch uses — see snapshot_volume).
        self.ensure_volume_snapshot_family_supported(&volume_id, "restore")
            .await?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "RestoreVolume",
                node_id.clone(),
                ResourceKind::Volume,
                Some(volume_id.clone()),
                &meta,
                Some(format!("snapshot_name={}", request.snapshot_name)),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_volume_snapshot(&chv_controlplane_store::VolumeSnapshotPatchInput {
                    volume_id: volume_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    snapshot_op: Some("restore".to_string()),
                    snapshot_name: Some(request.snapshot_name),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "restore volume accepted"))
    }

    async fn delete_volume_snapshot(
        &self,
        request: proto::DeleteVolumeSnapshotRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let volume_id = Self::parse_volume_id(request.volume_id)?;

        // #378: reject before journaling (same volume-node resolution the
        // dispatch uses — see snapshot_volume).
        self.ensure_volume_snapshot_family_supported(&volume_id, "snapshot deletion")
            .await?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "DeleteVolumeSnapshot",
                node_id.clone(),
                ResourceKind::Volume,
                Some(volume_id.clone()),
                &meta,
                Some(format!("snapshot_name={}", request.snapshot_name)),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_volume_snapshot(&chv_controlplane_store::VolumeSnapshotPatchInput {
                    volume_id: volume_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    snapshot_op: Some("delete".to_string()),
                    snapshot_name: Some(request.snapshot_name),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(
            &operation_id,
            "delete volume snapshot accepted",
        ))
    }

    async fn clone_volume(
        &self,
        request: proto::CloneVolumeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let source_volume_id = Self::parse_volume_id(request.source_volume_id)?;
        let target_volume_id = Self::parse_volume_id(request.target_volume_id)?;

        // #380: the clone intent used to PATCH a `volume_desired_state`
        // row for the target volume that nothing ever created — the FK
        // violation surfaced as a bare `volume with id {target} not
        // found`. Validate up front and materialize the target below in
        // one store transaction (`volumes` row with the source's shape +
        // the VDS intent row; #384 made that transaction strict and
        // self-contained — see `materialize_clone_target`).
        let source = self
            .desired_state_repo
            .get_volume_summary(&source_volume_id)
            .await?
            .ok_or_else(|| {
                ControlPlaneServiceError::NotFound(format!(
                    "source volume {} not found",
                    source_volume_id
                ))
            })?;
        // #384: the target-absence pre-check runs only for requests
        // WITHOUT a caller-supplied `meta.operation_id` (the BFF and
        // chvctl shape — every existing client). A caller-supplied
        // operation_id opts into idempotent-replay semantics: on replay
        // the target legitimately exists (this operation materialized
        // it earlier), so the pre-check must not reject the request —
        // the transactional core in the store decides (own earlier
        // materialization → idempotent success; anything else on the
        // target id → Conflict). The source pre-check always runs:
        // volume rows are never deleted, so a replay's source is as
        // present as it was the first time.
        let replay_eligible = !meta.operation_id.trim().is_empty();
        if !replay_eligible
            && self
                .desired_state_repo
                .get_volume_summary(&target_volume_id)
                .await?
                .is_some()
        {
            return Err(ControlPlaneServiceError::InvalidArgument(format!(
                "target volume id already exists: {}",
                target_volume_id
            )));
        }

        // #381 review: the clone target materializes on the SOURCE's node
        // (coherent placement — the orchestrator resolves the dispatch node
        // from the volumes row), so the operation record must journal that
        // same node; journaling the raw request node left metadata that
        // contradicted actual placement for non-BFF callers.
        let placement_node_id = source
            .node_id
            .as_deref()
            .map(NodeId::new)
            .transpose()
            .map_err(|e| {
                ControlPlaneServiceError::InvalidArgument(format!("invalid source node id: {}", e))
            })?
            .unwrap_or_else(|| node_id.clone());

        // #378: reject before journaling — checked against the PLACEMENT
        // node (the source's node, the one the operation journals, the
        // target materializes on, and the orchestrator dispatches to),
        // not the raw request node_id.
        self.ensure_node_not_core_managed(&placement_node_id, "clone")
            .await?;

        let (operation_id, desired_generation, operation_created) = self
            .create_operation_and_emit(
                "CloneVolume",
                placement_node_id.clone(),
                ResourceKind::Volume,
                Some(target_volume_id.clone()),
                &meta,
                Some(format!("source={}", source_volume_id)),
            )
            .await?;

        // #384: the target materialization is one BEGIN IMMEDIATE
        // transaction in the store (strict insert + in-tx source read +
        // the generation-guarded VDS intent row). Two things this fixes
        // that the old pre-check-then-`upsert_volume` shape could not:
        //
        // - a racing clone onto the SAME caller-supplied target id used
        //   to pass the pre-check above and then last-writer-wins
        //   overwrite the winner's row (two live conflicting operations
        //   on one target); the strict insert now fails the loser closed
        //   with `Conflict` BEFORE its intent persists — the loser's
        //   operation is journaled Failed, no second dispatch shape is
        //   written, and the volumes row keeps exactly the winner's
        //   shape;
        // - a resize of the SOURCE committing between the pre-check read
        //   above and the write used to shape the target from the stale
        //   capacity; the source row is now re-read inside the same
        //   transaction that inserts the target (under the RESERVED
        //   lock), so the read and the write cannot split.
        //
        // `own_replay` distinguishes the two ways the strict insert can
        // find the target row already present: a replayed
        // `meta.operation_id` (the operation receipt came back
        // not-created — this request ran before and already
        // materialized the target) is an idempotent success when the
        // existing row matches this operation's shape; anything else is
        // a racing different request and fails closed with Conflict.
        let requested_by = Self::normalize_requested_by(&meta);
        self.persist_intent_and_accept(&operation_id, || {
            let spec = chv_controlplane_store::CloneTargetSpec {
                target_volume_id: target_volume_id.clone(),
                placement_node_id: Some(placement_node_id.clone()),
                display_name: target_volume_id.as_str().to_string(),
                desired_generation,
                requested_by: requested_by.clone(),
                requested_unix_ms: Self::now_ms(),
            };
            async move {
                self.desired_state_repo
                    .materialize_clone_target(
                        &source_volume_id,
                        &spec,
                        /* own_replay: */ !operation_created,
                    )
                    .await
                    .map(|_| ())
            }
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "clone volume accepted"))
    }

    async fn pause_node_scheduling(
        &self,
        request: proto::PauseNodeSchedulingRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;

        let resource_id = Self::resource_id_from_node_id(&node_id)?;
        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "PauseNodeScheduling",
                node_id.clone(),
                ResourceKind::Node,
                Some(resource_id),
                &meta,
                Some("paused=true".into()),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.node_repo
                .set_scheduling_paused(&NodeSchedulingPatchInput {
                    node_id: node_id.clone(),
                    desired_generation,
                    scheduling_paused: true,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(
            &operation_id,
            "pause node scheduling accepted",
        ))
    }

    async fn resume_node_scheduling(
        &self,
        request: proto::ResumeNodeSchedulingRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;

        let resource_id = Self::resource_id_from_node_id(&node_id)?;
        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "ResumeNodeScheduling",
                node_id.clone(),
                ResourceKind::Node,
                Some(resource_id),
                &meta,
                Some("paused=false".into()),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.node_repo
                .set_scheduling_paused(&NodeSchedulingPatchInput {
                    node_id: node_id.clone(),
                    desired_generation,
                    scheduling_paused: false,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(
            &operation_id,
            "resume node scheduling accepted",
        ))
    }

    async fn drain_node(
        &self,
        request: proto::DrainNodeRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;

        let resource_id = Self::resource_id_from_node_id(&node_id)?;
        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "DrainNode",
                node_id.clone(),
                ResourceKind::Node,
                Some(resource_id),
                &meta,
                Some(format!(
                    "allow_workload_stop={}",
                    request.allow_workload_stop
                )),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.node_repo
                .set_drain_intent(&NodeDrainIntentInput {
                    node_id: node_id.clone(),
                    desired_generation,
                    allow_workload_stop: Some(request.allow_workload_stop),
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "drain node accepted"))
    }

    async fn enter_maintenance(
        &self,
        request: proto::EnterMaintenanceRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;

        let resource_id = Self::resource_id_from_node_id(&node_id)?;
        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "EnterMaintenance",
                node_id.clone(),
                ResourceKind::Node,
                Some(resource_id),
                &meta,
                Some(request.reason.clone()),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.node_repo
                .set_state_preserving_policy(&NodeStatePatchInput {
                    node_id: node_id.clone(),
                    desired_state: NodeState::Maintenance,
                    desired_generation,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    state_reason: Some(request.reason),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "enter maintenance accepted"))
    }

    async fn exit_maintenance(
        &self,
        request: proto::ExitMaintenanceRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;

        let resource_id = Self::resource_id_from_node_id(&node_id)?;
        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "ExitMaintenance",
                node_id.clone(),
                ResourceKind::Node,
                Some(resource_id),
                &meta,
                None,
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.node_repo
                .set_state_preserving_policy(&NodeStatePatchInput {
                    node_id: node_id.clone(),
                    desired_state: NodeState::TenantReady,
                    desired_generation,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    state_reason: None,
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "exit maintenance accepted"))
    }

    async fn pause_vm(
        &self,
        request: proto::PauseVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "PauseVm",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                None,
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_vm_power_state(&VmPowerStatePatchInput {
                    vm_id: vm_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    target_node_id: Some(node_id.clone()),
                    desired_power_state: Some("Paused".into()),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "pause vm accepted"))
    }

    async fn resume_vm(
        &self,
        request: proto::ResumeVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "ResumeVm",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                None,
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_vm_power_state(&VmPowerStatePatchInput {
                    vm_id: vm_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    target_node_id: Some(node_id.clone()),
                    desired_power_state: Some("Running".into()),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "resume vm accepted"))
    }

    async fn power_button_vm(
        &self,
        request: proto::PowerButtonVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "PowerButtonVm",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                None,
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_vm_power_state(&VmPowerStatePatchInput {
                    vm_id: vm_id.clone(),
                    desired_generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    target_node_id: Some(node_id.clone()),
                    desired_power_state: Some("Stopped".into()),
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "power button vm accepted"))
    }

    async fn add_disk(
        &self,
        request: proto::AddDiskRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, _, _) = self
            .create_operation_and_emit(
                "AddDisk",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(format!("disk_id={}", request.disk_id)),
            )
            .await?;

        self.accept_operation(&operation_id).await?;

        Ok(Self::ok_ack(&operation_id, "add disk accepted"))
    }

    async fn remove_device(
        &self,
        request: proto::RemoveDeviceRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, _, _) = self
            .create_operation_and_emit(
                "RemoveDevice",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(format!("device_id={}", request.device_id)),
            )
            .await?;

        self.accept_operation(&operation_id).await?;

        Ok(Self::ok_ack(&operation_id, "remove device accepted"))
    }

    async fn add_net(
        &self,
        request: proto::AddNetRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, _, _) = self
            .create_operation_and_emit(
                "AddNet",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(format!("net_id={}", request.net_id)),
            )
            .await?;

        self.accept_operation(&operation_id).await?;

        Ok(Self::ok_ack(&operation_id, "add net accepted"))
    }

    async fn resize_disk(
        &self,
        request: proto::ResizeDiskRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, _, _) = self
            .create_operation_and_emit(
                "ResizeDisk",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(format!(
                    "disk_id={}:size={}",
                    request.disk_id, request.new_size_bytes
                )),
            )
            .await?;

        self.accept_operation(&operation_id).await?;

        Ok(Self::ok_ack(&operation_id, "resize disk accepted"))
    }

    async fn snapshot_vm(
        &self,
        request: proto::SnapshotVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, _, _) = self
            .create_operation_and_emit(
                "SnapshotVm",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(format!("destination={}", request.destination)),
            )
            .await?;

        if !request.destination.is_empty() {
            if let Err(e) =
                sqlx::query("UPDATE operations SET correlation_id = ? WHERE operation_id = ?")
                    .bind(&request.destination)
                    .bind(operation_id.as_str())
                    .execute(self.operation_repo.pool())
                    .await
            {
                tracing::error!(
                    error = %e,
                    operation_id = %operation_id,
                    "failed to set snapshot destination correlation_id"
                );
            }
        }

        self.accept_operation(&operation_id).await?;

        Ok(Self::ok_ack(&operation_id, "snapshot vm accepted"))
    }

    async fn restore_snapshot(
        &self,
        request: proto::RestoreSnapshotRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, _, _) = self
            .create_operation_and_emit(
                "RestoreSnapshot",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(format!("source={}", request.source)),
            )
            .await?;

        if !request.source.is_empty() {
            if let Err(e) =
                sqlx::query("UPDATE operations SET correlation_id = ? WHERE operation_id = ?")
                    .bind(&request.source)
                    .bind(operation_id.as_str())
                    .execute(self.operation_repo.pool())
                    .await
            {
                tracing::error!(
                    error = %e,
                    operation_id = %operation_id,
                    "failed to set restore source correlation_id"
                );
            }
        }

        self.accept_operation(&operation_id).await?;

        Ok(Self::ok_ack(&operation_id, "restore snapshot accepted"))
    }

    async fn coredump_vm(
        &self,
        request: proto::CoredumpVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;

        let (operation_id, _, _) = self
            .create_operation_and_emit(
                "CoredumpVm",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(format!("destination={}", request.destination)),
            )
            .await?;

        self.accept_operation(&operation_id).await?;

        Ok(Self::ok_ack(&operation_id, "coredump vm accepted"))
    }

    async fn start_network(
        &self,
        request: proto::StartNetworkRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let network_id = Self::parse_network_id(request.network_id)?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "StartNetwork",
                node_id.clone(),
                ResourceKind::Network,
                Some(network_id.clone()),
                &meta,
                None,
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_network_status(&NetworkStatusPatchInput {
                    network_id: network_id.clone(),
                    desired_generation,
                    desired_status: Some("Active".into()),
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "start network accepted"))
    }

    async fn stop_network(
        &self,
        request: proto::StopNetworkRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let network_id = Self::parse_network_id(request.network_id)?;

        let operation_type = if request.force {
            "ForceStopNetwork"
        } else {
            "StopNetwork"
        };
        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                operation_type,
                node_id.clone(),
                ResourceKind::Network,
                Some(network_id.clone()),
                &meta,
                Some(format!("force={}", request.force)),
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_network_status(&NetworkStatusPatchInput {
                    network_id: network_id.clone(),
                    desired_generation,
                    desired_status: Some("Inactive".into()),
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "stop network accepted"))
    }

    async fn restart_network(
        &self,
        request: proto::RestartNetworkRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let network_id = Self::parse_network_id(request.network_id)?;

        let (operation_id, desired_generation, _operation_created) = self
            .create_operation_and_emit(
                "RestartNetwork",
                node_id.clone(),
                ResourceKind::Network,
                Some(network_id.clone()),
                &meta,
                None,
            )
            .await?;

        self.persist_intent_and_accept(&operation_id, || async {
            self.desired_state_repo
                .set_network_status(&NetworkStatusPatchInput {
                    network_id: network_id.clone(),
                    desired_generation,
                    desired_status: Some("Restarting".into()),
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: None,
                    requested_unix_ms: Self::now_ms(),
                })
                .await
        })
        .await?;

        Ok(Self::ok_ack(&operation_id, "restart network accepted"))
    }

    async fn migrate_vm(
        &self,
        request: proto::MigrateVmRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;
        let vm_id = Self::parse_vm_id(request.vm_id)?;
        // Source and destination references bypass parse_node_id but reach
        // the same agent-socket substitution during migration execution:
        // gate them here so a traversal-shaped id fails as a clear
        // InvalidArgument at the RPC instead of mid-migration.
        for node_ref in [&request.source_node_id, &request.destination_node_id] {
            if !chv_common::is_safe_path_component(node_ref) {
                return Err(ControlPlaneServiceError::InvalidArgument(format!(
                    "invalid node reference {node_ref:?}: must be a single path component"
                )));
            }
        }

        // Issue #394 Option C: the correlation_id is the only carrier from
        // this RPC's `MigrationConfig` to the migration state machine (the
        // orchestrator rebuilds the config from it via
        // `from_correlation_id`), so the operator's stop-the-world opt-in
        // must ride it — dropping the key here would silently downgrade
        // the request to quiescent-assumed.
        let pause_first = request.config.as_ref().is_some_and(|c| c.pause_first);
        let (operation_id, _, _) = self
            .create_operation_and_emit(
                "MigrateVm",
                node_id.clone(),
                ResourceKind::Vm,
                Some(vm_id.clone()),
                &meta,
                Some(format!(
                    "source={}:dest={}{}",
                    request.source_node_id,
                    request.destination_node_id,
                    if pause_first { ":pause_first=true" } else { "" }
                )),
            )
            .await?;

        self.accept_operation(&operation_id).await?;

        Ok(Self::ok_ack(&operation_id, "migrate vm accepted"))
    }

    async fn update_overlay(
        &self,
        request: proto::UpdateOverlayRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;

        let (operation_id, _, _) = self
            .create_operation_and_emit(
                "UpdateOverlay",
                node_id.clone(),
                ResourceKind::Network,
                Some(ResourceId::new(request.network_id.clone()).map_err(|e| {
                    ControlPlaneServiceError::InvalidArgument(format!("invalid network_id: {}", e))
                })?),
                &meta,
                Some(format!("vni={}", request.vni)),
            )
            .await?;

        self.accept_operation(&operation_id).await?;

        Ok(Self::ok_ack(&operation_id, "update overlay accepted"))
    }

    async fn send_gratuitous_arp(
        &self,
        request: proto::SendGratuitousArpRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = Self::parse_node_id(request.node_id)?;

        let (operation_id, _, _) = self
            .create_operation_and_emit(
                "SendGratuitousArp",
                node_id.clone(),
                ResourceKind::Network,
                Some(ResourceId::new(request.network_id.clone()).map_err(|e| {
                    ControlPlaneServiceError::InvalidArgument(format!("invalid network_id: {}", e))
                })?),
                &meta,
                Some(format!("vm_ip={}", request.vm_ip)),
            )
            .await?;

        self.accept_operation(&operation_id).await?;

        Ok(Self::ok_ack(&operation_id, "send gratuitous arp accepted"))
    }

    /// Operator relay: forwards the terminal resolution of a
    /// restart-interrupted (`InspectRequired`) operation to the owning
    /// agent's core journal. The control plane stays a desired-state
    /// authority — it does not journal on the node's behalf; the agent
    /// validates the payload and terminal-persists (see the agent's
    /// `resolve_inspect_required_operation`). The target node is
    /// `meta.target_node_id` (the request has no separate node_id field:
    /// the operation already belongs to exactly one node).
    async fn resolve_inspect_required_operation(
        &self,
        request: proto::ResolveInspectRequiredOperationRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let egress = self.node_egress.as_ref().ok_or_else(|| {
            ControlPlaneServiceError::Unsupported(
                "resolve_inspect_required_operation relay is not configured on this control plane"
                    .into(),
            )
        })?;
        let meta = self.meta_from_request(request.meta.clone())?;
        let node_id = Self::parse_node_id(meta.target_node_id.clone())?;
        if request.operation_id.trim().is_empty() {
            return Err(ControlPlaneServiceError::InvalidArgument(
                "operation_id is required".into(),
            ));
        }
        if request.vm_id.trim().is_empty() {
            return Err(ControlPlaneServiceError::InvalidArgument(
                "vm_id is required".into(),
            ));
        }
        // Fail fast on the shapes the agent would reject, so the caller gets
        // a clear InvalidArgument here instead of an opaque relay error: the
        // note must be present and single-line (audit evidence), and the
        // audit identity must be attributable. The agent re-validates at the
        // journal boundary (authoritative, incl. the 8000-byte note bound).
        let note = request.note.trim();
        if note.is_empty() {
            return Err(ControlPlaneServiceError::InvalidArgument(
                "note is required (operator inspection evidence)".into(),
            ));
        }
        if note.chars().any(|c| c.is_control()) {
            return Err(ControlPlaneServiceError::InvalidArgument(
                "note must not contain control characters".into(),
            ));
        }
        // Mirrors the agent's journal-boundary bound verbatim so an
        // oversized note fails here as a clear InvalidArgument instead of
        // surfacing the agent's rejection as an opaque internal error.
        if note.len() > 8_000 {
            return Err(ControlPlaneServiceError::InvalidArgument(
                "resolution note must be at most 8000 bytes".into(),
            ));
        }
        let requested_by = meta.requested_by.trim();
        if requested_by.chars().any(|c| c.is_control()) {
            return Err(ControlPlaneServiceError::InvalidArgument(
                "requested_by must not contain control characters".into(),
            ));
        }
        let requested_by = if requested_by.is_empty() {
            "control-plane"
        } else {
            requested_by
        };
        match request.disposition.as_str() {
            "succeeded" | "failed" => {}
            other => {
                return Err(ControlPlaneServiceError::InvalidArgument(format!(
                    "disposition must be \"succeeded\" or \"failed\", got {other:?}"
                )))
            }
        }
        let socket_path =
            crate::migration::resolve_agent_socket(&egress.agent_socket_pattern, node_id.as_str())
                .map_err(|e| ControlPlaneServiceError::InvalidArgument(e.to_string()))?;
        let mut client = egress
            .pool
            .get_or_connect(node_id.as_str(), &socket_path)
            .await
            .map_err(|e| {
                ControlPlaneServiceError::NodeUnavailable(format!(
                    "cannot reach agent for node {node_id}: {e}"
                ))
            })?;
        client
            .resolve_inspect_required_operation(
                node_id.as_str(),
                &request.vm_id,
                &request.operation_id,
                &request.disposition,
                &request.note,
                Some(requested_by),
            )
            .await
            .map_err(|e| match e {
                ChvError::BackendUnavailable { backend, reason } => {
                    ControlPlaneServiceError::NodeUnavailable(format!(
                        "agent resolve_inspect_required_operation unavailable ({backend}): {reason}"
                    ))
                }
                other => ControlPlaneServiceError::Internal(format!(
                    "agent resolve_inspect_required_operation failed: {other}"
                )),
            })
    }
}
