//! Durable gRPC audit coverage for the node-facing lifecycle and
//! enrollment boundaries (#658). These tests drive the real server
//! adapters against a `TestDb`, with synthetic peer-identity extensions.

use crate::audit_outbox_dispatcher::AuditOutboxDispatcher;
use crate::enrollment::{
    CertificateIssuer, EnrollmentService, EnrollmentServiceImplementation, IssuedCertificate,
};
use crate::error::ControlPlaneServiceError;
use crate::lifecycle::LifecycleService;
use crate::peer_identity::{InsecurePeer, PeerCertificateSerial, PeerNodeId};
use crate::reconcile::ReconcileService;
use crate::revocation::RevocationSnapshot;
use crate::server::{EnrollmentServer, LifecycleServer, ReconcileServer};
use chv_controlplane_store::{
    test_util::TestDb, AuditLogInsert, AuditLogRepository, AuditLogRow, AuditOutboxInsert,
    AuditOutboxRepository, AuditOutboxRow, BootstrapTokenRepository, NodeRepository,
    NodeUpsertInput, StorePool, VtepRepository, AUDIT_OUTBOX_DISPATCHED, AUDIT_OUTBOX_PENDING,
};
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
        _audit: Option<&chv_controlplane_store::AuditOutboxInsert>,
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
        _audit: Option<&chv_controlplane_store::AuditOutboxInsert>,
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
        _audit: Option<&chv_controlplane_store::AuditOutboxInsert>,
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

// ---------------------------------------------------------------------------
// #706 PR 3a — enrollment RPCs coupled to the transactional audit outbox.
//
// These tests drive the *real* `EnrollmentServiceImplementation` (not a
// mock), so the outbox row is enqueued inside the service's own mutation
// transaction exactly as in production. Mocks above still cover the
// unadopted fallback shape (a service that ignores the ticket).
// ---------------------------------------------------------------------------

/// Multi-use bootstrap token seeded for the adopted-enrollment tests
/// (`sha256("123")`, the fixture used by the enrollment unit tests).
const TEST_TOKEN: &str = "123";
const TEST_TOKEN_HASH: &str = "a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3";

struct FixedSerialIssuer;

#[async_trait::async_trait]
impl CertificateIssuer for FixedSerialIssuer {
    async fn issue_node_certificate(
        &self,
        _node_id: &NodeId,
    ) -> Result<IssuedCertificate, ControlPlaneServiceError> {
        Ok(IssuedCertificate {
            certificate_pem: vec![],
            private_key_pem: vec![],
            ca_pem: vec![],
            serial: "serial-adopted".into(),
        })
    }
}

fn adopted_service(db: &TestDb) -> EnrollmentServiceImplementation {
    EnrollmentServiceImplementation::new(
        NodeRepository::new(db.pool.clone()),
        BootstrapTokenRepository::new(db.pool.clone()),
        Some(Arc::new(FixedSerialIssuer)),
        VtepRepository::new(db.pool.clone()),
    )
}

fn adopted_server(db: &TestDb) -> EnrollmentServer {
    EnrollmentServer::new(Arc::new(adopted_service(db)))
        .with_audit(AuditLogRepository::new(db.pool.clone()))
}

async fn seed_bootstrap_token(pool: &StorePool) {
    sqlx::query("INSERT INTO bootstrap_tokens (token_hash, one_time_use) VALUES (?, false)")
        .bind(TEST_TOKEN_HASH)
        .execute(pool)
        .await
        .expect("seed bootstrap token");
}

async fn seed_node(db: &TestDb, node_id: &str, serial: Option<&str>) {
    NodeRepository::new(db.pool.clone())
        .upsert_node(&NodeUpsertInput {
            node_id: NodeId::new(node_id).expect("valid node id"),
            hostname: node_id.into(),
            display_name: node_id.into(),
            certificate_serial: serial.map(str::to_string),
            agent_version: Some("0.1.0".into()),
            control_plane_version: Some("0.1.0".into()),
            enrolled_unix_ms: 1_000,
            last_seen_unix_ms: 1_000,
        })
        .await
        .expect("seed node");
}

async fn outbox_rows(pool: &StorePool) -> Vec<AuditOutboxRow> {
    sqlx::query_as::<_, AuditOutboxRow>(
        "SELECT * FROM audit_outbox ORDER BY created_at_ms ASC, outbox_id ASC",
    )
    .fetch_all(pool)
    .await
    .expect("query outbox rows")
}

