use crate::error::ControlPlaneServiceError;
use async_trait::async_trait;
use chv_controlplane_store::{
    AlertCreateInput, AlertRepository, EventAppendInput, EventRepository,
    NetworkObservedStateInput, NodeObservedStateInput, NodeRepository, ObservedStateRepository,
    VmMetricsInput, VmObservedStateInput, VolumeObservedStateInput,
};
use chv_controlplane_types::domain::{
    EventSeverity, EventType, Generation, NodeId, NodeState, OperationId, ResourceId,
};
use chv_errors::ChvError;
use control_plane_node_api::control_plane_node_api as proto;
use std::str::FromStr;
use tracing::warn;

#[async_trait]
pub trait TelemetryService: Send + Sync {
    async fn report_node_state(
        &self,
        request: proto::NodeStateReport,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn report_vm_state(
        &self,
        request: proto::VmStateReport,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn report_volume_state(
        &self,
        request: proto::VolumeStateReport,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn report_network_state(
        &self,
        request: proto::NetworkStateReport,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn publish_event(
        &self,
        request: proto::PublishEventRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn publish_alert(
        &self,
        request: proto::PublishAlertRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;

    async fn report_migration_progress(
        &self,
        request: proto::MigrationProgress,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError>;
}

#[derive(Clone)]
pub struct TelemetryServiceImplementation {
    node_repo: NodeRepository,
    observed_state_repo: ObservedStateRepository,
    event_repo: EventRepository,
    alert_repo: AlertRepository,
}

impl TelemetryServiceImplementation {
    pub fn new(
        node_repo: NodeRepository,
        observed_state_repo: ObservedStateRepository,
        event_repo: EventRepository,
        alert_repo: AlertRepository,
    ) -> Self {
        Self {
            node_repo,
            observed_state_repo,
            event_repo,
            alert_repo,
        }
    }

    async fn ensure_reporting_node(
        &self,
        node_id: &NodeId,
        observed_unix_ms: i64,
    ) -> Result<(), ControlPlaneServiceError> {
        self.node_repo
            .ensure_node_record(node_id, None, None, observed_unix_ms)
            .await?;
        Ok(())
    }
}

use chv_controlplane_types::constants::{
    ALERT_STATUS_OPEN, STATUS_OK, SUMMARY_ALERT_PUBLISHED, SUMMARY_EVENT_PUBLISHED,
    SUMMARY_NETWORK_STATE_REPORTED, SUMMARY_NODE_STATE_REPORTED, SUMMARY_VM_STATE_REPORTED,
    SUMMARY_VOLUME_STATE_REPORTED,
};

#[async_trait]
impl TelemetryService for TelemetryServiceImplementation {
    async fn report_node_state(
        &self,
        request: proto::NodeStateReport,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let node_id = NodeId::new(request.node_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;

        let gen = if request.observed_generation.is_empty() {
            Generation::new(0)
        } else {
            Generation::from_str(&request.observed_generation).map_err(|_| {
                ControlPlaneServiceError::InvalidArgument("invalid observed_generation".into())
            })?
        };

        let state = NodeState::from_str(&request.state).map_err(|_| {
            ControlPlaneServiceError::InvalidArgument(format!(
                "invalid node state: {}",
                request.state
            ))
        })?;

        self.ensure_reporting_node(&node_id, request.reported_unix_ms)
            .await?;

        self.observed_state_repo
            .upsert_node(&NodeObservedStateInput {
                node_id,
                observed_generation: gen,
                observed_state: state,
                health_status: Some(request.health_status),
                runtime_status: None,
                state_reason: if request.last_error.is_empty() {
                    None
                } else {
                    Some(request.last_error)
                },
                entered_unix_ms: None,
                observed_unix_ms: request.reported_unix_ms,
            })
            .await?;

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: "".into(),
                status: STATUS_OK.into(),
                node_observed_generation: gen.to_string(),
                error_code: "".into(),
                human_summary: SUMMARY_NODE_STATE_REPORTED.into(),
            }),
        })
    }

    async fn report_vm_state(
        &self,
        request: proto::VmStateReport,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let node_id = NodeId::new(request.node_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;

        let vm_id = ResourceId::new(request.vm_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid vm_id: {}", e))
        })?;

        let gen = if request.observed_generation.is_empty() {
            Generation::new(0)
        } else {
            Generation::from_str(&request.observed_generation).map_err(|_| {
                ControlPlaneServiceError::InvalidArgument("invalid observed_generation".into())
            })?
        };

        self.ensure_reporting_node(&node_id, request.reported_unix_ms)
            .await?;

        self.observed_state_repo
            .upsert_vm(&VmObservedStateInput {
                node_id: Some(node_id),
                vm_id: vm_id.clone(),
                observed_generation: gen,
                runtime_status: request.runtime_status,
                health_status: Some(request.health_status),
                cloud_hypervisor_pid: None,
                api_socket_path: None,
                last_error: if request.last_error.is_empty() {
                    None
                } else {
                    Some(request.last_error)
                },
                last_transition_unix_ms: None,
                observed_unix_ms: request.reported_unix_ms,
            })
            .await?;

        // Store runtime counters if any are present (non-zero).
        if request.memory_bytes_total > 0
            || request.disk_bytes_read > 0
            || request.disk_bytes_written > 0
            || request.net_bytes_rx > 0
            || request.net_bytes_tx > 0
        {
            let _ = self
                .observed_state_repo
                .insert_vm_metrics(&VmMetricsInput {
                    vm_id: vm_id.clone(),
                    collected_unix_ms: request.reported_unix_ms,
                    cpu_percent: request.cpu_percent,
                    memory_bytes_used: request.memory_bytes_used,
                    memory_bytes_total: request.memory_bytes_total,
                    disk_bytes_read: request.disk_bytes_read,
                    disk_bytes_written: request.disk_bytes_written,
                    net_bytes_rx: request.net_bytes_rx,
                    net_bytes_tx: request.net_bytes_tx,
                })
                .await;
        }

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: "".into(),
                status: STATUS_OK.into(),
                node_observed_generation: gen.to_string(),
                error_code: "".into(),
                human_summary: SUMMARY_VM_STATE_REPORTED.into(),
            }),
        })
    }

