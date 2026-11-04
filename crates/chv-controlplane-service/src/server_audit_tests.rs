//! Durable gRPC audit coverage for the node-facing lifecycle and
//! enrollment boundaries (#658). These tests drive the real server
//! adapters against a `TestDb`, with synthetic peer-identity extensions.

use crate::enrollment::EnrollmentService;
use crate::error::ControlPlaneServiceError;
use crate::lifecycle::LifecycleService;
use crate::peer_identity::{InsecurePeer, PeerCertificateSerial, PeerNodeId};
use crate::reconcile::ReconcileService;
use crate::revocation::RevocationSnapshot;
use crate::server::{EnrollmentServer, LifecycleServer, ReconcileServer};
use chv_controlplane_store::{test_util::TestDb, AuditLogRepository, AuditLogRow, StorePool};
use chv_controlplane_types::domain::NodeId;
use control_plane_node_api::control_plane_node_api as proto;
use control_plane_node_api::control_plane_node_api::enrollment_service_server::EnrollmentService as EnrollmentServiceApi;
use control_plane_node_api::control_plane_node_api::lifecycle_service_server::LifecycleService as LifecycleServiceApi;
use control_plane_node_api::control_plane_node_api::reconcile_service_server::ReconcileService as ReconcileServiceApi;
use std::sync::Arc;
use tonic::{Code, Request};

#[derive(Default)]
struct MockLifecycle {
    fail_start: bool,
}

fn ok_ack() -> proto::AckResponse {
    proto::AckResponse {
        result: Some(proto::ResultMeta {
            operation_id: "op-mock".into(),
            status: "OK".into(),
            ..Default::default()
        }),
    }
}

macro_rules! mock_lifecycle_impl {
    ($($name:ident($ty:ty)),* $(,)?) => {
        #[tonic::async_trait]
        impl LifecycleService for MockLifecycle {
            async fn start_vm(
                &self,
                _request: proto::StartVmRequest,
            ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
                if self.fail_start {
                    Err(ControlPlaneServiceError::Internal(
                        "mock start failure".into(),
                    ))
                } else {
                    Ok(ok_ack())
                }
            }

            $(
                async fn $name(
                    &self,
                    _request: $ty,
                ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
                    Err(ControlPlaneServiceError::Unsupported(
                        "unused in audit tests".into(),
                    ))
                }
            )*
        }
    };
}

mock_lifecycle_impl! {
    create_vm(proto::CreateVmRequest),
    stop_vm(proto::StopVmRequest),
    reboot_vm(proto::RebootVmRequest),
    delete_vm(proto::DeleteVmRequest),
    resize_vm(proto::ResizeVmRequest),
    attach_volume(proto::AttachVolumeRequest),
    detach_volume(proto::DetachVolumeRequest),
    resize_volume(proto::ResizeVolumeRequest),
    snapshot_volume(proto::SnapshotVolumeRequest),
    restore_volume(proto::RestoreVolumeRequest),
    delete_volume_snapshot(proto::DeleteVolumeSnapshotRequest),
    clone_volume(proto::CloneVolumeRequest),
    pause_node_scheduling(proto::PauseNodeSchedulingRequest),
    resume_node_scheduling(proto::ResumeNodeSchedulingRequest),
    drain_node(proto::DrainNodeRequest),
    enter_maintenance(proto::EnterMaintenanceRequest),
    exit_maintenance(proto::ExitMaintenanceRequest),
    pause_vm(proto::PauseVmRequest),
    resume_vm(proto::ResumeVmRequest),
    power_button_vm(proto::PowerButtonVmRequest),
    add_disk(proto::AddDiskRequest),
    remove_device(proto::RemoveDeviceRequest),
    add_net(proto::AddNetRequest),
    resize_disk(proto::ResizeDiskRequest),
    snapshot_vm(proto::SnapshotVmRequest),
    restore_snapshot(proto::RestoreSnapshotRequest),
    coredump_vm(proto::CoredumpVmRequest),
    start_network(proto::StartNetworkRequest),
    stop_network(proto::StopNetworkRequest),
    restart_network(proto::RestartNetworkRequest),
    migrate_vm(proto::MigrateVmRequest),
    update_overlay(proto::UpdateOverlayRequest),
    send_gratuitous_arp(proto::SendGratuitousArpRequest),
    resolve_inspect_required_operation(proto::ResolveInspectRequiredOperationRequest),
}