fn dispatcher(pool: &StorePool) -> AuditOutboxDispatcher {
    AuditOutboxDispatcher::new(
        AuditOutboxRepository::new(pool.clone()),
        7 * 24 * 60 * 60 * 1_000,
    )
}

fn enrollment_request(node_id: &str, wireguard_public_key: &str) -> proto::EnrollmentRequest {
    proto::EnrollmentRequest {
        bootstrap_token: TEST_TOKEN.into(),
        inventory: Some(proto::NodeInventory {
            node_id: node_id.into(),
            hostname: format!("{node_id}-host"),
            architecture: "x86_64".into(),
            cpu_threads: 4,
            memory_bytes: 8 * 1024 * 1024 * 1024,
            vtep_ip: String::new(),
            wireguard_public_key: wireguard_public_key.into(),
            underlay_mtu: 1400,
            ..Default::default()
        }),
        versions: Some(proto::ServiceVersions {
            node_id: node_id.into(),
            chv_agent_version: "1.2.3".into(),
            chv_stord_version: "1.2.3".into(),
            chv_nwd_version: "1.2.3".into(),
            cloud_hypervisor_version: "40.0.0".into(),
            host_bundle_version: "1.2.3".into(),
        }),
    }
}

/// Drive one dispatch pass at or after the row's due time and assert it
/// converted exactly one row.
async fn dispatch_one(db: &TestDb, due_at_ms: i64) -> Vec<AuditLogRow> {
    let dispatched = dispatcher(&db.pool)
        .dispatch_pass(due_at_ms + 1)
        .await
        .expect("dispatch pass");
    assert_eq!(dispatched, 1, "exactly one outbox row dispatches");
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "dispatch inserts exactly one evidence row");
    rows
}

#[tokio::test]
async fn server_audit_enroll_node_adopts_outbox_and_dispatches_exactly_once() {
    let db = TestDb::new().await;
    seed_bootstrap_token(&db.pool).await;
    let server = adopted_server(&db);

    let request = peer_request(
        enrollment_request("node-enrolled", "cHViLWVucm9sbGVk"),
        "node-untrusted-claim",
    );
    server
        .enroll_node(request)
        .await
        .expect("enrollment succeeds");

    // The adapter must not write the direct row: the evidence committed
    // with the fabric-identity transaction (the final fail-closed write).
    assert!(
        audit_rows(&db.pool).await.is_empty(),
        "adopted success skips the post-hoc direct insert"
    );
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one outbox row per mutation");
    let row = &rows[0];
    assert_eq!(row.status, AUDIT_OUTBOX_PENDING);
    assert!(
        !row.audit_id.is_empty() && row.audit_id != row.outbox_id,
        "a separate audit_id was pre-minted"
    );
    let payload = row.parse_payload().expect("payload parses");
    assert_eq!(payload.action, "EnrollNode");
    assert_eq!(
        payload.path,
        "chv.controlplane.node.v1.EnrollmentService/EnrollNode"
    );
    assert_eq!(payload.actor_id, "anonymous");
    assert_eq!(payload.actor_role, "anonymous");
    assert_eq!(payload.resource_kind.as_deref(), Some("node"));
    assert_eq!(payload.resource_id.as_deref(), Some("node-enrolled"));
    assert_eq!(payload.outcome, "success");
    assert_eq!(payload.status_code, 200);
    assert_metadata_is_allowlisted(&payload.metadata, "bootstrap_token");

    let dispatched = dispatch_one(&db, row.created_at_ms).await;
    assert_eq!(dispatched[0].audit_id, row.audit_id, "audit_id is reused");
    assert_eq!(dispatched[0].action, "EnrollNode");
    assert_eq!(dispatched[0].outcome, "success");
    assert_eq!(
        AuditOutboxRepository::new(db.pool.clone())
            .get(&row.outbox_id)
            .await
            .expect("get outbox row")
            .expect("row exists")
            .status,
        AUDIT_OUTBOX_DISPATCHED
    );
}

#[tokio::test]
async fn server_audit_enroll_node_without_fabric_identity_couples_version_append() {
    let db = TestDb::new().await;
    seed_bootstrap_token(&db.pool).await;
    let server = adopted_server(&db);

    // No fabric identity: the version-history append is the final
    // fail-closed write of the sequence, so the ticket rides that tx.
    let request = peer_request(
        enrollment_request("node-no-fabric", ""),
        "node-untrusted-claim",
    );
    server
        .enroll_node(request)
        .await
        .expect("enrollment succeeds");

    assert!(audit_rows(&db.pool).await.is_empty());
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(
        rows.len(),
        1,
        "the evidence still commits with the mutation"
    );
    assert_eq!(rows[0].status, AUDIT_OUTBOX_PENDING);

    let dispatched = dispatch_one(&db, rows[0].created_at_ms).await;
    assert_eq!(dispatched[0].action, "EnrollNode");
    assert_eq!(dispatched[0].resource_id.as_deref(), Some("node-no-fabric"));
}

