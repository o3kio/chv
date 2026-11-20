use crate::audit_coupling::AdoptedAudit;
use crate::error::ControlPlaneServiceError;
use async_trait::async_trait;
use chv_controlplane_store::{
    DesiredStateRepository, EventAppendInput, EventRepository, NetworkDesiredStateInput,
    NetworkExposureInput, NodeRepository, NodeStatePatchInput, ObservedStateRepository,
    OperationRepository, OperationStatusUpdateInput, VmDesiredStateInput, VolumeDesiredStateInput,
};
use chv_controlplane_types::constants::STATUS_OK;
use chv_controlplane_types::domain::{
    EventSeverity, EventType, Generation, NodeId, NodeState, OperationId, OperationStatus,
    ResourceId, ResourceKind,
};
use chv_controlplane_types::fragment::{NetworkSpec, NodeSpec, VmSpec, VolumeSpec};
use control_plane_node_api::control_plane_node_api as proto;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

#[async_trait]
pub trait ReconcileService: Send + Sync {
    /// Persist a node desired-state fragment.
    ///
    /// `audit` is the request's pre-minted audit-outbox row (#706). When
    /// present, the implementation enqueues it inside the desired-state
    /// write's transaction, so the evidence commits with the fragment
    /// or not at all. The convergence event appended afterwards is a
    /// separate commit (see the residual window documented in
    /// `docs/specs/component/audit-log.md`).
    async fn apply_node_desired_state(
        &self,
        request: proto::ApplyNodeDesiredStateRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    /// `audit` follows [`Self::apply_node_desired_state`].
    async fn apply_vm_desired_state(
        &self,
        request: proto::ApplyVmDesiredStateRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    /// `audit` follows [`Self::apply_node_desired_state`].
    async fn apply_volume_desired_state(
        &self,
        request: proto::ApplyVolumeDesiredStateRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    /// `audit` follows [`Self::apply_node_desired_state`].
    async fn apply_network_desired_state(
        &self,
        request: proto::ApplyNetworkDesiredStateRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    /// Acknowledge an observed desired-state generation.
    ///
    /// `audit` follows [`Self::apply_node_desired_state`]: the row is
    /// enqueued with the observed-state acknowledgement. The operation
    /// status update and convergence event that follow are separate
    /// commits.
    async fn acknowledge_desired_state_version(
        &self,
        request: proto::AcknowledgeDesiredStateVersionRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;
}

#[derive(Clone)]
pub struct ReconcileServiceImplementation {
    node_repo: NodeRepository,
    desired_state_repo: DesiredStateRepository,
    event_repo: EventRepository,
    observed_state_repo: ObservedStateRepository,
    operation_repo: OperationRepository,
}

struct EventContext {
    operation_id: Option<OperationId>,
    node_id: Option<NodeId>,
    resource_kind: Option<ResourceKind>,
    resource_id: Option<ResourceId>,
    severity: EventSeverity,
    event_type: EventType,
    message: String,
    details: Option<String>,
    occurred_unix_ms: i64,
    requested_by: Option<String>,
}

impl EventContext {
    fn into_input(self) -> EventAppendInput {
        EventAppendInput {
            operation_id: self.operation_id,
            node_id: self.node_id,
            resource_kind: self.resource_kind,
            resource_id: self.resource_id,
            severity: self.severity,
            event_type: self.event_type,
            message: self.message,
            details: self.details,
            occurred_unix_ms: self.occurred_unix_ms,
            actor_id: None,
            requested_by: self.requested_by,
            correlation_id: None,
        }
    }
}

impl ReconcileServiceImplementation {
    pub fn new(
        node_repo: NodeRepository,
        desired_state_repo: DesiredStateRepository,
        event_repo: EventRepository,
        observed_state_repo: ObservedStateRepository,
        operation_repo: OperationRepository,
    ) -> Self {
        Self {
            node_repo,
            desired_state_repo,
            event_repo,
            observed_state_repo,
            operation_repo,
        }
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

    fn parse_operation_id(
        meta: &proto::RequestMeta,
    ) -> Result<Option<OperationId>, ControlPlaneServiceError> {
        if meta.operation_id.is_empty() {
            Ok(None)
        } else {
            Ok(Some(OperationId::new(meta.operation_id.clone()).map_err(
                |e| {
                    ControlPlaneServiceError::InvalidArgument(format!(
                        "invalid operation_id: {}",
                        e
                    ))
                },
            )?))
        }
    }

    fn normalize_requested_by(meta: &proto::RequestMeta) -> Option<String> {
        if meta.requested_by.is_empty() {
            None
        } else {
            Some(meta.requested_by.clone())
        }
    }

    fn parse_apply_status(apply_status: &str) -> Result<OperationStatus, ControlPlaneServiceError> {
        match apply_status.trim().to_ascii_lowercase().as_str() {
            "" | "ok" => Ok(OperationStatus::Succeeded),
            "stale" => Ok(OperationStatus::Stale),
            "conflict" => Ok(OperationStatus::Conflict),
            "rejected" => Ok(OperationStatus::Rejected),
            "failed" => Ok(OperationStatus::Failed),
            _ => Err(ControlPlaneServiceError::InvalidArgument(format!(
                "invalid apply_status: {}",
                apply_status
            ))),
        }
    }

    fn validate_fragment(
        meta: &proto::RequestMeta,
        fragment: &proto::DesiredStateFragment,
        node_id: &NodeId,
        expected_resource_id: &ResourceId,
        expected_kind: ResourceKind,
    ) -> Result<(), ControlPlaneServiceError> {
        if meta.target_node_id != node_id.as_str() {
            return Err(ControlPlaneServiceError::InvalidArgument(format!(
                "target_node_id mismatch: expected {}",
                node_id.as_str()
            )));
        }
        if fragment.id != expected_resource_id.as_str() {
            return Err(ControlPlaneServiceError::InvalidArgument(format!(
                "fragment.id mismatch: expected {}",
                expected_resource_id.as_str()
            )));
        }
        let fragment_kind = ResourceKind::from_str(&fragment.kind).map_err(|_| {
            ControlPlaneServiceError::InvalidArgument(format!(
                "invalid fragment.kind: {}",
                fragment.kind
            ))
        })?;
        if fragment_kind != expected_kind {
            return Err(ControlPlaneServiceError::InvalidArgument(format!(
                "fragment.kind mismatch: expected {}",
                expected_kind.as_str()
            )));
        }
        Ok(())
    }

    async fn emit_event(&self, ctx: EventContext) -> Result<(), ControlPlaneServiceError> {
        self.event_repo.append(&ctx.into_input()).await?;
        Ok(())
    }
}

#[async_trait]
impl ReconcileService for ReconcileServiceImplementation {
    async fn apply_node_desired_state(
        &self,
        request: proto::ApplyNodeDesiredStateRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = NodeId::new(request.node_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;
        let resource_id = ResourceId::new(node_id.as_str()).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;

        let fragment = request
            .fragment
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing fragment".into()))?;

        Self::validate_fragment(&meta, &fragment, &node_id, &resource_id, ResourceKind::Node)?;

        let generation = Generation::from_str(&fragment.generation).map_err(|_| {
            ControlPlaneServiceError::InvalidArgument("generation must be numeric".into())
        })?;

        let spec: NodeSpec = serde_json::from_slice(&fragment.spec_json).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid spec_json: {}", e))
        })?;

        let desired_state = NodeState::from_str(&spec.desired_state).map_err(|_| {
            ControlPlaneServiceError::InvalidArgument(format!(
                "invalid node state: {}",
                spec.desired_state
            ))
        })?;

        let now = Self::now_ms();

        if let Err(e) = self
            .node_repo
            .set_state_preserving_policy(
                &NodeStatePatchInput {
                    node_id: node_id.clone(),
                    desired_state,
                    desired_generation: generation,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: if fragment.updated_by.is_empty() {
                        None
                    } else {
                        Some(fragment.updated_by)
                    },
                    state_reason: spec.state_reason,
                    requested_unix_ms: now,
                },
                audit.map(AdoptedAudit::insert),
            )
            .await
        {
            let op_id = Self::parse_operation_id(&meta)?;
            self.emit_event(EventContext {
                operation_id: op_id,
                node_id: Some(node_id.clone()),
                resource_kind: Some(ResourceKind::Node),
                resource_id: Some(ResourceId::new(node_id.as_str()).unwrap()),
                severity: EventSeverity::Warning,
                event_type: EventType::DesiredStateRejected,
                message: "failed to apply node desired state".into(),
                details: None,
                occurred_unix_ms: now,
                requested_by: Self::normalize_requested_by(&meta),
            })
            .await?;
            tracing::warn!(
                resource_kind = "node",
                resource_id = %node_id.as_str(),
                error = %e,
                "desired state application failed — divergence detected"
            );
            return Err(ControlPlaneServiceError::Internal(format!(
                "persistence error: {e}"
            )));
        }
        // The desired-state write (and, when adopted, its audit row)
        // committed. Everything after this point — the convergence event
        // append — is a separate commit.
        if let Some(audit) = audit {
            audit.mark_committed();
        }

        let op_id = Self::parse_operation_id(&meta)?;
        self.emit_event(EventContext {
            operation_id: op_id,
            node_id: Some(node_id.clone()),
            resource_kind: Some(ResourceKind::Node),
            resource_id: Some(ResourceId::new(node_id.as_str()).unwrap()),
            severity: EventSeverity::Info,
            event_type: EventType::DesiredStateApplied,
            message: "node desired state applied".into(),
            details: None,
            occurred_unix_ms: now,
            requested_by: Self::normalize_requested_by(&meta),
        })
        .await?;

        tracing::info!(
            resource_kind = "node",
            resource_id = %node_id.as_str(),
            generation = %generation,
            "desired state applied — awaiting convergence"
        );

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id,
                status: STATUS_OK.into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: "node desired state applied".into(),
            }),
        })
    }

    async fn apply_vm_desired_state(
        &self,
        request: proto::ApplyVmDesiredStateRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = NodeId::new(request.node_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;
        let vm_id = ResourceId::new(request.vm_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid vm_id: {}", e))
        })?;

        let fragment = request
            .fragment
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing fragment".into()))?;

        Self::validate_fragment(&meta, &fragment, &node_id, &vm_id, ResourceKind::Vm)?;

        let generation = Generation::from_str(&fragment.generation).map_err(|_| {
            ControlPlaneServiceError::InvalidArgument("generation must be numeric".into())
        })?;

        let spec: VmSpec = serde_json::from_slice(&fragment.spec_json).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid spec_json: {}", e))
        })?;