#[derive(Default)]
struct MockEnrollment {
    fail_enroll: bool,
}

#[tonic::async_trait]
impl EnrollmentService for MockEnrollment {
    async fn enroll_node(
        &self,
        request: proto::EnrollmentRequest,
        _peer_addr: Option<std::net::SocketAddr>,
    ) -> Result<proto::EnrollmentResponse, ControlPlaneServiceError> {
        if self.fail_enroll {
            return Err(ControlPlaneServiceError::Unauthorized(
                "invalid bootstrap token".into(),
            ));
        }
        let issued_node_id = request
            .inventory
            .as_ref()
            .map(|inventory| inventory.node_id.clone())
            .unwrap_or_default();
        Ok(proto::EnrollmentResponse {
            result: Some(proto::ResultMeta {
                status: "OK".into(),
                ..Default::default()
            }),
            issued_node_id,
            ..Default::default()
        })
    }

    async fn rotate_node_certificate(
        &self,
        request: proto::RotateNodeCertificateRequest,
        _presented_serial: Option<String>,
        _allow_insecure: bool,
    ) -> Result<proto::RotateNodeCertificateResponse, ControlPlaneServiceError> {
        Ok(proto::RotateNodeCertificateResponse {
            result: Some(proto::ResultMeta {
                operation_id: request
                    .meta
                    .as_ref()
                    .map(|meta| meta.operation_id.clone())
                    .unwrap_or_default(),
                status: "OK".into(),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    async fn report_bootstrap_result(
        &self,
        _request: proto::ReportBootstrapResultRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        Ok(ok_ack())
    }
}

struct MockReconcile;

#[tonic::async_trait]
impl ReconcileService for MockReconcile {
    async fn apply_node_desired_state(
        &self,
        _request: proto::ApplyNodeDesiredStateRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        Ok(ok_ack())
    }

    async fn apply_vm_desired_state(
        &self,
        _request: proto::ApplyVmDesiredStateRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        Ok(ok_ack())
    }

    async fn apply_volume_desired_state(
        &self,
        _request: proto::ApplyVolumeDesiredStateRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        Ok(ok_ack())
    }

    async fn apply_network_desired_state(
        &self,
        _request: proto::ApplyNetworkDesiredStateRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        Ok(ok_ack())
    }

    async fn acknowledge_desired_state_version(
        &self,
        _request: proto::AcknowledgeDesiredStateVersionRequest,
    ) -> Result<proto::AckResponse, ControlPlaneServiceError> {
        Ok(ok_ack())
    }
}

fn meta(operation_id: &str) -> proto::RequestMeta {
    proto::RequestMeta {
        operation_id: operation_id.to_string(),
        ..Default::default()
    }
}

fn peer_request<T>(message: T, peer_node_id: &str) -> Request<T> {
    let mut request = Request::new(message);
    request.extensions_mut().insert(PeerNodeId(
        NodeId::new(peer_node_id.to_string()).expect("valid test node id"),
    ));
    request
}

async fn audit_rows(pool: &StorePool) -> Vec<AuditLogRow> {
    sqlx::query_as::<_, AuditLogRow>(
        "SELECT * FROM audit_log ORDER BY occurred_at_ms ASC, audit_id ASC",
    )
    .fetch_all(pool)
    .await
    .expect("query audit rows")
}

fn assert_metadata_is_allowlisted(metadata: &str, auth_source: &str) {
    let value: serde_json::Value = serde_json::from_str(metadata).expect("metadata json");
    let object = value.as_object().expect("metadata object");
    assert_eq!(object.len(), 2, "metadata must only carry the allowlist");
    assert_eq!(value["transport"], "grpc");
    assert_eq!(value["auth_source"], auth_source);
}

#[tokio::test]
async fn server_audit_lifecycle_success_writes_one_row() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let mut request = peer_request(
        proto::StartVmRequest {
            meta: Some(meta("op-start")),
            node_id: "node-a".into(),
            vm_id: "vm-1".into(),
        },
        "node-a",
    );
    request.metadata_mut().insert(
        chv_common::OPERATION_ID_METADATA_KEY,
        "op-start".parse().expect("metadata value"),
    );
    server.start_vm(request).await.expect("start vm succeeds");

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one row per mutation");
    let row = &rows[0];
    assert_eq!(row.action, "StartVm");
    assert_eq!(row.method, "gRPC");
    assert_eq!(
        row.path,
        "chv.controlplane.node.v1.LifecycleService/StartVm"
    );
    assert_eq!(row.actor_id, "node-a");
    assert_eq!(row.actor_username, "node-a");
    assert_eq!(row.actor_role, "node");
    assert_eq!(row.resource_kind.as_deref(), Some("vm"));
    assert_eq!(row.resource_id.as_deref(), Some("vm-1"));
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status_code, 200);
    assert_eq!(row.operation_id.as_deref(), Some("op-start"));
    assert_eq!(row.correlation_id, None);
    assert!(row.source_ip.is_none());
    assert_metadata_is_allowlisted(&row.metadata, "mtls");
}

#[tokio::test]
async fn server_audit_lifecycle_peer_mismatch_writes_denied_row() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    // Actor is node-a; the wire claims node-b. Validation denies before
    // the service call, and the attempt is still recorded.
    let request = peer_request(
        proto::StartVmRequest {
            meta: Some(meta("op-denied")),
            node_id: "node-b".into(),
            vm_id: "vm-2".into(),
        },
        "node-a",
    );
    let status = server
        .start_vm(request)
        .await
        .expect_err("peer mismatch must be denied");
    assert_eq!(status.code(), Code::PermissionDenied);

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "StartVm");
    assert_eq!(row.actor_id, "node-a", "actor is the certificate identity");
    assert_eq!(row.actor_role, "node");
    assert_eq!(row.resource_id.as_deref(), Some("vm-2"));
    assert_eq!(row.outcome, "denied");
    assert_eq!(row.status_code, 403);
}

#[tokio::test]
async fn server_audit_lifecycle_audit_write_failure_returns_internal() {
    let db = TestDb::new().await;
    // Make only the audit table unavailable; the lifecycle mutation itself
    // still succeeds, so the adapter must fail closed.
    sqlx::query("DROP TABLE audit_log")
        .execute(&db.pool)
        .await
        .expect("drop audit table");
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::StartVmRequest {
            meta: Some(meta("op-no-audit")),
            node_id: "node-a".into(),
            vm_id: "vm-3".into(),
        },
        "node-a",
    );
    let status = server
        .start_vm(request)
        .await
        .expect_err("successful mutation without an audit row must fail closed");
    assert_eq!(status.code(), Code::Internal);
    assert!(
        status.message().contains("AUDIT_LOG_WRITE_FAILED"),
        "message must be operator-greppable, got: {}",
        status.message()
    );
}