#[tokio::test]
async fn server_audit_rotate_node_certificate_adopts_outbox_and_dispatches_exactly_once() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", Some("old-serial")).await;
    let server = adopted_server(&db);

    let mut request = peer_request(
        proto::RotateNodeCertificateRequest {
            meta: Some(meta("op-rotate-adopted")),
            node_id: "node-a".into(),
        },
        "node-a",
    );
    request
        .extensions_mut()
        .insert(PeerCertificateSerial(Some("old-serial".into())));
    server
        .rotate_node_certificate(request)
        .await
        .expect("rotation succeeds");

    // The compare-and-swap really committed and the audit row did not.
    assert_eq!(
        NodeRepository::new(db.pool.clone())
            .get_certificate_serial(&NodeId::new("node-a").unwrap())
            .await
            .expect("read serial")
            .as_deref(),
        Some("serial-adopted")
    );
    assert!(audit_rows(&db.pool).await.is_empty(), "no direct insert");
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(payload.action, "RotateNodeCertificate");
    assert_eq!(
        payload.path,
        "chv.controlplane.node.v1.EnrollmentService/RotateNodeCertificate"
    );
    assert_eq!(payload.actor_id, "node-a");
    assert_eq!(payload.actor_role, "node");
    assert_eq!(payload.resource_id.as_deref(), Some("node-a"));
    assert_eq!(payload.outcome, "success");
    assert_eq!(payload.status_code, 200);
    assert_metadata_is_allowlisted(&payload.metadata, "mtls");

    let dispatched = dispatch_one(&db, rows[0].created_at_ms).await;
    assert_eq!(dispatched[0].audit_id, rows[0].audit_id);
    assert_eq!(dispatched[0].action, "RotateNodeCertificate");
}

#[tokio::test]
async fn server_audit_report_bootstrap_result_adopts_outbox_and_dispatches_exactly_once() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", Some("old-serial")).await;
    let server = adopted_server(&db);

    let request = peer_request(
        proto::ReportBootstrapResultRequest {
            meta: Some(meta("op-bootstrap-adopted")),
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

    assert!(audit_rows(&db.pool).await.is_empty(), "no direct insert");
    let rows = outbox_rows(&db.pool).await;
    assert_eq!(rows.len(), 1);
    let payload = rows[0].parse_payload().expect("payload parses");
    assert_eq!(payload.action, "ReportBootstrapResult");
    assert_eq!(
        payload.path,
        "chv.controlplane.node.v1.EnrollmentService/ReportBootstrapResult"
    );
    assert_eq!(payload.actor_id, "node-a");
    assert_eq!(payload.resource_id.as_deref(), Some("node-a"));
    assert_eq!(payload.outcome, "success");
    assert_eq!(payload.status_code, 200);

    let dispatched = dispatch_one(&db, rows[0].created_at_ms).await;
    assert_eq!(dispatched[0].audit_id, rows[0].audit_id);
    assert_eq!(dispatched[0].action, "ReportBootstrapResult");
}

#[tokio::test]
async fn server_audit_rotate_mismatched_serial_writes_no_outbox_row_and_one_denied_row() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", Some("old-serial")).await;
    let server = adopted_server(&db);

    let mut request = peer_request(
        proto::RotateNodeCertificateRequest {
            meta: Some(meta("op-rotate-lost")),
            node_id: "node-a".into(),
        },
        "node-a",
    );
    request
        .extensions_mut()
        .insert(PeerCertificateSerial(Some("stale-serial".into())));
    let status = server
        .rotate_node_certificate(request)
        .await
        .expect_err("a mismatched presented serial must fail closed");
    assert_eq!(status.code(), Code::PermissionDenied);

    // The mutation never ran: no certificate was minted, nothing was
    // enqueued, and the serial did not move. The adapter records exactly
    // one denied row on the direct path.
    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "a rejected rotation enqueues nothing"
    );
    assert_eq!(
        NodeRepository::new(db.pool.clone())
            .get_certificate_serial(&NodeId::new("node-a").unwrap())
            .await
            .expect("read serial")
            .as_deref(),
        Some("old-serial"),
        "the rotation must not commit"
    );
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one direct denied row");
    assert_eq!(rows[0].action, "RotateNodeCertificate");
    assert_eq!(rows[0].outcome, "denied");
    assert_eq!(rows[0].status_code, 403);
    assert_eq!(rows[0].actor_id, "node-a");
}

