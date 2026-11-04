//! Durable gRPC audit coverage for the node-facing lifecycle and
//! enrollment boundaries (#658). These tests drive the real server
//! adapters against a `TestDb`, with synthetic peer-identity extensions.

use crate::enrollment::EnrollmentService;
use crate::error::ControlPlaneServiceError;
use crate::lifecycle::LifecycleService;
use crate::peer_identity::PeerNodeId;
use crate::server::{EnrollmentServer, LifecycleServer};
use chv_controlplane_store::{test_util::TestDb, AuditLogRepository, AuditLogRow, StorePool};
use chv_controlplane_types::domain::NodeId;
use control_plane_node_api::control_plane_node_api as proto;
use control_plane_node_api::control_plane_node_api::enrollment_service_server::EnrollmentService as EnrollmentServiceApi;
use control_plane_node_api::control_plane_node_api::lifecycle_service_server::LifecycleService as LifecycleServiceApi;
use std::sync::Arc;
use tonic::{Code, Request};

struct MockLifecycle;

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
                Ok(ok_ack())
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

struct MockEnrollment;

#[tonic::async_trait]
impl EnrollmentService for MockEnrollment {
    async fn enroll_node(
        &self,
        request: proto::EnrollmentRequest,
        _peer_addr: Option<std::net::SocketAddr>,
    ) -> Result<proto::EnrollmentResponse, ControlPlaneServiceError> {
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
    let server = LifecycleServer::new(Arc::new(MockLifecycle))
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
    let server = LifecycleServer::new(Arc::new(MockLifecycle))
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
    let server = LifecycleServer::new(Arc::new(MockLifecycle))
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
    let server = EnrollmentServer::new(Arc::new(MockEnrollment))
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
    let server = EnrollmentServer::new(Arc::new(MockEnrollment))
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
    assert_metadata_is_allowlisted(&row.metadata, "insecure");
    assert!(
        !row.metadata.contains("bootstrap-secret-token"),
        "token must never reach the audit row"
    );
}
