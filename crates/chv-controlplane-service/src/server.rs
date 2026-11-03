use crate::enrollment::EnrollmentService;
use crate::inventory::InventoryService;
use crate::lifecycle::LifecycleService;
use crate::peer_identity::verify_peer_matches;
use crate::reconcile::ReconcileService;
use crate::telemetry::TelemetryService;
use chv_controlplane_types::domain::NodeId;
use control_plane_node_api::control_plane_node_api as proto;
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::Instrument;

fn extract_op_id<T>(request: &Request<T>) -> Option<String> {
    request
        .metadata()
        .get(chv_common::OPERATION_ID_METADATA_KEY)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Pin the wire-asserted `node_id` to the peer's mTLS-derived identity.
///
/// Used by every agent-side handler that accepts a `node_id` from an
/// authenticated agent. Returns `permission_denied` if the peer's certificate
/// CN/SAN does not match `requested_node_id`. Closes C1/H1: a compromised
/// node holding a valid leaf cert can no longer impersonate another node by
/// sending its `node_id` on the wire.
///
/// `enroll_node` is the one path that intentionally skips this — at enrollment
/// time the agent has no leaf cert yet and is authenticated solely by
/// bootstrap token.
#[allow(clippy::result_large_err)] // tonic::Status is the lingua franca of every gRPC handler in this file.
fn enforce_peer_node_id<T>(
    request: &Request<T>,
    requested_node_id: &str,
    method: &'static str,
) -> Result<(), Status> {
    let node_id = NodeId::new(requested_node_id.to_string())
        .map_err(|e| Status::invalid_argument(format!("invalid node_id: {}", e)))?;
    verify_peer_matches(request.extensions(), &node_id, method).map_err(Status::from)
}

pub struct EnrollmentServer {
    service: Arc<dyn EnrollmentService>,
}

impl EnrollmentServer {
    pub fn new(service: Arc<dyn EnrollmentService>) -> Self {
        Self { service }
    }
}

#[tonic::async_trait]
impl proto::enrollment_service_server::EnrollmentService for EnrollmentServer {
    async fn enroll_node(
        &self,
        request: Request<proto::EnrollmentRequest>,
    ) -> Result<Response<proto::EnrollmentResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        let _span = tracing::info_span!("enroll_node", %op_id);
        // Transport-level peer address: the enrollment path derives the
        // node's fabric underlay endpoint from it (see
        // EnrollmentService::enroll_node). Must be read before
        // `into_inner()` consumes the request.
        let peer_addr = request.remote_addr();
        let resp = self
            .service
            .enroll_node(request.into_inner(), peer_addr)
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn rotate_node_certificate(
        &self,
        request: Request<proto::RotateNodeCertificateRequest>,
    ) -> Result<Response<proto::RotateNodeCertificateResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "rotate_node_certificate",
        )?;
        let _span = tracing::info_span!("rotate_node_certificate", %op_id);
        let resp = self
            .service
            .rotate_node_certificate(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn report_bootstrap_result(
        &self,
        request: Request<proto::ReportBootstrapResultRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "report_bootstrap_result",
        )?;
        let _span = tracing::info_span!("report_bootstrap_result", %op_id);
        let resp = self
            .service
            .report_bootstrap_result(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }
}

pub struct InventoryServer {
    service: Arc<dyn InventoryService>,
}

impl InventoryServer {
    pub fn new(service: Arc<dyn InventoryService>) -> Self {
        Self { service }
    }
}

#[tonic::async_trait]
impl proto::inventory_service_server::InventoryService for InventoryServer {
    async fn report_node_inventory(
        &self,
        request: Request<proto::ReportNodeInventoryRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        let asserted = request
            .get_ref()
            .inventory
            .as_ref()
            .map(|i| i.node_id.as_str())
            .unwrap_or_default();
        enforce_peer_node_id(&request, asserted, "report_node_inventory")?;
        let _span = tracing::info_span!("report_node_inventory", %op_id);
        // Transport-level peer address: used to derive the node's fabric
        // underlay endpoint (see InventoryService::report_node_inventory).
        // Must be read before `into_inner()` consumes the request.
        let peer_addr = request.remote_addr();
        let resp = self
            .service
            .report_node_inventory(request.into_inner(), peer_addr)
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_node_inventory failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn report_service_versions(
        &self,
        request: Request<proto::ReportServiceVersionsRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        let asserted = request
            .get_ref()
            .versions
            .as_ref()
            .map(|v| v.node_id.as_str())
            .unwrap_or_default();
        enforce_peer_node_id(&request, asserted, "report_service_versions")?;
        let _span = tracing::info_span!("report_service_versions", %op_id);
        let resp = self
            .service
            .report_service_versions(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_service_versions failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }
}

pub struct TelemetryServer {
    service: Arc<dyn TelemetryService>,
}

impl TelemetryServer {
    pub fn new(service: Arc<dyn TelemetryService>) -> Self {
        Self { service }
    }
}

#[tonic::async_trait]
impl proto::telemetry_service_server::TelemetryService for TelemetryServer {
    async fn report_node_state(
        &self,
        request: Request<proto::NodeStateReport>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "report_node_state")?;
        let _span = tracing::info_span!("report_node_state", %op_id);
        let resp = self
            .service
            .report_node_state(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_node_state failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn report_vm_state(
        &self,
        request: Request<proto::VmStateReport>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "report_vm_state")?;
        let _span = tracing::info_span!("report_vm_state", %op_id);
        let resp = self
            .service
            .report_vm_state(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_vm_state failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn report_volume_state(
        &self,
        request: Request<proto::VolumeStateReport>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "report_volume_state")?;
        let _span = tracing::info_span!("report_volume_state", %op_id);
        let resp = self
            .service
            .report_volume_state(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_volume_state failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn report_network_state(
        &self,
        request: Request<proto::NetworkStateReport>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "report_network_state")?;
        let _span = tracing::info_span!("report_network_state", %op_id);
        let resp = self
            .service
            .report_network_state(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_network_state failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn publish_event(
        &self,
        request: Request<proto::PublishEventRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "publish_event")?;
        let _span = tracing::info_span!("publish_event", %op_id);
        let resp = self
            .service
            .publish_event(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "publish_event failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn publish_alert(
        &self,
        request: Request<proto::PublishAlertRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "publish_alert")?;
        let _span = tracing::info_span!("publish_alert", %op_id);
        let resp = self
            .service
            .publish_alert(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "publish_alert failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }

    async fn report_migration_progress(
        &self,
        request: Request<proto::MigrationProgress>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "report_migration_progress",
        )?;
        let _span = tracing::info_span!("report_migration_progress", %op_id);
        let resp = self
            .service
            .report_migration_progress(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "report_migration_progress failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }
}

pub struct LifecycleServer {
    service: Arc<dyn LifecycleService>,
}

impl LifecycleServer {
    pub fn new(service: Arc<dyn LifecycleService>) -> Self {
        Self { service }
    }
}

#[tonic::async_trait]
impl proto::lifecycle_service_server::LifecycleService for LifecycleServer {
    async fn create_vm(
        &self,
        request: Request<proto::CreateVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "create_vm")?;
        let _span = tracing::info_span!("create_vm", %op_id);
        let resp = self
            .service
            .create_vm(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    /// #513 (DP1, adopted): the standalone volume create journals
    /// BFF-direct (mirroring `POST /v1/vms`), NOT through a CP
    /// lifecycle RPC — the adopted design rejected the second
    /// journaling pipeline behind this surface (the lifecycle create
    /// RPCs have no production caller, per #516's finding). The node
    /// RPC of the same name (the #513 PR 1 dispatch carrier the
    /// orchestrator dispatches to agents) is a different surface; this
    /// CP-side shim stays closed until/unless lifecycle journaling is
    /// ever adopted for the create.
    ///
    /// Authz note: this handler never reads the request or touches node
    /// state — it always returns `Unimplemented` before `into_inner()`.
    /// There is no authority to bind to a peer identity, so it is
    /// intentionally exempt from `enforce_peer_node_id`.
    async fn create_volume(
        &self,
        _request: Request<proto::CreateVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        Err(Status::unimplemented(
            "create_volume is not served by the control plane lifecycle; the standalone volume \
             create journals via the BFF (#513)",
        ))
    }

    /// #522 (DP1, adopted): the volume delete journals BFF-direct
    /// (mirroring `POST /v1/vms/delete`), NOT through a CP lifecycle
    /// RPC — the adopted design's tombstone transaction belongs to the
    /// BFF route (PR 2). The node RPC of the same name (the #522 PR 1
    /// dispatch carrier the orchestrator dispatches to agents) is a
    /// different surface; this CP-side shim stays closed.
    ///
    /// Authz note: unconditional `Unimplemented`, no request/state access;
    /// exempt for the same reason as `create_volume`.
    async fn delete_volume(
        &self,
        _request: Request<proto::DeleteVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        Err(Status::unimplemented(
            "delete_volume is not served by the control plane lifecycle; the volume delete \
             journals via the BFF (#522)",
        ))
    }

    /// #355 (PR 1 of the decomposition): the network policy update
    /// journals BFF-direct in the desired-state transaction (PR 2,
    /// the CreateVolume journaling precedent), NOT through a CP
    /// lifecycle RPC. The node RPC of the same name (the #355 dispatch
    /// carrier the orchestrator fans out to agents) is a different
    /// surface; this CP-side shim stays closed.
    ///
    /// Authz note: unconditional `Unimplemented`, no request/state access;
    /// exempt for the same reason as `create_volume`.
    async fn apply_network_policy(
        &self,
        _request: Request<proto::ApplyNetworkPolicyRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        Err(Status::unimplemented(
            "apply_network_policy is not served by the control plane lifecycle; the network \
             policy update journals via the BFF (#355)",
        ))
    }

    async fn start_vm(
        &self,
        request: Request<proto::StartVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "start_vm")?;
        let _span = tracing::info_span!("start_vm", %op_id);
        let resp = self
            .service
            .start_vm(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn stop_vm(
        &self,
        request: Request<proto::StopVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "stop_vm")?;
        let _span = tracing::info_span!("stop_vm", %op_id);
        let resp = self
            .service
            .stop_vm(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn reboot_vm(
        &self,
        request: Request<proto::RebootVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "reboot_vm")?;
        let _span = tracing::info_span!("reboot_vm", %op_id);
        let resp = self
            .service
            .reboot_vm(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn delete_vm(
        &self,
        request: Request<proto::DeleteVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "delete_vm")?;
        let _span = tracing::info_span!("delete_vm", %op_id);
        let resp = self
            .service
            .delete_vm(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn resize_vm(
        &self,
        request: Request<proto::ResizeVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "resize_vm")?;
        let _span = tracing::info_span!("resize_vm", %op_id);
        let resp = self
            .service
            .resize_vm(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn attach_volume(
        &self,
        request: Request<proto::AttachVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "attach_volume")?;
        let _span = tracing::info_span!("attach_volume", %op_id);
        let resp = self
            .service
            .attach_volume(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn detach_volume(
        &self,
        request: Request<proto::DetachVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "detach_volume")?;
        let _span = tracing::info_span!("detach_volume", %op_id);
        let resp = self
            .service
            .detach_volume(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn resize_volume(
        &self,
        request: Request<proto::ResizeVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "resize_volume")?;
        let _span = tracing::info_span!("resize_volume", %op_id);
        let resp = self
            .service
            .resize_volume(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn snapshot_volume(
        &self,
        request: Request<proto::SnapshotVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "snapshot_volume")?;
        let _span = tracing::info_span!("snapshot_volume", %op_id);
        let resp = self
            .service
            .snapshot_volume(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn restore_volume(
        &self,
        request: Request<proto::RestoreVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "restore_volume")?;
        let _span = tracing::info_span!("restore_volume", %op_id);
        let resp = self
            .service
            .restore_volume(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn delete_volume_snapshot(
        &self,
        request: Request<proto::DeleteVolumeSnapshotRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "delete_volume_snapshot",
        )?;
        let _span = tracing::info_span!("delete_volume_snapshot", %op_id);
        let resp = self
            .service
            .delete_volume_snapshot(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn clone_volume(
        &self,
        request: Request<proto::CloneVolumeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "clone_volume")?;
        let _span = tracing::info_span!("clone_volume", %op_id);
        let resp = self
            .service
            .clone_volume(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn pause_node_scheduling(
        &self,
        request: Request<proto::PauseNodeSchedulingRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "pause_node_scheduling",
        )?;
        let _span = tracing::info_span!("pause_node_scheduling", %op_id);
        let resp = self
            .service
            .pause_node_scheduling(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn resume_node_scheduling(
        &self,
        request: Request<proto::ResumeNodeSchedulingRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "resume_node_scheduling",
        )?;
        let _span = tracing::info_span!("resume_node_scheduling", %op_id);
        let resp = self
            .service
            .resume_node_scheduling(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn drain_node(
        &self,
        request: Request<proto::DrainNodeRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "drain_node")?;
        let _span = tracing::info_span!("drain_node", %op_id);
        let resp = self
            .service
            .drain_node(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn enter_maintenance(
        &self,
        request: Request<proto::EnterMaintenanceRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "enter_maintenance")?;
        let _span = tracing::info_span!("enter_maintenance", %op_id);
        let resp = self
            .service
            .enter_maintenance(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn exit_maintenance(
        &self,
        request: Request<proto::ExitMaintenanceRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "exit_maintenance")?;
        let _span = tracing::info_span!("exit_maintenance", %op_id);
        let resp = self
            .service
            .exit_maintenance(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn pause_vm(
        &self,
        request: Request<proto::PauseVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "pause_vm")?;
        let _span = tracing::info_span!("pause_vm", %op_id);
        let resp = self
            .service
            .pause_vm(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn resume_vm(
        &self,
        request: Request<proto::ResumeVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "resume_vm")?;
        let _span = tracing::info_span!("resume_vm", %op_id);
        let resp = self
            .service
            .resume_vm(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn power_button_vm(
        &self,
        request: Request<proto::PowerButtonVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "power_button_vm")?;
        let _span = tracing::info_span!("power_button_vm", %op_id);
        let resp = self
            .service
            .power_button_vm(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn add_disk(
        &self,
        request: Request<proto::AddDiskRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "add_disk")?;
        let _span = tracing::info_span!("add_disk", %op_id);
        let resp = self
            .service
            .add_disk(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn remove_device(
        &self,
        request: Request<proto::RemoveDeviceRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "remove_device")?;
        let _span = tracing::info_span!("remove_device", %op_id);
        let resp = self
            .service
            .remove_device(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn add_net(
        &self,
        request: Request<proto::AddNetRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "add_net")?;
        let _span = tracing::info_span!("add_net", %op_id);
        let resp = self
            .service
            .add_net(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn resize_disk(
        &self,
        request: Request<proto::ResizeDiskRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "resize_disk")?;
        let _span = tracing::info_span!("resize_disk", %op_id);
        let resp = self
            .service
            .resize_disk(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn snapshot_vm(
        &self,
        request: Request<proto::SnapshotVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "snapshot_vm")?;
        let _span = tracing::info_span!("snapshot_vm", %op_id);
        let resp = self
            .service
            .snapshot_vm(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn restore_snapshot(
        &self,
        request: Request<proto::RestoreSnapshotRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "restore_snapshot")?;
        let _span = tracing::info_span!("restore_snapshot", %op_id);
        let resp = self
            .service
            .restore_snapshot(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn coredump_vm(
        &self,
        request: Request<proto::CoredumpVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "coredump_vm")?;
        let _span = tracing::info_span!("coredump_vm", %op_id);
        let resp = self
            .service
            .coredump_vm(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn start_network(
        &self,
        request: Request<proto::StartNetworkRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "start_network")?;
        let _span = tracing::info_span!("start_network", %op_id);
        let resp = self
            .service
            .start_network(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn stop_network(
        &self,
        request: Request<proto::StopNetworkRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "stop_network")?;
        let _span = tracing::info_span!("stop_network", %op_id);
        let resp = self
            .service
            .stop_network(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn restart_network(
        &self,
        request: Request<proto::RestartNetworkRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "restart_network")?;
        let _span = tracing::info_span!("restart_network", %op_id);
        let resp = self
            .service
            .restart_network(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    /// Authz note: unconditional `Unimplemented` (the control plane does
    /// not proxy `PingVmm`), no request/state access; exempt from
    /// `enforce_peer_node_id`.
    async fn ping_vmm(
        &self,
        _request: Request<proto::PingVmmRequest>,
    ) -> Result<Response<proto::PingVmmResponse>, Status> {
        Err(tonic::Status::unimplemented(
            "ping_vmm not supported on control plane",
        ))
    }

    async fn migrate_vm(
        &self,
        request: Request<proto::MigrateVmRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "migrate_vm")?;
        let _span = tracing::info_span!("migrate_vm", %op_id);
        let resp = self
            .service
            .migrate_vm(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn update_overlay(
        &self,
        request: Request<proto::UpdateOverlayRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "update_overlay")?;
        let _span = tracing::info_span!("update_overlay", %op_id);
        let resp = self
            .service
            .update_overlay(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn send_gratuitous_arp(
        &self,
        request: Request<proto::SendGratuitousArpRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(&request, &request.get_ref().node_id, "send_gratuitous_arp")?;
        let _span = tracing::info_span!("send_gratuitous_arp", %op_id);
        let resp = self
            .service
            .send_gratuitous_arp(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    /// Authz note: not node-scoped on the wire. This is an operator egress
    /// relay into a node; the request carries no `node_id` field and the
    /// target is derived from `meta.target_node_id` (the operator/CP is the
    /// authenticated caller, not the target node). It is therefore exempt
    /// from `enforce_peer_node_id`.
    async fn resolve_inspect_required_operation(
        &self,
        request: Request<proto::ResolveInspectRequiredOperationRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        let _span = tracing::info_span!("resolve_inspect_required_operation", op_id = %op_id.escape_debug());
        let resp = self
            .service
            .resolve_inspect_required_operation(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }
}

pub struct ReconcileServer {
    service: Arc<dyn ReconcileService>,
}

impl ReconcileServer {
    pub fn new(service: Arc<dyn ReconcileService>) -> Self {
        Self { service }
    }
}

#[tonic::async_trait]
impl proto::reconcile_service_server::ReconcileService for ReconcileServer {
    async fn apply_node_desired_state(
        &self,
        request: Request<proto::ApplyNodeDesiredStateRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "apply_node_desired_state",
        )?;
        let _span = tracing::info_span!("apply_node_desired_state", %op_id);
        let resp = self
            .service
            .apply_node_desired_state(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn apply_vm_desired_state(
        &self,
        request: Request<proto::ApplyVmDesiredStateRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "apply_vm_desired_state",
        )?;
        let _span = tracing::info_span!("apply_vm_desired_state", %op_id);
        let resp = self
            .service
            .apply_vm_desired_state(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn apply_volume_desired_state(
        &self,
        request: Request<proto::ApplyVolumeDesiredStateRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "apply_volume_desired_state",
        )?;
        let _span = tracing::info_span!("apply_volume_desired_state", %op_id);
        let resp = self
            .service
            .apply_volume_desired_state(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn apply_network_desired_state(
        &self,
        request: Request<proto::ApplyNetworkDesiredStateRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "apply_network_desired_state",
        )?;
        let _span = tracing::info_span!("apply_network_desired_state", %op_id);
        let resp = self
            .service
            .apply_network_desired_state(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }

    async fn acknowledge_desired_state_version(
        &self,
        request: Request<proto::AcknowledgeDesiredStateVersionRequest>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "acknowledge_desired_state_version",
        )?;
        let _span = tracing::info_span!("acknowledge_desired_state_version", %op_id);
        let resp = self
            .service
            .acknowledge_desired_state_version(request.into_inner())
            .instrument(_span)
            .await
            .map_err(tonic::Status::from)?;
        Ok(Response::new(resp))
    }
}

/// gRPC adapter for the node metric batch service (campaign #602 PR-2).
/// Same peer-identity discipline as every other node-facing RPC: the
/// wire-asserted `node_id` must match the mTLS peer certificate, so a
/// sender can never ingest telemetry under another node's identity.
pub struct MonitoringServer {
    service: Arc<dyn crate::monitoring_ingest::MonitoringIngestService>,
}

impl MonitoringServer {
    pub fn new(service: Arc<dyn crate::monitoring_ingest::MonitoringIngestService>) -> Self {
        Self { service }
    }
}

#[tonic::async_trait]
impl proto::monitoring_service_server::MonitoringService for MonitoringServer {
    async fn ingest_node_metric_batch(
        &self,
        request: Request<proto::NodeMetricBatchRequest>,
    ) -> Result<Response<proto::NodeMetricBatchResponse>, Status> {
        let op_id = extract_op_id(&request).unwrap_or_default();
        enforce_peer_node_id(
            &request,
            &request.get_ref().node_id,
            "ingest_node_metric_batch",
        )?;
        let _span = tracing::info_span!("ingest_node_metric_batch", %op_id);
        let resp = self
            .service
            .ingest_node_metric_batch(request.into_inner())
            .instrument(_span)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "ingest_node_metric_batch failed");
                tonic::Status::from(e)
            })?;
        Ok(Response::new(resp))
    }
}