#[tokio::test]
async fn server_audit_failed_adopted_enroll_writes_only_the_direct_denied_row() {
    let db = TestDb::new().await;
    // No token seeded: the mutation fails before any store write.
    let server = adopted_server(&db);

    let request = peer_request(enrollment_request("node-claimed", "cHVi"), "node-claimed");
    let status = server
        .enroll_node(request)
        .await
        .expect_err("invalid bootstrap token is denied");
    assert_eq!(status.code(), Code::Unauthenticated);

    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "a rejected enrollment enqueues nothing"
    );
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one direct failed/denied row");
    assert_eq!(rows[0].action, "EnrollNode");
    assert_eq!(rows[0].outcome, "denied");
    assert_eq!(rows[0].status_code, 401);
    assert_eq!(rows[0].actor_id, "anonymous");
    assert_eq!(rows[0].resource_id.as_deref(), Some("node-claimed"));
}

#[tokio::test]
async fn server_audit_adopted_crash_replay_writes_exactly_one_row() {
    let db = TestDb::new().await;
    seed_node(&db, "node-a", Some("old-serial")).await;
    let service = adopted_service(&db);

    // Deterministic crash: commit the mutation + outbox row, then never run
    // the adapter's `finish_audited` (the process died before it).
    let now = chv_common::now_unix_ms();
    let insert = AuditOutboxInsert {
        outbox_id: "ob-crash-replay".into(),
        audit_id: "audit-crash-replay".into(),
        payload: AuditLogInsert {
            occurred_at_ms: now,
            actor_id: "node-a".into(),
            actor_username: "node-a".into(),
            actor_role: "node".into(),
            action: "ReportBootstrapResult".into(),
            method: "gRPC".into(),
            path: "chv.controlplane.node.v1.EnrollmentService/ReportBootstrapResult".into(),
            resource_kind: Some("node".into()),
            resource_id: Some("node-a".into()),
            outcome: "success".into(),
            status_code: 200,
            correlation_id: None,
            operation_id: Some("op-crash".into()),
            source_ip: None,
            metadata: r#"{"transport":"grpc","auth_source":"mtls"}"#.into(),
        },
        created_at_ms: now,
    };
    service
        .report_bootstrap_result(
            proto::ReportBootstrapResultRequest {
                meta: Some(meta("op-crash")),
                node_id: "node-a".into(),
                bootstrap_status: "ok".into(),
                message: String::new(),
            },
            Some(&insert),
        )
        .await
        .expect("mutation commits with its outbox row");

    assert!(audit_rows(&db.pool).await.is_empty(), "no post-hoc row yet");
    assert_eq!(outbox_rows(&db.pool).await.len(), 1);

    // The startup dispatcher pass replays the durable row exactly once.
    let rows = dispatch_one(&db, now).await;
    assert_eq!(rows[0].audit_id, "audit-crash-replay");
    assert_eq!(rows[0].action, "ReportBootstrapResult");
    assert_eq!(rows[0].operation_id.as_deref(), Some("op-crash"));

    // A second pass is a no-op: still exactly one evidence row.
    let again = dispatcher(&db.pool)
        .dispatch_pass(now + 60_000)
        .await
        .expect("second pass");
    assert_eq!(again, 0);
    assert_eq!(audit_rows(&db.pool).await.len(), 1);
}

#[tokio::test]
async fn server_audit_unadopted_lifecycle_rpc_writes_one_direct_row_and_no_outbox() {
    let db = TestDb::new().await;
    let server = LifecycleServer::new(Arc::new(MockLifecycle::default()))
        .with_audit(AuditLogRepository::new(db.pool.clone()));

    let request = peer_request(
        proto::StartVmRequest {
            meta: Some(meta("op-unadopted")),
            node_id: "node-a".into(),
            vm_id: "vm-unadopted".into(),
        },
        "node-a",
    );
    server.start_vm(request).await.expect("start vm succeeds");

    assert!(
        outbox_rows(&db.pool).await.is_empty(),
        "unadopted RPCs never touch the outbox"
    );
    let rows = audit_rows(&db.pool).await;
    assert_eq!(rows.len(), 1, "exactly one direct row, as before");
    assert_eq!(rows[0].action, "StartVm");
    assert_eq!(rows[0].outcome, "success");
    assert_eq!(rows[0].status_code, 200);
}