        let now = Self::now_ms();

        if let Err(e) = self
            .desired_state_repo
            .upsert_vm(
                &VmDesiredStateInput {
                    vm_id: vm_id.clone(),
                    node_id: Some(node_id.clone()),
                    display_name: fragment.id.clone(),
                    tenant_id: None,
                    placement_policy: None,
                    desired_generation: generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: if fragment.updated_by.is_empty() {
                        None
                    } else {
                        Some(fragment.updated_by)
                    },
                    target_node_id: Some(node_id.clone()),
                    cpu_count: spec.cpu_count,
                    memory_bytes: spec.memory_bytes,
                    image_ref: spec.image_ref,
                    boot_mode: spec.boot_mode,
                    desired_power_state: spec.desired_power_state,
                    vsock_enabled: None,
                    requested_unix_ms: now,
                },
                audit.map(AdoptedAudit::insert),
            )
            .await
        {
            let op_id = Self::parse_operation_id(&meta)?;
            self.emit_event(EventContext {
                operation_id: op_id,
                node_id: Some(node_id.clone()),
                resource_kind: Some(ResourceKind::Vm),
                resource_id: Some(vm_id.clone()),
                severity: EventSeverity::Warning,
                event_type: EventType::DesiredStateRejected,
                message: "failed to apply vm desired state".into(),
                details: None,
                occurred_unix_ms: now,
                requested_by: Self::normalize_requested_by(&meta),
            })
            .await?;
            tracing::warn!(
                resource_kind = "vm",
                resource_id = %vm_id.as_str(),
                error = %e,
                "desired state application failed — divergence detected"
            );
            return Err(ControlPlaneServiceError::Internal(format!(
                "persistence error: {e}"
            )));
        }
        if let Some(audit) = audit {
            audit.mark_committed();
        }