    async fn report_volume_state(
        &self,
        request: proto::VolumeStateReport,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let _node_id = NodeId::new(request.node_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;

        let volume_id = ResourceId::new(request.volume_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid volume_id: {}", e))
        })?;

        let gen = if request.observed_generation.is_empty() {
            Generation::new(0)
        } else {
            Generation::from_str(&request.observed_generation).map_err(|_| {
                ControlPlaneServiceError::InvalidArgument("invalid observed_generation".into())
            })?
        };

        self.observed_state_repo
            .upsert_volume(&VolumeObservedStateInput {
                volume_id,
                observed_generation: gen,
                runtime_status: request.runtime_status,
                health_status: Some(request.health_status),
                attached_vm_id: None,
                device_path: None,
                export_path: None,
                last_transition_unix_ms: None,
                observed_unix_ms: request.reported_unix_ms,
            })
            .await?;

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: "".into(),
                status: STATUS_OK.into(),
                node_observed_generation: gen.to_string(),
                error_code: "".into(),
                human_summary: SUMMARY_VOLUME_STATE_REPORTED.into(),
            }),
        })
    }

    async fn report_network_state(
        &self,
        request: proto::NetworkStateReport,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let _node_id = NodeId::new(request.node_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;

        let network_id = ResourceId::new(request.network_id).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid network_id: {}", e))
        })?;

        let gen = if request.observed_generation.is_empty() {
            Generation::new(0)
        } else {
            Generation::from_str(&request.observed_generation).map_err(|_| {
                ControlPlaneServiceError::InvalidArgument("invalid observed_generation".into())
            })?
        };

        self.observed_state_repo
            .upsert_network(&NetworkObservedStateInput {
                network_id,
                observed_generation: gen,
                runtime_status: request.runtime_status,
                health_status: Some(request.health_status),
                exposure_status: None,
                applied_unix_ms: None,
                observed_unix_ms: request.reported_unix_ms,
            })
            .await?;

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: "".into(),
                status: STATUS_OK.into(),
                node_observed_generation: gen.to_string(),
                error_code: "".into(),
                human_summary: SUMMARY_NETWORK_STATE_REPORTED.into(),
            }),
        })
    }

    async fn publish_event(
        &self,
        request: proto::PublishEventRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let node_id = NodeId::new(request.node_id.clone()).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;

        let meta = request
            .meta
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing meta".into()))?;

        // Agent-generated events use synthetic operation_ids that don't exist in the
        // operations table. Skip the FK to avoid constraint failures.
        let op_id: Option<OperationId> = None;

        let severity = EventSeverity::from_str(&request.severity).map_err(|_| {
            ControlPlaneServiceError::InvalidArgument(format!(
                "invalid severity: {}",
                request.severity
            ))
        })?;

        let event_type = EventType::from_str(&request.event_type).map_err(|_| {
            ControlPlaneServiceError::InvalidArgument(format!(
                "invalid event_type: {}",
                request.event_type
            ))
        })?;

        self.ensure_reporting_node(&node_id, meta.request_unix_ms)
            .await?;

        if let Err(e) = self
            .event_repo
            .append(&EventAppendInput {
                operation_id: op_id,
                node_id: Some(node_id),
                resource_kind: None,
                resource_id: None,
                severity,
                event_type,
                message: request.summary,
                details: if request.details_json.is_empty() {
                    None
                } else {
                    Some(String::from_utf8(request.details_json).map_err(|_| {
                        ControlPlaneServiceError::InvalidArgument(
                            "details_json is not valid UTF-8".into(),
                        )
                    })?)
                },
                occurred_unix_ms: meta.request_unix_ms,
                actor_id: None,
                requested_by: Some(meta.requested_by),
                correlation_id: None,
            })
            .await
        {
            warn!(error = %e, node_id = %request.node_id, "failed to persist event, skipping");
        }

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id,
                status: STATUS_OK.into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: SUMMARY_EVENT_PUBLISHED.into(),
            }),
        })
    }

    async fn publish_alert(
        &self,
        request: proto::PublishAlertRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        let node_id = NodeId::new(request.node_id.clone()).map_err(|e| {
            ControlPlaneServiceError::InvalidArgument(format!("invalid node_id: {}", e))
        })?;

        let meta = request
            .meta
            .ok_or_else(|| ControlPlaneServiceError::InvalidArgument("missing meta".into()))?;

        let operation_id = meta.operation_id.trim();
        if operation_id.is_empty() {
            return Err(ControlPlaneServiceError::InvalidArgument(
                "operation_id cannot be empty".into(),
            ));
        }
        let op_id = OperationId::new(operation_id.to_string()).ok();

        let severity = EventSeverity::from_str(&request.severity).map_err(|_| {
            ControlPlaneServiceError::InvalidArgument(format!(
                "invalid severity: {}",
                request.severity
            ))
        })?;

        self.ensure_reporting_node(&node_id, meta.request_unix_ms)
            .await?;

        // CREATE PERSISTENT ALERT
        if let Err(e) = self
            .alert_repo
            .create(&AlertCreateInput {
                alert_type: request.alert_type.clone(),
                severity,
                resource_kind: None, // Could infer from alert_type if standard
                resource_id: None,
                node_id: Some(node_id.clone()),
                status: ALERT_STATUS_OPEN.into(),
                message: request.summary.clone(),
                operation_id: Some(operation_id.to_string()),
                opened_unix_ms: meta.request_unix_ms,
            })
            .await
        {
            warn!(error = %e, node_id = %request.node_id, "failed to persist alert, skipping");
        }

        // Also emit event for the alert
        if let Err(e) = self
            .event_repo
            .append(&EventAppendInput {
                operation_id: op_id,
                node_id: Some(node_id),
                resource_kind: None,
                resource_id: None,
                severity,
                event_type: EventType::Health,
                message: format!("[ALERT] {}", request.summary),
                details: if request.details_json.is_empty() {
                    None
                } else {
                    Some(
                        String::from_utf8(request.details_json.clone()).map_err(|_| {
                            ControlPlaneServiceError::InvalidArgument(
                                "details_json is not valid UTF-8".into(),
                            )
                        })?,
                    )
                },
                occurred_unix_ms: meta.request_unix_ms,
                actor_id: None,
                requested_by: Some(meta.requested_by),
                correlation_id: None,
            })
            .await
        {
            warn!(error = %e, node_id = %request.node_id, "failed to persist alert event, skipping");
        }

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: meta.operation_id,
                status: STATUS_OK.into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: SUMMARY_ALERT_PUBLISHED.into(),
            }),
        })
    }

    async fn report_migration_progress(
        &self,
        request: proto::MigrationProgress,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        // Update the migrations table with progress from the agent
        let pool = self.observed_state_repo.pool();

        match crate::migration::update_migration_progress(
            pool,
            &request.node_id,
            &request.vm_id,
            &request.operation_id,
            request.phase,
            request.bytes_transferred,
            request.total_bytes,
            request.convergence_round,
            request.dirty_blocks_remaining,
        )
        .await
        {
            Ok(()) => {}
            Err(ChvError::AccessDenied { resource, reason }) => {
                warn!(
                    vm_id = %request.vm_id,
                    operation_id = %request.operation_id,
                    node_id = %request.node_id,
                    %reason,
                    "rejected migration progress from a node that does not own the migration"
                );
                return Err(ControlPlaneServiceError::PermissionDenied(format!(
                    "access denied: {resource} — {reason}"
                )));
            }
            Err(e) => {
                // Do not acknowledge a progress update that was not persisted:
                // an ACK would tell the agent the row is fresh, the reaper
                // would see a false stall, and the report would be lost. The
                // agent retries on error.
                warn!(
                    vm_id = %request.vm_id,
                    operation_id = %request.operation_id,
                    error = %e,
                    "failed to update migration progress"
                );
                return Err(ControlPlaneServiceError::Internal(format!(
                    "failed to persist migration progress: {e}"
                )));
            }
        }

        Ok(proto::AckResponse {
            result: Some(proto::ResultMeta {
                operation_id: request.operation_id,
                status: STATUS_OK.into(),
                node_observed_generation: "".into(),
                error_code: "".into(),
                human_summary: "migration progress updated".into(),
            }),
        })
    }
}