#[tokio::test]
async fn server_audit_enrollment_certificate_rotation_writes_row() {
    let db = TestDb::new().await;
    let server = EnrollmentServer::new(Arc::new(MockEnrollment::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::RotateNodeCertificateRequest {
            meta: Some(meta("op-rotate")),
            node_id: "node-a".into(),
        },
        "node-a",
    );
    server
        .rotate_node_certificate(request)
        .await
        .expect("rotation succeeds");

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "RotateNodeCertificate");
    assert_eq!(row.method, "gRPC");
    assert_eq!(
        row.path,
        "chv.controlplane.node.v1.EnrollmentService/RotateNodeCertificate"
    );
    assert_eq!(row.actor_id, "node-a");
    assert_eq!(row.actor_role, "node");
    assert_eq!(row.resource_kind.as_deref(), Some("node"));
    assert_eq!(row.resource_id.as_deref(), Some("node-a"));
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status_code, 200);
    assert_metadata_is_allowlisted(&row.metadata, "mtls");
}

#[tokio::test]
async fn server_audit_enroll_node_is_anonymous_and_uses_issued_id() {
    let db = TestDb::new().await;
    let server = EnrollmentServer::new(Arc::new(MockEnrollment::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    // Bootstrap enrollment is token-authenticated: even if a peer
    // extension is present, the actor must be the explicit anonymous
    // marker, never the wire-asserted inventory id.
    let request = peer_request(
        proto::EnrollmentRequest {
            bootstrap_token: "bootstrap-secret-token".into(),
            inventory: Some(proto::NodeInventory {
                node_id: "node-enrolled".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
        "node-untrusted-claim",
    );
    server
        .enroll_node(request)
        .await
        .expect("enrollment succeeds");

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "EnrollNode");
    assert_eq!(row.method, "gRPC");
    assert_eq!(
        row.path,
        "chv.controlplane.node.v1.EnrollmentService/EnrollNode"
    );
    assert_eq!(row.actor_id, "anonymous");
    assert_eq!(row.actor_username, "anonymous");
    assert_eq!(row.actor_role, "anonymous");
    assert_eq!(row.resource_kind.as_deref(), Some("node"));
    assert_eq!(row.resource_id.as_deref(), Some("node-enrolled"));
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status_code, 200);
    assert_metadata_is_allowlisted(&row.metadata, "bootstrap_token");
    assert!(
        !row.metadata.contains("bootstrap-secret-token"),
        "token must never reach the audit row"
    );
}

fn insecure_request<T>(message: T) -> Request<T> {
    let mut request = Request::new(message);
    request.extensions_mut().insert(InsecurePeer);
    request
}

#[tokio::test]
async fn server_audit_rotation_rejected_for_revoked_peer_writes_denied_row() {
    let db = TestDb::new().await;
    let server = EnrollmentServer::new(Arc::new(MockEnrollment::default()))
        .with_revocation(RevocationSnapshot::from_serials(["revoked-serial"]))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let mut request = peer_request(
        proto::RotateNodeCertificateRequest {
            meta: Some(meta("op-rotate-revoked")),
            node_id: "node-a".into(),
        },
        "node-a",
    );
    request
        .extensions_mut()
        .insert(PeerCertificateSerial(Some("revoked-serial".into())));
    let status = server
        .rotate_node_certificate(request)
        .await
        .expect_err("revoked peer must not rotate");
    assert_eq!(status.code(), Code::Unauthenticated);

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "denial must still be recorded");
    let row = &rows[0];
    assert_eq!(row.action, "RotateNodeCertificate");
    assert_eq!(row.actor_id, "node-a");
    assert_eq!(row.outcome, "denied");
    assert_eq!(row.status_code, 401);
    assert_eq!(row.resource_id.as_deref(), Some("node-a"));
    assert_metadata_is_allowlisted(&row.metadata, "mtls");
}

#[tokio::test]
async fn server_audit_unimplemented_shim_writes_failed_row() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::CreateVolumeRequest {
            meta: Some(meta("op-unimplemented")),
            node_id: "node-a".into(),
            volume: Some(proto::VolumeMutationSpec {
                volume_id: "vol-shim".into(),
                ..Default::default()
            }),
        },
        "node-a",
    );
    let status = server
        .create_volume(request)
        .await
        .expect_err("shim always returns Unimplemented");
    assert_eq!(status.code(), Code::Unimplemented);

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "shim attempts are audited");
    let row = &rows[0];
    assert_eq!(row.action, "CreateVolume");
    assert_eq!(row.resource_kind.as_deref(), Some("volume"));
    assert_eq!(row.resource_id.as_deref(), Some("vol-shim"));
    assert_eq!(row.outcome, "failed");
    assert_eq!(row.status_code, 501);
    assert_eq!(row.actor_id, "node-a");
}