        let op_id = Self::parse_operation_id(&meta)?;
        self.emit_event(EventContext {
            operation_id: op_id,
            node_id: Some(node_id.clone()),
            resource_kind: Some(ResourceKind::Vm),
            resource_id: Some(vm_id.clone()),
            severity: EventSeverity::Info,
            event_type: EventType::DesiredStateApplied,
            message: "vm desired state applied".into(),
            details: None,
            occurred_unix_ms: now,
            requested_by: Self::normalize_requested_by(&meta),
        })
        .await?;

        tracing::info!(
            resource_kind = "vm",
            resource_id = %vm_id.as_str(),
            generation = %generation,
            "desired state applied — awaiting convergence"
        );

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id,
                status: STATUS_OK.into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: "vm desired state applied".into(),
            }),
        })
    }

    async fn apply_volume_desired_state(
        &self,
        request: proto::ApplyVolumeDesiredStateRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = NodeId::new(request.node_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;
        let volume_id = ResourceId::new(request.volume_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid volume_id: {}", e))
        })?;

        let fragment = request
            .fragment
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing fragment".into()))?;

        Self::validate_fragment(&meta, &fragment, &node_id, &volume_id, ResourceKind::Volume)?;

        let generation = Generation::from_str(&fragment.generation).map_err(|_| {
            ControlPlaneServiceError::InvalidArgument("generation must be numeric".into())
        })?;

        let spec: VolumeSpec = serde_json::from_slice(&fragment.spec_json).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid spec_json: {}", e))
        })?;

        let attached_vm_id = spec
            .attached_vm_id
            .as_ref()
            .map(|id| ResourceId::new(id.clone()))
            .transpose()
            .map_err(|e| {
                ControlPlaneServiceError::InvalidArgument(format!("invalid attached_vm_id: {}", e))
            })?;

        let now = Self::now_ms();

        let clone_source_volume_id = spec
            .clone_source_volume_id
            .as_ref()
            .map(|id| ResourceId::new(id.clone()))
            .transpose()
            .map_err(|e| {
                ControlPlaneServiceError::InvalidArgument(format!(
                    "invalid clone_source_volume_id: {}",
                    e
                ))
            })?;

        if let Err(e) = self
            .desired_state_repo
            .upsert_volume(
                &VolumeDesiredStateInput {
                    volume_id: volume_id.clone(),
                    node_id: Some(node_id.clone()),
                    display_name: fragment.id.clone(),
                    capacity_bytes: spec.capacity_bytes,
                    volume_kind: spec.volume_kind,
                    storage_class: spec.storage_class,
                    // Fragments do not carry ownership; NULL here preserves the
                    // owner a BFF creation or clone set (COALESCE in the upsert).
                    owner_id: None,
                    desired_generation: generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: if fragment.updated_by.is_empty() {
                        None
                    } else {
                        Some(fragment.updated_by)
                    },
                    attached_vm_id,
                    attachment_mode: spec.attachment_mode,
                    device_name: spec.device_name,
                    read_only: spec.read_only,
                    resize_to_bytes: None,
                    snapshot_op: spec.snapshot_op,
                    snapshot_name: spec.snapshot_name,
                    clone_source_volume_id,
                    requested_unix_ms: now,
                },
                audit.map(AdoptedAudit::insert),
            )
            .await
        {
            let op_id = Self::parse_operation_id(&meta)?;
            self.emit_event(EventContext {
                operation_id: op_id,
                node_id: Some(node_id.clone()),
                resource_kind: Some(ResourceKind::Volume),
                resource_id: Some(volume_id.clone()),
                severity: EventSeverity::Warning,
                event_type: EventType::DesiredStateRejected,
                message: "failed to apply volume desired state".into(),
                details: None,
                occurred_unix_ms: now,
                requested_by: Self::normalize_requested_by(&meta),
            })
            .await?;
            tracing::warn!(
                resource_kind = "volume",
                resource_id = %volume_id.as_str(),
                error = %e,
                "desired state application failed — divergence detected"
            );
            return Err(ControlPlaneServiceError::Internal(format!(
                "persistence error: {e}"
            )));
        }
        if let Some(audit) = audit {
            audit.mark_committed();
        }

        let op_id = Self::parse_operation_id(&meta)?;
        self.emit_event(EventContext {
            operation_id: op_id,
            node_id: Some(node_id.clone()),
            resource_kind: Some(ResourceKind::Volume),
            resource_id: Some(volume_id.clone()),
            severity: EventSeverity::Info,
            event_type: EventType::DesiredStateApplied,
            message: "volume desired state applied".into(),
            details: None,
            occurred_unix_ms: now,
            requested_by: Self::normalize_requested_by(&meta),
        })
        .await?;

        tracing::info!(
            resource_kind = "volume",
            resource_id = %volume_id.as_str(),
            generation = %generation,
            "desired state applied — awaiting convergence"
        );

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id,
                status: STATUS_OK.into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: "volume desired state applied".into(),
            }),
        })
    }

    async fn apply_network_desired_state(
        &self,
        request: proto::ApplyNetworkDesiredStateRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = NodeId::new(request.node_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;
        let network_id = ResourceId::new(request.network_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid network_id: {}", e))
        })?;

        let fragment = request
            .fragment
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing fragment".into()))?;

        Self::validate_fragment(
            &meta,
            &fragment,
            &node_id,
            &network_id,
            ResourceKind::Network,
        )?;

        let generation = Generation::from_str(&fragment.generation).map_err(|_| {
            ControlPlaneServiceError::InvalidArgument("generation must be numeric".into())
        })?;

        let spec: NetworkSpec = serde_json::from_slice(&fragment.spec_json).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid spec_json: {}", e))
        })?;

        let now = Self::now_ms();

        let exposures: Vec<NetworkExposureInput> = spec
            .exposures
            .unwrap_or_default()
            .into_iter()
            .map(|exposure| NetworkExposureInput {
                network_id: network_id.clone(),
                service_name: exposure.service_name,
                protocol: exposure.protocol,
                listen_address: exposure.listen_address,
                listen_port: exposure.listen_port,
                target_address: exposure.target_address,
                target_port: exposure.target_port,
                exposure_policy: exposure.exposure_policy,
                updated_unix_ms: now,
            })
            .collect();

        let firewall_rules_json = spec
            .firewall_rules
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| {
                ControlPlaneServiceError::Internal(format!(
                    "failed to serialize firewall_rules: {e}"
                ))
            })?;
        let nat_rules_json = spec
            .nat_rules
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| {
                ControlPlaneServiceError::Internal(format!("failed to serialize nat_rules: {e}"))
            })?;
        let dhcp_scope_json = spec
            .dhcp_scope
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| {
                ControlPlaneServiceError::Internal(format!("failed to serialize dhcp_scope: {e}"))
            })?;
        let dns_scope_json = spec
            .dns_scope
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| {
                ControlPlaneServiceError::Internal(format!("failed to serialize dns_scope: {e}"))
            })?;

        if let Err(e) = self
            .desired_state_repo
            .upsert_network_with_exposures(
                &NetworkDesiredStateInput {
                    network_id: network_id.clone(),
                    node_id: Some(node_id.clone()),
                    display_name: fragment.id.clone(),
                    network_class: spec.network_class,
                    desired_generation: generation,
                    desired_status: None,
                    requested_by: Self::normalize_requested_by(&meta),
                    updated_by: if fragment.updated_by.is_empty() {
                        None
                    } else {
                        Some(fragment.updated_by)
                    },
                    firewall_rules_json,
                    nat_rules_json,
                    dhcp_scope_json,
                    dns_enabled: spec.dns_enabled,
                    dns_scope_json,
                    requested_unix_ms: now,
                },
                &exposures,
                audit.map(AdoptedAudit::insert),
            )
            .await
        {
            let op_id = Self::parse_operation_id(&meta)?;
            self.emit_event(EventContext {
                operation_id: op_id,
                node_id: Some(node_id.clone()),
                resource_kind: Some(ResourceKind::Network),
                resource_id: Some(network_id.clone()),
                severity: EventSeverity::Warning,
                event_type: EventType::DesiredStateRejected,
                message: "failed to apply network desired state".into(),
                details: None,
                occurred_unix_ms: now,
                requested_by: Self::normalize_requested_by(&meta),
            })
            .await?;
            tracing::warn!(
                resource_kind = "network",
                resource_id = %network_id.as_str(),
                error = %e,
                "desired state application failed — divergence detected"
            );
            return Err(ControlPlaneServiceError::Internal(format!(
                "persistence error: {e}"
            )));
        }
        if let Some(audit) = audit {
            audit.mark_committed();
        }

        let op_id = Self::parse_operation_id(&meta)?;
        self.emit_event(EventContext {
            operation_id: op_id,
            node_id: Some(node_id.clone()),
            resource_kind: Some(ResourceKind::Network),
            resource_id: Some(network_id.clone()),
            severity: EventSeverity::Info,
            event_type: EventType::DesiredStateApplied,
            message: "network desired state applied".into(),
            details: None,
            occurred_unix_ms: now,
            requested_by: Self::normalize_requested_by(&meta),
        })
        .await?;

        tracing::info!(
            resource_kind = "network",
            resource_id = %network_id.as_str(),
            generation = %generation,
            "desired state applied — awaiting convergence"
        );

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id,
                status: STATUS_OK.into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: "network desired state applied".into(),
            }),
        })
    }

    async fn acknowledge_desired_state_version(
        &self,
        request: proto::AcknowledgeDesiredStateVersionRequest,
        audit: Option<&AdoptedAudit<'_>>,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let meta = self.meta_from_request(request.meta)?;
        let node_id = NodeId::new(request.node_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;
        let resource_id = ResourceId::new(request.fragment_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid fragment_id: {}", e))
        })?;
        let resource_kind = ResourceKind::from_str(&request.fragment_kind).map_err(|_| {
            ControlPlaneServiceError::InvalidArgument(format!(
                "invalid fragment_kind: {}",
                request.fragment_kind
            ))
        })?;

        let observed_generation =
            Generation::from_str(&request.observed_generation).map_err(|_| {
                ControlPlaneServiceError::InvalidArgument(
                    "observed_generation must be numeric".into(),
                )
            })?;

        let parsed_status = Self::parse_apply_status(&request.apply_status)?;

        let now = Self::now_ms();
        let op_id = Self::parse_operation_id(&meta)?;

        match resource_kind {
            ResourceKind::Node => {
                self.observed_state_repo
                    .acknowledge_node_generation(
                        &node_id,
                        observed_generation,
                        now,
                        audit.map(AdoptedAudit::insert),
                    )
                    .await?;
            }
            ResourceKind::Vm => {
                self.observed_state_repo
                    .acknowledge_vm_generation(
                        &resource_id,
                        observed_generation,
                        now,
                        audit.map(AdoptedAudit::insert),
                    )
                    .await?;
            }
            ResourceKind::Volume => {
                self.observed_state_repo
                    .acknowledge_volume_generation(
                        &resource_id,
                        observed_generation,
                        now,
                        audit.map(AdoptedAudit::insert),
                    )
                    .await?;
            }
            ResourceKind::Network => {
                self.observed_state_repo
                    .acknowledge_network_generation(
                        &resource_id,
                        observed_generation,
                        now,
                        audit.map(AdoptedAudit::insert),
                    )
                    .await?;
            }
        }
        // The acknowledgement (and, when adopted, its audit row)
        // committed. The operation-status update and the convergence
        // event below are separate commits.
        if let Some(audit) = audit {
            audit.mark_committed();
        }

        if let Some(ref operation_id) = op_id {
            self.operation_repo
                .update_status(&OperationStatusUpdateInput {
                    operation_id: operation_id.clone(),
                    status: parsed_status,
                    observed_generation: Some(observed_generation),
                    updated_unix_ms: now,
                    error_code: None,
                    error_message: None,
                    updated_by: None,
                })
                .await?;
        }

        self.emit_event(EventContext {
            operation_id: op_id,
            node_id: Some(node_id.clone()),
            resource_kind: Some(resource_kind),
            resource_id: Some(resource_id),
            severity: EventSeverity::Info,
            event_type: EventType::DesiredStateAcknowledged,
            message: format!(
                "desired state version {} acknowledged",
                request.observed_generation
            ),
            details: None,
            occurred_unix_ms: now,
            requested_by: Self::normalize_requested_by(&meta),
        })
        .await?;

        tracing::info!(
            node_id = %node_id.as_str(),
            generation = %observed_generation,
            "convergence confirmed — node acknowledged desired state version"
        );

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id,
                status: STATUS_OK.into(),
                node_observed_generation: request.observed_generation,
                error_code: "".into(),
                human_summary: "desired state version acknowledged".into(),
            }),
        })
    }
}