#[tokio::test]
async fn server_audit_service_failure_preserves_status_and_records_failed_row() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle { fail_start: true }))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::StartVmRequest {
            meta: Some(meta("op-service-fail")),
            node_id: "node-a".into(),
            vm_id: "vm-fail".into(),
        },
        "node-a",
    );
    let mut request = request;
    request.metadata_mut().insert(
        chv_common::OPERATION_ID_METADATA_KEY,
        "op-service-fail".parse().expect("metadata value"),
    );
    let status = server
        .start_vm(request)
        .await
        .expect_err("mock service fails");
    assert_eq!(status.code(), Code::Internal);
    assert!(
        !status.message().contains("AUDIT_LOG_WRITE_FAILED"),
        "a handler failure must not be masked as an audit-write failure: {}",
        status.message()
    );

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "StartVm");
    assert_eq!(row.outcome, "failed");
    assert_eq!(row.status_code, 500);
    assert_eq!(row.operation_id.as_deref(), Some("op-service-fail"));
}

#[tokio::test]
async fn server_audit_report_bootstrap_result_writes_row() {
    let db = TestDb::new().await;
    let server = EnrollmentServer::new(Arc::new(MockEnrollment::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::ReportBootstrapResultRequest {
            meta: Some(meta("op-bootstrap")),
            node_id: "node-a".into(),
            bootstrap_status: "ok".into(),
            message: String::new(),
        },
        "node-a",
    );
    server
        .report_bootstrap_result(request)
        .await
        .expect("bootstrap report succeeds");

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "ReportBootstrapResult");
    assert_eq!(row.actor_id, "node-a");
    assert_eq!(row.resource_kind.as_deref(), Some("node"));
    assert_eq!(row.resource_id.as_deref(), Some("node-a"));
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status_code, 200);
    assert_metadata_is_allowlisted(&row.metadata, "mtls");
}

#[tokio::test]
async fn server_audit_failed_enroll_is_anonymous_with_inventory_fallback() {
    let db = TestDb::new().await;
    let server = EnrollmentServer::new(Arc::new(MockEnrollment { fail_enroll: true }))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::EnrollmentRequest {
            bootstrap_token: "bootstrap-secret-token".into(),
            inventory: Some(proto::NodeInventory {
                node_id: "node-claimed".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
        "node-untrusted-claim",
    );
    let status = server
        .enroll_node(request)
        .await
        .expect_err("enrollment fails");
    assert_eq!(status.code(), Code::Unauthenticated);

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "EnrollNode");
    assert_eq!(row.actor_id, "anonymous");
    assert_eq!(
        row.resource_id.as_deref(),
        Some("node-claimed"),
        "a failed enrollment falls back to the wire-asserted inventory id"
    );
    assert_eq!(row.outcome, "denied");
    assert_eq!(row.status_code, 401);
    assert_metadata_is_allowlisted(&row.metadata, "bootstrap_token");
    assert!(!row.metadata.contains("bootstrap-secret-token"));
}

#[tokio::test]
async fn server_audit_insecure_lifecycle_is_anonymous_auth_source_insecure() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = insecure_request(proto::StartVmRequest {
        meta: Some(meta("op-insecure")),
        node_id: "node-dev".into(),
        vm_id: "vm-dev".into(),
    });
    server
        .start_vm(request)
        .await
        .expect("insecure dev mode accepts the asserted node id");

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.actor_id, "anonymous");
    assert_eq!(row.actor_role, "anonymous");
    assert_eq!(row.resource_id.as_deref(), Some("vm-dev"));
    assert_metadata_is_allowlisted(&row.metadata, "insecure");
}

#[tokio::test]
async fn server_audit_reconcile_apply_node_desired_state_writes_row() {
    let db = TestDb::new().await;
    let server = ReconcileServer::new(Arc::new(MockReconcile))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::ApplyNodeDesiredStateRequest {
            meta: Some(meta("op-reconcile")),
            node_id: "node-a".into(),
            fragment: Some(proto::DesiredStateFragment {
                id: "node-a".into(),
                kind: "node".into(),
                ..Default::default()
            }),
        },
        "node-a",
    );
    server
        .apply_node_desired_state(request)
        .await
        .expect("apply node desired state succeeds");

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "ApplyNodeDesiredState");
    assert_eq!(
        row.path,
        "chv.controlplane.node.v1.ReconcileService/ApplyNodeDesiredState"
    );
    assert_eq!(row.actor_id, "node-a");
    assert_eq!(row.resource_kind.as_deref(), Some("node"));
    assert_eq!(row.resource_id.as_deref(), Some("node-a"));
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status_code, 200);
    assert_metadata_is_allowlisted(&row.metadata, "mtls");
}

#[tokio::test]
async fn server_audit_reconcile_acknowledge_accepts_capitalized_kind() {
    let db = TestDb::new().await;
    let server = ReconcileServer::new(Arc::new(MockReconcile))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::AcknowledgeDesiredStateVersionRequest {
            meta: Some(meta("op-ack")),
            node_id: "node-a".into(),
            fragment_kind: "Vm".into(),
            fragment_id: "vm-ack".into(),
            observed_generation: "2".into(),
            apply_status: "applied".into(),
        },
        "node-a",
    );
    server
        .acknowledge_desired_state_version(request)
        .await
        .expect("acknowledge succeeds");

    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.action, "AcknowledgeDesiredStateVersion");
    assert_eq!(
        row.resource_kind.as_deref(),
        Some("vm"),
        "the audit kind must mirror the service's accepted parser"
    );
    assert_eq!(row.resource_id.as_deref(), Some("vm-ack"));
    assert_eq!(row.outcome, "success");
}
